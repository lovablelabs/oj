// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Hot updates behind a granular editor HMR gate (`OJ_HMR_GATE=1`,
//! `OJ_HMR_FULL_RELOAD=false`) reach every page, not only the ones connected
//! when the flush lets them out. A hot frame goes once, to the sockets open
//! at that moment; a page whose socket was down, or whose document was still
//! loading, used to miss it for good while another page took it in place.
//! Each document is stamped with the hot changes it has (`h`), the page
//! raises it as it applies frames, and a socket that connects behind gets
//! `{"type":"oj:behind","hot":true}`. Each change also ends with an
//! `oj:change` frame naming the pages it covered, so a page whose client
//! registered while the change was being built, and was left out, reloads.
//! The browser tests skip without Chrome, unless `OJ_REQUIRE_BROWSER` is set.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;
#[allow(dead_code)]
mod start_app;

use std::time::Duration;

use browser::Page;
use futures_util::StreamExt;
use start_app::{app, Server, WAIT};
use tokio_tungstenite::tungstenite::Message;

const GATE: &[(&str, &str)] = &[("OJ_HMR_GATE", "1"), ("OJ_HMR_FULL_RELOAD", "false")];
const WIDGET: &str = "src/widgets/glob-widget.jsx";
const SHOWN: &str = "(document.querySelector('[data-testid=\"glob-jsx\"]')?.textContent ?? '')";

/// The fixture's widget showing `text`, in a new Tailwind color when `bg`
/// names one: an edit whose look needs both the patch (the class) and the
/// restyled sheet (its rule).
fn widget(original: &str, text: &str, bg: Option<&str>) -> String {
    let source = original.replace("\"jsxglob:\"", &serde_json::to_string(text).unwrap());
    assert_ne!(source, original, "the widget's text marker moved");
    match bg {
        Some(hex) => {
            let styled = source.replace("<span>", &format!("<span className=\"bg-[{hex}]\">"));
            assert_ne!(styled, source, "the widget's span moved");
            styled
        }
        None => source,
    }
}

/// An editor's edit: `source` written, held by the gate (its status lists
/// the file), then flushed.
async fn edit(dev: &Server, root: &std::path::Path, source: &str) {
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

/// The widget's background color, as the page renders it.
const WIDGET_BG: &str =
    "getComputedStyle(document.querySelector('[data-testid=\"glob-jsx\"] span')).backgroundColor";

/// Waits until `page` hydrated and its hot client registered: the widget's
/// text alone is server-rendered, before the client runs, and an edit with
/// no hot client connected reloads instead of patching.
async fn hydrated(page: &Page) {
    page.wait_for(
        "document.querySelector('[data-testid=\"client-mounted\"]')?.textContent.trim() === 'client-mounted-ok'",
        WAIT,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
}

/// The hot changes a document says it has: `h` on its live-reload client's url.
fn hot_has(html: &str) -> u64 {
    let re = regex::Regex::new(r"live-reload\.js\?g=[0-9a-f]+-[0-9]+&h=([0-9]+)").unwrap();
    re.captures(html)
        .unwrap_or_else(|| panic!("no hot changes on the document's live-reload client:\n{html}"))
        [1]
    .parse()
    .unwrap()
}

/// The first text frame a socket naming `h` gets within `within`.
async fn first_frame(dev: &Server, h: u64, within: Duration) -> Option<String> {
    let url = format!("ws://127.0.0.1:{}/@oj-start/hmr?h={h}", dev.port);
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.expect("dial");
    let frame = tokio::time::timeout(within, async {
        while let Some(Ok(msg)) = ws.next().await {
            if let Message::Text(text) = msg {
                return Some(text.to_string());
            }
        }
        None
    })
    .await
    .ok()
    .flatten();
    let _ = ws.close(None).await;
    frame
}

/// A socket that names fewer hot changes than went out (a document rendered
/// before one, or a page whose socket was down) is told it is behind; one
/// that names them all is not.
#[tokio::test(flavor = "multi_thread")]
async fn a_socket_behind_a_hot_change_is_told_to_reload() {
    let Some(app) = app("hot-gate-behind") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let original = std::fs::read_to_string(root.join(WIDGET)).unwrap();
    let dev = Server::oj_dev_with(&root, GATE);
    // A page keeps the run hot: with no page connected, a change reloads.
    let page = Page::open(&dev.url("/")).await;
    hydrated(&page).await;

    let (_, before) = dev.request("GET", "/", &[]);
    let had = hot_has(&before);
    edit(&dev, &root, &widget(&original, "jsxbehind:", None)).await;
    page.wait_for(&format!("{SHOWN}.includes('jsxbehind:')"), WAIT)
        .await;
    assert!(
        dev.logs("oj start: hot updated"),
        "the edit was not a hot update\n{}",
        dev.output()
    );

    let frame = first_frame(&dev, had, Duration::from_secs(10)).await;
    let frame: serde_json::Value =
        serde_json::from_str(frame.as_deref().expect("no frame for a socket behind")).unwrap();
    assert_eq!(frame["type"], "oj:behind", "{frame}");
    assert_eq!(frame["hot"], true, "{frame}");

    let (_, after) = dev.request("GET", "/", &[]);
    let has = hot_has(&after);
    assert!(
        has > had,
        "a document rendered after the change names it ({has} > {had})"
    );
    let frame = first_frame(&dev, has, Duration::from_millis(1500)).await;
    assert!(
        frame.as_deref().is_none_or(|f| !f.contains("oj:behind")),
        "a current socket was told it is behind: {frame:?}"
    );
    page.close().await;
}

/// Two pages on one dev server, and page A's socket drops just as an edit
/// goes out: its client reconnects a second later, while the edit is still
/// being built, so the engine's patch batch leaves it out and page B alone
/// takes the edit in place. The change ends by naming the pages it covered,
/// and A, not among them, reloads onto it (or, when its socket is back after
/// the change, it connects behind and reloads).
#[tokio::test(flavor = "multi_thread")]
async fn a_page_whose_socket_dropped_during_a_hot_change_still_gets_it() {
    let Some(app) = app("hot-gate-reconnect") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let original = std::fs::read_to_string(root.join(WIDGET)).unwrap();
    let dev = Server::oj_dev_with(&root, GATE);
    // Page A keeps its sockets reachable, so the test can drop one; holds
    // its reconnects (closes a new socket as it opens) while
    // `window.__ojHold` is set; and retries at once instead of after a
    // second, so the test can time its return.
    let keep_sockets = r#"
        (() => {
          const Native = window.WebSocket;
          window.__ojTestSockets = [];
          window.WebSocket = function (...args) {
            const socket = new Native(...args);
            if (window.__ojHold) socket.addEventListener("open", () => socket.close());
            window.__ojTestSockets.push(socket);
            return socket;
          };
          window.WebSocket.prototype = Native.prototype;
          Object.assign(window.WebSocket, Native);
          const later = window.setTimeout;
          window.setTimeout = (fn, ms, ...rest) => later(fn, ms === 1000 ? 20 : ms, ...rest);
        })();
    "#;
    let a = Page::open_with_script(&dev.url("/"), Some(keep_sockets)).await;
    let b = Page::open(&dev.url("/")).await;
    hydrated(&a).await;
    hydrated(&b).await;
    // Both take a first edit in place.
    edit(
        &dev,
        &root,
        &widget(&original, "jsxfirst:", Some("#123456")),
    )
    .await;
    for page in [&a, &b] {
        page.wait_for(&format!("{SHOWN}.includes('jsxfirst:')"), WAIT)
            .await;
    }
    b.eval::<bool>("(window.__mark = 'kept', true)").await;

    // A's socket drops and stays down while the edit is written, held and
    // flushed; it comes back once the rebuild has started, so its client
    // registers while the edit is being built: the engine's patch batch
    // leaves it out, and no catch-up follows.
    let dropped: u32 = a
        .eval(
            "(window.__ojHold = true, window.__ojTestSockets\
             .filter((s) => s.url.includes('/@oj-start/hmr') && s.readyState === 1)\
             .map((s) => s.close()).length)",
        )
        .await;
    assert_eq!(dropped, 1, "page A's hot socket");
    let rebuilds = || dev.output().matches("rebuilding for").count();
    let before = rebuilds();
    edit(
        &dev,
        &root,
        &widget(&original, "jsxmissed:", Some("#abcdef")),
    )
    .await;
    let deadline = tokio::time::Instant::now() + WAIT;
    while rebuilds() == before {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the edit never started a rebuild\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    a.eval::<bool>("(window.__ojHold = false, true)").await;

    let b_shows = b
        .poll(
            &format!("{SHOWN}.includes('jsxmissed:') && {WIDGET_BG} === 'rgb(171, 205, 239)'"),
            Duration::from_secs(30),
        )
        .await;
    assert!(
        b_shows,
        "page B never showed the edit: {:?}; console: {:?}\n{}",
        b.eval::<String>(&format!("{SHOWN} + ' / ' + {WIDGET_BG} + ' / class=' + document.querySelector('[data-testid=\"glob-jsx\"] span').className")).await,
        b.console(),
        dev.output().lines().filter(|l| l.contains("oj start:")).collect::<Vec<_>>().join("\n")
    );
    assert!(
        b.eval::<bool>("window.__mark === 'kept'").await,
        "page B reloaded instead of taking the edit in place"
    );
    // Its text alone comes back by the engine's catch-up patch once its
    // client registers again; the new color's rule is in the restyle that
    // went out while it was away, which only a reload brings.
    let shows_edit =
        format!("{SHOWN}.includes('jsxmissed:') && {WIDGET_BG} === 'rgb(171, 205, 239)'");
    assert!(
        a.poll(&shows_edit, Duration::from_secs(20)).await,
        "page A stayed behind the edit its socket missed: {:?}\n{}",
        a.eval::<String>(&format!("{SHOWN} + ' / ' + {WIDGET_BG}"))
            .await,
        dev.output()
    );
    a.close().await;
    b.close().await;
}

/// The client's rule for a change's end frame, run from the shipped script:
/// named, the page has the change; left out, it needs it unless its document
/// was rendered after it. Whatever else the page applied meanwhile (a style
/// refresh raises its count too) plays no part.
#[tokio::test(flavor = "multi_thread")]
async fn the_client_decides_a_missed_change_by_its_document_not_by_order() {
    if browser::skip() {
        return;
    }
    let script = include_str!("../../oj_server/src/assets/start/live-reload.js");
    let rule = script
        .split("// <oj:missed-change>")
        .nth(1)
        .and_then(|rest| rest.split("// </oj:missed-change>").next())
        .expect("the missed-change rule's markers in live-reload.js");
    let page = Page::open("about:blank").await;
    let cases = [
        // (change, clients, page id, document's changes, missed)
        ("3", r#"["a","b"]"#, r#""a""#, "1", false),
        ("3", r#"["b"]"#, r#""a""#, "1", true),
        // Left out, but rendered after the change: nothing missing.
        ("3", r#"["b"]"#, r#""a""#, "3", false),
        ("3", r#"["b"]"#, r#""a""#, "4", false),
        // No client yet (still loading) and a document from before it.
        ("3", r#"["b"]"#, "null", "2", true),
        // A document that names nothing: a page left out needs it.
        ("3", r#"["b"]"#, r#""a""#, "null", true),
        ("3", r#"["a"]"#, r#""a""#, "null", false),
        // Not a change frame this rule can read.
        ("null", r#"["b"]"#, r#""a""#, "1", false),
    ];
    for (change, clients, id, doc, missed) in cases {
        let expr = format!(
            "(() => {{ {rule}; return missedChange({{ change: {change}, clients: {clients} }}, {id}, {doc}); }})()"
        );
        assert_eq!(
            page.eval::<bool>(&expr).await,
            missed,
            "change {change}, clients {clients}, page {id}, document {doc}"
        );
    }
    page.close().await;
}

/// A page the change does not touch (another route) is named among the
/// pages it covered, with nothing to patch: it stays as it is.
#[tokio::test(flavor = "multi_thread")]
async fn a_page_the_change_does_not_touch_stays_put() {
    let Some(app) = app("hot-gate-other-route") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let original = std::fs::read_to_string(root.join(WIDGET)).unwrap();
    let dev = Server::oj_dev_with(&root, GATE);
    let home = Page::open(&dev.url("/")).await;
    let about = Page::open(&dev.url("/about")).await;
    hydrated(&home).await;
    about
        .wait_for(
            "document.body?.textContent.includes('about-page-marker')",
            WAIT,
        )
        .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    for page in [&home, &about] {
        page.eval::<bool>("(window.__mark = 'kept', true)").await;
    }

    edit(
        &dev,
        &root,
        &widget(&original, "jsxelsewhere:", Some("#13579b")),
    )
    .await;
    home.wait_for(
        &format!("{SHOWN}.includes('jsxelsewhere:') && {WIDGET_BG} === 'rgb(19, 87, 155)'"),
        WAIT,
    )
    .await;
    // Long enough for its frames, and a reload, to have landed.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        home.eval::<bool>("window.__mark === 'kept'").await,
        "the page the change touched reloaded instead of taking it in place"
    );
    assert!(
        about.eval::<bool>("window.__mark === 'kept'").await,
        "a page on another route reloaded for a change that does not touch it\n{}",
        dev.output()
    );
    home.close().await;
    about.close().await;
}

/// The hot changes a document names come with hot updates: with no gate and
/// with a granular one, not behind a full-reload gate (every edit reloads
/// there, and the reload count covers the pages).
#[test]
fn documents_name_their_hot_changes_only_with_hot_updates() {
    let Some(app) = app("hot-gate-stamp") else {
        return;
    };
    let named = |env: &[(&str, &str)]| {
        let dev = Server::oj_dev_with(&app.0, env);
        let (status, html) = dev.request("GET", "/", &[]);
        assert_eq!(status, 200, "{html}");
        regex::Regex::new(r"live-reload\.js\?g=[0-9a-f]+-[0-9]+&h=[0-9]+")
            .unwrap()
            .is_match(&html)
    };
    assert!(named(&[]), "no gate: hot updates, so the changes are named");
    assert!(
        named(GATE),
        "granular gate: hot updates, so the changes are named"
    );
    assert!(
        !named(&[("OJ_HMR_GATE", "1"), ("OJ_HMR_FULL_RELOAD", "true")]),
        "full-reload gate: no hot updates, nothing to name"
    );
}
