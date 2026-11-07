// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `.pcss` and `.postcss` are stylesheets in dev, as Vite's CSS_LANGS_RE has
//! them: served as CSS, and a `.module.pcss` is a CSS module. (`.sss` needs
//! the app's `sugarss`; e2e/css-postcss-langs.mjs installs it.)

use crate::{App, INDEX_HTML};

#[tokio::test(flavor = "multi_thread")]
async fn postcss_extensions_are_served_as_stylesheets() {
    let app = App::new(&[
        ("package.json", r#"{"name":"css-langs","version":"1.0.0"}"#),
        ("src/a.pcss", ".p { color: pink; }\n"),
        ("src/b.postcss", ".q { color: peru; }\n"),
        ("src/c.module.pcss", ".card { color: red; }\n"),
        (
            "src/main.js",
            "import \"./a.pcss\";\nimport \"./b.postcss\";\nimport c from \"./c.module.pcss\";\nwindow.__C = c;\n",
        ),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;

    let a = server.text("/src/a.pcss?direct").await;
    assert!(a.contains(".p") && a.contains("pink"), "{a}");
    let b = server.text("/src/b.postcss?direct").await;
    assert!(b.contains(".q") && b.contains("peru"), "{b}");
    // As a module import, the sheet's JS wrapper (Vite's css-post in dev).
    let wrapper = server.text("/src/a.pcss").await;
    assert!(wrapper.contains("updateStyle"), "{wrapper}");
    let module = server.text("/src/c.module.pcss").await;
    assert!(
        module.contains("\"card\":\"_card_"),
        "a .module.pcss is a CSS module:\n{module}"
    );
}
