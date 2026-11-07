// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `server.hmr: false` in vite.config stops edits from broadcasting reload or
//! update frames, while `/__ws` still opens, as in Vite. The control test proves
//! the same edit does deliver a frame with hmr on, so the negative is not
//! vacuous.

use std::time::Duration;

use crate::listen::{close, frame_matching, is_reload, next_text_within};
use crate::{App, INDEX_HTML};

fn app(config: &str) -> App {
    App::new(&[
        (
            "package.json",
            r#"{"name":"hmr-off-app","version":"1.0.0"}"#,
        ),
        ("vite.config.mjs", config),
        ("src/main.js", "document.title = \"v1\";\n"),
        ("index.html", INDEX_HTML),
    ])
}

#[tokio::test(flavor = "multi_thread")]
async fn hmr_false_broadcasts_no_reload_frames() {
    let app = app("export default { server: { hmr: false } };\n");
    let server = app.serve().await;
    let listening = server.listen().await;
    // hmr: false still opens /__ws; it just carries no reload frames.
    let (mut socket, _) = server.ws(listening.addr, "/__ws", None).await;

    // Seed the module into the graph, then edit it.
    server.text("/src/main.js").await;
    app.write(&[("src/main.js", "document.title = \"v2\";\n")]);

    let mut reloads = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(2500);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Some(text) = next_text_within(&mut socket, left).await else {
            break;
        };
        if serde_json::from_str::<serde_json::Value>(&text).is_ok_and(|f| is_reload(&f)) {
            reloads.push(text);
        }
    }
    assert!(
        reloads.is_empty(),
        "expected no reload frames, got {reloads:?}"
    );
    close(socket).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn hmr_default_broadcasts_a_frame_for_the_same_edit() {
    let app = app("export default {};\n");
    let server = app.serve().await;
    let listening = server.listen().await;
    let (mut socket, _) = server.ws(listening.addr, "/__ws", None).await;

    server.text("/src/main.js").await;
    app.write(&[("src/main.js", "document.title = \"v2\";\n")]);
    frame_matching(
        &mut socket,
        "control: an edit with hmr on must broadcast a frame",
        is_reload,
    )
    .await;
    close(socket).await;
}
