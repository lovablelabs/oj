// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! HTML as a build entry, as Vite builds it: a `<link rel="stylesheet">` to a
//! local file (relative or root-absolute, Sass included) is imported by the
//! page's entry, compiled into its stylesheet in document order; an inline `<script type="module">` is bundled as its own
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
    let css_links: Vec<String> =
        Regex::new(r#"<link rel="stylesheet"(?: crossorigin)? href="([^"]+)""#)
            .unwrap()
            .captures_iter(&html)
            .map(|c| c[1].to_string())
            .collect();
    let entry_css_re = Regex::new(r"^/assets/index-[A-Za-z0-9_-]{8}\.css$").unwrap();
    assert_eq!(
        css_links.len(),
        2,
        "the entry's stylesheet and the external link: {css_links:?}\n{html}"
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
    let entry_href = css_links
        .iter()
        .find(|h| entry_css_re.is_match(h))
        .unwrap_or_else(|| panic!("linked stylesheets bundled into the entry's: {css_links:?}"));
    let entry_css = fs::read_to_string(dist.join(entry_href.trim_start_matches('/'))).unwrap();
    let theme_at = Regex::new(r"#010203|rgb\(1,\s*2,\s*3\)")
        .unwrap()
        .find(&entry_css)
        .unwrap_or_else(|| panic!("scss compiled:\n{entry_css}"))
        .start();
    assert!(
        !entry_css.contains("$c"),
        "sass variable shipped:\n{entry_css}"
    );
    assert!(
        entry_css
            .find("margin:0")
            .is_some_and(|base_at| theme_at < base_at),
        "the linked css follows the scss, in document order:\n{entry_css}"
    );

    let scripts: Vec<String> =
        Regex::new(r#"<script type="module" crossorigin src="([^"]+)"></script>"#)
            .unwrap()
            .captures_iter(&html)
            .map(|c| c[1].to_string())
            .collect();
    // The page is one entry, as in Vite: its inline and external module
    // scripts load through a single chunk.
    assert_eq!(
        scripts.len(),
        1,
        "inline + main scripts are one page entry: {scripts:?}\n{html}"
    );
    let chunk_re = Regex::new(r"^/assets/.+\.js$").unwrap();
    assert!(
        scripts.iter().all(|s| chunk_re.is_match(s)),
        "it points at a hashed chunk: {scripts:?}"
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
    let entry = fs::read_to_string(dist.join(scripts[0].trim_start_matches('/'))).unwrap();
    assert!(
        entry.find("__INLINE") < entry.find("__MAIN") && entry.contains("__MAIN"),
        "the inline script runs before the external one, in document order:\n{entry}"
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
