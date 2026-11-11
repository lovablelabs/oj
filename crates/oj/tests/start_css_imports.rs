// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A plain stylesheet whose `@import`s name a package's CSS and a sibling
//! file, imported from the root route. The page links the sheet, and the
//! served text carries the imported rules in place of the `@import`s: a
//! browser cannot resolve a package specifier, so a sheet served as written
//! loses every rule of the imported files. Vite's compileCSS runs every
//! sheet containing an `@import` through postcss-import with its CSS
//! resolvers (bare ids through node_modules), in dev as in build: the
//! built sheet is checked the same way.
//!
//! The app is the Start fixture (see `start_app`). The browser half (an edit
//! of an imported file re-links the sheet) skips without Chrome, unless
//! `OJ_REQUIRE_BROWSER` is set.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;
#[allow(dead_code)]
mod start_app;

use std::path::Path;

use browser::Page;
use start_app::{app, build, Server, WAIT};

const TOKENS: &str = ":root{--acme-tokens:tokens-rule-inlined}\n";
const BASE: &str = ".base-rule{margin-top:7px}\n";
const SHEET: &str =
    "@import '@acme/tokens/dist/tokens.css';\n@import './base.css';\n.own{color:red}\n";

/// The package, the sheets, and the root route importing the sheet.
fn write_app(root: &Path) {
    // The fixture's node_modules is one symlink; a real directory of links to
    // its packages takes the test package beside them, where an app's deps
    // live.
    let shared = std::fs::read_link(root.join("node_modules")).unwrap();
    std::fs::remove_file(root.join("node_modules")).unwrap();
    std::fs::create_dir(root.join("node_modules")).unwrap();
    for entry in std::fs::read_dir(&shared).unwrap().flatten() {
        std::os::unix::fs::symlink(
            entry.path(),
            root.join("node_modules").join(entry.file_name()),
        )
        .unwrap();
    }
    let files = [
        (
            "node_modules/@acme/tokens/package.json",
            r#"{"name":"@acme/tokens","version":"1.0.0","style":"dist/tokens.css"}"#,
        ),
        ("node_modules/@acme/tokens/dist/tokens.css", TOKENS),
        ("src/base.css", BASE),
        ("src/styles.css", SHEET),
    ];
    for (name, body) in files {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    let root_route = root.join("src/routes/__root.tsx");
    let source = std::fs::read_to_string(&root_route).unwrap();
    let import = "import \"../../styles/app.css\";";
    assert!(source.contains(import), "{source}");
    let source = source.replacen(import, &format!("{import}\nimport \"../styles.css\";"), 1);
    std::fs::write(&root_route, source).unwrap();
}

/// The `href` of the page's `<link rel="stylesheet">` for `styles.css`.
fn sheet_href(html: &str) -> Option<String> {
    html.split("<link")
        .skip(1)
        .map(|tag| &tag[..tag.find('>').unwrap_or(tag.len())])
        .filter(|tag| tag.contains("stylesheet") && tag.contains("styles.css"))
        .find_map(|tag| {
            let rest = tag.split("href=\"").nth(1)?;
            Some(rest[..rest.find('"')?].to_string())
        })
}

/// The served sheet carries the imported rules and no `@import`.
fn assert_inlined(label: &str, css: &str, output: &str) {
    assert!(
        css.contains("--acme-tokens:tokens-rule-inlined"),
        "{label}: the package's rule is missing from the served sheet:\n{css}\n{output}"
    );
    assert!(
        css.contains(".base-rule{margin-top:") || css.contains(".base-rule {"),
        "{label}: the sibling sheet's rule is missing from the served sheet:\n{css}"
    );
    assert!(
        css.contains("color:red") || css.contains("color: red"),
        "{label}: the sheet's own rule is missing:\n{css}"
    );
    assert!(
        !css.contains("@import"),
        "{label}: an @import reached the browser unresolved:\n{css}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plain_sheets_package_and_relative_imports_are_served_inlined() {
    let Some(app) = app("css-imports") else {
        return;
    };
    let root = &app.0;
    write_app(root);

    let dev = Server::oj_dev(root);
    let (status, home) = dev.request("GET", "/", &[]);
    assert_eq!(status, 200, "/\n{home}\n{}", dev.output());
    let href = sheet_href(&home).unwrap_or_else(|| {
        panic!(
            "the page links no styles.css stylesheet\n{home}\n{}",
            dev.output()
        )
    });
    let (status, css) = dev.request("GET", &href, &[]);
    assert_eq!(status, 200, "{href}\n{css}\n{}", dev.output());
    assert_inlined("dev", &css, &dev.output());

    // An edit of an imported file reaches the served sheet, and an open page
    // re-links it (as Vite's HMR updates the importing sheet for a change in
    // one of its postcss-import dependencies).
    let page = if browser::skip() {
        None
    } else {
        let page = Page::open(&dev.url("/")).await;
        assert!(
            page.poll("document.readyState === 'complete' && document.body", WAIT)
                .await,
            "the page did not load: errors {:?}\n{}",
            page.errors(),
            dev.output()
        );
        page.eval::<serde_json::Value>(
            "(() => { const el = document.createElement('div'); el.className = 'base-rule'; el.id = 'probe'; document.body.appendChild(el); return null; })()",
        )
        .await;
        assert!(
            page.poll(
                "getComputedStyle(document.querySelector('#probe')).marginTop === '7px'",
                WAIT,
            )
            .await,
            "the page did not take the imported rule: errors {:?}\n{}",
            page.errors(),
            dev.output()
        );
        Some(page)
    };
    std::fs::write(root.join("src/base.css"), ".base-rule{margin-top:9px}\n").unwrap();
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let (_, css) = dev.request("GET", &href, &[]);
        if css.contains("margin-top:9px") || css.contains("margin-top: 9px") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the served sheet kept the old rule after the edit:\n{css}\n{}",
            dev.output()
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    if let Some(page) = page {
        assert!(
            page.poll(
                "getComputedStyle(document.querySelector('#probe')).marginTop === '9px'",
                WAIT,
            )
            .await,
            "the open page kept the old rule after the edit: errors {:?}\n{}",
            page.errors(),
            dev.output()
        );
        page.close().await;
    }
    drop(dev);

    build(root);
    let dist = root.join("dist");
    let mut sheets = Vec::new();
    collect_css(&dist, &mut sheets);
    let css = sheets
        .iter()
        .map(|p| std::fs::read_to_string(p).unwrap())
        .find(|css| css.contains("color:red") || css.contains("color: red"))
        .unwrap_or_else(|| panic!("no built sheet carries the app's own rule: {sheets:?}"));
    assert_inlined("build", &css, "");
}

fn collect_css(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_css(&path, out);
        } else if path.extension().is_some_and(|e| e == "css") {
            out.push(path);
        }
    }
}
