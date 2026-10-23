// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A load that fails names its cause in the build error, as Vite prints it.
//! rolldown reported a failed load as "plugin threw an error" and dropped
//! the error itself.

use super::{build_app, scratch, write_files};

const INDEX: &str = r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#;

#[tokio::test]
async fn a_stylesheet_that_cannot_compile_says_why() {
    // Sass, not Less/Stylus: those share one engine per process, bound to the
    // first build's root, and a root without the package would poison it.
    let root = scratch("load-error-sass");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"loaderr","version":"1.0.0"}"#),
            ("src/b.scss", ".box { color: $missing; }\n"),
            ("src/main.js", "import \"./b.scss\";\n"),
            ("index.html", INDEX),
        ],
    );
    let err = build_app(&root)
        .await
        .expect_err("the sheet uses an undefined variable");
    let msg = format!("{err:#}");
    assert!(msg.contains("Undefined variable"), "{msg}");
    // Sass names the sheet `./stdin`; the error's first line names the file.
    let sheet = root.join("src/b.scss").canonicalize().unwrap();
    assert!(
        msg.contains(&format!("[oj:build] Could not load {}", sheet.display())),
        "{msg}"
    );
    assert!(!msg.contains("╭─["), "no empty source box: {msg}");
}

#[tokio::test]
async fn a_throwing_plugin_load_says_why() {
    let root = scratch("load-error-plugin");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"loaderr","version":"1.0.0"}"#),
            ("src/main.js", "import \"virtual:boom\";\n"),
            ("index.html", INDEX),
            (
                "oj.plugins.mjs",
                r#"export default [{
    name: "bad-load",
    resolveId(id) { return id === "virtual:boom" ? "\0boom" : null; },
    load(id) { if (id === "\0boom") throw new Error("load exploded"); return null; },
  }];
"#,
            ),
        ],
    );
    let err = build_app(&root).await.expect_err("the plugin load throws");
    let msg = format!("{err:#}");
    assert!(msg.contains("load exploded"), "{msg}");
    assert!(msg.contains("bad-load"), "{msg}");
    assert!(msg.contains("Could not load \\0boom"), "{msg}");
}
