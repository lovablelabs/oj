// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `oj build` runs vite-tsconfig-paths where `vite build` does: in a library
//! build (the app's plugins) and in a worker bundle (`worker.plugins`). Told
//! by `projects` to read `tsconfig.lib.json`, it maps `@/x`; the nearest
//! `tsconfig.json`, the one rolldown's own resolver reads, has no `paths`.
//!
//! Uses e2e/fixtures/start-app's install (vite, vite-tsconfig-paths); skips
//! without it, unless `OJ_REQUIRE_FIXTURES`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn node_modules() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../e2e/fixtures/start-app/node_modules")
        .canonicalize()
        .ok()
        .filter(|d| d.join("vite-tsconfig-paths/package.json").is_file());
    if dir.is_none() {
        assert!(
            std::env::var_os("OJ_REQUIRE_FIXTURES").is_none(),
            "OJ_REQUIRE_FIXTURES is set but the start-app fixture is not installed"
        );
        eprintln!("skipping: npm ci --prefix e2e/fixtures/start-app");
    }
    dir
}

fn run(cmd: &mut Command) {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?} failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

const COMMON: &[(&str, &str)] = &[
    ("package.json", r#"{"name":"buildpaths","type":"module"}"#),
    ("tsconfig.json", r#"{ "compilerOptions": {} }"#),
    (
        "tsconfig.lib.json",
        r#"{ "compilerOptions": { "paths": { "@/*": ["./lib/*"] } } }"#,
    ),
    ("lib/x.ts", "export const marker = \"from-lib-x\";\n"),
];

/// Builds `files` with Vite and oj; the text of every emitted `.js` file,
/// per tool.
fn build_both(files: &[(&str, &str)]) -> Option<[String; 2]> {
    let modules = node_modules()?;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    for (rel, body) in COMMON.iter().chain(files) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    std::os::unix::fs::symlink(&modules, root.join("node_modules")).unwrap();

    run(Command::new("node")
        .arg(modules.join("vite/bin/vite.js"))
        .args(["build", "--outDir", "vite-dist"])
        .current_dir(&root));
    run(Command::new(env!("CARGO_BIN_EXE_oj"))
        .args(["build", "--outDir", "oj-dist"])
        .arg(&root)
        .current_dir(&root));
    Some(["vite-dist", "oj-dist"].map(|dist| js_text(&root.join(dist))))
}

fn js_text(dir: &Path) -> String {
    let mut out = String::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "js") {
                out.push_str(&std::fs::read_to_string(&path).unwrap());
            }
        }
    }
    out
}

#[test]
fn a_library_build_runs_vite_tsconfig_paths() {
    let Some(built) = build_both(&[
        ("src/index.ts", "export { marker } from \"@/x\";\n"),
        (
            "vite.config.js",
            "import tsConfigPaths from \"vite-tsconfig-paths\";\n\
             export default { plugins: [tsConfigPaths({ projects: [\"./tsconfig.lib.json\"] })], build: { lib: { entry: \"src/index.ts\", formats: [\"es\"], fileName: \"index\" } } };\n",
        ),
    ]) else {
        return;
    };
    for (tool, code) in ["vite", "oj"].iter().zip(built) {
        assert!(code.contains("from-lib-x"), "{tool}:\n{code}");
    }
}

#[test]
fn a_worker_bundle_runs_its_worker_plugins() {
    let Some(built) = build_both(&[
        (
            "index.html",
            "<!doctype html><html><body><script type=\"module\" src=\"/src/main.ts\"></script></body></html>",
        ),
        (
            "src/main.ts",
            "import W from \"./worker.ts?worker\";\nnew W();\n",
        ),
        (
            "src/worker.ts",
            "import { marker } from \"@/x\";\nself.postMessage(marker);\n",
        ),
        (
            "vite.config.js",
            "import tsConfigPaths from \"vite-tsconfig-paths\";\n\
             export default { worker: { plugins: () => [tsConfigPaths({ projects: [\"./tsconfig.lib.json\"] })] } };\n",
        ),
    ]) else {
        return;
    };
    for (tool, code) in ["vite", "oj"].iter().zip(built) {
        assert!(code.contains("from-lib-x"), "{tool}:\n{code}");
    }
}
