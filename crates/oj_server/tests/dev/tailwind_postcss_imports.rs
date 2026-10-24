// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A Tailwind sheet under a PostCSS config has its `@import`s inlined before
//! the app's PostCSS runs, as Vite's postcss-import does: the inlined files
//! are the sheet's dependencies (an edit to one, `..`-spelled or outside the
//! root, updates the sheet), their urls point at served paths, `layer()`
//! imports keep Tailwind's layer order, and a partial's relative `@plugin`
//! still finds its file.

use crate::listen::frame_matching;
use crate::App;

/// The whole fixture install as the app's node_modules: PostCSS and Tailwind
/// need their own deps beside them.
fn link_fixture_node_modules(app: &App) {
    let node_modules = crate::fixture("postcss")
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    std::os::unix::fs::symlink(node_modules, app.root.join("node_modules")).unwrap();
}

/// Whether `frame` is an update naming `path` (any query).
fn updates(frame: &serde_json::Value, path: &str) -> bool {
    frame["type"] == "update"
        && frame["updates"].as_array().is_some_and(|u| {
            u.iter().any(|u| {
                u["path"]
                    .as_str()
                    .is_some_and(|p| p.split('?').next() == Some(path))
            })
        })
}

#[tokio::test(flavor = "multi_thread")]
async fn inlined_files_hot_update_the_sheet_and_serve_root_urls() {
    crate::require_fixture!("postcss");
    // A directory beside the app, outside its root.
    let run = crate::run_dir().join(format!("shared-{}", std::process::id()));
    std::fs::create_dir_all(&run).unwrap();
    std::fs::write(run.join("s.css"), ".from-shared { color: red; }\n").unwrap();
    let shared = format!("{}/s.css", run.display());
    let app = App::new(&[
        ("package.json", r#"{"name":"tw-deps","version":"1.0.0"}"#),
        (
            "postcss.config.mjs",
            "export default { plugins: [{ postcssPlugin: \"oj-marker\", Once(root) { root.append(\".from-postcss { color: green; }\"); } }] };\n",
        ),
        ("src/styles/c.css", ".from-c { color: red; }\n"),
        (
            "src/styles/nested/b.css",
            "@import \"../c.css\";\n.from-b { background: url(./img.png); }\n",
        ),
        ("src/main.js", "import \"./index.css\";\n"),
        ("index.html", crate::INDEX_HTML),
    ]);
    std::fs::write(app.root.join("src/styles/nested/img.png"), [7u8; 100]).unwrap();
    // The out-of-root import, spelled from the sheet as written.
    let up = pathdiff::diff_paths(&shared, app.root.join("src")).unwrap();
    let index = format!(
        "@import \"{}\";\n@import \"./styles/nested/b.css\";\n@tailwind utilities;\n.app {{ margin: 0; }}\n",
        up.display()
    );
    app.write(&[("src/index.css", &index)]);
    link_fixture_node_modules(&app);
    let server = app.serve().await;
    let listening = server.listen().await;
    let (mut ws, _) = server.ws(listening.addr, "/__ws", None).await;

    let css = server.text("/src/index.css?direct").await;
    for rule in [
        ".from-shared",
        ".from-b",
        ".from-c",
        ".app",
        ".from-postcss",
    ] {
        assert!(css.contains(rule), "{rule} served:\n{css}");
    }
    assert!(
        css.contains("url(\"/src/styles/nested/img.png\")"),
        "an inlined file's url points at its served path:\n{css}"
    );

    app.write(&[("src/styles/c.css", ".from-c-edited { color: red; }\n")]);
    frame_matching(&mut ws, "update of the sheet for a ../ import", |f| {
        updates(f, "/src/index.css")
    })
    .await;
    assert!(server
        .text("/src/index.css?direct")
        .await
        .contains(".from-c-edited"));

    std::fs::write(run.join("s.css"), ".from-shared-edited { color: red; }\n").unwrap();
    frame_matching(
        &mut ws,
        "update of the sheet for an out-of-root import",
        |f| updates(f, "/src/index.css"),
    )
    .await;
    assert!(server
        .text("/src/index.css?direct")
        .await
        .contains(".from-shared-edited"));
    crate::listen::close(ws).await;
    let _ = std::fs::remove_dir_all(&run);
}

#[tokio::test(flavor = "multi_thread")]
async fn tailwind_postcss_keeps_layer_order_and_partial_plugin_paths() {
    crate::require_fixture!("postcss", "tailwindcss", "@tailwindcss");
    let app = App::new(&[
        ("package.json", r#"{"name":"tw4-postcss","version":"1.0.0","type":"module"}"#),
        (
            "postcss.config.mjs",
            "export default { plugins: { \"@tailwindcss/postcss\": {} } };\n",
        ),
        (
            "src/index.css",
            "@import \"tailwindcss\";\n@import \"./styles/layer.css\" layer(components);\n@import \"./styles/sub/plugin.css\";\n",
        ),
        ("src/styles/layer.css", ".from-layer { margin: 1px; }\n"),
        ("src/styles/sub/plugin.css", "@plugin \"./plug.js\";\n"),
        (
            "src/styles/sub/plug.js",
            "export default function ({ addBase }) { addBase({ \".from-plugin\": { color: \"red\" } }); }\n",
        ),
        ("src/main.js", "import \"./index.css\";\n"),
        ("index.html", crate::INDEX_HTML),
    ]);
    link_fixture_node_modules(&app);
    let server = app.serve().await;
    let css = server.text("/src/index.css?direct").await;

    let order = css
        .find("@layer theme, base, components, utilities;")
        .unwrap_or_else(|| panic!("Tailwind's layer order statement:\n{css}"));
    let layer = css
        .find("@layer components {")
        .unwrap_or_else(|| panic!("layer() import inlined as a layer block:\n{css}"));
    assert!(
        order < layer,
        "the layer order statement precedes the imported layer:\n{css}"
    );
    assert!(css.contains(".from-layer"), "{css}");
    assert!(
        css.contains(".from-plugin"),
        "a partial's relative @plugin resolved from its own file:\n{css}"
    );
}

/// The inlined files are graph edges for HMR, Vite's file-only entries: the
/// page modulepreloads the crawled modules but never a partial, which on
/// its own would compile without its importer.
#[tokio::test(flavor = "multi_thread")]
async fn inlined_files_get_no_modulepreload() {
    crate::require_fixture!("postcss");
    let app = App::new(&[
        ("package.json", r#"{"name":"tw-preload","version":"1.0.0"}"#),
        (
            "postcss.config.mjs",
            "export default { plugins: [{ postcssPlugin: \"oj-marker\", Once() {} }] };\n",
        ),
        ("src/styles/a.css", ".from-a { color: red; }\n"),
        (
            "src/index.css",
            "@import \"./styles/a.css\";\n@tailwind utilities;\n",
        ),
        ("src/main.js", "import \"./index.css\";\n"),
        ("index.html", crate::INDEX_HTML),
    ]);
    link_fixture_node_modules(&app);
    let server = app.serve().await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    let html = loop {
        let html = server.text("/").await;
        if html.contains(r#"<link rel="modulepreload" href="/src/main.js""#) {
            break html;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the crawled entry was never preloaded:\n{html}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    assert!(
        html.contains(r#"<link rel="modulepreload" href="/src/index.css?import""#),
        "{html}"
    );
    assert!(
        !html.contains("/src/styles/a.css"),
        "an inlined file was preloaded:\n{html}"
    );
    assert!(server
        .text("/src/index.css?direct")
        .await
        .contains(".from-a"));
}
