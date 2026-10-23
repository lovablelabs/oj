// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! oj's own routes answer under `base`, as Vite's do behind its base
//! middleware: the page names the dev client there.

use crate::App;

#[tokio::test(flavor = "multi_thread")]
async fn ojs_routes_answer_under_base() {
    let app = App::new(&[
        ("package.json", r#"{"name":"based","version":"1.0.0"}"#),
        ("oj.config.json", r#"{"base":"/app/"}"#),
        (
            "index.html",
            r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
        ),
        ("src/main.js", "export const x = 1;\n"),
    ]);
    let server = app.serve().await;
    let html = server.text("/app/").await;
    for src in ["/app/@oj/client.js", "/app/@oj/refresh-preamble.js"] {
        assert!(html.contains(&format!("src=\"{src}\"")), "{html}");
        let res = server.get(src).await;
        assert_eq!(res.status, 200, "{src}: {}", res.body);
    }
    // Compiled modules import oj's urls without the base; they still answer.
    for src in ["/@oj/client.js", "/@oj/refresh-runtime.js"] {
        assert_eq!(server.get(src).await.status, 200, "{src}");
    }
}
