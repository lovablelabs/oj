// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `oj build` reads `legacy.inconsistentCjsInterop` as `vite build` does. A
//! CommonJS module flagged `__esModule` is read by its importer: in a
//! `"type": "module"` package the default import is module.exports; with the
//! option on it is the module's own `default`, as for an importer whose
//! package has no `type` (Vite's resolve plugin then leaves the package.json
//! out of what it reports to rolldown).
//!
//! The plugin that does this in `oj build` reports each module's
//! `sideEffects` verdict itself, as Vite's does, since rolldown reads it off
//! the package.json the plugin withholds.
//!
//! Uses e2e/fixtures/start-app's install (vite); skips without it, unless
//! `OJ_REQUIRE_FIXTURES`.

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

fn output(cmd: &mut Command) -> String {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?} failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A transpiled ES module: flagged `__esModule`, with a `default` of its own.
const DEP: &str = r#""use strict";
Object.defineProperty(exports, "__esModule", { value: true });
exports.default = function Comp() { return "comp"; };
exports.named = "n";
"#;

/// Every form whose result follows the importer: the default import in each
/// of its spellings, the namespace, a re-export of the default, a dynamic
/// import.
const MAIN: &str = r#"import "../fx/dist/polyfill.js";
import { unused } from "../fx/dist/pure.js";
import d, { named } from "../dep/index.cjs";
import { default as asDefault } from "../dep/index.cjs";
import * as ns from "../dep/index.cjs";
import reDefault from "./reexport.js";
import * as re from "./reexport.js";
const keys = (o) => Object.keys(o).sort().join(",");
export const report = async () => {
  const dynamic = await import("../dep/index.cjs");
  return [
    typeof d,
    typeof asDefault,
    named,
    keys(ns) + ":" + typeof ns.default,
    typeof reDefault,
    keys(re.star) + ":" + typeof re.star.default,
    keys(dynamic) + ":" + typeof dynamic.default,
    "polyfill:" + globalThis.__polyfilled + " pure:" + globalThis.__pure,
  ].join(" | ");
};
"#;

const REEXPORT: &str =
    "export { default } from \"../dep/index.cjs\";\nexport * as star from \"../dep/index.cjs\";\n";

/// What the built library reports, per tool: [vite, oj].
fn build_both(legacy: bool) -> Option<[String; 2]> {
    let modules = node_modules()?;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let config = format!(
        "export default {{\n  legacy: {{ inconsistentCjsInterop: {legacy} }},\n  build: {{ minify: false, lib: {{ entry: \"src/main.js\", formats: [\"es\"], fileName: \"out\" }} }},\n}};\n"
    );
    for (rel, body) in [
        (
            "package.json",
            r#"{"name":"legacy-interop","type":"module"}"#,
        ),
        ("vite.config.js", config.as_str()),
        ("src/main.js", MAIN),
        ("src/reexport.js", REEXPORT),
        ("dep/index.cjs", DEP),
        // A package whose `sideEffects` names one file, relative to it: the
        // side-effect import of that file stays, the unused import of
        // another is dropped.
        (
            "fx/package.json",
            r#"{"name":"fx","type":"module","sideEffects":["dist/polyfill.js"]}"#,
        ),
        ("fx/dist/polyfill.js", "globalThis.__polyfilled = true;\n"),
        (
            "fx/dist/pure.js",
            "globalThis.__pure = true;\nexport const unused = 1;\n",
        ),
    ] {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    std::os::unix::fs::symlink(&modules, root.join("node_modules")).unwrap();
    output(
        Command::new("node")
            .arg(modules.join("vite/bin/vite.js"))
            .args(["build", "--outDir", "vite-dist"])
            .current_dir(&root),
    );
    output(
        Command::new(env!("CARGO_BIN_EXE_oj"))
            .args(["build", "--outDir", "oj-dist"])
            .arg(&root)
            .current_dir(&root),
    );
    let report = |dist: &str| {
        let file = std::fs::read_dir(root.join(dist))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("out."))
            })
            .unwrap_or_else(|| panic!("the library file in {dist}"));
        output(Command::new("node").args([
            "--input-type=module",
            "-e",
            &format!(
                "const m = await import({:?}); console.log(await m.report());",
                file.display().to_string()
            ),
        ]))
    };
    Some([report("vite-dist"), report("oj-dist")])
}

#[test]
fn the_option_reads_a_flagged_dep_by_its_own_default_in_a_build() {
    let Some([vite, oj]) = build_both(true) else {
        return;
    };
    let own = "function | function | n | default,named:function | function | default,named:function | default,named:function | polyfill:true pure:undefined";
    assert_eq!(vite, own, "vite build with legacy.inconsistentCjsInterop");
    assert_eq!(oj, own, "oj build with legacy.inconsistentCjsInterop");
}

#[test]
fn without_the_option_a_type_module_importer_gets_module_exports() {
    let Some([vite, oj]) = build_both(false) else {
        return;
    };
    let whole = "object | object | n | default,named:object | object | default,named:object | default,named:object | polyfill:true pure:undefined";
    assert_eq!(vite, whole, "vite build");
    assert_eq!(oj, whole, "oj build");
}
