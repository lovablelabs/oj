// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `new URL("./asset", import.meta.url)` is rewritten to a hoisted `?url`
//! asset import (Vite's asset-import-meta-url), so the asset resolves instead
//! of 404ing, and that `?url` module serves the asset path. The build half is
//! oj's `new_url_asset_build`. `oj_compiler`'s
//! `dev_compile_rewrites_new_url_import_meta_url` pins the rewrite on the
//! compiler alone; this pins it through the served module and its `?url`.

use regex::Regex;

use crate::App;

#[tokio::test(flavor = "multi_thread")]
async fn new_url_import_meta_url_becomes_a_served_url_import() {
    let app = App::new(&[
        ("package.json", r#"{"name":"newurl","private":true}"#),
        (
            "index.html",
            r#"<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
        ),
        (
            "src/logo.svg",
            "<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>\n",
        ),
        (
            "src/main.js",
            "export const u = new URL(\"./logo.svg\", import.meta.url).href;\n",
        ),
    ]);
    let server = app.serve().await;

    let module = server.text("/src/main.js").await;
    assert!(
        !Regex::new(r#"new URL\(\s*["']\./logo\.svg["']"#)
            .unwrap()
            .is_match(&module),
        "new URL literal left unrewritten (would 404):\n{module}"
    );
    assert!(
        module.contains("logo.svg?url"),
        "expected a hoisted ?url asset import:\n{module}"
    );

    let spec = Regex::new(r#"from\s*["']([^"']*logo\.svg\?url[^"']*)["']"#)
        .unwrap()
        .captures(&module)
        .unwrap_or_else(|| panic!("hoisted import of logo.svg?url:\n{module}"))[1]
        .to_string();
    let path = if spec.starts_with('/') {
        spec
    } else {
        format!("/src/{spec}")
    };
    let url_mod = server.text(&path).await;
    // vite:asset-import-meta-url names it with fileToUrl, which in dev
    // inlines an svg under assetsInlineLimit.
    assert!(
        url_mod.contains(
            r#"export default "data:image/svg+xml,%3csvg%20xmlns='http://www.w3.org/2000/svg'%3e%3c/svg%3e""#
        ),
        "?url module did not resolve the asset:\n{url_mod}"
    );
}
