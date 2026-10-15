// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The build half of the workspace-root postcss config check: a
//! `postcss.config.mjs` at the workspace root (postcss-load-config's search,
//! stopping at the workspace root) runs on a package's Sass output in
//! `oj build`, with `postcss` resolved from the root's node_modules.

use std::path::Path;

use regex::Regex;

use super::{build_app, files_under, scratch, write_files};

#[tokio::test]
async fn workspace_root_postcss_config_runs_on_sass_output_in_build() {
    let node_modules =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../e2e/fixtures/start-app/node_modules");
    if super::fixture("postcss").is_none() {
        assert!(
            std::env::var_os("OJ_REQUIRE_FIXTURES").is_none(),
            "OJ_REQUIRE_FIXTURES is set but postcss is not installed"
        );
        eprintln!("skipping: fixture postcss not installed");
        return;
    }
    let ws = scratch("postcss-config-discovery");
    write_files(
        &ws,
        &[
            (
                "package.json",
                r#"{"name":"ws","private":true,"workspaces":["packages/*"]}"#,
            ),
            (
                "postcss.config.mjs",
                "export default { plugins: [{ postcssPlugin: \"oj-marker\", Once(root) { root.append(\".from-postcss { color: green; }\"); } }] };\n",
            ),
            ("packages/web/package.json", r#"{"name":"web","version":"1.0.0"}"#),
            ("packages/web/src/a.scss", "$c: red;\n.a { color: $c; }\n"),
            ("packages/web/src/main.js", "import \"./a.scss\";\nwindow.__OK = true;\n"),
            (
                "packages/web/index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
        ],
    );
    std::fs::create_dir_all(ws.join(".git")).unwrap();
    std::os::unix::fs::symlink(
        node_modules.canonicalize().unwrap(),
        ws.join("node_modules"),
    )
    .unwrap();
    let app = ws.join("packages/web");
    build_app(&app).await.expect("the workspace package builds");

    let built = files_under(&app.join("dist/assets"))
        .into_iter()
        .filter(|(p, _)| p.ends_with(".css") && !p.contains('/'))
        .map(|(_, c)| c)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        built.contains(".from-postcss"),
        "postcss applied in build:\n{built}"
    );
    assert!(
        Regex::new(r"color:\s*red").unwrap().is_match(&built),
        "sass compiled in build:\n{built}"
    );
}
