// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `import Icon from "./icon.svg?react"` in dev: the svg becomes a React
//! component that renders an `<svg>` keeping the file's attributes
//! (`stroke-width` survives the JSX attribute rewrite), with the caller's
//! props spread onto it (`className` replaces the file's class). The build
//! half is oj's `svgr_build`.

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
