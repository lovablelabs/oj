// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! An edit to a module that is not itself an HMR boundary (a plain `.ts`
//! util two imports below a React component) reaches the page hot, with
//! component state kept: the re-fetched boundary imports the util's new
//! version (`?t=<stamp>`) down the chain, not the browser's cached bare url,
//! and nothing above the boundary is stamped.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use regex::Regex;

use crate::browser::Page;
use crate::{require_fixture, App};

const WAIT: Duration = Duration::from_secs(20);

const INDEX: &str = r#"<!doctype html><html><head><title>t</title></head><body><div id="root"></div><script type="module" src="/src/main.tsx"></script></body></html>"#;

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

#[tokio::test(flavor = "multi_thread")]
async fn non_boundary_dep_edit_stamps_the_chain_and_stays_hot() {
    require_fixture!("react", "react-dom", "scheduler");
    if crate::browser::skip() {
        return;
    }
    // App (boundary) -> hooks.ts (not a boundary) -> label.ts (edited)
    let app = App::new(&[
        ("package.json", r#"{"name":"hmr-dep","version":"1.0.0"}"#),
        (
            "src/main.tsx",
            "import { createRoot } from \"react-dom/client\";\nimport { App } from \"./App\";\n\
             createRoot(document.getElementById(\"root\")!).render(<App />);\n",
        ),
        (
            "src/hooks.ts",
            "import { label } from \"./label\";\nexport const useLabel = () => label;\n",
        ),
        ("src/label.ts", "export const label = \"v1\";\n"),
        (
            "src/App.tsx",
            "import { useState } from \"react\";\nimport { useLabel } from \"./hooks\";\n\
             export function App() {\n  const [n, setN] = useState(0);\n  const label = useLabel();\n\
             \x20 return (<div><h1>{label}</h1><button onClick={() => setN(n + 1)}>Clicks: {n}</button></div>);\n}\n",
        ),
        ("index.html", INDEX),
    ]);
    app.link_fixture("react");
    app.link_fixture("react-dom");
    app.link_fixture("scheduler");
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for(
        r#"(document.querySelector("h1")?.textContent ?? "").includes("v1")"#,
        WAIT,
    )
    .await;
    page.eval::<bool>(r#"document.querySelector("button").click(), true"#)
        .await;
    page.eval::<bool>(r#"document.querySelector("button").click(), true"#)
        .await;
    page.eval::<bool>("window.__NOT_RELOADED = true").await;

    app.write(&[("src/label.ts", "export const label = \"v2\";\n")]);
    page.wait_for(
        r#"(document.querySelector("h1")?.textContent ?? "").includes("v2")"#,
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

    let app_js = server.text(&format!("/src/App.tsx?t={}", now_ms())).await;
    assert!(
        Regex::new(r#""/src/hooks\.ts\?t=\d+""#)
            .unwrap()
            .is_match(&app_js),
        "App.tsx import of hooks.ts is unstamped:\n{app_js}"
    );
    let hooks_js = server.text(&format!("/src/hooks.ts?t={}", now_ms())).await;
    assert!(
        Regex::new(r#""/src/label\.ts\?t=\d+""#)
            .unwrap()
            .is_match(&hooks_js),
        "hooks.ts import of label.ts is unstamped:\n{hooks_js}"
    );
    let main_js = server.text("/src/main.tsx").await;
    assert!(
        !main_js.contains("App.tsx?t="),
        "above the boundary nothing is stamped:\n{main_js}"
    );
}
