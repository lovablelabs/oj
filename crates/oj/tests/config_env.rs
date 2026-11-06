// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A config that reads the env gets the value of the run, as under Vite,
//! which evaluates the config on every start. oj caches the evaluated config
//! across runs, keyed by the vars the evaluation read: another value builds
//! with the new one, and a var nothing read leaves the cache serving.
//! Vite's own copy of the env (its loadEnv inside resolveConfig) keys only
//! the `envPrefix` vars it keeps.
//!
//! The apps run on e2e/fixtures/stylex-app's install (the real Vite). Without
//! it the tests skip, unless `OJ_REQUIRE_FIXTURES`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn node_modules() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../e2e/fixtures/stylex-app/node_modules")
        .canonicalize()
        .ok()
        .filter(|d| d.join("vite/bin/vite.js").is_file());
    if dir.is_none() {
        assert!(
            std::env::var_os("OJ_REQUIRE_FIXTURES").is_none(),
            "OJ_REQUIRE_FIXTURES is set but the stylex-app fixture is not installed"
        );
        eprintln!("skipping: npm ci --prefix e2e/fixtures/stylex-app");
    }
    dir
}

/// A scratch app whose page prints `__API__`, with `config` as its
/// vite.config.mjs. Its node_modules is a directory of links to the
/// fixture's packages, so what Vite writes there stays in the app.
fn app(name: &str, config: &str) -> Option<PathBuf> {
    let modules = node_modules()?;
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("oj-config-env-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{"name":"fx","private":true,"type":"module"}"#,
    )
    .unwrap();
    std::fs::write(
        root.join("index.html"),
        r#"<!doctype html><html><body><script type="module" src="/src/main.js"></script></body></html>"#,
    )
    .unwrap();
    std::fs::write(
        root.join("src/main.js"),
        "document.body.textContent = __API__;\n",
    )
    .unwrap();
    std::fs::write(root.join("vite.config.mjs"), config).unwrap();
    let own = root.join("node_modules");
    std::fs::create_dir_all(&own).unwrap();
    for entry in std::fs::read_dir(&modules).unwrap().flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        std::os::unix::fs::symlink(entry.path(), own.join(name)).unwrap();
    }
    Some(root)
}

/// One `oj build` of `root` with `env` set: the built JS, and whether the
/// evaluated config came from oj's cache.
fn build(root: &Path, env: &[(&str, &str)]) -> (String, bool) {
    let dist = root.join("dist");
    let _ = std::fs::remove_dir_all(&dist);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oj"));
    cmd.args(["build", "--outDir", "dist"])
        .arg(root)
        .current_dir(root)
        .env("DEBUG", "oj:boot")
        .envs(env.iter().copied());
    for var in [
        "OJ_TEST_API_URL",
        "OJ_TEST_UNREAD",
        "VITE_OJ_TEST",
        "DEBUG_OJ_TEST",
    ] {
        if !env.iter().any(|(k, _)| *k == var) {
            cmd.env_remove(var);
        }
    }
    let out = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "oj build failed:\n{}\n{stderr}",
        String::from_utf8_lossy(&out.stdout)
    );
    let mut js = String::new();
    for entry in std::fs::read_dir(dist.join("assets")).unwrap().flatten() {
        if entry.path().extension().is_some_and(|e| e == "js") {
            js.push_str(&std::fs::read_to_string(entry.path()).unwrap());
        }
    }
    (js, stderr.contains("vite-extract cache hit"))
}

#[test]
fn a_var_the_config_reads_builds_with_the_value_of_the_run() {
    let Some(root) = app(
        "read",
        r#"export default {
  define: { __API__: JSON.stringify(process.env.OJ_TEST_API_URL ?? "unset") },
};
"#,
    ) else {
        return;
    };
    let (js, _) = build(&root, &[("OJ_TEST_API_URL", "api-one")]);
    assert!(js.contains("api-one"), "{js}");

    let (js, cached) = build(&root, &[("OJ_TEST_API_URL", "api-two")]);
    assert!(
        js.contains("api-two") && !js.contains("api-one"),
        "the cached config kept the old value\n{js}"
    );
    assert!(
        !cached,
        "a var the config read changed, yet the cache served"
    );

    let (js, cached) = build(
        &root,
        &[("OJ_TEST_API_URL", "api-two"), ("OJ_TEST_UNREAD", "1")],
    );
    assert!(js.contains("api-two"), "{js}");
    assert!(cached, "a var the config never read missed the cache");

    let (js, _) = build(&root, &[]);
    assert!(js.contains("unset"), "{js}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn vites_own_env_copy_keys_only_the_env_prefix() {
    let Some(root) = app(
        "vite-copy",
        r#"import { defineConfig } from "vite";
export default defineConfig(({ mode }) => ({ define: { __API__: JSON.stringify(mode) } }));
"#,
    ) else {
        return;
    };
    let (js, _) = build(&root, &[]);
    assert!(js.contains("production"), "{js}");
    let (_, cached) = build(&root, &[("DEBUG_OJ_TEST", "1")]);
    assert!(cached, "Vite's own env copy keyed the cache on every var");
    let (_, cached) = build(&root, &[("DEBUG_OJ_TEST", "1"), ("VITE_OJ_TEST", "1")]);
    assert!(
        !cached,
        "a var under envPrefix changed, yet the cache served"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_config_that_loads_the_whole_env_builds_with_the_value_of_the_run() {
    let Some(root) = app(
        "load-env",
        r#"import { defineConfig, loadEnv } from "vite";
export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), "");
  return { define: { __API__: JSON.stringify(env.OJ_TEST_API_URL ?? "unset") } };
});
"#,
    ) else {
        return;
    };
    let (js, _) = build(&root, &[("OJ_TEST_API_URL", "api-one")]);
    assert!(js.contains("api-one"), "{js}");
    let (js, cached) = build(&root, &[("OJ_TEST_API_URL", "api-two")]);
    assert!(
        js.contains("api-two") && !js.contains("api-one"),
        "the cached config kept the old value\n{js}"
    );
    assert!(!cached);
    let (_, cached) = build(&root, &[("OJ_TEST_API_URL", "api-two")]);
    assert!(cached, "an unchanged env missed the cache");
    let _ = std::fs::remove_dir_all(&root);
}
