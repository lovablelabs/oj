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

/// Returns early from a test when no Chrome is installed.
#[macro_export]
macro_rules! require_chrome {
    () => {
        if $crate::browser::chrome().is_none() {
            eprintln!("skipping: no Chrome (npx --prefix e2e playwright install chromium)");
            return;
        }
    };
}

/// Bound on any one CDP round trip, so a stuck protocol fails the test.
const CDP: Duration = Duration::from_secs(30);

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, Value>>>>>;

/// One Chrome per test, with one page attached through a flat session.
pub struct Page {
    chrome: tokio::process::Child,
    out: mpsc::UnboundedSender<Message>,
    pending: Pending,
    next_id: AtomicU64,
    session: String,
    errors: Arc<Mutex<Vec<String>>>,
}

impl Page {
    pub async fn open(url: &str) -> Page {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let profile =
            crate::run_dir().join(format!("chrome-{}", NEXT.fetch_add(1, Ordering::Relaxed)));
        let mut chrome = tokio::process::Command::new(chrome().expect("require_chrome! first"))
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
            // Parallel tests must not share a profile.
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
        {
            let (pending, errors) = (Arc::clone(&pending), Arc::clone(&errors));
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
                    }
                }
            });
        }
        let mut page = Page {
            chrome,
            out,
            pending,
            next_id: AtomicU64::new(1),
            session: String::new(),
            errors,
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
        page.send("Page.navigate", json!({ "url": url })).await;
        page
    }

    async fn call(&self, session: Option<&str>, method: &str, params: Value) -> Value {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let mut frame = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            frame["sessionId"] = json!(session);
        }
        let _ = self.out.send(Message::text(frame.to_string()));
        match tokio::time::timeout(CDP, rx).await {
            Ok(Ok(Ok(result))) => result,
            Ok(Ok(Err(err))) => panic!("chrome {method}: {err}"),
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
        let res = self
            .send(
                "Runtime.evaluate",
                json!({ "expression": expr, "returnByValue": true, "awaitPromise": true }),
            )
            .await;
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
        let deadline = tokio::time::Instant::now() + within;
        let expr = format!("Boolean({predicate})");
        loop {
            // Throws while the page is still navigating; keep polling.
            if self.try_eval(&expr).await == Ok(Value::Bool(true)) {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!(
                    "timed out after {within:?} waiting for {predicate}; page errors: {:?}",
                    self.errors()
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
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

/// The page-error capture the browser tests assert empty must see a throw,
/// or those assertions are vacuous.
#[tokio::test(flavor = "multi_thread")]
async fn uncaught_page_errors_are_captured() {
    crate::require_chrome!();
    let app = crate::App::new(&[
        (
            "package.json",
            r#"{"name":"page-errors","version":"1.0.0"}"#,
        ),
        (
            "src/main.js",
            "window.__READY = true;\nsetTimeout(() => { throw new Error(\"boom-from-page\"); });\n",
        ),
        ("index.html", crate::INDEX_HTML),
    ]);
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for("window.__READY === true", Duration::from_secs(20))
        .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while page.errors().is_empty() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let errors = page.errors();
    assert!(
        errors.iter().any(|e| e.contains("boom-from-page")),
        "captured: {errors:?}"
    );
    page.close().await;
}
