// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A bare import nothing resolves fails the importer like Vite's
//! importAnalysis (`Failed to resolve import ... Does the file exist?`),
//! with or without a plugin host: the host runs for every app, so only a
//! plugin `resolveId` (the one thing that could still claim it) defers it to
//! `/@id/`.

use crate::{App, INDEX_HTML};

#[tokio::test(flavor = "multi_thread")]
async fn a_bare_import_nothing_resolves_fails_like_vite() {
    let app = App::new(&[
        ("package.json", r#"{"name":"bare-miss","version":"1.0.0"}"#),
        (
            "src/main.js",
            "import missing from \"not-installed-pkg\";\nwindow.__m = missing;\n",
        ),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let res = server.get("/src/main.js").await;
    assert_eq!(res.status, 500, "{}", res.body);
    assert!(
        res.body
            .contains("Failed to resolve import \"not-installed-pkg\" from \"src/main.js\""),
        "{}",
        res.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_resolve_id_can_still_claim_a_bare_import() {
    let app = App::new(&[
        ("package.json", r#"{"name":"bare-claim","version":"1.0.0"}"#),
        (
            "oj.plugins.mjs",
            "export default [{ name: \"claim\",\n  resolveId(id) { if (id === \"virtual-bare\") return \"\\0virtual-bare\"; },\n  load(id) { if (id === \"\\0virtual-bare\") return \"export default 42;\"; },\n}];\n",
        ),
        (
            "src/main.js",
            "import v from \"virtual-bare\";\nwindow.__v = v;\n",
        ),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let res = server.get("/src/main.js").await;
    assert_eq!(res.status, 200, "{}", res.body);
    assert!(
        res.body.contains("/@id/"),
        "the plugin's id is deferred to /@id/:\n{}",
        res.body
    );
}
