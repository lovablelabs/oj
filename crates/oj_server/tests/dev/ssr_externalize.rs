// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Which resolved dependency files the ssr environment leaves to Node. Vite's
//! canExternalizeFile hands Node only `.js` / `.mjs` / `.cjs` or an
//! extensionless file, reading the extension with its query, so
//! `pkg/styles.css?url` and a raw `.ts` source stay in the pipeline. Before,
//! oj externalized every dependency file but a stylesheet or JSON, and read
//! `styles.css?url` as extension `css?url`: Node then loaded the sheet.

use crate::{App, INDEX_HTML};

#[tokio::test(flavor = "multi_thread")]
async fn ssr_externalizes_only_what_vite_can_externalize() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"ssrext","private":true,"type":"module"}"#,
        ),
        (
            "node_modules/pkg/package.json",
            r#"{"name":"pkg","version":"1.0.0","type":"module","main":"index.js"}"#,
        ),
        ("node_modules/pkg/index.js", "export default 'pkg';\n"),
        ("node_modules/pkg/lib.cjs", "module.exports = 1;\n"),
        ("node_modules/pkg/bin", "export default 'bin';\n"),
        ("node_modules/pkg/styles.css", ".pkg { color: red }\n"),
        ("node_modules/pkg/util.ts", "export const n: number = 1;\n"),
        ("node_modules/pkg/data.json", "{\"a\":1}\n"),
        ("src/main.js", "export {};\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let importer = app.root.join("src/main.js");
    let resolve = |spec: &'static str| {
        let url = format!(
            "/@ssr-resolve?importer={}&spec={}",
            importer.display(),
            spec.replace('?', "%3F")
        );
        let server = &server;
        async move { server.text(&url).await }
    };

    for spec in ["pkg", "pkg/lib.cjs", "pkg/bin"] {
        let r = resolve(spec).await;
        assert!(r.contains(r#""external":true"#), "{spec} stays Node's: {r}");
    }
    for (spec, file) in [
        ("pkg/styles.css?url", "/node_modules/pkg/styles.css?url"),
        ("pkg/styles.css", "/node_modules/pkg/styles.css"),
        ("pkg/util.ts", "/node_modules/pkg/util.ts"),
        ("pkg/data.json", "/node_modules/pkg/data.json"),
        ("pkg/index.js?raw", "/node_modules/pkg/index.js?raw"),
    ] {
        let r = resolve(spec).await;
        assert!(
            !r.contains(r#""external""#) && r.contains(&format!("{file}\"")),
            "{spec} goes through the pipeline as {file}: {r}"
        );
    }

    // The sheet's URL module, not the sheet run as JavaScript.
    let sheet = app.root.join("node_modules/pkg/styles.css");
    let module = server
        .get(&format!("/@ssr-module?id={}%3Furl", sheet.display()))
        .await;
    assert_eq!(module.status, 200, "{}", module.body);
    assert!(
        module.body.contains("export default") && module.body.contains("styles.css"),
        "{}",
        module.body
    );
    // An app file's `?raw`, its text.
    let raw = server
        .text(&format!("/@ssr-module?id={}%3Fraw", importer.display()))
        .await;
    assert!(raw.contains(r#"export default "export {};\n""#), "{raw}");
}
