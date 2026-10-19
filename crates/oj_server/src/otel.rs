//! OpenTelemetry export (`OJ_OTEL=1`): diagnostics as log records plus an
//! event counter, and a span and duration sample per dev-server request.
//! Rides the embedded Deno's OTLP pipeline, so the standard
//! `OTEL_EXPORTER_OTLP_*`, `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES`
//! variables configure it.

use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

use axum::extract::Request;
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;
use deno_telemetry::{OtelConfig, OtelRuntimeConfig, OTEL_GLOBALS};
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

/// Longest a restart or shutdown waits for pending exports.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

struct Instruments {
    scope: InstrumentationScope,
    diag_events: Counter<u64>,
    request_duration: Histogram<f64>,
}

static INSTRUMENTS: OnceLock<Instruments> = OnceLock::new();

/// Starts the exporter when `OJ_OTEL` is set; a bad configuration warns and
/// leaves oj running without it.
pub fn init(version: &'static str) {
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
        runtime_version: version.into(),
    };
    if let Err(e) = deno_telemetry::init(&sys_traits::impls::RealSys, runtime, config) {
        eprintln!("oj: OpenTelemetry export disabled: {e}");
        return;
    }
    let Some(globals) = OTEL_GLOBALS.get() else {
        return;
    };
    let scope = InstrumentationScope::builder("oj")
        .with_version(version)
        .build();
    let meter = globals.meter_provider.meter_with_scope(scope.clone());
    let instruments = Instruments {
        scope,
        diag_events: meter
            .u64_counter("oj.diag.events")
            .with_description("Dev-server diagnostic events, folded repeats included")
            .build(),
        request_duration: meter
            .f64_histogram("http.server.request.duration")
            .with_unit("s")
            .with_boundaries(vec![
                0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
            ])
            .build(),
    };
    if INSTRUMENTS.set(instruments).is_ok() {
        oj_diag::set_sink(export_diag);
    }
}

pub fn enabled() -> bool {
    INSTRUMENTS.get().is_some()
}

/// Exports what is queued, bounded so an unreachable collector cannot hold up
/// a restart or shutdown.
pub fn flush() {
    let Some(globals) = OTEL_GLOBALS.get() else {
        return;
    };
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = globals.span_processor.force_flush();
        let _ = globals.log_processor.force_flush();
        let _ = globals.meter_provider.force_flush();
        let _ = done_tx.send(());
    });
    let _ = done_rx.recv_timeout(FLUSH_TIMEOUT);
}

fn export_diag(event: &oj_diag::Event) {
    let (Some(globals), Some(instruments)) = (OTEL_GLOBALS.get(), INSTRUMENTS.get()) else {
        return;
    };
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
    instruments.diag_events.add(
        1,
        &[
            KeyValue::new("oj.diag.kind", event.kind.as_str()),
            KeyValue::new("oj.diag.level", level),
            KeyValue::new("oj.diag.source", source),
        ],
    );
    let mut record = new_log_record();
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
    globals.log_processor.emit(&mut record, &instruments.scope);
}

/// opentelemetry 0.32 mints records only through a logger; this one has no
/// processors, so records go out through Deno's batch processor instead.
fn new_log_record() -> SdkLogRecord {
    static FACTORY: OnceLock<SdkLogger> = OnceLock::new();
    FACTORY
        .get_or_init(|| SdkLoggerProvider::builder().build().logger("oj"))
        .create_log_record()
}

/// Router layer: one server span (a child of an incoming W3C `traceparent`)
/// and one duration sample per request.
pub async fn trace_request(req: Request, next: Next) -> Response {
    let (Some(globals), Some(instruments)) = (OTEL_GLOBALS.get(), INSTRUMENTS.get()) else {
        return next.run(req).await;
    };
    let start_time = SystemTime::now();
    let started = Instant::now();
    let method = req.method().as_str().to_owned();
    let path = req.uri().path().to_owned();
    let parent = parse_traceparent(req.headers());
    let response = next.run(req).await;
    let status = response.status();
    let class = route_class(&path);
    let attributes = vec![
        KeyValue::new("http.request.method", method.clone()),
        KeyValue::new("http.response.status_code", i64::from(status.as_u16())),
        KeyValue::new("oj.route.class", class),
    ];
    instruments
        .request_duration
        .record(started.elapsed().as_secs_f64(), &attributes);
    // parentbased_always_on, the OTel default: a root request is sampled and
    // a child follows its parent's decision.
    if parent.as_ref().is_some_and(|p| !p.is_sampled()) {
        return response;
    }
    let trace_id = parent.as_ref().map_or_else(
        || globals.id_generator.new_trace_id(),
        SpanContext::trace_id,
    );
    let mut span_attributes = attributes;
    span_attributes.push(KeyValue::new("url.path", path));
    globals.span_processor.on_end(SpanData {
        span_context: SpanContext::new(
            trace_id,
            globals.id_generator.new_span_id(),
            TraceFlags::SAMPLED,
            false,
            TraceState::default(),
        ),
        parent_span_id: parent
            .as_ref()
            .map_or(SpanId::INVALID, SpanContext::span_id),
        parent_span_is_remote: parent.is_some(),
        span_kind: SpanKind::Server,
        name: format!("{method} {class}").into(),
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
        instrumentation_scope: instruments.scope.clone(),
    });
    response
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
    match file.rsplit_once('.').map(|(_, ext)| ext) {
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
    fn routes_classify_by_prefix_then_extension() {
        assert_eq!(route_class("/@oj/client.js"), "internal");
        assert_eq!(route_class("/__ws"), "internal");
        assert_eq!(route_class("/@oj-deps/react.js"), "dep");
        assert_eq!(route_class("/@fs/app/src/x.ts"), "module");
        assert_eq!(route_class("/src/App.tsx"), "module");
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
}
