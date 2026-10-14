// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The Start bundles' app-alias plugin, run the way oj runs it: the shipped
//! rolldown-assets.mjs inside oj's engine, bundling with the REAL rolldown the
//! fixture brings. An integration test so build.rs's exported-symbols link
//! flag applies (rolldown is a napi addon). Skips quietly when the fixture is
//! not installed.

use std::path::Path;

const PROBE: &str = r#"import { appAliasPlugin } from "./start/rolldown-assets.mjs";
import { rolldown } from "rolldown";

const bundle = async (input, plugins) => {
  const build = await rolldown({ input, plugins, logLevel: "silent" });
  try {
    const { output } = await build.generate({ format: "esm" });
    return output.map((o) => o.code ?? "").join("\n");
  } finally {
    await build.close();
  }
};

export async function run(root) {
  const config = {
    resolve: {
      alias: [
        { find: "@", replacement: `${root}/src` },
        { find: /^~legacy\/(.*)$/, replacement: `${root}/src/legacy/$1` },
      ],
    },
  };
  const out = {};
  out.aliased = await bundle(`${root}/src/entry.jsx`, [appAliasPlugin(config)]);
  out.unaliased = await bundle(`${root}/src/entry.jsx`, []);
  try {
    await bundle(`${root}/src/broken.jsx`, [appAliasPlugin(config)]);
    out.broken = null;
  } catch (e) {
    out.broken = String(e?.message ?? e);
  }
  return out;
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn start_bundles_apply_the_app_alias_through_real_rolldown() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let fixture_modules = repo.join("e2e/fixtures/start-app/node_modules");
    if !fixture_modules.join("rolldown").exists() {
        eprintln!("skipping: fixture rolldown not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::os::unix::fs::symlink(
        fixture_modules.canonicalize().unwrap(),
        root.join("node_modules"),
    )
    .unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{"name":"fx","type":"module"}"#,
    )
    .unwrap();
    oj_server::write_start_assets(&root.join("start")).unwrap();
    std::fs::write(root.join("probe.mjs"), PROBE).unwrap();

    let src = root.join("src");
    std::fs::create_dir_all(src.join("lib")).unwrap();
    std::fs::create_dir_all(src.join("legacy")).unwrap();
    std::fs::write(
        src.join("lib/target.js"),
        "export const mark = \"alias-mark\";\n",
    )
    .unwrap();
    // A legacy .jsx importer, reached through the RegExp entry, importing
    // through the object-style `@` entry; the scoped-looking `@/` never
    // shadows a real `@scope/pkg`.
    std::fs::write(
        src.join("legacy/Legacy.jsx"),
        "import { mark } from \"@/lib/target\";\nexport const legacy = () => mark;\n",
    )
    .unwrap();
    std::fs::write(
        src.join("entry.jsx"),
        "import { legacy } from \"~legacy/Legacy.jsx\";\nconsole.log(legacy());\n",
    )
    .unwrap();
    std::fs::write(
        src.join("broken.jsx"),
        "import { mark } from \"@/lib/alias-missing\";\nconsole.log(mark);\n",
    )
    .unwrap();

    let engine = oj_js::JsEngine::spawn(oj_js::EngineConfig::new(&root), None, None).unwrap();
    let out = engine
        .call(
            root.join("probe.mjs").to_string_lossy(),
            "run",
            vec![serde_json::json!(root.to_string_lossy())],
            None,
        )
        .await
        .expect("the probe runs");

    let aliased = out["aliased"].as_str().unwrap();
    assert!(
        aliased.contains("alias-mark") && !aliased.contains("@/lib"),
        "both alias entries resolve into the bundle:\n{aliased}"
    );
    // The control: the same entry without the plugin leaves the import out of
    // the bundle — the bare specifier the browser failed on.
    let unaliased = out["unaliased"].as_str().unwrap_or_default();
    assert!(
        !unaliased.contains("alias-mark"),
        "without the plugin rolldown cannot resolve the alias:\n{unaliased}"
    );
    // A miss keeps the rewritten id (Vite's alias plugins), so the bundle
    // fails naming it instead of externalizing the bare `@/` specifier.
    let broken = out["broken"]
        .as_str()
        .expect("a missing aliased import fails the bundle");
    assert!(
        broken.contains("alias-missing"),
        "the failure names the missing import:\n{broken}"
    );
}
