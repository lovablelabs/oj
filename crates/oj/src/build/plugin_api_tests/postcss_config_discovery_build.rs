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

/// The build half of the dev `a_postcss_config_runs_on_vites_postcss` check:
/// with only vite in the app's node_modules (a strict layout), the PostCSS
/// config runs on vite's own postcss, as Vite's css.ts does.
#[tokio::test]
async fn postcss_runs_on_vites_own_postcss_in_build() {
    let Some(vite) = super::fixture("vite") else {
        assert!(
            std::env::var_os("OJ_REQUIRE_FIXTURES").is_none(),
            "OJ_REQUIRE_FIXTURES is set but vite is not installed"
        );
        eprintln!("skipping: fixture vite not installed");
        return;
    };
    let root = scratch("postcss-from-vite");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"postcss-from-vite","version":"1.0.0"}"#),
            (
                "postcss.config.mjs",
                "export default { plugins: [{ postcssPlugin: \"oj-marker\", Once(root) { root.append(\".from-postcss { color: green; }\"); } }] };\n",
            ),
            ("src/app.css", ".app { color: red; }\n"),
            ("src/main.js", "import \"./app.css\";\n"),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
        ],
    );
    std::fs::create_dir_all(root.join("node_modules")).unwrap();
    std::os::unix::fs::symlink(&vite, root.join("node_modules/vite")).unwrap();
    build_app(&root).await.expect("the app builds");

    let built = files_under(&root.join("dist/assets"))
        .into_iter()
        .filter(|(p, _)| p.ends_with(".css"))
        .map(|(_, c)| c)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        built.contains(".from-postcss"),
        "the config ran on vite's postcss in build:\n{built}"
    );
}
