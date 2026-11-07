// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `import init from "./x.wasm?init"` in dev: the default export is an init
//! function whose promise resolves to the instantiated module, so its exports
//! run in the page. The build half is oj's `wasm_build`.

use std::time::Duration;

use crate::browser::Page;
use crate::{App, INDEX_HTML};

const WAIT: Duration = Duration::from_secs(20);

/// `(module (func (export "add") (param i32 i32) (result i32) local.get 0 local.get 1 i32.add))`
const ADD_WASM: &[u8] = &[
    0, 0x61, 0x73, 0x6d, 1, 0, 0, 0, 1, 7, 1, 0x60, 2, 0x7f, 0x7f, 1, 0x7f, 3, 2, 1, 0, 7, 7, 1, 3,
    0x61, 0x64, 0x64, 0, 0, 10, 9, 1, 7, 0, 0x20, 0, 0x20, 1, 0x6a, 0x0b,
];

const MAIN: &str = "import init from \"./add.wasm?init\";\n\
init().then((instance) => { window.__SUM = instance.exports.add(2, 3); });\n";

#[tokio::test(flavor = "multi_thread")]
async fn wasm_init_import_instantiates_in_dev() {
    if crate::browser::skip() {
        return;
    }
    let app = App::new(&[
        ("package.json", r#"{"name":"wasm-app","version":"1.0.0"}"#),
        ("src/main.js", MAIN),
        ("index.html", INDEX_HTML),
    ]);
    // App::write takes text; the module is binary.
    std::fs::write(app.root.join("src/add.wasm"), ADD_WASM).unwrap();
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for("window.__SUM !== undefined", WAIT).await;
    let sum: serde_json::Value = page.eval("window.__SUM").await;
    assert_eq!(sum, serde_json::json!(5), "add(2, 3) through ?init");
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}
