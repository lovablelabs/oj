// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A JS import that starts with `/` is a file under the root (`/src/x.js`) or,
//! failing that, a public-dir file (`/vite.svg`, as create-vite's template
//! imports its logo). The public file's module exports its URL under `base`,
//! and the file itself is never hashed into assets/.

use super::{build_app, files_under, js_under, scratch, write_files};

fn app(name: &str, base: &str) -> std::path::PathBuf {
    let root = scratch(name);
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"rootabs","private":true}"#),
            ("oj.config.json", &format!(r#"{{"base":"{base}"}}"#)),
            (
                "index.html",
                r#"<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
            ("public/vite.svg", "<svg xmlns=\"http://www.w3.org/2000/svg\"/>\n"),
            (
                "src/main.js",
                "import logo from '/vite.svg';\nimport { x } from '/src/x.js';\nconsole.log(logo, x);\n",
            ),
            ("src/x.js", "export const x = 'from-src-x';\n"),
        ],
    );
    root
}

#[tokio::test]
async fn root_absolute_and_public_imports_resolve_under_an_absolute_base() {
    let root = app("root-abs-imports", "/app/");
    build_app(&root).await.expect("the app builds");
    let js = js_under(&root.join("dist/assets"));
    assert!(
        js.contains("from-src-x"),
        "/src/x.js was not bundled:\n{js}"
    );
    assert!(
        js.contains("\"/app/vite.svg\"") || js.contains("`/app/vite.svg`"),
        "public URL not under the base:\n{js}"
    );
    assert!(
        root.join("dist/vite.svg").is_file(),
        "public file not copied"
    );
    let hashed = files_under(&root.join("dist/assets"))
        .into_iter()
        .any(|(p, _)| p.ends_with(".svg"));
    assert!(!hashed, "public file was emitted as a hashed asset");
}

#[tokio::test]
async fn public_import_resolves_against_the_chunk_under_a_relative_base() {
    let root = app("root-abs-imports-relative", "./");
    build_app(&root).await.expect("the app builds");
    let js = js_under(&root.join("dist/assets"));
    assert!(
        js.contains("../vite.svg"),
        "public URL not relative to the chunk:\n{js}"
    );
    assert!(
        root.join("dist/vite.svg").is_file(),
        "public file not written at its own path"
    );
}
