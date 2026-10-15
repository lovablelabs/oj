// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The build half of the `oj build --config` check: the named config is used
//! and the root vite.config is not; a `mode` the config file names is the
//! default mode (Vite: `inlineConfig.mode || config.mode || "production"`)
//! and the function config and `.env.<mode>` see it; `envPrefix` and
//! `resolve.extensions` from vite.config reach the build; `--mode` still wins.

use std::fs;
use std::path::Path;

use crate::build::{build, CliOptions};

use super::{files_under, scratch, write_files};

const ALT_CONFIG: &str = r#"export default ({ mode }) => ({
  mode: "staging",
  envPrefix: ["VITE_", "APP_"],
  resolve: { extensions: [".custom.js", ".js"] },
  css: { preprocessorOptions: { scss: { additionalData: "$brand: rgb(1, 2, 3);" } } },
  build: { outDir: "alt-out-" + mode },
});
"#;

/// Every file under `<root>/<out_dir>/assets`, concatenated.
fn assets(root: &Path, out_dir: &str) -> String {
    let dir = root.join(out_dir).join("assets");
    assert!(dir.is_dir(), "{} exists", dir.display());
    files_under(&dir)
        .into_iter()
        .map(|(_, code)| code)
        .collect::<Vec<_>>()
        .join("\n")
}

fn alt_config(root: &Path) -> CliOptions {
    // main.rs resolves a relative --config against the app root before build().
    CliOptions {
        config: Some(root.join("configs/vite.alt.js")),
        ..Default::default()
    }
}

#[tokio::test]
async fn config_flag_mode_env_prefix_and_extensions_reach_the_build() {
    let root = scratch("build-config-flag");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"cfgflag","version":"1.0.0"}"#),
            (".env", "APP_TOKEN=from-env\nVITE_SEEN=vite\n"),
            (".env.staging", "APP_TOKEN=staging-token\n"),
            ("src/styles.scss", ".box { color: $brand; }\n"),
            ("src/util.custom.js", "export const U = \"custom-ext\";\n"),
            (
                "src/main.js",
                "import { U } from \"./util\";\nwindow.__MODE = import.meta.env.MODE; window.__TOKEN = import.meta.env.APP_TOKEN; window.__U = U;\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
            // A decoy that must not be used when --config points elsewhere.
            (
                "vite.config.js",
                "export default { build: { outDir: \"decoy-out\" } };\n",
            ),
            ("configs/vite.alt.js", ALT_CONFIG),
        ],
    );

    if let Err(err) = build(root.clone(), None, alt_config(&root)).await {
        panic!("the --config build failed:\n{err:#}");
    }
    assert!(
        !root.join("decoy-out").exists(),
        "--config must not use the root vite.config"
    );
    assert!(
        root.join("alt-out-staging").exists(),
        "config-file `mode` is the default mode and the function config saw it"
    );
    let js = assets(&root, "alt-out-staging");
    assert!(
        regex::Regex::new(r#"[`"]staging[`"]"#)
            .unwrap()
            .is_match(&js),
        "import.meta.env.MODE is the config-file mode:\n{js}"
    );
    assert!(
        js.contains("staging-token"),
        ".env.staging loaded under the config-file mode with the APP_ envPrefix:\n{js}"
    );
    assert!(
        js.contains("custom-ext"),
        "resolve.extensions from vite.config resolved ./util to util.custom.js:\n{js}"
    );

    let _ = fs::remove_dir_all(root.join(".oj-cache"));
    if let Err(err) = build(root.clone(), Some("production"), alt_config(&root)).await {
        panic!("the --config --mode production build failed:\n{err:#}");
    }
    assert!(
        root.join("alt-out-production").exists(),
        "CLI --mode wins over the config-file mode"
    );
    let js = assets(&root, "alt-out-production");
    assert!(
        js.contains("from-env"),
        "production build reads .env only:\n{js}"
    );
}
