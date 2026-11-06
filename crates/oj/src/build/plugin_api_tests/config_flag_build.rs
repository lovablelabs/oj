// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The build half of the `--config` check: a `--config` naming a file that
//! does not exist fails the build with "failed to load config from ...",
//! as Vite's "Could not resolve config file" does, instead of silently
//! falling back to the root vite.config.

use crate::build::{build, CliOptions};

use super::{scratch, write_files};

#[tokio::test]
async fn missing_config_flag_fails_the_build_naming_the_file() {
    let root = scratch("config-flag-missing");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"cfg-app","version":"1.0.0"}"#),
            ("vite.config.mjs", "export default { plugins: [] };\n"),
            (
                ".config/vite.config.mjs",
                r#"import base from "../vite.config.mjs";
   export default {
     ...base,
     plugins: [
       ...(base.plugins ?? []),
       { name: "ovr", transformIndexHtml(h) { return h.replace("</head>", '<script>window.__OVR = 1;</script></head>'); } },
     ],
   };
"#,
            ),
            ("src.js", "window.__READY = true;\n"),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src.js"></script></body></html>"#,
            ),
        ],
    );
    // main.rs resolves a relative --config against the app root before build().
    let cli = CliOptions {
        config: Some(root.join(".config/nope.config.mjs")),
        ..Default::default()
    };
    let err = build(root.clone(), None, &tokio::runtime::Handle::current(), cli)
        .await
        .expect_err("a build with a missing --config must fail");
    let msg = format!("{err:#}");
    assert!(
        regex::Regex::new(r"failed to load config from .*nope\.config\.mjs")
            .unwrap()
            .is_match(&msg),
        "missing --config error:\n{msg}"
    );
}
