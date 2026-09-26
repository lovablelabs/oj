// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Executes ssr_transform output through a mock module-runner runtime (Node) to
//! prove semantic correctness — live bindings, namespaces, re-exports, dynamic
//! import, import.meta — beyond what substring assertions can check.

use std::io::Write;
use std::path::Path;
use std::process::Command;

fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

const HARNESS: &str = r#"
const AsyncFunction = Object.getPrototypeOf(async function(){}).constructor;
const fs = require("node:fs");
const entries = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
const registry = {};
const importFn = async (id) => registry[id];
async function evalMod(code, meta) {
  const mod = {};
  const exportName = (name, getter) =>
    Object.defineProperty(mod, name, { enumerable: true, configurable: true, get: getter });
  const exportAll = (obj) => {
    for (const k of Object.keys(obj)) if (k !== "default") exportName(k, () => obj[k]);
  };
  const fn = new AsyncFunction(
    "__vite_ssr_import__", "__vite_ssr_exportName__", "__vite_ssr_exportAll__",
    "__vite_ssr_dynamic_import__", "__vite_ssr_import_meta__", code);
  await fn(importFn, exportName, exportAll, importFn, meta);
  return mod;
}
(async () => {
  for (const [id, code] of entries) registry[id] = await evalMod(code, { url: id });
  const out = await registry["./main.js"].run();
  process.stdout.write(JSON.stringify(out));
})().catch((e) => { process.stderr.write(String(e && e.stack || e)); process.exit(1); });
"#;

fn transform(src: &str) -> String {
    oj_compiler::ssr::ssr_transform(src, Path::new("m.js"))
}

#[test]
fn ssr_output_executes_with_live_bindings_and_reexports() {
    if !node_available() {
        eprintln!("SKIP ssr_exec: node not available");
        return;
    }

    // A dependency with a live `let`, a fn that mutates it, a default, a named
    // const, plus a re-export barrel and a namespace consumer.
    let dep = transform(
        "export let count = 0;\
         export function inc() { count++ }\
         export const label = 'dep';\
         export default 42;",
    );
    let barrel = transform("export * from './dep.js';\nexport { label as tag } from './dep.js';");
    let main = transform(
        "import def, { count, inc, label } from './dep.js';\
         import * as ns from './dep.js';\
         import { tag } from './barrel.js';\
         export async function run() {\
           const before = count;\
           inc(); inc();\
           const dyn = await import('./dep.js');\
           return { def, before, after: count, nsAfter: ns.count, label, tag, dynDefault: dyn.default, meta: import.meta.url };\
         }",
    );

    let codes = serde_json::json!([
        ["./dep.js", dep],
        ["./barrel.js", barrel],
        ["./main.js", main],
    ]);

    let mut codes_file = tempfile::NamedTempFile::new().unwrap();
    codes_file.write_all(codes.to_string().as_bytes()).unwrap();

    let mut harness = tempfile::Builder::new().suffix(".cjs").tempfile().unwrap();
    harness.write_all(HARNESS.as_bytes()).unwrap();

    let out = Command::new("node")
        .arg(harness.path())
        .arg(codes_file.path())
        .output()
        .expect("run node harness");
    assert!(
        out.status.success(),
        "harness failed: {}\n--- main ---\n{main}",
        String::from_utf8_lossy(&out.stderr),
    );
    let result: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|_| panic!("bad json: {}", String::from_utf8_lossy(&out.stdout)));

    assert_eq!(result["def"], 42, "default import");
    assert_eq!(result["before"], 0, "live binding before mutation");
    assert_eq!(
        result["after"], 2,
        "live binding after inc() x2 (member access is live)"
    );
    assert_eq!(result["nsAfter"], 2, "namespace member is live");
    assert_eq!(result["label"], "dep", "named import");
    assert_eq!(result["tag"], "dep", "re-export alias through barrel");
    assert_eq!(result["dynDefault"], 42, "dynamic import default");
    assert_eq!(result["meta"], "./main.js", "import.meta.url");
}

#[test]
fn ssr_output_handles_extends_import_and_this_stripping() {
    if !node_available() {
        eprintln!("SKIP ssr_exec: node not available");
        return;
    }
    // Base has a method that relies on `this`; a free-function export that would
    // bind `this` wrongly if not stripped; and a subclass extending the import.
    let dep = transform(
        "export class Base { who() { return this.kind } constructor() { this.kind = 'base' } }\
         export function raw() { return this === undefined || this === globalThis ? 'unbound' : 'bound' }",
    );
    let main = transform(
        "import { Base, raw } from './dep.js';\
         class Derived extends Base { who() { return 'derived:' + super.who() } }\
         export function run() { return { sub: new Derived().who(), thisMode: raw() }; }",
    );
    let codes = serde_json::json!([["./dep.js", dep], ["./main.js", main]]);

    let mut codes_file = tempfile::NamedTempFile::new().unwrap();
    codes_file.write_all(codes.to_string().as_bytes()).unwrap();
    let mut harness = tempfile::Builder::new().suffix(".cjs").tempfile().unwrap();
    harness.write_all(HARNESS.as_bytes()).unwrap();

    let out = Command::new("node")
        .arg(harness.path())
        .arg(codes_file.path())
        .output()
        .expect("run node harness");
    assert!(
        out.status.success(),
        "harness failed: {}\n--- main ---\n{main}",
        String::from_utf8_lossy(&out.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|_| panic!("bad json: {}", String::from_utf8_lossy(&out.stdout)));

    assert_eq!(
        result["sub"], "derived:base",
        "subclass extending an imported class + super()"
    );
    assert_eq!(
        result["thisMode"], "unbound",
        "(0, import.fn)() strips `this` at call sites"
    );
}
