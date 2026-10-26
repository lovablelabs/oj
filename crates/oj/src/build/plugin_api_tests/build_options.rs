// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite build options oj used to reject or ignore, pinned on the emitted
//! output: `build.sourcemap` "hidden" / "inline", `build.minify` as a minifier
//! name, `build.target` presets and arrays (oj_config's
//! `sourcemap_accepts_bool_and_vite_strings`, `minify_accepts_bool_and_minifier_names`
//! and `target_expands_vite_presets_and_accepts_arrays` pin the parsing; these
//! pin that the parsed value reaches rolldown/oxc), and `build.emptyOutDir`
//! inside the project root (a `.git` survives) and outside it.

use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;

use crate::build::{build, CliOptions};

use super::{scratch, write_files};

/// The app: optional chaining and nullish coalescing, so a target that lowers
/// them shows in the output; `config_file` (oj.config.json or vite.config.js)
/// holds `config`.
fn app(name: &str, config_file: &str, config: &str) -> PathBuf {
    let root = scratch(name);
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"buildopts","version":"1.0.0"}"#),
            (
                "src/main.js",
                "const o = { a: { b: 1 } };\nwindow.__V = o?.a?.b ?? 0;\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
            (config_file, config),
        ],
    );
    root
}

async fn build_or_panic(root: &Path, cli: CliOptions) {
    if let Err(err) = build(
        root.to_path_buf(),
        None,
        &tokio::runtime::Handle::current(),
        cli,
    )
    .await
    {
        panic!("build of {} failed:\n{err:#}", root.display());
    }
}

/// Builds an oj.config.json app and returns `(main chunk code, its .map path)`.
async fn built(name: &str, config: &str) -> (String, PathBuf) {
    let root = app(name, "oj.config.json", config);
    build_or_panic(&root, CliOptions::default()).await;
    main_chunk(&root.join("dist"))
}

fn main_chunk(out_dir: &Path) -> (String, PathBuf) {
    let assets = out_dir.join("assets");
    let names: Vec<String> = fs::read_dir(&assets)
        .unwrap_or_else(|e| panic!("{} exists: {e}", assets.display()))
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let js = names
        .iter()
        .find(|f| f.starts_with("index-") && f.ends_with(".js"))
        .unwrap_or_else(|| panic!("main chunk emitted: {names:?}"));
    (
        fs::read_to_string(assets.join(js)).unwrap(),
        assets.join(format!("{js}.map")),
    )
}

/// `sourcemap: "hidden"` writes the map but no `sourceMappingURL` comment;
/// `"inline"` embeds a data url and writes no `.map` file.
#[tokio::test]
async fn sourcemap_hidden_and_inline_shape_the_output() {
    let (code, map) = built(
        "buildopts-smap-hidden",
        r#"{"build":{"sourcemap":"hidden"}}"#,
    )
    .await;
    assert!(map.exists(), "hidden: .map written at {}", map.display());
    assert!(
        !code.contains("sourceMappingURL"),
        "hidden: no sourceMappingURL comment:\n{code}"
    );

    let (code, map) = built(
        "buildopts-smap-inline",
        r#"{"build":{"sourcemap":"inline"}}"#,
    )
    .await;
    assert!(!map.exists(), "inline: no .map file at {}", map.display());
    assert!(
        code.contains("sourceMappingURL=data:application/json"),
        "inline: data url comment:\n{code}"
    );
}

/// A minifier name (`"terser"`, with its `terserOptions`) means "minify";
/// `minify: false` keeps the source formatting.
#[tokio::test]
async fn minify_minifier_name_minifies_and_false_keeps_formatting() {
    let (code, _) = built(
        "buildopts-minify-terser",
        r#"{"build":{"minify":"terser","terserOptions":{"compress":{}}}}"#,
    )
    .await;
    assert!(
        !Regex::new(r"\n\s+window").unwrap().is_match(&code),
        "minify: 'terser' minifies:\n{code}"
    );

    let (code, _) = built("buildopts-minify-false", r#"{"build":{"minify":false}}"#).await;
    assert!(
        code.contains("window.__V = "),
        "minify: false keeps source formatting:\n{code}"
    );
}

/// The default baseline keeps `?.`; an array target with es2015 lowers it; the
/// Vite preset names are accepted, and the legacy `modules` preset (chrome87)
/// lowers `?.` like esbuild (a V8 bug before 91) but keeps `??`.
#[tokio::test]
async fn build_target_presets_and_arrays_reach_the_transform() {
    let (code, _) = built("buildopts-target-default", "{}").await;
    assert!(
        code.contains("?."),
        "default baseline keeps optional chaining:\n{code}"
    );

    let (code, _) = built(
        "buildopts-target-array",
        r#"{"build":{"target":["es2015","safari10"]}}"#,
    )
    .await;
    assert!(
        !code.contains("?."),
        "array target lowers optional chaining:\n{code}"
    );

    let (code, _) = built(
        "buildopts-target-baseline",
        r#"{"build":{"target":"baseline-widely-available"}}"#,
    )
    .await;
    assert!(code.contains("?."), "vite preset name accepted:\n{code}");

    let (code, _) = built(
        "buildopts-target-modules",
        r#"{"build":{"target":"modules"}}"#,
    )
    .await;
    assert!(
        code.contains("??"),
        "legacy 'modules' preset accepted:\n{code}"
    );
}

/// The same string variants through a `vite.config` (adopted values, not the
/// oj.config schema): hidden sourcemap, `minify: "esbuild"`, array target.
#[tokio::test]
async fn vite_config_string_variants_reach_the_output() {
    let root = app(
        "buildopts-viteconfig",
        "vite.config.js",
        "export default { build: { sourcemap: \"hidden\", minify: \"esbuild\", target: [\"es2015\"] } };\n",
    );
    build_or_panic(&root, CliOptions::default()).await;
    let (code, map) = main_chunk(&root.join("dist"));
    assert!(
        map.exists() && !code.contains("sourceMappingURL"),
        "vite.config hidden sourcemap (map at {}):\n{code}",
        map.display()
    );
    assert!(!code.contains("?."), "vite.config array target:\n{code}");
}

/// The default outDir (inside the project root) is emptied before the build,
/// as Vite's emptyOutDir default does, but a `.git` inside it survives
/// (Vite's prepareOutDir skips it), so a dist checked out as a deploy repo
/// keeps its history.
#[tokio::test]
async fn out_dir_inside_root_is_emptied_but_keeps_git() {
    let root = app("buildopts-inside", "oj.config.json", "{}");
    write_files(
        &root,
        &[("dist/stale.txt", "old"), ("dist/.git/HEAD", "ref")],
    );
    build_or_panic(&root, CliOptions::default()).await;
    assert!(
        !root.join("dist/stale.txt").exists(),
        "a stale file in the default outDir is removed"
    );
    assert!(
        root.join("dist/.git/HEAD").exists(),
        "a .git inside the outDir is preserved"
    );
    assert!(
        root.join("dist/index.html").exists(),
        "the build still emits"
    );
}

/// An outDir outside the project root is not emptied by default (Vite's
/// `resolveEmptyOutDir`); `--emptyOutDir` or `build.emptyOutDir: true`
/// empties it, and the build still writes there. The decision itself is
/// pinned by `out_dir_emptiable_follows_vite_and_never_counts_root_as_inside`
/// and the CLI layering by `cli_options_layer_over_the_loaded_config`; this
/// pins that a real build routes an absolute config outDir through them.
#[tokio::test]
async fn out_dir_outside_root_is_emptied_only_when_asked() {
    let outside = scratch("buildopts-outside-dist");
    let config = |extra: &str| {
        format!(
            r#"{{"build":{{"outDir":{}{extra}}}}}"#,
            serde_json::to_string(&outside.display().to_string()).unwrap()
        )
    };
    let stale = outside.join("stale.txt");

    fs::write(&stale, "old").unwrap();
    let root = app("buildopts-outside-default", "oj.config.json", &config(""));
    crate::build::take_build_warnings();
    build_or_panic(&root, CliOptions::default()).await;
    assert!(
        stale.exists(),
        "outDir outside root is not emptied by default"
    );
    let warnings = crate::build::take_build_warnings();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("is not inside project root")),
        "warns about the un-emptied outDir: {warnings:?}"
    );

    let root = app("buildopts-outside-flag", "oj.config.json", &config(""));
    build_or_panic(
        &root,
        CliOptions {
            empty_out_dir: true,
            ..Default::default()
        },
    )
    .await;
    assert!(!stale.exists(), "--emptyOutDir empties it");

    fs::write(&stale, "old").unwrap();
    let root = app(
        "buildopts-outside-config",
        "oj.config.json",
        &config(r#","emptyOutDir":true"#),
    );
    build_or_panic(&root, CliOptions::default()).await;
    assert!(!stale.exists(), "build.emptyOutDir: true empties it");
    assert!(
        outside.join("index.html").exists(),
        "the outside build still emitted"
    );
}
