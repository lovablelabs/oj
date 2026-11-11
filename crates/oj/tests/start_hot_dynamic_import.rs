// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A dynamic import inside a module Start dev hot-updated. A component's
//! effect does `await import("../lib/x")` for a module the page has not
//! loaded yet (its own chunk, with a dependency of its own); once the
//! component is hot-updated, the patched code reaches that module through
//! the page's runtime, which used to throw `No factory registered for module
//! src/lib/x.ts`: the module was never run by the page, and no patch carries
//! a module that did not change. The runtime now fetches the chunk the
//! current client bundle holds it in, as Vite serves the module behind a
//! dynamic import on demand, so the import resolves, to the module's current
//! code even when it was edited before it was ever loaded. The browser half
//! skips without Chrome, unless `OJ_REQUIRE_BROWSER` is set.

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
const LAZY: &str = "src/lib/lazy-label.ts";
const SHOWN: &str = "(document.querySelector('[data-testid=\"lazy-hot\"]')?.textContent ?? '')";

/// The fixture's widget with a button whose click runs an effect that
/// dynamically imports the lazy module and shows what it exports (or the
/// error the import threw); `tag` tells one version of the widget from the
/// next.
fn widget(original: &str, tag: &str) -> String {
    let hook = concat!(
        "const [opens, setOpens] = useState(0);\n",
        "  const [label, setLabel] = useState(\"\");\n",
        "  useEffect(() => {\n",
        "    if (!opens) return;\n",
        "    let live = true;\n",
        "    (async () => {\n",
        "      try {\n",
        "        const m = await import(\"../lib/lazy-label\");\n",
        "        if (live) setLabel(m.lazyLabel());\n",
        "      } catch (e) {\n",
        "        if (live) setLabel(\"failed:\" + e.message);\n",
        "      }\n",
        "    })();\n",
        "    return () => {\n",
        "      live = false;\n",
        "    };\n",
        "  }, [opens]);\n",
        "  ",
    );
    let span = format!(
        "<><span>{{\"jsxglob:\" + titles}}</span>\
         <button data-testid=\"lazy-open\" onClick={{() => setOpens((n) => n + 1)}}>open</button>\
         <span data-testid=\"lazy-hot\">{{\"lazyhot:{tag}:\" + label}}</span></>"
    );
    let source = original.replacen("<span>{\"jsxglob:\" + titles}</span>", &span, 1);
    assert_ne!(source, original, "the widget's span moved");
    let source = source.replacen("return ", &format!("{hook}return "), 1);
    format!("import {{ useEffect, useState }} from \"react\";\n{source}")
}

/// The lazy module: a dependency the entry never imports, so its chunk is
/// the module's own, and a label naming the version.
fn lazy(version: &str) -> String {
    format!(
        "import clsx from \"clsx\";\nexport function lazyLabel() {{\n  return clsx(\"lazy\", \"{version}\");\n}}\n"
    )
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
    page.eval::<bool>("(window.__mark = 'kept', true)").await;
}

/// Clicks the widget's button: its effect imports the lazy module.
async fn open(page: &Page) {
    page.eval::<bool>("(document.querySelector('[data-testid=\"lazy-open\"]').click(), true)")
        .await;
}

/// The page shows `text` in the widget, in place (no reload), with no
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

/// Waits for the next "hot updated" line past `before` of them.
async fn hot_updated(dev: &Server, before: usize) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while dev.output().matches("oj start: hot updated").count() <= before {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the edit was not a hot update\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn hot_updates(dev: &Server) -> usize {
    dev.output().matches("oj start: hot updated").count()
}

/// A hot-updated component's dynamic import of a module the page never
/// loaded resolves; the lazy module's own edits reach the page too, loaded
/// or not.
#[tokio::test(flavor = "multi_thread")]
async fn a_patched_components_dynamic_import_of_an_unloaded_module_resolves() {
    let Some(app) = app("hot-dynamic-import") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let original = std::fs::read_to_string(root.join(WIDGET)).unwrap();
    write(&root, LAZY, &lazy("v1"));
    write(&root, WIDGET, &widget(&original, "w1"));
    let dev = Server::oj_dev(&root);
    let page = Page::open(&dev.url("/")).await;
    hydrated(&page).await;
    page.wait_for(&format!("{SHOWN} === 'lazyhot:w1:'"), WAIT)
        .await;

    // (a) The component is hot-updated before the page ever imported the
    // lazy module; the first import happens through the patched code.
    let seen = hot_updates(&dev);
    write(&root, WIDGET, &widget(&original, "w2"));
    shows(&page, &dev, "lazyhot:w2:").await;
    hot_updated(&dev, seen).await;
    open(&page).await;
    shows(&page, &dev, "lazyhot:w2:lazy v1").await;

    // (b) The lazy module, loaded now, is edited: the next import sees the
    // new code.
    let seen = hot_updates(&dev);
    write(&root, LAZY, &lazy("v2"));
    hot_updated(&dev, seen).await;
    open(&page).await;
    shows(&page, &dev, "lazyhot:w2:lazy v2").await;

    // (c) A second page never imported the lazy module. The lazy module is
    // edited (nothing to re-run on that page), then the component: that
    // page's first import, through the patched component, gets the lazy
    // module as it is now; the first page, which holds it, gets its patch.
    let second = Page::open(&dev.url("/")).await;
    hydrated(&second).await;
    second
        .wait_for(&format!("{SHOWN} === 'lazyhot:w2:'"), WAIT)
        .await;
    let seen = hot_updates(&dev);
    write(&root, LAZY, &lazy("v3"));
    hot_updated(&dev, seen).await;
    let seen = hot_updates(&dev);
    write(&root, WIDGET, &widget(&original, "w3"));
    shows(&second, &dev, "lazyhot:w3:").await;
    hot_updated(&dev, seen).await;
    open(&second).await;
    shows(&second, &dev, "lazyhot:w3:lazy v3").await;
    open(&page).await;
    shows(&page, &dev, "lazyhot:w3:lazy v3").await;
    second.close().await;
    page.close().await;
}
