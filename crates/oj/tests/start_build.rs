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
            && code.contains("/assets/index-"),
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

/// Where the client's chunks are written is the app's to say, as under
/// `vite build`: `output.chunkFileNames` of the client environment (a
/// function of the chunk here), then every plugin's `outputOptions` hook,
/// each taking the options the last one returned: a plugin wrapping
/// `chunkFileNames` to give some chunks a directory of their own.
const FILE_NAMES_PROBE: &str = r#"
import { writeFileSync as writeNames } from "node:fs";
function fileNamesProbe(): Plugin {
  let root = "";
  return {
    name: "file-names-probe",
    apply: "build",
    configResolved(c) {
      root = c.root;
    },
    outputOptions(options) {
      if (this.environment.name !== "client") return null;
      const original = options.chunkFileNames;
      writeNames(
        `${root}/output-options.json`,
        JSON.stringify({ entry: options.entryFileNames, chunk: typeof original }),
      );
      return {
        ...options,
        chunkFileNames: (chunk) => {
          const pattern = typeof original === "function" ? original(chunk) : original;
          return chunk.isDynamicEntry ? pattern.replace("assets/", "assets/lazy/") : pattern;
        },
      };
    },
  };
}
"#;

#[test]
fn chunk_file_names_come_from_the_config_and_the_output_options_hooks() {
    let Some(root) = app("chunk-file-names") else {
        return;
    };
    let config = root.join("vite.config.ts");
    let source = std::fs::read_to_string(&config).unwrap();
    let client = r#"    client: { define: { __FIXTURE_SIDE__: JSON.stringify("client-side") } },"#;
    assert!(
        source.contains(client),
        "the fixture's client environment moved"
    );
    let source = source
        .replacen(
            client,
            r#"    client: {
      define: { __FIXTURE_SIDE__: JSON.stringify("client-side") },
      build: {
        rolldownOptions: {
          output: { chunkFileNames: (chunk) => `assets/${chunk.name.replace(/^_/, "app-")}-[hash].js` },
        },
      },
    },"#,
            1,
        )
        .replacen("  plugins: [\n", "  plugins: [\n    fileNamesProbe(),\n", 1);
    std::fs::write(&config, format!("{source}{FILE_NAMES_PROBE}")).unwrap();
    build(&root);

    // The hook took Vite's entry pattern and the config's function.
    let options: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("output-options.json")).unwrap())
            .unwrap();
    assert_eq!(options["entry"], "assets/[name]-[hash].js", "{options}");
    assert_eq!(options["chunk"], "function", "{options}");

    let names = |dir: &str| -> Vec<String> {
        std::fs::read_dir(root.join(dir))
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    };
    // The fixture's lazy chunk (`_fixture-throws`): renamed by the config's
    // function, moved by the hook's.
    let lazy = names("dist/client/assets/lazy");
    assert!(
        lazy.iter()
            .any(|f| f.starts_with("app-fixture-throws-") && f.ends_with(".js")),
        "the lazy chunk under assets/lazy: {lazy:?}, beside {:?}",
        names("dist/client/assets")
    );
    // The entry keeps its place and imports the chunk where it was written.
    let assets = names("dist/client/assets");
    let entry = assets
        .iter()
        .find(|f| f.starts_with("index-") && f.ends_with(".js"))
        .unwrap_or_else(|| panic!("the client entry: {assets:?}"));
    let code = std::fs::read_to_string(root.join("dist/client/assets").join(entry)).unwrap();
    assert!(
        code.contains(&format!("./lazy/{}", lazy[0])),
        "the entry imports ./lazy/{}",
        lazy[0]
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
            .starts_with("assets/index-"),
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

    // The root route's stylesheet is the entry chunk's sheet, named after
    // the chunk as Vite's css-post names it, listed under the entry's `css`;
    // a sheet a chunk lists has no row of its own (Vite keys the asset by
    // the chunk's original file name, which the chunk's row holds).
    let css = entries[0]["css"].as_array().cloned().unwrap_or_default();
    assert!(
        css.len() == 1
            && css[0]
                .as_str()
                .is_some_and(|f| f.starts_with("assets/index-") && f.ends_with(".css")),
        "the entry lists its own sheet: {raw}"
    );
    assert!(
        !manifest.contains_key("styles/app.css"),
        "no row for a sheet a chunk lists:\n{raw}"
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

fn chunk_css_app(
    name: &str,
    top_level: &str,
    setup: impl FnOnce(&Path),
) -> Option<(PathBuf, serde_json::Value)> {
    let app = app(name)?;
    setup(&app);
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
    let Some((app, probe)) = chunk_css_app("chunk-css", "", |_| {}) else {
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
            p.file_name().is_some_and(|f| {
                f.to_string_lossy()
                    .starts_with("_tanstack-start-manifest_v-")
            })
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
    let Some((app, probe)) = chunk_css_app(
        "css-code-split-off",
        "  build: { cssCodeSplit: false },\n",
        |_| {},
    ) else {
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

/// The fixture as a Cloudflare app: inside a directory whose node_modules
/// holds @cloudflare/vite-plugin (e2e/fixtures/cloudflare-deps), with the
/// plugin ahead of Start's and the Worker environment named `ssr`, as
/// Cloudflare's Start guide sets it up. Returns (the directory, the app).
fn cloudflare_app(name: &str) -> Option<(PathBuf, PathBuf)> {
    let deps = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../e2e/fixtures/cloudflare-deps")
        .canonicalize()
        .unwrap();
    if !deps.join("node_modules/@cloudflare/vite-plugin").is_dir() {
        assert!(
            std::env::var_os("OJ_REQUIRE_FIXTURES").is_none(),
            "OJ_REQUIRE_FIXTURES is set but the cloudflare-deps fixture is not installed"
        );
        eprintln!("skipping: npm ci --prefix e2e/fixtures/cloudflare-deps");
        return None;
    }
    let source = app(name)?;
    let parent = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("oj-start-build-cf-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&parent);
    std::fs::create_dir_all(&parent).unwrap();
    std::os::unix::fs::symlink(deps.join("node_modules"), parent.join("node_modules")).unwrap();
    let root = parent.join("app");
    std::fs::rename(&source, &root).unwrap();
    std::fs::write(
        root.join("wrangler.jsonc"),
        r#"{"name":"oj-start-fixture","main":"@tanstack/react-start/server-entry","compatibility_date":"2025-09-01","compatibility_flags":["nodejs_compat"]}"#,
    )
    .unwrap();
    let config = std::fs::read_to_string(root.join("vite.config.ts")).unwrap();
    let config = config
        .replacen(
            "import { defineConfig } from \"vite\";",
            "import { defineConfig } from \"vite\";\nimport { cloudflare } from \"@cloudflare/vite-plugin\";",
            1,
        )
        .replacen(
            "    tanstackStart(",
            "    cloudflare({ viteEnvironment: { name: \"ssr\" } }),\n    tanstackStart(",
            1,
        );
    assert!(
        config.contains("cloudflare("),
        "the fixture's vite.config.ts changed shape; update this test"
    );
    std::fs::write(root.join("vite.config.ts"), config).unwrap();
    Some((parent, root))
}

/// Start's config hook declares its server environment's output directory,
/// `<build.outDir>/server`, and @cloudflare/vite-plugin (which would name the
/// directory after the environment, `ssr`) keeps a declared one: the Worker
/// is written to `dist/server`, where `vite build` puts it, the plugins see
/// that directory in the resolved config, and the deploy config points at it.
#[test]
fn a_cloudflare_worker_is_written_to_starts_server_directory() {
    let Some((parent, root)) = cloudflare_app("worker-dir") else {
        return;
    };
    let config = std::fs::read_to_string(root.join("vite.config.ts")).unwrap();
    let config = config.replacen(
        "    cloudflare(",
        "    { name: \"reads-out-dirs\", configResolved(c) { process.getBuiltinModule(\"node:fs\").writeFileSync(\"out-dirs.json\", JSON.stringify([c.environments.client.build.outDir, c.environments.ssr.build.outDir, process.env.VITE_CONFIG_NATIVE_IGNORE_WARNING ?? null])); } },\n    cloudflare(",
        1,
    );
    std::fs::write(root.join("vite.config.ts"), config).unwrap();
    build(&root);

    for file in ["index.js", "wrangler.json"] {
        assert!(
            root.join("dist/server").join(file).is_file(),
            "dist/server/{file} is written"
        );
    }
    assert!(
        !root.join("dist/ssr").exists(),
        "nothing is written under the environment's name"
    );
    assert!(
        root.join("dist/client/assets").is_dir(),
        "the client stays in dist/client"
    );
    // The plugin declares `preserveEntrySignatures: "strict"`, which rolldown
    // 1.3 (Vite 8.4's) enforces with a facade: `index.js` re-exports the
    // Worker's own chunk, `assets/worker-entry-*.js`, as `vite build` writes
    // it (Vite 8.2's rolldown 1.2 wrote the chunk itself as `index.js`).
    // This fixture's Worker needs no facade (nothing imports the entry's
    // exports), so `index.js` is the chunk itself either way.
    let index = std::fs::read_to_string(root.join("dist/server/index.js")).unwrap();
    assert!(
        index.len() > 1024 && !root.join("dist/ssr").exists(),
        "index.js is the Worker, under dist/server:\n{}",
        &index[..index.len().min(300)]
    );
    // Vite's `build.manifest`, which the plugin turns on for the entry Worker.
    let raw = std::fs::read_to_string(root.join("dist/server/.vite/manifest.json"))
        .expect("dist/server/.vite/manifest.json is written for the Worker environment");
    let manifest: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&raw).unwrap();
    let entries: Vec<_> = manifest.values().filter(|r| r["isEntry"] == true).collect();
    assert!(
        entries.len() == 1 && entries[0]["file"] == "index.js",
        "one entry row, the Worker:\n{raw}"
    );
    assert!(
        manifest.values().all(|r| r["file"]
            .as_str()
            .is_some_and(|f| root.join("dist/server").join(f).is_file())),
        "every row names a file of dist/server:\n{raw}"
    );
    // The Worker environment's `build.emitAssets` (the plugin turns it on):
    // the Worker's chunks write their stylesheets under dist/server/assets,
    // named after the chunk as the client's are, and the manifest's entry
    // row lists the entry's; a Node server build writes none.
    let server_css: Vec<String> = std::fs::read_dir(root.join("dist/server/assets"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|f| f.ends_with(".css"))
        .collect();
    assert!(
        !server_css.is_empty(),
        "the Worker's chunks write their sheets under dist/server/assets"
    );
    assert!(
        manifest
            .values()
            .any(|r| r["css"].as_array().is_some_and(|c| c.iter().any(|f| f
                .as_str()
                .is_some_and(|f| f.starts_with("assets/") && f.ends_with(".css"))))),
        "a Worker chunk's manifest row lists its sheet:\n{raw}"
    );
    // The plugin's client generateBundle emits `.assetsignore` (the public
    // directory's, plus the two files Workers Assets must not serve); it lands
    // over the public copy, as Vite copies `public/` before the bundle writes.
    let ignore = std::fs::read_to_string(root.join("dist/client/.assetsignore"))
        .expect("dist/client/.assetsignore is written");
    assert_eq!(ignore, "wrangler.json\n.dev.vars\n");
    let deploy = std::fs::read_to_string(root.join(".wrangler/deploy/config.json")).unwrap();
    assert!(
        deploy.contains("dist/server/wrangler.json"),
        "wrangler deploys the Worker from dist/server: {deploy}"
    );
    let worker: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("dist/server/wrangler.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        worker["assets"]["directory"], "../client",
        "the Worker serves the client's files"
    );
    let seen = std::fs::read_to_string(root.join("out-dirs.json")).unwrap();
    assert_eq!(
        seen, r#"["dist/client","dist/server",null]"#,
        "the resolved config names both directories, and oj's own environment (the Vite warning switch it used to set) stays out of the config's: `loadEnv(mode, root, \"\")` would hand it to the Worker's wrangler.json vars"
    );
    let _ = std::fs::remove_dir_all(&parent);
}

/// The top-level `build.outDir` moves Start's default along.
#[test]
fn a_cloudflare_worker_follows_the_top_level_out_dir() {
    let Some((parent, root)) = cloudflare_app("worker-out-dir") else {
        return;
    };
    let original = std::fs::read_to_string(root.join("vite.config.ts")).unwrap();
    let moved = original.replacen(
        "export default defineConfig({",
        "export default defineConfig({\n  build: { outDir: \"out\" },",
        1,
    );
    assert_ne!(
        moved, original,
        "the fixture's vite.config.ts changed shape"
    );
    std::fs::write(root.join("vite.config.ts"), moved).unwrap();
    build(&root);
    assert!(
        root.join("out/server/index.js").is_file() && root.join("out/client/assets").is_dir(),
        "build.outDir holds both environments"
    );
    assert!(!root.join("dist").exists(), "nothing lands in dist");
    let _ = std::fs::remove_dir_all(&parent);
}

/// The client's chunks read `process.env` as `vite build` writes them: Vite's
/// define plugin makes `process.env` an empty object with NODE_ENV its one
/// key, Start's plugin defines the server-fn base, and no chunk opens with a
/// `globalThis.process` banner (oj's chunks did; a chunk of `vite build`
/// carries none, so every chunk differed from Vite's by that line).
#[test]
fn a_client_chunk_defines_process_env_as_vite_does() {
    let Some(root) = app("process-env") else {
        return;
    };
    build(&root);
    let assets = root.join("dist/client/assets");
    let mut chunks = 0;
    let mut fn_base = false;
    for entry in std::fs::read_dir(&assets).unwrap().flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "js") {
            continue;
        }
        let code = std::fs::read_to_string(&path).unwrap();
        chunks += 1;
        assert!(
            !code.contains("globalThis.process="),
            "{}: no process banner",
            path.display()
        );
        assert!(
            !code.contains("process.env.TSS_SERVER_FN_BASE"),
            "{}: the server-fn base is defined, not read",
            path.display()
        );
        fn_base |= code.contains("/_serverFn/");
    }
    assert!(
        chunks > 0 && fn_base,
        "a chunk carries the defined server-fn base ({chunks} chunks)"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `build.minify: "esbuild"`: each chunk is minified by esbuild in renderChunk
/// (Vite's vite:esbuild-transpile, after every other plugin's), rolldown's own
/// minifier off, so a plugin's renderChunk sees the chunk as rolldown wrote it,
/// and no annotation comment survives (Vite's output options drop them).
#[test]
fn minify_esbuild_minifies_each_chunk_in_render_chunk() {
    let Some(root) = app("minify-esbuild") else {
        return;
    };
    let config = root.join("vite.config.ts");
    let source = std::fs::read_to_string(&config).unwrap();
    let probe = r#"    react(),
    {
      name: "render-chunk-probe",
      enforce: "post",
      renderChunk(code, chunk) {
        if (this.environment.name === "client" && chunk.isEntry) writeSeen("render-chunk-seen.txt", code);
        return null;
      },
    },"#;
    let source = source
        .replacen(
            "export default defineConfig({",
            "import { writeFileSync as writeSeen } from \"node:fs\";\nexport default defineConfig({\n  build: { minify: \"esbuild\" },",
            1,
        )
        .replacen("    react(),", probe, 1);
    assert!(
        source.contains("render-chunk-probe") && source.contains("minify: \"esbuild\""),
        "the fixture's vite.config.ts changed shape; update this test"
    );
    std::fs::write(&config, source).unwrap();
    build(&root);

    let seen = std::fs::read_to_string(root.join("render-chunk-seen.txt"))
        .expect("the probe saw the client entry chunk");
    assert!(
        seen.lines().count() > 20,
        "a post renderChunk hook sees the chunk before esbuild minifies it:\n{}",
        &seen[..seen.len().min(300)]
    );
    let entry = std::fs::read_dir(root.join("dist/client/assets"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name().is_some_and(|f| {
                let f = f.to_string_lossy();
                f.starts_with("index-") && f.ends_with(".js")
            })
        })
        .expect("the client entry chunk");
    let code = std::fs::read_to_string(&entry).unwrap();
    // The hook saw rolldown's unminified output (spaced assignments by the
    // thousand); the written chunk is esbuild's (no spaced assignment, no
    // annotation comment, and `const` kept, which rolldown's minifier lowers).
    assert!(
        seen.matches(" = ").count() > 100,
        "the hook saw unminified code: {}",
        &seen[..seen.len().min(300)]
    );
    assert!(
        code.len() > 200
            && code.len() * 10 < seen.len() * 7
            && !code.contains(" = ")
            && !code.contains("/* @__PURE__ */")
            && code.contains("const "),
        "esbuild minified the written entry chunk ({} bytes from {}):\n{}",
        code.len(),
        seen.len(),
        &code[..code.len().min(300)]
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The client bundle enters at the app's own Start client entry (`src/client.tsx`
/// by convention, or `tanstackStart({ client: { entry } })`), as Vite's does
/// through `virtual:tanstack-start-client-entry`: its code is in the entry
/// chunk, named `index-<hash>.js` after Start's input, and the manifest keys
/// the entry by the app's file. oj entered at its own stub, so an app's
/// polyfills and boot-time setup were missing from the build.
#[test]
fn the_client_bundle_enters_at_the_apps_client_entry() {
    let Some(root) = app("client-entry") else {
        return;
    };
    std::fs::write(
        root.join("src/client.tsx"),
        r#"import { StartClient } from "@tanstack/react-start/client";
import { StrictMode, startTransition } from "react";
import { hydrateRoot } from "react-dom/client";
(globalThis as { __fixtureClientEntry?: string }).__fixtureClientEntry = "fixture-client-entry-ran";
startTransition(() => {
  hydrateRoot(document, <StrictMode><StartClient /></StrictMode>);
});
"#,
    )
    .unwrap();
    build(&root);
    let entry = std::fs::read_dir(root.join("dist/client/assets"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name().is_some_and(|f| {
                let f = f.to_string_lossy();
                f.starts_with("index-") && f.ends_with(".js")
            })
        })
        .expect("the client entry chunk, index-<hash>.js");
    let code = std::fs::read_to_string(&entry).unwrap();
    assert!(
        code.contains("fixture-client-entry-ran"),
        "the app's client entry is the bundle's entry: {}",
        entry.display()
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Vite's css code splitting on the Start client: a lazy route's stylesheet
/// is one file named after the route's chunk (css-post's renderChunk), the
/// entry's after the entry (`index-<hash>.css`); the root route links the
/// entry's alone, the route's chunk lists its own under `viteMetadata` and
/// the manifest's `css`; a chunk holding nothing but a shared sheet (a pure
/// CSS chunk) is dropped, its importers taking the sheet; and every dynamic
/// `import()` is wrapped in Vite's preload helper with a `__vite__mapDeps`
/// table that names the lazy chunk's stylesheets. oj wrote one sheet per CSS
/// module and linked them all on the root, with no preload helper.
/// Two routes lazily load a widget each; the widgets import a sheet of their
/// own (about's) and one they share, so the shared sheet lands in a chunk of
/// its own with no JS (a pure CSS chunk).
fn lazy_css_widgets(app: &Path) {
    let lazy = |route: &str, widget: &str, marker: &str| {
        let path = app.join("src/routes").join(route);
        let source = std::fs::read_to_string(&path).unwrap();
        let source = source
            .replacen(
                "import { createRoute } from \"@tanstack/react-router\";\n",
                &format!(
                    "import {{ createRoute }} from \"@tanstack/react-router\";\nimport {{ lazy, Suspense }} from \"react\";\nconst Widget = lazy(() => import(\"../{widget}\"));\n"
                ),
                1,
            )
            .replacen(
                marker,
                &format!("{marker}\n      <Suspense fallback={{null}}><Widget /></Suspense>"),
                1,
            );
        assert!(
            source.contains("const Widget") && source.contains("<Widget />"),
            "the fixture's {route} changed shape; update this test"
        );
        std::fs::write(&path, source).unwrap();
    };
    lazy(
        "about.tsx",
        "about-widget",
        "<h1 className=\"fixture-heading\">about-page-marker</h1>",
    );
    lazy(
        "user.tsx",
        "user-widget",
        "<h1 className=\"fixture-heading\">{`user-${id}-marker`}</h1>",
    );
    std::fs::write(
        app.join("src/about.css"),
        ".about-widget { color: teal; }\n",
    )
    .unwrap();
    std::fs::write(
        app.join("src/shared.css"),
        ".shared-widget { margin: 1px; }\n",
    )
    .unwrap();
    std::fs::write(
        app.join("src/about-widget.tsx"),
        "import \"./about.css\";\nimport \"./shared.css\";\nexport default function AboutWidget() {\n  return <p className=\"about-widget shared-widget\">about-widget</p>;\n}\n",
    )
    .unwrap();
    std::fs::write(
        app.join("src/user-widget.tsx"),
        "import \"./shared.css\";\nexport default function UserWidget() {\n  return <p className=\"shared-widget\">user-widget</p>;\n}\n",
    )
    .unwrap();
}

#[test]
fn a_lazy_routes_stylesheet_is_named_after_its_chunk_and_preloaded() {
    let Some((app, probe)) = chunk_css_app(
        "lazy-css",
        "  build: { manifest: true },\n",
        lazy_css_widgets,
    ) else {
        return;
    };
    let assets = app.join("dist/client/assets");
    let names: Vec<String> = std::fs::read_dir(&assets)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let find = |prefix: &str, ext: &str| {
        names
            .iter()
            .find(|n| n.starts_with(prefix) && n.ends_with(ext))
            .cloned()
    };
    let index_css = find("index-", ".css").expect("the entry's sheet, index-<hash>.css");
    let about_css = find("about-widget-", ".css").unwrap_or_else(|| {
        panic!("the about widget's sheet, about-widget-<hash>.css, among {names:?}")
    });
    let shared_css = find("shared-", ".css").unwrap_or_else(|| {
        panic!("the shared sheet of two lazy routes, shared-<hash>.css, among {names:?}")
    });
    assert!(
        find("shared-", ".js").is_none(),
        "the pure CSS chunk holding shared.css is dropped: {names:?}"
    );
    assert!(
        find("app-", ".css").is_none(),
        "no sheet is named after a CSS module: {names:?}"
    );

    // The about chunk took the pure chunk's sheet: no import of the dropped
    // chunk remains in it, and its generateBundle metadata lists both sheets.
    let about_js = find("about-widget-", ".js").expect("the about widget's chunk");
    let about_code = std::fs::read_to_string(assets.join(&about_js)).unwrap();
    assert!(
        !about_code.contains("/shared-"),
        "the import of the dropped chunk is gone from {about_js}:\n{}",
        &about_code[..about_code.len().min(400)]
    );
    let css_of = |chunk: &str| -> Vec<String> {
        probe["css"][format!("assets/{chunk}")]
            .as_array()
            .map(|c| {
                c.iter()
                    .filter_map(|f| f.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let about_sheets = css_of(&about_js);
    assert!(
        about_sheets.contains(&format!("assets/{about_css}")),
        "the about chunk lists its own sheet: {about_sheets:?}\n{probe}"
    );

    // The manifest: the about chunk's row lists its sheet; the entry's its own.
    let raw = std::fs::read_to_string(app.join("dist/client/.vite/manifest.json")).unwrap();
    let manifest: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&raw).unwrap();
    let about_row = manifest
        .values()
        .find(|r| r["file"] == format!("assets/{about_js}"))
        .unwrap_or_else(|| panic!("a manifest row for {about_js}:\n{raw}"));
    assert!(
        about_row["css"]
            .as_array()
            .is_some_and(|c| c.iter().any(|f| f == &format!("assets/{about_css}"))),
        "the about row lists its sheet: {about_row}"
    );

    // The root links the entry's sheet alone: a lazy widget's sheet arrives
    // with the widget, through the preload helper.
    let served = served_manifest(&app);
    assert_eq!(
        served["routes"]["__root__"]["css"],
        serde_json::json!([format!("/assets/{index_css}")]),
        "{served}"
    );

    // Vite's preload helper: the chunk that imports the about route lazily
    // carries a `__vite__mapDeps` table naming the about chunk and its sheets.
    let importer = std::fs::read_dir(&assets)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "js"))
        .map(|p| std::fs::read_to_string(p).unwrap())
        .find(|code| code.contains("__vite__mapDeps") && code.contains(&about_js))
        .expect("a chunk preloads the about route's chunk through __vite__mapDeps");
    assert!(
        importer.contains(&about_css) && importer.contains(&shared_css),
        "the preload list names the about route's sheets"
    );
    let _ = std::fs::remove_dir_all(&app);
}

/// A stylesheet imported with `?url` is compiled as Vite's css plugin compiles
/// any sheet: its `@import`s inlined (postcss-import) and its urls rewritten to
/// the emitted assets. oj emitted a plain `.css` `?url` sheet as written, so a
/// fonts sheet importing a package's `@font-face` rules shipped the bare
/// `@import`, which 404s from `assets/`, and its fonts were never emitted.
#[test]
fn a_url_sheets_imports_are_inlined_like_vites() {
    let Some(root) = app("url-sheet-imports") else {
        return;
    };
    std::fs::write(
        root.join("styles/fonts.css"),
        "@import \"./fonts-part.css\";\n\n.fonts-root { color: red }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("styles/fonts-part.css"),
        ".fonts-part { background: url(../src/star.svg) }\n",
    )
    .unwrap();
    let route = root.join("src/routes/__root.tsx");
    let source = std::fs::read_to_string(&route).unwrap();
    let source = source
        .replacen(
            "import \"../../styles/app.css\";",
            "import \"../../styles/app.css\";\nimport fontsUrl from \"../../styles/fonts.css?url\";",
            1,
        )
        .replacen(
            "    meta: [{ title: \"oj start fixture\" }],",
            "    meta: [{ title: \"oj start fixture\" }],\n    links: [{ rel: \"stylesheet\", href: fontsUrl }],",
            1,
        );
    assert!(
        source.contains("fontsUrl }"),
        "the fixture's __root.tsx changed shape; update this test"
    );
    std::fs::write(&route, source).unwrap();
    build(&root);

    let sheet = std::fs::read_dir(root.join("dist/client/assets"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name().is_some_and(|f| {
                let f = f.to_string_lossy();
                f.starts_with("fonts-") && f.ends_with(".css")
            })
        })
        .expect("the ?url sheet is emitted as fonts-<hash>.css");
    let css = std::fs::read_to_string(&sheet).unwrap();
    assert!(
        !css.contains("@import"),
        "the sheet's @import was not inlined:\n{css}"
    );
    assert!(
        css.contains(".fonts-part") && css.contains(".fonts-root"),
        "the imported sheet's rules are missing:\n{css}"
    );
    // A small svg inlines as its data URL, as Vite's build inlines it.
    assert!(
        css.contains("url(\"data:image/svg+xml,") || css.contains("url(data:image/svg+xml,"),
        "the imported sheet's url was not rewritten as Vite's build rewrites it:\n{css}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A worker bundle under `build.minify: "esbuild"` comes out as Vite writes
/// it: Vite's bundleWorkerEntry hands rolldown no `minify` (its default,
/// `'dce-only'`), vite:esbuild-transpile mangles and compresses the chunk in
/// renderChunk, and rolldown's DCE pass prints the result again with its own
/// whitespace, so the worker keeps its lines (tab-indented) while the page's
/// chunks lose theirs. oj minified the worker in full, 20% to 30% smaller
/// than Vite's.
#[test]
fn a_worker_under_esbuild_minify_keeps_rolldowns_lines() {
    let Some(root) = app("worker-esbuild-minify") else {
        return;
    };
    std::fs::write(
        root.join("src/wk.ts"),
        "class CaptureProcessor {\n  batchOfSamples = new Float32Array(1024);\n  filledSamples = 0;\n  process(inputChannels: Float32Array[]) {\n    const frameCount = inputChannels[0]?.length ?? 0;\n    for (let frameIndex = 0; frameIndex < frameCount; frameIndex++) {\n      let summed = 0;\n      for (const channel of inputChannels) summed += channel[frameIndex];\n      this.batchOfSamples[this.filledSamples++] = summed / inputChannels.length;\n    }\n    return true;\n  }\n}\nself.onmessage = () => { postMessage(new CaptureProcessor().process([new Float32Array(2)])); };\n",
    )
    .unwrap();
    let route = root.join("src/routes/__root.tsx");
    let source = std::fs::read_to_string(&route).unwrap();
    let source = source
        .replacen(
            "import \"../../styles/app.css\";",
            "import \"../../styles/app.css\";\nimport wkUrl from \"../wk.ts?worker&url\";",
            1,
        )
        .replacen(
            "    meta: [{ title: \"oj start fixture\" }],",
            "    meta: [{ title: \"oj start fixture\" }, { name: \"wk\", content: wkUrl }],",
            1,
        );
    assert!(
        source.contains("content: wkUrl"),
        "the fixture's __root.tsx changed shape; update this test"
    );
    std::fs::write(&route, source).unwrap();
    let config = root.join("vite.config.ts");
    let vite = std::fs::read_to_string(&config).unwrap().replacen(
        "export default defineConfig({",
        "export default defineConfig({\n  build: { minify: \"esbuild\" },",
        1,
    );
    assert!(
        vite.contains("minify: \"esbuild\""),
        "the fixture's vite.config.ts changed shape"
    );
    std::fs::write(&config, vite).unwrap();
    build(&root);

    let chunk = |prefix: &str| {
        std::fs::read_dir(root.join("dist/client/assets"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                p.file_name().is_some_and(|f| {
                    let f = f.to_string_lossy();
                    f.starts_with(prefix) && f.ends_with(".js")
                })
            })
            .unwrap_or_else(|| panic!("a {prefix}*.js chunk"))
    };
    let worker = std::fs::read_to_string(chunk("wk-")).unwrap();
    assert!(
        worker.contains("\n\t") && worker.lines().count() > 8,
        "the worker chunk lost its lines (rolldown prints esbuild's result again):\n{worker}"
    );
    assert!(
        !worker.contains("frameIndex") && worker.contains("!0"),
        "the worker chunk is not mangled and compressed by esbuild:\n{worker}"
    );
    let entry = std::fs::read_to_string(chunk("index-")).unwrap();
    assert!(
        !entry.contains("\n\t"),
        "the page's chunks keep esbuild's whitespace minification:\n{}",
        &entry[..entry.len().min(400)]
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The preload helper module (`\0vite/preload-helper.js`) is the one the
/// app's own Vite composes, read off its vite:build-import-analysis plugin,
/// so a chunk carries the helper of the Vite the app runs (a patched Vite's
/// included); oj wrote its own copy, taken from a later Vite than the
/// fixture's.
#[test]
fn the_preload_helper_is_the_apps_vites() {
    let Some(root) = app("preload-helper") else {
        return;
    };
    let config = root.join("vite.config.ts");
    let vite = std::fs::read_to_string(&config).unwrap().replacen(
        "export default defineConfig({",
        "export default defineConfig({\n  build: { minify: false },",
        1,
    );
    assert!(
        vite.contains("minify: false"),
        "the fixture's vite.config.ts changed shape"
    );
    std::fs::write(&config, vite).unwrap();
    build(&root);
    // The fixture's Vite (8.2.1) scans the document's links itself; oj's copy
    // of the helper (a later Vite's) keeps a set of preloaded hrefs instead.
    let entry = std::fs::read_dir(root.join("dist/client/assets"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "js"))
        .find_map(|p| {
            let code = std::fs::read_to_string(&p).ok()?;
            code.contains("__vitePreload = ").then_some(code)
        })
        .expect("a client chunk holds the preload helper");
    let at = entry.find("__vitePreload = ").unwrap_or(0);
    assert!(
        entry.contains("getElementsByTagName(\"link\")") && !entry.contains("preloadedHrefs"),
        "the helper is not the fixture Vite's own:\n{}",
        &entry[at..(at + 600).min(entry.len())]
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A private copy of the StyleX Start fixture (e2e/fixtures/start-stylex-app)
/// sharing its installed `node_modules`; None (after saying why) when the
/// fixture is not installed.
fn stylex_app(name: &str) -> Option<PathBuf> {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../e2e/fixtures/start-stylex-app")
        .canonicalize()
        .unwrap();
    if !fixture.join("node_modules/@stylexjs/unplugin").is_dir() {
        assert!(
            std::env::var_os("OJ_REQUIRE_FIXTURES").is_none(),
            "OJ_REQUIRE_FIXTURES is set but the start-stylex-app fixture is not installed"
        );
        eprintln!("skipping: npm ci --prefix e2e/fixtures/start-stylex-app");
        return None;
    }
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("oj-start-stylex-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    fn copy(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap().flatten() {
            let file_name = entry.file_name();
            if matches!(
                file_name.to_str(),
                Some("node_modules" | "dist" | ".oj-cache")
            ) {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                copy(&path, &to.join(&file_name));
            } else {
                std::fs::copy(&path, to.join(&file_name)).unwrap();
            }
        }
    }
    copy(&fixture, &root);
    std::os::unix::fs::symlink(fixture.join("node_modules"), root.join("node_modules")).unwrap();
    Some(root)
}

/// The StyleX plugin's `cssInjectionTarget` picks the stylesheet its rules
/// join, as @stylexjs/unplugin's generateBundle asks it over the bundle's
/// sheets; oj took the plugin's default pick (the first `index-*.css`, else
/// `style-*.css`, else the first sheet by name), so an app sending its rules
/// to one sheet found them in another.
#[test]
fn stylex_rules_join_the_sheet_css_injection_target_picks() {
    let Some(root) = stylex_app("injection-target") else {
        return;
    };
    let config = root.join("vite.config.ts");
    let source = std::fs::read_to_string(&config).unwrap().replacen(
        "stylex.vite()",
        "stylex.vite({ cssInjectionTarget: (fileName: string) => /(^|\\/)zeta-[\\w-]+\\.css$/.test(fileName) })",
        1,
    );
    assert!(
        source.contains("cssInjectionTarget"),
        "the fixture's vite.config.ts changed shape"
    );
    std::fs::write(&config, source).unwrap();
    std::fs::write(root.join("src/zeta.css"), ".zeta-marker { color: teal }\n").unwrap();
    let route = root.join("src/routes/__root.tsx");
    let source = std::fs::read_to_string(&route).unwrap();
    let source = source
        .replacen(
            "import appCss from \"../styles.css?url\";",
            "import appCss from \"../styles.css?url\";\nimport zetaCss from \"../zeta.css?url\";",
            1,
        )
        .replacen(
            "links: [{ rel: \"stylesheet\", href: appCss }, ...devLinks],",
            "links: [{ rel: \"stylesheet\", href: appCss }, { rel: \"stylesheet\", href: zetaCss }, ...devLinks],",
            1,
        );
    assert!(
        source.contains("href: zetaCss"),
        "the fixture's __root.tsx changed shape"
    );
    std::fs::write(&route, source).unwrap();
    build(&root);

    let sheets: Vec<(String, String)> = std::fs::read_dir(root.join("dist/client/assets"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "css"))
        .map(|p| {
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                std::fs::read_to_string(&p).unwrap(),
            )
        })
        .collect();
    let extra = sheets
        .iter()
        .find(|(name, _)| name.starts_with("zeta-"))
        .expect("the ?url sheet zeta-<hash>.css");
    // Card.tsx's `stylex.keyframes` (opacity 0 to 1), as the plugin's sheet writes it.
    assert!(
        extra.1.contains("@keyframes")
            && extra.1.contains("opacity")
            && extra.1.contains(".zeta-marker"),
        "the StyleX rules did not join the sheet cssInjectionTarget picked:\n{}",
        &extra.1[..extra.1.len().min(400)]
    );
    for (name, css) in &sheets {
        assert!(
            name.starts_with("zeta-") || !css.contains("@keyframes"),
            "the StyleX rules also landed in {name}"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// An app that makes a plugin of its own from `@stylexjs/unplugin`'s
/// (spreading the plugin's hooks under a name of its own, as Lovable's
/// two-instance split build does) runs under oj as under Vite: oj's
/// stand-in for the package carries the real plugin's hooks over its shared
/// rule store, with oj's compiler as the `transform`, and the host runs
/// them. The plugin's generateBundle then appends the rules to the sheet it
/// picks and re-emits it under a new hash, the old file gone and every
/// reference (chunk code, the manifests) moved to the new name. oj's
/// stand-in had no hooks, so such an app fell back to one unsplit instance
/// (other class prefixes and layers: other chunk sizes, other chunk cuts).
#[test]
fn an_apps_own_plugin_made_of_stylexs_runs_the_real_hooks() {
    let Some(root) = stylex_app("own-plugin") else {
        return;
    };
    let config = root.join("vite.config.ts");
    let source = std::fs::read_to_string(&config)
        .unwrap()
        .replacen(
            "export default defineConfig({",
            "const upstream = stylex.vite({ classNamePrefix: \"zz\" });\n\
             const own = {\n\
             \x20 ...upstream,\n\
             \x20 name: \"app-stylex\",\n\
             \x20 transform(code: string, id: string) {\n\
             \x20   return (upstream.transform as Function).call(this, code, id);\n\
             \x20 },\n\
             };\n\
             export default defineConfig({",
            1,
        )
        .replacen("stylex.vite()", "own", 1);
    assert!(
        source.contains("plugins: [own,"),
        "the fixture's vite.config.ts changed shape"
    );
    std::fs::write(&config, source).unwrap();
    build(&root);

    let assets = root.join("dist/client/assets");
    let sheets: Vec<String> = std::fs::read_dir(&assets)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("styles-") && name.ends_with(".css"))
        .collect();
    assert_eq!(
        sheets.len(),
        1,
        "one styles-<hash>.css, the re-emitted sheet, not also the one it replaced: {sheets:?}"
    );
    let sheet = &sheets[0];
    let css = std::fs::read_to_string(assets.join(sheet)).unwrap();
    assert!(
        css.contains("@keyframes") && css.contains(".zz"),
        "the real plugin's generateBundle did not append the rules, under the app's prefix, to {sheet}:\n{}",
        &css[..css.len().min(400)]
    );
    assert!(
        !assets.join("stylex.css").exists(),
        "the plugin's writeBundle fallback sheet was written"
    );
    // Every mention of the sheet in the output (the chunk importing it
    // `?url`, both manifests, the server's copy of the client manifest)
    // names the re-emitted file.
    let mentions = regex::Regex::new(r"styles-[\w-]+\.css").unwrap();
    let mut files = Vec::new();
    fn walk(dir: &Path, files: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, files);
            } else {
                files.push(path);
            }
        }
    }
    walk(&root.join("dist"), &mut files);
    let mut seen = 0;
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for mention in mentions.find_iter(&text) {
            seen += 1;
            assert_eq!(
                mention.as_str(),
                sheet,
                "{} names a sheet the build replaced",
                file.display()
            );
        }
    }
    assert!(seen >= 2, "the chunk and the manifest name the sheet");
    let _ = std::fs::remove_dir_all(&root);
}

/// A module a plugin resolves by passing its `this.resolve` answer on keeps
/// the package.json that answer names, as under rolldown, so its format
/// stays what `"type": "module"` makes it: its CommonJS imports take the
/// node-mode interop (`__toESM(require_x(), 1)`). oj's host dropped the
/// package.json from such answers, and every module resolved that way lost
/// the flag (`__toESM(require_x())`), one byte off Vite's in every chunk.
#[test]
fn a_plugin_resolved_module_keeps_its_packages_format() {
    let Some(root) = app("plugin-resolved-format") else {
        return;
    };
    let config = root.join("vite.config.ts");
    let source = std::fs::read_to_string(&config).unwrap();
    let probe = r#"    react(),
    {
      name: "resolve-through-this",
      enforce: "pre",
      async resolveId(source, importer) {
        if (!importer || !source.endsWith("./routes/index")) return null;
        return await this.resolve(source, importer, { skipSelf: true });
      },
    },"#;
    let source = source
        .replacen(
            "export default defineConfig({",
            "export default defineConfig({\n  build: { minify: false },",
            1,
        )
        .replacen("    react(),", probe, 1);
    assert!(
        source.contains("resolve-through-this") && source.contains("minify: false"),
        "the fixture's vite.config.ts changed shape; update this test"
    );
    std::fs::write(&config, source).unwrap();
    build(&root);

    // The index route imports `legacy-cjs` (CommonJS); its chunk reads the
    // module through the node-mode interop.
    let chunk = std::fs::read_dir(root.join("dist/client/assets"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "js"))
        .find_map(|p| {
            let code = std::fs::read_to_string(&p).ok()?;
            code.contains("__toESM(require_").then_some(code)
        })
        .expect("a client chunk imports a CommonJS module");
    let interop: Vec<&str> = chunk
        .match_indices("__toESM(require_")
        .map(|(at, _)| &chunk[at..chunk[at..].find(';').map_or(chunk.len(), |e| at + e)])
        .collect();
    assert!(
        interop.iter().all(|call| call.ends_with(", 1)")),
        "a CommonJS import lost the node-mode interop flag: {interop:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A chunk's sourcemap names the app's code, not the stylesheets, `?raw`
/// text or asset URLs it imports: Vite's asset, worker and CSS plugins hand
/// rolldown an empty map (`{ mappings: "" }`) for the module they produce,
/// and rolldown lists no source for such a module. oj's loaders returned no
/// map, so every sheet and asset text travelled in `sources` and
/// `sourcesContent`.
#[test]
fn a_chunks_sourcemap_names_no_stylesheet_or_asset_module() {
    let Some(root) = app("sourcemap-sources") else {
        return;
    };
    let config = root.join("vite.config.ts");
    let source = std::fs::read_to_string(&config).unwrap();
    assert!(source.contains("export default defineConfig({\n"));
    std::fs::write(
        &config,
        source.replacen(
            "export default defineConfig({\n",
            "export default defineConfig({\n  build: { sourcemap: true },\n",
            1,
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("src/content/sourcemap-note.txt"),
        "a note the chunk inlines\n",
    )
    .unwrap();
    let route = root.join("src/routes/__root.tsx");
    let source = std::fs::read_to_string(&route).unwrap();
    let source = source
        .replacen(
            "import \"../../styles/app.css\";",
            "import \"../../styles/app.css\";\nimport sourcemapNote from \"../content/sourcemap-note.txt?raw\";\nimport sourcemapStar from \"../star.svg?url\";",
            1,
        )
        .replacen(
            "    meta: [{ title: \"oj start fixture\" }],",
            "    meta: [{ title: \"oj start fixture\" }, { name: \"x-note\", content: sourcemapNote }, { name: \"x-star\", content: sourcemapStar }],",
            1,
        );
    assert!(
        source.contains("content: sourcemapStar }"),
        "the fixture's __root.tsx changed shape; update this test"
    );
    std::fs::write(&route, source).unwrap();
    build(&root);

    let maps: Vec<(PathBuf, serde_json::Value)> =
        std::fs::read_dir(root.join("dist/client/assets"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "map"))
            .map(|p| {
                let map = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
                (p, map)
            })
            .collect();
    assert!(!maps.is_empty(), "build.sourcemap: true writes .map files");
    let sources = |map: &serde_json::Value| -> Vec<String> {
        map["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect()
    };
    let (root_map, root_sources) = maps
        .iter()
        .map(|(p, m)| (p, sources(m)))
        .find(|(_, s)| s.iter().any(|s| s.ends_with("src/routes/__root.tsx")))
        .expect("a chunk's map names the root route as a source");
    let stray: Vec<&String> = root_sources
        .iter()
        .filter(|s| s.ends_with(".css") || s.contains("?raw") || s.contains("?url"))
        .collect();
    assert!(
        stray.is_empty(),
        "{} lists asset-like modules as sources, which Vite's maps never do: {stray:?}",
        root_map.display()
    );
    for (p, map) in &maps {
        let stray: Vec<String> = sources(map)
            .into_iter()
            .filter(|s| s.ends_with(".css") || s.contains("?raw") || s.contains("?url"))
            .collect();
        assert!(stray.is_empty(), "{} lists {stray:?}", p.display());
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// A module a package's `sideEffects` glob covers is still dropped when it
/// is pure and imported for nothing, as `vite build` drops it: Vite's
/// resolver answers every resolution with the package's verdict as
/// `moduleSideEffects`, and rolldown takes a `true` as "analyze the module",
/// not as a side effect in itself. oj's answers carried no verdict, so
/// rolldown read the field itself and kept the module, and the importing
/// chunk kept an `import "./other-chunk.js"` for it (Lovable's StyleX tokens
/// under `**/*.stylex.ts`). A module with real side effects stays either way.
#[test]
fn a_pure_module_a_packages_side_effects_glob_covers_drops_out_as_in_vite() {
    let Some(root) = app("side-effects-glob") else {
        return;
    };
    let pkg = root.join("fxpkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("package.json"),
        "{ \"name\": \"fxpkg\", \"private\": true, \"sideEffects\": [\"**/*.fx.js\"] }\n",
    )
    .unwrap();
    std::fs::write(
        pkg.join("tokens.fx.js"),
        "export const tokens = { marker: \"fx-pure-marker\" };\n",
    )
    .unwrap();
    std::fs::write(
        pkg.join("effect.fx.js"),
        "console.log(\"fx-effect-marker\");\n",
    )
    .unwrap();
    let route = root.join("src/routes/__root.tsx");
    let source = std::fs::read_to_string(&route).unwrap();
    let source = source.replacen(
        "import \"../../styles/app.css\";",
        "import \"../../styles/app.css\";\nimport \"../../fxpkg/tokens.fx.js\";\nimport \"../../fxpkg/effect.fx.js\";",
        1,
    );
    assert!(
        source.contains("fxpkg/effect.fx.js"),
        "the fixture's __root.tsx changed shape; update this test"
    );
    std::fs::write(&route, source).unwrap();
    build(&root);

    let client: String = std::fs::read_dir(root.join("dist/client/assets"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "js"))
        .map(|p| std::fs::read_to_string(p).unwrap())
        .collect();
    assert!(
        client.contains("fx-effect-marker"),
        "a module with side effects the glob covers stays in the client bundle"
    );
    assert!(
        !client.contains("fx-pure-marker"),
        "a pure module the glob covers is dropped, as Vite's build drops it; oj kept it"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A sheet a module imports plainly and another imports as `?url` joins its
/// chunk's stylesheet once: vite:css-post reads a chunk's sheet off each
/// module's own id, and a `?url` module (or the `?transform-only` one its
/// compile runs under) carries none. oj keyed the sheets by path, so the
/// chunk took the sheet once per id (Lovable's app-boot sheet carried
/// Partners.css three times over, and its manifest row named another
/// chunk's sheet).
#[test]
fn a_sheet_imported_plainly_and_by_url_joins_its_chunks_sheet_once() {
    let Some(root) = app("sheet-plain-and-url") else {
        return;
    };
    std::fs::write(
        root.join("styles/dual.css"),
        ".dual-marker { color: red }\n",
    )
    .unwrap();
    let route = root.join("src/routes/__root.tsx");
    let source = std::fs::read_to_string(&route).unwrap();
    let source = source
        .replacen(
            "import \"../../styles/app.css\";",
            "import \"../../styles/app.css\";\nimport \"../../styles/dual.css\";\nimport dualUrl from \"../../styles/dual.css?url\";",
            1,
        )
        .replacen(
            "    meta: [{ title: \"oj start fixture\" }],",
            "    meta: [{ title: \"oj start fixture\" }, { name: \"x-dual\", content: dualUrl }],",
            1,
        );
    assert!(
        source.contains("content: dualUrl }"),
        "the fixture's __root.tsx changed shape; update this test"
    );
    std::fs::write(&route, source).unwrap();
    build(&root);

    let sheets: Vec<(PathBuf, String)> = std::fs::read_dir(root.join("dist/client/assets"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "css"))
        .map(|p| {
            let css = std::fs::read_to_string(&p).unwrap();
            (p, css)
        })
        .collect();
    let with_marker: Vec<&PathBuf> = sheets
        .iter()
        .filter(|(_, css)| css.contains(".dual-marker"))
        .map(|(p, _)| p)
        .collect();
    assert!(
        with_marker.iter().any(|p| p
            .file_name()
            .is_some_and(|f| f.to_string_lossy().starts_with("dual-"))),
        "the ?url import emits the sheet as dual-<hash>.css: {with_marker:?}"
    );
    assert_eq!(
        with_marker.len(),
        2,
        "the sheet is in its chunk's stylesheet and the ?url asset, nowhere else: {with_marker:?}"
    );
    for (p, css) in &sheets {
        assert_eq!(
            css.matches(".dual-marker").count() <= 1,
            true,
            "{} carries the sheet more than once",
            p.display()
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// A chunk's sheet with the bytes of an asset emitted already takes that
/// asset's name, as under `vite build`: rolldown's file emitter keeps one
/// asset per source for the files emitted without a `fileName`, and once
/// output starts (css-post emits a chunk's sheet in renderChunk) the first
/// name stays. oj wrote the chunk's sheet under the chunk's name beside the
/// `?url` asset of the same text (Lovable shipped `Partners-<hash>.css` and
/// `PartnersProgramSteps-<hash>.css`, one file twice, and its chunks hashed
/// the second name).
#[test]
fn a_chunk_sheet_with_an_emitted_assets_bytes_takes_the_assets_name() {
    let Some((app, probe)) = chunk_css_app(
        "sheet-dedupe",
        "  build: { manifest: true },\n",
        |app| {
            let sheet = ".steps-marker { color: teal; }\n";
            std::fs::write(app.join("styles/partners.css"), sheet).unwrap();
            std::fs::write(app.join("src/steps.css"), sheet).unwrap();
            std::fs::write(
                app.join("src/steps-widget.tsx"),
                "import \"./steps.css\";\nexport default function StepsWidget() {\n  return <p className=\"steps-marker\">steps-widget</p>;\n}\n",
            )
            .unwrap();
            let route = app.join("src/routes/__root.tsx");
            let source = std::fs::read_to_string(&route).unwrap();
            let source = source
                .replacen(
                    "import \"../../styles/app.css\";",
                    "import \"../../styles/app.css\";\nimport partnersUrl from \"../../styles/partners.css?url\";",
                    1,
                )
                .replacen(
                    "    meta: [{ title: \"oj start fixture\" }],",
                    "    meta: [{ title: \"oj start fixture\" }, { name: \"x-partners\", content: partnersUrl }],",
                    1,
                );
            assert!(
                source.contains("content: partnersUrl }"),
                "the fixture's __root.tsx changed shape; update this test"
            );
            std::fs::write(&route, source).unwrap();
            let route = app.join("src/routes/about.tsx");
            let source = std::fs::read_to_string(&route).unwrap();
            let source = source
                .replacen(
                    "import { createRoute } from \"@tanstack/react-router\";\n",
                    "import { createRoute } from \"@tanstack/react-router\";\nimport { lazy, Suspense } from \"react\";\nconst Widget = lazy(() => import(\"../steps-widget\"));\n",
                    1,
                )
                .replacen(
                    "<h1 className=\"fixture-heading\">about-page-marker</h1>",
                    "<h1 className=\"fixture-heading\">about-page-marker</h1>\n      <Suspense fallback={null}><Widget /></Suspense>",
                    1,
                );
            assert!(
                source.contains("<Widget />"),
                "the fixture's about.tsx changed shape; update this test"
            );
            std::fs::write(&route, source).unwrap();
        },
    ) else {
        return;
    };
    let names: Vec<String> = std::fs::read_dir(app.join("dist/client/assets"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let with_marker: Vec<&String> = names
        .iter()
        .filter(|n| {
            n.ends_with(".css")
                && std::fs::read_to_string(app.join("dist/client/assets").join(n))
                    .is_ok_and(|css| css.contains(".steps-marker"))
        })
        .collect();
    assert_eq!(
        with_marker.len(),
        1,
        "the sheet is one file, the ?url asset's: {with_marker:?}"
    );
    let partners = with_marker[0];
    assert!(
        partners.starts_with("partners-"),
        "the file keeps the first emit's name, partners-<hash>.css: {partners}"
    );
    assert!(
        !names
            .iter()
            .any(|n| n.starts_with("steps-widget-") && n.ends_with(".css")),
        "no steps-widget-<hash>.css beside it: {names:?}"
    );
    // The widget chunk's sheet is that file, in its viteMetadata and the manifest.
    let widget = names
        .iter()
        .find(|n| n.starts_with("steps-widget-") && n.ends_with(".js"))
        .expect("the widget's chunk");
    assert_eq!(
        probe["css"][format!("assets/{widget}")],
        serde_json::json!([format!("assets/{partners}")]),
        "the chunk's importedCss:\n{probe}"
    );
    let raw = std::fs::read_to_string(app.join("dist/client/.vite/manifest.json")).unwrap();
    let manifest: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&raw).unwrap();
    let row = manifest
        .values()
        .find(|row| row["file"] == format!("assets/{widget}"))
        .unwrap_or_else(|| panic!("a manifest row for {widget}:\n{raw}"));
    assert_eq!(
        row["css"],
        serde_json::json!([format!("assets/{partners}")]),
        "the chunk's manifest row names the sheet:\n{raw}"
    );
    assert_eq!(
        manifest["styles/partners.css"]["file"],
        serde_json::json!(format!("assets/{partners}")),
        "the ?url import's row:\n{raw}"
    );
    let _ = std::fs::remove_dir_all(&app);
}

/// A plain sheet imported with `import()` leaves no JS chunk behind, as
/// under `vite build`: its module is empty, the chunk holding it alone is a
/// pure CSS chunk that generateBundle drops, and the import becomes
/// `Promise.resolve({})` with the sheet alone in its preload list. oj's
/// module exported an object, so the chunk stayed, one request for nothing
/// (Lovable's katex.min and x-theme sheets).
#[test]
fn a_dynamically_imported_plain_sheet_leaves_no_chunk_as_in_vite() {
    let Some(root) = app("dynamic-sheet-chunk") else {
        return;
    };
    let config = root.join("vite.config.ts");
    let source = std::fs::read_to_string(&config).unwrap();
    let source = source.replacen(
        r#"    client: { define: { __FIXTURE_SIDE__: JSON.stringify("client-side") } },"#,
        r#"    client: { define: { __FIXTURE_SIDE__: JSON.stringify("client-side") }, build: { manifest: true } },"#,
        1,
    );
    assert!(
        source.contains("manifest: true"),
        "the fixture's vite.config.ts changed shape; update this test"
    );
    std::fs::write(&config, source).unwrap();
    std::fs::write(
        root.join("styles/lazy-only.css"),
        ".lazy-only-marker { color: blue }\n",
    )
    .unwrap();
    let route = root.join("src/routes/__root.tsx");
    let source = std::fs::read_to_string(&route).unwrap();
    let source = source.replacen(
        "import \"../../styles/app.css\";",
        "import \"../../styles/app.css\";\nif (typeof window !== \"undefined\") void import(\"../../styles/lazy-only.css\");",
        1,
    );
    assert!(
        source.contains("lazy-only.css"),
        "the fixture's __root.tsx changed shape; update this test"
    );
    std::fs::write(&route, source).unwrap();
    build(&root);

    let assets: Vec<String> = std::fs::read_dir(root.join("dist/client/assets"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !assets
            .iter()
            .any(|f| f.starts_with("lazy-only-") && f.contains(".js")),
        "no JS chunk (nor its map) remains for the sheet: {assets:?}"
    );
    let sheet = assets
        .iter()
        .find(|f| f.starts_with("lazy-only-") && f.ends_with(".css"))
        .expect("the sheet is emitted as lazy-only-<hash>.css");
    let client: String = assets
        .iter()
        .filter(|f| f.ends_with(".js"))
        .map(|f| std::fs::read_to_string(root.join("dist/client/assets").join(f)).unwrap())
        .collect();
    assert!(
        client.contains("Promise.resolve({"),
        "the import of the dropped chunk is Promise.resolve({{}}), as Vite rewrites it"
    );
    assert!(
        client.contains(&format!("assets/{sheet}")),
        "the sheet is in the preload dependency table"
    );
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("dist/client/.vite/manifest.json")).unwrap(),
    )
    .unwrap();
    let rows = manifest.as_object().unwrap();
    assert!(
        !rows.values().any(|r| r["file"]
            .as_str()
            .is_some_and(|f| f.starts_with("assets/lazy-only-") && f.ends_with(".js"))),
        "the manifest names no chunk for the sheet"
    );
    assert!(
        rows.values()
            .all(|r| r["dynamicImports"].as_array().is_none_or(|d| !d
                .iter()
                .any(|k| k.as_str().is_some_and(|k| k.contains("lazy-only.css"))))),
        "no row lists the dropped chunk among its dynamicImports"
    );
    assert_eq!(
        rows.get("styles/lazy-only.css")
            .and_then(|r| r["file"].as_str()),
        Some(format!("assets/{sheet}").as_str()),
        "the sheet has its own row, as Vite's manifest keys a dropped pure CSS chunk's sheet"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A server function's module is built in two, as Start's compiler builds
/// it: the module itself, where the handler is a `createSsrRpc(id)` stub,
/// and the provider (`<file>?tss-serverfn-split`) holding the handler, a
/// chunk of its own the resolver imports when the function is first called.
/// oj kept every handler in its module, one chunk where `vite build` writes
/// two.
#[test]
fn a_server_functions_handler_lives_in_its_provider_chunk_alone() {
    let Some(root) = app("server-fn-provider") else {
        return;
    };
    // An app's `pre` resolve hook that answers a queried id with the file
    // alone (what a framework plugin did under Lovable's config): the provider
    // import must still reach the bundle with its query, or the provider IS
    // the module, compiled as the caller, with no handler left to call.
    let config = root.join("vite.config.ts");
    let source = std::fs::read_to_string(&config).unwrap();
    assert!(
        source.contains("  plugins: [\n"),
        "the fixture's vite.config.ts changed shape; update this test"
    );
    std::fs::write(
        &config,
        source.replacen(
            "  plugins: [\n",
            "  plugins: [\n    { name: \"fixture-clean-resolve\", enforce: \"pre\", async resolveId(id, importer) { if (!id.includes(\"?tss-serverfn-split\")) return null; const r = await this.resolve(id.split(\"?\")[0], importer, { skipSelf: true }); return r?.id ?? null; } },\n",
            1,
        ),
    )
    .unwrap();
    build(&root);
    let mut server: Vec<(PathBuf, String)> = Vec::new();
    for dir in ["dist", "dist/chunks"] {
        for entry in std::fs::read_dir(root.join(dir)).unwrap().flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "mjs") {
                let code = std::fs::read_to_string(&path).unwrap();
                server.push((path, code));
            }
        }
    }
    let holders: Vec<&PathBuf> = server
        .iter()
        .filter(|(_, code)| code.contains("server-fn-marker"))
        .map(|(p, _)| p)
        .collect();
    assert_eq!(
        holders.len(),
        1,
        "the handler's body is in one chunk, the provider's: {holders:?}"
    );
    let provider = holders[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        provider.starts_with("data-"),
        "the provider chunk is named after its module: {provider}"
    );
    let provider_code = &server.iter().find(|(p, _)| p == holders[0]).unwrap().1;
    assert!(
        provider_code.contains("getGreeting_createServerFn_handler"),
        "the provider exports the handler's sibling: {provider_code}"
    );
    let resolver = server
        .iter()
        .find(|(_, code)| code.contains("Server function info not found"))
        .expect("the resolver is in the server bundle");
    assert!(
        resolver.1.contains(&format!("./{provider}")),
        "the resolver imports the provider chunk: {}",
        resolver.0.display()
    );
    assert!(
        resolver
            .0
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("__23tanstack-start-server-fn-resolver-"),
        "the resolver chunk is named for Start's virtual module, as under Vite: {}",
        resolver.0.display()
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// An import a package's `browser` field maps to `false` (node-forge's
/// `crypto`) is Vite's browser-external stub, the one module every stubbed
/// builtin shares (`__vite-browser-external`, `module.exports = {}`); oj let
/// rolldown answer with an empty module named for the package's directory,
/// so the client bundle held one stub per package and a module id no Vite
/// build has.
#[test]
fn a_browser_field_false_mapping_is_vites_browser_external_stub() {
    let Some(root) = app("browser-field-false") else {
        return;
    };
    let pkg = root.join("fxbrowser");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("package.json"),
        "{ \"name\": \"fxbrowser\", \"private\": true, \"browser\": { \"crypto\": false } }\n",
    )
    .unwrap();
    std::fs::write(
        pkg.join("index.js"),
        "var crypto = require(\"crypto\");\nmodule.exports = { cryptoKind: typeof crypto.randomBytes };\n",
    )
    .unwrap();
    let config = root.join("vite.config.ts");
    let source = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        source.replacen(
            "export default defineConfig({\n",
            "export default defineConfig({\n  build: { minify: false },\n",
            1,
        ),
    )
    .unwrap();
    let route = root.join("src/routes/__root.tsx");
    let source = std::fs::read_to_string(&route).unwrap();
    let source = source
        .replacen(
            "import \"../../styles/app.css\";",
            "import \"../../styles/app.css\";\nimport fxbrowser from \"../../fxbrowser/index.js\";",
            1,
        )
        .replacen(
            "    meta: [{ title: \"oj start fixture\" }],",
            "    meta: [{ title: \"oj start fixture\" }, { name: \"x-crypto\", content: fxbrowser.cryptoKind }],",
            1,
        );
    assert!(
        source.contains("fxbrowser.cryptoKind"),
        "the fixture's __root.tsx changed shape; update this test"
    );
    std::fs::write(&route, source).unwrap();
    build(&root);

    let client: String = std::fs::read_dir(root.join("dist/client/assets"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "js"))
        .map(|p| std::fs::read_to_string(p).unwrap())
        .collect();
    // rolldown names the stub's wrapper after its id (`require___vite_browser_external`).
    assert!(
        client.contains("__vite_browser_external")
            || client.contains("\"__vite-browser-external\""),
        "the mapped-away import is Vite's browser-external module"
    );
    assert!(
        !client.contains("require(\"crypto\")") && !client.contains("from \"crypto\""),
        "no import of the builtin itself remains"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A bare import nothing resolves, from a package that lists it as an
/// optional peer dependency, is Vite's `__vite-optional-peer-dep` module, a
/// module that throws when it runs (framer-motion's `@emotion/is-prop-valid`);
/// rolldown left a `require()` of it to its runtime's require shim, which
/// Vite's bundle never carries.
#[test]
fn an_optional_peer_dependency_nothing_resolves_is_vites_throwing_module() {
    let Some(root) = app("optional-peer-dep") else {
        return;
    };
    let pkg = root.join("fxpeer");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("package.json"),
        "{ \"name\": \"fxpeer\", \"private\": true, \"peerDependencies\": { \"not-installed-dep\": \"*\" }, \"peerDependenciesMeta\": { \"not-installed-dep\": { \"optional\": true } } }\n",
    )
    .unwrap();
    std::fs::write(
        pkg.join("index.js"),
        "var dep;\ntry { dep = require(\"not-installed-dep\"); } catch (e) { dep = { kind: \"fx-peer-fallback\" }; }\nmodule.exports = { kind: dep.kind };\n",
    )
    .unwrap();
    let route = root.join("src/routes/__root.tsx");
    let source = std::fs::read_to_string(&route).unwrap();
    let source = source
        .replacen(
            "import \"../../styles/app.css\";",
            "import \"../../styles/app.css\";\nimport fxpeer from \"../../fxpeer/index.js\";",
            1,
        )
        .replacen(
            "    meta: [{ title: \"oj start fixture\" }],",
            "    meta: [{ title: \"oj start fixture\" }, { name: \"x-peer\", content: fxpeer.kind }],",
            1,
        );
    assert!(
        source.contains("fxpeer.kind"),
        "the fixture's __root.tsx changed shape; update this test"
    );
    std::fs::write(&route, source).unwrap();
    build(&root);

    let client: String = std::fs::read_dir(root.join("dist/client/assets"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "js"))
        .map(|p| std::fs::read_to_string(p).unwrap())
        .collect();
    assert!(
        client.contains("Could not resolve ") && client.contains("Is it installed?"),
        "the optional peer dependency is Vite's throwing module"
    );
    assert!(
        !client.contains("in an environment that doesn't expose the "),
        "rolldown's require shim is not in the bundle"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The manifest lists an asset a module imports as `?url` (and a worker, a
/// sheet's `url()`) on its chunk's `assets`, and gives every written asset a
/// row keyed by its source, as Vite's manifest does; rolldown's bundle never
/// holds the files oj writes itself, so they were missing (Lovable: 173
/// rows and 16 chunks' `assets`).
#[test]
fn the_manifest_lists_a_chunks_url_assets_and_gives_each_asset_a_row() {
    let Some(root) = app("manifest-url-assets") else {
        return;
    };
    let config = root.join("vite.config.ts");
    let source = std::fs::read_to_string(&config).unwrap();
    let source = source.replacen(
        r#"    client: { define: { __FIXTURE_SIDE__: JSON.stringify("client-side") } },"#,
        r#"    client: { define: { __FIXTURE_SIDE__: JSON.stringify("client-side") }, build: { manifest: true } },"#,
        1,
    );
    assert!(
        source.contains("manifest: true"),
        "the fixture's vite.config.ts changed shape; update this test"
    );
    std::fs::write(&config, source).unwrap();
    // Over the inline limit, so it is written as a file.
    std::fs::write(root.join("src/big-asset.bin"), vec![0x4f; 6000]).unwrap();
    let route = root.join("src/routes/__root.tsx");
    let source = std::fs::read_to_string(&route).unwrap();
    let source = source
        .replacen(
            "import \"../../styles/app.css\";",
            "import \"../../styles/app.css\";\nimport bigAssetUrl from \"../big-asset.bin?url\";",
            1,
        )
        .replacen(
            "    meta: [{ title: \"oj start fixture\" }],",
            "    meta: [{ title: \"oj start fixture\" }, { name: \"x-big\", content: bigAssetUrl }],",
            1,
        );
    assert!(
        source.contains("content: bigAssetUrl }"),
        "the fixture's __root.tsx changed shape; update this test"
    );
    std::fs::write(&route, source).unwrap();
    build(&root);

    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("dist/client/.vite/manifest.json")).unwrap(),
    )
    .unwrap();
    let rows = manifest.as_object().unwrap();
    let row = rows
        .get("src/big-asset.bin")
        .expect("the asset has a row keyed by its source");
    let file = row["file"].as_str().unwrap();
    assert!(
        file.starts_with("assets/big-asset-") && file.ends_with(".bin"),
        "{file}"
    );
    assert_eq!(row["src"].as_str(), Some("src/big-asset.bin"));
    assert!(
        root.join("dist/client").join(file).is_file(),
        "the row names the written file"
    );
    let owner = rows
        .values()
        .find(|r| {
            r["assets"]
                .as_array()
                .is_some_and(|a| a.iter().any(|f| f.as_str() == Some(file)))
        })
        .expect("the importing module's chunk lists the asset");
    assert!(
        owner["src"]
            .as_str()
            .is_some_and(|s| s.ends_with("src/routes/__root.tsx"))
            || owner["name"].as_str().is_some(),
        "{owner}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A module importing StyleX leaves the plugin as babel prints it, as under
/// `vite build`: @stylexjs/unplugin runs `babel.transformAsync` over every
/// such module and hands on its print, and @babel/generator writes an
/// expression's parentheses only where the grammar needs them, so a
/// source's `cond ? ((...a) => x) : y` reaches Vite's own transform without
/// the arrow's. oj's compiler spliced the source and kept them, oxc's parser
/// marks a parenthesized function, and every print after it kept them, down
/// through esbuild's minify into the chunk (Lovable's drawer, under
/// `build.minify: "esbuild"`).
#[test]
fn a_stylex_module_leaves_the_plugin_as_babel_prints_it() {
    let Some(root) = stylex_app("babel-print") else {
        return;
    };
    let config = root.join("vite.config.ts");
    let source = std::fs::read_to_string(&config).unwrap().replacen(
        "export default defineConfig({\n  plugins:",
        "export default defineConfig({\n  build: { minify: \"esbuild\" },\n  plugins:",
        1,
    );
    assert!(
        source.contains("minify: \"esbuild\""),
        "the fixture's vite.config.ts changed shape"
    );
    std::fs::write(&config, source).unwrap();
    let card = root.join("src/Card.tsx");
    let source = std::fs::read_to_string(&card).unwrap().replacen(
        "  return (\n    <section id=\"card\" {...stylex.props(styles.card)}>\n",
        "  const label =\n    typeof onToggle === \"function\" ? ((...a: unknown[]) => <>{a.length}</>) : <>{String(onToggle)}</>;\n  return (\n    <section id=\"card\" {...stylex.props(styles.card)}>\n      {typeof label === \"function\" ? label() : label}\n",
        1,
    );
    assert!(
        source.contains("const label ="),
        "the fixture's Card.tsx changed shape"
    );
    std::fs::write(&card, source).unwrap();
    build(&root);

    let chunks: Vec<(PathBuf, String)> = std::fs::read_dir(root.join("dist/client/assets"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "js"))
        .map(|p| {
            let code = std::fs::read_to_string(&p).unwrap();
            (p, code)
        })
        .collect();
    let (chunk, code) = chunks
        .iter()
        .find(|(_, code)| code.contains("\"function\"?"))
        .expect("the Card chunk, minified by esbuild");
    assert!(
        !code.contains("?((..."),
        "{}: the arrow keeps the source's parentheses:\n{code}",
        chunk.display()
    );
    assert!(
        code.contains("\"function\"?(..."),
        "{}: the arrow is there without them:\n{code}",
        chunk.display()
    );
    let _ = std::fs::remove_dir_all(&root);
}
