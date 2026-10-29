// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `OJ_TYPECHECK`: a TypeScript checker kept warm beside the dev server.
//!
//! One thread owns an [`oj_typecheck::Checker`] for the app root. The
//! watcher forwards every change to it, and it re-checks once changes settle,
//! so `/@oj/typecheck` (and `oj typecheck`, which asks the running server)
//! usually answers from a report that is already current. A request names
//! the files its caller changed; they are re-read before the answer, so it
//! never depends on watcher timing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::{ServerState, WatchFeed};

/// Where a dev server with `OJ_TYPECHECK` names its address for
/// `oj typecheck`, under the app's cache root.
const ADDRESS_FILE: &str = "dev-server.json";

pub(crate) struct Typecheck {
    tx: std::sync::mpsc::Sender<Msg>,
}

enum Msg {
    /// Open the projects now (after the server is listening), so the first
    /// check finds them loaded.
    Warm,
    Changed(Vec<PathBuf>),
    Check {
        changed: Vec<PathBuf>,
        reply: tokio::sync::oneshot::Sender<serde_json::Value>,
    },
}

impl Typecheck {
    /// The checker thread for `root`, when `OJ_TYPECHECK` is on.
    pub(crate) fn start(root: &Path) -> Option<Arc<Typecheck>> {
        if !oj_env::get().knobs.typecheck {
            return None;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let root = root.to_path_buf();
        std::thread::Builder::new()
            .name("oj-typecheck".into())
            .spawn(move || imp::run(root, rx))
            .ok()?;
        Some(Arc::new(Typecheck { tx }))
    }

    pub(crate) fn warm(&self) {
        let _ = self.tx.send(Msg::Warm);
    }

    /// Forwards the watcher's events from now on.
    pub(crate) fn follow(&self, feed: &WatchFeed) {
        let events = feed.subscribe();
        let tx = self.tx.clone();
        let _ = std::thread::Builder::new()
            .name("oj-typecheck-feed".into())
            .spawn(move || {
                while let Ok(event) = events.recv() {
                    if tx.send(Msg::Changed(event.paths)).is_err() {
                        break;
                    }
                }
            });
    }

    /// The report for the current files, `changed` re-read first.
    pub(crate) async fn check(&self, changed: Vec<PathBuf>) -> serde_json::Value {
        let (reply, answer) = tokio::sync::oneshot::channel();
        if self.tx.send(Msg::Check { changed, reply }).is_err() {
            return unavailable("the type checker stopped");
        }
        answer
            .await
            .unwrap_or_else(|_| unavailable("the type checker stopped"))
    }
}

fn unavailable(reason: &str) -> serde_json::Value {
    serde_json::json!({ "status": "unavailable", "reason": reason })
}

/// `GET|POST /@oj/typecheck`, body `{"changed": [paths]}` (optional). Like
/// `/@oj/diagnostics`, it answers only direct local callers: requests a
/// browser or the preview proxy sends carry `Origin` or `X-Forwarded-Host`.
pub(crate) async fn serve_typecheck(
    headers: HeaderMap,
    State(state): State<Arc<ServerState>>,
    body: Bytes,
) -> Response {
    if headers.contains_key(header::ORIGIN) || headers.contains_key("x-forwarded-host") {
        return (StatusCode::FORBIDDEN, "").into_response();
    }
    let Some(typecheck) = &state.typecheck else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let changed = match parse_changed(&body) {
        Ok(changed) => changed,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    let answer = typecheck.check(changed).await;
    (
        [(header::CONTENT_TYPE, "application/json")],
        answer.to_string(),
    )
        .into_response()
}

fn parse_changed(body: &[u8]) -> Result<Vec<PathBuf>, String> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Vec::new());
    }
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("invalid JSON body: {e}"))?;
    Ok(value["changed"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_str().map(PathBuf::from))
        .collect())
}

/// Names the bound address for `oj typecheck` (pid, host, port) in the cache
/// root. A stale file is harmless: the client checks the pid and falls back
/// to checking in its own process.
pub(crate) fn advertise(root: &Path, host: std::net::IpAddr, port: u16, https: bool) {
    let host = if host.is_unspecified() {
        std::net::IpAddr::from([127, 0, 0, 1])
    } else {
        host
    };
    let dir = oj_cache::cache_root(root);
    let body = serde_json::json!({
        "pid": std::process::id(),
        "host": host.to_string(),
        "port": port,
        "https": https,
    });
    let _ = std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(dir.join(ADDRESS_FILE), body.to_string()));
}

/// Asks the dev server running for `root` to check, re-reading `changed`
/// first. `None` when no server with `OJ_TYPECHECK` answers there.
pub async fn check_via_server(root: &Path, changed: &[PathBuf]) -> Option<serde_json::Value> {
    let root = root.canonicalize().ok()?;
    let text = std::fs::read_to_string(oj_cache::cache_root(&root).join(ADDRESS_FILE)).ok()?;
    let address: serde_json::Value = serde_json::from_str(&text).ok()?;
    if address["https"].as_bool() == Some(true) || !pid_alive(address["pid"].as_u64()?) {
        return None;
    }
    let url = format!(
        "http://{}:{}/@oj/typecheck",
        address["host"].as_str()?,
        address["port"].as_u64()?
    );
    let changed: Vec<String> = changed
        .iter()
        .map(|p| root.join(p).to_string_lossy().into_owned())
        .collect();
    let response = reqwest::Client::new()
        .post(url)
        .header(header::CONTENT_TYPE, "application/json")
        .body(serde_json::json!({ "changed": changed }).to_string())
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    serde_json::from_slice(&response.bytes().await.ok()?).ok()
}

#[cfg(unix)]
fn pid_alive(pid: u64) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(not(unix))]
fn pid_alive(_pid: u64) -> bool {
    true
}

/// Checks `root` once in this process, for `oj typecheck` when no dev server
/// answers.
pub fn check_once(root: &Path) -> serde_json::Value {
    imp::check_once(root)
}

mod imp {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{Receiver, RecvTimeoutError};
    use std::time::Duration;

    use oj_typecheck::{Checker, Report};

    use super::{unavailable, Msg};

    /// How long changes settle before a background re-check.
    const SETTLE: Duration = Duration::from_millis(150);

    pub(super) fn run(root: PathBuf, rx: Receiver<Msg>) {
        lower_priority();
        let mut state = State {
            root,
            checker: None,
            pending: Vec::new(),
            report: None,
        };
        loop {
            let msg = if state.pending.is_empty() {
                rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
            } else {
                rx.recv_timeout(SETTLE)
            };
            match msg {
                Ok(Msg::Warm) => {
                    let _ = state.answer();
                }
                Ok(Msg::Changed(paths)) => {
                    // Before the first open there is nothing to update: the
                    // open reads every file fresh.
                    if state.checker.is_some() {
                        state
                            .pending
                            .extend(paths.into_iter().filter(|p| oj_typecheck::is_relevant(p)));
                    }
                }
                Ok(Msg::Check { changed, reply }) => {
                    state.pending.extend(changed);
                    let _ = reply.send(state.answer());
                }
                Err(RecvTimeoutError::Timeout) => {
                    let _ = state.answer();
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    struct State {
        root: PathBuf,
        checker: Option<Checker>,
        pending: Vec<PathBuf>,
        report: Option<Report>,
    }

    impl State {
        /// The report for the current files: opens the projects when needed,
        /// applies pending changes, and re-checks when the snapshot moved.
        fn answer(&mut self) -> serde_json::Value {
            let outcome = catch_unwind(AssertUnwindSafe(|| self.refresh()));
            match outcome {
                Ok(Ok(Some(answer))) => answer,
                Ok(Ok(None)) => unavailable("no tsconfig.json in the app root"),
                Ok(Err(e)) => {
                    self.reset();
                    unavailable(&e)
                }
                Err(panic) => {
                    self.reset();
                    unavailable(&format!(
                        "the type checker panicked: {}",
                        panic_text(&panic)
                    ))
                }
            }
        }

        fn refresh(&mut self) -> Result<Option<serde_json::Value>, String> {
            if self.checker.is_none() {
                self.pending.clear();
                self.report = None;
                match Checker::open(&self.root)? {
                    Some(checker) => self.checker = Some(checker),
                    None => return Ok(None),
                }
            }
            let checker = self.checker.as_mut().expect("opened above");
            let pending = std::mem::take(&mut self.pending);
            checker.changed(pending.iter().map(PathBuf::as_path))?;
            if self
                .report
                .as_ref()
                .is_none_or(|r| r.generation != checker.generation())
            {
                self.report = Some(checker.check()?);
            }
            Ok(self.report.as_ref().map(answer_of))
        }

        /// Drops a checker that failed; the next request opens a fresh one.
        fn reset(&mut self) {
            self.report = None;
            self.pending.clear();
            if let Some(checker) = self.checker.take() {
                if catch_unwind(AssertUnwindSafe(|| drop(checker))).is_err() {
                    eprintln!("oj: typecheck: dropping a failed checker panicked");
                }
            }
        }
    }

    fn answer_of(report: &Report) -> serde_json::Value {
        let errors = report.error_count();
        serde_json::json!({
            "status": if errors == 0 { "ok" } else { "errors" },
            "generation": report.generation,
            "durationMs": report.duration.as_millis() as u64,
            "errorCount": errors,
            "diagnostics": report.diagnostics,
            "text": report.text(),
        })
    }

    pub(super) fn check_once(root: &Path) -> serde_json::Value {
        let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<_, String> {
            let Some(mut checker) = Checker::open(root)? else {
                return Ok(None);
            };
            checker.check().map(Some)
        }));
        match outcome {
            Ok(Ok(Some(report))) => answer_of(&report),
            Ok(Ok(None)) => unavailable("no tsconfig.json in the app root"),
            Ok(Err(e)) => unavailable(&e),
            Err(panic) => unavailable(&format!(
                "the type checker panicked: {}",
                panic_text(&panic)
            )),
        }
    }

    fn panic_text(panic: &Box<dyn std::any::Any + Send>) -> String {
        panic
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_string())
    }

    /// Checks yield to HMR and page loads: on Linux, `setpriority` on the
    /// calling thread, which the checker's own threads inherit.
    fn lower_priority() {
        #[cfg(target_os = "linux")]
        // SAFETY: adjusts this thread's nice value; no memory is touched.
        unsafe {
            libc::setpriority(libc::PRIO_PROCESS, 0, 10);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_changed;

    #[test]
    fn a_request_body_names_the_changed_files() {
        assert!(parse_changed(b"").unwrap().is_empty());
        let changed = parse_changed(br#"{"changed":["src/App.tsx","/abs/b.ts"]}"#).unwrap();
        assert_eq!(changed.len(), 2);
        assert!(parse_changed(b"{nope").is_err());
    }
}
