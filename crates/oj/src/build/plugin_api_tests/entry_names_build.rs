// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! An HTML page is Vite's rollup input, so its entry chunk and that chunk's
//! stylesheet take the input's name, never the page script's file name:
//! `index` for the default index.html, each key of an object `input`.

use std::fs;

use super::{build_app, files_under, scratch, write_files};

fn page(script: &str) -> String {
    format!(
        r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="{script}"></script></body></html>"#
    )
}

/// The emitted file names under `dist/assets`.
fn assets(root: &std::path::Path) -> Vec<String> {
    files_under(&root.join("dist/assets"))
        .into_iter()
        .map(|(p, _)| p)
        .collect()
}

#[tokio::test]
async fn the_default_page_names_its_chunk_and_stylesheet_index() {
    let root = scratch("entry-names-default");
    let main = page("/src/main.ts");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"names","version":"1.0.0"}"#),
            ("index.html", &main),
            (
                "src/main.ts",
                "import \"./app.css\";\nconsole.log(\"main\");\n",
            ),
            ("src/app.css", ".a { color: red }\n"),
        ],
    );
    build_app(&root).await.expect("the app builds");
    let files = assets(&root);
    let js: Vec<_> = files.iter().filter(|f| f.ends_with(".js")).collect();
    let css: Vec<_> = files.iter().filter(|f| f.ends_with(".css")).collect();
    assert!(
        js.len() == 1 && js[0].starts_with("index-"),
        "entry chunk: {files:?}"
    );
    assert!(
        css.len() == 1 && css[0].starts_with("index-"),
        "stylesheet: {files:?}"
    );
    let html = fs::read_to_string(root.join("dist/index.html")).unwrap();
    assert!(html.contains(&format!("/assets/{}", js[0])), "{html}");
    assert!(html.contains(&format!("/assets/{}", css[0])), "{html}");
}

#[tokio::test]
async fn object_input_keys_name_each_pages_chunk_and_stylesheet() {
    let root = scratch("entry-names-keys");
    let index = page("/src/main.ts");
    let admin = page("/src/admin.ts");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"names","version":"1.0.0"}"#),
            (
                "oj.config.json",
                r#"{"build":{"rollupOptions":{"input":{"app":"index.html","dashboard":"admin/index.html"}}}}"#,
            ),
            ("index.html", &index),
            ("admin/index.html", &admin),
            (
                "src/main.ts",
                "import \"./app.css\";\nconsole.log(\"main\");\n",
            ),
            (
                "src/admin.ts",
                "import \"./admin.css\";\nconsole.log(\"admin\");\n",
            ),
            ("src/app.css", ".a { color: red }\n"),
            ("src/admin.css", ".b { color: blue }\n"),
        ],
    );
    build_app(&root).await.expect("the app builds");
    let files = assets(&root);
    for key in ["app", "dashboard"] {
        for ext in [".js", ".css"] {
            assert!(
                files
                    .iter()
                    .any(|f| f.starts_with(&format!("{key}-")) && f.ends_with(ext)),
                "{key}-*{ext}: {files:?}"
            );
        }
    }
    assert!(
        !files
            .iter()
            .any(|f| f.starts_with("main-") || f.starts_with("admin-")),
        "no chunk named after its script: {files:?}"
    );
}
