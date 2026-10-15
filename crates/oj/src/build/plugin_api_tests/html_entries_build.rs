// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! HTML as a build entry, as Vite builds it: a `<link rel="stylesheet">` to a
//! local file (relative or root-absolute, Sass included) is compiled, hashed
//! and rewritten; an inline `<script type="module">` is bundled as its own
//! entry (relative imports resolve against the page) and replaced by a
//! `src`; an external link stays untouched. Under preview the inline module
//! runs with its import and both stylesheets apply.

use std::fs;
use std::time::Duration;

use regex::Regex;

use super::browser::Page;
use super::{build_app, preview, scratch, write_files};

const WAIT: Duration = Duration::from_secs(20);

const INDEX: &str = "<!doctype html><html><head><title>t</title>\n\
<link rel=\"stylesheet\" href=\"./src/theme.scss\">\n\
<link rel=\"stylesheet\" href=\"/src/base.css\">\n\
<link rel=\"stylesheet\" href=\"https://cdn.example.test/x.css\">\n\
</head><body><h1>hi</h1>\n\
<script type=\"module\">import { x } from \"./src/x.js\"; document.title = x; window.__INLINE = true;</script>\n\
<script type=\"module\" src=\"/src/main.js\"></script>\n\
</body></html>";

#[tokio::test(flavor = "multi_thread")]
async fn html_links_and_inline_modules_are_built_entries() {
    let root = scratch("html-entries");
    write_files(
        &root,
        &[
            (
                "package.json",
                r#"{"name":"html-entries","version":"1.0.0"}"#,
            ),
            ("src/theme.scss", "$c: rgb(1, 2, 3);\nh1 { color: $c; }\n"),
            ("src/base.css", "body { margin: 0; }\n"),
            ("src/x.js", "export const x = \"from-inline\";\n"),
            ("src/main.js", "window.__MAIN = true;\n"),
            ("index.html", INDEX),
        ],
    );
    build_app(&root).await.expect("the app builds");

    let dist = root.join("dist");
    let html = fs::read_to_string(dist.join("index.html")).unwrap();
    let css_links: Vec<String> = Regex::new(r#"<link rel="stylesheet" href="([^"]+)""#)
        .unwrap()
        .captures_iter(&html)
        .map(|c| c[1].to_string())
        .collect();
    let theme_re = Regex::new(r"^/assets/theme-[0-9a-f]+\.css$").unwrap();
    let base_re = Regex::new(r"^/assets/base-[0-9a-f]+\.css$").unwrap();
    assert!(
        css_links.iter().any(|h| theme_re.is_match(h)),
        "scss link hashed: {css_links:?}"
    );
    assert!(
        css_links.iter().any(|h| base_re.is_match(h)),
        "css link hashed: {css_links:?}"
    );
    assert!(
        css_links
            .iter()
            .any(|h| h == "https://cdn.example.test/x.css"),
        "external link untouched: {css_links:?}"
    );
    assert!(
        !Regex::new(r"src/theme\.scss|/src/base\.css")
            .unwrap()
            .is_match(&html),
        "source paths left in the built html:\n{html}"
    );
    let theme_href = css_links.iter().find(|h| h.contains("theme-")).unwrap();
    let theme_css = fs::read_to_string(dist.join(theme_href.trim_start_matches('/'))).unwrap();
    assert!(
        Regex::new(r"#010203|rgb\(1,\s*2,\s*3\)")
            .unwrap()
            .is_match(&theme_css),
        "scss compiled:\n{theme_css}"
    );
    assert!(
        !theme_css.contains("$c"),
        "sass variable shipped:\n{theme_css}"
    );

    let scripts: Vec<String> =
        Regex::new(r#"<script type="module" src="([^"]+)"(?: crossorigin)?></script>"#)
            .unwrap()
            .captures_iter(&html)
            .map(|c| c[1].to_string())
            .collect();
    assert_eq!(
        scripts.len(),
        2,
        "inline + main scripts externalized: {scripts:?}\n{html}"
    );
    let chunk_re = Regex::new(r"^/assets/.+\.js$").unwrap();
    assert!(
        scripts.iter().all(|s| chunk_re.is_match(s)),
        "both point at hashed chunks: {scripts:?}"
    );
    assert!(
        !Regex::new(r"import \{ x \}|@oj-inline")
            .unwrap()
            .is_match(&html),
        "inline body or placeholder left in html:\n{html}"
    );
    assert!(
        scripts
            .iter()
            .any(|s| fs::read_to_string(dist.join(s.trim_start_matches('/')))
                .unwrap()
                .contains("from-inline")),
        "inline script bundled with its relative import: {scripts:?}"
    );

    if super::browser::skip() {
        return;
    }
    let server = preview(&dist, "/").await;
    let page = Page::open(&server.url).await;
    page.wait_for("window.__INLINE === true && window.__MAIN === true", WAIT)
        .await;
    let title: String = page.eval("document.title").await;
    assert_eq!(title, "from-inline", "inline module ran with its import");
    let color: String = page
        .eval(r#"getComputedStyle(document.querySelector("h1")).color"#)
        .await;
    assert_eq!(color, "rgb(1, 2, 3)", "linked scss applied");
    let margin: String = page.eval("getComputedStyle(document.body).margin").await;
    assert_eq!(margin, "0px", "linked css applied");
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}
