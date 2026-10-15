// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The build half of the `new URL("./asset", import.meta.url)` check: the
//! asset resolves (a data: URL for a tiny file, an /assets path for a big
//! one), never a raw relative literal that 404s.

use regex::Regex;

use super::{build_app, files_under, scratch, write_files};

#[tokio::test]
async fn new_url_import_meta_url_asset_resolves_in_build() {
    let root = scratch("new-url-asset");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"newurl","private":true}"#),
            (
                "index.html",
                r#"<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
            (
                "src/logo.svg",
                "<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>\n",
            ),
            (
                "src/main.js",
                "export const u = new URL(\"./logo.svg\", import.meta.url).href;\n",
            ),
        ],
    );
    build_app(&root).await.expect("the app builds");

    // Top-level .js files of dist/assets.
    let built = files_under(&root.join("dist/assets"))
        .into_iter()
        .filter(|(p, _)| p.ends_with(".js") && !p.contains('/'))
        .map(|(_, c)| c)
        .collect::<String>();
    assert!(
        !Regex::new(r#"new URL\(\s*[`"']\./logo\.svg"#)
            .unwrap()
            .is_match(&built),
        "build left a raw new URL literal (would 404):\n{built}"
    );
    assert!(
        built.contains("data:image/svg") || built.contains("/assets/logo"),
        "build did not resolve the new URL asset:\n{built}"
    );
}
