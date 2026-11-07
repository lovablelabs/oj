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

/// A Google Fonts import carries `;`s inside its url: it stays for the
/// browser, whole and first, and the relative imports after it are inlined.
/// `tailwind` adds a Tailwind marker under a PostCSS config, the sheet path
/// that hands the app's PostCSS an already-inlined sheet as Vite's
/// postcss-import does; an edit to an inlined file then reaches the sheet.
async fn remote_font_then_relative_imports(tailwind: bool) {
    let marker = if tailwind {
        "@tailwind utilities;\n"
    } else {
        ""
    };
    let index = format!(
        "@import url('https://fonts.test/css2?family=X:wght@400;500;700&display=swap');\n@import \"./styles/a.css\";\n@import \"./styles/b.css\";\n\n{marker}.app {{ margin: 0; }}\n"
    );
    let mut files = vec![
        ("package.json", r#"{"name":"fonts-app","version":"1.0.0"}"#),
        ("src/styles/a.css", ".from-a { color: red; }\n"),
        ("src/styles/b.css", ".from-b { color: blue; }\n"),
        ("src/index.css", index.as_str()),
        (
            "src/main.js",
            "import \"./index.css\";\nwindow.__OK = true;\n",
        ),
        ("index.html", INDEX_HTML),
    ];
    if tailwind {
        files.push((
            "postcss.config.mjs",
            "export default { plugins: [{ postcssPlugin: \"oj-marker\", Once(root) { root.append(\".from-postcss { color: green; }\"); } }] };\n",
        ));
    }
    let app = App::new(&files);
    if tailwind {
        // The whole fixture install: postcss needs its own deps beside it.
        let node_modules = crate::fixture("postcss")
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        std::os::unix::fs::symlink(node_modules, app.root.join("node_modules")).unwrap();
    }
    let server = app.serve().await;
    let css = server.text("/src/index.css?direct").await;

    let fonts = Regex::new(
        r#"^\s*@import (url\()?["']https://fonts\.test/css2\?family=X:wght@400;500;700&display=swap["']\)?;"#,
    )
    .unwrap();
    assert!(
        fonts.is_match(&css),
        "remote import kept whole and first:\n{css}"
    );
    for rule in [".from-a", ".from-b", ".app"] {
        assert!(css.contains(rule), "{rule} served:\n{css}");
    }
    assert!(
        !css.contains("./styles/"),
        "relative imports left unresolved:\n{css}"
    );
    if !tailwind {
        return;
    }
    assert!(css.contains(".from-postcss"), "app's postcss ran:\n{css}");

    let _listening = server.listen().await;
    app.write(&[("src/styles/a.css", ".from-a-edited { color: red; }\n")]);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !server
        .text("/src/index.css?direct")
        .await
        .contains(".from-a-edited")
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "edit to an inlined file never reached the sheet"
        );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn relative_imports_after_a_remote_font_import_are_inlined() {
    remote_font_then_relative_imports(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn tailwind_sheet_inlines_imports_before_postcss() {
    crate::require_fixture!("postcss");
    remote_font_then_relative_imports(true).await;
}
