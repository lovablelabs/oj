// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `OJ_OTEL=1` exports over OTLP, configured by the standard `OTEL_*`
//! variables: a failing module arrives at the collector as a `compile_error`
//! log record and in the `oj.diag.events` counter, a request carrying a W3C
//! traceparent produces a server span that is that parent's child plus a
//! `http.server.request.duration` sample, only oj's instrumentation scope
//! exports, and the signal shutdown closes the pipeline cleanly.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

const TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const PARENT_ID: &str = "00f067aa0ba902b7";

/// Every OTLP/JSON body the collector received, by signal.
#[derive(Default)]
struct Received {
    logs: Vec<Value>,
    traces: Vec<Value>,
    metrics: Vec<Value>,
}

/// An OTLP/HTTP JSON collector on its own runtime thread.
fn collector() -> (String, Arc<Mutex<Received>>) {
    let received = Arc::new(Mutex::new(Received::default()));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let state = received.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let app = axum::Router::new().route(
                "/v1/{signal}",
                axum::routing::post(
                    move |axum::extract::Path(signal): axum::extract::Path<String>,
                          body: String| {
                        let state = state.clone();
                        async move {
                            let body: Value = serde_json::from_str(&body).unwrap();
                            let mut received = state.lock().unwrap();
                            match signal.as_str() {
                                "logs" => received.logs.push(body),
                                "traces" => received.traces.push(body),
                                "metrics" => received.metrics.push(body),
                                _ => {}
                            }
                            ([("content-type", "application/json")], "{}")
                        }
                    },
                ),
            );
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(listener, app).await.unwrap();
        });
    });
    (endpoint, received)
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The status of `GET path` over HTTP/1.1, with extra header lines.
fn get(port: u16, path: &str, headers: &str) -> u16 {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n{headers}\r\n"
    )
    .unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

fn settles(what: &str, received: &Mutex<Received>, done: impl Fn(&Received) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done(&received.lock().unwrap()) {
        assert!(Instant::now() < deadline, "never arrived: {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Items under `resource{kind}[].scope{kind}[].{items}` of every body.
fn items<'a>(bodies: &'a [Value], kind: &str, items: &str) -> Vec<&'a Value> {
    bodies
        .iter()
        .flat_map(|b| {
            b[format!("resource{kind}")]
                .as_array()
                .into_iter()
                .flatten()
        })
        .flat_map(|r| r[format!("scope{kind}")].as_array().into_iter().flatten())
        .flat_map(|s| s[items].as_array().into_iter().flatten())
        .collect()
}

fn scopes(bodies: &[Value], kind: &str) -> Vec<String> {
    bodies
        .iter()
        .flat_map(|b| {
            b[format!("resource{kind}")]
                .as_array()
                .into_iter()
                .flatten()
        })
        .flat_map(|r| r[format!("scope{kind}")].as_array().into_iter().flatten())
        .map(|s| s["scope"]["name"].as_str().unwrap_or("").to_string())
        .collect()
}

/// An attribute's string or int value, as text.
fn attr(item: &Value, key: &str) -> Option<String> {
    let value = &item["attributes"]
        .as_array()?
        .iter()
        .find(|a| a["key"] == key)?["value"];
    value["stringValue"]
        .as_str()
        .map(str::to_string)
        .or_else(|| value["intValue"].as_str().map(str::to_string))
        .or_else(|| value["intValue"].as_i64().map(|n| n.to_string()))
}

fn number(value: &Value) -> u64 {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}

struct Server {
    child: Child,
    port: u16,
    stderr: Arc<Mutex<String>>,
    root: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Server {
    /// `oj dev` on `app(name)` exporting to `endpoint`, its batch processors
    /// and metric reader exporting every `export_ms`, once it serves.
    fn start(name: &str, endpoint: &str, export_ms: &str) -> Server {
        Server::start_with(name, endpoint, export_ms, &[])
    }

    /// [`Server::start`] with `env` applied last: `Some` sets a variable,
    /// `None` removes it.
    fn start_with(
        name: &str,
        endpoint: &str,
        export_ms: &str,
        env: &[(&str, Option<&str>)],
    ) -> Server {
        let root = app(name);
        let port = free_port();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_oj"));
        cmd.args(["dev"])
            .arg(&root)
            .args(["--port", &port.to_string()])
            .env("OJ_OTEL", "1")
            .env("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint)
            .env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
            .env("OTEL_SERVICE_NAME", "oj-test")
            .env("OTEL_METRIC_EXPORT_INTERVAL", export_ms)
            .env("OTEL_BLRP_SCHEDULE_DELAY", export_ms)
            .env("OTEL_BSP_SCHEDULE_DELAY", export_ms)
            .env_remove("OTEL_TRACES_SAMPLER")
            .env_remove("OTEL_TRACES_SAMPLER_ARG");
        for (key, value) in env {
            match value {
                Some(value) => cmd.env(key, value),
                None => cmd.env_remove(key),
            };
        }
        let mut child = cmd
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = Arc::new(Mutex::new(String::new()));
        let mut pipe = child.stderr.take().unwrap();
        let sink = stderr.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });
        let mut server = Server {
            child,
            port,
            stderr,
            root,
        };
        let deadline = Instant::now() + Duration::from_secs(60);
        while TcpStream::connect(("127.0.0.1", port)).is_err() || get(port, "/ok.tsx", "") != 200 {
            if let Ok(Some(status)) = server.child.try_wait() {
                panic!(
                    "oj exited before serving: {status}\n{}",
                    server.stderr.lock().unwrap()
                );
            }
            assert!(Instant::now() < deadline, "oj never served /ok.tsx");
            std::thread::sleep(Duration::from_millis(50));
        }
        server
    }

    /// SIGTERM, the way a supervisor stops oj; a clean exit with the
    /// signal's code and no panic.
    fn stop(mut self) {
        // SAFETY: a signal to the child this test spawned and still owns.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "oj did not exit on SIGTERM");
            std::thread::sleep(Duration::from_millis(50));
        };
        std::thread::sleep(Duration::from_millis(100));
        let stderr = self.stderr.lock().unwrap().clone();
        assert!(!stderr.contains("panicked"), "shutdown panicked:\n{stderr}");
        assert_eq!(
            status.code(),
            Some(143),
            "oj exits with the signal's code:\n{stderr}"
        );
    }
}

fn app(name: &str) -> PathBuf {
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("oj-otel-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    for (name, text) in [
        ("package.json", r#"{"name":"otel","version":"1.0.0"}"#),
        (
            "index.html",
            r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/main.tsx"></script></body></html>"#,
        ),
        ("ok.tsx", "export const fine = 1;\n"),
        ("main.tsx", "export const broken = {;\n"),
    ] {
        std::fs::write(root.join(name), text).unwrap();
    }
    root
}

#[test]
fn otel_exports_diagnostics_spans_and_metrics() {
    let (endpoint, received) = collector();
    let server = Server::start("export", &endpoint, "200");
    let port = server.port;

    let traceparent =
        format!("traceparent: 00-{TRACE_ID}-{PARENT_ID}-01\r\ntracestate: vendor=abc\r\n");
    assert_eq!(get(port, "/ok.tsx", &traceparent), 200);
    for _ in 0..3 {
        assert_eq!(
            get(port, "/main.tsx", ""),
            500,
            "the broken module is a 500"
        );
    }

    settles("a compile_error log record", &received, |r| {
        items(&r.logs, "Logs", "logRecords").iter().any(|record| {
            attr(record, "oj.diag.kind").as_deref() == Some("compile_error")
                && record["body"]["stringValue"]
                    .as_str()
                    .is_some_and(|b| b.contains("main.tsx"))
        })
    });
    settles("a span that joins the incoming trace", &received, |r| {
        items(&r.traces, "Spans", "spans")
            .iter()
            .any(|s| s["traceId"] == TRACE_ID && s["parentSpanId"] == PARENT_ID)
    });
    settles("every repeat in oj.diag.events", &received, |r| {
        items(&r.metrics, "Metrics", "metrics")
            .iter()
            .filter(|m| m["name"] == "oj.diag.events")
            .flat_map(|m| m["sum"]["dataPoints"].as_array().into_iter().flatten())
            .any(|p| {
                attr(p, "oj.diag.kind").as_deref() == Some("compile_error")
                    && number(&p["asInt"]) >= 3
            })
    });
    settles("a request duration sample", &received, |r| {
        items(&r.metrics, "Metrics", "metrics")
            .iter()
            .filter(|m| m["name"] == "http.server.request.duration")
            .flat_map(|m| {
                m["histogram"]["dataPoints"]
                    .as_array()
                    .into_iter()
                    .flatten()
            })
            .any(|p| number(&p["count"]) > 0)
    });

    {
        let r = received.lock().unwrap();
        let records = items(&r.logs, "Logs", "logRecords");
        let record = records
            .iter()
            .find(|rec| attr(rec, "oj.diag.kind").as_deref() == Some("compile_error"))
            .unwrap();
        assert_eq!(record["severityText"], "error");
        assert_eq!(attr(record, "oj.diag.source").as_deref(), Some("server"));
        let service = r.logs[0]["resourceLogs"][0]["resource"]["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["key"] == "service.name")
            .map(|a| a["value"]["stringValue"].clone());
        assert_eq!(
            service,
            Some(Value::from("oj-test")),
            "OTEL_SERVICE_NAME names the resource"
        );

        let spans = items(&r.traces, "Spans", "spans");
        let span = spans.iter().find(|s| s["traceId"] == TRACE_ID).unwrap();
        assert_eq!(span["name"], "GET");
        assert_eq!(
            span["traceState"], "vendor=abc",
            "the parent's tracestate rides along"
        );
        assert_eq!(attr(span, "url.path").as_deref(), Some("/ok.tsx"));
        assert_eq!(attr(span, "url.scheme").as_deref(), Some("http"));
        assert_eq!(
            attr(span, "network.protocol.version").as_deref(),
            Some("1.1")
        );
        assert_eq!(attr(span, "oj.route.class").as_deref(), Some("module"));
        assert_eq!(
            attr(span, "http.response.status_code").as_deref(),
            Some("200")
        );
        assert!(
            number(&span["endTimeUnixNano"]) >= number(&span["startTimeUnixNano"]),
            "the span ends after it starts"
        );
        let failed = spans
            .iter()
            .find(|s| attr(s, "url.path").as_deref() == Some("/main.tsx"))
            .expect("requests without a traceparent start their own trace");
        assert_eq!(
            failed["status"]["code"], 2,
            "a 500 marks the span as an error"
        );
        assert_eq!(attr(failed, "error.type").as_deref(), Some("500"));

        // The embedded JS runtimes keep Deno's own instrumentation off.
        let mut all = scopes(&r.logs, "Logs");
        all.extend(scopes(&r.traces, "Spans"));
        all.extend(scopes(&r.metrics, "Metrics"));
        all.sort();
        all.dedup();
        assert_eq!(all, ["oj"], "only oj's instrumentation scope exports");
    }

    server.stop();
}

// With exports minutes apart, only the shutdown can deliver what a session
// recorded: the signal exit runs deno_telemetry's exit hook, which exports
// what is queued before closing the pipeline.
#[test]
fn the_signal_shutdown_exports_what_is_queued() {
    let (endpoint, received) = collector();
    let server = Server::start("shutdown", &endpoint, "60000");
    let traceparent = format!("traceparent: 00-{TRACE_ID}-{PARENT_ID}-01\r\n");
    assert_eq!(get(server.port, "/ok.tsx", &traceparent), 200);
    assert_eq!(get(server.port, "/main.tsx", ""), 500);
    std::thread::sleep(Duration::from_millis(300));
    {
        let r = received.lock().unwrap();
        assert!(
            r.traces.is_empty() && r.logs.is_empty(),
            "nothing exports before the shutdown"
        );
    }
    server.stop();
    let r = received.lock().unwrap();
    assert!(
        items(&r.traces, "Spans", "spans")
            .iter()
            .any(|s| s["traceId"] == TRACE_ID),
        "the queued span reached the collector"
    );
    assert!(
        items(&r.logs, "Logs", "logRecords")
            .iter()
            .any(|rec| attr(rec, "oj.diag.kind").as_deref() == Some("compile_error")),
        "the queued diagnostic reached the collector"
    );
    assert!(
        items(&r.metrics, "Metrics", "metrics")
            .iter()
            .any(|m| m["name"] == "http.server.request.duration"),
        "the shutdown collected and exported the metrics"
    );
}

/// What reaches the collector from a session that serves a traced request
/// and a broken module, exporting every 200ms, after it is stopped.
fn session(name: &str, env: &[(&str, Option<&str>)]) -> (Arc<Mutex<Received>>, String) {
    let (endpoint, received) = collector();
    let server = Server::start_with(name, &endpoint, "200", env);
    let traceparent = format!("traceparent: 00-{TRACE_ID}-{PARENT_ID}-01\r\n");
    assert_eq!(get(server.port, "/ok.tsx", &traceparent), 200);
    assert_eq!(get(server.port, "/main.tsx", ""), 500);
    // Several export intervals: anything enabled has had time to arrive.
    std::thread::sleep(Duration::from_millis(1500));
    let stderr = server.stderr.lock().unwrap().clone();
    server.stop();
    (received, stderr)
}

fn nothing_arrived(received: &Mutex<Received>) -> bool {
    let r = received.lock().unwrap();
    r.logs.is_empty() && r.traces.is_empty() && r.metrics.is_empty()
}

// Off by default: with the OTLP endpoint configured but no OJ_OTEL, nothing
// is exported and oj runs as it always did.
#[test]
fn nothing_exports_without_oj_otel() {
    let (received, _) = session("off", &[("OJ_OTEL", None)]);
    assert!(
        nothing_arrived(&received),
        "OJ_OTEL unset exported something"
    );
}

// A config Deno refuses (an unsupported sampler) turns export off with a
// warning; the dev server itself serves on.
#[test]
fn a_bad_otel_config_warns_and_oj_serves_on() {
    let (received, stderr) = session("bad", &[("OTEL_TRACES_SAMPLER", Some("sometimes"))]);
    assert!(
        stderr.contains("OpenTelemetry export disabled"),
        "the refusal is reported:\n{stderr}"
    );
    assert!(nothing_arrived(&received));
}

// always_off drops every span, even one whose parent was sampled; the
// duration histogram and the diagnostics are not sampled and still arrive.
#[test]
fn an_always_off_sampler_drops_spans_only() {
    let (received, _) = session("sampler", &[("OTEL_TRACES_SAMPLER", Some("always_off"))]);
    let r = received.lock().unwrap();
    assert!(
        items(&r.traces, "Spans", "spans").is_empty(),
        "a span was exported"
    );
    assert!(items(&r.metrics, "Metrics", "metrics")
        .iter()
        .any(|m| m["name"] == "http.server.request.duration"));
    assert!(items(&r.logs, "Logs", "logRecords")
        .iter()
        .any(|rec| attr(rec, "oj.diag.kind").as_deref() == Some("compile_error")));
}
