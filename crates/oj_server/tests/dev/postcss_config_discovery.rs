// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! PostCSS config discovery like postcss-load-config, through the dev server:
//! a `postcss.config.mjs` at the WORKSPACE root (not the package) applies to a
//! package in `packages/web`, with `postcss` resolved from the root's
//! node_modules, and it runs on Sass output. `find_postcss_config`'s search
//! order is pinned by `postcss_config_is_found_like_postcss_load_config`; this
//! pins that the found config actually runs. The build half is oj's
//! `postcss_config_discovery_build`.

use regex::Regex;

use crate::{App, INDEX_HTML};

#[tokio::test(flavor = "multi_thread")]
async fn workspace_root_postcss_config_runs_on_sass_output() {
    crate::require_fixture!("postcss");
    let ws = App::new(&[
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
        ("packages/web/index.html", INDEX_HTML),
    ]);
    std::fs::create_dir_all(ws.root.join(".git")).unwrap();
    // The whole fixture install as the root's node_modules: postcss needs
    // its own deps beside it.
    let node_modules = crate::fixture("postcss")
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    std::os::unix::fs::symlink(node_modules, ws.root.join("node_modules")).unwrap();

    let mut dev = ws.dev();
    dev.root = ws.root.join("packages/web");
    let server = crate::serve(dev).await;
    let css = server.text("/src/a.scss?direct").await;
    assert!(
        Regex::new(r"color:\s*red").unwrap().is_match(&css),
        "sass compiled:\n{css}"
    );
    assert!(
        css.contains(".from-postcss"),
        "workspace-root postcss config applied after sass (dev):\n{css}"
    );
}
