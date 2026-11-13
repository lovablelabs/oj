// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The editor HMR gate: a socket gets `lovable:dev-server-mode` (classic)
//! and `lovable:boot-progress` (numeric module counts) on connect; an edit
//! while gated is held (no update, no reload) and listed by
//! `GET /__hmr_gate`; `POST /__hmr_flush` releases it as a full reload.
//! Turned on with `server.hmrGate` in oj.config.json, the same switch the
//! OJ_HMR_GATE env knob flips (env cannot be set per test).

use std::time::Duration;

use axum::body::Body;
use axum::http::Request;

use crate::browser::Page;
use crate::listen::next_text_within;
use crate::App;

const WAIT: Duration = Duration::from_secs(20);

#[tokio::test(flavor = "multi_thread")]
async fn gated_edit_is_held_until_flush_then_reloads() {
    if crate::browser::skip() {
        return;
    }
    let app = App::new(&[
        ("package.json", r#"{"name":"gate-app","version":"1.0.0"}"#),
        ("oj.config.json", r#"{"server":{"hmrGate":true}}"#),
        (
            "src/main.js",
            "document.title = \"v1\"; window.__READY = true;\n",
        ),
        ("index.html", crate::INDEX_HTML),
    ]);
    let server = app.serve().await;
    let listening = server.listen().await;

    let (mut ws, _) = server.ws(listening.addr, "/__ws", None).await;
    let mut mode = None;
    let mut boot = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    while mode.is_none() || boot.is_none() {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let text = next_text_within(&mut ws, left)
            .await
            .unwrap_or_else(|| panic!("missing on-connect frames (mode={mode:?}, boot={boot:?})"));
        let Ok(frame) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if frame["type"] != "custom" {
            continue;
        }
        match frame["event"].as_str() {
            Some("lovable:dev-server-mode") => mode = Some(frame["data"].clone()),
            Some("lovable:boot-progress") => boot = Some(frame["data"].clone()),
            _ => {}
        }
    }
    let (mode, boot) = (mode.unwrap(), boot.unwrap());
    assert_eq!(
        mode["mode"], "classic",
        "gate announces classic mode on connect"
    );
    assert!(
        boot["ssrModules"].is_number(),
        "boot-progress ssrModules is a number: {boot}"
    );
    assert!(
        boot["clientModules"].is_number(),
        "boot-progress clientModules is a number: {boot}"
    );
    crate::listen::close(ws).await;

    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for("window.__READY === true", WAIT).await;
    page.eval::<bool>(r#"(window.__MARK = "kept", true)"#).await;
    assert_eq!(page.eval::<String>("document.title").await, "v1");

    app.write(&[(
        "src/main.js",
        "document.title = \"v2\"; window.__READY = true;\n",
    )]);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        page.eval::<String>("document.title").await,
        "v1",
        "edit is held: title unchanged before flush"
    );
    assert_eq!(
        page.eval::<Option<String>>("window.__MARK ?? null")
            .await
            .as_deref(),
        Some("kept"),
        "no reload happened before flush"
    );

    let status: serde_json::Value =
        serde_json::from_str(&server.text("/__hmr_gate").await).unwrap();
    assert_eq!(status["enabled"], true, "gate enabled: {status}");
    assert!(
        status["count"].as_u64().is_some_and(|c| c >= 1),
        "gate has pending changes: {status}"
    );

    let res = server
        .request(Request::post("/__hmr_flush").body(Body::empty()).unwrap())
        .await;
    assert_eq!(res.status, 200, "flush:\n{}", res.body);
    let flush: serde_json::Value = serde_json::from_str(&res.body).unwrap();
    assert_eq!(
        flush["mode"], "full-reload",
        "flush reports full-reload mode: {flush}"
    );
    assert!(
        flush["count"].as_u64().is_some_and(|c| c >= 1),
        "flush released changes: {flush}"
    );

    page.wait_for(r#"document.title === "v2""#, WAIT).await;
    assert!(
        page.eval::<bool>("window.__MARK === undefined").await,
        "page reloaded on flush (marker gone)"
    );
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}

// What the gate counts as "no change": a rewrite of the bytes the pages
// already have. The pages only have bytes the server served them, so a
// module fetched while an edit is held (a reload, another tab, a fetch
// racing the flush) keeps a later write of the older bytes a change. These
// drive the gate the way an editor does (write, GET /__hmr_gate, POST
// /__hmr_flush) and stand in for a page with a GET of the module.

const COLOR: &str = "src/color.js";
const GATED: (&str, &str) = ("oj.config.json", r#"{"server":{"hmrGate":true}}"#);

fn color(v: &str) -> String {
    format!("export const color = {v:?};\n")
}

async fn gated_app() -> (App, crate::Server) {
    let app = App::new(&[
        ("package.json", r#"{"name":"gate-noop","version":"1.0.0"}"#),
        GATED,
        (
            "src/main.js",
            "import { color } from \"./color.js\";\nconsole.log(color);\n",
        ),
        (COLOR, &color("c0")),
        ("index.html", crate::INDEX_HTML),
    ]);
    let server = app.serve().await;
    (app, server)
}

async fn pending(server: &crate::Server) -> u64 {
    let status: serde_json::Value =
        serde_json::from_str(&server.text("/__hmr_gate").await).unwrap();
    status["count"].as_u64().unwrap_or(0)
}

/// Waits until the gate holds `n` changes.
async fn wait_pending(server: &crate::Server, n: u64, what: &str) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while pending(server).await != n {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what}: the gate never held {n} change(s)"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn flush(server: &crate::Server) -> u64 {
    let res = server
        .request(Request::post("/__hmr_flush").body(Body::empty()).unwrap())
        .await;
    assert_eq!(res.status, 200, "flush:\n{}", res.body);
    let body: serde_json::Value = serde_json::from_str(&res.body).unwrap();
    body["count"].as_u64().unwrap_or(0)
}

/// A page's fetch of the module: what it now shows.
async fn page_fetch(server: &crate::Server, n: u32) -> String {
    server.text(&format!("/src/color.js?t={n}")).await
}

/// Long enough for the watcher to have delivered a write (its batches settle
/// in 10ms): a gate that dropped the write has dropped it by then.
const SETTLED: Duration = Duration::from_millis(1500);

/// Releases `v` to the pages: written, held, flushed, fetched.
async fn release(app: &App, server: &crate::Server, v: &str, n: u32) {
    app.write(&[(COLOR, &color(v))]);
    wait_pending(server, 1, &format!("releasing {v}")).await;
    assert_eq!(flush(server).await, 1, "the flush released {v}");
    assert!(
        page_fetch(server, n).await.contains(v),
        "the page shows {v}"
    );
    tokio::time::sleep(Duration::from_millis(800)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revert_after_a_page_fetched_the_held_edit_stays_held() {
    let (app, server) = gated_app().await;
    release(&app, &server, "X", 1).await;

    // W is held, but a page fetches it anyway.
    app.write(&[(COLOR, &color("W"))]);
    wait_pending(&server, 1, "the edit to W").await;
    assert!(
        page_fetch(&server, 2).await.contains("\"W\""),
        "the page fetched W"
    );

    // Back to X, the bytes the last flush released: the page showing W
    // still needs this, so it stays held and the flush releases it.
    app.write(&[(COLOR, &color("X"))]);
    tokio::time::sleep(SETTLED).await;
    assert_eq!(
        pending(&server).await,
        1,
        "the revert to X was dropped while a page shows W"
    );
    assert_eq!(flush(&server).await, 1, "the flush released the revert");
    assert!(page_fetch(&server, 3).await.contains("\"X\""));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cache_hit_of_the_held_edit_counts_as_serving_it() {
    let (app, server) = gated_app().await;
    release(&app, &server, "X", 1).await;

    app.write(&[(COLOR, &color("W"))]);
    wait_pending(&server, 1, "the edit to W").await;
    // Twice: the second fetch is the module cache's.
    assert!(page_fetch(&server, 2).await.contains("\"W\""));
    assert!(page_fetch(&server, 3).await.contains("\"W\""));

    app.write(&[(COLOR, &color("X"))]);
    tokio::time::sleep(SETTLED).await;
    assert_eq!(pending(&server).await, 1, "the revert to X was dropped");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rewrite_of_what_every_page_fetched_is_still_dropped() {
    let (app, server) = gated_app().await;
    release(&app, &server, "X", 1).await;
    // Fetched again (a cache hit): still only X was ever served.
    assert!(page_fetch(&server, 2).await.contains("\"X\""));

    app.write(&[(COLOR, &color("X"))]);
    tokio::time::sleep(SETTLED).await;
    assert_eq!(pending(&server).await, 0, "a rewrite of X was held");
    assert_eq!(flush(&server).await, 0, "the flush released a rewrite of X");
}

/// Rewrites the module the way an atomic save does: the bytes go to a
/// temporary file beside it, renamed over the module (a new inode, the
/// same path).
fn rename_over(app: &App, v: &str) {
    let path = app.root.join(COLOR);
    let tmp = path.with_extension("js.tmp");
    std::fs::write(&tmp, color(v)).unwrap();
    std::fs::rename(&tmp, &path).unwrap();
}

/// A rewrite that changed nothing (the bytes the pages have, written again:
/// a new mtime or inode under the same content) is dropped as no change,
/// and a real write a moment later is still held. The watcher loses no
/// event to a per-path throttle, so the second write reaches the gate,
/// which judges it by the bytes it reads at batch time: `gap` past the 10ms
/// quiet window, the write forms its own batch; inside it, both land in one
/// batch and the file already holds the new bytes when it is read. A
/// watcher that silenced a path it had reported moments before would make
/// the no-op swallow the edit, every read stale until a restart.
async fn a_noop_rewrite_then_a_write(gap: Duration, rewrite: fn(&App, &str)) {
    let (app, server) = gated_app().await;
    release(&app, &server, "X", 1).await;

    rewrite(&app, "X");
    tokio::time::sleep(gap).await;
    app.write(&[(COLOR, &color("Y"))]);
    wait_pending(
        &server,
        1,
        &format!(
            "the edit to Y, {}ms after a no-op rewrite of X",
            gap.as_millis()
        ),
    )
    .await;
    assert_eq!(flush(&server).await, 1, "the flush released Y");
    assert!(
        page_fetch(&server, 2).await.contains("\"Y\""),
        "the page shows Y"
    );
}

fn write_in_place(app: &App, v: &str) {
    app.write(&[(COLOR, &color(v))]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_right_after_a_noop_rewrite_is_still_held() {
    a_noop_rewrite_then_a_write(Duration::from_millis(20), write_in_place).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_inside_the_quiet_window_of_a_noop_rewrite_is_still_held() {
    a_noop_rewrite_then_a_write(Duration::from_millis(5), write_in_place).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_right_after_a_noop_rename_is_still_held() {
    a_noop_rewrite_then_a_write(Duration::from_millis(20), rename_over).await;
}

/// The no-op alone, as a rename: nothing is held, neither the module (the
/// bytes the pages have) nor the temporary file that was renamed away.
#[tokio::test(flavor = "multi_thread")]
async fn a_noop_rename_alone_is_dropped() {
    let (app, server) = gated_app().await;
    release(&app, &server, "X", 1).await;

    rename_over(&app, "X");
    tokio::time::sleep(SETTLED).await;
    let status = server.text("/__hmr_gate").await;
    assert_eq!(
        pending(&server).await,
        0,
        "a rename over X with X was held: {status}"
    );
    assert_eq!(
        flush(&server).await,
        0,
        "the flush released a rename over X"
    );
}

/// A known file deleted under the gate is held: its importers need the
/// reload. Gone at batch time like an atomic save's temporary file, but not
/// born in the batch, which is what tells the two apart.
#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_module_is_held() {
    let (app, server) = gated_app().await;
    release(&app, &server, "X", 1).await;

    std::fs::remove_file(app.root.join(COLOR)).unwrap();
    wait_pending(&server, 1, "the deletion of the module").await;
    assert_eq!(flush(&server).await, 1, "the flush released the deletion");
}

/// A file created under the gate and still there at batch time is held: an
/// import that failed to resolve may find it now. Born in the batch like an
/// atomic save's temporary file, but not gone.
#[tokio::test(flavor = "multi_thread")]
async fn a_created_module_is_held() {
    let (app, server) = gated_app().await;
    release(&app, &server, "X", 1).await;

    app.write(&[("src/new.js", "export const fresh = true;\n")]);
    wait_pending(&server, 1, "the new module").await;
    assert_eq!(flush(&server).await, 1, "the flush released the new module");
}

/// An atomic save with new content holds the target alone, not the
/// temporary file renamed away beside it, and the flush releases the new
/// bytes.
#[tokio::test(flavor = "multi_thread")]
async fn an_atomic_save_of_new_content_holds_the_target_alone() {
    let (app, server) = gated_app().await;
    release(&app, &server, "X", 1).await;

    rename_over(&app, "Y");
    tokio::time::sleep(SETTLED).await;
    let status: serde_json::Value =
        serde_json::from_str(&server.text("/__hmr_gate").await).unwrap();
    let held: Vec<&String> = status["pending"].as_object().unwrap().keys().collect();
    // The watcher spells the root as the file system does (`/private/var`
    // for `/var`): the file name is what the paths share.
    assert!(
        held.len() == 1 && held[0].ends_with("/src/color.js"),
        "the save held the target alone: {status}"
    );
    assert_eq!(flush(&server).await, 1, "the flush released the save");
    assert!(
        page_fetch(&server, 2).await.contains("\"Y\""),
        "the page shows Y"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_reverted_before_any_page_fetched_it_drops_its_hold() {
    let (app, server) = gated_app().await;
    release(&app, &server, "X", 1).await;

    app.write(&[(COLOR, &color("W"))]);
    wait_pending(&server, 1, "the edit to W").await;
    // No page fetched W: back to X, the pages already show it.
    app.write(&[(COLOR, &color("X"))]);
    wait_pending(&server, 0, "the revert to X").await;
    assert_eq!(flush(&server).await, 0);
}

/// The editor's flush reaches the gate before its own write (or the write's
/// watcher event): it releases nothing, and the write lands right after.
/// The gate lets it through instead of holding it for a flush already sent.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_right_after_a_flush_that_released_nothing_reaches_the_pages() {
    let (app, server) = gated_app().await;
    release(&app, &server, "X", 1).await;

    assert_eq!(
        flush(&server).await,
        0,
        "nothing held: the flush races its write"
    );
    app.write(&[(COLOR, &color("W"))]);
    tokio::time::sleep(SETTLED).await;
    assert_eq!(
        pending(&server).await,
        0,
        "the write was held for a flush the editor already sent"
    );
    assert!(page_fetch(&server, 2).await.contains("\"W\""));
}

/// What the pages have moves with a write the gate lets through: a later
/// write back to the bytes the last flush released is a change (the pages
/// show W now), held and released, never dropped as "no change".
#[tokio::test(flavor = "multi_thread")]
async fn a_write_let_through_moves_what_the_pages_have() {
    let (app, server) = gated_app().await;
    release(&app, &server, "X", 1).await;

    assert_eq!(flush(&server).await, 0);
    app.write(&[(COLOR, &color("W"))]);
    tokio::time::sleep(SETTLED).await;
    assert_eq!(pending(&server).await, 0, "W went straight to the pages");

    // Past the empty flush's window: the next write is held as always.
    app.write(&[(COLOR, &color("X"))]);
    wait_pending(&server, 1, "the write back to X").await;
    assert_eq!(
        flush(&server).await,
        1,
        "the flush released the write back to X"
    );
    assert!(page_fetch(&server, 2).await.contains("\"X\""));
}

/// A flush that released something opens no window: the editor's next
/// write is held, even soon after it (well inside the window an empty flush
/// would have opened).
#[tokio::test(flavor = "multi_thread")]
async fn a_write_right_after_a_flush_that_released_something_is_held() {
    let (app, server) = gated_app().await;
    app.write(&[(COLOR, &color("X"))]);
    wait_pending(&server, 1, "the edit to X").await;
    assert_eq!(flush(&server).await, 1);
    // Past one tick of the file-time clock: Linux stamps a file's mtime
    // from a coarse clock that can read a few milliseconds behind the
    // flush's own timestamp, and the gate counts a write stamped before a
    // flush as covered by it (released, not held), the safe side.
    tokio::time::sleep(Duration::from_millis(50)).await;
    app.write(&[(COLOR, &color("Y"))]);
    wait_pending(&server, 1, "the next edit, right after a releasing flush").await;
    assert_eq!(flush(&server).await, 1);
}
