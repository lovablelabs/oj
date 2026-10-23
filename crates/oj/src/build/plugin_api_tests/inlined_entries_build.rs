// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite's inlined HTML entries: a page's entry chunk that is nothing but
//! side-effect imports (its scripts only pull in CSS and shared chunks) is
//! not emitted; the page loads the chunks it imports with `<script>` tags,
//! post-order, and links its CSS. The modulepreload polyfill is a module the
//! page imports, so pages share it as `modulepreload-polyfill-*.js`.

use std::fs;
use std::path::Path;
use std::time::Duration;

use super::browser::Page;
use super::{build_app, files_under, preview, scratch, write_files};

fn page(script: &str) -> String {
    format!(
        r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="{script}"></script></body></html>"#
    )
}

fn assets(root: &Path) -> Vec<String> {
    files_under(&root.join("dist/assets"))
        .into_iter()
        .map(|(p, _)| p)
        .collect()
}

fn two_css_only_pages(name: &str, config: &str) -> std::path::PathBuf {
    let root = scratch(name);
    let index = page("/src/main.js");
    let admin = page("/src/admin.js");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"inline","version":"1.0.0"}"#),
            ("oj.config.json", config),
            ("index.html", &index),
            ("admin/index.html", &admin),
            ("src/main.js", "import \"./main.css\";\n"),
            ("src/admin.js", "import \"./admin.css\";\n"),
            ("src/main.css", "body { color: rgb(1, 2, 3) }\n"),
            ("src/admin.css", "body { color: rgb(4, 5, 6) }\n"),
        ],
    );
    root
}

const TWO_PAGES: &str =
    r#"{"build":{"rollupOptions":{"input":{"main":"index.html","admin":"admin/index.html"}}}}"#;

#[tokio::test(flavor = "multi_thread")]
async fn css_only_pages_emit_no_js_chunk_and_load_the_shared_polyfill() {
    let root = two_css_only_pages("inline-css-pages", TWO_PAGES);
    build_app(&root).await.expect("the app builds");

    let files = assets(&root);
    let js: Vec<_> = files.iter().filter(|f| f.ends_with(".js")).collect();
    assert!(
        js.len() == 1 && js[0].starts_with("modulepreload-polyfill-"),
        "only the shared polyfill chunk is JS: {files:?}"
    );
    for (page, css) in [("index.html", "main-"), ("admin/index.html", "admin-")] {
        let html = fs::read_to_string(root.join("dist").join(page)).unwrap();
        assert!(
            html.contains(&format!(
                r#"<script type="module" crossorigin src="/assets/{}"></script>"#,
                js[0]
            )),
            "{page} loads the polyfill chunk:\n{html}"
        );
        assert!(
            html.contains(&format!("href=\"/assets/{css}")) && html.contains("rel=\"stylesheet\""),
            "{page} links its stylesheet:\n{html}"
        );
        assert!(
            !html.contains("rel=\"modulepreload\""),
            "an inlined entry preloads nothing:\n{html}"
        );
    }

    if !super::browser::skip() {
        let server = preview(&root.join("dist"), "/").await;
        for (path, color) in [("", "rgb(1, 2, 3)"), ("admin/", "rgb(4, 5, 6)")] {
            let page = Page::open(&format!("{}{path}", server.url)).await;
            page.wait_for(
                &format!("getComputedStyle(document.body).color === {color:?}"),
                Duration::from_secs(20),
            )
            .await;
            assert_eq!(page.errors(), Vec::<String>::new(), "{path} page errors");
            assert!(
                !page.console().iter().any(|m| m.contains("Failed to load")),
                "{path} loaded every script: {:?}",
                page.console()
            );
            page.close().await;
        }
    }
}

/// One page keeps its entry: the polyfill module is inlined into it, so the
/// chunk has code, as Vite's single-page build does.
#[tokio::test]
async fn a_single_css_only_page_keeps_its_entry_with_the_polyfill() {
    let root = scratch("inline-single");
    let index = page("/src/main.js");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"inline","version":"1.0.0"}"#),
            ("index.html", &index),
            ("src/main.js", "import \"./main.css\";\n"),
            ("src/main.css", "body { color: red }\n"),
        ],
    );
    build_app(&root).await.expect("the app builds");
    let files = assets(&root);
    let js: Vec<_> = files.iter().filter(|f| f.ends_with(".js")).collect();
    assert!(js.len() == 1 && js[0].starts_with("index-"), "{files:?}");
    let code = fs::read_to_string(root.join("dist/assets").join(js[0])).unwrap();
    assert!(
        code.contains("modulepreload"),
        "the entry is the polyfill:\n{code}"
    );
    let html = fs::read_to_string(root.join("dist/index.html")).unwrap();
    assert!(html.contains(&format!("/assets/{}", js[0])), "{html}");
}

/// With the polyfill off a CSS-only page imports nothing: no JS at all, and
/// the page keeps only its stylesheet link.
#[tokio::test]
async fn without_the_polyfill_a_css_only_page_ships_no_js() {
    let root = scratch("inline-no-polyfill");
    let index = page("/src/main.js");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"inline","version":"1.0.0"}"#),
            (
                "oj.config.json",
                r#"{"build":{"modulePreload":{"polyfill":false}}}"#,
            ),
            ("index.html", &index),
            ("src/main.js", "import \"./main.css\";\n"),
            ("src/main.css", "body { color: red }\n"),
        ],
    );
    build_app(&root).await.expect("the app builds");
    let files = assets(&root);
    assert!(!files.iter().any(|f| f.ends_with(".js")), "{files:?}");
    let html = fs::read_to_string(root.join("dist/index.html")).unwrap();
    assert!(!html.contains("<script"), "no script left:\n{html}");
    assert!(html.contains("rel=\"stylesheet\""), "{html}");
}

/// Pages whose scripts only import one shared module load the chunk it
/// lands in directly: rolldown groups the polyfill and the module, imported
/// by the same pages, into one `shared-*.js`, as Vite's build does.
#[tokio::test]
async fn import_only_pages_load_their_shared_chunk_directly() {
    let root = scratch("inline-shared");
    let index = page("/src/main.js");
    let admin = page("/src/admin.js");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"inline","version":"1.0.0"}"#),
            ("oj.config.json", TWO_PAGES),
            ("index.html", &index),
            ("admin/index.html", &admin),
            ("src/main.js", "import \"./shared.js\";\n"),
            ("src/admin.js", "import \"./shared.js\";\n"),
            (
                "src/shared.js",
                "window.__SHARED = (window.__SHARED || 0) + 1;\n",
            ),
        ],
    );
    build_app(&root).await.expect("the app builds");
    let files = assets(&root);
    let js: Vec<_> = files.iter().filter(|f| f.ends_with(".js")).collect();
    assert!(
        js.len() == 1 && js[0].starts_with("shared-"),
        "one shared chunk, no page entries: {files:?}"
    );
    let code = fs::read_to_string(root.join("dist/assets").join(js[0])).unwrap();
    assert!(
        code.contains("__SHARED") && code.contains("modulepreload"),
        "{code}"
    );
    for page in ["index.html", "admin/index.html"] {
        let html = fs::read_to_string(root.join("dist").join(page)).unwrap();
        assert!(
            html.contains(&format!(
                r#"<script type="module" crossorigin src="/assets/{}"></script>"#,
                js[0]
            )),
            "{page} loads the shared chunk:\n{html}"
        );
    }
}
