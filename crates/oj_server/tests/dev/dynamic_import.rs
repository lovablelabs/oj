// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A template-literal dynamic import (`import(`./pages/${which}.js`)`) loads
//! each matching module in dev: the page's own call resolves one at boot, and
//! a later call from the page resolves another, with no page errors. The
//! build half is oj's `dynamic_import_build`.

use std::time::Duration;

use crate::browser::Page;
use crate::{App, INDEX_HTML};

const WAIT: Duration = Duration::from_secs(20);

const MAIN: &str = "async function load(which) { const m = await import(`./pages/${which}.js`); return m.name; }\n\
window.__LOAD = load;\n\
load(\"home\").then((n) => { window.__HOME = n; });\n";

#[tokio::test(flavor = "multi_thread")]
async fn template_dynamic_import_loads_each_page_in_dev() {
    if crate::browser::skip() {
        return;
    }
    let app = App::new(&[
        ("package.json", r#"{"name":"dynimp-app","version":"1.0.0"}"#),
        ("src/pages/home.js", "export const name = \"home-page\";\n"),
        (
            "src/pages/about.js",
            "export const name = \"about-page\";\n",
        ),
        ("src/main.js", MAIN),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for("window.__HOME !== undefined", WAIT).await;
    let home: String = page.eval("window.__HOME").await;
    let about: String = page.eval(r#"window.__LOAD("about")"#).await;
    assert_eq!(home, "home-page", "boot-time dynamic import");
    assert_eq!(
        about, "about-page",
        "dynamic import called later from the page"
    );
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}
