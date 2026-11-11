// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A module's new export through Start dev's hot updates. One change adds an
//! export to a plain data module and reads it from the component that imports
//! the module; whichever file the editor writes first, the open page shows
//! the new export's value in place. Written as two files, the change reaches
//! the engine as two updates and two patches; applied one after the other,
//! the component's patch re-ran it against the data module's exports from
//! before the edit: it read the new export as `undefined`, its render threw
//! on it (`.map`, a method call) into the nearest error boundary, or
//! unmounted the page without one, and the data module's patch, applied
//! next, had nothing left to refresh, while a fresh load rendered fine. The page now fetches every patch it has been sent,
//! the editor's gate having released them together, before it re-runs a
//! module, so each importer binds to the newest exports, as Vite's boundary
//! re-imports every module its graph invalidated. The browser half skips
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

const GATE: &[(&str, &str)] = &[("OJ_HMR_GATE", "1"), ("OJ_HMR_FULL_RELOAD", "false")];
const WIDGET: &str = "src/widgets/glob-widget.jsx";
const DATA: &str = "src/data/x.js";
const SHOWN: &str = "(document.querySelector('[data-testid=\"export-hot\"]')?.textContent ?? '')";

/// The fixture's widget reading `shown` from the data module, imported by
/// the names in `names`, or as a namespace when `names` is None.
fn widget(original: &str, names: Option<&str>, shown: &str) -> String {
    let import = match names {
        Some(names) => format!("import {{ {names} }} from \"../data/x\";\n"),
        None => "import * as data from \"../data/x\";\n".to_string(),
    };
    let span = format!(
        "<><span>{{\"jsxglob:\" + titles}}</span><span data-testid=\"export-hot\">{{\"exporthot:\" + {shown}}}</span></>"
    );
    let source = original.replacen("<span>{\"jsxglob:\" + titles}</span>", &span, 1);
    assert_ne!(source, original, "the widget's span moved");
    format!("{import}{source}")
}

fn write(root: &Path, rel: &str, source: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, source).unwrap();
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

/// Waits until the gate holds every file of `rels`.
async fn held(dev: &Server, rels: &[&str]) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let (_, status) = dev.request("GET", "/__hmr_gate", &[]);
        if rels
            .iter()
            .all(|rel| status.contains(&format!("/{rel}\":")))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the gate never held {rels:?}: {status}\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// An editor's change of several files, written one after the other (each
/// held by the gate before the next, so the engine sees each as an update of
/// its own), then flushed as one.
async fn change(dev: &Server, root: &Path, files: &[(&str, &str)]) {
    let mut written = Vec::new();
    for (rel, source) in files {
        write(root, rel, source);
        written.push(*rel);
        held(dev, &written).await;
    }
    let (status, body) = dev.request("POST", "/__hmr_flush", &[]);
    assert_eq!(status, 200, "flush: {body}");
}

/// The page shows `text` in the widget, in place (no reload), with no
/// uncaught error and no render error on the way.
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
    // A render that threw into the router's error boundary is a crash too:
    // its error component took the page over until the next patch.
    let rendered_errors: Vec<String> = page
        .console()
        .into_iter()
        .filter(|line| line.starts_with("error:"))
        .collect();
    assert!(
        rendered_errors.is_empty(),
        "a render threw on the way to {text:?}: {rendered_errors:?}"
    );
}

/// A data module gains an export and the component importing it reads the
/// new export, in one change: the open page shows its value whichever file
/// was written first, and a namespace import sees an export added to the
/// data module alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_export_read_by_its_edited_importer_reaches_an_open_page() {
    let Some(app) = app("hot-exports") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let original = std::fs::read_to_string(root.join(WIDGET)).unwrap();
    write(&root, DATA, "export const a = \"a1\";\n");
    write(&root, WIDGET, &widget(&original, Some("a"), "a"));
    let dev = Server::oj_dev_with(&root, GATE);
    let page = Page::open(&dev.url("/")).await;
    hydrated(&page).await;
    page.wait_for(&format!("{SHOWN} === 'exporthot:a1'"), WAIT)
        .await;
    page.eval::<bool>("(window.__mark = 'kept', true)").await;
    // A page far from its dev server: fetching a patch takes longer than the
    // refresh runtime waits before it re-renders, so a patch applied on its
    // own renders before the next one lands.
    page.emulate_latency(150).await;

    // (a) The component is written first, reading an export the data module
    // gains in the same change.
    change(
        &dev,
        &root,
        &[
            (
                WIDGET,
                &widget(&original, Some("a, b"), "a + \":\" + b.toUpperCase()"),
            ),
            (DATA, "export const a = \"a1\";\nexport const b = \"b1\";\n"),
        ],
    )
    .await;
    shows(&page, &dev, "exporthot:a1:B1").await;
    assert!(
        dev.logs("oj start: hot updated"),
        "the change was not a hot update\n{}",
        dev.output()
    );

    // (b) The data module is written first.
    change(
        &dev,
        &root,
        &[
            (
                DATA,
                "export const a = \"a1\";\nexport const b = \"b1\";\nexport const c = \"c1\";\n",
            ),
            (
                WIDGET,
                &widget(
                    &original,
                    Some("a, b, c"),
                    "a + \":\" + b + \":\" + c.toUpperCase()",
                ),
            ),
        ],
    )
    .await;
    shows(&page, &dev, "exporthot:a1:b1:C1").await;

    // (c) The component reads the module as a namespace; the data module
    // alone gains an export, and the namespace the component holds has it.
    change(
        &dev,
        &root,
        &[(
            WIDGET,
            &widget(&original, None, "Object.keys(data).sort().join(\",\")"),
        )],
    )
    .await;
    shows(&page, &dev, "exporthot:a,b,c").await;
    change(
        &dev,
        &root,
        &[(
            DATA,
            "export const a = \"a1\";\nexport const b = \"b1\";\nexport const c = \"c1\";\nexport const d = \"d1\";\n",
        )],
    )
    .await;
    shows(&page, &dev, "exporthot:a,b,c,d").await;
    page.close().await;
}
