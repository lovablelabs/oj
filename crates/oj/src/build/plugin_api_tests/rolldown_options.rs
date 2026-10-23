// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;

use regex::Regex;

use super::{build_app, files_under, scratch, write_files};

const INDEX_HTML: &str = r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#;

/// The `index-*.js` entry under `dist/assets`, panicking with the listing.
fn main_entry(root: &std::path::Path) -> (Vec<String>, String) {
    let names: Vec<String> = fs::read_dir(root.join("dist/assets"))
        .expect("dist/assets exists")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let main = names
        .iter()
        .find(|f| f.starts_with("index-") && f.ends_with(".js"))
        .unwrap_or_else(|| panic!("entry index-*.js under assets/: {names:?}"))
        .clone();
    let code = fs::read_to_string(root.join("dist/assets").join(&main)).unwrap();
    (names, code)
}

/// `build.rolldownOptions.output.entryFileNames`/`chunkFileNames` replace
/// Vite's `assets/[name]-[hash]` patterns (so the default assets dir goes
/// unused and the html follows the custom path), and `external` leaves the
/// module as a bare import instead of bundling it.
#[tokio::test]
async fn rolldown_options_rename_entries_and_keep_externals_bare() {
    let root = scratch("ro-names");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"ro-app","version":"1.0.0"}"#),
            (
                "oj.config.json",
                r#"{"build":{"rolldownOptions":{"output":{"entryFileNames":"custom/[name].js","chunkFileNames":"custom/[name]-[hash].js"},"external":["cdn-lib"]}}}"#,
            ),
            (
                "src/main.js",
                "import { thing } from \"cdn-lib\";\nwindow.__X = thing;\n",
            ),
            ("index.html", INDEX_HTML),
        ],
    );
    build_app(&root).await.expect("the app builds");

    let dist = root.join("dist");
    assert!(
        dist.join("custom/index.js").exists(),
        "entryFileNames template applied: {:?}",
        files_under(&dist)
            .into_iter()
            .map(|(p, _)| p)
            .collect::<Vec<_>>()
    );
    assert!(!dist.join("assets").exists(), "default assets dir not used");
    let html = fs::read_to_string(dist.join("index.html")).unwrap();
    assert!(
        html.contains(r#"src="/custom/index.js""#),
        "html references the custom entry path:\n{html}"
    );
    let entry = fs::read_to_string(dist.join("custom/index.js")).unwrap();
    assert!(
        entry.contains("\"cdn-lib\"") || entry.contains("'cdn-lib'"),
        "external module left as a bare import, not bundled:\n{entry}"
    );
}

/// Vite spreads the rest of `rollupOptions.output` into rolldown: format,
/// banner/footer, the iife `name`, `globals`, `inlineDynamicImports`; and
/// `treeshake: false` reaches rolldown. With an output ARRAY only the first
/// output is built (the warning about it is CLI stderr). Comments (the
/// banner/footer) do not survive minification, as in Vite, so minify is off.
#[tokio::test]
async fn rolldown_output_options_shape_an_iife_bundle() {
    let root = scratch("ro-iife");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"ro-app","version":"1.0.0"}"#),
            ("src/lazy.js", "export const lazy = \"LAZY_MARKER\";\n"),
            (
                "src/main.js",
                "import { thing } from \"cdn-lib\";\nwindow.__X = thing;\nexport const load = () => import(\"./lazy.js\");\n",
            ),
            (
                "oj.config.json",
                r#"{"build":{"minify":false,"rolldownOptions":{"input":{"index":"src/main.js"},"external":["cdn-lib"],"treeshake":false,"output":[{"format":"iife","name":"App","banner":"/* BANNER */","footer":"/* FOOTER */","inlineDynamicImports":true,"globals":{"cdn-lib":"CdnLib"}},{"format":"cjs"}]}}}"#,
            ),
            ("index.html", INDEX_HTML),
        ],
    );
    crate::build::take_build_warnings();
    build_app(&root).await.expect("the iife app builds");
    let warnings = crate::build::take_build_warnings();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("output lists 2 outputs; oj builds the first one only")),
        "an array output is warned about, not silently truncated: {warnings:?}"
    );

    let (files, iife) = main_entry(&root);
    assert!(
        !files.iter().any(|f| f.starts_with("lazy-")),
        "inlineDynamicImports: true folds the lazy chunk into the entry: {files:?}"
    );
    assert!(
        iife.starts_with("/* BANNER */"),
        "banner is the first thing in the file: {}",
        &iife[..iife.len().min(40)]
    );
    assert!(
        iife.trim_end().ends_with("/* FOOTER */"),
        "footer is last:\n{iife}"
    );
    assert!(
        Regex::new(r"var App\s*=|App\s*=\s*\(function|\bApp\b")
            .unwrap()
            .is_match(&iife),
        "iife exposes output.name:\n{iife}"
    );
    assert!(
        iife.contains("CdnLib"),
        "output.globals names the external's global:\n{iife}"
    );
    assert!(
        iife.contains("LAZY_MARKER"),
        "the dynamic import was inlined:\n{iife}"
    );
    assert!(
        !iife.contains("__vitePreload"),
        "no es-only preload helper in an iife bundle:\n{iife}"
    );
}

/// `output.paths` rewrites an external specifier to a URL in the es output,
/// and a banner precedes oj's `__vitePreload` helper, which es output keeps.
#[tokio::test]
async fn rolldown_output_paths_rewrite_externals_in_es_output() {
    let root = scratch("ro-paths");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"ro-app","version":"1.0.0"}"#),
            ("src/lazy.js", "export const lazy = \"LAZY_MARKER\";\n"),
            (
                "src/main.js",
                "import { thing } from \"cdn-lib\";\nwindow.__X = thing;\nwindow.__load = () => import(\"./lazy.js\");\n",
            ),
            (
                "oj.config.json",
                r#"{"build":{"rolldownOptions":{"external":["cdn-lib"],"output":{"paths":{"cdn-lib":"https://cdn.example.test/lib.js"},"banner":"/*! license */"}}}}"#,
            ),
            ("index.html", INDEX_HTML),
        ],
    );
    build_app(&root).await.expect("the es app builds");

    let (_, es_main) = main_entry(&root);
    assert!(
        es_main.contains("\"https://cdn.example.test/lib.js\"")
            || es_main.contains("'https://cdn.example.test/lib.js'"),
        "output.paths rewrites the external specifier:\n{es_main}"
    );
    assert!(
        es_main.starts_with("/*! license */"),
        "banner precedes oj's preload helper in es output:\n{es_main}"
    );
    assert!(
        es_main.contains("__vitePreload"),
        "es output keeps the preload helper:\n{es_main}"
    );
}
