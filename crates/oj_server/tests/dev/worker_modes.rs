// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `import Work from "./worker.js?worker"` in dev: the default export is a
//! Worker constructor for that file, and the worker it starts answers the
//! page's message. The build half is oj's `worker_modes_build`.

use std::time::Duration;

use crate::browser::Page;
use crate::{App, INDEX_HTML};

const WAIT: Duration = Duration::from_secs(20);

const WORKER: &str = "self.onmessage = (e) => { self.postMessage(e.data * 2); };\n";

const MAIN: &str = "import Work from \"./worker.js?worker\";\n\
const w = new Work();\n\
w.onmessage = (e) => { window.__W = e.data; };\n\
w.postMessage(21);\n";

#[tokio::test(flavor = "multi_thread")]
async fn worker_import_replies_in_dev() {
    if crate::browser::skip() {
        return;
    }
    let app = App::new(&[
        ("package.json", r#"{"name":"worker-app","version":"1.0.0"}"#),
        ("src/worker.js", WORKER),
        ("src/main.js", MAIN),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for("window.__W !== undefined", WAIT).await;
    let reply: serde_json::Value = page.eval("window.__W").await;
    assert_eq!(reply, serde_json::json!(42), "worker reply to 21");
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}

/// A module worker importing a plain `.ts` module with a capitalized function
/// export runs in dev. Vite's oxc plugin gives such a module no React Refresh,
/// so it carries no glue reading `window` or loading the overlay, which
/// extends `HTMLElement` (neither exists in a worker).
#[tokio::test(flavor = "multi_thread")]
async fn module_worker_importing_a_ts_function_export_runs_in_dev() {
    if crate::browser::skip() {
        return;
    }
    let app = App::new(&[
        ("package.json", r#"{"name":"worker-ts","version":"1.0.0"}"#),
        (
            "src/scale.ts",
            "export function M(x: number): number { return x * 2; }\n",
        ),
        (
            "src/worker.ts",
            "import { M } from \"./scale\";\nself.onmessage = (e: MessageEvent<number>) => { self.postMessage(M(e.data)); };\n",
        ),
        (
            "src/main.js",
            "import Work from \"./worker.ts?worker\";\n\
const w = new Work();\n\
w.onmessage = (e) => { window.__W = e.data; };\n\
w.onerror = (e) => { window.__W = `error: ${e.message}`; };\n\
w.postMessage(21);\n",
        ),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for("window.__W !== undefined", WAIT).await;
    let reply: serde_json::Value = page.eval("window.__W").await;
    assert_eq!(reply, serde_json::json!(42), "worker reply to 21");
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}
