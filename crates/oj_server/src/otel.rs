//! OpenTelemetry export (`OJ_OTEL=1`): diagnostics as log records plus an
//! event counter, and a span and duration sample per dev-server request.
//! Rides the embedded Deno's OTLP pipeline, so the standard
//! `OTEL_EXPORTER_OTLP_*`, `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES`
//! variables configure it.
//!
//! oj keeps no state of its own here: the diagnostics exporter is a thread
//! that owns its instruments and drains an [`oj_diag::subscribe`] queue, and
//! the router layer owns a [`RequestTracer`]. The pipeline is deno_telemetry's
//! (`OTEL_GLOBALS`), and so is its shutdown, the exit hook `Deno.exit()` runs.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Instant, SystemTime};

use axum::body::{Body, Bytes, HttpBody};
use axum::extract::Request;
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;
use deno_telemetry::{OtelConfig, OtelGlobals, OtelRuntimeConfig, OTEL_GLOBALS};
use hyper::body::{Frame, SizeHint};
use opentelemetry::logs::{LogRecord as _, Logger as _, LoggerProvider as _, Severity};
use opentelemetry::metrics::{Counter, Histogram, MeterProvider as _};
use opentelemetry::trace::{
    SpanContext, SpanId, SpanKind, Status, TraceFlags, TraceId, TraceState,
};
use opentelemetry::{InstrumentationScope, KeyValue};
use opentelemetry_sdk::logs::{LogProcessor as _, SdkLogger, SdkLoggerProvider};
use opentelemetry_sdk::trace::{
    IdGenerator as _, SpanData, SpanEvents, SpanLinks, SpanProcessor as _,
};

/// Diagnostics waiting for the exporter thread; past it they are counted as
/// unsent in `/@oj/diagnostics` instead of blocking whoever emitted them.
const DIAG_QUEUE: usize = 1024;

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
    /// Unset or empty is the spec default, parentbased_always_on. Deno's init
    /// already refuses an unsupported name, so one never reaches here.
    fn from_vars(name: Option<&str>, arg: Option<&str>) -> Sampler {
        let ratio = || {
            arg.and_then(|s| s.trim().parse::<f64>().ok())
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
    if let Err(e) = deno_telemetry::init(&sys_traits::impls::RealSys, runtime, config) {
        eprintln!("oj: OpenTelemetry export disabled: {e}");
        return;
    }
    let Some(globals) = OTEL_GLOBALS.get() else {
        return;
    };
    let events = oj_diag::subscribe(DIAG_QUEUE);
    let exporter = DiagExporter::new(globals);
    let spawned = std::thread::Builder::new()
        .name("oj-otel-diag".into())
        .spawn(move || {
            for event in events {
                exporter.export(&event);
            }
        });
    if let Err(e) = spawned {
        eprintln!("oj: OpenTelemetry diagnostics export disabled: {e}");
    }
}

/// Exports what is queued and closes the pipeline: deno_telemetry's exit
/// hook, the one `Deno.exit()` runs. oj runs it at each of its own exits (the
/// signal shutdown, a restart's re-exec, `main` returning). A signal can also
/// have deno_signals run it first; the SDK providers ignore a second shutdown.
pub fn shutdown() {
    if OTEL_GLOBALS.get().is_some() {
        deno_signals::run_exit();
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
        let level = match event.level {
            oj_diag::Level::Warn => "warn",
            oj_diag::Level::Error => "error",
            oj_diag::Level::Fatal => "fatal",
        };
        let source = match event.source {
            oj_diag::Source::Server => "server",
            oj_diag::Source::Client => "client",
            oj_diag::Source::Plugin => "plugin",
            oj_diag::Source::Host => "host",
        };
        self.events.add(
            1,
            &[
                KeyValue::new("oj.diag.kind", event.kind.as_str()),
                KeyValue::new("oj.diag.level", level),
                KeyValue::new("oj.diag.source", source),
            ],
        );
        let mut record = self.records.create_log_record();
        let now = SystemTime::now();
        record.set_timestamp(now);
        record.set_observed_timestamp(now);
        record.set_event_name("oj.diag");
        record.set_severity_number(match event.level {
            oj_diag::Level::Warn => Severity::Warn,
            oj_diag::Level::Error => Severity::Error,
            oj_diag::Level::Fatal => Severity::Fatal,
        });
        record.set_severity_text(level);
        record.set_body(event.message.clone().into());
        record.add_attribute("oj.diag.kind", event.kind.as_str());
        record.add_attribute("oj.diag.source", source);
        if let Some(module) = &event.module {
            record.add_attribute("oj.diag.module", module.clone());
        }
        if let Some(plugin) = &event.plugin {
            record.add_attribute("oj.diag.plugin", plugin.clone());
        }
        self.globals.log_processor.emit(&mut record, &self.scope);
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
    scope: InstrumentationScope,
    duration: Histogram<f64>,
    sampler: Sampler,
}

impl RequestTracer {
    /// `None` when export is off.
    pub fn new() -> Option<Self> {
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
        let method_original = req.method().as_str().to_owned();
        let protocol = protocol_version(req.version());
        let path = req.uri().path().to_owned();
        let parent = parse_traceparent(req.headers());
        let response = next.run(req).await;
        let status = response.status();
        on_body_end(response, move || {
            let class = route_class(&path);
            let method = known_method(&method_original);
            let mut attributes = vec![
                KeyValue::new("http.request.method", method),
                KeyValue::new("http.response.status_code", i64::from(status.as_u16())),
                KeyValue::new("url.scheme", scheme),
                KeyValue::new("oj.route.class", class),
            ];
            if let Some(protocol) = protocol {
                attributes.push(KeyValue::new("network.protocol.version", protocol));
            }
            if status.is_server_error() {
                attributes.push(KeyValue::new("error.type", status.as_str().to_owned()));
            }
            self.duration
                .record(started.elapsed().as_secs_f64(), &attributes);
            let trace_id = parent.as_ref().map_or_else(
                || self.globals.id_generator.new_trace_id(),
                SpanContext::trace_id,
            );
            if !self.sampler.should_sample(parent.as_ref(), trace_id) {
                return;
            }
            let mut span_attributes = attributes;
            span_attributes.push(KeyValue::new("url.path", path));
            if method == "_OTHER" {
                span_attributes.push(KeyValue::new(
                    "http.request.method_original",
                    method_original,
                ));
            }
            self.globals.span_processor.on_end(SpanData {
                span_context: SpanContext::new(
                    trace_id,
                    self.globals.id_generator.new_span_id(),
                    TraceFlags::SAMPLED,
                    false,
                    TraceState::default(),
                ),
                parent_span_id: parent
                    .as_ref()
                    .map_or(SpanId::INVALID, SpanContext::span_id),
                parent_span_is_remote: parent.is_some(),
                span_kind: SpanKind::Server,
                name: if method == "_OTHER" { "HTTP" } else { method }.into(),
                start_time,
                end_time: SystemTime::now(),
                attributes: span_attributes,
                dropped_attributes_count: 0,
                events: SpanEvents::default(),
                links: SpanLinks::default(),
                status: if status.is_server_error() {
                    Status::error("")
                } else {
                    Status::Unset
                },
                instrumentation_scope: self.scope.clone(),
            });
        })
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

type OnEnd = Box<dyn FnOnce() + Send>;

/// Runs `end` once the response body is done: its last frame was read, or
/// it was dropped (fully sent, an aborted connection, or a body never polled).
fn on_body_end(response: Response, end: impl FnOnce() + Send + 'static) -> Response {
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
    fn finish(&mut self) {
        if let Some(end) = self.end.take() {
            end();
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
        if let Poll::Ready(None) = frame {
            this.finish();
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
    fn drop(&mut self) {
        self.finish();
    }
}

/// `00-<trace id>-<parent id>-<flags>`; anything malformed starts a new trace.
fn parse_traceparent(headers: &HeaderMap) -> Option<SpanContext> {
    let value = headers.get("traceparent")?.to_str().ok()?;
    let mut parts = value.trim().split('-');
    let (version, trace, span, flags) =
        (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if version.len() != 2
        || version == "ff"
        || trace.len() != 32
        || span.len() != 16
        || flags.len() != 2
    {
        return None;
    }
    let trace_id = TraceId::from_hex(trace).ok()?;
    let span_id = SpanId::from_hex(span).ok()?;
    let flags = u8::from_str_radix(flags, 16).ok()?;
    let context = SpanContext::new(
        trace_id,
        span_id,
        TraceFlags::new(flags),
        true,
        TraceState::default(),
    );
    context.is_valid().then_some(context)
}

/// A low-cardinality name for what a dev-server path serves.
fn route_class(path: &str) -> &'static str {
    if path.starts_with("/@oj/") || path.starts_with("/@vite/") || path.starts_with("/__") {
        return "internal";
    }
    if path.starts_with("/@oj-deps/") || path.contains("/node_modules/.vite/") {
        return "dep";
    }
    if path.starts_with("/@id/") || path.starts_with("/@fs/") {
        return "module";
    }
    let file = path.rsplit('/').next().unwrap_or("");
    let ext = file.rsplit_once('.').map(|(_, ext)| ext);
    // Other `/@` paths are dev-server modules (`/@react-refresh`,
    // `/@virtual/..`, `/@oj-empty`); without an extension they are no page.
    if path.starts_with("/@") && matches!(ext, None | Some("html")) {
        return "module";
    }
    match ext {
        None | Some("html") => "document",
        Some(
            "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "mts" | "cts" | "vue" | "svelte" | "json",
        ) => "module",
        Some("css" | "scss" | "sass" | "less" | "styl" | "pcss") => "style",
        Some(_) => "asset",
    }
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
    }

    fn headers(traceparent: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("traceparent", traceparent.parse().unwrap());
        headers
    }

    #[test]
    fn traceparent_parses_the_w3c_shape_only() {
        let context = parse_traceparent(&headers(
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
        let unsampled = parse_traceparent(&headers(
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
            assert!(parse_traceparent(&headers(bad)).is_none(), "{bad}");
        }
        assert!(parse_traceparent(&HeaderMap::new()).is_none());
    }

    #[tokio::test]
    async fn body_end_waits_for_the_last_frame() {
        use futures_util::StreamExt as _;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<Bytes, std::io::Error>>();
        let stream = futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        let ended = Arc::new(AtomicUsize::new(0));
        let seen = ended.clone();
        let response = on_body_end(Response::new(Body::from_stream(stream)), move || {
            seen.fetch_add(1, Ordering::SeqCst);
        });
        let mut body = response.into_body().into_data_stream();
        tx.send(Ok(Bytes::from_static(b"head"))).unwrap();
        assert_eq!(body.next().await.unwrap().unwrap(), "head");
        assert_eq!(
            ended.load(Ordering::SeqCst),
            0,
            "headers and a first chunk are not the end"
        );
        tx.send(Ok(Bytes::from_static(b"tail"))).unwrap();
        drop(tx);
        assert_eq!(body.next().await.unwrap().unwrap(), "tail");
        assert!(body.next().await.is_none());
        assert_eq!(ended.load(Ordering::SeqCst), 1, "the last frame ends it");
        drop(body);
        assert_eq!(ended.load(Ordering::SeqCst), 1, "ends once");

        let ended = Arc::new(AtomicUsize::new(0));
        let seen = ended.clone();
        let unread = on_body_end(Response::new(Body::from("x")), move || {
            seen.fetch_add(1, Ordering::SeqCst);
        });
        drop(unread);
        assert_eq!(ended.load(Ordering::SeqCst), 1, "a dropped body still ends");
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
}
