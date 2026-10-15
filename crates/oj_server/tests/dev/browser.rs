// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A real headless Chrome over the DevTools protocol, for checks only a page
//! can make (HMR applied in place, module side effects). Raw JSON over the
//! DevTools websocket rather than a typed client: typed clients lag Chrome's
//! protocol and drop the events they cannot parse. Tests skip when no
//! Chrome is found: `OJ_TEST_CHROME`, else the newest Playwright-installed
//! headless shell or Chromium (`npx --prefix e2e playwright install chromium`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

/// The Chrome binary to drive, or None to skip.
pub fn chrome() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("OJ_TEST_CHROME") {
        return Some(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let caches = [
        std::env::var_os("PLAYWRIGHT_BROWSERS_PATH").map(PathBuf::from),
        Some(home.join("Library/Caches/ms-playwright")),
        Some(home.join(".cache/ms-playwright")),
    ];
    // Headless shell first: lighter, and what Playwright itself launches headless.
    let layouts: [(&str, &[&str]); 2] = [
        (
            "chromium_headless_shell-",
            &[
                "chrome-headless-shell-linux64/chrome-headless-shell",
                "chrome-headless-shell-mac-arm64/chrome-headless-shell",
                "chrome-headless-shell-mac-x64/chrome-headless-shell",
            ],
        ),
        (
            "chromium-",
            &[
                "chrome-linux64/chrome",
                "chrome-linux/chrome",
                "chrome-mac-arm64/Chromium.app/Contents/MacOS/Chromium",
                "chrome-mac/Chromium.app/Contents/MacOS/Chromium",
            ],
        ),
    ];
    for cache in caches.into_iter().flatten() {
        for (prefix, bins) in layouts {
            let mut dirs: Vec<(u32, PathBuf)> = std::fs::read_dir(&cache)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().into_string().ok()?;
                    let rev = name.strip_prefix(prefix)?.parse().ok()?;
                    Some((rev, e.path()))
                })
                .collect();
            dirs.sort_by_key(|(rev, _)| std::cmp::Reverse(*rev));
            for (_, dir) in dirs {
                if let Some(bin) = bins.iter().map(|b| dir.join(b)).find(|b| b.exists()) {
                    return Some(bin);
                }
            }
        }
    }
    None
}

/// True (after saying why) when no Chrome is installed: `if skip() { return; }`.
pub fn skip() -> bool {
    let missing = chrome().is_none();
    // CI installs Chrome; a skip there would pass every browser test unrun.
    if missing && std::env::var_os("OJ_REQUIRE_BROWSER").is_some() {
        panic!("OJ_REQUIRE_BROWSER is set but no Chrome was found");
    }
    if missing {
        eprintln!("skipping: no Chrome (npx --prefix e2e playwright install chromium)");
    }
    missing
}

/// Bound on any one CDP round trip, so a stuck protocol fails the test.
const CDP: Duration = Duration::from_secs(30);

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, Value>>>>>;

/// One Chrome per test, with one page attached through a flat session.
pub struct Page {
    chrome: tokio::process::Child,
    profile: PathBuf,
    _slot: tokio::sync::SemaphorePermit<'static>,
    out: mpsc::UnboundedSender<Message>,
    pending: Pending,
    next_id: AtomicU64,
    session: String,
    errors: Arc<Mutex<Vec<String>>>,
    console: Arc<Mutex<Vec<String>>>,
}

impl Page {
    pub async fn open(url: &str) -> Page {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        // A dozen Chromes booting beside the dev servers starve each other
        // into page-wait timeouts; a few at a time keeps the waits honest.
        static SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
        let slot = SLOTS.acquire().await.unwrap();
        // Parallel tests must not share a profile; removed once Chrome exits.
        let profile = std::env::temp_dir().join(format!(
            "oj-chrome-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut chrome = tokio::process::Command::new(chrome().expect("check skip() first"))
            .args([
                "--headless",
                // CI containers have no user namespace for Chrome's sandbox.
                "--no-sandbox",
                "--remote-debugging-port=0",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-gpu",
                "--disable-background-timer-throttling",
                "--disable-renderer-backgrounding",
            ])
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("about:blank")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn chrome");
        let endpoint = tokio::time::timeout(CDP, devtools_endpoint(chrome.stderr.take().unwrap()))
            .await
            .expect("chrome printed its DevTools endpoint");
        let (ws, _) = tokio_tungstenite::connect_async(endpoint.as_str())
            .await
            .expect("connect to chrome");
        let (mut sink, mut stream) = ws.split();
        let (out, mut out_rx) = mpsc::unbounded_channel::<Message>();
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if sink.send(msg).await.is_err() {
                    break;
                }
            }
        });
        let pending: Pending = Arc::default();
        let errors: Arc<Mutex<Vec<String>>> = Arc::default();
        let console: Arc<Mutex<Vec<String>>> = Arc::default();
        {
            let (pending, errors, console) = (
                Arc::clone(&pending),
                Arc::clone(&errors),
                Arc::clone(&console),
            );
            tokio::spawn(async move {
                while let Some(Ok(msg)) = stream.next().await {
                    let Ok(text) = msg.into_text() else { continue };
                    let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                        continue;
                    };
                    if let Some(id) = frame["id"].as_u64() {
                        if let Some(tx) = pending.lock().unwrap().remove(&id) {
                            let _ = tx.send(match frame.get("error") {
                                Some(err) => Err(err.clone()),
                                None => Ok(frame["result"].clone()),
                            });
                        }
                    } else if frame["method"] == "Runtime.exceptionThrown" {
                        let detail = &frame["params"]["exceptionDetails"];
                        let text = detail["exception"]["description"]
                            .as_str()
                            .or(detail["text"].as_str())
                            .unwrap_or("uncaught exception");
                        errors.lock().unwrap().push(text.to_string());
                    } else if frame["method"] == "Runtime.consoleAPICalled" {
                        let params = &frame["params"];
                        let kind = params["type"].as_str().unwrap_or("");
                        if kind == "error" || kind == "warning" {
                            let text = params["args"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .map(|a| match a["value"].as_str() {
                                    Some(s) => s.to_string(),
                                    None => a["description"]
                                        .as_str()
                                        .map(str::to_string)
                                        .unwrap_or_else(|| a["value"].to_string()),
                                })
                                .collect::<Vec<_>>()
                                .join(" ");
                            console.lock().unwrap().push(format!("{kind}: {text}"));
                        }
                    } else if frame["method"] == "Log.entryAdded" {
                        // Chrome's own messages: failed loads, preload warnings.
                        let entry = &frame["params"]["entry"];
                        let level = entry["level"].as_str().unwrap_or("");
                        if level == "error" || level == "warning" {
                            let text = entry["text"].as_str().unwrap_or("");
                            let url = entry["url"].as_str().unwrap_or("");
                            console
                                .lock()
                                .unwrap()
                                .push(format!("{level}: {text} {url}"));
                        }
                    }
                }
            });
        }
        let mut page = Page {
            chrome,
            profile,
            _slot: slot,
            out,
            pending,
            next_id: AtomicU64::new(1),
            session: String::new(),
            errors,
            console,
        };
        let target = page
            .call(None, "Target.createTarget", json!({ "url": "about:blank" }))
            .await;
        let attached = page
            .call(
                None,
                "Target.attachToTarget",
                json!({ "targetId": target["targetId"], "flatten": true }),
            )
            .await;
        page.session = attached["sessionId"].as_str().unwrap().to_string();
        page.send("Runtime.enable", json!({})).await;
        page.send("Page.enable", json!({})).await;
        page.send("Log.enable", json!({})).await;
        page.send("Page.navigate", json!({ "url": url })).await;
        page
    }

    async fn call(&self, session: Option<&str>, method: &str, params: Value) -> Value {
        match self.try_call(session, method, params).await {
            Ok(result) => result,
            Err(err) => panic!("chrome {method}: {err}"),
        }
    }

    /// A command whose error reply comes back as Err (a timeout or a closed
    /// connection still panics: the browser is gone).
    async fn try_call(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let mut frame = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            frame["sessionId"] = json!(session);
        }
        let _ = self.out.send(Message::text(frame.to_string()));
        match tokio::time::timeout(CDP, rx).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => panic!("chrome {method}: connection closed"),
            Err(_) => panic!("chrome {method}: no answer in {CDP:?}"),
        }
    }

    /// A command on the page's session.
    async fn send(&self, method: &str, params: Value) -> Value {
        self.call(Some(&self.session), method, params).await
    }

    /// Evaluates `expr` in the page and deserializes its value; an exception
    /// in the expression is an Err with its description.
    async fn try_eval(&self, expr: &str) -> Result<Value, String> {
        // An error reply too: while a reload swaps documents Chrome answers
        // "execution context was destroyed" or finds no default context.
        let res = self
            .try_call(
                Some(&self.session),
                "Runtime.evaluate",
                json!({ "expression": expr, "returnByValue": true, "awaitPromise": true }),
            )
            .await
            .map_err(|err| err.to_string())?;
        if let Some(detail) = res.get("exceptionDetails") {
            return Err(detail["exception"]["description"]
                .as_str()
                .or(detail["text"].as_str())
                .unwrap_or("exception")
                .to_string());
        }
        Ok(res["result"].get("value").cloned().unwrap_or(Value::Null))
    }

    /// Evaluates `expr` in the page and deserializes the result.
    pub async fn eval<T: DeserializeOwned>(&self, expr: &str) -> T {
        let value = self
            .try_eval(expr)
            .await
            .unwrap_or_else(|e| panic!("evaluate {expr}: {e}"));
        serde_json::from_value(value).unwrap_or_else(|e| panic!("evaluate {expr} result: {e}"))
    }

    /// Polls `predicate` (a JS expression) until it is true, like
    /// Playwright's waitForFunction; panics after `within`.
    pub async fn wait_for(&self, predicate: &str, within: Duration) {
        if !self.poll(predicate, within).await {
            panic!(
                "timed out after {within:?} waiting for {predicate}; page errors: {:?}",
                self.errors()
            );
        }
    }

    /// `wait_for` that reports a timeout as false, for callers that want to
    /// gather their own diagnostics first.
    pub async fn poll(&self, predicate: &str, within: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        let expr = format!("Boolean({predicate})");
        loop {
            // Throws, or has no context, while the page is still navigating.
            if self.try_eval(&expr).await == Ok(Value::Bool(true)) {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Console errors and warnings so far, the page's and Chrome's own
    /// (Playwright's `console` event filtered to error and warning).
    pub fn console(&self) -> Vec<String> {
        self.console.lock().unwrap().clone()
    }

    /// Uncaught exceptions so far (Playwright's `pageerror`).
    pub fn errors(&self) -> Vec<String> {
        self.errors.lock().unwrap().clone()
    }

    pub async fn close(mut self) {
        let _ = self.out.send(Message::text(
            json!({ "id": 0, "method": "Browser.close", "params": {} }).to_string(),
        ));
        if tokio::time::timeout(Duration::from_secs(5), self.chrome.wait())
            .await
            .is_err()
        {
            let _ = self.chrome.kill().await;
        }
    }
}

/// Also on a panicking test: Chrome is killed on drop, and its profile goes.
impl Drop for Page {
    fn drop(&mut self) {
        let _ = self.chrome.start_kill();
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

/// Chrome prints `DevTools listening on ws://...` on stderr once it serves.
async fn devtools_endpoint(stderr: tokio::process::ChildStderr) -> String {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(url) = line.strip_prefix("DevTools listening on ") {
            let url = url.trim().to_string();
            // Drain the rest so a full pipe never blocks Chrome.
            tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
            return url;
        }
    }
    panic!("chrome exited before printing its DevTools endpoint");
}
