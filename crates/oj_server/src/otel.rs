//! OpenTelemetry export (`OJ_OTEL=1`): diagnostics as log records plus an
//! event counter, and a span and duration sample per dev-server request.
//! Rides the embedded Deno's OTLP pipeline, so the standard
//! `OTEL_EXPORTER_OTLP_*`, `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES`
//! variables configure it.
//!
//! oj keeps no state of its own here: the diagnostics exporter is an output
//! of the diagnostics ring ([`oj_diag::add_output`]) that owns its
//! instruments, and the router layer owns a [`RequestTracer`]. The pipeline
//! is deno_telemetry's (`OTEL_GLOBALS`): its batch processors do the network
//! I/O on their own runtime thread, so an exporter only builds and enqueues.

use std::pin::Pin;
use std::str::FromStr as _;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime};

use axum::body::{Body, Bytes, HttpBody};
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use hyper::body::{Frame, SizeHint};
use oj_js::deno_telemetry::{OtelConfig, OtelGlobals, OtelRuntimeConfig, OTEL_GLOBALS};
use opentelemetry::logs::{LogRecord as _, Logger as _, LoggerProvider as _, Severity};
use opentelemetry::metrics::{Counter, Histogram, MeterProvider as _};
use opentelemetry::trace::{
    SpanContext, SpanId, SpanKind, Status, TraceFlags, TraceId, TraceState,
};
use opentelemetry::{InstrumentationScope, KeyValue};
use opentelemetry_sdk::logs::{LogProcessor as _, SdkLogRecord, SdkLogger, SdkLoggerProvider};
use opentelemetry_sdk::trace::{
    IdGenerator as _, SpanData, SpanEvents, SpanLinks, SpanProcessor as _,
};

fn scope() -> InstrumentationScope {
    InstrumentationScope::builder("oj")
        .with_version(env!("CARGO_PKG_VERSION"))
        .build()
}

/// Head sampler from `OTEL_TRACES_SAMPLER` / `OTEL_TRACES_SAMPLER_ARG`. Deno
/// parses the same pair but keeps its decision private, so oj reads them too.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Sampler {
    parent_based: bool,
    /// Probability for a root span: 1.0 is always_on, 0.0 always_off.
    ratio: f64,
}

impl Sampler {
    /// Unset or empty is the spec default, parentbased_always_on (Deno's own
    /// default is always_on, but its JS instrumentation is off here, so no
    /// Deno span shares a trace with oj's). Deno's init already refuses an
    /// unsupported name, so one never reaches here.
    fn from_vars(name: Option<&str>, arg: Option<&str>) -> Sampler {
        // An unparsable or non-finite probability is the spec default, 1.0.
        let ratio = || {
            arg.and_then(|s| s.trim().parse::<f64>().ok())
                .filter(|r| r.is_finite())
                .unwrap_or(1.0)
                .clamp(0.0, 1.0)
        };
        let (parent_based, ratio) = match name.map(str::trim).unwrap_or("") {
            "always_on" => (false, 1.0),
            "always_off" => (false, 0.0),
            "traceidratio" => (false, ratio()),
            "parentbased_always_off" => (true, 0.0),
            "parentbased_traceidratio" => (true, ratio()),
            _ => (true, 1.0),
        };
        Sampler {
            parent_based,
            ratio,
        }
    }

    fn should_sample(&self, parent: Option<&SpanContext>, trace_id: TraceId) -> bool {
        if let Some(parent) = parent.filter(|_| self.parent_based) {
            return parent.is_sampled();
        }
        if self.ratio >= 1.0 {
            return true;
        }
        if self.ratio <= 0.0 {
            return false;
        }
        // The opentelemetry-rust TraceIdRatioBased rule (Deno's too): the low
        // 64 bits of the trace id, halved, against the probability bound.
        let bytes = trace_id.to_bytes();
        let low = u64::from_be_bytes(bytes[8..16].try_into().unwrap());
        (low >> 1) < (self.ratio * (1u64 << 63) as f64) as u64
    }
}

/// Starts the exporter when `OJ_OTEL` is set; a bad configuration warns and
/// leaves oj running without it.
pub fn init() {
    if !oj_env::get().knobs.otel {
        return;
    }
    let config = OtelConfig {
        tracing_enabled: true,
        metrics_enabled: true,
        ..Default::default()
    };
    let runtime = OtelRuntimeConfig {
        runtime_name: "oj".into(),
        runtime_version: env!("CARGO_PKG_VERSION").into(),
    };
    if let Err(e) = oj_js::deno_telemetry::init(&sys_traits::impls::RealSys, runtime, config) {
        eprintln!("oj: OpenTelemetry export disabled: {e}");
        return;
    }
    let Some(globals) = OTEL_GLOBALS.get() else {
        return;
    };
    let exporter = DiagExporter::new(globals);
    oj_diag::add_output(Arc::new(move |event: &oj_diag::Event| {
        exporter.export(event)
    }));
}

/// Longest an exit waits for the last export. The OTLP exporter's own timeout
/// is 10s per signal by default, so an unreachable collector would otherwise
/// hold a Ctrl-C, or a restart on a config edit, for half a minute.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(2);

/// Exports what is queued and closes the pipeline, as deno_telemetry's own
/// exit hook does on `Deno.exit()`, without running the other exit hooks, and
/// within [`SHUTDOWN_BUDGET`]. oj calls it at each of its exits (the signal
/// shutdown, a restart's re-exec, `main` returning or unwinding). A signal can
/// also have deno_signals run Deno's hook first; the providers ignore a
/// second shutdown.
pub fn shutdown() {
    let Some(globals) = OTEL_GLOBALS.get() else {
        return;
    };
    let (done, finished) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("oj-otel-shutdown".into())
        .spawn(move || {
            let _ = globals.span_processor.shutdown();
            let _ = globals.log_processor.shutdown();
            let _ = globals.meter_provider.shutdown();
            let _ = done.send(());
        });
    if spawned.is_ok() {
        let _ = finished.recv_timeout(SHUTDOWN_BUDGET);
    }
}

/// Diagnostics as `oj.diag` log records and the `oj.diag.events` counter.
struct DiagExporter {
    globals: &'static OtelGlobals,
    scope: InstrumentationScope,
    events: Counter<u64>,
    /// opentelemetry 0.32 mints records only through a logger; this one has
    /// no processors, so records go out through Deno's batch processor.
    records: SdkLogger,
}

impl DiagExporter {
    fn new(globals: &'static OtelGlobals) -> Self {
        let scope = scope();
        let events = globals
            .meter_provider
            .meter_with_scope(scope.clone())
            .u64_counter("oj.diag.events")
            .with_description("Dev-server diagnostic events, folded repeats included")
            .build();
        Self {
            globals,
            scope,
            events,
            records: SdkLoggerProvider::builder().build().logger("oj"),
        }
    }

    fn export(&self, event: &oj_diag::Event) {
        self.events.add(
            1,
            &[
                KeyValue::new("oj.diag.kind", event.kind.as_str()),
                KeyValue::new("oj.diag.level", level_name(event.level)),
                KeyValue::new("oj.diag.source", source_name(event.source)),
            ],
        );
        let mut record = diag_record(&self.records, event);
        self.globals.log_processor.emit(&mut record, &self.scope);
    }
}

/// An `oj.diag` log record for `event`, stamped with when it happened (the
/// exporter runs a queue behind) and observed now.
fn diag_record(records: &SdkLogger, event: &oj_diag::Event) -> SdkLogRecord {
    let severity = match event.level {
        oj_diag::Level::Warn => Severity::Warn,
        oj_diag::Level::Error => Severity::Error,
        oj_diag::Level::Fatal => Severity::Fatal,
    };
    let mut record = records.create_log_record();
    record.set_timestamp(SystemTime::UNIX_EPOCH + Duration::from_millis(event.ts));
    record.set_observed_timestamp(SystemTime::now());
    record.set_event_name("oj.diag");
    record.set_severity_number(severity);
    record.set_severity_text(level_name(event.level));
    record.set_body(event.message.clone().into());
    record.add_attribute("oj.diag.kind", event.kind.as_str());
    record.add_attribute("oj.diag.source", source_name(event.source));
    if let Some(module) = &event.module {
        record.add_attribute("oj.diag.module", module.clone());
    }
    if let Some(plugin) = &event.plugin {
        record.add_attribute("oj.diag.plugin", plugin.clone());
    }
    if let Some(detail) = &event.detail {
        record.add_attribute("oj.diag.detail", detail.clone());
    }
    record
}

fn level_name(level: oj_diag::Level) -> &'static str {
    match level {
        oj_diag::Level::Warn => "warn",
        oj_diag::Level::Error => "error",
        oj_diag::Level::Fatal => "fatal",
    }
}

fn source_name(source: oj_diag::Source) -> &'static str {
    match source {
        oj_diag::Source::Server => "server",
        oj_diag::Source::Client => "client",
        oj_diag::Source::Plugin => "plugin",
        oj_diag::Source::Host => "host",
    }
}

/// The router layer's state: one server span (a child of an incoming W3C
/// `traceparent`) and one duration sample per request, both ending when the
/// body has been sent (as Vite's request timer does on `res.end`), not at
/// the headers. Attribute names follow the OTel HTTP server semantic
/// conventions; there is no route template, so `oj.route.class` stands in
/// for `http.route` and the span is named by the method alone.
pub struct RequestTracer {
    globals: &'static OtelGlobals,
    /// `base`: routes are classified past it, as oj's routes see them.
    base: Option<String>,
    scope: InstrumentationScope,
    duration: Histogram<f64>,
    sampler: Sampler,
}

/// `router` with a [`RequestTracer`] as its outermost layer, so the span and
/// the duration cover every other one; `router` itself when export is off.
pub fn traced<S: Clone + Send + Sync + 'static>(
    router: axum::Router<S>,
    scheme: &'static str,
    base: Option<&str>,
) -> axum::Router<S> {
    let Some(tracer) = RequestTracer::new(base) else {
        return router;
    };
    let tracer = Arc::new(tracer);
    router.layer(axum::middleware::from_fn(
        move |req: Request, next: Next| tracer.clone().trace(scheme, req, next),
    ))
}

impl RequestTracer {
    /// `None` when export is off.
    pub fn new(base: Option<&str>) -> Option<Self> {
        let globals = OTEL_GLOBALS.get()?;
        let scope = scope();
        let duration = globals
            .meter_provider
            .meter_with_scope(scope.clone())
            .f64_histogram("http.server.request.duration")
            .with_unit("s")
            .with_boundaries(vec![
                0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
            ])
            .build();
        Some(Self {
            globals,
            base: base.map(str::to_owned),
            scope,
            duration,
            sampler: Sampler::from_vars(
                oj_env::get().var("OTEL_TRACES_SAMPLER"),
                oj_env::get().var("OTEL_TRACES_SAMPLER_ARG"),
            ),
        })
    }

    pub async fn trace(
        self: Arc<Self>,
        scheme: &'static str,
        req: Request,
        next: Next,
    ) -> Response {
        let start_time = SystemTime::now();
        let started = Instant::now();
        let path = req.uri().path().to_owned();
        let mut exchange = Exchange {
            method_original: req.method().as_str().to_owned(),
            protocol: protocol_version(req.version()),
            class: classify(&path, self.base.as_deref()),
            path,
            scheme,
            status: StatusCode::OK,
            body: BodyEnd::Sent,
            parent: parse_parent(req.headers()),
        };
        let response = next.run(req).await;
        exchange.status = response.status();
        on_body_end(response, move |body| {
            exchange.body = body;
            let attributes = exchange.attributes();
            self.duration
                .record(started.elapsed().as_secs_f64(), &attributes);
            let trace_id = exchange.parent.as_ref().map_or_else(
                || self.globals.id_generator.new_trace_id(),
                SpanContext::trace_id,
            );
            if !self
                .sampler
                .should_sample(exchange.parent.as_ref(), trace_id)
            {
                return;
            }
            let limits = SpanLimits {
                attributes: self.globals.span_attribute_count_limit,
                value_length: self.globals.span_attribute_value_length_limit,
            };
            let span = exchange.span(
                attributes,
                (trace_id, self.globals.id_generator.new_span_id()),
                (start_time, SystemTime::now()),
                limits,
                self.scope.clone(),
            );
            self.globals.span_processor.on_end(span);
        })
    }
}

/// What one request contributes once its body is done: the duration
/// sample's attributes and, when sampled, its server span.
struct Exchange {
    method_original: String,
    protocol: Option<&'static str>,
    /// [`route_class`] of the path past `base`.
    class: &'static str,
    /// The request's own path, `base` included (`url.path`).
    path: String,
    scheme: &'static str,
    status: StatusCode,
    body: BodyEnd,
    parent: Option<SpanContext>,
}

/// How a response body ended.
#[derive(Debug, Clone, Copy, PartialEq)]
enum BodyEnd {
    /// Its last frame was read.
    Sent,
    /// It yielded an error mid-stream: the request failed after its status.
    Failed,
    /// Dropped before its end: the client went away, not the server's fault.
    Dropped,
}

/// Deno's span limits (`OTEL_SPAN_ATTRIBUTE_COUNT_LIMIT`,
/// `OTEL_ATTRIBUTE_VALUE_LENGTH_LIMIT`), which its own spans honor.
#[derive(Debug, Clone, Copy)]
struct SpanLimits {
    attributes: usize,
    value_length: Option<usize>,
}

impl Exchange {
    fn failed(&self) -> bool {
        self.status.is_server_error() || self.body == BodyEnd::Failed
    }

    /// The low-cardinality set the histogram and the span share.
    fn attributes(&self) -> Vec<KeyValue> {
        let mut attributes = vec![
            KeyValue::new("http.request.method", known_method(&self.method_original)),
            KeyValue::new("http.response.status_code", i64::from(self.status.as_u16())),
            KeyValue::new("url.scheme", self.scheme),
            KeyValue::new("oj.route.class", self.class),
        ];
        if let Some(protocol) = self.protocol {
            attributes.push(KeyValue::new("network.protocol.version", protocol));
        }
        // Server spans: a 4xx is the client's error, not the server's. A body
        // that failed after its status has no code of its own.
        if self.status.is_server_error() {
            attributes.push(KeyValue::new("error.type", self.status.as_str().to_owned()));
        } else if self.body == BodyEnd::Failed {
            attributes.push(KeyValue::new("error.type", "_OTHER"));
        }
        attributes
    }

    fn span(
        self,
        mut attributes: Vec<KeyValue>,
        (trace_id, span_id): (TraceId, SpanId),
        (start_time, end_time): (SystemTime, SystemTime),
        limits: SpanLimits,
        scope: InstrumentationScope,
    ) -> SpanData {
        let method = known_method(&self.method_original);
        let failed = self.failed();
        attributes.push(KeyValue::new("url.path", self.path));
        if method == "_OTHER" {
            attributes.push(KeyValue::new(
                "http.request.method_original",
                self.method_original,
            ));
        }
        let dropped = attributes.len().saturating_sub(limits.attributes);
        attributes.truncate(limits.attributes);
        if let Some(max) = limits.value_length {
            for kv in &mut attributes {
                if let opentelemetry::Value::String(text) = &kv.value {
                    if let Some((at, _)) = text.as_str().char_indices().nth(max) {
                        kv.value = text.as_str()[..at].to_owned().into();
                    }
                }
            }
        }
        SpanData {
            // A child carries its parent's vendor state (W3C tracestate).
            span_context: SpanContext::new(
                trace_id,
                span_id,
                TraceFlags::SAMPLED,
                false,
                self.parent
                    .as_ref()
                    .map_or_else(TraceState::default, |p| p.trace_state().clone()),
            ),
            parent_span_id: self
                .parent
                .as_ref()
                .map_or(SpanId::INVALID, SpanContext::span_id),
            parent_span_is_remote: self.parent.is_some(),
            span_kind: SpanKind::Server,
            name: if method == "_OTHER" { "HTTP" } else { method }.into(),
            start_time,
            end_time,
            attributes,
            dropped_attributes_count: dropped as u32,
            events: SpanEvents::default(),
            links: SpanLinks::default(),
            status: if failed {
                Status::error("")
            } else {
                Status::Unset
            },
            instrumentation_scope: scope,
        }
    }
}

/// `http.request.method`: the known methods as-is, anything else `_OTHER`
/// so a scanner's junk verbs cannot blow up metric cardinality.
fn known_method(method: &str) -> &'static str {
    match method {
        "GET" => "GET",
        "HEAD" => "HEAD",
        "POST" => "POST",
        "PUT" => "PUT",
        "DELETE" => "DELETE",
        "CONNECT" => "CONNECT",
        "OPTIONS" => "OPTIONS",
        "TRACE" => "TRACE",
        "PATCH" => "PATCH",
        _ => "_OTHER",
    }
}

fn protocol_version(version: axum::http::Version) -> Option<&'static str> {
    use axum::http::Version;
    match version {
        Version::HTTP_09 => Some("0.9"),
        Version::HTTP_10 => Some("1.0"),
        Version::HTTP_11 => Some("1.1"),
        Version::HTTP_2 => Some("2"),
        Version::HTTP_3 => Some("3"),
        _ => None,
    }
}

type OnEnd = Box<dyn FnOnce(BodyEnd) + Send>;

/// Runs `end` once the response body is done, with how it ended: its last
/// frame was read (or it was empty), it failed, or it was dropped first (an
/// aborted connection, a body never polled).
fn on_body_end(response: Response, end: impl FnOnce(BodyEnd) + Send + 'static) -> Response {
    response.map(|inner| {
        Body::new(EndBody {
            inner,
            end: Some(Box::new(end)),
        })
    })
}

struct EndBody {
    inner: Body,
    end: Option<OnEnd>,
}

impl EndBody {
    fn finish(&mut self, how: BodyEnd) {
        if let Some(end) = self.end.take() {
            end(how);
        }
    }
}

impl HttpBody for EndBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let this = self.get_mut();
        let frame = Pin::new(&mut this.inner).poll_frame(cx);
        match &frame {
            Poll::Ready(None) => this.finish(BodyEnd::Sent),
            Poll::Ready(Some(Err(_))) => this.finish(BodyEnd::Failed),
            _ => {}
        }
        frame
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for EndBody {
    /// hyper never polls a body that reports its end up front (an empty one).
    fn drop(&mut self) {
        let how = if self.inner.is_end_stream() {
            BodyEnd::Sent
        } else {
            BodyEnd::Dropped
        };
        self.finish(how);
    }
}

/// The incoming W3C trace context: `traceparent` as
/// `<version>-<trace id>-<parent id>-<flags>` in lowercase hex, with no
/// trailing fields for version `00` (later versions may append some), and
/// its `tracestate`. A malformed `traceparent` starts a new trace; a
/// malformed `tracestate` alone is dropped.
fn parse_parent(headers: &HeaderMap) -> Option<SpanContext> {
    let lower_hex = |s: &str, len: usize| {
        s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    };
    let value = headers.get("traceparent")?.to_str().ok()?;
    let mut parts = value.trim().split('-');
    let (version, trace, span, flags) =
        (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if !lower_hex(version, 2)
        || version == "ff"
        || (version == "00" && parts.next().is_some())
        || !lower_hex(trace, 32)
        || !lower_hex(span, 16)
        || !lower_hex(flags, 2)
    {
        return None;
    }
    let state = headers
        .get("tracestate")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| TraceState::from_str(v).ok())
        .unwrap_or_default();
    let context = SpanContext::new(
        TraceId::from_hex(trace).ok()?,
        SpanId::from_hex(span).ok()?,
        // Only the sampled bit is defined; W3C has unknown flags zeroed.
        TraceFlags::new(u8::from_str_radix(flags, 16).ok()?) & TraceFlags::SAMPLED,
        true,
        state,
    );
    context.is_valid().then_some(context)
}

/// [`route_class`] of `path` as oj's routes see it: past `base`. A path
/// outside the base keeps its own name (oj's unbased module URLs).
fn classify(path: &str, base: Option<&str>) -> &'static str {
    let route = base
        .and_then(|base| crate::strip_base(path, base))
        .unwrap_or(path);
    route_class(route)
}

/// A low-cardinality name for what a dev-server path (past `base`) serves.
/// Only a known file extension makes a path a file: page URLs carry dots too
/// (`/users/jane.doe`, `/docs/v1.2`).
fn route_class(path: &str) -> &'static str {
    if path.starts_with("/@oj/") || path.starts_with("/@vite/") || path.starts_with("/__") {
        return "internal";
    }
    if path.starts_with("/@oj-deps/") || path.contains("/node_modules/.vite/") {
        return "dep";
    }
    // TanStack Start's server-function RPCs (start_dev routes the prefix).
    if path.starts_with("/_serverFn/") {
        return "server_fn";
    }
    if path.starts_with("/@id/") || path.starts_with("/@fs/") {
        return "module";
    }
    let file = path.rsplit('/').next().unwrap_or("");
    let ext = file
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase());
    let class = match ext.as_deref() {
        Some(
            "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "mts" | "cts" | "vue" | "svelte" | "json",
        ) => Some("module"),
        Some("css" | "scss" | "sass" | "less" | "styl" | "stylus" | "pcss" | "postcss") => {
            Some("style")
        }
        Some(
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "svg" | "ico" | "bmp" | "tif"
            | "tiff" | "apng" | "jfif" | "pjpeg" | "pjp" | "cur" | "woff" | "woff2" | "ttf" | "otf"
            | "eot" | "mp4" | "webm" | "ogg" | "mp3" | "wav" | "flac" | "aac" | "opus" | "mov"
            | "m4a" | "vtt" | "wasm" | "map" | "txt" | "xml" | "pdf" | "webmanifest" | "zip",
        ) => Some("asset"),
        _ => None,
    };
    if let Some(class) = class {
        return class;
    }
    // Other `/@` paths are dev-server modules (`/@react-refresh`,
    // `/@virtual/..`, `/@oj-empty`), never pages.
    if path.starts_with("/@") {
        return "module";
    }
    "document"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn methods_and_protocols_map_to_semconv_values() {
        assert_eq!(known_method("GET"), "GET");
        assert_eq!(known_method("PATCH"), "PATCH");
        assert_eq!(known_method("get"), "_OTHER");
        assert_eq!(known_method("PROPFIND"), "_OTHER");
        assert_eq!(protocol_version(axum::http::Version::HTTP_11), Some("1.1"));
        assert_eq!(protocol_version(axum::http::Version::HTTP_2), Some("2"));
    }

    #[test]
    fn routes_classify_by_prefix_then_extension() {
        assert_eq!(route_class("/@oj/client.js"), "internal");
        assert_eq!(route_class("/__ws"), "internal");
        assert_eq!(route_class("/@oj-deps/react.js"), "dep");
        assert_eq!(route_class("/@fs/app/src/x.ts"), "module");
        assert_eq!(route_class("/src/App.tsx"), "module");
        assert_eq!(route_class("/@react-refresh"), "module");
        assert_eq!(route_class("/@virtual/routes"), "module");
        assert_eq!(route_class("/@oj-empty"), "module");
        assert_eq!(route_class("/@ssr-module"), "module");
        assert_eq!(route_class("/@oj-start/client.css"), "style");
        assert_eq!(route_class("/src/index.css"), "style");
        assert_eq!(route_class("/logo.svg"), "asset");
        assert_eq!(route_class("/"), "document");
        assert_eq!(route_class("/dashboard/settings"), "document");
        assert_eq!(route_class("/index.html"), "document");
        assert_eq!(route_class("/@vite/client"), "internal");
        assert_eq!(route_class("/node_modules/.vite/deps/react.js"), "dep");
        assert_eq!(route_class("/_serverFn/src_fns_ts--getUser"), "server_fn");
        assert_eq!(route_class("/assets/app.js.map"), "asset");
        assert_eq!(route_class("/Logo.PNG"), "asset");
        assert_eq!(route_class("/fonts/inter.woff2"), "asset");
        assert_eq!(route_class("/@virtual/page.html"), "module");
    }

    // Page URLs carry dots too: only a known file extension makes a file.
    #[test]
    fn a_dotted_page_url_is_still_a_document() {
        for page in [
            "/users/jane.doe",
            "/docs/v1.2",
            "/posts/hello.world",
            "/app/",
        ] {
            assert_eq!(route_class(page), "document", "{page}");
        }
    }

    // With `base: '/app/'`, oj's routes see `/app/@oj/client.js` as
    // `/@oj/client.js`; its class must too. Unbased module URLs stay as is.
    #[test]
    fn routes_classify_past_the_base() {
        let base = Some("/app/");
        assert_eq!(classify("/app/@oj/client.js", base), "internal");
        assert_eq!(classify("/app/@oj-deps/react.js", base), "dep");
        assert_eq!(classify("/app/__ws", base), "internal");
        assert_eq!(classify("/app/", base), "document");
        assert_eq!(classify("/app", base), "document");
        assert_eq!(
            classify("/src/App.tsx", base),
            "module",
            "an unbased module URL"
        );
        assert_eq!(classify("/apple.svg", base), "asset", "not under /app/");
        assert_eq!(
            classify("/app/@oj/client.js", None),
            "module",
            "without the base it is no oj route, just a script"
        );
    }

    fn headers(traceparent: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("traceparent", traceparent.parse().unwrap());
        headers
    }

    #[test]
    fn traceparent_parses_the_w3c_shape_only() {
        let context = parse_parent(&headers(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        ))
        .unwrap();
        assert_eq!(
            context.trace_id(),
            TraceId::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").unwrap()
        );
        assert_eq!(
            context.span_id(),
            SpanId::from_hex("00f067aa0ba902b7").unwrap()
        );
        assert!(context.is_sampled());
        assert!(context.is_remote());
        let unsampled = parse_parent(&headers(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00",
        ))
        .unwrap();
        assert!(!unsampled.is_sampled());
        for bad in [
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e47-00f067aa0ba902b7-01",
            "00-zzf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "garbage",
        ] {
            assert!(parse_parent(&headers(bad)).is_none(), "{bad}");
        }
        assert!(parse_parent(&HeaderMap::new()).is_none());
    }

    type Feed = tokio::sync::mpsc::UnboundedSender<Result<Bytes, std::io::Error>>;
    type Ends = Arc<std::sync::Mutex<Vec<BodyEnd>>>;

    /// A response whose body is fed through `tx`, and how its end was seen.
    fn streamed() -> (Feed, Response, Ends) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<Bytes, std::io::Error>>();
        let stream = futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        let ends = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = ends.clone();
        let response = on_body_end(Response::new(Body::from_stream(stream)), move |how| {
            seen.lock().unwrap().push(how);
        });
        (tx, response, ends)
    }

    #[tokio::test]
    async fn body_end_waits_for_the_last_frame() {
        use futures_util::StreamExt as _;
        let (tx, response, ends) = streamed();
        let mut body = response.into_body().into_data_stream();
        tx.send(Ok(Bytes::from_static(b"head"))).unwrap();
        assert_eq!(body.next().await.unwrap().unwrap(), "head");
        assert!(
            ends.lock().unwrap().is_empty(),
            "headers and a first chunk are not the end"
        );
        tx.send(Ok(Bytes::from_static(b"tail"))).unwrap();
        drop(tx);
        assert_eq!(body.next().await.unwrap().unwrap(), "tail");
        assert!(body.next().await.is_none());
        assert_eq!(
            *ends.lock().unwrap(),
            [BodyEnd::Sent],
            "the last frame ends it"
        );
        drop(body);
        assert_eq!(*ends.lock().unwrap(), [BodyEnd::Sent], "ends once");
    }

    #[tokio::test]
    async fn a_body_ends_failed_or_dropped_when_it_never_finishes() {
        use futures_util::StreamExt as _;
        let (tx, response, ends) = streamed();
        let mut body = response.into_body().into_data_stream();
        tx.send(Err(std::io::Error::other("render threw"))).unwrap();
        assert!(body.next().await.unwrap().is_err());
        drop(body);
        assert_eq!(
            *ends.lock().unwrap(),
            [BodyEnd::Failed],
            "a failed frame is the end"
        );

        let (tx, response, ends) = streamed();
        let mut body = response.into_body().into_data_stream();
        tx.send(Ok(Bytes::from_static(b"half"))).unwrap();
        assert_eq!(body.next().await.unwrap().unwrap(), "half");
        drop(body);
        assert_eq!(
            *ends.lock().unwrap(),
            [BodyEnd::Dropped],
            "the client went away"
        );

        let ends = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = ends.clone();
        drop(on_body_end(Response::new(Body::empty()), move |how| {
            seen.lock().unwrap().push(how)
        }));
        assert_eq!(
            *ends.lock().unwrap(),
            [BodyEnd::Sent],
            "an empty body is never polled"
        );
    }

    #[test]
    fn sampler_follows_the_otel_traces_sampler_vars() {
        let sampled = SpanContext::new(
            TraceId::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").unwrap(),
            SpanId::from_hex("00f067aa0ba902b7").unwrap(),
            TraceFlags::SAMPLED,
            true,
            TraceState::default(),
        );
        let unsampled = SpanContext::new(
            sampled.trace_id(),
            sampled.span_id(),
            TraceFlags::default(),
            true,
            TraceState::default(),
        );
        let root = TraceId::from_hex("00000000000000000000000000000001").unwrap();
        let high = TraceId::from_hex("0000000000000000ffffffffffffffff").unwrap();

        let default = Sampler::from_vars(None, None);
        assert_eq!(default, Sampler::from_vars(Some(""), None));
        assert!(default.should_sample(None, root));
        assert!(default.should_sample(Some(&sampled), root));
        assert!(!default.should_sample(Some(&unsampled), root));

        let off = Sampler::from_vars(Some("always_off"), None);
        assert!(!off.should_sample(None, root));
        assert!(!off.should_sample(Some(&sampled), root));

        let on = Sampler::from_vars(Some("always_on"), None);
        assert!(on.should_sample(Some(&unsampled), root));

        let ratio = Sampler::from_vars(Some("traceidratio"), Some("0.5"));
        assert!(ratio.should_sample(None, root));
        assert!(!ratio.should_sample(None, high));
        assert!(!ratio.should_sample(Some(&sampled), high));

        let parent_ratio = Sampler::from_vars(Some("parentbased_traceidratio"), Some("0.5"));
        assert!(parent_ratio.should_sample(Some(&sampled), high));
        assert!(!parent_ratio.should_sample(None, high));

        let parent_off = Sampler::from_vars(Some("parentbased_always_off"), None);
        assert!(!parent_off.should_sample(None, root));
        assert!(parent_off.should_sample(Some(&sampled), root));

        assert_eq!(
            Sampler::from_vars(Some("traceidratio"), Some("7")).ratio,
            1.0
        );
        assert_eq!(Sampler::from_vars(Some("traceidratio"), None).ratio, 1.0);
    }

    const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
    const PARENT: &str = "00f067aa0ba902b7";

    fn context(traceparent: &str, tracestate: Option<&str>) -> Option<SpanContext> {
        let mut headers = headers(traceparent);
        if let Some(state) = tracestate {
            headers.insert("tracestate", state.parse().unwrap());
        }
        parse_parent(&headers)
    }

    // W3C trace-context: lowercase hex throughout, a hex version other than
    // ff, and no trailing fields on version 00 (later versions may add some).
    #[test]
    fn traceparent_follows_the_w3c_field_rules() {
        let valid = format!("00-{TRACE}-{PARENT}-01");
        assert!(context(&valid, None).is_some());
        for bad in [
            format!("00-{}-{PARENT}-01", TRACE.to_uppercase()),
            format!("00-{TRACE}-{}-01", PARENT.to_uppercase()),
            format!("0g-{TRACE}-{PARENT}-01"),
            format!("zz-{TRACE}-{PARENT}-01"),
            format!("00-{TRACE}-{PARENT}-0x"),
            format!("00-{TRACE}-{PARENT}-01-extra"),
            format!("00-{TRACE}-{PARENT}"),
        ] {
            assert!(context(&bad, None).is_none(), "{bad}");
        }
        let future = context(&format!("01-{TRACE}-{PARENT}-01-what-comes-next"), None)
            .expect("a later version may append fields");
        assert_eq!(future.trace_id(), TraceId::from_hex(TRACE).unwrap());
    }

    #[test]
    fn tracestate_rides_along_and_a_bad_one_alone_is_dropped() {
        let traceparent = format!("00-{TRACE}-{PARENT}-01");
        let parent = context(&traceparent, Some("vendor=abc,other=1")).unwrap();
        assert_eq!(parent.trace_state().get("vendor"), Some("abc"));
        assert_eq!(parent.trace_state().get("other"), Some("1"));
        let kept = context(&traceparent, Some("not a list entry")).unwrap();
        assert_eq!(
            kept.trace_state().header(),
            "",
            "the parent survives its bad state"
        );
    }

    fn exchange(method: &str, status: u16, parent: Option<SpanContext>) -> Exchange {
        Exchange {
            method_original: method.into(),
            protocol: Some("1.1"),
            class: route_class("/src/App.tsx"),
            path: "/src/App.tsx".into(),
            scheme: "https",
            status: StatusCode::from_u16(status).unwrap(),
            body: BodyEnd::Sent,
            parent,
        }
    }

    fn value<'a>(attributes: &'a [KeyValue], key: &str) -> Option<&'a opentelemetry::Value> {
        attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| &kv.value)
    }

    // The histogram's attributes stay low-cardinality: no path, and an error
    // type only for the server's own failures.
    #[test]
    fn request_attributes_follow_http_server_semconv() {
        let ok = exchange("GET", 200, None).attributes();
        let keys: Vec<&str> = ok.iter().map(|kv| kv.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "http.request.method",
                "http.response.status_code",
                "url.scheme",
                "oj.route.class",
                "network.protocol.version"
            ]
        );
        assert_eq!(
            value(&ok, "http.response.status_code"),
            Some(&200i64.into())
        );
        assert_eq!(value(&ok, "url.scheme"), Some(&"https".into()));
        assert_eq!(value(&ok, "oj.route.class"), Some(&"module".into()));
        assert!(value(&exchange("GET", 404, None).attributes(), "error.type").is_none());
        assert_eq!(
            value(&exchange("GET", 503, None).attributes(), "error.type"),
            Some(&"503".into())
        );
        assert_eq!(
            value(
                &exchange("PROPFIND", 200, None).attributes(),
                "http.request.method"
            ),
            Some(&"_OTHER".into())
        );
    }

    const NO_LIMITS: SpanLimits = SpanLimits {
        attributes: 128,
        value_length: None,
    };

    fn span(exchange: Exchange) -> SpanData {
        span_with(exchange, NO_LIMITS)
    }

    fn span_with(exchange: Exchange, limits: SpanLimits) -> SpanData {
        let attributes = exchange.attributes();
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
        exchange.span(
            attributes,
            (
                TraceId::from_hex(TRACE).unwrap(),
                SpanId::from_hex("1111111111111111").unwrap(),
            ),
            (start, start + Duration::from_millis(5)),
            limits,
            scope(),
        )
    }

    #[test]
    fn a_traced_request_is_its_parents_child() {
        let parent = context(&format!("00-{TRACE}-{PARENT}-01"), Some("vendor=abc"));
        let span = span(exchange("GET", 200, parent));
        assert_eq!(span.name, "GET");
        assert_eq!(span.span_kind, SpanKind::Server);
        assert_eq!(span.parent_span_id, SpanId::from_hex(PARENT).unwrap());
        assert!(span.parent_span_is_remote);
        assert!(
            !span.span_context.is_remote(),
            "the server span itself is local"
        );
        assert!(span.span_context.is_sampled());
        assert_eq!(span.span_context.trace_state().get("vendor"), Some("abc"));
        assert_eq!(span.status, Status::Unset);
        assert_eq!(
            value(&span.attributes, "url.path"),
            Some(&"/src/App.tsx".into())
        );
        assert_eq!(
            span.end_time.duration_since(span.start_time).unwrap(),
            Duration::from_millis(5)
        );
        assert_eq!(span.instrumentation_scope.name(), "oj");
    }

    #[test]
    fn a_root_span_has_no_parent_and_errors_mark_it() {
        let failed = span(exchange("POST", 500, None));
        assert_eq!(failed.parent_span_id, SpanId::INVALID);
        assert!(!failed.parent_span_is_remote);
        assert_eq!(failed.status, Status::error(""));
        assert_eq!(value(&failed.attributes, "error.type"), Some(&"500".into()));
        let odd = span(exchange("PROPFIND", 207, None));
        assert_eq!(odd.name, "HTTP", "an unknown method names no span");
        assert_eq!(
            value(&odd.attributes, "http.request.method_original"),
            Some(&"PROPFIND".into())
        );
        assert!(value(
            &span(exchange("GET", 200, None)).attributes,
            "http.request.method_original"
        )
        .is_none());
    }

    fn attribute<'a>(
        record: &'a SdkLogRecord,
        key: &str,
    ) -> Option<&'a opentelemetry::logs::AnyValue> {
        record
            .attributes_iter()
            .find(|(k, _)| k.as_str() == key)
            .map(|(_, v)| v)
    }

    // The exporter runs a queue behind the emitter, so a record carries the
    // event's own time, and every field a person debugging needs.
    #[test]
    fn a_diagnostic_becomes_an_oj_diag_record() {
        let logger = SdkLoggerProvider::builder().build().logger("oj");
        let mut event = oj_diag::Event::new(oj_diag::Kind::CompileError, "Unexpected token")
            .source(oj_diag::Source::Plugin)
            .module("/src/main.tsx".to_string())
            .plugin("vite:react".to_string())
            .detail("  1 | export const broken = {;");
        event.ts = 1_700_000_000_123;
        let record = diag_record(&logger, &event);
        assert_eq!(
            record.timestamp(),
            Some(SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_123))
        );
        assert!(record.observed_timestamp() > record.timestamp());
        assert_eq!(record.event_name(), Some("oj.diag"));
        assert_eq!(record.severity_number(), Some(Severity::Error));
        assert_eq!(record.severity_text(), Some("error"));
        assert_eq!(record.body(), Some(&"Unexpected token".into()));
        assert_eq!(
            attribute(&record, "oj.diag.kind"),
            Some(&"compile_error".into())
        );
        assert_eq!(attribute(&record, "oj.diag.source"), Some(&"plugin".into()));
        assert_eq!(
            attribute(&record, "oj.diag.module"),
            Some(&"/src/main.tsx".into())
        );
        assert_eq!(
            attribute(&record, "oj.diag.plugin"),
            Some(&"vite:react".into())
        );
        assert_eq!(
            attribute(&record, "oj.diag.detail"),
            Some(&"  1 | export const broken = {;".into())
        );

        let bare = diag_record(
            &logger,
            &oj_diag::Event::new(oj_diag::Kind::HostGone, "gone").fatal(),
        );
        assert_eq!(bare.severity_number(), Some(Severity::Fatal));
        assert_eq!(bare.severity_text(), Some("fatal"));
        for absent in ["oj.diag.module", "oj.diag.plugin", "oj.diag.detail"] {
            assert!(attribute(&bare, absent).is_none(), "{absent}");
        }
    }

    #[test]
    fn a_bad_sampler_probability_is_the_default() {
        for bad in ["NaN", "inf", "-inf", "half", ""] {
            assert_eq!(
                Sampler::from_vars(Some("traceidratio"), Some(bad)).ratio,
                1.0,
                "{bad}"
            );
        }
        assert_eq!(
            Sampler::from_vars(Some("traceidratio"), Some("-0.5")).ratio,
            0.0
        );
        assert_eq!(
            Sampler::from_vars(Some(" always_off "), None).ratio,
            0.0,
            "names are trimmed"
        );
    }

    // The wrapper must keep the body's exact length, or every response would
    // lose its Content-Length and go out chunked.
    #[test]
    fn the_body_wrapper_keeps_the_size_hint() {
        let response = on_body_end(Response::new(Body::from("hello")), |_| {});
        let body = response.into_body();
        assert_eq!(body.size_hint().exact(), Some(5));
        assert!(!body.is_end_stream());
        let empty = on_body_end(Response::new(Body::empty()), |_| {}).into_body();
        assert!(empty.is_end_stream());
    }

    // A body that fails after its status was sent is the server's failure;
    // one the client abandoned is not.
    #[test]
    fn a_failed_body_marks_the_request_and_a_dropped_one_does_not() {
        let mut failed = exchange("GET", 200, None);
        failed.body = BodyEnd::Failed;
        assert_eq!(
            value(&failed.attributes(), "error.type"),
            Some(&"_OTHER".into())
        );
        assert_eq!(span(failed).status, Status::error(""));
        let mut dropped = exchange("GET", 200, None);
        dropped.body = BodyEnd::Dropped;
        assert!(value(&dropped.attributes(), "error.type").is_none());
        assert_eq!(span(dropped).status, Status::Unset);
    }

    // Deno's own spans honor OTEL_SPAN_ATTRIBUTE_COUNT_LIMIT and
    // OTEL_ATTRIBUTE_VALUE_LENGTH_LIMIT; these spans are built by hand, so
    // they apply them here.
    #[test]
    fn spans_honor_denos_attribute_limits() {
        let mut long = exchange("GET", 200, None);
        long.path = format!("/{}é", "a".repeat(40));
        let limited = span_with(
            long,
            SpanLimits {
                attributes: 3,
                value_length: Some(8),
            },
        );
        assert_eq!(limited.attributes.len(), 3);
        assert_eq!(
            limited.dropped_attributes_count, 3,
            "the rest is counted, not lost silently"
        );
        assert_eq!(
            value(&limited.attributes, "url.scheme"),
            Some(&"https".into())
        );
        let mut long = exchange("GET", 200, None);
        long.path = format!("/{}é", "a".repeat(6));
        let cut = span_with(
            long,
            SpanLimits {
                attributes: 128,
                value_length: Some(8),
            },
        );
        assert_eq!(
            value(&cut.attributes, "url.path"),
            Some(&"/aaaaaaé".into()),
            "cut by characters, never inside one"
        );
        assert_eq!(
            value(&cut.attributes, "http.response.status_code"),
            Some(&200i64.into()),
            "only strings are cut"
        );
    }

    #[test]
    fn unknown_trace_flags_are_zeroed() {
        let all = context(&format!("00-{TRACE}-{PARENT}-ff"), None).unwrap();
        assert_eq!(all.trace_flags(), TraceFlags::SAMPLED);
        let unsampled = context(&format!("00-{TRACE}-{PARENT}-fe"), None).unwrap();
        assert_eq!(unsampled.trace_flags(), TraceFlags::default());
        for signed in [
            format!("00-{TRACE}-{PARENT}-+1"),
            format!("+0-{TRACE}-{PARENT}-01"),
            format!("0-{TRACE}-{PARENT}-01"),
            format!("00-{TRACE}-{PARENT}-1"),
            format!("00-{}-{PARENT}-01", &TRACE[..31]),
        ] {
            assert!(context(&signed, None).is_none(), "{signed}");
        }
    }
}
