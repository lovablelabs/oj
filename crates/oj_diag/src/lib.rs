// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Structured dev-server diagnostics. Every noteworthy failure is recorded as
//! an [`Event`] in a bounded in-memory ring (the dev server lists it at
//! `/@oj/diagnostics`) and, when NDJSON output is on ([`set_json_output`]),
//! printed as one machine-parsable stderr line a supervisor can ingest.
//! Human stderr output is a separate concern: call sites keep their own
//! `eprintln!` lines and [`emit`] prints nothing outside NDJSON mode, so
//! recording an event never changes what a person reads in the terminal.

use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// Events kept; the oldest is dropped (and counted) past this.
const RING_CAP: usize = 256;
/// Bytes kept of `detail` (a stack or code frame), so one enormous frame
/// cannot grow the ring beyond its budget.
const DETAIL_CAP: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Warn,
    Error,
    Fatal,
}

/// Who observed the failure (not who caused it): `Client` events arrive over
/// the HMR websocket, `Host` events come from the plugin-host lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Server,
    Client,
    Plugin,
    Host,
}

/// A closed set, not free text: it is what dashboards group by and what a
/// supervisor alerts on, so spellings must never drift per call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A module failed to compile or transform while being served.
    CompileError,
    /// A bare import had no resolution.
    ResolveError,
    /// A plugin hook threw (`hotUpdate`, `watchChange`, ...).
    PluginHook,
    /// The plugin host was declared gone (unresponsive, over memory).
    HostGone,
    /// A plugin-host respawn attempt (warn) or its failure (error).
    HostRespawn,
    /// No respawns left: plugin-served routes are down for this process.
    HostExhausted,
    /// The dependency optimizer failed (initial run, re-run, or scan).
    OptimizeError,
    /// The dev server is about to exec itself (config/env change, plugin
    /// `server.restart()`).
    Restart,
    /// A Rust panic reached the process panic hook.
    Panic,
    /// A browser runtime error or unhandled rejection forwarded by the client.
    RuntimeError,
    /// A forwarded browser `console.error`/`console.warn` line.
    ConsoleError,
    /// A client reported an HMR update that failed to apply.
    HmrApplyFailed,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::CompileError => "compile_error",
            Kind::ResolveError => "resolve_error",
            Kind::PluginHook => "plugin_hook",
            Kind::HostGone => "host_gone",
            Kind::HostRespawn => "host_respawn",
            Kind::HostExhausted => "host_exhausted",
            Kind::OptimizeError => "optimize_error",
            Kind::Restart => "restart",
            Kind::Panic => "panic",
            Kind::RuntimeError => "runtime_error",
            Kind::ConsoleError => "console_error",
            Kind::HmrApplyFailed => "hmr_apply_failed",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    /// Epoch milliseconds when first recorded (a folded repeat refreshes it).
    pub ts: u64,
    pub level: Level,
    pub source: Source,
    pub kind: Kind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    /// A stack or code frame, bounded to [`DETAIL_CAP`] bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Consecutive identical events fold into one entry with a bumped count.
    pub count: u32,
}

impl Event {
    pub fn new(kind: Kind, message: impl Into<String>) -> Self {
        Event {
            ts: now_millis(),
            level: Level::Error,
            source: Source::Server,
            kind,
            message: message.into(),
            module: None,
            plugin: None,
            detail: None,
            count: 1,
        }
    }

    pub fn warn(mut self) -> Self {
        self.level = Level::Warn;
        self
    }

    pub fn fatal(mut self) -> Self {
        self.level = Level::Fatal;
        self
    }

    pub fn source(mut self, source: Source) -> Self {
        self.source = source;
        self
    }

    pub fn module(mut self, module: impl Into<Option<String>>) -> Self {
        self.module = module.into();
        self
    }

    pub fn plugin(mut self, plugin: impl Into<Option<String>>) -> Self {
        self.plugin = plugin.into();
        self
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        let mut detail = detail.into();
        if detail.len() > DETAIL_CAP {
            let mut end = DETAIL_CAP;
            while !detail.is_char_boundary(end) {
                end -= 1;
            }
            detail.truncate(end);
            detail.push('…');
        }
        self.detail = Some(detail);
        self
    }
}

#[derive(Default)]
struct Inner {
    events: VecDeque<Event>,
    counters: BTreeMap<&'static str, u64>,
    /// Events the ring evicted (history loss is visible, never silent).
    dropped: u64,
}

#[derive(Default)]
pub struct Diagnostics {
    json: AtomicBool,
    inner: Mutex<Inner>,
}

impl Diagnostics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_json_output(&self, on: bool) {
        self.json.store(on, Ordering::Relaxed);
    }

    pub fn json_output(&self) -> bool {
        self.json.load(Ordering::Relaxed)
    }

    /// Record an event. A repeat of the newest entry (same kind, module and
    /// message) folds into it: the count bumps, the timestamp refreshes, and
    /// `None` is returned so [`emit`] prints nothing for it. A fresh entry
    /// returns its NDJSON line when NDJSON output is on.
    pub fn record(&self, event: Event) -> Option<String> {
        let mut inner = self.inner.lock().unwrap();
        *inner.counters.entry(event.kind.as_str()).or_default() += 1;
        if let Some(last) = inner.events.back_mut() {
            if last.kind == event.kind
                && last.module == event.module
                && last.message == event.message
            {
                last.count += 1;
                last.ts = event.ts;
                return None;
            }
        }
        let line = self.json_output().then(|| ndjson_line(&event));
        inner.events.push_back(event);
        if inner.events.len() > RING_CAP {
            inner.events.pop_front();
            inner.dropped += 1;
        }
        line
    }

    /// The ring as JSON: `{events, counters, dropped}`, oldest event first.
    /// `after` keeps only events recorded after that epoch-millisecond stamp
    /// (a poller's cursor).
    pub fn snapshot(&self, after: Option<u64>) -> serde_json::Value {
        let inner = self.inner.lock().unwrap();
        let events: Vec<&Event> = inner
            .events
            .iter()
            .filter(|e| after.is_none_or(|a| e.ts > a))
            .collect();
        serde_json::json!({
            "events": events,
            "counters": inner.counters,
            "dropped": inner.dropped,
        })
    }
}

/// The process-wide ring every [`emit`] records into.
pub fn global() -> &'static Diagnostics {
    static GLOBAL: OnceLock<Diagnostics> = OnceLock::new();
    GLOBAL.get_or_init(Diagnostics::default)
}

/// Turn NDJSON stderr output on or off for the process ring.
pub fn set_json_output(on: bool) {
    global().set_json_output(on);
}

/// Record into the process ring; in NDJSON mode a fresh (non-folded) event
/// also prints one line to stderr.
pub fn emit(event: Event) {
    if let Some(line) = global().record(event) {
        eprintln!("{line}");
    }
}

/// One stderr line for an event: the event's fields plus the `"oj":"diag"`
/// marker a log parser matches on before trusting the rest of the object.
fn ndjson_line(event: &Event) -> String {
    let mut value = serde_json::to_value(event).expect("diag event serializes");
    value["oj"] = serde_json::Value::String("diag".to_string());
    value.to_string()
}

/// Record panics as fatal events before the previous hook (the default
/// printer) runs. Installs once; later calls are no-ops.
pub fn install_panic_hook() {
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "panic".to_string()
        };
        let mut event = Event::new(Kind::Panic, format!("panicked: {message}")).fatal();
        if let Some(location) = info.location() {
            event = event.detail(location.to_string());
        }
        emit(event);
        previous(info);
    }));
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: Kind, message: &str) -> Event {
        Event::new(kind, message)
    }

    #[test]
    fn ring_caps_and_counts_dropped() {
        let diag = Diagnostics::new();
        for i in 0..(RING_CAP + 44) {
            diag.record(event(Kind::CompileError, &format!("boom {i}")));
        }
        let snap = diag.snapshot(None);
        assert_eq!(snap["events"].as_array().unwrap().len(), RING_CAP);
        assert_eq!(snap["dropped"], 44);
        assert_eq!(snap["counters"]["compile_error"], (RING_CAP + 44) as u64);
        // The oldest survivors are the ones past the drop point.
        assert_eq!(snap["events"][0]["message"], "boom 44");
    }

    #[test]
    fn consecutive_repeats_fold_into_a_count() {
        let diag = Diagnostics::new();
        assert!(diag.record(event(Kind::CompileError, "boom")).is_none()); // json off
        for _ in 0..3 {
            diag.record(event(Kind::CompileError, "boom"));
        }
        diag.record(event(Kind::CompileError, "other"));
        diag.record(event(Kind::CompileError, "boom"));
        let snap = diag.snapshot(None);
        let events = snap["events"].as_array().unwrap();
        assert_eq!(
            events.len(),
            3,
            "folded repeats, then other, then boom again"
        );
        assert_eq!(events[0]["count"], 4);
        assert_eq!(
            snap["counters"]["compile_error"], 6,
            "counters see every repeat"
        );
    }

    #[test]
    fn a_different_module_never_folds() {
        let diag = Diagnostics::new();
        diag.record(event(Kind::CompileError, "boom").module("/a.tsx".to_string()));
        diag.record(event(Kind::CompileError, "boom").module("/b.tsx".to_string()));
        assert_eq!(diag.snapshot(None)["events"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn ndjson_line_carries_the_marker_and_fields() {
        let diag = Diagnostics::new();
        diag.set_json_output(true);
        let line = diag
            .record(
                event(Kind::RuntimeError, "x is not defined")
                    .source(Source::Client)
                    .warn()
                    .module("/src/App.tsx".to_string())
                    .detail("at App (/src/App.tsx:3:1)"),
            )
            .expect("fresh event prints in json mode");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["oj"], "diag");
        assert_eq!(v["kind"], "runtime_error");
        assert_eq!(v["level"], "warn");
        assert_eq!(v["source"], "client");
        assert_eq!(v["module"], "/src/App.tsx");
        assert_eq!(v["count"], 1);
        assert!(v["ts"].as_u64().unwrap() > 0);
        // A folded repeat prints nothing even in json mode.
        assert!(diag
            .record(
                event(Kind::RuntimeError, "x is not defined")
                    .source(Source::Client)
                    .module("/src/App.tsx".to_string())
            )
            .is_none());
    }

    #[test]
    fn snapshot_after_is_a_cursor() {
        let diag = Diagnostics::new();
        diag.record(event(Kind::ResolveError, "early"));
        let cut = now_millis() + 60_000;
        assert_eq!(
            diag.snapshot(Some(cut))["events"].as_array().unwrap().len(),
            0
        );
        assert_eq!(
            diag.snapshot(Some(0))["events"].as_array().unwrap().len(),
            1
        );
    }

    #[test]
    fn detail_is_bounded() {
        let big = "x".repeat(DETAIL_CAP * 2);
        let e = Event::new(Kind::Panic, "p").detail(big);
        assert!(e.detail.unwrap().len() <= DETAIL_CAP + '…'.len_utf8());
    }
}
