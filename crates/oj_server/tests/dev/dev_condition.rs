// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite resolves with a `development` condition in dev. A dep whose exports
//! map branches on it resolves to the same file in the dev server and in the
//! pre-bundle (optimizeDeps.include), and a dynamic import() of a CommonJS dep
//! gets the namespace interop wrapper. The build half (`production`) is oj's
//! `dev_condition_build`. `oj_resolver`'s `resolves_exports_per_condition`
//! pins the resolver alone.

use std::time::{Duration, Instant};

use regex::Regex;

use crate::{App, INDEX_HTML};

const MAIN_JS: &str = "import { FLAVOR } from \"cond-dep\";\nwindow.__FLAVOR = FLAVOR;\nimport(\"cjs-dep\").then((m) => { window.__ANSWER = m.answer; window.__DEFAULT = m.default && m.default.answer; });\n";

fn app() -> App {
    App::new(&[
        (
            "package.json",
            r#"{"name":"devcond-app","version":"1.0.0","dependencies":{"cond-dep":"1.0.0","cjs-dep":"1.0.0"}}"#,
        ),
        (
            "node_modules/cond-dep/package.json",
            r#"{"name":"cond-dep","version":"1.0.0","type":"module","exports":{".":{"development":"./dev.js","default":"./prod.js"}}}"#,
        ),
        (
            "node_modules/cond-dep/dev.js",
            "export const FLAVOR = \"DEV_BUILD\";\n",
        ),
        (
            "node_modules/cond-dep/prod.js",
            "export const FLAVOR = \"PROD_BUILD\";\n",
        ),
        (
            "node_modules/cjs-dep/package.json",
            r#"{"name":"cjs-dep","version":"1.0.0","main":"index.js"}"#,
        ),
        ("node_modules/cjs-dep/index.js", "exports.answer = 42;\n"),
        ("src/main.js", MAIN_JS),
        ("index.html", INDEX_HTML),
    ])
}

#[tokio::test(flavor = "multi_thread")]
async fn dev_resolves_the_development_export_and_wraps_cjs_dynamic_imports() {
    let app = app();
    let server = app.serve().await;
    let main = server.text("/src/main.js").await;
    let dep_url = Regex::new(r#"from\s+"([^"]+)""#)
        .unwrap()
        .captures(&main)
        .unwrap_or_else(|| panic!("static import in main.js:\n{main}"))[1]
        .to_string();
    let served = server.text(&dep_url).await;
    assert!(
        served.contains("DEV_BUILD"),
        "dev did not pick the development export:\n{}",
        &served[..served.len().min(300)]
    );
    assert!(
        Regex::new(r#"import\("[^"]+"\)\.then\(__oj_dyn_interop\)"#)
            .unwrap()
            .is_match(&main),
        "dynamic import of a CJS dep is not interop-wrapped:\n{main}"
    );
    assert!(
        main.contains("const __oj_dyn_interop = "),
        "interop helper missing:\n{main}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pre_bundle_picks_the_same_development_export() {
    let app = app();
    app.write(&[(
        "oj.config.json",
        r#"{"optimizeDeps":{"include":["cond-dep"]}}"#,
    )]);
    let server = app.serve().await;

    // The pre-bundle commits in the background after boot.
    let deps_import = Regex::new(r#"from\s+"(/@oj-deps/[^"]+)""#).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let url = loop {
        let main = server.text("/src/main.js").await;
        if let Some(c) = deps_import.captures(&main) {
            break c[1].to_string();
        }
        assert!(
            Instant::now() < deadline,
            "cond-dep was not pre-bundled from optimizeDeps.include:\n{main}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let bundled = server.text(&url).await;
    assert!(
        bundled.contains("DEV_BUILD"),
        "pre-bundle picked a different file than the dev server:\n{}",
        &bundled[..bundled.len().min(300)]
    );
}
