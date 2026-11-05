// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `import data from "./data.json" with { type: "json" }` in a dev page, as
//! Vite serves it: the JSON file's module is JavaScript (`export default
//! {...}`), so importAnalysis drops a static import's attributes from the
//! code it serves (`str().remove(end + 1, expEnd)` when es-module-lexer
//! reports an attribute). Kept, the browser expects a JSON module, gets
//! `text/javascript`, and the page never boots ("Expected a JSON module
//! script").

use std::time::Duration;

use crate::browser::Page;
use crate::{App, INDEX_HTML};

fn app() -> App {
    App::new(&[
        ("package.json", r#"{"name":"json-attrs","version":"1.0.0"}"#),
        ("src/data.json", r#"{ "answer": 42 }"#),
        ("src/more.json", r#"{ "more": "yes" }"#),
        (
            "src/reexport.js",
            "export { default as more } from \"./more.json\" with { type: \"json\" };\n",
        ),
        (
            "src/main.js",
            "import data from \"./data.json\" with { type: \"json\" };\n\
             import { more } from \"./reexport.js\";\n\
             window.__ANSWER = data.answer;\n\
             window.__MORE = more.more;\n",
        ),
        ("index.html", INDEX_HTML),
    ])
}

#[tokio::test(flavor = "multi_thread")]
async fn a_json_import_with_attributes_is_served_without_them() {
    let app = app();
    let server = app.serve().await;
    for module in ["/src/main.js", "/src/reexport.js"] {
        let code = server.text(module).await;
        assert!(code.contains("/src/"), "{module}:\n{code}");
        assert!(
            !code.contains("type: \"json\"") && !code.contains("with {"),
            "{module} keeps its import attributes:\n{code}"
        );
    }
    if crate::browser::skip() {
        return;
    }
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    let loaded = page
        .poll(
            "window.__ANSWER === 42 && window.__MORE === \"yes\"",
            Duration::from_secs(20),
        )
        .await;
    let errors = page.errors();
    let console = page.console();
    page.close().await;
    assert!(loaded, "the page never ran: {errors:?} {console:?}");
    assert_eq!(errors, Vec::<String>::new(), "page errors");
}
