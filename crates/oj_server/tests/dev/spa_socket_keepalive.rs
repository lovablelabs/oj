// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A Vite-protocol page whose socket goes silent still gets the next edit.
//!
//! A connection can die on the way without a close (a proxy or load balancer
//! drops it, a laptop sleeps), and the browser never fires `onclose`. The
//! server pings the socket every interval with Vite's `{"type":"ping"}`, and
//! the client's watchdog (socket-watchdog.js, shared with Start's client)
//! drops a socket silent past its deadline; the page then reloads once the
//! server answers, as after any lost socket, onto whatever changed meanwhile.
//! [`SilentProxy`] between the page and the server makes the open sockets
//! half-open on demand.

use std::time::Duration;

use crate::browser::Page;
use crate::silent_proxy::SilentProxy;
use crate::{require_fixture, App};

const WAIT: Duration = Duration::from_secs(20);
/// A 900ms deadline: two pings and one more.
const PING: Duration = Duration::from_millis(300);
const WATCHDOG: &str = "[oj] no word from the dev server in 900ms; reconnecting";

const INDEX: &str = r#"<!doctype html><html><head><title>t</title></head><body><div id="root"></div><script type="module" src="/src/main.tsx"></script></body></html>"#;
const MAIN: &str = "import { createRoot } from \"react-dom/client\";\nimport { App } from \"./App\";\ncreateRoot(document.getElementById(\"root\")!).render(<App />);\n";

fn app_tsx(text: &str) -> String {
    format!("export function App() {{\n  return <h1>{text}</h1>;\n}}\n")
}

fn h1_is(text: &str) -> String {
    format!(r#"(document.querySelector("h1")?.textContent ?? "") === {text:?}"#)
}

/// The page's console.log lines, across its reloads.
const RECORD_LOG: &str = "const log = console.log; console.log = (...a) => { const said = JSON.parse(sessionStorage.getItem('said') || '[]'); said.push(a.join(' ')); sessionStorage.setItem('said', JSON.stringify(said)); log(...a); };";

async fn said(page: &Page) -> Vec<String> {
    let json: String = page.eval("sessionStorage.getItem('said') || '[]'").await;
    serde_json::from_str(&json).unwrap()
}

/// The app, its server and listener (kept alive), and the proxy the page goes through.
type Served = (App, crate::Server, crate::listen::Listening, SilentProxy);

async fn app_behind_proxy(ping: Option<Duration>) -> Option<Served> {
    if crate::browser::skip() {
        return None;
    }
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"spa-keepalive","version":"1.0.0"}"#,
        ),
        ("index.html", INDEX),
        ("src/main.tsx", MAIN),
        ("src/App.tsx", &app_tsx("v1")),
    ]);
    app.link_fixture("react");
    app.link_fixture("react-dom");
    app.link_fixture("scheduler");
    let server = app.serve().await;
    server.built.set_hmr_ping(ping);
    let listening = server.listen().await;
    let proxy = SilentProxy::start(listening.addr.port()).await;
    Some((app, server, listening, proxy))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_spa_page_whose_socket_went_silent_still_gets_the_next_edit() {
    require_fixture!("react", "react-dom", "scheduler");
    let Some((app, server, _listening, proxy)) = app_behind_proxy(Some(PING)).await else {
        return;
    };
    let subscribers = || server.built.reload_tx.receiver_count();
    let before = subscribers();
    let page = Page::open_with_script(&proxy.url("/"), Some(RECORD_LOG)).await;
    page.wait_for(&h1_is("v1"), WAIT).await;
    assert_eq!(subscribers(), before + 1, "the page's socket");

    proxy.silence();
    app.write(&[("src/App.tsx", &app_tsx("v2"))]);
    assert!(
        page.poll(&h1_is("v2"), Duration::from_secs(15)).await,
        "the page kept its dead socket and never showed the edit"
    );
    let lines = said(&page).await;
    assert!(
        lines.iter().any(|line| line == WATCHDOG),
        "no watchdog line in the console: {lines:?}"
    );
    // Both ends, as engine.io's "should trigger on both ends upon ping
    // timeout": the server closes the dead socket at its own deadline, so
    // only the reloaded page's socket is left.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while subscribers() != before + 1 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        subscribers(),
        before + 1,
        "the server kept the dead socket next to the new one"
    );

    // The reloaded page's socket carries the next edit, in place.
    page.eval::<bool>("(window.__kept = true)").await;
    app.write(&[("src/App.tsx", &app_tsx("v3"))]);
    page.wait_for(&h1_is("v3"), WAIT).await;
    assert!(
        page.eval::<bool>("window.__kept === true").await,
        "the edit after the recovery reloaded the page instead of updating it"
    );
    assert!(page.errors().is_empty(), "page errors: {:?}", page.errors());
    page.close().await;
}

/// The control: with no keepalive the same silence leaves the page stale, so
/// the proxy reproduces the failure the watchdog fixes.
#[tokio::test(flavor = "multi_thread")]
async fn without_the_keepalive_a_silent_spa_socket_leaves_the_page_stale() {
    require_fixture!("react", "react-dom", "scheduler");
    let Some((app, _server, _listening, proxy)) = app_behind_proxy(None).await else {
        return;
    };
    let page = Page::open(&proxy.url("/")).await;
    page.wait_for(&h1_is("v1"), WAIT).await;

    proxy.silence();
    app.write(&[("src/App.tsx", &app_tsx("v2"))]);
    assert!(
        !page.poll(&h1_is("v2"), Duration::from_secs(5)).await,
        "the page caught up without a keepalive: the proxy did not silence its socket"
    );
    page.close().await;
}
