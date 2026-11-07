// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The build half of the Svelte check: `oj build` compiles a Svelte 5
//! component with the app's own `svelte` package, and in the preview `mount`
//! renders it and a click updates its `$state` rune, with no page errors.

use std::time::Duration;

use super::browser::Page;
use super::{build_app, link_fixtures, preview, scratch, write_files};

const WAIT: Duration = Duration::from_secs(20);

const APP_SVELTE: &str = "<script>\n  let count = $state(0);\n</script>\n<button id=\"btn\" onclick={() => count++}>count: {count}</button>\n";

const MAIN: &str = "import { mount } from \"svelte\";\nimport App from \"./App.svelte\";\n\
mount(App, { target: document.getElementById(\"app\") });\n";

const INDEX: &str = r#"<!doctype html><html><head><title>t</title></head><body><div id="app"></div><script type="module" src="/src/main.js"></script></body></html>"#;

#[tokio::test(flavor = "multi_thread")]
async fn svelte_component_mounts_and_reacts_in_preview() {
    if super::browser::skip() {
        return;
    }
    let root = scratch("svelte");
    if !link_fixtures(&root, &["svelte"]) {
        return;
    }
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"svelte-app","version":"1.0.0"}"#),
            ("src/App.svelte", APP_SVELTE),
            ("src/main.js", MAIN),
            ("index.html", INDEX),
        ],
    );
    build_app(&root).await.expect("the svelte app builds");

    let preview = preview(&root.join("dist"), "/").await;
    let page = Page::open(&preview.url).await;
    page.wait_for(r#"document.getElementById("btn")"#, WAIT)
        .await;
    let text: String = page
        .eval(r#"document.getElementById("btn").textContent"#)
        .await;
    assert_eq!(text, "count: 0", "initial render");
    page.eval::<serde_json::Value>(r#"document.getElementById("btn").click()"#)
        .await;
    let text: String = page
        .eval(r#"document.getElementById("btn").textContent"#)
        .await;
    assert_eq!(text, "count: 1", "reactivity after a click");
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}
