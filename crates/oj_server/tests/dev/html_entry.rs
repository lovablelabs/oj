// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! An index.html whose module script has a relative `src` with no leading
//! slash (`src="src/index.tsx"`) runs in dev, as Vite accepts it. The build
//! half is `html_entry_build`.

use std::time::Duration;

use crate::browser::Page;
use crate::App;

const WAIT: Duration = Duration::from_secs(20);

const HTML: &str = r#"<!doctype html><html lang="en"><head><title>t</title></head><body><div id="app"></div><script type="module" src="src/index.tsx"></script></body></html>"#;

#[tokio::test(flavor = "multi_thread")]
async fn relative_script_src_without_leading_slash_runs_in_dev() {
    if crate::browser::skip() {
        return;
    }
    let app = App::new(&[
        ("package.json", r#"{"name":"he-app","version":"1.0.0"}"#),
        ("src/index.tsx", "window.__OK = \"relative-entry-works\";\n"),
        ("index.html", HTML),
    ]);
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for("window.__OK", WAIT).await;
    let ok: String = page.eval("window.__OK").await;
    assert_eq!(ok, "relative-entry-works", "entry did not run");
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}
