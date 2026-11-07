// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! HMR client and server decisions at Vite parity, in a real page: an edit
//! inside an import cycle below a boundary hot-updates with state kept; an
//! edit to a module the page never loaded is ignored; frames are Vite's
//! UpdatePayload (css-update and js-update entries with path, acceptedPath,
//! timestamp) and ErrorPayload (`err.message`, no top-level `message`); a
//! syntax error shows the overlay and its fix recovers without a reload.
//! Not covered here: a `.env` edit restarts the server and the page then
//! reloads itself. A restart would exec the test binary, so the harness
//! parks it and fails the test instead.

use std::time::Duration;

use crate::browser::Page;
use crate::{require_fixture, App};

const WAIT: Duration = Duration::from_secs(20);

const INDEX: &str = r#"<!doctype html><html><head><title>t</title><link rel="stylesheet" href="/style.css"></head><body><div id="root"></div><script type="module" src="/src/main.tsx"></script></body></html>"#;

const MAIN: &str = "import { createRoot } from \"react-dom/client\";\nimport { App } from \"./App\";\ncreateRoot(document.getElementById(\"root\")!).render(<App />);\n";

const APP: &str = "import { useState } from \"react\";\nimport { viaB } from \"./a\";\n\
export function App() {\n  const [n, setN] = useState(0);\n\
  // never called: Lazy stays unloaded in the browser but is in the server graph\n\
  (window as any).__loadLazy = () => import(\"./Lazy\");\n\
  return (<div><h1>{viaB()}</h1><button onClick={() => setN(n + 1)}>Clicks: {n}</button></div>);\n}\n";

fn b(suffix: &str) -> String {
    format!("import {{ label }} from \"./a\";\nexport const fromB = () => label + {suffix};\n")
}

fn h1_has(text: &str) -> String {
    format!(r#"(document.querySelector("h1")?.textContent ?? "").includes({text:?})"#)
}

/// Playwright locators pierce open shadow roots; the overlay renders inside one.
const DIALOG_SHOWN: &str = r#"(() => {
  const find = (root) => {
    if (root.querySelector('[role="dialog"]')) return true;
    for (const el of root.querySelectorAll("*")) if (el.shadowRoot && find(el.shadowRoot)) return true;
    return false;
  };
  return find(document);
})()"#;

/// A second socket in the page, dialed with the token the served client
/// carries (Vite's webSocketToken), records every frame the server sends.
const RECORD_FRAMES: &str = r#"(async () => {
  window.__frames = [];
  const client = await (await fetch("/@oj/client.js")).text();
  const token = /const wsToken = "([0-9a-f]+)";/.exec(client)[1];
  const ws = new WebSocket("ws://" + location.host + "/__ws?token=" + token);
  ws.onmessage = (e) => window.__frames.push(JSON.parse(e.data));
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  window.__NOT_RELOADED = true;
  return true;
})()"#;

#[tokio::test(flavor = "multi_thread")]
async fn cycle_edit_unloaded_boundary_and_payload_shapes() {
    require_fixture!("react", "react-dom", "scheduler");
    if crate::browser::skip() {
        return;
    }
    let app = App::new(&[
        ("package.json", r#"{"name":"hmr-client","version":"1.0.0"}"#),
        (".env", "VITE_FOO=1\n"),
        ("style.css", "h1 { color: rgb(1, 2, 3); }\n"),
        ("src/main.tsx", MAIN),
        // a <-> b cycle (barrel style), both below App
        (
            "src/a.ts",
            "import { fromB } from \"./b\";\nexport const label = \"v1\";\nexport const viaB = () => fromB();\n",
        ),
        ("src/b.ts", &b("\"!\"")),
        (
            "src/Lazy.tsx",
            "export default function Lazy() { return <p>lazy</p>; }\n",
        ),
        ("src/App.tsx", APP),
        ("index.html", INDEX),
    ]);
    app.link_fixture("react");
    app.link_fixture("react-dom");
    app.link_fixture("scheduler");
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for(&h1_has("v1!"), WAIT).await;
    assert!(page.eval::<bool>(RECORD_FRAMES).await);
    page.eval::<bool>(r#"document.querySelector("button").click(), true"#)
        .await;

    // An edit inside the cycle hot-updates through App.
    app.write(&[("src/b.ts", &b("\"?\""))]);
    page.wait_for(&h1_has("v1?"), WAIT).await;
    assert!(
        page.eval::<bool>("window.__NOT_RELOADED === true").await,
        "cycle edit reloaded the page"
    );
    let clicks: String = page
        .eval(r#"document.querySelector("button").textContent"#)
        .await;
    assert!(
        clicks.contains("Clicks: 1"),
        "state lost on cycle edit: {clicks}"
    );

    // An edit to a boundary the page never loaded is ignored.
    app.write(&[(
        "src/Lazy.tsx",
        "export default function Lazy() { return <p>lazy2</p>; }\n",
    )]);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        page.eval::<bool>("window.__NOT_RELOADED === true").await,
        "unloaded module edit reloaded the page"
    );

    // A css edit arrives as a css-update entry inside an UpdatePayload.
    app.write(&[("style.css", "h1 { color: rgb(4, 5, 6); }\n")]);
    page.wait_for(
        r#"getComputedStyle(document.querySelector("h1")).color === "rgb(4, 5, 6)""#,
        WAIT,
    )
    .await;
    let css: serde_json::Value = page
        .eval(
            r#"window.__frames.find((f) => f.type === "update" && f.updates.some((u) => u.type === "css-update")) ?? null"#,
        )
        .await;
    let frames: serde_json::Value = page.eval("window.__frames").await;
    // The fixed wait above cannot tell "no reload" from "not yet": a reload
    // the unloaded edit sent would sit in the socket's frames.
    assert!(
        !frames
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["type"] == "full-reload"),
        "a full reload was sent: {frames}"
    );
    assert!(
        !css.is_null(),
        "no UpdatePayload with a css-update entry: {frames}"
    );
    let entry = css["updates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["type"] == "css-update")
        .unwrap();
    assert_eq!(entry["path"], "/style.css");
    assert_eq!(entry["acceptedPath"], "/style.css");
    assert!(
        entry["timestamp"].is_number(),
        "timestamp is a number: {entry}"
    );
    let js: serde_json::Value = page
        .eval(
            r#"window.__frames.find((f) => f.type === "update" && f.updates.some((u) => u.type === "js-update")) ?? null"#,
        )
        .await;
    let accepted = &js["updates"][0]["acceptedPath"];
    assert!(
        accepted.as_str().is_some_and(|s| !s.is_empty()),
        "js-update entries carry type and acceptedPath: {js}"
    );

    // A syntax error arrives as an ErrorPayload and shows the overlay.
    app.write(&[("src/b.ts", &b(""))]);
    page.wait_for(r#"window.__frames.some((f) => f.type === "error")"#, WAIT)
        .await;
    let err: serde_json::Value = page
        .eval(r#"window.__frames.find((f) => f.type === "error")"#)
        .await;
    assert!(
        err["err"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "ErrorPayload shape: {err}"
    );
    assert!(
        err.get("message").is_none(),
        "legacy top-level message is gone: {err}"
    );
    page.wait_for(DIALOG_SHOWN, Duration::from_secs(10)).await;
    app.write(&[("src/b.ts", &b("\"#\""))]);
    page.wait_for(&h1_has("v1#"), WAIT).await;
    assert!(
        page.eval::<bool>("window.__NOT_RELOADED === true").await,
        "error round-trip reloaded the page"
    );
    page.close().await;
}
