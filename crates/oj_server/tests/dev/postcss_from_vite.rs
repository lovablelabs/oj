// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A PostCSS config runs on Vite's own postcss, as Vite's css.ts does
//! (`import('postcss')` from vite, its dependency), when the app cannot
//! resolve postcss itself: a strict (pnpm) layout keeps vite's dependencies
//! out of the app root. Before, oj skipped the config silently. The build
//! half is oj's `postcss_runs_on_vites_own_postcss_in_build`.

use crate::{require_fixture, App, INDEX_HTML};

#[tokio::test(flavor = "multi_thread")]
async fn a_postcss_config_runs_on_vites_postcss() {
    require_fixture!("vite", "postcss");
    let app = App::new(&[
        ("package.json", r#"{"name":"postcss-from-vite","version":"1.0.0"}"#),
        (
            "postcss.config.mjs",
            "export default { plugins: [{ postcssPlugin: \"oj-marker\", Once(root) { root.append(\".from-postcss { color: green; }\"); } }] };\n",
        ),
        ("src/app.css", ".app { color: red; }\n"),
        ("src/main.js", "import \"./app.css\";\n"),
        ("index.html", INDEX_HTML),
    ]);
    // Only vite in the app's node_modules: postcss resolves from vite's own
    // install, not from the app root.
    app.link_fixture("vite");
    let server = app.serve().await;
    let css = server.text("/src/app.css?direct").await;
    assert!(css.contains(".app"), "{css}");
    assert!(
        css.contains(".from-postcss"),
        "the config ran on vite's postcss:\n{css}"
    );
}
