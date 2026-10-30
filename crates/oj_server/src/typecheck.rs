// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `oj dev --typecheck`: a TypeScript checker kept warm beside the dev server.
//!
//! The checker runs in a child `oj typecheck-worker` process, so nothing
//! tsrs does (an abort, a stack overflow, exhausting its arena's address
//! range) can take the dev server down; a worker that dies or hangs is
//! replaced on the next request. It owns an [`oj_typecheck::Checker`] for the
//! app root, the watcher forwards every change to it, and it re-checks once
//! changes settle, so `/@oj/typecheck` (and `oj typecheck`, which asks the
//! running server) usually answers from a report that is already current. A
//! request names the files its caller changed; they are re-read before the
//! answer, so it never depends on watcher timing.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::{ServerState, WatchFeed};

/// Where a dev server with `--typecheck` names its address for
/// `oj typecheck`, under the app's cache root.
const ADDRESS_FILE: &str = "dev-server.json";

/// The checker's own thread. tsrs runs its API requests on 256 MiB stacks
/// (checking recurses deeply), and some of the work runs on the calling
/// thread.
const CHECKER_STACK: usize = 256 << 20;

/// How long one check may take before its worker is replaced.
const CHECK_TIMEOUT: Duration = Duration::from_secs(300);

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
    /// The checker for `root`: in a worker
    /// process when the `oj` binary registered itself for child jobs,
    /// otherwise (oj_server's own tests) on a thread of this process.
    pub(crate) fn start(root: &Path) -> Option<Arc<Typecheck>> {
        let (tx, rx) = std::sync::mpsc::channel();
        let root = root.to_path_buf();
        let exe = crate::plugins::ENGINE_JOB_EXE.get().cloned();
        let thread = std::thread::Builder::new().name("oj-typecheck".into());
        let spawned = match exe {
            Some(exe) => thread.spawn(move || drive_worker(&exe, &root, &rx)),
            None => thread
                .stack_size(CHECKER_STACK)
                .spawn(move || imp::run(root, rx)),
        };
        spawned.ok()?;
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
                    let paths: Vec<PathBuf> = event
                        .paths
                        .into_iter()
                        .filter(|p| oj_typecheck::is_relevant(p))
                        .collect();
                    if !paths.is_empty() && tx.send(Msg::Changed(paths)).is_err() {
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

/// The dev server's side of the worker: forwards each message as a JSON
/// line, (re)starting the worker when it is needed and not running.
fn drive_worker(exe: &Path, root: &Path, rx: &Receiver<Msg>) {
    let mut worker: Option<Worker> = None;
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Warm => {
                Worker::ensure(&mut worker, exe, root, true);
            }
            Msg::Changed(paths) => {
                // Without a worker there is nothing to update: its open reads
                // every file fresh.
                if let Some(w) = &mut worker {
                    if w.send(&serde_json::json!({ "changed": paths })).is_err() {
                        worker = None;
                    }
                }
            }
            Msg::Check { changed, reply } => {
                // A worker that exited before it answered may have died
                // before the request (a kill the reaper has not seen yet):
                // a fresh one gets one more try. One that dies twice reports.
                let mut answer = Err(Failure::Exited(String::new()));
                for _ in 0..2 {
                    answer = match Worker::ensure(&mut worker, exe, root, false) {
                        Some(w) => w.check(&changed),
                        None => Err(Failure::Other(
                            "the type checker could not start".to_string(),
                        )),
                    };
                    match &answer {
                        Err(Failure::Exited(_)) => worker = None,
                        _ => break,
                    }
                }
                let answer = answer.unwrap_or_else(|failure| {
                    worker = None;
                    unavailable(failure.reason())
                });
                let _ = reply.send(answer);
            }
        }
    }
}

enum Failure {
    /// The worker exited (or closed its pipes) before answering.
    Exited(String),
    Other(String),
}

impl Failure {
    fn reason(&self) -> &str {
        match self {
            Failure::Exited(reason) | Failure::Other(reason) => reason,
        }
    }
}

struct Worker {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl Worker {
    /// The running worker, a new one in place of a missing or exited one.
    /// `warm` asks a new worker to open the projects right away.
    fn ensure<'w>(
        slot: &'w mut Option<Worker>,
        exe: &Path,
        root: &Path,
        warm: bool,
    ) -> Option<&'w mut Worker> {
        if slot
            .as_mut()
            .is_some_and(|w| !matches!(w.child.try_wait(), Ok(None)))
        {
            *slot = None;
        }
        if slot.is_none() {
            match Worker::spawn(exe, root) {
                Ok(mut w) => {
                    if warm {
                        let _ = w.send(&serde_json::json!({ "warm": true }));
                    }
                    *slot = Some(w);
                }
                Err(e) => {
                    eprintln!("oj: typecheck: cannot start the worker: {e}");
                    return None;
                }
            }
        }
        slot.as_mut()
    }

    fn spawn(exe: &Path, root: &Path) -> std::io::Result<Worker> {
        let mut child = Command::new(exe)
            .arg("typecheck-worker")
            .arg(root)
            .env(
                crate::plugins::PARENT_PID_ENV,
                std::process::id().to_string(),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child.stdin.take().expect("piped above");
        let stdout = child.stdout.take().expect("piped above");
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("oj-typecheck-reader".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Worker {
            child,
            stdin,
            lines,
        })
    }

    fn send(&mut self, msg: &serde_json::Value) -> std::io::Result<()> {
        writeln!(self.stdin, "{msg}")?;
        self.stdin.flush()
    }

    fn check(&mut self, changed: &[PathBuf]) -> Result<serde_json::Value, Failure> {
        if let Err(e) = self.send(&serde_json::json!({ "check": changed })) {
            return Err(Failure::Exited(format!("the type checker stopped: {e}")));
        }
        match self.lines.recv_timeout(CHECK_TIMEOUT) {
            Ok(line) => serde_json::from_str(&line)
                .map_err(|e| Failure::Other(format!("the type checker answered garbage: {e}"))),
            Err(RecvTimeoutError::Timeout) => Err(Failure::Other(format!(
                "the type check took longer than {}s; the checker restarts",
                CHECK_TIMEOUT.as_secs()
            ))),
            Err(RecvTimeoutError::Disconnected) => {
                let status = self.child.wait().map(|s| s.to_string()).unwrap_or_default();
                Err(Failure::Exited(format!(
                    "the type checker exited ({status}); the next request restarts it"
                )))
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `oj typecheck-worker <root>`: the worker side. Reads one JSON message per
/// stdin line (`{"warm":true}`, `{"changed":[...]}`, `{"check":[...]}`) and
/// writes one JSON report line per `check`. Returns when stdin closes.
pub fn serve_worker(root: &Path) {
    let (tx, rx) = std::sync::mpsc::channel();
    let root = root.to_path_buf();
    let checker = std::thread::Builder::new()
        .name("oj-typecheck".into())
        .stack_size(CHECKER_STACK)
        .spawn(move || imp::run(root, rx))
        .expect("spawn the checker thread");
    let stdout = std::io::stdout();
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let paths = |key: &str| -> Vec<PathBuf> {
            msg[key]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p.as_str().map(PathBuf::from))
                .collect()
        };
        if msg["warm"].as_bool() == Some(true) {
            let _ = tx.send(Msg::Warm);
        } else if msg.get("changed").is_some() {
            let _ = tx.send(Msg::Changed(paths("changed")));
        } else if msg.get("check").is_some() {
            let (reply, answer) = tokio::sync::oneshot::channel();
            let _ = tx.send(Msg::Check {
                changed: paths("check"),
                reply,
            });
            let answer = answer
                .blocking_recv()
                .unwrap_or_else(|_| unavailable("the type checker stopped"));
            let mut out = stdout.lock();
            if writeln!(out, "{answer}")
                .and_then(|()| out.flush())
                .is_err()
            {
                break;
            }
        }
    }
    drop(tx);
    let _ = checker.join();
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

/// Names the bound address for `oj typecheck` (app root, pid, host, port) in
/// the cache root. A stale file is harmless: the client checks the root and
/// the pid, and checks in its own process when they do not match.
pub(crate) fn advertise(root: &Path, host: std::net::IpAddr, port: u16, https: bool) {
    let host = match host {
        std::net::IpAddr::V4(ip) if ip.is_unspecified() => std::net::Ipv4Addr::LOCALHOST.into(),
        std::net::IpAddr::V6(ip) if ip.is_unspecified() => std::net::Ipv6Addr::LOCALHOST.into(),
        host => host,
    };
    let dir = oj_cache::cache_root(root);
    let body = serde_json::json!({
        "root": root,
        "pid": std::process::id(),
        "host": host.to_string(),
        "port": port,
        "https": https,
    });
    let _ = std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(dir.join(ADDRESS_FILE), body.to_string()));
}

/// Asks the dev server running for `root` to check, re-reading `changed`
/// (absolute paths) first. `Ok(None)` when no server with `--typecheck`
/// runs for `root`, `Err` when one runs and did not answer.
pub async fn check_via_server(
    root: &Path,
    changed: &[PathBuf],
) -> Result<Option<serde_json::Value>, String> {
    let Some(url) = server_url(root) else {
        return Ok(None);
    };
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(CHECK_TIMEOUT + Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .post(&url)
        .header(header::CONTENT_TYPE, "application/json")
        .body(serde_json::json!({ "changed": changed }).to_string())
        .send()
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("{url}: {}", response.status()));
    }
    let body = response.bytes().await.map_err(|e| format!("{url}: {e}"))?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| format!("{url}: {e}"))
}

/// The typecheck URL of the live, plain-http dev server the address file
/// names for `root`.
fn server_url(root: &Path) -> Option<String> {
    let root = root.canonicalize().ok()?;
    let text = std::fs::read_to_string(oj_cache::cache_root(&root).join(ADDRESS_FILE)).ok()?;
    let address: serde_json::Value = serde_json::from_str(&text).ok()?;
    if address["root"].as_str().map(Path::new) != Some(root.as_path())
        || address["https"].as_bool() == Some(true)
        || !pid_alive(address["pid"].as_u64()?)
    {
        return None;
    }
    let host: std::net::IpAddr = address["host"].as_str()?.parse().ok()?;
    let port = u16::try_from(address["port"].as_u64()?).ok()?;
    Some(format!(
        "http://{}/@oj/typecheck",
        std::net::SocketAddr::new(host, port)
    ))
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
/// answers. Runs on a thread of its own, with the stack tsrs needs.
pub fn check_once(root: &Path) -> serde_json::Value {
    let root = root.to_path_buf();
    std::thread::Builder::new()
        .name("oj-typecheck".into())
        .stack_size(CHECKER_STACK)
        .spawn(move || imp::check_once(&root))
        .ok()
        .and_then(|t| t.join().ok())
        .unwrap_or_else(|| unavailable("the type checker crashed"))
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
