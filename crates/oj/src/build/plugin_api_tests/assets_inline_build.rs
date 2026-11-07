// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `build.assetsInlineLimit` (Vite's 4096 default): assets under the limit
//! become data URIs (svg as a url-encoded `data:image/svg+xml,`, binaries as
//! base64 with their own mime type, bmp included), larger ones are emitted
//! under `assets/`, and a limit of 0 emits every asset. A CRLF svg (a Windows
//! checkout) inlines with no line terminator left in the URI, which would not
//! parse as a string literal.

use std::fs;
use std::path::Path;
use std::time::Duration;

use base64::Engine;
use regex::Regex;

use super::browser::Page;
use super::{build_app, preview, scratch, write_files};

const WAIT: Duration = Duration::from_secs(20);

const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
const BMP_1X1: &str =
    "Qk06AAAAAAAAADYAAAAoAAAAAQAAAAEAAAABABgAAAAAAAQAAAAAAAAAAAAAAAAAAAAAAAAAAAD/AA==";

fn write_base64(path: &Path, b64: &str) {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .unwrap();
    fs::write(path, bytes).unwrap();
}

/// A clean build, as each run of `oj build` against a fresh dist.
async fn build_clean(root: &Path) -> Vec<String> {
    let _ = fs::remove_dir_all(root.join("dist"));
    build_app(root).await.expect("the app builds");
    fs::read_dir(root.join("dist/assets"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn assets_under_the_limit_inline_and_limit_zero_emits_all() {
    let root = scratch("assets-inline");
    let big = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg">{}</svg>"#,
        "<rect/>".repeat(900)
    );
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"inline-app","version":"1.0.0"}"#),
            ("src/small.svg", r#"<svg xmlns="http://www.w3.org/2000/svg"><rect/></svg>"#),
            ("src/big.svg", big.as_str()),
            (
                "src/crlf.svg",
                "<svg xmlns=\"http://www.w3.org/2000/svg\">\r\n  <rect width=\"1\" height=\"1\"/>\r\n</svg>\r\n",
            ),
            (
                "src/main.js",
                "import small from \"./small.svg\";\nimport big from \"./big.svg\";\nimport png from \"./dot.png\";\n\
                 import crlf from \"./crlf.svg\";\nimport bmp from \"./dot.bmp\";\n\
                 window.__S = small; window.__B = big; window.__PNG = png; window.__CRLF = crlf; window.__BMP = bmp;\n\
                 window.__READY = true;\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
        ],
    );
    write_base64(&root.join("src/dot.png"), PNG_1X1);
    write_base64(&root.join("src/dot.bmp"), BMP_1X1);

    let files = build_clean(&root).await;
    assert!(
        files
            .iter()
            .any(|f| f.starts_with("big-") && f.ends_with(".svg")),
        "big.svg emitted (over limit): {files:?}"
    );
    assert!(
        !files
            .iter()
            .any(|f| f.starts_with("small-") && f.ends_with(".svg")),
        "small.svg inlined (under limit): {files:?}"
    );
    assert!(
        !files.iter().any(|f| f.ends_with(".png")),
        "png inlined (under limit): {files:?}"
    );

    if !super::browser::skip() {
        let server = preview(&root.join("dist"), "/").await;
        let page = Page::open(&server.url).await;
        page.wait_for("window.__READY", WAIT).await;
        let s: String = page.eval("window.__S").await;
        let b: String = page.eval("window.__B").await;
        let png: String = page.eval("window.__PNG").await;
        let crlf: String = page.eval("window.__CRLF").await;
        let bmp: String = page.eval("window.__BMP").await;
        assert!(
            s.starts_with("data:image/svg+xml,"),
            "small svg is an inline data uri: {s}"
        );
        assert!(
            png.starts_with("data:image/png;base64,"),
            "png is an inline base64 data uri: {png}"
        );
        assert!(
            Regex::new(r"/assets/big-.*\.svg$").unwrap().is_match(&b),
            "big svg is an emitted file url: {b}"
        );
        assert!(
            crlf.starts_with("data:image/svg+xml,"),
            "a CRLF svg inlines too: {crlf}"
        );
        assert!(
            !crlf.contains('\r') && !crlf.contains('\n'),
            "no line terminator survives in the data uri: {crlf:?}"
        );
        assert!(
            crlf.contains("%3crect"),
            "the CRLF svg kept its content: {crlf}"
        );
        assert!(
            bmp.starts_with("data:image/bmp;base64,"),
            "bmp inlines with its own mime type: {bmp}"
        );
        assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
        page.close().await;
    }

    write_files(
        &root,
        &[("oj.config.json", r#"{"build":{"assetsInlineLimit":0}}"#)],
    );
    let files = build_clean(&root).await;
    assert!(
        files.iter().any(|f| f.starts_with("small-")) && files.iter().any(|f| f.ends_with(".png")),
        "limit 0 emits every asset: {files:?}"
    );
    assert!(
        files.iter().any(|f| f.ends_with(".bmp")),
        "limit 0 emits the bmp too: {files:?}"
    );
}
