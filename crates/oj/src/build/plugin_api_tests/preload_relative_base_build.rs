// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `base: "./"` emits page-relative urls (`./assets/...` from the root page,
//! `../assets/...` from a nested page, sibling refs inside css), and dynamic
//! imports go through Vite's `__vitePreload`: deps listed chunk-relative and
//! resolved against the importer, `vite:preloadError` dispatched, the lazy
//! chunk's shared chunk and stylesheet preloaded so its css applies before it
//! runs. An absolute base lists deps outDir-relative and prefixes the base in
//! the helper. Only the page entry carries the modulepreload polyfill, and
//! `build.modulePreload: false` drops it.

use std::fs;
use std::path::Path;
use std::time::Duration;

use regex::Regex;

use super::browser::Page;
use super::{build_app, preview, scratch, write_files};

const WAIT: Duration = Duration::from_secs(20);

struct Built {
    html: String,
    nested: String,
    main: String,
    lazy: String,
    main_css: String,
}

fn app() -> std::path::PathBuf {
    let root = scratch("preload-relative-base");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"relbase","version":"1.0.0"}"#),
            ("src/util.js", "export const util = \"shared-util\";\n"),
            ("src/main.css", "body { background: url(./bg.png) no-repeat; margin: 0 }\n"),
            ("src/lazy.css", "body { color: rgb(255, 0, 0) }\n"),
            (
                "src/lazy.js",
                "import \"./lazy.css\";\nimport { util } from \"./util.js\";\n\
                 export const color = getComputedStyle(document.body).color;\nexport const u = util;\n",
            ),
            (
                "src/main.js",
                "import \"./main.css\";\nwindow.__LAZY_DONE = import(\"./lazy.js\").then((m) => { window.__LAZY = m.color; window.__UTIL = m.u; });\n",
            ),
            (
                "src/nested.js",
                "import { util } from \"./util.js\";\nwindow.__NESTED = util;\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
            (
                "nested/index.html",
                r#"<!doctype html><html><head><title>n</title></head><body><script type="module" src="../src/nested.js"></script></body></html>"#,
            ),
        ],
    );
    // Over the 4096-byte assetsInlineLimit, so it is emitted rather than inlined.
    let mut png = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
    png.extend(std::iter::repeat_n(1u8, 5000));
    fs::write(root.join("src/bg.png"), png).unwrap();
    root
}

fn asset_names(dist: &Path) -> Vec<String> {
    fs::read_dir(dist.join("assets"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

fn read_asset(dist: &Path, prefix: &str, ext: &str) -> String {
    let assets = asset_names(dist);
    let f = assets
        .iter()
        .find(|a| a.starts_with(prefix) && a.ends_with(ext))
        .unwrap_or_else(|| panic!("{prefix}*{ext} emitted: {assets:?}"));
    fs::read_to_string(dist.join("assets").join(f)).unwrap()
}

async fn build_with(root: &Path, config: &str) -> Built {
    write_files(root, &[("oj.config.json", config)]);
    build_app(root).await.expect("the app builds");
    let dist = root.join("dist");
    Built {
        html: fs::read_to_string(dist.join("index.html")).unwrap(),
        nested: fs::read_to_string(dist.join("nested/index.html")).unwrap(),
        main: read_asset(&dist, "main-", ".js"),
        lazy: read_asset(&dist, "lazy-", ".js"),
        main_css: read_asset(&dist, "main-", ".css"),
    }
}

fn base_config(base: &str) -> String {
    format!(
        r#"{{"base":"{base}","build":{{"rollupOptions":{{"input":["index.html","nested/index.html"]}}}}}}"#
    )
}

#[track_caller]
fn assert_match(text: &str, re: &str, what: &str) {
    assert!(Regex::new(re).unwrap().is_match(text), "{what}:\n{text}");
}

#[track_caller]
fn assert_no_match(text: &str, re: &str, what: &str) {
    assert!(!Regex::new(re).unwrap().is_match(text), "{what}:\n{text}");
}

/// Polls for the lazy chunk's side effect; on timeout reports the page's
/// links and scripts, the useful state when the preload chain broke.
async fn wait_lazy(page: &Page) {
    if !page.poll("window.__LAZY !== undefined", WAIT).await {
        let tags: serde_json::Value = page
            .eval(r#"[...document.querySelectorAll("link,script")].map((l) => l.outerHTML)"#)
            .await;
        panic!(
            "lazy chunk never ran\ntags: {tags}\nerrors: {:?}\nconsole: {:?}",
            page.errors(),
            page.console()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn relative_base_urls_and_vite_preload_helper() {
    let root = app();

    let rel = build_with(&root, &base_config("./")).await;
    assert_match(
        &rel.html,
        r#"src="\./assets/main-[^"]+\.js""#,
        "root page script is ./assets/...",
    );
    assert_match(
        &rel.html,
        r#"rel="stylesheet" href="\./assets/main-[^"]+\.css""#,
        "root page css is ./assets/...",
    );
    assert_match(
        &rel.nested,
        r#"src="\.\./assets/nested-[^"]+\.js""#,
        "nested page script is ../assets/...",
    );
    assert_match(
        &rel.nested,
        r#"rel="modulepreload" href="\.\./assets/util-[^"]+\.js""#,
        "nested modulepreload is ../assets/...",
    );
    assert_match(
        &rel.main_css,
        r#"url\(\./bg-[a-z0-9]+\.png\)"#,
        "css url() is a sibling reference",
    );
    assert_match(
        &rel.main,
        r#"__vitePreload\(function\(\)\{return import\([`"']\./lazy-[^`"']+\.js[`"']\)\},__vite__mapDeps\(\[[\d,]+\]\),import\.meta\.url\)"#,
        "dynamic import wrapped",
    );
    assert_match(
        &rel.main,
        r#"m\.f=\["\./lazy-[^"]+\.js","\./util-[^"]+\.js","\./lazy-[^"]+\.css"\]"#,
        "deps are chunk-relative, Vite's addDeps order: lazy chunk, shared util, then its css",
    );
    assert_match(
        &rel.main,
        r"new URL\(dep,importerUrl\)",
        "relative base resolves deps against the importer",
    );
    assert_match(
        &rel.main,
        r"vite:preloadError",
        "preload helper dispatches vite:preloadError",
    );
    assert_match(
        &rel.main,
        r"relList\.supports",
        "page entry carries the modulepreload polyfill",
    );
    assert_no_match(
        &rel.lazy,
        r#"relList\.supports\("modulepreload"\)\)return;for"#,
        "non-entry chunk has no polyfill",
    );

    if !super::browser::skip() {
        let server = preview(&root.join("dist"), "/").await;
        let page = Page::open(&server.url).await;
        wait_lazy(&page).await;
        let color: String = page.eval("window.__LAZY").await;
        let util: String = page.eval("window.__UTIL").await;
        assert_eq!(
            color, "rgb(255, 0, 0)",
            "lazy chunk css applied before the chunk executed"
        );
        assert_eq!(util, "shared-util", "shared chunk linked");
        let mut problems = page.errors();
        problems.extend(page.console());
        page.close().await;

        let nested = Page::open(&format!("{}nested/", server.url)).await;
        nested.wait_for("window.__NESTED !== undefined", WAIT).await;
        problems.extend(nested.errors());
        problems.extend(nested.console());
        nested.close().await;
        assert_eq!(
            problems,
            Vec::<String>::new(),
            "page errors, console errors and warnings"
        );
    }

    let abs = build_with(&root, &base_config("/app/")).await;
    assert_match(
        &abs.html,
        r#"src="/app/assets/main-[^"]+\.js""#,
        "absolute base kept",
    );
    assert_match(
        &abs.main,
        r#"m\.f=\["assets/lazy-[^"]+\.js","assets/util-[^"]+\.js","assets/lazy-[^"]+\.css"\]"#,
        "deps are outDir-relative with an absolute base",
    );
    assert_match(
        &abs.main,
        r#"function\(dep\)\{return "/app/"\+dep\}"#,
        "absolute base is prefixed in the helper",
    );

    let off = build_with(
        &root,
        r#"{"build":{"modulePreload":false,"rollupOptions":{"input":["index.html","nested/index.html"]}}}"#,
    )
    .await;
    assert_no_match(
        &off.main,
        r#"relList\.supports\("modulepreload"\)\)return;for"#,
        "modulePreload:false removes the polyfill",
    );
}
