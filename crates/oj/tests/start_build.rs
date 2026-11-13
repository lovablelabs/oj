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
        "    { name: \"reads-out-dirs\", configResolved(c) { process.getBuiltinModule(\"node:fs\").writeFileSync(\"out-dirs.json\", JSON.stringify([c.environments.client.build.outDir, c.environments.ssr.build.outDir])); } },\n    cloudflare(",
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
        seen, r#"["dist/client","dist/server"]"#,
        "the resolved config names both directories"
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
