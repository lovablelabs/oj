// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The build half of the compiled-stylesheet-import check (Vite parity):
//! `a.scss?url` emits a compiled `.css` asset the main chunk references,
//! `b.css?inline` is compiled css text (never an octet-stream data URI), and
//! an aliased plain `.svg` import is an asset url.

use std::fs;

use regex::Regex;

use super::{build_app, files_under, scratch, write_files};

#[tokio::test]
async fn css_url_is_a_compiled_asset_and_inline_is_compiled_text() {
    let root = scratch("css-url-inline");
    let src = serde_json::to_string(&root.join("src").to_string_lossy()).unwrap();
    let config = format!("export default {{ resolve: {{ alias: {{ \"@\": {src} }} }} }};\n");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"cssurl-app","version":"1.0.0"}"#),
            ("vite.config.js", config.as_str()),
            ("src/a.scss", "$c: red;\n.a { color: $c; }\n"),
            ("src/b.css", ".b {\n  color: blue;\n}\n"),
            (
                "src/assets/logo.svg",
                r#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4"/></svg>"#,
            ),
            (
                "src/main.js",
                "import aUrl from \"./a.scss?url\";\nimport bInline from \"./b.css?inline\";\nimport logo from \"@/assets/logo.svg\";\nwindow.__A = aUrl; window.__B = bInline; window.__LOGO = logo;\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
        ],
    );
    build_app(&root).await.expect("the app builds");

    let assets = root.join("dist/assets");
    let files: Vec<String> = files_under(&assets).into_iter().map(|(p, _)| p).collect();
    let main_file = files
        .iter()
        .find(|f| f.starts_with("index-") && f.ends_with(".js"))
        .unwrap_or_else(|| panic!("main chunk emitted: {files:?}"));
    let js = fs::read_to_string(assets.join(main_file)).unwrap();
    let css_file = files
        .iter()
        .find(|f| f.starts_with("a-") && f.ends_with(".css"))
        .unwrap_or_else(|| panic!("compiled a.css asset emitted for ?url: {files:?}"));
    let css = fs::read_to_string(assets.join(css_file)).unwrap();
    assert!(
        Regex::new(r"color:\s*red").unwrap().is_match(&css),
        "?url asset is compiled sass:\n{css}"
    );
    assert!(
        !css.contains("$c"),
        "sass source shipped as the ?url asset:\n{css}"
    );
    assert!(
        js.contains(css_file.as_str()),
        "main chunk references the css asset url {css_file}:\n{js}"
    );
    assert!(
        Regex::new(r"\.b\{color:(blue|#00f)\}")
            .unwrap()
            .is_match(&js),
        "?inline is compiled css text in the build:\n{js}"
    );
    assert!(
        !js.contains("application/octet-stream"),
        "css ?inline shipped as an octet-stream data URI:\n{js}"
    );
    assert!(
        js.contains("data:image/svg+xml") || js.contains("assets/logo-"),
        "aliased svg is an asset url in the build:\n{js}"
    );
}
