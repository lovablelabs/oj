// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! An edit to a sheet another sheet `@import`s (inlined into it) refreshes
//! the importer on its next request whether or not an update goes out, as
//! Vite's watcher invalidates the module graph before, and apart from, HMR:
//! with `server.hmr: false`, and while the editor gate holds the edit.

use std::path::PathBuf;
use std::time::Duration;

use crate::App;

const SHEET: &str = "/src/app.css?direct";

/// `outside`: the imported sheet lives beside the root, not under it.
async fn importer_refreshes(config: (&str, &str), outside: bool) {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"css-importer","version":"1.0.0"}"#,
        ),
        config,
        ("index.html", crate::INDEX_HTML),
    ]);
    let (theme, spec): (PathBuf, String) = if outside {
        let name = format!("{}-shared", app.root.file_name().unwrap().to_string_lossy());
        let dir = app.root.with_file_name(&name);
        std::fs::create_dir_all(&dir).unwrap();
        (dir.join("theme.css"), format!("../../{name}/theme.css"))
    } else {
        (app.root.join("src/theme.css"), "./theme.css".to_string())
    };
    std::fs::create_dir_all(theme.parent().unwrap()).unwrap();
    std::fs::write(&theme, ".seed { margin: 0 }\n").unwrap();
    app.write(&[(
        "src/app.css",
        &format!("@import \"{spec}\";\n.app {{ margin: 0 }}\n"),
    )]);
    let server = app.serve().await;
    let _listening = server.listen().await;

    let first = server.text(SHEET).await;
    assert!(first.contains(".seed"), "seeded sheet: {first}");

    // Rewrite until the sheet shows the edit: a watch added on the fly (the
    // outside sheet's directory) starts delivering a moment later, on macOS
    // close to a minute under the whole suite's churn.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    for n in 0.. {
        std::fs::write(&theme, format!(".edited{n} {{ margin: 0 }}\n")).unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        if server.text(SHEET).await.contains(".edited") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "importer stayed stale after its @import target changed"
        );
    }
    if outside {
        let _ = std::fs::remove_dir_all(theme.parent().unwrap());
    }
}

const HMR_OFF: (&str, &str) = (
    "vite.config.mjs",
    "export default { server: { hmr: false } };\n",
);
const GATED: (&str, &str) = ("oj.config.json", r#"{"server":{"hmrGate":true}}"#);

#[tokio::test(flavor = "multi_thread")]
async fn hmr_off_importer_refreshes() {
    importer_refreshes(HMR_OFF, false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn hmr_off_importer_of_an_outside_sheet_refreshes() {
    importer_refreshes(HMR_OFF, true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn gated_importer_refreshes_before_the_flush() {
    importer_refreshes(GATED, false).await;
}
