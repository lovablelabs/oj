// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! How the Start client bundles fail, run the way oj runs them: the shipped
//! rolldown-assets.mjs inside oj's engine, bundling with the REAL rolldown the
//! fixture brings. Vite's onRollupLog makes an unresolved import a build
//! error (never rolldown's warn-and-externalize, which shipped a bare
//! specifier and a blank page), the app's own `external` exempts an import,
//! and the error payload the overlay renders keeps the file and position.
//! Skips quietly when the fixture is not installed.

use std::path::Path;

const PROBE: &str = r#"import { buildErrorPayload, clientExternal, viteOnLog } from "./start/rolldown-assets.mjs";
import { rolldown } from "rolldown";

const bundle = async (input, options = {}) => {
  const build = await rolldown({ input, onLog: viteOnLog, ...options });
  try {
    const { output } = await build.generate({ format: "esm" });
    return output.map((o) => o.code ?? "").join("\n");
  } finally {
    await build.close();
  }
};

export async function run(root) {
  const out = {};
  try {
    await bundle(`${root}/src/typo.js`);
  } catch (e) {
    out.payload = buildErrorPayload(e);
  }
  const config = { build: { rolldownOptions: { external: ["cdn-only-dep"] } } };
  out.external = await bundle(`${root}/src/external.js`, clientExternal(config));
  // An environment's own list wins over the top-level one, as in Vite.
  out.envExternal = clientExternal({
    build: { rolldownOptions: { external: ["a"] } },
    environments: { client: { build: { rolldownOptions: { external: ["b"] } } } },
  }).external;
  out.none = clientExternal({});
  return out;
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn start_client_bundles_fail_on_unresolved_imports_like_vite() {
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
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("typo.js"),
        "import { thing } from \"no-such-dep-typo\";\nconsole.log(thing);\n",
    )
    .unwrap();
    std::fs::write(
        src.join("external.js"),
        "import { cdn } from \"cdn-only-dep\";\nconsole.log(cdn);\n",
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

    let payload = &out["payload"];
    let message = payload["message"]
        .as_str()
        .expect("an unresolved import fails the bundle");
    assert!(
        message.contains(r#"Rolldown failed to resolve import "no-such-dep-typo""#)
            && message.contains("build.rolldownOptions.external"),
        "Vite's onRollupLog message:\n{message}"
    );
    assert!(
        payload["id"]
            .as_str()
            .is_some_and(|id| id.ends_with("src/typo.js")),
        "the payload names the importer: {payload}"
    );
    assert_eq!(payload["loc"]["line"], 1, "and the position: {payload}");
    let frame = payload["frame"].as_str().unwrap_or_default();
    // Vite's generateCodeFrame layout from the source, never rolldown's
    // "treating it as an external dependency" text (it is an error here).
    assert!(
        frame.starts_with("  1|  import { thing } from \"no-such-dep-typo\";")
            && frame.contains("\n   |  ")
            && frame.contains('^')
            && !frame.contains("external dependency")
            && !frame.contains('\u{1b}'),
        "the frame is the source excerpt with a caret:\n{frame}"
    );
    assert_eq!(
        payload["stack"], "",
        "no stack from oj's own throw site: {payload}"
    );

    let external = out["external"].as_str().unwrap();
    assert!(
        external.contains(r#"from "cdn-only-dep""#),
        "an app-declared external stays a bare import, never an error:\n{external}"
    );
    assert_eq!(out["envExternal"], serde_json::json!(["b"]));
    assert_eq!(out["none"], serde_json::json!({}));
}
