// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! oj reads a literal vite.config in Rust (oj_server's static_config) instead
//! of evaluating it with Vite on a JS engine. Its output must equal the
//! engine's, field for field, or a boot without an engine serves a different
//! config. Runs against e2e/fixtures/stylex-app's installed Vite,
//! `@vitejs/plugin-react` and `@stylexjs/unplugin` (`npm ci` there); skips
//! without them unless `OJ_REQUIRE_FIXTURES` is set.

use std::path::{Path, PathBuf};

fn fixture() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../e2e/fixtures/stylex-app")
        .canonicalize()
        .unwrap();
    if dir.join("node_modules/vite").is_dir() {
        return Some(dir);
    }
    assert!(
        std::env::var_os("OJ_REQUIRE_FIXTURES").is_none(),
        "OJ_REQUIRE_FIXTURES is set but the stylex-app fixture is not installed"
    );
    eprintln!("skipping: npm ci --prefix e2e/fixtures/stylex-app");
    None
}

/// An app at `dir/app` using the fixture's node_modules, with this config.
fn app(fixture: &Path, dir: &Path, file: &str, config: &str) -> (PathBuf, PathBuf) {
    let root = dir.join("app");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::copy(fixture.join("package.json"), root.join("package.json")).unwrap();
    std::os::unix::fs::symlink(fixture.join("node_modules"), root.join("node_modules")).unwrap();
    std::fs::write(root.join(file), config).unwrap();
    let root = root.canonicalize().unwrap();
    let vite = root.join(file);
    (root, vite)
}

fn both(root: &Path, vite: &Path) -> (Option<serde_json::Value>, serde_json::Value) {
    let (read, evaluated) = oj_server::plugins::static_and_engine_extraction(root, vite);
    let mut evaluated = evaluated.expect("the engine evaluates the config");
    let fields = evaluated.as_object_mut().unwrap();
    fields.remove("__deps");
    fields.remove("__depsTruncated");
    // The transcript, stored apart from the values in the cache.
    fields.remove("__stderr");
    (read, evaluated)
}

const CONFIGS: &[(&str, &str, &str)] = &[
    (
        "react and stylex, plain object",
        "vite.config.mjs",
        r#"import stylex from "@stylexjs/unplugin";
import react from "@vitejs/plugin-react";
export default { plugins: [stylex.vite(), react()], logLevel: "warn" };
"#,
    ),
    (
        "defineConfig, stylex options and a port",
        "vite.config.ts",
        r#"import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import stylex from "@stylexjs/unplugin";
export default defineConfig({
  plugins: [react(), stylex.vite({ useCSSLayers: true, dev: true, unstable_moduleResolution: { type: "commonJS" } })],
  server: { port: 8080 },
  clearScreen: false,
});
"#,
    ),
    (
        "react only with path aliases",
        "vite.config.ts",
        r#"import path from "node:path";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
      "~lib": path.resolve(import.meta.dirname, "src/../lib"),
      shared: "/abs/shared",
    },
  },
});
"#,
    ),
    (
        "the Lovable template's shape: host, port and an alias",
        "vite.config.ts",
        r#"import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import path from "path";
export default defineConfig({
  server: { host: "::", port: 8080 },
  plugins: [react()],
  resolve: { alias: { "@": path.resolve(__dirname, "./src") } },
});
"#,
    ),
    (
        "an IPv4 host with stylex",
        "vite.config.mjs",
        r#"import stylex from "@stylexjs/unplugin";
import react from "@vitejs/plugin-react";
export default { plugins: [stylex.vite(), react()], server: { host: "0.0.0.0" } };
"#,
    ),
    (
        "trailing-slash aliases and a whole-number stylex option",
        "vite.config.mjs",
        r#"import stylex from "@stylexjs/unplugin";
import react from "@vitejs/plugin-react";
export default {
  plugins: [react(), stylex.vite({ lightningcssOptions: { targets: { chrome: 6225920 } } })],
  resolve: { alias: { "@/": "/abs/src/", "~": "/abs/lib/", "@": "/abs/other" } },
};
"#,
    ),
];

#[test]
fn literal_configs_read_in_rust_equal_the_engine_evaluation() {
    let Some(fixture) = fixture() else {
        return;
    };
    oj_server::plugins::engine_jobs_via_subprocess(PathBuf::from(env!("CARGO_BIN_EXE_oj")));
    for (name, file, config) in CONFIGS {
        let dir = tempfile::tempdir().unwrap();
        let (root, vite) = app(&fixture, dir.path(), file, config);
        let (read, evaluated) = both(&root, &vite);
        let read = read.unwrap_or_else(|| panic!("{name}: Rust declined the config"));
        assert_eq!(read, evaluated, "{name}");
    }
}

/// Vite allows the workspace root, not the app, when the app sits in a
/// pnpm workspace: the Rust reader must find the same root.
#[test]
fn a_workspace_app_allows_the_workspace_root_like_vite() {
    let Some(fixture) = fixture() else {
        return;
    };
    oj_server::plugins::engine_jobs_via_subprocess(PathBuf::from(env!("CARGO_BIN_EXE_oj")));
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pnpm-workspace.yaml"), "packages: [app]\n").unwrap();
    let (root, vite) = app(&fixture, dir.path(), "vite.config.mjs", CONFIGS[0].2);
    let (read, evaluated) = both(&root, &vite);
    assert_eq!(read.expect("Rust reads the config"), evaluated);
    assert_eq!(
        evaluated["fsAllow"][0],
        dir.path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );
}

/// Vite bundles a symlinked config from its real path, so `import.meta.dirname`
/// is the target's directory, not the app's.
#[test]
fn a_symlinked_config_resolves_paths_from_its_target_like_vite() {
    let Some(fixture) = fixture() else {
        return;
    };
    oj_server::plugins::engine_jobs_via_subprocess(PathBuf::from(env!("CARGO_BIN_EXE_oj")));
    let dir = tempfile::tempdir().unwrap();
    let config = r#"import path from "node:path";
import react from "@vitejs/plugin-react";
export default { plugins: [react()], resolve: { alias: { "@": path.resolve(import.meta.dirname, "src") } } };
"#;
    let (root, vite) = app(&fixture, dir.path(), "vite.config.mjs", "");
    let shared = dir.path().join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join("vite.config.mjs"), config).unwrap();
    std::fs::remove_file(&vite).unwrap();
    std::os::unix::fs::symlink(shared.join("vite.config.mjs"), &vite).unwrap();
    let (read, evaluated) = both(&root, &vite);
    assert_eq!(read.expect("Rust reads the config"), evaluated);
    let target = shared.canonicalize().unwrap().join("src");
    assert_eq!(evaluated["alias"]["@"], target.to_string_lossy().as_ref());
}

/// The template holds one Vite's defaults (8.0 had another `fs.deny`), so a
/// Vite other than the one it was taken from goes to the engine.
#[test]
fn another_vite_version_goes_to_the_engine() {
    let Some(fixture) = fixture() else {
        return;
    };
    let config = CONFIGS[3].2;
    let with_vite = |version: &str| {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::copy(fixture.join("package.json"), root.join("package.json")).unwrap();
        // The fixture's install, but a Vite claiming `version`.
        let modules = root.join("node_modules");
        std::fs::create_dir_all(&modules).unwrap();
        for entry in std::fs::read_dir(fixture.join("node_modules"))
            .unwrap()
            .flatten()
        {
            if entry.file_name() != "vite" {
                std::os::unix::fs::symlink(entry.path(), modules.join(entry.file_name())).unwrap();
            }
        }
        let vite = modules.join("vite");
        std::fs::create_dir_all(vite.join("dist/client")).unwrap();
        std::fs::write(
            vite.join("package.json"),
            format!(r#"{{"name":"vite","version":"{version}"}}"#),
        )
        .unwrap();
        std::fs::write(root.join("vite.config.ts"), config).unwrap();
        oj_server::plugins::static_extraction(&root, &root.join("vite.config.ts"))
    };
    assert!(
        with_vite("8.3.5").is_some(),
        "the template's own version is read"
    );
    assert!(
        with_vite("8.0.0").is_none(),
        "Vite 8.0 must go to the engine"
    );
}

/// Shapes the Rust reader must hand to the engine: values only evaluation
/// knows, plugins outside oj's native set, and options it has no template for.
#[test]
fn configs_outside_the_literal_shape_go_to_the_engine() {
    let Some(fixture) = fixture() else {
        return;
    };
    let declined = [
        "import react from \"@vitejs/plugin-react\";\nexport default { plugins: [react()], server: { host: true } };\n",
        "import react from \"@vitejs/plugin-react\";\nexport default { plugins: [react(), { name: \"mine\" }] };\n",
        "import react from \"@vitejs/plugin-react\";\nexport default ({ mode }) => ({ plugins: [react()] });\n",
        "import react from \"@vitejs/plugin-react\";\nexport default { plugins: [react()], server: { port: Number(process.env.PORT) } };\n",
        "import react from \"@vitejs/plugin-react\";\nconsole.log(1);\nexport default { plugins: [react()] };\n",
        "import stylex from \"@stylexjs/unplugin\";\nexport default { plugins: [stylex.vite()] };\n",
        "import react from \"@vitejs/plugin-react\";\nexport default { plugins: [react({ babel: {} })] };\n",
        // plugin-react imported but not listed: its config hook never runs.
        "import react from \"@vitejs/plugin-react\";\nexport default { server: { port: 3000 } };\n",
        // JavaScript keeps the last of a repeated key.
        "import react from \"@vitejs/plugin-react\";\nexport default { plugins: [react()], resolve: { alias: { \"@\": \"/a\", \"@\": \"/b\" } } };\n",
        "import react from \"@vitejs/plugin-react\";\nexport default { plugins: [react()], resolve: { alias: { a: \"/a\" }, alias: { b: \"/b\" } } };\n",
        // `__proto__` sets the prototype, not an alias.
        "import react from \"@vitejs/plugin-react\";\nexport default { plugins: [react()], resolve: { alias: { __proto__: \"/x\" } } };\n",
    ];
    for config in declined {
        let dir = tempfile::tempdir().unwrap();
        let (root, vite) = app(&fixture, dir.path(), "vite.config.mjs", config);
        let (read, _) = oj_server::plugins::static_and_engine_extraction(&root, &vite);
        assert!(read.is_none(), "Rust must decline:\n{config}");
    }
}
