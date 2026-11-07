//! tsrs-only: sub-phase timings and counts that `--extendedDiagnostics` prints after Go's table.
//!
//! Callers enable collection only when diagnostics are requested. `record` adds to an existing row of the same
//! name, so a phase that runs in several pieces accumulates. Rows print in first-recorded order.

use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub enum PhaseValue {
    Time(Duration),
    Count(u64),
}

static PHASES: Mutex<Vec<(&'static str, PhaseValue)>> = Mutex::new(Vec::new());

pub fn record(name: &'static str, d: Duration) {
    let mut phases = PHASES.lock().unwrap();
    match phases.iter_mut().find(|(n, _)| *n == name) {
        Some((_, PhaseValue::Time(t))) => *t += d,
        Some(_) => {}
        None => phases.push((name, PhaseValue::Time(d))),
    }
}

pub fn count(enabled: bool, name: &'static str, n: u64) {
    if !enabled {
        return;
    }
    let mut phases = PHASES.lock().unwrap();
    match phases.iter_mut().find(|(n2, _)| *n2 == name) {
        Some((_, PhaseValue::Count(c))) => *c += n,
        Some(_) => {}
        None => phases.push((name, PhaseValue::Count(n))),
    }
}

/// A diagnostic timer that reads no clock and records no row when disabled.
pub struct Timer(Option<Instant>);

impl Timer {
    pub fn new(enabled: bool) -> Self {
        Self(enabled.then(Instant::now))
    }

    pub fn elapsed(&self) -> Duration {
        self.0.map_or(Duration::ZERO, |start| start.elapsed())
    }

    pub fn record(self, name: &'static str) {
        if let Some(start) = self.0 {
            record(name, start.elapsed());
        }
    }
}

pub fn time<T>(enabled: bool, name: &'static str, f: impl FnOnce() -> T) -> T {
    let start = Timer::new(enabled);
    let result = f();
    start.record(name);
    result
}

pub fn snapshot() -> Vec<(&'static str, PhaseValue)> {
    PHASES.lock().unwrap().clone()
}
