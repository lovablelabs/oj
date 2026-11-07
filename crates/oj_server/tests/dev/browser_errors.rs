// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The page-error capture the browser tests assert empty must see a throw,
//! or those assertions are vacuous.

use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn uncaught_page_errors_are_captured() {
    if crate::browser::skip() {
        return;
    }
    let app = crate::App::new(&[
        (
            "package.json",
            r#"{"name":"page-errors","version":"1.0.0"}"#,
        ),
        (
            "src/main.js",
            "window.__READY = true;\nconsole.error(\"boom-console\");\nsetTimeout(() => { throw new Error(\"boom-from-page\"); });\n",
        ),
        ("index.html", crate::INDEX_HTML),
    ]);
    let server = app.serve().await;
    let listening = server.listen().await;
    let page =
        crate::browser::Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
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
    let console = page.console();
    assert!(
        console.iter().any(|c| c.contains("boom-console")),
        "console captured: {console:?}"
    );
    page.close().await;
}
