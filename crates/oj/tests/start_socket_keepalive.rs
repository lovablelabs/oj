// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A page whose socket goes silent still gets the next change.
//!
//! A connection can die on the way without a close: a proxy or load balancer
//! drops it, a laptop sleeps. The browser then never fires `onclose`, so a
//! client that only reconnects on close keeps the dead socket and misses
//! every change after it until someone refreshes the page. Start dev's page
//! socket now sends `{"type":"oj:ping"}` every `OJ_HMR_PING_MS`, and the
//! page's client replaces a socket that stays silent for two intervals and a
//! grace of one more (at most 10s); the new socket names the page's stamps,
//! and the server brings it up to date. The server also closes a socket whose
//! page stops answering its pings (`oj:pong`).
//!
//! Each test puts [`SilentProxy`] between the page and oj. `silence()` makes
//! the WebSocket connections open at that moment swallow every byte, both
//! ways, without closing: the half-open socket. New connections still pass.
//! The browser tests skip without Chrome, unless `OJ_REQUIRE_BROWSER` is set.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;
#[allow(dead_code)]
mod start_app;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use browser::Page;
use start_app::{app, Server, WAIT};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One browser at a time: the Page helper allows a few Chromes at once.
static BROWSER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const WIDGET: &str = "src/widgets/glob-widget.jsx";
const SHOWN: &str = "(document.querySelector('[data-testid=\"glob-jsx\"]')?.textContent ?? '')";
/// Short, so the watchdog's deadline (three pings at this interval) is under a second.
const PING_MS: &str = "300";
/// Long past that deadline and a reconnect.
const CATCH_UP: Duration = Duration::from_secs(15);
/// The watchdog's console line at [`PING_MS`].
const WATCHDOG: &str = "[oj] no word from the dev server in 900ms; reconnecting";

/// A TCP proxy to oj that can leave its open WebSocket connections
/// half-open: [`SilentProxy::silence`] makes each one drop what either end
/// sends and never close, as a connection lost on the way looks to both
/// ends. Plain HTTP connections always pass: the page's reload must work.
struct SilentProxy {
    port: u16,
    /// Bumped by `silence`: a socket opened under an older one is dead.
    generation: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for SilentProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl SilentProxy {
    async fn start(upstream: u16) -> SilentProxy {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let generation = Arc::new(AtomicU64::new(0));
        let task = tokio::spawn({
            let generation = Arc::clone(&generation);
            async move {
                while let Ok((client, _)) = listener.accept().await {
                    tokio::spawn(relay(client, upstream, Arc::clone(&generation)));
                }
            }
        });
        SilentProxy {
            port,
            generation,
            task,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// Every WebSocket open now goes silent, both ways, for good.
    fn silence(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }
}

async fn relay(mut client: TcpStream, upstream: u16, generation: Arc<AtomicU64>) {
    let Ok(mut server) = TcpStream::connect(("127.0.0.1", upstream)).await else {
        return;
    };
    // The request's head says whether this connection is a WebSocket.
    let mut first = vec![0u8; 16 * 1024];
    let n = match client.read(&mut first).await {
        Ok(0) | Err(_) => return,
        Ok(n) => n,
    };
    let head = String::from_utf8_lossy(&first[..n]).to_ascii_lowercase();
    let socket = head.contains("upgrade: websocket");
    if server.write_all(&first[..n]).await.is_err() {
        return;
    }
    let born = generation.load(Ordering::SeqCst);
    let alive: Arc<dyn Fn() -> bool + Send + Sync> = if socket {
        let generation = Arc::clone(&generation);
        Arc::new(move || generation.load(Ordering::SeqCst) == born)
    } else {
        Arc::new(|| true)
    };
    let (client_read, client_write) = client.into_split();
    let (server_read, server_write) = server.into_split();
    tokio::spawn(pipe(client_read, server_write, Arc::clone(&alive)));
    tokio::spawn(pipe(server_read, client_write, alive));
}

/// Copies `from` to `to` while `alive`; after, swallows what arrives and
/// holds `to` open forever: no byte, no close, no reset.
async fn pipe(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    alive: Arc<dyn Fn() -> bool + Send + Sync>,
) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if !alive() {
            continue;
        }
        if to.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
    if alive() {
        let _ = to.shutdown().await;
    } else {
        // The other end must not learn the connection is gone.
        let _hold = (from, to);
        std::future::pending::<()>().await;
    }
}

fn widget(original: &str, text: &str) -> String {
    let source = original.replace("\"jsxglob:\"", &serde_json::to_string(text).unwrap());
    assert_ne!(source, original, "the widget's text marker moved");
    source
}

/// The page hydrated and its socket open: the hot client registers on
/// connect, and an edit with no hot client connected reloads instead.
async fn hydrated(page: &Page) {
    page.wait_for(
        "document.querySelector('[data-testid=\"client-mounted\"]')?.textContent.trim() === 'client-mounted-ok'",
        WAIT,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
}

/// Records the page's `console.log` lines in sessionStorage, before any of
/// its scripts run, so they outlast a catch-up reload (the browser helper
/// keeps errors and warnings only), and keeps the document's dev server
/// sockets in `window.__ojSockets`.
const RECORD_LOG: &str = "const log = console.log; console.log = (...a) => { const said = JSON.parse(sessionStorage.getItem('said') || '[]'); said.push(a.join(' ')); sessionStorage.setItem('said', JSON.stringify(said)); log(...a); }; window.__ojSockets = []; const Ws = WebSocket; window.WebSocket = class extends Ws { constructor(...a) { super(...a); if (String(a[0]).includes('/@oj-start/hmr')) window.__ojSockets.push(this); } };";

/// The document's dev server sockets still open.
const OPEN_SOCKETS: &str = "window.__ojSockets.filter((s) => s.readyState === 1).length";

/// What the page said in the console, across reloads.
async fn said(page: &Page) -> Vec<String> {
    page.eval("JSON.parse(sessionStorage.getItem('said') || '[]')")
        .await
}

fn shows(text: &str) -> String {
    format!("{SHOWN}.includes({})", serde_json::to_string(text).unwrap())
}

/// An editor's edit under the gate: written, held (the gate's status lists
/// it), then flushed, straight to oj.
async fn gated_edit(dev: &Server, root: &std::path::Path, source: &str) {
    std::fs::write(root.join(WIDGET), source).unwrap();
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let (_, status) = dev.request("GET", "/__hmr_gate", &[]);
        if status.contains("/src/widgets/glob-widget.jsx\":") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the gate never held the edit\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (status, body) = dev.request("POST", "/__hmr_flush", &[]);
    assert_eq!(status, 200, "flush: {body}");
}

/// The page's widget never shows `text` within `within`.
async fn stays_without(page: &Page, text: &str, within: Duration) -> bool {
    !page.poll(&shows(text), within).await
}

/// The case the keepalive is for: the socket goes silent with no close, an
/// edit lands, and the page still shows it, without a refresh.
#[tokio::test(flavor = "multi_thread")]
async fn a_page_whose_socket_went_silent_still_gets_the_next_edit() {
    let Some(app) = app("keepalive-silent") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let _one_at_a_time = BROWSER.lock().await;
    let root = app.0.clone();
    let original = std::fs::read_to_string(root.join(WIDGET)).unwrap();
    let dev = Server::oj_dev_with(&root, &[("OJ_HMR_PING_MS", PING_MS)]);
    let proxy = SilentProxy::start(dev.port).await;
    let page = Page::open_with_script(&proxy.url("/"), Some(RECORD_LOG)).await;
    hydrated(&page).await;

    // Through the proxy, an edit arrives as usual.
    std::fs::write(root.join(WIDGET), widget(&original, "through-proxy:")).unwrap();
    page.wait_for(&shows("through-proxy:"), WAIT).await;

    proxy.silence();
    std::fs::write(root.join(WIDGET), widget(&original, "while-silent:")).unwrap();
    assert!(
        page.poll(&shows("while-silent:"), CATCH_UP).await,
        "the page kept its dead socket and never showed the edit\n{}",
        dev.output()
    );
    // The watchdog did it, and said so.
    let lines = said(&page).await;
    assert!(
        lines.iter().any(|line| line == WATCHDOG),
        "no watchdog line in the console: {lines:?}"
    );
    // One socket, the replacement, and it carries the next edit as a patch
    // in place: the page stays registered for hot updates. (The catch-up
    // may have reloaded the page: its new document hydrates and connects
    // first.)
    hydrated(&page).await;
    assert!(
        page.poll(&format!("{OPEN_SOCKETS} === 1"), CATCH_UP).await,
        "the page has {} open dev server sockets",
        page.eval::<u32>(OPEN_SOCKETS).await
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    let open: u32 = page.eval(OPEN_SOCKETS).await;
    assert_eq!(open, 1, "a second socket opened: {open}");
    page.eval::<bool>("(window.__kept = true)").await;
    std::fs::write(root.join(WIDGET), widget(&original, "after:")).unwrap();
    page.wait_for(&shows("after:"), WAIT).await;
    assert!(
        page.eval::<bool>("window.__kept === true").await,
        "the edit after the recovery reloaded the page instead of patching it"
    );
    assert!(page.errors().is_empty(), "page errors: {:?}", page.errors());
    page.close().await;
}

/// The socket goes silent with nothing changing: the page replaces it, and
/// the server later closes the dead one at its own deadline. The page keeps
/// its document (nothing to catch up on) and its runtime id, so that late
/// close must not unregister it from hot updates: the next edit still
/// arrives as a patch in place, on one socket.
#[tokio::test(flavor = "multi_thread")]
async fn a_page_whose_socket_went_silent_with_nothing_changing_keeps_patching_in_place() {
    let Some(app) = app("keepalive-blip") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let _one_at_a_time = BROWSER.lock().await;
    let root = app.0.clone();
    let original = std::fs::read_to_string(root.join(WIDGET)).unwrap();
    let dev = Server::oj_dev_with(&root, &[("OJ_HMR_PING_MS", PING_MS)]);
    let proxy = SilentProxy::start(dev.port).await;
    let page = Page::open_with_script(&proxy.url("/"), Some(RECORD_LOG)).await;
    hydrated(&page).await;
    page.eval::<bool>("(window.__kept = true)").await;

    proxy.silence();
    assert!(
        page.poll(
            &format!(
                "JSON.parse(sessionStorage.getItem('said') || '[]').includes({})",
                serde_json::to_string(WATCHDOG).unwrap()
            ),
            CATCH_UP
        )
        .await,
        "the watchdog never replaced the silent socket: {:?}",
        said(&page).await
    );
    // Past the server's deadline for the dead socket (900ms from its last
    // pong, checked each 300ms), so its late close has happened.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        dev.logs("a page stopped answering pings"),
        "the server never closed the dead socket\n{}",
        dev.output()
    );

    std::fs::write(root.join(WIDGET), widget(&original, "after-blip:")).unwrap();
    page.wait_for(&shows("after-blip:"), WAIT).await;
    assert!(
        page.eval::<bool>("window.__kept === true").await,
        "the page reloaded: its replacement socket lost its hot client\n{}",
        dev.output()
    );
    let open: u32 = page.eval(OPEN_SOCKETS).await;
    assert_eq!(open, 1, "the page has {open} open dev server sockets");
    assert!(page.errors().is_empty(), "page errors: {:?}", page.errors());
    page.close().await;
}

/// The same, as the editor runs it: behind a granular gate, the edit held
/// and flushed while the page's socket is silent.
#[tokio::test(flavor = "multi_thread")]
async fn a_page_whose_socket_went_silent_still_gets_a_gated_edit() {
    let Some(app) = app("keepalive-silent-gated") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let _one_at_a_time = BROWSER.lock().await;
    let root = app.0.clone();
    let original = std::fs::read_to_string(root.join(WIDGET)).unwrap();
    let dev = Server::oj_dev_with(
        &root,
        &[
            ("OJ_HMR_GATE", "1"),
            ("OJ_HMR_FULL_RELOAD", "false"),
            ("OJ_HMR_PING_MS", PING_MS),
        ],
    );
    let proxy = SilentProxy::start(dev.port).await;
    let page = Page::open(&proxy.url("/")).await;
    hydrated(&page).await;

    gated_edit(&dev, &root, &widget(&original, "gated-through-proxy:")).await;
    page.wait_for(&shows("gated-through-proxy:"), WAIT).await;

    proxy.silence();
    gated_edit(&dev, &root, &widget(&original, "gated-while-silent:")).await;
    assert!(
        page.poll(&shows("gated-while-silent:"), CATCH_UP).await,
        "the page kept its dead socket and never showed the flushed edit\n{}",
        dev.output()
    );
    assert!(page.errors().is_empty(), "page errors: {:?}", page.errors());
    page.close().await;
}

/// Without the keepalive (`OJ_HMR_PING_MS=0`) the page keeps the dead
/// socket and stays stale: the failure the tests above guard against, and
/// proof that the proxy reproduces it.
#[tokio::test(flavor = "multi_thread")]
async fn without_the_keepalive_a_silent_socket_leaves_the_page_stale() {
    let Some(app) = app("keepalive-off") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let _one_at_a_time = BROWSER.lock().await;
    let root = app.0.clone();
    let original = std::fs::read_to_string(root.join(WIDGET)).unwrap();
    let dev = Server::oj_dev_with(&root, &[("OJ_HMR_PING_MS", "0")]);
    let proxy = SilentProxy::start(dev.port).await;
    let page = Page::open(&proxy.url("/")).await;
    hydrated(&page).await;

    std::fs::write(root.join(WIDGET), widget(&original, "off-through-proxy:")).unwrap();
    page.wait_for(&shows("off-through-proxy:"), WAIT).await;

    proxy.silence();
    std::fs::write(root.join(WIDGET), widget(&original, "off-while-silent:")).unwrap();
    assert!(
        stays_without(&page, "off-while-silent:", Duration::from_secs(5)).await,
        "the page caught up with no keepalive: the proxy did not silence its socket"
    );
    page.close().await;
}
