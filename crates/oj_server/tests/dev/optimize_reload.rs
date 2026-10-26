// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A page whose only script is a refresh preamble that then imports the app
//! (TanStack Start's dev client entry) recovers from a reloading dep rerun.
//! The app imports deps the scan never saw; their rerun moves a pre-bundled
//! dep's chunk, so the commit reloads and an older held request is the
//! outdated 504 that fails the app's dynamic import. As under Vite, the
//! refresh runtime brings the HMR client in with the preamble, so the reload
//! still reaches the page and the reloaded page runs.

use std::time::Duration;

use crate::browser::Page;
use crate::App;

const CONFIG: &str = r#"export default {
  optimizeDeps: { entries: ["scan.js"] },
  plugins: [{
    name: "document",
    configureServer(server) {
      server.middlewares.use((req, res, next) => {
        if (req.url !== "/page") return next();
        res.setHeader("content-type", "text/html");
        res.end('<!doctype html><html><head></head><body><script type="module" src="/entry.js"></script></body></html>');
      });
    },
  }],
};
"#;

fn esm(name: &str, body: &str) -> [(String, String); 2] {
    [
        (
            format!("node_modules/{name}/package.json"),
            format!(r#"{{"name":"{name}","version":"1.0.0","type":"module","main":"index.js"}}"#),
        ),
        (format!("node_modules/{name}/index.js"), body.to_string()),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn a_preamble_page_reloads_past_an_outdated_dep() {
    crate::require_fixture!("vite");
    if crate::browser::skip() {
        return;
    }
    let mut files = vec![
        (
            "package.json".to_string(),
            r#"{"name":"preamble-app","private":true,"type":"module"}"#.to_string(),
        ),
        ("vite.config.js".into(), CONFIG.into()),
        ("scan.js".into(), "import \"dep-a\";\n".into()),
        (
            "entry.js".into(),
            "import \"/@react-refresh\";\nawait import(\"/app.js\");\n".into(),
        ),
        (
            "app.js".into(),
            "import { a } from \"dep-a\";\nimport { b } from \"late-b\";\nimport { d } from \"late-c\";\nwindow.__R = a + b + d;\n"
                .into(),
        ),
    ];
    files.extend(esm(
        "common",
        "export function c() {\n  return String.fromCharCode(99);\n}\n",
    ));
    files.extend(esm(
        "dep-a",
        "import { c } from \"common\";\nexport const a = \"a\" + c();\n",
    ));
    files.extend(esm(
        "late-b",
        "import { c } from \"common\";\nexport const b = \"b\" + c();\n",
    ));
    files.extend(esm("late-c", "export const d = \"d\";\n"));
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_str()))
        .collect();
    let app = App::new(&refs);
    app.link_fixture("vite");
    let server = app.serve().await;
    crate::plugin_middleware::middleware_ready(&server).await;
    let listening = server.listen().await;

    let page = Page::open(&format!("http://127.0.0.1:{}/page", listening.addr.port())).await;
    let ran = page
        .poll(r#"window.__R === "acbcd""#, Duration::from_secs(30))
        .await;
    assert!(
        ran,
        "the page never ran its app; errors: {:?}, console: {:?}",
        page.errors(),
        page.console()
    );
    let nav: String = page
        .eval(r#"performance.getEntriesByType("navigation")[0].type"#)
        .await;
    assert_eq!(nav, "reload", "the page ran only after the rerun's reload");
    page.close().await;
}
