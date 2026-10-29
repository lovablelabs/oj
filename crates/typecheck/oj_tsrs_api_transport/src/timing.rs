// Port of tsc/internal/ipc/timing.go: connection-level getServerTiming / resetServerTiming.

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

pub(crate) const METHOD_GET_SERVER_TIMING: &str = "getServerTiming";
pub(crate) const METHOD_RESET_SERVER_TIMING: &str = "resetServerTiming";

const SERVER_RECENT_REQUEST_CAPACITY: usize = 5;

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ServerRequestTiming {
    method: String,
    processing_time_ms: f64,
    timestamp: i64,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ServerTimingTotals {
    request_count: u64,
    total_processing_time_ms: f64,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ServerTimingInfo {
    enabled: bool,
    totals: ServerTimingTotals,
    recent_requests: Vec<ServerRequestTiming>,
}

#[derive(Default)]
struct State {
    totals: ServerTimingTotals,
    ring: Vec<ServerRequestTiming>,
    head: usize,
}

#[derive(Default)]
pub struct TimingCollector {
    state: Mutex<State>,
}

impl TimingCollector {
    pub(crate) fn record(&self, method: &str, d: Duration) {
        let processing_ms = d.as_nanos() as f64 / 1_000_000.0;
        let timestamp =
            SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
        let mut s = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        s.totals.request_count += 1;
        s.totals.total_processing_time_ms += processing_ms;
        let entry = ServerRequestTiming {
            method: method.to_string(),
            processing_time_ms: processing_ms,
            timestamp,
        };
        if s.ring.len() < SERVER_RECENT_REQUEST_CAPACITY {
            s.ring.push(entry);
        } else {
            let head = s.head;
            s.ring[head] = entry;
            s.head = (head + 1) % SERVER_RECENT_REQUEST_CAPACITY;
        }
    }

    fn snapshot(&self) -> ServerTimingInfo {
        let s = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let n = s.ring.len();
        let recent = (0..n).map(|i| s.ring[(s.head + i) % n].clone()).collect();
        ServerTimingInfo { enabled: true, totals: s.totals.clone(), recent_requests: recent }
    }

    pub(crate) fn reset(&self) {
        let mut s = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *s = State::default();
    }
}

pub(crate) fn server_timing_snapshot(c: Option<&TimingCollector>) -> ServerTimingInfo {
    match c {
        Some(c) => c.snapshot(),
        None => ServerTimingInfo {
            enabled: false,
            totals: ServerTimingTotals::default(),
            recent_requests: Vec::new(),
        },
    }
}
