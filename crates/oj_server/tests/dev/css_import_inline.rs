// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Plain `@import`s are inlined in dev like Vite's postcss-import step:
//! relative chains, a bare node_modules package through its `style` entry,
//! url() rebased to the server root, and a media-query import kept as an
//! `@media` block. The build half lives in oj's plugin_api_tests
//! (`css_import_inline_build`).

use regex::Regex;

use crate::{App, INDEX_HTML};

/// The app; the two pngs are binary, so they go in after `App::new`.
fn app() -> App {
    let app = App::new(&[
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
        ("index.html", INDEX_HTML),
    ]);
    std::fs::write(app.root.join("src/base/dot.png"), vec![7u8; 100]).unwrap();
    std::fs::write(app.root.join("src/base/big.png"), vec![9u8; 5000]).unwrap();
    app
}

#[tokio::test(flavor = "multi_thread")]
async fn plain_imports_are_inlined_with_urls_rebased() {
    let app = app();
    let server = app.serve().await;
    let css = server.text("/src/app.css?direct").await;

    assert!(
        Regex::new(r"--x:\s*1").unwrap().is_match(&css),
        "nested relative import inlined:\n{css}"
    );
    assert!(css.contains(".reset"), "relative import inlined:\n{css}");
    assert!(
        css.contains(".norm"),
        "bare package import inlined via style entry:\n{css}"
    );
    assert!(
        Regex::new(r#"url\(["']?/src/base/dot\.png"#)
            .unwrap()
            .is_match(&css),
        "inlined file's url() rebased to the server root:\n{css}"
    );
    assert!(
        Regex::new(r"@media print\s*\{[^}]*\.print")
            .unwrap()
            .is_match(&css),
        "media-query import inlined as @media:\n{css}"
    );
    assert!(
        !Regex::new(r#"@import\s+["']\./base|@import\s+['"]normalize-fake"#)
            .unwrap()
            .is_match(&css),
        "inlined imports still present:\n{css}"
    );
}
