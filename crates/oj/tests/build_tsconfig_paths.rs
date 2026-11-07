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

/// Builds `files` with Vite and oj; per tool, the text of every emitted
/// `.js` file and the spy log (`{{LOG}}` in a file names it).
fn build_both(files: &[(&str, &str)]) -> Option<[(String, String); 2]> {
    let modules = node_modules()?;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let log = root.join("spy.log");
    for (rel, body) in COMMON.iter().chain(files) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body.replace("{{LOG}}", &log.display().to_string())).unwrap();
    }
    std::os::unix::fs::symlink(&modules, root.join("node_modules")).unwrap();
    let take_log = || {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::remove_file(&log);
        text
    };
    run(Command::new("node")
        .arg(modules.join("vite/bin/vite.js"))
        .args(["build", "--outDir", "vite-dist"])
        .current_dir(&root));
    let vite_log = take_log();
    run(Command::new(env!("CARGO_BIN_EXE_oj"))
        .args(["build", "--outDir", "oj-dist"])
        .arg(&root)
        .current_dir(&root));
    let oj_log = take_log();
    Some([
        (js_text(&root.join("vite-dist")), vite_log),
        (js_text(&root.join("oj-dist")), oj_log),
    ])
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
    for (tool, (code, _)) in ["vite", "oj"].iter().zip(built) {
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
    for (tool, (code, _)) in ["vite", "oj"].iter().zip(built) {
        assert!(code.contains("from-lib-x"), "{tool}:\n{code}");
    }
}

// Vite calls `worker.plugins` for every worker bundle, a nested one too
// (`plugins(newBundleChain)`): a worker imported from a worker maps `@/x`.
#[test]
fn a_nested_worker_bundle_runs_its_worker_plugins() {
    let Some(built) = build_both(&[
        (
            "index.html",
            "<!doctype html><html><body><script type=\"module\" src=\"/src/main.ts\"></script></body></html>",
        ),
        ("src/main.ts", "import A from \"./a.ts?worker\";\nnew A();\n"),
        ("src/a.ts", "import B from \"./b.ts?worker\";\nnew B();\n"),
        (
            "src/b.ts",
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
    for (tool, (code, _)) in ["vite", "oj"].iter().zip(built) {
        assert!(code.contains("from-lib-x"), "{tool}:\n{code}");
        assert!(!code.contains("\"@/x\""), "{tool} left the import:\n{code}");
    }
}

/// A plugin logging its lifecycle hooks under `tag`, to `{{LOG}}`.
const SPY: &str = r#"import fs from "node:fs";
const log = (line) => fs.appendFileSync("{{LOG}}", line + "\n");
export const spy = (tag) => ({
  name: "spy",
  config() { log(`${tag} config`); },
  buildStart() { log(`${tag} buildStart`); },
  buildEnd() { log(`${tag} buildEnd`); },
  renderStart() { log(`${tag} renderStart`); },
  generateBundle() { log(`${tag} generateBundle`); },
  closeBundle() { log(`${tag} closeBundle`); },
});
"#;

/// Each tag's hooks, in order (tags interleave differently per tool).
fn by_tag(log: &str) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut out: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for line in log.lines() {
        if let Some((tag, hook)) = line.split_once(' ') {
            out.entry(tag.to_string())
                .or_default()
                .push(hook.to_string());
        }
    }
    out
}

// Vite's bundleWorkerEntry: a container per worker bundle, from the
// `worker.plugins` factory, through buildStart, buildEnd, renderStart,
// generateBundle and closeBundle (`bundle.close()` in its `finally`); one
// worker imported twice is bundled once.
#[test]
fn worker_plugins_run_vites_lifecycle_once_per_worker_bundle() {
    let Some(built) = build_both(&[
        ("spy.js", SPY),
        (
            "index.html",
            "<!doctype html><html><body><script type=\"module\" src=\"/src/main.ts\"></script></body></html>",
        ),
        (
            "src/main.ts",
            "import W from \"./w.ts?worker\";\nimport I from \"./i.ts?worker&inline\";\nimport \"./other.ts\";\nnew W();\nnew I();\n",
        ),
        (
            "src/other.ts",
            "import W from \"./w.ts?worker\";\nimport I from \"./i.ts?worker&inline\";\nexport const ws = [W, I];\n",
        ),
        ("src/w.ts", "self.postMessage(1);\n"),
        ("src/i.ts", "self.postMessage(2);\n"),
        (
            "vite.config.js",
            "import { spy } from \"./spy.js\";\nlet n = 0;\n\
             export default { worker: { plugins: () => [spy(`worker${++n}`)] } };\n",
        ),
    ]) else {
        return;
    };
    let [(_, vite), (_, oj)] = built;
    assert!(!vite.is_empty(), "the spy logged nothing under Vite");
    assert_eq!(by_tag(&oj), by_tag(&vite), "oj:\n{oj}\nvite:\n{vite}");
}

// A library build is one rolldown build per format over the same plugins:
// buildStart, buildEnd, renderStart and generateBundle per format, then
// closeBundle once; a plugin's config() `define` applies.
#[test]
fn a_library_build_runs_the_plugins_lifecycle_and_their_defines() {
    let Some(built) = build_both(&[
        ("spy.js", SPY),
        ("src/index.ts", "export const v = __V__;\n"),
        (
            "vite.config.js",
            "import { spy } from \"./spy.js\";\n\
             export default { plugins: [spy(\"app\"), { name: \"def\", config: () => ({ define: { __V__: JSON.stringify(\"from-config-define\") } }) }], build: { lib: { entry: \"src/index.ts\", formats: [\"es\", \"cjs\"], fileName: \"index\" } } };\n",
        ),
    ]) else {
        return;
    };
    let [(vite_code, vite), (oj_code, oj)] = built;
    for (tool, code) in [("vite", &vite_code), ("oj", &oj_code)] {
        assert!(code.contains("from-config-define"), "{tool}:\n{code}");
    }
    // The build hooks only: `oj build` reads the config through Vite's
    // resolveConfig and then runs the plugins' config hooks again in its host
    // (true of every oj build, not of this one).
    let build_hooks = |log: &str| {
        let mut tags = by_tag(log);
        for hooks in tags.values_mut() {
            hooks.retain(|h| h != "config");
        }
        tags
    };
    assert_eq!(
        build_hooks(&oj),
        build_hooks(&vite),
        "oj:\n{oj}\nvite:\n{vite}"
    );
}
