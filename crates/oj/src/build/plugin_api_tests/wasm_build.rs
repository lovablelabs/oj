// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The build half of the `?init` wasm check: `oj build` emits the module as a
//! `.wasm` asset, and in the preview the init function instantiates it so its
//! exports run in a real browser with no page errors.

use std::time::Duration;

use super::browser::Page;
use super::{build_app, preview, scratch, write_files};

const WAIT: Duration = Duration::from_secs(20);

/// `(module (func (export "add") (param i32 i32) (result i32) local.get 0 local.get 1 i32.add))`
const ADD_WASM: &[u8] = &[
    0, 0x61, 0x73, 0x6d, 1, 0, 0, 0, 1, 7, 1, 0x60, 2, 0x7f, 0x7f, 1, 0x7f, 3, 2, 1, 0, 7, 7, 1, 3,
    0x61, 0x64, 0x64, 0, 0, 10, 9, 1, 7, 0, 0x20, 0, 0x20, 1, 0x6a, 0x0b,
];

const MAIN: &str = "import init from \"./add.wasm?init\";\n\
init().then((instance) => { window.__SUM = instance.exports.add(2, 3); });\n";

#[tokio::test(flavor = "multi_thread")]
async fn wasm_init_import_emits_asset_and_instantiates_in_preview() {
    let root = scratch("wasm-init");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"wasm-app","version":"1.0.0"}"#),
            ("src/main.js", MAIN),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
        ],
    );
    // write_files takes text; the module is binary.
    std::fs::write(root.join("src/add.wasm"), ADD_WASM).unwrap();
    build_app(&root).await.expect("the wasm app builds");

    let assets: Vec<String> = std::fs::read_dir(root.join("dist/assets"))
        .expect("dist/assets exists")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        assets.iter().any(|f| f.ends_with(".wasm")),
        "prod build did not emit the wasm asset: {assets:?}"
    );

    if super::browser::skip() {
        return;
    }
    let preview = preview(&root.join("dist"), "/").await;
    let page = Page::open(&preview.url).await;
    page.wait_for("window.__SUM !== undefined", WAIT).await;
    let sum: serde_json::Value = page.eval("window.__SUM").await;
    assert_eq!(sum, serde_json::json!(5), "add(2, 3) through ?init");
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}
