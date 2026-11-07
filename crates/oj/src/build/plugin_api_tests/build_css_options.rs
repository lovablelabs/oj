// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;

use regex::Regex;

use super::{build_app, scratch, write_files};

const NESTED: &str = ".a {\n  color: red;\n  .b { color: blue }\n}\n";

const LIGHTNING_SRC: &str =
    ".kept { color: red }\n.gone { color: blue }\n.c:hover { color: green }\n";
const LIGHTNING: &str =
    r#"css: { lightningcss: { unusedSymbols: ["gone"], pseudoClasses: { hover: "is-hover" } } }"#;

/// Builds an app importing `source` as its one stylesheet under `config`
/// (`oj.config.ts`) and returns that stylesheet, asserting there is exactly one.
async fn build_css(name: &str, config: &str, source: &str) -> String {
    let root = scratch(name);
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"css-options","type":"module"}"#),
            ("oj.config.ts", config),
            (
                "index.html",
                r#"<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
            ("src/main.js", "import \"./app.css\";\n"),
            ("src/app.css", source),
        ],
    );
    build_app(&root)
        .await
        .unwrap_or_else(|e| panic!("{name} builds: {e:#}"));
    let assets = root.join("dist/assets");
    let css: Vec<String> = fs::read_dir(&assets)
        .expect("dist/assets exists")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|f| f.ends_with(".css"))
        .collect();
    assert_eq!(css.len(), 1, "expected one stylesheet, got {css:?}");
    fs::read_to_string(assets.join(&css[0])).unwrap()
}

// build.cssMinify and build.cssTarget (Vite build.ts): cssMinify defaults to
// build.minify and can be set on its own; cssTarget defaults to build.target
// and decides what lightningcss lowers (CSS nesting here). css.lightningcss
// is spread into the build minify (css.ts minifyCSS), the only lightningcss
// pass under Vite's default transformer.

/// By default CSS is minified (cssMinify follows build.minify) and nesting is
/// lowered for the baseline build target.
#[tokio::test]
async fn default_css_is_minified_and_nesting_lowered() {
    let css = build_css("css-opt-default", "export default {};\n", NESTED).await;
    assert!(
        !Regex::new(r"\n\s+color").unwrap().is_match(&css) && css.contains(".a{color:red}"),
        "default: minified: {css:?}"
    );
    assert!(
        Regex::new(r"\.a \.b\s*\{").unwrap().is_match(&css),
        "default: nesting lowered for the baseline target: {css:?}"
    );
}

/// cssMinify defaults to build.minify, so `minify: false` leaves CSS readable.
#[tokio::test]
async fn build_minify_false_leaves_css_unminified() {
    let css = build_css(
        "css-opt-nominify",
        "export default { build: { minify: false } };\n",
        NESTED,
    )
    .await;
    assert!(
        css.trim().contains('\n') && css.contains("color: red"),
        "build.minify false leaves CSS unminified: {css:?}"
    );
}

/// cssMinify set on its own wins over build.minify false.
#[tokio::test]
async fn css_minify_true_minifies_with_build_minify_false() {
    let css = build_css(
        "css-opt-cssminify-on",
        "export default { build: { minify: false, cssMinify: true } };\n",
        NESTED,
    )
    .await;
    assert!(
        css.contains(".a{color:red}"),
        "cssMinify true minifies even with build.minify false: {css:?}"
    );
}

/// cssMinify false keeps CSS readable while JS minify stays on.
#[tokio::test]
async fn css_minify_false_keeps_css_readable_with_minify_on() {
    let css = build_css(
        "css-opt-cssminify-off",
        "export default { build: { cssMinify: false } };\n",
        NESTED,
    )
    .await;
    assert!(
        css.contains("color: red"),
        "cssMinify false keeps CSS readable with build.minify on: {css:?}"
    );
}

/// A cssTarget whose browsers support nesting keeps it unlowered.
#[tokio::test]
async fn css_target_with_nesting_support_keeps_nesting() {
    let css = build_css(
        "css-opt-csstarget",
        "export default { build: { cssTarget: [\"chrome120\", \"safari17.2\", \"firefox117\"] } };\n",
        NESTED,
    )
    .await;
    assert!(
        !css.contains(".a .b") && css.contains(".b{"),
        "cssTarget with nesting support keeps nesting: {css:?}"
    );
}

/// cssTarget defaults to build.target.
#[tokio::test]
async fn css_target_defaults_to_build_target() {
    let css = build_css(
        "css-opt-target",
        "export default { build: { target: [\"chrome120\", \"safari17.2\", \"firefox117\"] } };\n",
        NESTED,
    )
    .await;
    assert!(
        !css.contains(".a .b"),
        "cssTarget defaults to build.target: {css:?}"
    );
}

/// css.lightningcss options (unusedSymbols, pseudoClasses) reach the build
/// minify pass.
#[tokio::test]
async fn css_lightningcss_applies_to_the_build_minify() {
    let css = build_css(
        "css-opt-lightning",
        &format!("export default {{ {LIGHTNING} }};\n"),
        LIGHTNING_SRC,
    )
    .await;
    assert!(
        css.contains(".kept{") && !css.contains(".gone") && css.contains(".c.is-hover{"),
        "css.lightningcss applies to the build minify: {css:?}"
    );
}

/// With minify off there is no lightningcss pass, so css.lightningcss options
/// do not apply.
#[tokio::test]
async fn css_lightningcss_stays_out_of_an_unminified_build() {
    let css = build_css(
        "css-opt-lightning-nominify",
        &format!("export default {{ build: {{ minify: false }}, {LIGHTNING} }};\n"),
        LIGHTNING_SRC,
    )
    .await;
    assert!(
        css.contains(".gone") && css.contains(":hover"),
        "css.lightningcss stays out of an unminified build: {css:?}"
    );
}
