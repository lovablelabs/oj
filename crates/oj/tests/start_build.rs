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
        .find(|f| f.starts_with("client-") && f.ends_with(".js"))
        .unwrap_or_else(|| panic!("the client entry: {assets:?}"));
    let code = std::fs::read_to_string(root.join("dist/client/assets").join(entry)).unwrap();
    assert!(
        code.contains(&format!("./lazy/{}", lazy[0])),
        "the entry imports ./lazy/{}",
        lazy[0]
    );
    let _ = std::fs::remove_dir_all(&root);
}
