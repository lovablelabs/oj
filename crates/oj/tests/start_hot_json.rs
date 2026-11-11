// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! JSON imports through Start dev's hot updates. A component edited to import
//! a JSON file, and a JSON file edited in place, both reach an open page as a
//! patch whose JSON bindings hold the current JSON value: the default export
//! (an object read as `data.url`, an array read with `.filter`) and the named
//! exports, as Vite's json plugin (plugins/json.ts) gives a JSON module. The
//! patch used to carry the JSON module as a bare expression with no exports,
//! so the patched component read `undefined.url`. The browser half skips
//! without Chrome, unless `OJ_REQUIRE_BROWSER` is set.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;
#[allow(dead_code)]
mod start_app;

use std::path::Path;
use std::time::Duration;

use browser::Page;
use start_app::{app, Server, WAIT};

const WIDGET: &str = "src/widgets/glob-widget.jsx";
const SHOWN: &str = "(document.querySelector('[data-testid=\"json-hot\"]')?.textContent ?? '')";

/// The fixture's widget with JSON imports added: an object read by a field, an
/// array filtered, and a named export.
fn widget_with_json(original: &str) -> String {
    let imports = concat!(
        "import photo from \"../data/photo.json\";\n",
        "import images from \"../data/images.json\";\n",
        "import { title as photoTitle } from \"../data/photo.json\";\n",
    );
    let shown = concat!(
        "<><span>{\"jsxglob:\" + titles}</span>",
        "<span data-testid=\"json-hot\">{\"jsonhot:\" + photo.url + \":\" + ",
        "images.filter((i) => i.url.endsWith(\".png\")).length + \":\" + photoTitle}</span></>",
    );
    let source = original.replacen("<span>{\"jsxglob:\" + titles}</span>", shown, 1);
    assert_ne!(source, original, "the widget's span moved");
    format!("{imports}{source}")
}

fn write_json(root: &Path, name: &str, json: &str) {
    std::fs::create_dir_all(root.join("src/data")).unwrap();
    std::fs::write(root.join("src/data").join(name), json).unwrap();
}

/// Waits until `page` hydrated and its hot client registered: an edit with no
/// hot client connected reloads instead of patching.
async fn hydrated(page: &Page) {
    page.wait_for(
        "document.querySelector('[data-testid=\"client-mounted\"]')?.textContent.trim() === 'client-mounted-ok'",
        WAIT,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
}

/// The page shows `text` in the JSON widget, in place (no reload), with no
/// uncaught error.
async fn shows(page: &Page, dev: &Server, text: &str) {
    let shown = page
        .poll(
            &format!("{SHOWN} === {}", serde_json::to_string(text).unwrap()),
            Duration::from_secs(40),
        )
        .await;
    assert!(
        shown,
        "the page never showed {text:?}: shows {:?}; page errors: {:?}; console: {:?}\n{}",
        page.eval::<String>(SHOWN).await,
        page.errors(),
        page.console(),
        dev.output()
            .lines()
            .filter(|l| l.contains("oj start:"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        page.eval::<bool>("window.__mark === 'kept'").await,
        "the page reloaded instead of taking the edit in place\n{}",
        dev.output()
    );
    assert!(
        page.errors().is_empty(),
        "the patch threw: {:?}",
        page.errors()
    );
}

/// A component hot-updated to import JSON files it never imported, then each
/// JSON file edited in place: every step lands on the open page as a hot
/// update holding the JSON's current value.
#[tokio::test(flavor = "multi_thread")]
async fn json_imports_added_and_edited_reach_an_open_page_as_hot_updates() {
    let Some(app) = app("hot-json") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let original = std::fs::read_to_string(root.join(WIDGET)).unwrap();
    // The JSON files exist before anything imports them, outside the
    // directory the fixture globs: new modules to the bundle once the
    // component imports them by name.
    write_json(
        &root,
        "photo.json",
        r#"{ "title": "Photo One", "url": "photo-v1" }"#,
    );
    write_json(
        &root,
        "images.json",
        r#"[{ "url": "a.png" }, { "url": "b.png" }, { "url": "c.svg" }]"#,
    );
    let dev = Server::oj_dev(&root);
    let page = Page::open(&dev.url("/")).await;
    hydrated(&page).await;
    page.eval::<bool>("(window.__mark = 'kept', true)").await;

    // (a) The component newly imports the JSON files.
    std::fs::write(root.join(WIDGET), widget_with_json(&original)).unwrap();
    shows(&page, &dev, "jsonhot:photo-v1:2:Photo One").await;
    assert!(
        dev.logs("oj start: hot updated"),
        "the edit was not a hot update\n{}",
        dev.output()
    );

    // (b) The object JSON changes in place: its default and named exports
    // follow.
    write_json(
        &root,
        "photo.json",
        r#"{ "title": "Photo Two", "url": "photo-v2" }"#,
    );
    shows(&page, &dev, "jsonhot:photo-v2:2:Photo Two").await;

    // (c) The array JSON changes in place.
    write_json(
        &root,
        "images.json",
        r#"[{ "url": "a.png" }, { "url": "b.png" }, { "url": "c.png" }, { "url": "d.svg" }]"#,
    );
    shows(&page, &dev, "jsonhot:photo-v2:3:Photo Two").await;
    page.close().await;
}
