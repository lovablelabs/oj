// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `oj build` of a TanStack Start app, end to end on the `oj` binary: each
//! test builds a private copy of e2e/fixtures/start-app, which shares the
//! fixture's installed `node_modules`.
//!
//! The fixture is installed by `npm ci --prefix e2e/fixtures/start-app`;
//! without it the tests skip, unless `OJ_REQUIRE_FIXTURES` is set.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A private copy of the fixture's sources, or None (after saying why) when
/// the fixture is not installed.
fn app(name: &str) -> Option<PathBuf> {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../e2e/fixtures/start-app")
        .canonicalize()
        .unwrap();
    if !fixture.join("node_modules/@tanstack/react-start").is_dir() {
        assert!(
            std::env::var_os("OJ_REQUIRE_FIXTURES").is_none(),
            "OJ_REQUIRE_FIXTURES is set but the start-app fixture is not installed"
        );
        eprintln!("skipping: npm ci --prefix e2e/fixtures/start-app");
        return None;
    }
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("oj-start-build-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    copy_sources(&fixture, &root);
    std::os::unix::fs::symlink(fixture.join("node_modules"), root.join("node_modules")).unwrap();
    Some(root)
}

/// Everything but installs and outputs: no node_modules, dist, caches, or a
/// route tree an earlier build generated.
fn copy_sources(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let name = entry.file_name();
        if matches!(
            name.to_str(),
            Some("node_modules" | "dist" | ".oj-cache" | ".tanstack" | "routeTree.gen.ts")
        ) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            copy_sources(&path, &to.join(&name));
        } else {
            std::fs::copy(&path, to.join(&name)).unwrap();
        }
    }
}

fn build(root: &Path) {
    let out = Command::new(env!("CARGO_BIN_EXE_oj"))
        .arg("build")
        .arg(root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "oj build failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The route tree is generated under the build's own config and env, as the
/// router plugin does inside `vite build`: a cold build extracts the config
/// for the build only, never a second time for `serve`. Each extraction
/// leaves one cache entry.
#[test]
fn a_cold_start_build_extracts_its_config_for_the_build_only() {
    let Some(root) = app("extract-once") else {
        return;
    };
    build(&root);
    let entries: Vec<_> = std::fs::read_dir(root.join(".oj-cache/v1/config-extract"))
        .map(|d| d.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert_eq!(entries.len(), 1, "config extractions: {entries:?}");
    assert!(
        root.join("src/routeTree.gen.ts").is_file(),
        "the build generated no route tree"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A plugin reading the module graph from a bundle hook, as a manifest
/// plugin does: `this.getModuleIds` and `this.getModuleInfo` are the
/// bundle's own under `vite build`, so they know the modules no JS hook ran
/// for (node_modules), and a module's code reads as JS.
const GRAPH_PROBE: &str = r#"
import { writeFileSync } from "node:fs";
function graphProbe(): Plugin {
  let root = "";
  return {
    name: "graph-probe",
    configResolved(c) {
      root = c.root;
    },
    generateBundle() {
      const ids = [...this.getModuleIds()];
      const about = this.getModuleInfo(ids.find((id) => id.endsWith("/src/routes/about.tsx")) ?? "");
      const dep = ids.find((id) => id.includes("/node_modules/") && this.getModuleInfo(id)?.importers.length);
      writeFileSync(
        `${root}/graph-${this.environment.name}.json`,
        JSON.stringify({
          nodeModules: ids.filter((id) => id.includes("/node_modules/")).length,
          dep: dep ?? null,
          aboutImporters: about?.importers.length ?? null,
          aboutCode: about?.code ?? null,
        }),
      );
    },
  };
}
"#;

#[test]
fn bundle_hooks_read_the_start_bundles_whole_module_graph() {
    let Some(root) = app("module-graph") else {
        return;
    };
    let config = root.join("vite.config.ts");
    let source = std::fs::read_to_string(&config).unwrap();
    let source = source.replacen("  plugins: [\n", "  plugins: [\n    graphProbe(),\n", 1);
    std::fs::write(&config, format!("{source}{GRAPH_PROBE}")).unwrap();
    build(&root);
    for env in ["client", "ssr"] {
        let path = root.join(format!("graph-{env}.json"));
        let graph: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            graph["nodeModules"].as_u64().unwrap_or(0) > 0 && graph["dep"].is_string(),
            "{env}: the node_modules modules and their importers: {graph}"
        );
        assert!(
            graph["aboutImporters"].as_u64().unwrap_or(0) > 0,
            "{env}: src/routes/about.tsx and its importers: {graph}"
        );
        let code = graph["aboutCode"].as_str().unwrap_or_default();
        assert!(
            code.contains("createRoute") && !code.contains("<main>"),
            "{env}: src/routes/about.tsx's code reads as JS: {graph}"
        );
    }
}

/// A plugin rewriting Start's manifest, as an app adding its own preloads
/// does: under `vite build` the server bundle's `tanstack-start-manifest:v`
/// is Start's module (`export const tsrStartManifest = () => (...)`, the
/// routes' preloads and scripts built from the client bundle), and a
/// `transform` hook sees it under Vite's id for a virtual module.
const MANIFEST_PROBE: &str = r#"
import { writeFileSync as writeProbe } from "node:fs";
function manifestProbe(): Plugin {
  let root = "";
  return {
    name: "manifest-probe",
    apply: "build",
    configResolved(c) {
      root = c.root;
    },
    transform(code, id) {
      if (!id.endsWith("tanstack-start-manifest:v")) return;
      writeProbe(
        `${root}/manifest-${this.environment.name}.json`,
        JSON.stringify({ id, consumer: this.environment.config.consumer, code }),
      );
      if (this.environment.config.consumer !== "server") return;
      return code.replace("preloads:[", 'preloads:["/assets/added-by-transform.js",');
    },
  };
}
"#;

#[test]
fn a_plugin_transforms_starts_manifest_in_the_server_bundle() {
    let Some(root) = app("manifest-transform") else {
        return;
    };
    let config = root.join("vite.config.ts");
    let source = std::fs::read_to_string(&config).unwrap();
    let source = source.replacen("  plugins: [\n", "  plugins: [\n    manifestProbe(),\n", 1);
    std::fs::write(&config, format!("{source}{MANIFEST_PROBE}")).unwrap();
    build(&root);

    let probe: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("manifest-ssr.json"))
            .expect("the transform saw the manifest in the server bundle"),
    )
    .unwrap();
    assert_eq!(probe["id"], "\0tanstack-start-manifest:v", "{probe}");
    assert_eq!(probe["consumer"], "server", "{probe}");
    let code = probe["code"].as_str().unwrap_or_default();
    assert!(
        code.starts_with("export const tsrStartManifest = () => (")
            && code.contains("/assets/client-"),
        "Start's own manifest, naming the client entry: {code}"
    );

    // The bundle carries what the transform returned, and the page still
    // links the client's stylesheet, which oj adds to the root route.
    let mut bundle = String::new();
    for dir in ["dist", "dist/chunks"] {
        for entry in std::fs::read_dir(root.join(dir)).unwrap().flatten() {
            if entry.path().extension().is_some_and(|e| e == "mjs") {
                bundle.push_str(&std::fs::read_to_string(entry.path()).unwrap());
            }
        }
    }
    assert!(
        bundle.contains("/assets/added-by-transform.js"),
        "the server bundle has the transformed manifest"
    );
    let sheet = std::fs::read_dir(root.join("dist/client/assets"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|f| f.ends_with(".css"))
        .expect("the client emitted a stylesheet");
    assert!(
        bundle.contains(&format!("/assets/{sheet}")),
        "the manifest links /assets/{sheet}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `build.manifest` on the client environment writes Vite's
/// `.vite/manifest.json` beside the client's files: a row per chunk, keyed
/// by its facade module's path when it has one (the entry, a dynamic entry)
/// and `_<file>` otherwise, and a row per stylesheet keyed by its source.
#[test]
fn the_client_environments_manifest_is_written() {
    let Some(root) = app("vite-manifest") else {
        return;
    };
    let config = root.join("vite.config.ts");
    let source = std::fs::read_to_string(&config).unwrap();
    let client = r#"    client: { define: { __FIXTURE_SIDE__: JSON.stringify("client-side") } },"#;
    assert!(
        source.contains(client),
        "the fixture's client environment moved"
    );
    let source = source.replacen(
        client,
        r#"    client: { define: { __FIXTURE_SIDE__: JSON.stringify("client-side") }, build: { manifest: true } },"#,
        1,
    );
    std::fs::write(&config, source).unwrap();
    build(&root);

    let raw = std::fs::read_to_string(root.join("dist/client/.vite/manifest.json"))
        .expect("dist/client/.vite/manifest.json is written");
    let manifest: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&raw).unwrap();
    let on_disk = |row: &serde_json::Value| {
        row["file"]
            .as_str()
            .is_some_and(|f| root.join("dist/client").join(f).is_file())
    };
    assert!(
        manifest.values().all(on_disk),
        "every row names a file of dist/client:\n{raw}"
    );

    let entries: Vec<_> = manifest.values().filter(|r| r["isEntry"] == true).collect();
    assert_eq!(entries.len(), 1, "one entry row:\n{raw}");
    assert!(
        entries[0]["file"]
            .as_str()
            .unwrap()
            .starts_with("assets/client-"),
        "{raw}"
    );

    // The fixture's lazy chunk has a facade module (a plugin's virtual
    // module): keyed by it, as a dynamic entry, and the entry's row names it
    // by that key.
    let (lazy_key, lazy) = manifest
        .iter()
        .find(|(_, r)| r["isDynamicEntry"] == true)
        .unwrap_or_else(|| panic!("a dynamic entry row:\n{raw}"));
    assert!(
        !lazy_key.starts_with('_') && lazy["src"] == lazy_key.as_str(),
        "the dynamic entry is keyed by its source: {lazy_key}: {lazy}"
    );
    assert!(
        manifest.values().any(|r| r["dynamicImports"]
            .as_array()
            .is_some_and(|d| d.iter().any(|k| k == lazy_key.as_str()))),
        "a chunk imports {lazy_key} dynamically:\n{raw}"
    );

    // The stylesheet: a row of its own by source, and under `css` of the
    // chunk that imports it.
    let sheet = &manifest["styles/app.css"];
    let file = sheet["file"].as_str().unwrap_or_default();
    assert!(
        file.starts_with("assets/app-") && file.ends_with(".css"),
        "styles/app.css is a row: {sheet}\n{raw}"
    );
    assert!(
        manifest.values().any(|r| r["css"]
            .as_array()
            .is_some_and(|c| c.iter().any(|f| f == file))),
        "a chunk lists {file} under css:\n{raw}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The chunks a bundle hook sees carry Vite's `viteMetadata`: `importedCss`
/// and `importedAssets` are Sets, and a chunk holding a stylesheet's module
/// names the sheet's file, as Vite's css plugin records it. A plugin
/// collecting each chunk's stylesheets reads it without a check.
const CHUNK_CSS_PROBE: &str = r#"
import { writeFileSync as writeChunkCss } from "node:fs";
function chunkCssProbe(): Plugin {
  let root = "";
  return {
    name: "chunk-css-probe",
    apply: "build",
    configResolved(c) {
      root = c.root;
    },
    generateBundle(_options, bundle) {
      if (this.environment.name !== "client") return;
      const chunks = Object.values(bundle).filter((output) => output.type === "chunk");
      writeChunkCss(
        `${root}/chunk-css.json`,
        JSON.stringify({
          sets: chunks.every(
            (chunk) =>
              chunk.viteMetadata.importedCss instanceof Set && chunk.viteMetadata.importedAssets instanceof Set,
          ),
          css: Object.fromEntries(chunks.map((chunk) => [chunk.fileName, [...chunk.viteMetadata.importedCss]])),
          codeSplit: this.environment.config.build.cssCodeSplit,
        }),
      );
    },
  };
}
"#;

fn chunk_css_app(name: &str, top_level: &str) -> Option<(PathBuf, serde_json::Value)> {
    let app = app(name)?;
    let config = app.join("vite.config.ts");
    let source = std::fs::read_to_string(&config).unwrap();
    let source = source
        .replacen("  plugins: [\n", "  plugins: [\n    chunkCssProbe(),\n", 1)
        .replacen(
            "  publicDir: \"public\",\n",
            &format!("{top_level}  publicDir: \"public\",\n"),
            1,
        );
    std::fs::write(&config, format!("{source}{CHUNK_CSS_PROBE}")).unwrap();
    build(&app);
    let probe = serde_json::from_str(
        &std::fs::read_to_string(app.join("chunk-css.json"))
            .expect("the client's generateBundle ran"),
    )
    .unwrap();
    Some((app, probe))
}

/// The page of a built app links `sheet` once, from the manifest in its
/// server bundle.
fn bundle_text(root: &Path) -> String {
    let mut bundle = String::new();
    for dir in ["dist", "dist/chunks"] {
        for entry in std::fs::read_dir(root.join(dir)).unwrap().flatten() {
            if entry.path().extension().is_some_and(|e| e == "mjs") {
                bundle.push_str(&std::fs::read_to_string(entry.path()).unwrap());
            }
        }
    }
    bundle
}

#[test]
fn a_chunk_names_the_stylesheets_of_its_modules() {
    let Some((app, probe)) = chunk_css_app("chunk-css", "") else {
        return;
    };
    assert_eq!(probe["sets"], true, "every chunk has the two Sets: {probe}");
    let sheets: Vec<&str> = probe["css"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|css| css.as_array().unwrap())
        .filter_map(|f| f.as_str())
        .collect();
    assert!(
        !sheets.is_empty()
            && sheets.iter().all(|f| {
                f.starts_with("assets/")
                    && f.ends_with(".css")
                    && app.join("dist/client").join(f).is_file()
            }),
        "the chunks name the client's stylesheet files: {probe}"
    );
    // Start's manifest takes a route's stylesheets from its chunks; in the
    // manifest the server serves, the root links the sheet, once, and no
    // other route does.
    let manifest = served_manifest(&app);
    let href = format!("/{}", sheets[0]);
    assert_eq!(
        manifest["routes"]["__root__"]["css"],
        serde_json::json!([href]),
        "{manifest}"
    );
    for (id, route) in manifest["routes"].as_object().unwrap() {
        assert!(
            id == "__root__" || route["css"].as_array().is_none_or(|css| css.is_empty()),
            "{id} links a sheet the root links: {manifest}"
        );
    }
    let _ = std::fs::remove_dir_all(&app);
}

/// What `tsrStartManifest()` returns in the built server: the manifest
/// chunk, evaluated.
fn served_manifest(root: &Path) -> serde_json::Value {
    let chunk = std::fs::read_dir(root.join("dist/chunks"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .is_some_and(|f| f.to_string_lossy().starts_with("manifest-sheets-"))
        })
        .expect("the server bundle's manifest chunk");
    let out = Command::new("node")
        .arg("--input-type=module")
        .arg("-e")
        .arg("const m = await import(process.argv[1]); console.log(JSON.stringify(m.tsrStartManifest()));")
        .arg(format!("file://{}", chunk.display()))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "the manifest chunk evaluates: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// `build.cssCodeSplit: false` builds: the sheets are written and linked as
/// with the option on, which is how the environment's config reads.
#[test]
fn a_build_with_css_code_split_off_links_its_stylesheets() {
    let Some((app, probe)) =
        chunk_css_app("css-code-split-off", "  build: { cssCodeSplit: false },\n")
    else {
        return;
    };
    assert_eq!(probe["codeSplit"], true, "{probe}");
    let sheet = std::fs::read_dir(app.join("dist/client/assets"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|f| f.ends_with(".css"))
        .expect("the client emitted a stylesheet");
    assert!(
        bundle_text(&app).contains(&format!("/assets/{sheet}")),
        "the manifest links /assets/{sheet}"
    );
    let _ = std::fs::remove_dir_all(&app);
}
