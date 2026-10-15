// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The build half of the Sass preprocessorOptions check: `additionalData`,
//! `loadPaths`, and node_modules packages through their `sass` entry (also
//! `~`-prefixed) compile into the emitted stylesheet like Vite's sass importer.

use regex::Regex;

use super::{build_app, files_under, scratch, write_files};

#[tokio::test]
async fn sass_preprocessor_options_and_package_entries_in_build() {
    let root = scratch("css-preprocessor-options");
    write_files(
        &root,
        &[
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
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
        ],
    );
    build_app(&root).await.expect("the sass app builds");

    // Top-level css files of dist/assets.
    let css = files_under(&root.join("dist/assets"))
        .into_iter()
        .filter(|(p, _)| p.ends_with(".css") && !p.contains('/'))
        .map(|(_, c)| c)
        .collect::<Vec<_>>()
        .join("\n");
    let has = |re: &str| Regex::new(re).unwrap().is_match(&css);
    assert!(
        has(r"color:\s*(#f00|red)"),
        "build: additionalData variable:\n{css}"
    );
    assert!(css.contains("#abcdef"), "build: loadPaths @use:\n{css}");
    assert!(
        has(r"margin:\s*7px"),
        "build: package.json sass entry:\n{css}"
    );
    assert!(has(r"padding:\s*9px"), "build: ~pkg/path import:\n{css}");
    assert!(
        !has(r"\$brand|\$accent|@use"),
        "build: sass left in output:\n{css}"
    );
}
