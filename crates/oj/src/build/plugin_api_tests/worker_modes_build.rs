// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The build half of the `?worker` check: `oj build` emits the worker as its
//! own `worker-*.js` chunk, and in the preview the worker it starts answers
//! the page's message with no page errors.

use std::time::Duration;

use super::browser::Page;
use super::{build_app, preview, scratch, write_files};

const WAIT: Duration = Duration::from_secs(20);

const WORKER: &str = "self.onmessage = (e) => { self.postMessage(e.data * 2); };\n";

const MAIN: &str = "import Work from \"./worker.js?worker\";\n\
const w = new Work();\n\
w.onmessage = (e) => { window.__W = e.data; };\n\
w.postMessage(21);\n";

#[tokio::test(flavor = "multi_thread")]
async fn worker_import_emits_chunk_and_replies_in_preview() {
    let root = scratch("worker-modes");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"worker-app","version":"1.0.0"}"#),
            ("src/worker.js", WORKER),
            ("src/main.js", MAIN),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
        ],
    );
    build_app(&root).await.expect("the worker app builds");

    let assets: Vec<String> = std::fs::read_dir(root.join("dist/assets"))
        .expect("dist/assets exists")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        assets
            .iter()
            .any(|f| f.starts_with("worker-") && f.ends_with(".js")),
        "worker not emitted as a separate chunk: {assets:?}"
    );

    if super::browser::skip() {
        return;
    }
    let preview = preview(&root.join("dist"), "/").await;
    let page = Page::open(&preview.url).await;
    page.wait_for("window.__W !== undefined", WAIT).await;
    let reply: serde_json::Value = page.eval("window.__W").await;
    assert_eq!(reply, serde_json::json!(42), "worker reply to 21");
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}

// A worker has no `document`: Vite builds it with `isWorker` set, so its
// `import()` is left unwrapped (no `__vitePreload`, no preload helper), while
// the page's `import()` still preloads.
#[tokio::test(flavor = "multi_thread")]
async fn a_workers_dynamic_import_gets_no_preload_helper() {
    let root = scratch("worker-no-preload");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"worker-app","version":"1.0.0"}"#),
            ("src/lazy.js", "export const suffix = \"!\";\n"),
            ("src/page-lazy.js", "export const page = 1;\n"),
            (
                "src/worker.js",
                "self.onmessage = async () => { const { suffix } = await import(\"./lazy.js\"); self.postMessage(suffix); };\n",
            ),
            (
                "src/main.js",
                "import Work from \"./worker.js?worker\";\nnew Work();\nimport(\"./page-lazy.js\").then((m) => console.log(m.page));\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
        ],
    );
    build_app(&root).await.expect("the worker app builds");
    let assets = root.join("dist/assets");
    let read = |prefix: &str| {
        std::fs::read_dir(&assets)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .find(|f| f.starts_with(prefix) && f.ends_with(".js"))
            .map(|f| std::fs::read_to_string(assets.join(f)).unwrap())
            .unwrap_or_else(|| panic!("no {prefix}*.js emitted"))
    };
    let worker = read("worker-");
    assert!(worker.contains("import("), "{worker}");
    // The page's import() pulls the helper into a shared chunk: the worker
    // must not import it (or carry it inline).
    assert!(
        !worker.contains("preload-helper")
            && !worker.contains(" from")
            && !worker.contains("}from"),
        "the worker imports the preload helper:\n{worker}"
    );
    assert!(
        !worker.contains("document"),
        "the worker carries document-only preload code:\n{worker}"
    );
    let page = read("index-");
    assert!(
        page.contains("modulepreload") || page.contains("preload"),
        "the page's import() still preloads:\n{page}"
    );
}
