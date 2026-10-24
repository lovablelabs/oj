// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;

use super::{build_app, files_under, scratch, write_files};

/// An app with a lazy chunk, an imported stylesheet whose url() asset is
/// above the 4 KiB inline limit (so it is emitted as a file), and an html
/// stylesheet link, configured by `config` as `oj.config.ts`.
fn scaffold(name: &str, config: &str) -> PathBuf {
    let root = scratch(name);
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"assets-dir","type":"module"}"#),
            ("oj.config.ts", config),
            (
                "index.html",
                r#"<!doctype html><html><head><link rel="stylesheet" href="/src/page.css"></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
            (
                "src/main.js",
                "import \"./app.css\";\nimport(\"./lazy.js\").then((m) => m.run());\n",
            ),
            (
                "src/lazy.js",
                "export function run() { document.body.textContent = \"lazy\"; }\n",
            ),
            ("src/app.css", ".a { background: url(./big.png); }\n"),
            ("src/page.css", ".p { color: red; }\n"),
        ],
    );
    fs::write(root.join("src/big.png"), vec![1u8; 8192]).unwrap();
    root
}

/// Every output path under `dist`, minus Vite's `.vite/` metadata dir.
fn outputs(root: &Path) -> Vec<String> {
    files_under(&root.join("dist"))
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| !p.starts_with(".vite/"))
        .collect()
}

/// The concatenated contents of every `.css` output in `files`.
fn css_of(root: &Path, files: &[String]) -> String {
    files
        .iter()
        .filter(|f| f.ends_with(".css"))
        .map(|f| fs::read_to_string(root.join("dist").join(f)).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
}

/// build.assetsDir: hashed chunks, stylesheets and url() assets land under
/// the configured directory (Vite's default output patterns are
/// `${assetsDir}/[name]-[hash]...`); the html, the css url() and the manifest
/// all reference that directory under an absolute base.
#[tokio::test]
async fn assets_dir_places_every_hashed_output_under_it() {
    let root = scaffold(
        "assets-dir-static",
        "export default { build: { assetsDir: \"static\", manifest: true } };\n",
    );
    build_app(&root).await.expect("the assetsDir app builds");

    let files = outputs(&root);
    let hashed: Vec<&String> = files.iter().filter(|f| *f != "index.html").collect();
    assert!(
        !hashed.is_empty() && hashed.iter().all(|f| f.starts_with("static/")),
        "every hashed output is under static/: {files:?}"
    );
    let lazy = Regex::new(r"^static/lazy-[^/]+\.js$").unwrap();
    assert!(
        hashed.iter().any(|f| lazy.is_match(f)),
        "lazy chunk under static/: {files:?}"
    );
    assert!(
        hashed.iter().any(|f| f.ends_with(".css")),
        "stylesheets under static/: {files:?}"
    );
    let big = Regex::new(r"^static/big-[^/]+\.png$").unwrap();
    assert!(
        hashed.iter().any(|f| big.is_match(f)),
        "url() asset under static/: {files:?}"
    );
    assert!(
        !files.iter().any(|f| f.starts_with("assets/")),
        "nothing under assets/: {files:?}"
    );

    let html = fs::read_to_string(root.join("dist/index.html")).unwrap();
    assert!(
        Regex::new(r#"/static/index-[^"']+\.js"#)
            .unwrap()
            .is_match(&html)
            && Regex::new(r#"/static/[^"']+\.css"#)
                .unwrap()
                .is_match(&html),
        "html references /static/:\n{html}"
    );

    let css = css_of(&root, &files);
    assert!(
        Regex::new(r#"url\("?/static/big-[^)"]+\.png"?\)"#)
            .unwrap()
            .is_match(&css),
        "css url() points at /static/:\n{css}"
    );

    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(root.join("dist/.vite/manifest.json")).expect("manifest written"),
    )
    .unwrap();
    let entries = manifest.as_object().expect("manifest is an object");
    assert!(
        !entries.is_empty(),
        "the manifest lists the outputs: {manifest}"
    );
    assert!(
        entries
            .values()
            .all(|e| e["file"].as_str().is_some_and(|f| f.starts_with("static/"))),
        "manifest file paths start with static/: {manifest}"
    );
}

/// assetsDir "" puts hashed outputs at the outDir root, and with a relative
/// base the stylesheet references its url() asset as a sibling.
#[tokio::test]
async fn empty_assets_dir_with_relative_base_references_siblings() {
    let root = scaffold(
        "assets-dir-root",
        "export default { base: \"./\", build: { assetsDir: \"\" } };\n",
    );
    build_app(&root)
        .await
        .expect("the root-assetsDir app builds");

    let files = outputs(&root);
    assert!(
        files.iter().all(|f| !f.contains('/')),
        "hashed outputs at the outDir root: {files:?}"
    );
    let css = css_of(&root, &files);
    assert!(
        Regex::new(r#"url\("?\./big-[^)"]+\.png"?\)"#)
            .unwrap()
            .is_match(&css),
        "relative css url() is a sibling:\n{css}"
    );
}

/// With no assetsDir configured the default stays Vite's `assets/`.
#[tokio::test]
async fn default_assets_dir_is_assets() {
    let root = scaffold("assets-dir-default", "export default {};\n");
    build_app(&root).await.expect("the default app builds");

    let files = outputs(&root);
    assert!(
        files.iter().any(|f| f.starts_with("assets/")),
        "the default build emits hashed outputs: {files:?}"
    );
    assert!(
        files
            .iter()
            .filter(|f| *f != "index.html")
            .all(|f| f.starts_with("assets/")),
        "default assetsDir is assets/: {files:?}"
    );
}
