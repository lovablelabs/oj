// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Generic `import.meta.hot` in a plain (non-React) app, as Vite defines it,
//! applied in a real page: `accept()` makes a module self-accepting,
//! `accept("./dep", cb)` makes the importer the dependency's boundary and
//! hands the callback the new module, `accept([deps], cb)` gets an array
//! aligned with deps, and `dispose(cb)` runs before replacement. None of it
//! reloads the page.

use std::time::Duration;

use crate::browser::Page;
use crate::{App, INDEX_HTML};

const WAIT: Duration = Duration::from_secs(20);

fn store(s: &str) -> String {
    format!(
        "window.__storeRuns = (window.__storeRuns || 0) + 1;\nexport const s = \"{s}\";\n\
         if (import.meta.hot) {{\n  import.meta.hot.dispose(() => {{ window.__storeDisposed = (window.__storeDisposed || 0) + 1; }});\n\
         \x20 import.meta.hot.accept((m) => {{ window.__storeAccepted = m.s; }});\n}}\n"
    )
}

const MAIN: &str = r#"import { value } from "./util.js";
import { other } from "./other.js";
import "./store.js";
window.__util = value; window.__other = other; window.__READY = true;
if (import.meta.hot) {
  import.meta.hot.accept("./util.js", (m) => { window.__util = m.value; });
  import.meta.hot.accept(["./other.js", "./util.js"], ([o, u]) => { window.__arr = [o && o.other, u && u.value]; });
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn hot_accept_dispose_and_dep_boundaries_apply_without_reload() {
    if crate::browser::skip() {
        return;
    }
    let app = App::new(&[
        ("package.json", r#"{"name":"hot-accept","version":"1.0.0"}"#),
        ("src/util.js", "export const value = \"u1\";\n"),
        ("src/other.js", "export const other = \"o1\";\n"),
        ("src/store.js", &store("s1")),
        ("src/main.js", MAIN),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;

    let main = server.text("/src/main.js").await;
    assert!(
        main.contains(r#"createHotContext("/src/main.js")"#),
        "hot context injected for a module using import.meta.hot:\n{main}"
    );
    assert!(
        main.contains(r#"accept("/src/util.js""#),
        "accept dep specifier rewritten to its served url:\n{main}"
    );
    let util = server.text("/src/util.js").await;
    assert!(
        !util.contains("createHotContext"),
        "a module not using import.meta.hot gets no context:\n{util}"
    );

    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for("window.__READY === true", WAIT).await;
    page.eval::<bool>("window.__NOT_RELOADED = true").await;

    // Dep accept: editing util updates main's binding through the callback.
    app.write(&[("src/util.js", "export const value = \"u2\";\n")]);
    page.wait_for(r#"window.__util === "u2""#, WAIT).await;
    let arr: serde_json::Value = page.eval("window.__arr").await;
    assert_eq!(
        arr,
        serde_json::json!([null, "u2"]),
        "array form gets the new module in its slot only"
    );

    // Array-form dep: editing other.
    app.write(&[("src/other.js", "export const other = \"o2\";\n")]);
    page.wait_for(
        r#"Array.isArray(window.__arr) && window.__arr[0] === "o2""#,
        WAIT,
    )
    .await;

    // Self accept: store re-imports, dispose ran, the callback got new exports.
    app.write(&[("src/store.js", &store("s2"))]);
    page.wait_for(r#"window.__storeAccepted === "s2""#, WAIT)
        .await;
    let runs: u32 = page.eval("window.__storeRuns").await;
    let disposed: u32 = page.eval("window.__storeDisposed").await;
    let not_reloaded: bool = page.eval("window.__NOT_RELOADED === true").await;
    assert_eq!(runs, 2, "store executed exactly twice");
    assert_eq!(disposed, 1, "dispose ran once before replacement");
    assert!(not_reloaded, "page never reloaded");
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}
