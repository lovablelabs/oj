// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! css.preprocessorOptions for Sass in dev: `additionalData` is prepended,
//! `loadPaths` resolve a bare `@use`, and a node_modules package resolves
//! through its package.json `sass` entry (also with the `~` prefix), as
//! Vite's sass importer does. The build half is oj's
//! `css_preprocessor_options_build`.

use regex::Regex;

use crate::{App, INDEX_HTML};

fn expect_compiled(css: &str, label: &str) {
    let has = |re: &str| Regex::new(re).unwrap().is_match(css);
    assert!(
        has(r"color:\s*(#f00|red)"),
        "{label}: additionalData variable:\n{css}"
    );
    assert!(css.contains("#abcdef"), "{label}: loadPaths @use:\n{css}");
    assert!(
        has(r"margin:\s*7px"),
        "{label}: package.json sass entry:\n{css}"
    );
    assert!(has(r"padding:\s*9px"), "{label}: ~pkg/path import:\n{css}");
    assert!(
        !has(r"\$brand|\$accent|@use"),
        "{label}: sass left in output:\n{css}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sass_preprocessor_options_and_package_entries_in_dev() {
    let app = App::new(&[
        ("package.json", r#"{"name":"sassopts-app","version":"1.0.0"}"#),
        (
            "vite.config.js",
            "export default { css: { preprocessorOptions: { scss: { additionalData: \"$brand: #f00;\", loadPaths: [\"styles\"] } } } };\n",
        ),
        ("styles/_theme.scss", "$accent: #abcdef;\n"),
        (
            "node_modules/@acme/tokens/package.json",
            r#"{"name":"@acme/tokens","version":"1.0.0","main":"index.js","sass":"src/index.scss"}"#,
        ),
        ("node_modules/@acme/tokens/src/index.scss", "$space: 7px;\n"),
        ("node_modules/@acme/tokens/src/_mixins.scss", "@mixin pad { padding: 9px; }\n"),
        (
            "src/a.scss",
            "@use \"theme\";\n@use \"@acme/tokens\" as t;\n@use \"~@acme/tokens/src/mixins\";\n.a { color: $brand; border-color: theme.$accent; margin: t.$space; @include mixins.pad; }\n",
        ),
        ("src/main.js", "import \"./a.scss\";\nwindow.__OK = true;\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let direct = server.text("/src/a.scss?direct").await;
    expect_compiled(&direct, "dev");
}
