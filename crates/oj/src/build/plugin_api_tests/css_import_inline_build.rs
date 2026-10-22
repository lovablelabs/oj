// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The build half of the dev `@import` inlining check: plain imports are
//! inlined (postcss-import parity), url() assets follow
//! build.assetsInlineLimit (small files become data URLs), and
//! output.assetFileNames names the rest and the chunk stylesheet.

use std::fs;

use regex::Regex;

use super::{build_app, files_under, scratch, write_files};

#[tokio::test]
async fn imports_inline_limit_and_asset_file_names_in_build() {
    let root = scratch("css-import-inline");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"cssimp-app","version":"1.0.0"}"#),
            (
                "vite.config.js",
                "export default { build: { assetsInlineLimit: 1024, rollupOptions: { output: { assetFileNames: \"static/[name].[hash][extname]\" } } } };\n",
            ),
            (
                "node_modules/normalize-fake/package.json",
                r#"{"name":"normalize-fake","style":"n.css"}"#,
            ),
            ("node_modules/normalize-fake/n.css", ".norm { margin: 0; }\n"),
            ("src/vars.css", ":root { --x: 1; }\n"),
            (
                "src/base/reset.css",
                "@import \"../vars.css\";\n.reset { background: url(./dot.png); }\n.big { background: url(\"./big.png\"); }\n",
            ),
            (
                "src/app.css",
                "@import \"./base/reset.css\";\n@import 'normalize-fake';\n@import \"./print.css\" print;\n.app { color: red; }\n",
            ),
            ("src/print.css", ".print { display: none; }\n"),
            ("src/main.js", "import \"./app.css\";\nwindow.__OK = true;\n"),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
        ],
    );
    fs::write(root.join("src/base/dot.png"), vec![7u8; 100]).unwrap();
    fs::write(root.join("src/base/big.png"), vec![9u8; 5000]).unwrap();
    build_app(&root).await.expect("the app builds");

    let static_dir = root.join("dist/static");
    assert!(static_dir.is_dir(), "assetFileNames directory used");
    let files: Vec<String> = files_under(&static_dir)
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    let css_file = files
        .iter()
        .find(|f| f.ends_with(".css"))
        .unwrap_or_else(|| panic!("chunk stylesheet named by assetFileNames: {files:?}"));
    assert!(
        Regex::new(r"^main\.[0-9a-f]{8}\.css$")
            .unwrap()
            .is_match(css_file),
        "stylesheet name follows [name].[hash][extname]: {css_file}"
    );
    let built = fs::read_to_string(static_dir.join(css_file)).unwrap();
    assert!(
        built.contains(".norm"),
        "package import inlined in build:\n{built}"
    );
    assert!(
        Regex::new(r"--x:\s*1").unwrap().is_match(&built),
        "nested import inlined in build:\n{built}"
    );
    assert!(
        built.contains("data:image/png;base64,"),
        "100-byte png inlined under assetsInlineLimit:\n{built}"
    );
    let big_re = Regex::new(r"^big\.[0-9a-f]{8}\.png$").unwrap();
    let big = files
        .iter()
        .find(|f| big_re.is_match(f))
        .unwrap_or_else(|| panic!("5000-byte png emitted by assetFileNames pattern: {files:?}"));
    assert!(
        built.contains(&format!("static/{big}")),
        "css references the emitted asset path {big}:\n{built}"
    );
    assert!(
        !Regex::new(r#"@import\s+["']\./"#).unwrap().is_match(&built),
        "relative imports left in build css:\n{built}"
    );
    assert!(
        built.contains("@media print{.print"),
        "media import inlined as @media in build:\n{built}"
    );
}

/// A Tailwind sheet under a PostCSS config has its relative imports inlined
/// before PostCSS in build too (Vite's postcss-import runs first in the
/// chain; Tailwind v3 resolves none), and a Google Fonts import, `;`s in its
/// url, stays whole.
#[tokio::test]
async fn tailwind_sheet_under_postcss_inlines_imports_in_build() {
    let node_modules = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../e2e/fixtures/start-app/node_modules");
    if super::fixture("postcss").is_none() {
        assert!(
            std::env::var_os("OJ_REQUIRE_FIXTURES").is_none(),
            "OJ_REQUIRE_FIXTURES is set but postcss is not installed"
        );
        eprintln!("skipping: fixture postcss not installed");
        return;
    }
    let root = scratch("tailwind-postcss-imports");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"tw-imports","version":"1.0.0"}"#),
            (
                "postcss.config.mjs",
                "export default { plugins: [{ postcssPlugin: \"oj-marker\", Once(root) { root.append(\".from-postcss { color: green; }\"); } }] };\n",
            ),
            ("src/styles/a.css", ".from-a { color: red; }\n"),
            (
                "src/index.css",
                "@import url('https://fonts.test/css2?family=X:wght@400;700&display=swap');\n@import \"./styles/a.css\";\n@tailwind utilities;\n.app { margin: 0; }\n",
            ),
            ("src/main.js", "import \"./index.css\";\n"),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
        ],
    );
    std::os::unix::fs::symlink(
        node_modules.canonicalize().unwrap(),
        root.join("node_modules"),
    )
    .unwrap();
    build_app(&root).await.expect("the app builds");

    let built = files_under(&root.join("dist/assets"))
        .into_iter()
        .filter(|(p, _)| p.ends_with(".css"))
        .map(|(_, c)| c)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        built.contains(".from-a"),
        "relative import inlined in build:\n{built}"
    );
    assert!(built.contains(".from-postcss"), "postcss ran:\n{built}");
    assert!(!built.contains("styles/a.css"), "no import left:\n{built}");
    assert!(
        built.contains("wght@400;700&display=swap"),
        "font import kept whole:\n{built}"
    );
}
