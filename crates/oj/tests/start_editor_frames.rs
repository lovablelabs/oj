// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! What an editor hears on Vite's client channel while a Start app runs.
//! In Vite that socket carries what the pages are told: an `update` for a
//! change they take in place, a `full-reload` when they reload
//! (server/hmr.ts `updateModules`). A Start page gets its patches on
//! Start's own socket, so the dev server tells the editor's socket the same
//! two things itself: an update when the pages took the change in place, a
//! full-reload when they were told to reload, each when the editor's gate
//! lets the change out. It used to hear only the inner server's module
//! graph, which a page on the bundle does not run.
//!
//! A sheet a route links by its url (`./app.css?url` in `head().links`) is
//! one of those in-place changes: Vite swaps the linked sheet with a
//! `css-update` and reloads nothing. The bundle holds only the sheet's url,
//! which the edit does not change, so the pages re-link it; it used to
//! rebuild the bundle in full and reload them.
//!
//! A component file that also exports something Fast Refresh cannot take (a
//! helper beside the components) invalidates itself on every edit, and its
//! importers re-run in place, as under Vite. The refresh wrapper imports
//! the module itself to compare exports, and the page's runtime counted
//! that import among the importers: the invalidation came back to where it
//! started, and every such edit reloaded the page. An importer the page
//! holds no factory for (it came with the page's first bundle; a patch or a
//! lazy chunk registers one) is asked of the dev server, which sends it as
//! an update, as Vite's server answers `vite:invalidate` with an update for
//! the importers.
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
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const GATE: &[(&str, &str)] = &[("OJ_HMR_GATE", "1"), ("OJ_HMR_FULL_RELOAD", "false")];
const WIDGET: &str = "src/widgets/glob-widget.jsx";
const SHOWN: &str = "(document.querySelector('[data-testid=\"glob-jsx\"]')?.textContent ?? '')";

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The editor's socket: Vite's client channel at the preview's root.
async fn editor_socket(dev: &Server) -> Socket {
    let mut request = format!("ws://127.0.0.1:{}/", dev.port)
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", "vite-hmr".parse().unwrap());
    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("dial the editor's socket");
    socket
}

/// The `update` and `full-reload` frames the socket gets within `within`.
async fn decisions(socket: &mut Socket, within: Duration) -> Vec<serde_json::Value> {
    let mut got = Vec::new();
    let _ = tokio::time::timeout(within, async {
        while let Some(Ok(msg)) = socket.next().await {
            let Message::Text(text) = msg else { continue };
            let Ok(frame) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            if frame["type"] == "update" || frame["type"] == "full-reload" {
                got.push(frame);
            }
        }
    })
    .await;
    got
}

/// The next `update` or `full-reload` frame.
async fn next_decision(socket: &mut Socket, dev: &Server) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if let Some(frame) = decisions(socket, Duration::from_millis(200))
            .await
            .into_iter()
            .next()
        {
            return frame;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the editor's socket heard nothing\n{}",
            dev.output()
        );
    }
}

/// The widget edited to show `text`, written and held by the gate.
async fn write_held(dev: &Server, root: &std::path::Path, original: &str, text: &str) {
    let source = original.replace("\"jsxglob:\"", &serde_json::to_string(text).unwrap());
    assert_ne!(source, original, "the widget's text marker moved");
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
}

fn flush(dev: &Server) {
    let (status, body) = dev.request("POST", "/__hmr_flush", &[]);
    assert_eq!(status, 200, "flush: {body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_the_page_takes_in_place_is_an_update_on_the_editors_socket() {
    let Some(app) = app("editor-frames-hot") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let original = std::fs::read_to_string(root.join(WIDGET)).unwrap();
    let dev = Server::oj_dev_with(&root, GATE);
    let page = Page::open(&dev.url("/")).await;
    page.wait_for(
        "document.querySelector('[data-testid=\"client-mounted\"]')?.textContent.trim() === 'client-mounted-ok'",
        WAIT,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    page.eval::<bool>("(window.__mark = 'kept', true)").await;
    let mut editor = editor_socket(&dev).await;

    write_held(&dev, &root, &original, "jsxeditor:").await;
    // Built behind the gate: the pages have nothing yet, nor does the editor.
    let deadline = tokio::time::Instant::now() + WAIT;
    while !dev.logs("oj start: hot updated") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the edit was not a hot update\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let early = decisions(&mut editor, Duration::from_millis(700)).await;
    assert!(
        early.is_empty(),
        "the editor heard of a change its gate still holds: {early:?}"
    );

    flush(&dev);
    let frame = next_decision(&mut editor, &dev).await;
    assert_eq!(frame["type"], "update", "{frame}\n{}", dev.output());
    let updates = frame["updates"].as_array().unwrap();
    assert_eq!(updates.len(), 1, "{frame}");
    assert_eq!(updates[0]["type"], "js-update", "{frame}");
    assert_eq!(
        updates[0]["path"], "/src/widgets/glob-widget.jsx",
        "{frame}"
    );
    assert!(
        updates[0]["timestamp"].as_u64().is_some_and(|t| t > 0),
        "{frame}"
    );

    page.wait_for(&format!("{SHOWN}.includes('jsxeditor:')"), WAIT)
        .await;
    // Long enough for a reload, and its frame, to have landed.
    let late = decisions(&mut editor, Duration::from_secs(2)).await;
    assert!(
        !late.iter().any(|f| f["type"] == "full-reload"),
        "the editor heard a reload for a change the page took in place: {late:?}\n{}",
        dev.output()
    );
    assert!(
        page.eval::<bool>("window.__mark === 'kept'").await,
        "the page reloaded"
    );
    page.close().await;
}

/// With no page connected nothing can take the change in place: the pages
/// are told to reload (a document that loads later is behind until it
/// does), and that is what the editor hears, once its gate lets it out.
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_the_pages_reload_for_is_a_full_reload_on_the_editors_socket() {
    let Some(app) = app("editor-frames-reload") else {
        return;
    };
    let root = app.0.clone();
    let original = std::fs::read_to_string(root.join(WIDGET)).unwrap();
    let dev = Server::oj_dev_with(&root, GATE);
    let mut editor = editor_socket(&dev).await;

    write_held(&dev, &root, &original, "jsxreload:").await;
    let deadline = tokio::time::Instant::now() + WAIT;
    while !dev.logs("reload held for the editor's flush") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the edit was not rebuilt behind the gate\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let early = decisions(&mut editor, Duration::from_millis(700)).await;
    assert!(
        early.is_empty(),
        "the editor heard of a reload its gate still holds: {early:?}"
    );

    flush(&dev);
    let frame = next_decision(&mut editor, &dev).await;
    assert_eq!(frame["type"], "full-reload", "{frame}\n{}", dev.output());
    assert_eq!(frame["path"], "*", "{frame}");
    let late = decisions(&mut editor, Duration::from_secs(2)).await;
    assert!(
        late.is_empty(),
        "one reload, one frame: {late:?}\n{}",
        dev.output()
    );
}

const LINKED: &str = "styles/linked.css";
const WIDGET_OUTLINE: &str =
    "getComputedStyle(document.querySelector('[data-testid=\"glob-jsx\"]')).outlineColor";

fn linked_sheet(rgb: &str) -> String {
    format!("[data-testid=\"glob-jsx\"] {{ outline: 3px solid {rgb}; }}\n")
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_to_a_sheet_linked_by_its_url_restyles_the_page_in_place() {
    let Some(app) = app("editor-frames-linked-sheet") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    // The root route links a sheet of the app's by its url, as a Start
    // template's `import appCss from "../styles.css?url"` does.
    std::fs::write(root.join(LINKED), linked_sheet("rgb(1, 2, 3)")).unwrap();
    let route = root.join("src/routes/__root.tsx");
    let source = std::fs::read_to_string(&route).unwrap();
    let linked = source
        .replace(
            "import \"../../styles/app.css\";",
            "import \"../../styles/app.css\";\nimport linkedUrl from \"../../styles/linked.css?url\";",
        )
        .replace(
            "meta: [{ title: \"oj start fixture\" }],",
            "meta: [{ title: \"oj start fixture\" }],\n    links: [{ rel: \"stylesheet\", href: linkedUrl }],",
        );
    assert_eq!(
        linked.matches("linkedUrl").count(),
        2,
        "the fixture's root route moved"
    );
    std::fs::write(&route, linked).unwrap();

    let dev = Server::oj_dev_with(&root, GATE);
    let page = Page::open(&dev.url("/")).await;
    page.wait_for(
        "document.querySelector('[data-testid=\"client-mounted\"]')?.textContent.trim() === 'client-mounted-ok'",
        WAIT,
    )
    .await;
    page.wait_for(&format!("{WIDGET_OUTLINE} === 'rgb(1, 2, 3)'"), WAIT)
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    page.eval::<bool>("(window.__mark = 'kept', true)").await;
    let mut editor = editor_socket(&dev).await;

    std::fs::write(root.join(LINKED), linked_sheet("rgb(4, 5, 6)")).unwrap();
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let (_, status) = dev.request("GET", "/__hmr_gate", &[]);
        if status.contains("/styles/linked.css\":") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the gate never held the edit\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    flush(&dev);

    page.wait_for(&format!("{WIDGET_OUTLINE} === 'rgb(4, 5, 6)'"), WAIT)
        .await;
    let frames = decisions(&mut editor, Duration::from_secs(2)).await;
    assert!(
        frames.iter().any(|f| f["type"] == "update"
            && f["updates"].as_array().is_some_and(|u| u
                .iter()
                .any(|u| u["type"] == "css-update" && u["path"] == "/styles/linked.css"))),
        "the editor heard no css-update for the sheet: {frames:?}\n{}",
        dev.output()
    );
    assert!(
        !frames.iter().any(|f| f["type"] == "full-reload"),
        "the editor heard a reload: {frames:?}\n{}",
        dev.output()
    );
    assert!(
        page.eval::<bool>("window.__mark === 'kept'").await,
        "the page reloaded for a linked sheet's edit\n{}",
        dev.output()
    );
    assert!(
        !dev.logs("oj start: rebuilt"),
        "the bundle was rebuilt for a sheet it only names\n{}",
        dev.output()
    );
    page.close().await;
}

const MIXED: &str = "src/widgets/mixed.jsx";

/// A component beside a helper: no Fast Refresh boundary.
fn mixed(text: &str) -> String {
    format!(
        "export const mixedLabel = (n) => `mixed ${{n}}`;\nexport function Mixed() {{\n  return <i data-testid=\"mixed\">{{\"{text}\"}}</i>;\n}}\n"
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_to_a_component_file_with_another_export_updates_in_place() {
    let Some(app) = app("editor-frames-mixed-exports") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    // The widget, a Fast Refresh boundary, renders the mixed file's component.
    std::fs::write(root.join(MIXED), mixed("mixed:one")).unwrap();
    let widget = std::fs::read_to_string(root.join(WIDGET)).unwrap();
    let rendering = format!(
        "import {{ Mixed }} from \"./mixed.jsx\";\n{}",
        widget.replace("titles}</span>", "titles}<Mixed /></span>")
    );
    assert!(rendering.contains("<Mixed />"), "the widget's span moved");
    std::fs::write(root.join(WIDGET), rendering).unwrap();

    let dev = Server::oj_dev_with(&root, GATE);
    let page = Page::open(&dev.url("/")).await;
    page.wait_for(
        "document.querySelector('[data-testid=\"client-mounted\"]')?.textContent.trim() === 'client-mounted-ok'",
        WAIT,
    )
    .await;
    page.wait_for(&format!("{SHOWN}.includes('mixed:one')"), WAIT)
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    page.eval::<bool>("(window.__mark = 'kept', true)").await;
    let mut editor = editor_socket(&dev).await;

    // The widget came with the page's first bundle: the page has no factory
    // to run it again from, and the server sends it.
    std::fs::write(root.join(MIXED), mixed("mixed:two")).unwrap();
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let (_, status) = dev.request("GET", "/__hmr_gate", &[]);
        if status.contains("/src/widgets/mixed.jsx\":") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the gate never held the edit\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    flush(&dev);
    page.wait_for(&format!("{SHOWN}.includes('mixed:two')"), WAIT)
        .await;
    // Long enough for a reload the page asked for to have landed.
    let frames = decisions(&mut editor, Duration::from_secs(3)).await;
    assert!(
        page.eval::<bool>("window.__mark === 'kept'").await,
        "the page reloaded for an edit its importer can take in place\n{}",
        dev.output()
    );
    assert!(
        !frames.iter().any(|f| f["type"] == "full-reload"),
        "the editor heard a reload: {frames:?}\n{}",
        dev.output()
    );

    // The widget is a module the page can run again now: a second edit of
    // the mixed file re-runs it from what the page holds.
    std::fs::write(root.join(MIXED), mixed("mixed:three")).unwrap();
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let (_, status) = dev.request("GET", "/__hmr_gate", &[]);
        if status.contains("/src/widgets/mixed.jsx\":") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the gate never held the second edit\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    flush(&dev);
    page.wait_for(&format!("{SHOWN}.includes('mixed:three')"), WAIT)
        .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        page.eval::<bool>("window.__mark === 'kept'").await,
        "the page reloaded for the second edit\n{}",
        dev.output()
    );
    // Nothing the server sent again for the page is left held by the gate.
    let (_, status) = dev.request("GET", "/__hmr_gate", &[]);
    assert!(
        !status.contains("glob-widget.jsx"),
        "the gate holds a file no edit wrote: {status}"
    );
    page.close().await;
}
