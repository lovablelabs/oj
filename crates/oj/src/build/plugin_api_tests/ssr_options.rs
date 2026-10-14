// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! SSR build options and chunking options oj used to ignore: `build.ssr: true`
//! with `rollupOptions.input`, `ssr.noExternal` / `ssr.external`,
//! `build.ssrManifest`, `output.advancedChunks`, path values in the
//! `manualChunks` object, and function-valued rollup options (which cannot
//! cross the config's JSON boundary and are reported, not fatal).

use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::Value;

use super::{build_app, files_under, scratch, write_files};

/// The shared app: a server entry importing two hand-made node_modules deps, a
/// client entry with a source util, a dep and a lazy chunk that imports css.
fn ssr_app(name: &str) -> PathBuf {
    let root = scratch(name);
    let pkg = |n: &str| {
        format!(r#"{{"name":"{n}","version":"1.0.0","type":"module","main":"index.js"}}"#)
    };
    let (ext_pkg, bundled_pkg) = (pkg("dep-ext"), pkg("dep-bundled"));
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"ssropts","version":"1.0.0"}"#),
            ("node_modules/dep-ext/package.json", &ext_pkg),
            (
                "node_modules/dep-ext/index.js",
                "export const ext = \"DEP_EXT_MARKER\";\n",
            ),
            ("node_modules/dep-bundled/package.json", &bundled_pkg),
            (
                "node_modules/dep-bundled/index.js",
                "export const bundled = \"DEP_BUNDLED_MARKER\";\n",
            ),
            (
                "src/entry-server.js",
                "import { ext } from \"dep-ext\";\nimport { bundled } from \"dep-bundled\";\nexport function render() { return ext + bundled; }\n",
            ),
            ("src/utils/index.js", "export const util = \"UTIL_MARKER\";\n"),
            ("src/lazy.css", ".lazy { color: red }\n"),
            (
                "src/lazy.js",
                "import \"./lazy.css\";\nexport const lazy = \"LAZY_MARKER\";\n",
            ),
            (
                "src/main.js",
                "import { util } from \"./utils/index.js\";\nimport { ext } from \"dep-ext\";\nwindow.__U = util + ext;\nimport(\"./lazy.js\").then((m) => { window.__L = m.lazy; });\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
        ],
    );
    root
}

async fn build_with(root: &Path, config_file: &str, config: &str) {
    write_files(root, &[(config_file, config)]);
    if let Err(err) = build_app(root).await {
        panic!("build with {config_file} `{config}` failed:\n{err:#}");
    }
}

fn read(root: &Path, rel: &str) -> String {
    fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("{rel} was written: {e}"))
}

fn imports_from(dep: &str) -> Regex {
    Regex::new(&format!(r#"from\s*["']{dep}["']"#)).unwrap()
}

fn ssr_manifest(root: &Path, rel: &str) -> serde_json::Map<String, Value> {
    match serde_json::from_str(&read(root, rel)).expect("ssr manifest is json") {
        Value::Object(m) => m,
        other => panic!("ssr manifest is an object: {other}"),
    }
}

/// The url list a manifest key maps to (empty when absent).
fn urls<'a>(m: &'a serde_json::Map<String, Value>, key: &str) -> Vec<&'a str> {
    m.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// `build.ssr: true` takes the server entry from `rollupOptions.input`, and
/// bare deps stay external by default (Vite's SSR externalization).
#[tokio::test]
async fn build_ssr_true_takes_rollup_input_and_keeps_deps_external() {
    let root = ssr_app("ssropts-ssr-true");
    build_with(
        &root,
        "oj.config.json",
        r#"{"build":{"ssr":true,"rollupOptions":{"input":"src/entry-server.js"}}}"#,
    )
    .await;
    let server = read(&root, "dist/entry-server.mjs");
    assert!(
        imports_from("dep-ext").is_match(&server),
        "dep-ext stays external by default:\n{server}"
    );
    assert!(
        imports_from("dep-bundled").is_match(&server),
        "dep-bundled stays external by default:\n{server}"
    );
    assert!(
        !server.contains("DEP_BUNDLED_MARKER"),
        "dep-bundled not inlined by default:\n{server}"
    );
}

/// An `ssr.noExternal` list bundles just those deps; the rest stay external.
#[tokio::test]
async fn ssr_no_external_list_inlines_only_the_listed_deps() {
    let root = ssr_app("ssropts-noext-list");
    build_with(
        &root,
        "oj.config.json",
        r#"{"build":{"ssr":"src/entry-server.js"},"ssr":{"noExternal":["dep-bundled"]}}"#,
    )
    .await;
    let server = read(&root, "dist/entry-server.mjs");
    assert!(
        server.contains("DEP_BUNDLED_MARKER"),
        "ssr.noExternal inlines dep-bundled:\n{server}"
    );
    assert!(
        imports_from("dep-ext").is_match(&server),
        "dep-ext still external:\n{server}"
    );
}

/// `noExternal: true` inlines every dep, but `ssr.external` still wins for
/// its entries.
#[tokio::test]
async fn ssr_external_wins_over_no_external_true() {
    let root = ssr_app("ssropts-noext-true");
    build_with(
        &root,
        "oj.config.json",
        r#"{"build":{"ssr":"src/entry-server.js"},"ssr":{"noExternal":true,"external":["dep-ext"]}}"#,
    )
    .await;
    let server = read(&root, "dist/entry-server.mjs");
    assert!(
        server.contains("DEP_BUNDLED_MARKER"),
        "noExternal: true inlines deps:\n{server}"
    );
    assert!(
        imports_from("dep-ext").is_match(&server),
        "ssr.external wins over noExternal: true:\n{server}"
    );
}

/// The same SSR options are read from a vite.config.
#[tokio::test]
async fn vite_config_ssr_no_external_is_applied() {
    let root = ssr_app("ssropts-vite-config");
    build_with(
        &root,
        "vite.config.js",
        "export default { build: { ssr: true, rollupOptions: { input: \"src/entry-server.js\" } }, ssr: { noExternal: [\"dep-bundled\"] } };\n",
    )
    .await;
    let server = read(&root, "dist/entry-server.mjs");
    assert!(
        server.contains("DEP_BUNDLED_MARKER"),
        "vite.config ssr.noExternal applied:\n{server}"
    );
}

/// `build.ssrManifest` in the client build maps root-relative source modules
/// to their chunk + css urls (the entry has none), and, like Vite's css deps
/// map, keys the dynamically imported chunk by file name with the stylesheets
/// it brings in.
#[tokio::test]
async fn ssr_manifest_maps_modules_and_chunks_to_their_js_and_css() {
    let root = ssr_app("ssropts-manifest");
    build_with(&root, "oj.config.json", r#"{"build":{"ssrManifest":true}}"#).await;
    let m = ssr_manifest(&root, "dist/.vite/ssr-manifest.json");
    let keys: Vec<&String> = m.keys().collect();
    assert!(
        m.get("src/lazy.js").is_some_and(Value::is_array),
        "lazy module keyed root-relative: {keys:?}"
    );
    let lazy = urls(&m, "src/lazy.js");
    let lazy_js = Regex::new(r"^/assets/lazy-.*\.js$").unwrap();
    let lazy_css = Regex::new(r"^/assets/lazy-.*\.css$").unwrap();
    assert!(
        lazy.iter().any(|u| lazy_js.is_match(u)),
        "lazy chunk url listed: {lazy:?}"
    );
    assert!(
        lazy.iter().any(|u| lazy_css.is_match(u)),
        "lazy css url listed: {lazy:?}"
    );
    assert_eq!(
        m.get("src/main.js"),
        Some(&Value::Array(vec![])),
        "entry module has no preload urls"
    );
    let chunk_key = Regex::new(r"^lazy-.*\.js$").unwrap();
    let lazy_chunk = m
        .keys()
        .find(|k| chunk_key.is_match(k))
        .unwrap_or_else(|| panic!("dynamic import chunk keyed by file name: {keys:?}"));
    let chunk_urls = urls(&m, lazy_chunk);
    assert!(
        chunk_urls.iter().any(|u| lazy_css.is_match(u)),
        "css deps of the lazy chunk listed: {chunk_urls:?}"
    );
}

/// A string `build.ssrManifest` names the manifest file.
#[tokio::test]
async fn ssr_manifest_file_name_is_honored() {
    let root = ssr_app("ssropts-manifest-name");
    build_with(
        &root,
        "oj.config.json",
        r#"{"build":{"ssrManifest":"custom-ssr.json"}}"#,
    )
    .await;
    assert!(
        root.join("dist/custom-ssr.json").exists(),
        "ssrManifest file name honored"
    );
}

/// importedAssets (Vite): a module's imported asset url is listed for its
/// chunk under `base`, and the asset module itself maps to its url.
#[tokio::test]
async fn ssr_manifest_lists_imported_assets_under_base() {
    let root = ssr_app("ssropts-manifest-assets");
    fs::write(root.join("src/big.png"), vec![1u8; 9000]).unwrap();
    write_files(
        &root,
        &[(
            "src/lazy.js",
            "import \"./lazy.css\";\nimport big from \"./big.png\";\nexport const lazy = \"LAZY_MARKER\" + big;\n",
        )],
    );
    build_with(
        &root,
        "oj.config.json",
        r#"{"base":"/app/","build":{"ssrManifest":true}}"#,
    )
    .await;
    let m = ssr_manifest(&root, "dist/.vite/ssr-manifest.json");
    let big = Regex::new(r"^/app/assets/big-.*\.png$").unwrap();
    let lazy = urls(&m, "src/lazy.js");
    assert!(
        lazy.iter().any(|u| big.is_match(u)),
        "imported asset url listed under base: {lazy:?}"
    );
    let own = urls(&m, "src/big.png");
    assert!(
        own.iter().any(|u| big.is_match(u)),
        "the asset module itself maps to its url: {own:?}"
    );
}

/// An SSR app (server entry + client sibling): the client entry honors `base`
/// for import.meta.env.BASE_URL, and the script/style urls the server injects
/// sit under base + assetsDir. Only server.mjs's text is checked; running it
/// with node is not part of this assertion.
#[tokio::test]
async fn ssr_app_client_sibling_honors_base_and_assets_dir() {
    let root = ssr_app("ssropts-client-sibling");
    write_files(
        &root,
        &[(
            "src/entry-client.js",
            "import \"./lazy.css\";\nwindow.__B = import.meta.env.BASE_URL;\n",
        )],
    );
    build_with(
        &root,
        "oj.config.json",
        r#"{"base":"/app/","build":{"ssr":"src/entry-server.js","assetsDir":"static"}}"#,
    )
    .await;
    let server = read(&root, "dist/server.mjs");
    let client_js = Regex::new(r#""/app/static/entry-client-[^"]+\.js""#).unwrap();
    let client_css = Regex::new(r#""/app/static/style-[^"]+\.css""#).unwrap();
    let injected = Regex::new(r#"CLIENT_JS = "[^"]*""#).unwrap();
    assert!(
        client_js.is_match(&server),
        "server injects the client script under base + assetsDir: {:?}",
        injected.find(&server).map(|m| m.as_str())
    );
    assert!(
        client_css.is_match(&server),
        "server injects the client stylesheet under base + assetsDir:\n{server}"
    );
    let client = files_under(&root.join("dist/static"))
        .into_iter()
        .find(|(f, _)| f.starts_with("entry-client-") && f.ends_with(".js"))
        .map(|(_, code)| code)
        .expect("the client entry chunk lands in dist/static");
    let base_define = Regex::new(r#"["'`]/app/["'`]"#).unwrap();
    assert!(
        base_define.is_match(&client),
        "client BASE_URL define is the base:\n{client}"
    );
}

/// The `[name, contents]` of the first top-level `dist/assets` js file whose
/// name starts with `prefix`.
fn asset_chunk(root: &Path, prefix: &str) -> Option<(String, String)> {
    files_under(&root.join("dist/assets"))
        .into_iter()
        .find(|(f, _)| !f.contains('/') && f.starts_with(prefix) && f.ends_with(".js"))
}

/// A `manualChunks` object: bare names hit node_modules packages, `./` paths
/// hit source files.
#[tokio::test]
async fn manual_chunks_object_takes_package_names_and_source_paths() {
    let root = ssr_app("ssropts-manual-chunks");
    build_with(
        &root,
        "oj.config.json",
        r#"{"build":{"rollupOptions":{"output":{"manualChunks":{"vendor":["dep-ext"],"utils":["./src/utils/index.js"]}}}}}"#,
    )
    .await;
    let vendor = asset_chunk(&root, "vendor-");
    assert!(
        vendor
            .as_ref()
            .is_some_and(|(_, c)| c.contains("DEP_EXT_MARKER")),
        "vendor chunk from package name: {vendor:?}"
    );
    let utils = asset_chunk(&root, "utils-");
    assert!(
        utils
            .as_ref()
            .is_some_and(|(_, c)| c.contains("UTIL_MARKER")),
        "utils chunk from a ./ path value: {utils:?}"
    );
}

/// rolldown's native `advancedChunks` groups, with a RegExp `test` (so the
/// config is an oj.config.mjs).
#[tokio::test]
async fn advanced_chunks_groups_are_emitted() {
    let root = ssr_app("ssropts-advanced-chunks");
    build_with(
        &root,
        "oj.config.mjs",
        "export default { build: { rolldownOptions: { output: { advancedChunks: { groups: [{ name: \"libs\", test: /node_modules/ }] } } } } };\n",
    )
    .await;
    let libs = asset_chunk(&root, "libs-");
    let names: Vec<String> = files_under(&root.join("dist/assets"))
        .into_iter()
        .map(|(f, _)| f)
        .collect();
    assert!(
        libs.as_ref()
            .is_some_and(|(_, c)| c.contains("DEP_EXT_MARKER")),
        "advancedChunks group emitted: {names:?}"
    );
}

/// A function-valued rollup option cannot cross the config's JSON boundary;
/// it is warned about and ignored, and the build still succeeds, from an
/// oj.config.mjs and from a vite.config.
#[tokio::test]
async fn function_valued_rollup_options_do_not_fail_the_build() {
    let root = ssr_app("ssropts-fn-oj-config");
    crate::build::take_build_warnings();
    build_with(
        &root,
        "oj.config.mjs",
        "export default { build: { rollupOptions: { output: { manualChunks(id) { return \"x\"; }, chunkFileNames: () => \"c.js\" } } } };\n",
    )
    .await;
    assert!(
        root.join("dist/index.html").exists(),
        "oj.config build still succeeds"
    );
    let warnings = crate::build::take_build_warnings();
    for key in ["output.manualChunks", "output.chunkFileNames"] {
        assert!(
            warnings
                .iter()
                .any(|w| w.contains(&format!("{key} is a function"))),
            "oj.config warns about the {key} function: {warnings:?}"
        );
    }

    let root = ssr_app("ssropts-fn-vite-config");
    build_with(
        &root,
        "vite.config.js",
        "export default { build: { rollupOptions: { output: { manualChunks(id) { return \"x\"; } } } } };\n",
    )
    .await;
    assert!(
        root.join("dist/index.html").exists(),
        "vite.config build still succeeds"
    );
    let warnings = crate::build::take_build_warnings();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("output.manualChunks is a function")),
        "vite.config warns about the manualChunks function: {warnings:?}"
    );
}
