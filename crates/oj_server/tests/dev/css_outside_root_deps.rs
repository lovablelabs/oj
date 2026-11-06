// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A stylesheet's deps outside the root reach the watcher and the module
//! graph, as Vite adds a CSS module's deps to its watch files: an `@import`
//! of a sibling package's sheet, and a file a PostCSS plugin reports as a
//! `dependency` message. An edit to either refreshes the served sheet.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{require_fixture, App};

const SHEET: &str = "/src/app.css?direct";

/// A directory beside the app root, outside it.
fn sibling(app: &App) -> PathBuf {
    let name = format!("{}-shared", app.root.file_name().unwrap().to_string_lossy());
    let dir = app.root.with_file_name(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Rewrite `file` until the sheet shows `want`: a watch added on the fly
/// starts delivering a moment later. On macOS FSEvents restarts its stream,
/// which under the whole suite's churn has taken close to a minute.
async fn edit_until(server: &crate::Server, file: &Path, body: impl Fn(u32) -> String, want: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    for n in 0.. {
        std::fs::write(file, body(n)).unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        if server.text(SHEET).await.contains(want) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "sheet stayed stale after an outside-root edit"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn outside_root_import_edit_refreshes_the_sheet() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"css-outside-import","version":"1.0.0"}"#,
        ),
        ("index.html", crate::INDEX_HTML),
    ]);
    let shared = sibling(&app);
    let theme = shared.join("theme.css");
    std::fs::write(&theme, ".seed { margin: 0 }\n").unwrap();
    let name = shared.file_name().unwrap().to_string_lossy().into_owned();
    app.write(&[(
        "src/app.css",
        &format!("@import \"../../{name}/theme.css\";\n.app {{ margin: 0 }}\n"),
    )]);
    let server = app.serve().await;
    let _listening = server.listen().await;

    let first = server.text(SHEET).await;
    assert!(first.contains(".seed"), "seeded sheet: {first}");

    edit_until(
        &server,
        &theme,
        |n| format!(".edited{n} {{ margin: 0 }}\n"),
        ".edited",
    )
    .await;
    let _ = std::fs::remove_dir_all(&shared);
}

#[tokio::test(flavor = "multi_thread")]
async fn postcss_dependency_edit_refreshes_the_sheet() {
    require_fixture!("postcss");
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"css-outside-postcss","version":"1.0.0"}"#,
        ),
        ("index.html", crate::INDEX_HTML),
        ("src/app.css", ".app { margin: 0 }\n"),
    ]);
    app.link_fixture("postcss");
    let shared = sibling(&app);
    let tokens = shared.join("tokens.txt");
    std::fs::write(&tokens, "seed").unwrap();
    // A plugin reading a file outside the root, reported as PostCSS asks.
    app.write(&[(
        "postcss.config.mjs",
        &format!(
            "import {{ readFileSync }} from \"node:fs\";\n\
             const file = {:?};\n\
             export default {{ plugins: [{{ postcssPlugin: \"tokens\", Once(root, {{ result }}) {{\n\
             \x20 root.append(`.${{readFileSync(file, \"utf8\").trim()}} {{ margin: 0 }}`);\n\
             \x20 result.messages.push({{ type: \"dependency\", plugin: \"tokens\", file, parent: result.opts.from }});\n\
             }} }}] }};\n",
            tokens.display().to_string()
        ),
    )]);
    let server = app.serve().await;
    let _listening = server.listen().await;

    let first = server.text(SHEET).await;
    assert!(first.contains(".seed"), "seeded sheet: {first}");

    edit_until(&server, &tokens, |n| format!("edited{n}"), ".edited").await;
    let _ = std::fs::remove_dir_all(&shared);
}
