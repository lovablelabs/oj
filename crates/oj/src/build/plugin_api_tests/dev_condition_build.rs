// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The build half of the resolve-condition check: `oj build` resolves a dep's
//! exports map with Vite's `production` condition, never `development`.

use super::{build_app, files_under, scratch, write_files};

#[tokio::test]
async fn build_resolves_the_production_export() {
    let root = scratch("dev-condition");
    write_files(
        &root,
        &[
            (
                "package.json",
                r#"{"name":"devcond-app","version":"1.0.0","dependencies":{"cond-dep":"1.0.0","cjs-dep":"1.0.0"}}"#,
            ),
            (
                "node_modules/cond-dep/package.json",
                r#"{"name":"cond-dep","version":"1.0.0","type":"module","exports":{".":{"development":"./dev.js","default":"./prod.js"}}}"#,
            ),
            ("node_modules/cond-dep/dev.js", "export const FLAVOR = \"DEV_BUILD\";\n"),
            ("node_modules/cond-dep/prod.js", "export const FLAVOR = \"PROD_BUILD\";\n"),
            (
                "node_modules/cjs-dep/package.json",
                r#"{"name":"cjs-dep","version":"1.0.0","main":"index.js"}"#,
            ),
            ("node_modules/cjs-dep/index.js", "exports.answer = 42;\n"),
            (
                "src/main.js",
                "import { FLAVOR } from \"cond-dep\";\nwindow.__FLAVOR = FLAVOR;\nimport(\"cjs-dep\").then((m) => { window.__ANSWER = m.answer; window.__DEFAULT = m.default && m.default.answer; });\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
        ],
    );
    build_app(&root).await.expect("the app builds");

    let built = files_under(&root.join("dist/assets"))
        .into_iter()
        .filter(|(p, _)| p.ends_with(".js") && !p.contains('/'))
        .map(|(_, c)| c)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        built.contains("PROD_BUILD"),
        "build did not pick the production export:\n{built}"
    );
    assert!(
        !built.contains("DEV_BUILD"),
        "build picked the development export:\n{built}"
    );
}
