// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;

use super::{build_app, js_under, scratch, write_files};

const INDEX_HTML: &str = r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/entry.js"></script></body></html>"#;

/// Build-side hook gating: the Rust gate skips plugin RPCs a filter plan rules
/// out, and must never skip one a plugin would have acted on. This app covers
/// every shape the gate distinguishes: function-form resolveId/load
/// (unfiltered, always offered: the virtual module), a RegExp-filtered
/// transform that matches (its marker must land in the output), a
/// RegExp-filtered transform that matches nothing, a string-filtered transform
/// (a glob to rolldown, so conservatively unfiltered in the plan, still
/// filtered by the host), a RegExp-filtered load for a real import,
/// transformIndexHtml, and moduleParsed (which only fires inside the
/// transform RPC in build, so its presence must pin transform unfiltered and
/// every module still reaches it).
#[tokio::test]
async fn the_build_hook_gate_never_skips_a_hook_a_plugin_acts_on() {
    let root = scratch("hookgate");
    write_files(
        &root,
        &[
            (
                "package.json",
                r#"{"name":"hookgate-app","version":"1.0.0"}"#,
            ),
            (
                "src/entry.js",
                r#"import { virt } from "virtual:gate-info";
import { special } from "./data.special";
console.log("gate-entry", virt, special);
"#,
            ),
            // Resolved from disk like any file; the RegExp-filtered load then
            // supplies its real content, proving the load RPC crossed for a
            // matching id.
            ("src/data.special", "placeholder\n"),
            ("index.html", INDEX_HTML),
            (
                "oj.plugins.mjs",
                r#"export default [
  {
    name: "gate-virtual",
    resolveId(id) {
      if (id === "virtual:gate-info") return "\0gate-info";
    },
    load(id) {
      if (id === "\0gate-info") return 'export const virt = "virtual-crossed";';
    },
  },
  {
    name: "gate-special-load",
    load: {
      filter: { id: /\.special$/ },
      handler(id) {
        return 'export const special = "special-load-crossed";';
      },
    },
  },
  {
    name: "gate-transform-match",
    transform: {
      filter: { id: /entry\.js$/ },
      handler(code) {
        return { code: code + '\nconsole.log("regex-transform-crossed");', map: null };
      },
    },
  },
  {
    name: "gate-transform-never",
    transform: {
      filter: { id: /\.does-not-exist$/ },
      handler(code) {
        return { code: code + '\nconsole.log("never-transform-LEAKED");', map: null };
      },
    },
  },
  {
    name: "gate-transform-string-filter",
    transform: {
      filter: { id: "**/entry.js" },
      handler(code, id) {
        if (!id.endsWith("entry.js")) return null;
        return { code: code + '\nconsole.log("string-transform-crossed");', map: null };
      },
    },
  },
  {
    name: "gate-module-parsed",
    moduleParsed(info) {
      globalThis.__ojGateSeen = globalThis.__ojGateSeen || [];
      globalThis.__ojGateSeen.push(info.id);
    },
    generateBundle() {
      this.emitFile({ type: "asset", fileName: "module-parsed.txt", source: (globalThis.__ojGateSeen || []).join("\n") });
    },
  },
  {
    name: "gate-html",
    transformIndexHtml(html) {
      return html.replace("</head>", '<meta name="gate" content="html-crossed" /></head>');
    },
  },
];
"#,
            ),
        ],
    );
    build_app(&root).await.expect("the hook-gate app builds");

    let dist = root.join("dist");
    let assets = js_under(&dist.join("assets"));
    assert!(
        assets.contains("virtual-crossed"),
        "function-form resolveId+load crossed the gate:\n{assets}"
    );
    assert!(
        assets.contains("special-load-crossed"),
        "RegExp-filtered load crossed for a matching id:\n{assets}"
    );
    assert!(
        assets.contains("regex-transform-crossed"),
        "RegExp-filtered transform crossed for a matching id:\n{assets}"
    );
    assert!(
        assets.contains("string-transform-crossed"),
        "string-filtered plugin stayed unfiltered and crossed:\n{assets}"
    );
    assert!(
        !assets.contains("never-transform-LEAKED"),
        "non-matching filtered transform did not fire:\n{assets}"
    );

    let html = fs::read_to_string(dist.join("index.html")).expect("dist/index.html");
    assert!(
        html.contains("html-crossed"),
        "transformIndexHtml still runs:\n{html}"
    );

    let parsed = fs::read_to_string(dist.join("module-parsed.txt"))
        .expect("generateBundle wrote module-parsed.txt");
    let ids: Vec<&str> = parsed.lines().filter(|l| !l.is_empty()).collect();
    assert!(
        ids.iter().any(|id| id.ends_with("entry.js")),
        "moduleParsed saw the entry: {ids:?}"
    );
    assert!(
        ids.iter().any(|id| id.contains("gate-info")),
        "moduleParsed saw the virtual module: {ids:?}"
    );
    assert!(
        ids.iter().any(|id| id.ends_with(".special")),
        "moduleParsed saw the plugin-loaded module: {ids:?}"
    );
}

/// With ONLY RegExp-filtered plugins (no function-form hooks, no
/// moduleParsed, no string filter) the plan is genuinely filtered, so the gate
/// really skips RPCs; the filtered load and transform must still cross for
/// the modules they match.
#[tokio::test]
async fn a_genuinely_filtered_plan_still_crosses_for_its_matches() {
    let root = scratch("hookgate-strict");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"hookgate2","version":"1.0.0"}"#),
            (
                "src/entry.js",
                "import { special } from \"./data.special.js\";\nimport { other } from \"./other.js\";\nconsole.log(special, other);\n",
            ),
            (
                "src/data.special.js",
                "export const special = \"placeholder\";\n",
            ),
            ("src/other.js", "export const other = 1;\n"),
            ("index.html", INDEX_HTML),
            (
                "oj.plugins.mjs",
                r#"export default [
  {
    name: "strict-load",
    load: {
      filter: { id: /\.special\.js$/ },
      handler() {
        return 'export const special = "strict-load-crossed";';
      },
    },
  },
  {
    name: "strict-transform",
    transform: {
      filter: { id: /entry\.js$/ },
      handler(code) {
        return { code: code + '\nconsole.log("strict-transform-crossed");', map: null };
      },
    },
  },
];
"#,
            ),
        ],
    );
    build_app(&root)
        .await
        .expect("the strict-filter app builds");
    // A no-op gate is detectable: the filtered plans skipped real RPCs.
    let [_, load, transform] = gate_counts(&root);
    assert!(load > 0, "the filtered load plan skipped at least one RPC");
    assert!(
        transform > 0,
        "the filtered transform plan skipped at least one RPC"
    );

    let assets = js_under(&root.join("dist/assets"));
    assert!(
        assets.contains("strict-load-crossed"),
        "filtered load still crossed for its match:\n{assets}"
    );
    assert!(
        assets.contains("strict-transform-crossed"),
        "filtered transform still crossed for its match:\n{assets}"
    );
}

/// A perl-class pattern (`\b`: ASCII in JS, Unicode in Rust) must fail its
/// plugin OPEN: the transform plan goes unfiltered rather than rejecting the
/// filter, so it runs for every module, while load gating keeps working.
#[tokio::test]
async fn a_perl_class_code_filter_fails_open() {
    let root = scratch("hookgate-boundary");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"hookgate3","version":"1.0.0"}"#),
            (
                "src/entry.js",
                "import { special } from \"./data.special.js\";\nimport { other } from \"./other.js\";\nconsole.log(special, other);\n",
            ),
            (
                "src/data.special.js",
                "export const special = \"placeholder\";\n",
            ),
            ("src/other.js", "export const other = 1;\n"),
            ("index.html", INDEX_HTML),
            (
                "oj.plugins.mjs",
                r#"export default [
  {
    name: "strict-load",
    load: {
      filter: { id: /\.special\.js$/ },
      handler() {
        return 'export const special = "strict-load-crossed";';
      },
    },
  },
  {
    name: "boundary-transform",
    transform: {
      filter: { code: /\bimport\b/ },
      handler(code) {
        return { code: code + '\nconsole.log("boundary-transform-crossed");', map: null };
      },
    },
  },
];
"#,
            ),
        ],
    );
    build_app(&root)
        .await
        .expect("the boundary-filter app builds");
    // The entry carries `import`, so a filter that failed closed (or a gate
    // that skipped it) leaves the marker out.
    let assets = js_under(&root.join("dist/assets"));
    assert!(
        assets.contains("boundary-transform-crossed") && assets.contains("strict-load-crossed"),
        "the boundary transform and the filtered load both crossed:\n{assets}"
    );
    let [_, load, transform] = gate_counts(&root);
    assert_eq!(
        transform, 0,
        "a perl-class code filter pins transform unfiltered (no skips)"
    );
    assert!(
        load > 0,
        "load gating still active alongside the pinned transform"
    );
}

/// The one build of `root`'s gated-out RPC counts `[resolveId, load, transform]`.
fn gate_counts(root: &std::path::Path) -> [u64; 3] {
    match crate::build::gate_skips(root).as_slice() {
        [counts] => *counts,
        other => panic!(
            "expected one closeBundle gate report for {}, got {other:?}",
            root.display()
        ),
    }
}
