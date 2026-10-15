// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A Svelte 5 component compiled with the app's own `svelte` package in dev:
//! `mount` renders it, a click updates its `$state` rune, and an edit to the
//! `.svelte` file hot-swaps the component without reloading the page. The
//! build half is oj's `svelte_build`.

use std::time::Duration;

use crate::browser::Page;
use crate::App;

const WAIT: Duration = Duration::from_secs(20);

fn app_svelte(label: &str) -> String {
    format!(
        "<script>\n  let count = $state(0);\n</script>\n<button id=\"btn\" onclick={{() => count++}}>{label}: {{count}}</button>\n"
    )
}

const MAIN: &str = "import { mount } from \"svelte\";\nimport App from \"./App.svelte\";\n\
mount(App, { target: document.getElementById(\"app\") });\n";

const INDEX: &str = r#"<!doctype html><html><head><title>t</title></head><body><div id="app"></div><script type="module" src="/src/main.js"></script></body></html>"#;

#[tokio::test(flavor = "multi_thread")]
async fn svelte_component_mounts_reacts_and_hot_swaps_in_dev() {
    if crate::browser::skip() {
        return;
    }
    crate::require_fixture!("svelte");
    let app = App::new(&[
        ("package.json", r#"{"name":"svelte-app","version":"1.0.0"}"#),
        ("src/App.svelte", &app_svelte("count")),
        ("src/main.js", MAIN),
        ("index.html", INDEX),
    ]);
    app.link_fixture("svelte");
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;

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

    page.eval::<String>(r#"window.__SURVIVE = "yes""#).await;
    app.write(&[("src/App.svelte", &app_svelte("clicks"))]);
    page.wait_for(
        r#"document.getElementById("btn")?.textContent?.startsWith("clicks:")"#,
        WAIT,
    )
    .await;
    let survived: Option<String> = page.eval("window.__SURVIVE").await;
    assert_eq!(
        survived.as_deref(),
        Some("yes"),
        "HMR did a full reload instead of a hot swap"
    );
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}
