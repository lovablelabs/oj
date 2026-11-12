// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `import Icon from "./icon.svg?react"` in dev: the svg becomes a React
//! component that renders an `<svg>` keeping the file's attributes
//! (`stroke-width` survives the JSX attribute rewrite), with the caller's
//! props spread onto it (`className` replaces the file's class). The build
//! half is oj's `svgr_build`. With vite-plugin-svgr in the config, its
//! `load` decides which svgs are components: configured for every `.svg`
//! (`include: "**/*.svg"`), a plain import is the component it makes, named
//! or default export; left at its default (`**/*.svg?react`), a plain import
//! stays the URL and `?react` is the plugin's component, not oj's.

use std::time::Duration;

use crate::browser::Page;
use crate::App;

const WAIT: Duration = Duration::from_secs(20);

const ICON: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" stroke-width="2" class="base"><path d="M4 4h16" stroke="currentColor"/></svg>"#;

const MAIN: &str =
    "import { createRoot } from \"react-dom/client\";\nimport Icon from \"./icon.svg?react\";\n\
const el = document.createElement(\"div\"); el.id = \"root\"; document.body.appendChild(el);\n\
createRoot(el).render(<Icon className=\"added\" data-testid=\"icon\" />);\n";

const INDEX: &str = r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.tsx"></script></body></html>"#;

#[tokio::test(flavor = "multi_thread")]
async fn svg_react_import_renders_component_with_props_in_dev() {
    if crate::browser::skip() {
        return;
    }
    crate::require_fixture!("react", "react-dom", "scheduler");
    let app = App::new(&[
        ("package.json", r#"{"name":"svgr-app","version":"1.0.0"}"#),
        ("src/icon.svg", ICON),
        ("src/main.tsx", MAIN),
        ("index.html", INDEX),
    ]);
    for pkg in ["react", "react-dom", "scheduler"] {
        app.link_fixture(pkg);
    }
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for(r##"document.querySelector("#root svg")"##, WAIT)
        .await;
    let outer: String = page
        .eval(r##"document.querySelector("#root svg").outerHTML"##)
        .await;
    assert!(outer.starts_with("<svg"), "no svg rendered: {outer}");
    assert!(
        outer.contains(r#"class="added""#),
        "props not spread onto svg: {outer}"
    );
    assert!(
        outer.contains(r#"stroke-width="2""#),
        "attribute rewrite lost stroke-width: {outer}"
    );
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}

/// The whole fixture install: vite-plugin-svgr imports `vite`, `@svgr/core`
/// and `@rollup/pluginutils`.
fn link_fixture_install(app: &App) {
    let node_modules = crate::fixture("vite-plugin-svgr")
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    std::os::unix::fs::symlink(node_modules, app.root.join("node_modules")).unwrap();
}

/// `path` as a module import fetches it: the browser's `sec-fetch-dest: script`.
async fn imported(server: &crate::Server, path: &str) -> crate::Response {
    server
        .request(
            axum::http::Request::get(path)
                .header("sec-fetch-dest", "script")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
}

/// An app whose vite.config lists `svgr(<options>)` and whose entry runs
/// `main` (TSX, React linked) over `src/logo.svg`.
fn svgr_app(options: &str, main: &str) -> App {
    let config = format!(
        "import svgr from \"vite-plugin-svgr\";\nexport default {{ plugins: [svgr({options})] }};\n"
    );
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"svgr-cfg","version":"1.0.0","type":"module"}"#,
        ),
        ("vite.config.js", &config),
        ("src/logo.svg", ICON),
        ("src/main.tsx", main),
        ("index.html", INDEX),
    ]);
    link_fixture_install(&app);
    app
}

const CRA_OPTIONS: &str = r#"{ include: "**/*.svg", svgrOptions: { exportType: "named", namedExport: "ReactComponent" } }"#;

/// Renders `Logo` into `#root` with a marker class.
fn render_main(import: &str) -> String {
    format!(
        "import {{ createRoot }} from \"react-dom/client\";\n{import}\n\
const el = document.createElement(\"div\"); el.id = \"root\"; document.body.appendChild(el);\n\
createRoot(el).render(<Logo className=\"added\" />);\n"
    )
}

// vite-plugin-svgr configured for every `.svg` (`include: "**/*.svg"`, the
// CRA convention with a named `ReactComponent` export): the plugin's `load`
// claims the plain `.svg` id, so the module is the component, not the URL.
#[tokio::test(flavor = "multi_thread")]
async fn svgr_plugin_configured_for_plain_svgs_serves_named_components() {
    crate::require_fixture!("vite-plugin-svgr", "@svgr/core", "vite");
    let app = svgr_app(
        CRA_OPTIONS,
        "import { ReactComponent as Logo } from \"./logo.svg\";\nwindow.__LOGO = Logo;\n",
    );
    let server = app.serve().await;
    let main = server.text("/src/main.tsx").await;
    assert!(
        main.contains("\"/src/logo.svg\""),
        "the importer keeps the plain svg import (no ?url):\n{main}"
    );
    let logo = imported(&server, "/src/logo.svg").await;
    assert_eq!(logo.status, 200, "{}", logo.body);
    assert!(
        logo.body.contains("as ReactComponent"),
        "the plugin's named export is served, not the URL module:\n{}",
        logo.body
    );
    assert!(
        !logo.body.contains("export default \"/src/logo.svg\""),
        "not the URL module:\n{}",
        logo.body
    );
    // The server render takes the plugin's load too (vite:asset's load runs
    // after it in every environment).
    let ssr = server
        .text(&format!(
            "/@ssr-module?id={}",
            app.root.join("src/logo.svg").display()
        ))
        .await;
    assert!(
        ssr.contains("ReactComponent"),
        "the ssr module is the plugin's component:\n{ssr}"
    );
    // The default-export shape over every `.svg` too.
    let app = svgr_app(
        r#"{ include: "**/*.svg", svgrOptions: { exportType: "default" } }"#,
        "import Logo from \"./logo.svg\";\nwindow.__LOGO = Logo;\n",
    );
    let server = app.serve().await;
    let logo = imported(&server, "/src/logo.svg").await;
    assert_eq!(logo.status, 200, "{}", logo.body);
    assert!(
        logo.body.contains("export default") && logo.body.contains("jsx"),
        "a component is the default export:\n{}",
        logo.body
    );
}

// The plugin's own default (`**/*.svg?react`): a plain import stays the URL
// (inlined, the file being small), as nothing claimed it, and `?react` is the
// plugin's component, served as its `load` made it: oj's own `?react` svgr
// only takes markup, so it does not wrap the plugin's code a second time.
#[tokio::test(flavor = "multi_thread")]
async fn svgr_plugin_default_react_query_is_the_plugins_component() {
    crate::require_fixture!("vite-plugin-svgr", "@svgr/core", "vite");
    let app = svgr_app(
        "",
        "import url from \"./logo.svg\";\nimport Logo from \"./logo.svg?react\";\nwindow.__LOGO = [url, Logo];\n",
    );
    let server = app.serve().await;
    let plain = imported(&server, "/src/logo.svg").await;
    assert!(
        plain
            .body
            .starts_with("export default \"data:image/svg+xml,")
            && !plain.body.contains("jsx"),
        "unclaimed plain svg is its URL:\n{}",
        plain.body
    );
    let react = imported(&server, "/src/logo.svg?react").await;
    assert_eq!(
        react.status, 200,
        "?react is the plugin's component:\n{}",
        react.body
    );
    assert!(
        react.body.contains("export default") && react.body.contains("jsx"),
        "?react is a component:\n{}",
        react.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn svgr_plugin_named_component_renders_in_dev() {
    if crate::browser::skip() {
        return;
    }
    crate::require_fixture!(
        "vite-plugin-svgr",
        "@svgr/core",
        "vite",
        "react",
        "react-dom"
    );
    let app = svgr_app(
        CRA_OPTIONS,
        &render_main("import { ReactComponent as Logo } from \"./logo.svg\";"),
    );
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for(r##"document.querySelector("#root svg")"##, WAIT)
        .await;
    let outer: String = page
        .eval(r##"document.querySelector("#root svg").outerHTML"##)
        .await;
    assert!(
        outer.starts_with("<svg") && outer.contains(r#"class="added""#),
        "the named component rendered with its props: {outer}"
    );
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}
