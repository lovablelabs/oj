// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Tailwind loaded the standard Vite way, `import "./index.css"` from the
//! entry with no `<link>`, hot-swaps on a source edit: the css-update re-runs
//! the css module's updateStyle instead of reloading, so component state
//! survives a class added in a component.

use std::time::Duration;

use crate::browser::Page;
use crate::{require_fixture, App};

const WAIT: Duration = Duration::from_secs(20);

const INDEX: &str = r#"<!doctype html><html><head><title>t</title></head><body><div id="root"></div><script type="module" src="/src/main.tsx"></script></body></html>"#;

fn app_src(class: &str) -> String {
    format!(
        "import {{ useState }} from \"react\";\n\
         export function App() {{\n  const [n, setN] = useState(0);\n\
         \x20 return (<div><h1 className=\"{class}\">hi</h1><button onClick={{() => setN(n + 1)}}>Clicks: {{n}}</button></div>);\n}}\n"
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn js_imported_tailwind_hot_swaps_on_class_edit() {
    require_fixture!(
        "react",
        "react-dom",
        "scheduler",
        "tailwindcss",
        "@tailwindcss"
    );
    if crate::browser::skip() {
        return;
    }
    let app = App::new(&[
        ("package.json", r#"{"name":"tw-js-import","version":"1.0.0"}"#),
        ("src/index.css", "@import \"tailwindcss\";\n"),
        (
            "src/main.tsx",
            "import \"./index.css\";\nimport { createRoot } from \"react-dom/client\";\nimport { App } from \"./App\";\n\
             createRoot(document.getElementById(\"root\")!).render(<App />);\n",
        ),
        ("src/App.tsx", &app_src("underline")),
        ("index.html", INDEX),
    ]);
    for pkg in [
        "react",
        "react-dom",
        "scheduler",
        "tailwindcss",
        "@tailwindcss",
    ] {
        app.link_fixture(pkg);
    }
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for(r#"document.querySelector("h1")"#, WAIT).await;
    page.wait_for(
        r#"getComputedStyle(document.querySelector("h1")).textDecorationLine === "underline""#,
        WAIT,
    )
    .await;
    assert_eq!(
        page.eval::<u32>(r#"document.querySelectorAll("link[rel=stylesheet]").length"#)
            .await,
        0,
        "no <link>: css is JS-imported"
    );
    page.eval::<bool>(r#"document.querySelector("button").click(), true"#)
        .await;
    page.eval::<bool>(r#"document.querySelector("button").click(), true"#)
        .await;
    page.eval::<bool>("window.__NOT_RELOADED = true").await;

    app.write(&[("src/App.tsx", &app_src("underline italic"))]);
    page.wait_for(
        r#"getComputedStyle(document.querySelector("h1")).fontStyle === "italic""#,
        WAIT,
    )
    .await;
    assert!(
        page.eval::<bool>("window.__NOT_RELOADED === true").await,
        "page was reloaded"
    );
    let clicks: String = page
        .eval(r#"document.querySelector("button").textContent"#)
        .await;
    assert!(
        clicks.contains("Clicks: 2"),
        "component state lost: {clicks}"
    );
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}

/// The same sheet `<link>`ed by the page: its request compiles as the sheet's
/// `?direct` module, and a source edit's css-update still names the sheet
/// the page links (`/src/index.css`), so the link swaps in place. Naming the
/// `?direct` request instead left the client no `<link>` to swap, and it
/// reloaded the page.
#[tokio::test(flavor = "multi_thread")]
async fn linked_tailwind_hot_swaps_on_class_edit() {
    require_fixture!(
        "react",
        "react-dom",
        "scheduler",
        "tailwindcss",
        "@tailwindcss"
    );
    if crate::browser::skip() {
        return;
    }
    let app = App::new(&[
        ("package.json", r#"{"name":"tw-linked","version":"1.0.0"}"#),
        ("src/index.css", "@import \"tailwindcss\";\n"),
        (
            "src/main.tsx",
            "import { createRoot } from \"react-dom/client\";\nimport { App } from \"./App\";\n\
             createRoot(document.getElementById(\"root\")!).render(<App />);\n",
        ),
        ("src/App.tsx", &app_src("underline")),
        (
            "index.html",
            r#"<!doctype html><html><head><title>t</title><link rel="stylesheet" href="/src/index.css"></head><body><div id="root"></div><script type="module" src="/src/main.tsx"></script></body></html>"#,
        ),
    ]);
    for pkg in [
        "react",
        "react-dom",
        "scheduler",
        "tailwindcss",
        "@tailwindcss",
    ] {
        app.link_fixture(pkg);
    }
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for(
        r#"document.querySelector("h1") && getComputedStyle(document.querySelector("h1")).textDecorationLine === "underline""#,
        WAIT,
    )
    .await;
    page.eval::<bool>(r#"document.querySelector("button").click(), true"#)
        .await;
    page.eval::<bool>("window.__NOT_RELOADED = true").await;

    app.write(&[("src/App.tsx", &app_src("underline italic"))]);
    page.wait_for(
        r#"getComputedStyle(document.querySelector("h1")).fontStyle === "italic""#,
        WAIT,
    )
    .await;
    assert!(
        page.eval::<bool>("window.__NOT_RELOADED === true").await,
        "page was reloaded"
    );
    let clicks: String = page
        .eval(r#"document.querySelector("button").textContent"#)
        .await;
    assert!(
        clicks.contains("Clicks: 1"),
        "component state lost: {clicks}"
    );
    page.close().await;
}
