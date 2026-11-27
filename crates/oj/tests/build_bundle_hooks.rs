// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The bundle a plugin's `generateBundle` and `writeBundle` see under `oj
//! build` reads and takes changes as Rollup's does. oj hands the hooks the
//! bundle's metadata and reads a chunk's `code` or an asset's `source` when
//! a hook asks for it (the whole bundle's text is hundreds of MB on a large
//! app, and a hook reads a few files of it), so: a text reads as written, a
//! `code` or `source` a hook assigns is what is written, an entry it emits
//! is, and `writeBundle` sees the bundle as `generateBundle` left it. Built by Vite as well, for the same outcome.
//!
//! Uses e2e/fixtures/start-app's install (vite); skips without it, unless
//! `OJ_REQUIRE_FIXTURES`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn node_modules() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../e2e/fixtures/start-app/node_modules")
        .canonicalize()
        .ok()
        .filter(|d| d.join("vite/package.json").is_file());
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

/// A plugin that reads and changes the bundle in `generateBundle`, and
/// reads what `writeBundle` then sees, logging to `{{LOG}}`.
const PROBE: &str = r#"import fs from "node:fs";
const log = (line) => fs.appendFileSync("{{LOG}}", line + "\n");
const entryOf = (bundle) => Object.values(bundle).find((o) => o.type === "chunk" && o.isEntry);
// A stylesheet's or map's source, read the same way, is covered by the
// Start build's probe (start_build.rs): under this fixture's Vite neither is
// in the bundle a plugin's generateBundle sees.
export const probe = () => ({
  name: "bundle-probe",
  generateBundle(_options, bundle) {
    const entry = entryOf(bundle);
    log(`entry code read: ${entry.code.includes("probe-marker")}`);
    entry.code = entry.code.replace("probe-marker", "probe-rewritten");
    log(`entry code set: ${entry.code.includes("probe-rewritten")}`);
    // rolldown adds an asset emitted here to the bundle after the hook, so
    // it is read in writeBundle.
    this.emitFile({ type: "asset", fileName: "from-generate.txt", source: "emitted in generateBundle" });
  },
  writeBundle(_options, bundle) {
    const entry = entryOf(bundle);
    log(`written code: ${entry.code.includes("probe-rewritten")} ${entry.code.includes("probe-marker")}`);
    log(`written from-generate: ${bundle["from-generate.txt"].source}`);
  },
});
"#;

const FILES: &[(&str, &str)] = &[
    ("package.json", r#"{"name":"bundlehooks","type":"module"}"#),
    ("probe.js", PROBE),
    (
        "index.html",
        "<!doctype html><html><body><script type=\"module\" src=\"/src/main.js\"></script></body></html>\n",
    ),
    ("src/main.js", "document.title = \"probe-marker\";\n"),
    (
        "vite.config.js",
        "import { probe } from \"./probe.js\";\nexport default { plugins: [probe()] };\n",
    ),
];

/// Builds the fixture with Vite and oj; per tool, every file under its dist
/// by path, and the probe's log.
fn build_both() -> Option<[(BTreeMap<String, String>, String); 2]> {
    let modules = node_modules()?;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let log = root.join("probe.log");
    for (rel, body) in FILES {
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
        (dist_files(&root.join("vite-dist")), vite_log),
        (dist_files(&root.join("oj-dist")), oj_log),
    ])
}

fn dist_files(dir: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path.strip_prefix(dir).unwrap().display().to_string();
                out.insert(rel, std::fs::read_to_string(&path).unwrap_or_default());
            }
        }
    }
    out
}

#[test]
fn generate_bundle_reads_and_changes_the_bundle_as_under_vite() {
    let Some(built) = build_both() else {
        return;
    };
    let [(vite, vite_log), (oj, oj_log)] = built;
    assert!(!vite_log.is_empty(), "the probe logged nothing under Vite");
    assert_eq!(oj_log, vite_log, "oj:\n{oj_log}\nvite:\n{vite_log}");
    for (tool, files) in [("vite", &vite), ("oj", &oj)] {
        let entry = files
            .iter()
            .find(|(path, _)| path.ends_with(".js"))
            .map(|(_, text)| text)
            .unwrap_or_else(|| panic!("{tool} wrote no chunk: {:?}", files.keys()));
        assert!(
            entry.contains("probe-rewritten") && !entry.contains("probe-marker"),
            "{tool} did not write the code generateBundle assigned:\n{entry}"
        );
        assert_eq!(
            files.get("from-generate.txt").map(String::as_str),
            Some("emitted in generateBundle"),
            "{tool}: the asset emitted in generateBundle"
        );
    }
}
