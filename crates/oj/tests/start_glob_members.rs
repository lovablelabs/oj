// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A file created or deleted where a module's `import.meta.glob` looks
//! changes what that module expands to, with no edit to the module itself.
//! The server render, the client bundle and an open page all follow: the
//! glob's importer re-expands, as Vite's import-glob plugin adds the glob's
//! importers to such an update (its `hotUpdate`). A `?raw` glob is the shape
//! the bundler has no module for by the member's bare path, so it is the one
//! that used to keep the stale expansion (and fail the next rebundle on the
//! member it still imported). The browser half skips without Chrome, unless
//! `OJ_REQUIRE_BROWSER` is set.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;
#[allow(dead_code)]
mod start_app;

use std::path::Path;
use std::time::{Duration, Instant};

use browser::Page;
use start_app::{app, Server, WAIT};

/// The client bundle's entry, by the name the fixture's bundle gives it.
const ENTRY: &str = "/@oj-start/client-entry.js";

/// A widget listing the files of a `?raw` glob over a directory of its own.
const NOTES_WIDGET: &str = r#"const notes = import.meta.glob("../notes/*.md", { query: "?raw", eager: true, import: "default" });
export function Notes() {
  const names = Object.keys(notes).map((k) => k.split("/").pop()).sort().join(",");
  return <span data-testid="raw-notes">{"rawnotes:" + names}</span>;
}
"#;

/// The fixture app with the notes widget on its home route and two notes.
fn with_notes(root: &Path) {
    let notes = root.join("src/notes");
    std::fs::create_dir_all(&notes).unwrap();
    std::fs::write(notes.join("a.md"), "# a\n").unwrap();
    std::fs::write(notes.join("b.md"), "# b\n").unwrap();
    std::fs::write(root.join("src/widgets/notes.jsx"), NOTES_WIDGET).unwrap();
    let index = root.join("src/routes/index.tsx");
    let source = std::fs::read_to_string(&index).unwrap();
    let source = source.replacen(
        "import { GlobWidget } from \"../widgets/glob-widget.jsx\";",
        "import { GlobWidget } from \"../widgets/glob-widget.jsx\";\nimport { Notes } from \"../widgets/notes.jsx\";",
        1,
    );
    let patched = source.replacen(
        "<p data-testid=\"glob-jsx\"><GlobWidget /></p>",
        "<p data-testid=\"glob-jsx\"><GlobWidget /></p>\n      <p><Notes /></p>",
        1,
    );
    assert_ne!(patched, source, "the fixture's glob widget moved");
    std::fs::write(&index, patched).unwrap();
}

/// The home document and the client entry, as served now.
fn served(dev: &Server) -> (String, String) {
    let (status, home) = dev.request("GET", "/", &[]);
    assert_eq!(status, 200, "{home}");
    let (status, entry) = dev.request("GET", ENTRY, &[]);
    assert_eq!(status, 200, "{entry}");
    (home, entry)
}

/// Polls until `done` holds for the served home document and client entry.
fn until(dev: &Server, what: &str, done: impl Fn(&str, &str) -> bool) {
    let deadline = Instant::now() + WAIT;
    loop {
        let (home, entry) = served(dev);
        if done(&home, &entry) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{what} never showed\n--- home\n{home}\n--- output\n{}",
            dev.output()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn no_bundle_failure(dev: &Server) {
    let output = dev.output();
    assert!(
        !output.contains("client bundle failed"),
        "a rebundle failed:\n{output}"
    );
}

#[test]
fn a_created_glob_member_reaches_the_render_and_the_bundle() {
    let Some(app) = app("glob-member-create") else {
        return;
    };
    let root = app.0.clone();
    with_notes(&root);
    let dev = Server::oj_dev(&root);
    let (home, entry) = served(&dev);
    assert!(home.contains("jsxglob:Alpha Widget|Beta Widget"), "{home}");
    assert!(home.contains("rawnotes:a.md,b.md"), "{home}");
    assert!(entry.contains("b.md") && !entry.contains("c.md"), "{entry}");

    std::fs::write(
        root.join("src/content/gamma.json"),
        r#"{ "title": "Gamma Widget" }"#,
    )
    .unwrap();
    std::fs::write(root.join("src/notes/c.md"), "# c\n").unwrap();
    until(&dev, "the created members", |home, entry| {
        home.contains("jsxglob:Alpha Widget|Beta Widget|Gamma Widget")
            && home.contains("rawnotes:a.md,b.md,c.md")
            && entry.contains("Gamma Widget")
            && entry.contains("c.md")
    });
    no_bundle_failure(&dev);
}

#[test]
fn a_deleted_glob_member_leaves_the_render_and_the_bundle() {
    let Some(app) = app("glob-member-delete") else {
        return;
    };
    let root = app.0.clone();
    with_notes(&root);
    let dev = Server::oj_dev(&root);
    let (home, entry) = served(&dev);
    assert!(home.contains("rawnotes:a.md,b.md"), "{home}");
    assert!(
        entry.contains("Beta Widget") && entry.contains("b.md"),
        "{entry}"
    );

    std::fs::remove_file(root.join("src/content/beta.json")).unwrap();
    std::fs::remove_file(root.join("src/notes/b.md")).unwrap();
    until(&dev, "the deletions", |home, entry| {
        home.contains("jsxglob:Alpha Widget<")
            && home.contains("rawnotes:a.md<")
            && !entry.contains("Beta Widget")
            && !entry.contains("b.md")
    });
    no_bundle_failure(&dev);
    // The next edit bundles against the current members, not the deleted one.
    let widget = root.join("src/widgets/notes.jsx");
    let source = std::fs::read_to_string(&widget).unwrap();
    std::fs::write(&widget, source.replace("\"rawnotes:\"", "\"notes:\"")).unwrap();
    until(&dev, "the edit after the deletion", |home, entry| {
        home.contains("notes:a.md<") && entry.contains("\"notes:\"")
    });
    no_bundle_failure(&dev);
}

/// An open page sees the members come and go, with no manual refresh.
#[tokio::test(flavor = "multi_thread")]
async fn an_open_page_follows_the_glob_members() {
    let Some(app) = app("glob-member-page") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    with_notes(&root);
    let dev = Server::oj_dev(&root);
    let page = Page::open(&dev.url("/")).await;
    const NOTES: &str =
        "(document.querySelector('[data-testid=\"raw-notes\"]')?.textContent ?? '')";
    page.wait_for(
        "document.querySelector('[data-testid=\"client-mounted\"]')?.textContent.trim() === 'client-mounted-ok'",
        WAIT,
    )
    .await;
    assert_eq!(page.eval::<String>(NOTES).await, "rawnotes:a.md,b.md");

    std::fs::write(root.join("src/notes/c.md"), "# c\n").unwrap();
    page.wait_for(&format!("{NOTES} === 'rawnotes:a.md,b.md,c.md'"), WAIT)
        .await;

    std::fs::remove_file(root.join("src/notes/b.md")).unwrap();
    page.wait_for(&format!("{NOTES} === 'rawnotes:a.md,c.md'"), WAIT)
        .await;
    no_bundle_failure(&dev);
}
