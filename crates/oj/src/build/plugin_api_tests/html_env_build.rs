// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The build half of Vite's htmlEnvHook check: `%VITE_*%` and `%MODE%` in
//! index.html are replaced in dist/index.html with the env value, and an
//! unknown placeholder is left untouched.

use std::fs;

use super::{build_app, scratch, write_files};

#[tokio::test]
async fn html_env_placeholders_are_substituted_in_the_build() {
    let root = scratch("html-env-build");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"htmlenv","private":true}"#),
            (".env", "VITE_APP_TITLE=Hello OJ\n"),
            (
                "index.html",
                r#"<!doctype html><html><head><title>%VITE_APP_TITLE%</title><meta name="mode" content="%MODE%"><meta name="missing" content="%VITE_MISSING%"></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
            ("src/main.js", "export const v = 1;\n"),
        ],
    );
    build_app(&root).await.expect("the app builds");

    let html = fs::read_to_string(root.join("dist/index.html")).unwrap();
    assert!(
        html.contains("<title>Hello OJ</title>"),
        "%VITE_APP_TITLE% not substituted:\n{html}"
    );
    assert!(
        html.contains(r#"content="production""#),
        "%MODE% not substituted to production:\n{html}"
    );
    assert!(
        html.contains(r#"content="%VITE_MISSING%""#),
        "unknown %VITE_MISSING% should be left as-is:\n{html}"
    );
}
