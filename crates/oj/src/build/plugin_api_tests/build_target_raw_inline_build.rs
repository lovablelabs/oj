// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `?raw` and `?inline` imports survive `build.target` lowering: the default
//! (esnext) build inlines the text and a base64 data URI and keeps optional
//! chaining, an `es2015` target lowers `?.`/`??` and still applies both
//! queries, and the lowered code evaluates in a page.

use std::fs;
use std::path::Path;
use std::time::Duration;

use base64::Engine;

use super::browser::Page;
use super::{build_app, preview, scratch, write_files};

const WAIT: Duration = Duration::from_secs(20);

const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

/// A clean build; returns the main chunk.
async fn build_entry(root: &Path) -> String {
    let _ = fs::remove_dir_all(root.join("dist"));
    build_app(root).await.expect("the app builds");
    let assets = root.join("dist/assets");
    let main = fs::read_dir(&assets)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|f| f.starts_with("index-") && f.ends_with(".js"))
        .expect("main chunk emitted");
    fs::read_to_string(assets.join(main)).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn raw_and_inline_survive_target_lowering() {
    let root = scratch("build-target-raw-inline");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"rt-app","version":"1.0.0"}"#),
            ("src/note.txt", "hello-raw-content"),
            (
                "src/main.js",
                "import raw from \"./note.txt?raw\";\nimport inl from \"./dot.png?inline\";\n\
                 const get = (o) => o?.deep?.value ?? \"fallback\";\n\
                 window.__RAW = raw; window.__INL = inl; window.__G = get({});\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
        ],
    );
    fs::write(
        root.join("src/dot.png"),
        base64::engine::general_purpose::STANDARD
            .decode(PNG_1X1)
            .unwrap(),
    )
    .unwrap();

    let code = build_entry(&root).await;
    assert!(
        code.contains("hello-raw-content"),
        "?raw content not inlined into build:\n{code}"
    );
    assert!(
        code.contains("data:image/png;base64,"),
        "?inline not a base64 data uri in build:\n{code}"
    );
    assert!(
        code.contains("?."),
        "esnext build should keep optional chaining:\n{code}"
    );

    write_files(
        &root,
        &[("oj.config.json", r#"{"build":{"target":"es2015"}}"#)],
    );
    let code = build_entry(&root).await;
    assert!(
        code.contains("hello-raw-content"),
        "?raw survives target downleveling:\n{code}"
    );
    assert!(
        code.contains("data:image/png;base64,"),
        "?inline survives target downleveling:\n{code}"
    );
    assert!(
        !code.contains("?."),
        "es2015 target must lower optional chaining:\n{code}"
    );

    if super::browser::skip() {
        return;
    }
    let server = preview(&root.join("dist"), "/").await;
    let page = Page::open(&server.url).await;
    page.wait_for("window.__RAW !== undefined", WAIT).await;
    let raw: String = page.eval("window.__RAW").await;
    let inl: String = page.eval("window.__INL").await;
    let g: String = page.eval("window.__G").await;
    assert_eq!(raw, "hello-raw-content");
    assert!(inl.starts_with("data:image/png;base64,"), "inline: {inl}");
    assert_eq!(
        g, "fallback",
        "downleveled optional-chaining/nullish must still evaluate"
    );
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}
