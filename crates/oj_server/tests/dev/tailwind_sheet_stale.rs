// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A served Tailwind sheet picks up a class added in a source file whether or
//! not an update goes out: with `server.hmr: false`, and after the gate's
//! full-reload flush (which sends no css-update). The default-config control
//! proves the edit itself reaches the sheet.

use std::time::Duration;

use axum::body::Body;
use axum::http::Request;

use crate::{require_fixture, App};

const SHEET: &str = "/src/index.css?direct";

fn component(class: &str) -> String {
    format!("export const A = () => <h1 className=\"{class}\">hi</h1>;\n")
}

async fn sheet_gains_new_class(config: (&str, &str), flush: bool) {
    require_fixture!("tailwindcss", "@tailwindcss");
    let app = App::new(&[
        ("package.json", r#"{"name":"tw-stale","version":"1.0.0"}"#),
        config,
        ("src/index.css", "@import \"tailwindcss\";\n"),
        ("src/App.tsx", &component("underline")),
        ("index.html", crate::INDEX_HTML),
    ]);
    app.link_fixture("tailwindcss");
    app.link_fixture("@tailwindcss");
    let server = app.serve().await;
    let _listening = server.listen().await;

    let first = server.text(SHEET).await;
    assert!(first.contains(".underline"), "seeded sheet: {first}");
    assert!(!first.contains(".italic"), "seeded sheet has no italic");

    app.write(&[("src/App.tsx", &component("underline italic"))]);
    if flush {
        // The edit must be held before the flush can release it.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let res = server
            .request(Request::post("/__hmr_flush").body(Body::empty()).unwrap())
            .await;
        assert_eq!(res.status, 200, "flush:\n{}", res.body);
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !server.text(SHEET).await.contains(".italic") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "sheet stayed stale after the edit"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn hmr_off_sheet_picks_up_an_edit() {
    sheet_gains_new_class(
        (
            "vite.config.mjs",
            "export default { server: { hmr: false } };\n",
        ),
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn gated_flush_sheet_picks_up_an_edit() {
    sheet_gains_new_class(("oj.config.json", r#"{"server":{"hmrGate":true}}"#), true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn default_sheet_picks_up_an_edit() {
    sheet_gains_new_class(("vite.config.mjs", "export default {};\n"), false).await;
}

/// A source the sheet scans from outside the root (`@source` into a sibling
/// package, relative to the sheet as Tailwind resolves it) is styled, and an
/// edit there refreshes the cached sheet: its deps reach the watcher, as Vite
/// watches a sheet's deps. Through the app's PostCSS config, and through oj's
/// own Tailwind v4 compile when there is none.
async fn outside_root_source_refreshes_the_sheet(postcss: bool) {
    require_fixture!("tailwindcss", "@tailwindcss", "postcss");
    let app = App::new(&[
        ("package.json", r#"{"name":"tw-outside","version":"1.0.0"}"#),
        ("index.html", crate::INDEX_HTML),
    ]);
    if postcss {
        // @tailwindcss/postcss scans `process.cwd()` unless given a `base`,
        // and the test process's cwd is whichever app root an in-process
        // plugin host last chdir'd into, or this crate's own sources (whose
        // tests spell `italic`). Pin it to the app, as running from its root
        // does.
        let config = format!(
            "export default {{ plugins: {{ \"@tailwindcss/postcss\": {{ base: {:?} }} }} }};\n",
            app.root.display().to_string()
        );
        app.write(&[("postcss.config.mjs", &config)]);
    }
    let name = format!("{}-ui", app.root.file_name().unwrap().to_string_lossy());
    let ui = app.root.with_file_name(&name);
    std::fs::create_dir_all(ui.join("src")).unwrap();
    std::fs::write(ui.join("src/Button.tsx"), component("underline")).unwrap();
    app.write(&[(
        "src/index.css",
        &format!("@import \"tailwindcss\";\n@source \"../../{name}/src\";\n"),
    )]);
    for pkg in ["tailwindcss", "@tailwindcss", "postcss"] {
        app.link_fixture(pkg);
    }
    let server = app.serve().await;
    let _listening = server.listen().await;

    let first = server.text(SHEET).await;
    assert!(first.contains(".underline"), "seeded sheet: {first}");
    assert!(!first.contains(".italic"), "seeded sheet has no italic");

    // A watch added on the fly starts delivering a moment later: rewrite until
    // an event lands, never a fixed sleep. On macOS FSEvents restarts its
    // stream, which under the whole suite's churn has taken close to a minute.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    for n in 0.. {
        let class = format!("underline italic p-{n}");
        std::fs::write(ui.join("src/Button.tsx"), component(&class)).unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        if server.text(SHEET).await.contains(".italic") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "sheet stayed stale after an outside-root edit"
        );
    }
    let _ = std::fs::remove_dir_all(&ui);
}

#[tokio::test(flavor = "multi_thread")]
async fn outside_root_source_refreshes_the_postcss_sheet() {
    outside_root_source_refreshes_the_sheet(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn outside_root_source_refreshes_the_v4_sheet() {
    outside_root_source_refreshes_the_sheet(false).await;
}
