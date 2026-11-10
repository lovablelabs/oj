// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The rebundle gate's input set follows the bundle's module graph: a module
//! the client bundle gained through a hot update (a component an edit
//! imported for the first time, a new route and the components it imports)
//! is an input of the very next edit, as in Vite, where a module joins the
//! graph the moment it is first transformed and a change to it propagates
//! from then on (server/moduleGraph.ts, `ensureEntryFromUrl`;
//! server/hmr.ts, `handleHMRUpdate`). It used to stay out of the set the
//! gate consults until the next full output was written (two seconds after
//! edits pause), so an edit to it in between was "outside every served
//! graph": no rebundle, and a fresh load got the stale chunk.
//! The browser tests skip without Chrome, unless `OJ_REQUIRE_BROWSER` is set.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;
#[allow(dead_code)]
mod start_app;

use std::path::Path;
use std::time::Duration;

use browser::Page;
use start_app::{app, Server, WAIT};

const IMPORTER: &str = "src/widgets/glob-widget.jsx";
const WIDGET: &str = "src/widgets/fresh-widget.tsx";
const ROUTE: &str = "src/routes/fresh.tsx";
const ROUTE_WIDGET: &str = "src/widgets/fresh-route-widget.tsx";
const TREE: &str = "src/routeTree.ts";

/// An edit's own time to reach the output: well under the suite's wait, so
/// a change the gate drops fails the test instead of stalling it.
const EDIT: Duration = Duration::from_secs(45);

fn widget(name: &str, marker: &str) -> String {
    format!(
        "export function {name}() {{\n  return <span>{{{}}}</span>;\n}}\n",
        serde_json::to_string(marker).unwrap()
    )
}

/// Waits until `page` hydrated: the marker alone is server-rendered, before
/// the client runs.
async fn hydrated(page: &Page) {
    page.wait_for(
        "document.querySelector('[data-testid=\"client-mounted\"]')?.textContent.trim() === 'client-mounted-ok'",
        WAIT,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
}

/// The page's text shows `needle` within `within`, through a reload or a
/// patch.
async fn shows(page: &Page, needle: &str, within: Duration, dev: &Server) {
    let predicate = format!(
        "document.body.textContent.includes({})",
        serde_json::to_string(needle).unwrap()
    );
    assert!(
        page.poll(&predicate, within).await,
        "the page never showed {needle:?}; errors: {:?}; console: {:?}\n{}",
        page.errors(),
        page.console(),
        dev.output()
    );
}

/// Waits for the dev server to log `needle` past `before`, within `within`.
async fn logged(dev: &Server, before: usize, needle: &str, within: Duration) {
    let deadline = tokio::time::Instant::now() + within;
    while !dev.output()[before..].contains(needle) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the dev server never logged {needle:?}:\n{}",
            &dev.output()[before..]
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Waits until the dev server reports a batch past `before` as handled: a
/// hot update, or a rebuild (reloading, or held for the editor's flush).
async fn went_out(dev: &Server, before: usize) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let since = dev.output()[before..].to_string();
        if since.contains("oj start: hot updated") || since.contains("oj start: rebuilt,") {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the dev server never handled the batch:\n{since}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Whether a served client chunk carries `needle`: what a fresh load of the
/// page gets, as opposed to what an open page was patched with.
fn output_has(root: &Path, needle: &str) -> bool {
    let chunks = root.join(".oj-cache/v1/start/client-chunks");
    let Ok(entries) = std::fs::read_dir(&chunks) else {
        return false;
    };
    entries.flatten().any(|e| {
        e.path().extension().is_some_and(|x| x == "js")
            && std::fs::read_to_string(e.path()).is_ok_and(|s| s.contains(needle))
    })
}

/// The edit to `path` made since `before` reached the served output, and
/// was never passed over by the gate.
async fn reached(dev: &Server, root: &Path, before: usize, path: &str, needle: &str) {
    let deadline = tokio::time::Instant::now() + EDIT;
    while !output_has(root, needle) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the served chunks never carried {needle:?} after the edit to {path}:\n{}",
            &dev.output()[before..]
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let since = dev.output()[before..].to_string();
    assert!(
        !since.contains("change outside every served graph"),
        "the edit to {path} missed the gate:\n{since}"
    );
}

/// A component an edit imported for the first time is an input of the very
/// next edit: edited right after the hot update that brought it in, before
/// the output refresh that follows a pause, the edit still rebundles and
/// reaches the served chunks.
#[tokio::test(flavor = "multi_thread")]
async fn a_module_a_hot_update_brought_in_is_an_input_of_the_next_edit() {
    let Some(app) = app("served-graph-import") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let dev = Server::oj_dev(&root);
    let (status, _) = dev.request("GET", "/", &[]);
    assert_eq!(status, 200, "{}", dev.output());
    let page = Page::open(&dev.url("/")).await;
    hydrated(&page).await;

    // The widget first, then the import from a component the bundle already
    // has (a Fast Refresh boundary, so the update is a patch, not a reload):
    // the batch that brings the widget into the bundle is a hot update.
    std::fs::write(root.join(WIDGET), widget("FreshWidget", "fresh-widget:v1")).unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let importer = std::fs::read_to_string(root.join(IMPORTER)).unwrap();
    let imported = format!(
        "import {{ FreshWidget }} from \"./fresh-widget\";\n{}",
        importer.replace(
            "return <span>{\"jsxglob:\" + titles}</span>;",
            "return <span>{\"jsxglob:\" + titles}<FreshWidget /></span>;"
        )
    );
    assert_ne!(
        imported,
        format!("import {{ FreshWidget }} from \"./fresh-widget\";\n{importer}"),
        "the widget's importer gained the import"
    );
    let before = dev.output().len();
    let _: bool = page.eval("(window.__served_graph_marker = 1, true)").await;
    std::fs::write(root.join(IMPORTER), imported).unwrap();
    logged(&dev, before, "oj start: hot updated", WAIT).await;
    shows(&page, "fresh-widget:v1", WAIT, &dev).await;
    let kept: bool = page.eval("window.__served_graph_marker === 1").await;
    assert!(
        kept,
        "the update was a patch, not a reload:\n{}",
        dev.output()
    );

    // Right after the hot update: the new module is an input already.
    let before = dev.output().len();
    std::fs::write(root.join(WIDGET), widget("FreshWidget", "fresh-widget:v2")).unwrap();
    reached(&dev, &root, before, WIDGET, "fresh-widget:v2").await;
    shows(&page, "fresh-widget:v2", EDIT, &dev).await;
    page.close().await;
}

/// A new route file and the component it imports join the bundle together;
/// an edit to that component right afterwards reaches the served chunks.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_routes_imports_are_inputs_of_the_next_edit() {
    let Some(app) = app("served-graph-route") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let dev = Server::oj_dev(&root);
    let (status, _) = dev.request("GET", "/", &[]);
    assert_eq!(status, 200, "{}", dev.output());
    let page = Page::open(&dev.url("/")).await;
    hydrated(&page).await;

    std::fs::write(
        root.join(ROUTE_WIDGET),
        widget("FreshRouteWidget", "fresh-route:v1"),
    )
    .unwrap();
    std::fs::write(
        root.join(ROUTE),
        "import { createRoute } from \"@tanstack/react-router\";\n\
         import { rootRoute } from \"./__root\";\n\
         import { FreshRouteWidget } from \"../widgets/fresh-route-widget\";\n\
         \n\
         export const freshRoute = createRoute({\n\
           getParentRoute: () => rootRoute,\n\
           path: \"/fresh\",\n\
           component: () => (\n\
             <main>\n\
               <h1 className=\"fixture-heading\">fresh-page-marker</h1>\n\
               <p data-testid=\"fresh-route\"><FreshRouteWidget /></p>\n\
             </main>\n\
           ),\n\
         });\n",
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let tree = std::fs::read_to_string(root.join(TREE)).unwrap();
    let grown = tree
        .replace(
            "import { aboutRoute } from \"./routes/about\";",
            "import { aboutRoute } from \"./routes/about\";\nimport { freshRoute } from \"./routes/fresh\";",
        )
        .replace("[indexRoute, aboutRoute,", "[indexRoute, aboutRoute, freshRoute,");
    assert_ne!(grown, tree, "the route tree gained the route");
    let before = dev.output().len();
    std::fs::write(root.join(TREE), grown).unwrap();
    // The route and its component are in the bundle once the tree's change
    // went out, as a patch or a rebuild.
    went_out(&dev, before).await;

    let before = dev.output().len();
    std::fs::write(
        root.join(ROUTE_WIDGET),
        widget("FreshRouteWidget", "fresh-route:v2"),
    )
    .unwrap();
    reached(&dev, &root, before, ROUTE_WIDGET, "fresh-route:v2").await;
    page.close().await;
    let page = Page::open(&dev.url("/fresh")).await;
    shows(&page, "fresh-route:v2", WAIT, &dev).await;
    page.close().await;
}
