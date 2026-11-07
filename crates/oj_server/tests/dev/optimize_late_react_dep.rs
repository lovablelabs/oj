// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A React page that, once running, is edited to use a component whose dependency
//! the startup scan never saw (a Radix-shaped package importing `react`,
//! `react-dom` and `react/jsx-runtime`). The rerun that optimizes it must
//! leave the page with one copy of each dep: every optimized file is loaded
//! under one `?v=`, and the late component's hooks run on the page's React.

use std::collections::HashMap;
use std::time::Duration;

use crate::browser::Page;
use crate::{require_fixture, App};

const WAIT: Duration = Duration::from_secs(30);

const INDEX: &str = r#"<!doctype html><html><head><title>t</title></head><body><div id="root"></div><script type="module" src="/src/main.tsx"></script></body></html>"#;

const SELECT: &str = r#"import * as React from "react";
import { createPortal } from "react-dom";
import { jsx } from "react/jsx-runtime";
export function Select() {
  const [value] = React.useState("late-ok");
  return jsx("p", { id: "late", "data-portal": typeof createPortal, children: value });
}
"#;

const APP_BEFORE: &str = "import { useState } from \"react\";\n\
export function App() {\n  const [n] = useState(\"waiting\");\n  return <p id=\"waiting\">{n}</p>;\n}\n";

/// The edit that brings the late dep in, as adding a library component does.
const APP_AFTER: &str = "import { useState } from \"react\";\nimport { Select } from \"late-select\";\n\
export function App() {\n  const [n] = useState(\"waiting\");\n  return <div><p id=\"waiting\">{n}</p><Select /></div>;\n}\n";

#[tokio::test(flavor = "multi_thread")]
async fn a_late_dep_sharing_react_keeps_one_copy_of_each_dep() {
    require_fixture!("react", "react-dom", "scheduler");
    if crate::browser::skip() {
        return;
    }
    let app = App::new(&[
        ("package.json", r#"{"name":"late-dep","version":"1.0.0"}"#),
        ("index.html", INDEX),
        (
            "src/main.tsx",
            "import { createRoot } from \"react-dom/client\";\nimport { App } from \"./App\";\n\
             createRoot(document.getElementById(\"root\")!).render(<App />);\n",
        ),
        ("src/App.tsx", APP_BEFORE),
        (
            "node_modules/late-select/package.json",
            r#"{"name":"late-select","version":"1.0.0","type":"module","main":"index.js"}"#,
        ),
        ("node_modules/late-select/index.js", SELECT),
    ]);
    app.link_fixture("react");
    app.link_fixture("react-dom");
    app.link_fixture("scheduler");
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for(
        r##"document.querySelector("#waiting")?.textContent === "waiting""##,
        WAIT,
    )
    .await;
    app.write(&[("src/App.tsx", APP_AFTER)]);
    let rendered = page
        .poll(
            r##"document.querySelector("#late")?.textContent === "late-ok""##,
            WAIT,
        )
        .await;
    let errors = page.errors();
    let urls: Vec<String> = page
        .eval(
            r#"performance.getEntriesByType("resource").map((e) => e.name).filter((n) => n.includes("/@oj-deps/"))"#,
        )
        .await;
    page.close().await;
    assert!(
        rendered,
        "the late component never rendered; errors: {errors:?}\ndeps loaded: {urls:#?}"
    );
    assert_eq!(errors, Vec::<String>::new(), "page errors");
    let mut versions: HashMap<String, Vec<String>> = HashMap::new();
    for url in &urls {
        let (path, query) = url.split_once('?').unwrap_or((url, ""));
        let v = versions.entry(path.to_string()).or_default();
        if !v.iter().any(|q| q == query) {
            v.push(query.to_string());
        }
    }
    let twice: Vec<_> = versions.iter().filter(|(_, v)| v.len() > 1).collect();
    assert!(
        twice.is_empty(),
        "dep files loaded under more than one version: {twice:#?}"
    );
}
