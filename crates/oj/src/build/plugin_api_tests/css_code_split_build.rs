// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `build.cssCodeSplit` (Vite's default): each chunk gets its own stylesheet.
//! The entry's css, with the css of statically imported modules merged in,
//! is linked render-blocking in index.html; a dynamically imported chunk's
//! css is a separate file the chunk injects itself when it loads, so the
//! page has one more stylesheet link only after the import runs.

use std::fs;
use std::time::Duration;

use super::browser::Page;
use super::{build_app, preview, scratch, write_files};

const WAIT: Duration = Duration::from_secs(20);

#[tokio::test(flavor = "multi_thread")]
async fn dynamic_chunk_css_is_split_and_injected_on_load() {
    let root = scratch("css-code-split");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"css-split-app","version":"1.0.0"}"#),
            ("src/entry.css", "body { background: rgb(1, 2, 3); }\n"),
            ("src/comp.css", ".comp { color: rgb(4, 5, 6); }\n"),
            ("src/lazy.css", ".lazy { color: rgb(7, 8, 9); }\n"),
            (
                "src/comp.js",
                "import \"./comp.css\";\nexport function mountComp() { const d = document.createElement(\"div\"); d.className = \"comp\"; d.textContent = \"comp\"; document.body.appendChild(d); }\n",
            ),
            (
                "src/lazy.js",
                "import \"./lazy.css\";\nexport function mountLazy() { const d = document.createElement(\"div\"); d.className = \"lazy\"; d.textContent = \"lazy\"; document.body.appendChild(d); }\n",
            ),
            (
                "src/main.js",
                "import \"./entry.css\";\nimport { mountComp } from \"./comp.js\";\nmountComp();\nwindow.__loadLazy = () => import(\"./lazy.js\").then((m) => m.mountLazy());\nwindow.__ready = true;\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
        ],
    );
    build_app(&root).await.expect("the app builds");

    let dist = root.join("dist");
    let assets_dir = dist.join("assets");
    let assets: Vec<String> = fs::read_dir(&assets_dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let css_files: Vec<&String> = assets.iter().filter(|f| f.ends_with(".css")).collect();
    assert!(
        css_files.len() >= 2,
        "expected split CSS (>=2 files), got: {css_files:?}"
    );

    // The minifier writes rgb(1, 2, 3) as #010203.
    let read_css = |f: &str| {
        fs::read_to_string(assets_dir.join(f))
            .unwrap()
            .to_lowercase()
    };
    let entry_css: Vec<&&String> = css_files
        .iter()
        .filter(|f| read_css(f).contains("#010203"))
        .collect();
    let lazy_css: Vec<&&String> = css_files
        .iter()
        .filter(|f| read_css(f).contains("#070809"))
        .collect();
    assert_eq!(
        entry_css.len(),
        1,
        "entry style should be in exactly one chunk css: {entry_css:?}"
    );
    assert_eq!(
        lazy_css.len(),
        1,
        "lazy style should be in exactly one chunk css: {lazy_css:?}"
    );
    let (entry_css, lazy_css) = (entry_css[0].as_str(), lazy_css[0].as_str());
    assert_ne!(
        entry_css, lazy_css,
        "lazy CSS must be a separate file from the entry CSS"
    );
    assert!(
        read_css(entry_css).contains("#040506"),
        "statically-imported comp CSS must merge into the entry stylesheet:\n{}",
        read_css(entry_css)
    );

    let html = fs::read_to_string(dist.join("index.html")).unwrap();
    assert!(
        html.contains(entry_css),
        "entry CSS must be linked render-blocking in HTML:\n{html}"
    );
    assert!(
        !html.contains(lazy_css),
        "lazy CSS must NOT be linked in the HTML (loads on demand):\n{html}"
    );

    let lazy_js = assets
        .iter()
        .find(|f| f.starts_with("lazy-") && f.ends_with(".js"))
        .unwrap_or_else(|| panic!("no lazy chunk emitted: {assets:?}"));
    let lazy_code = fs::read_to_string(assets_dir.join(lazy_js)).unwrap();
    assert!(
        lazy_code.contains(r#"createElement("link")"#)
            || lazy_code.contains("createElement('link')"),
        "lazy chunk must self-inject its stylesheet:\n{lazy_code}"
    );

    if super::browser::skip() {
        return;
    }
    let server = preview(&dist, "/").await;
    let page = Page::open(&server.url).await;
    page.wait_for("window.__ready", WAIT).await;
    let bg: String = page
        .eval("getComputedStyle(document.body).backgroundColor")
        .await;
    assert_eq!(bg, "rgb(1, 2, 3)", "entry stylesheet not applied");
    let comp: String = page
        .eval(r#"getComputedStyle(document.querySelector(".comp")).color"#)
        .await;
    assert_eq!(
        comp, "rgb(4, 5, 6)",
        "statically-imported stylesheet not applied"
    );
    let before: u32 = page
        .eval(r#"document.querySelectorAll('link[rel="stylesheet"]').length"#)
        .await;
    // Awaits the promise, as page.evaluate would.
    let _: serde_json::Value = page.eval("window.__loadLazy()").await;
    page.wait_for(r#"document.querySelector(".lazy")"#, WAIT)
        .await;
    let lazy: String = page
        .eval(r#"getComputedStyle(document.querySelector(".lazy")).color"#)
        .await;
    assert_eq!(
        lazy, "rgb(7, 8, 9)",
        "lazy stylesheet did not load with its chunk"
    );
    let after: u32 = page
        .eval(r#"document.querySelectorAll('link[rel="stylesheet"]').length"#)
        .await;
    assert_eq!(
        after,
        before + 1,
        "dynamic import should add exactly one stylesheet link"
    );
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}
