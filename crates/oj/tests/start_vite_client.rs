// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A page oj did not render (a harness HTML file under `public/`) mounts one
//! of a TanStack Start app's components the way Vite documents it: the
//! plugin-react preamble inline, then `<script type="module"
//! src="/@vite/client">`, then a module importing the component. Under
//! `vite dev` the client is Vite's (constants.ts `CLIENT_PUBLIC_PATH`); oj
//! answers it with its own dev client, the module the component's
//! `import.meta.hot` comes from. A document oj rendered already runs Start's
//! client, so loading `/@vite/client` there opens no second socket. The
//! browser half skips without Chrome, unless `OJ_REQUIRE_BROWSER` is set.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;
#[allow(dead_code)]
mod start_app;

use std::time::Duration;

use browser::Page;
use start_app::{app, Server};

const HARNESS: &str = r#"<!doctype html><html><head>
<script type="module">
import RefreshRuntime from "/@react-refresh";
RefreshRuntime.injectIntoGlobalHook(window);
window.$RefreshReg$ = () => {};
window.$RefreshSig$ = () => (type) => type;
window.__vite_plugin_react_preamble_installed__ = true;
</script>
<script type="module" src="/@vite/client"></script>
</head><body><div id="root"></div>
<script type="module">
import React from "/node_modules/.vite/deps/react.js";
import ReactDOM from "/node_modules/.vite/deps/react-dom_client.js";
import { Card } from "/src/components/Card.tsx";
ReactDOM.createRoot(document.getElementById("root")).render(React.createElement(Card));
</script>
</body></html>
"#;

const CARD: &str = r#"import { useState } from "react";
export function Card() {
  const [text] = useState("card-mounted");
  return <p id="card">{text}</p>;
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_harness_page_loads_vites_client_beside_the_preamble() {
    let Some(app) = app("vite-client") else {
        return;
    };
    let root = &app.0;
    std::fs::create_dir_all(root.join("src/components")).unwrap();
    std::fs::write(root.join("src/components/Card.tsx"), CARD).unwrap();
    std::fs::write(root.join("public/harness.html"), HARNESS).unwrap();
    let dev = Server::oj_dev(root);

    let (status, body) = dev.request("GET", "/@vite/client", &[]);
    assert_eq!(status, 200, "GET /@vite/client\n{body}\n{}", dev.output());
    assert_eq!(
        body,
        "import \"/@oj-start/page-client.js\";\nexport * from \"/@oj/client.js\";\n"
    );

    if browser::skip() {
        return;
    }
    let page = Page::open_with_script(&dev.url("/harness.html"), Some(SOCKETS)).await;
    let mounted = page
        .poll(
            r##"document.querySelector("#card")?.textContent === "card-mounted""##,
            Duration::from_secs(60),
        )
        .await;
    assert!(
        mounted,
        "the component never mounted; page errors: {:?}\n{}",
        page.errors(),
        dev.output()
    );
    let same: bool = page
        .eval(
            r#"Promise.all([import("/@vite/client"), import("/@oj/client.js")])
                .then(([vite, oj]) => vite.createHotContext === oj.createHotContext)"#,
        )
        .await;
    assert!(same, "/@vite/client is a second client");
    // The harness keeps oj's client socket, which its component's updates
    // come over.
    let sockets: Vec<String> = page.eval("window.__sockets").await;
    assert!(
        sockets.len() == 1 && !sockets[0].ends_with("/@oj-start/hmr"),
        "the harness page's sockets: {sockets:?}"
    );
    page.eval::<bool>("(window.__kept = true)").await;
    std::fs::write(
        root.join("src/components/Card.tsx"),
        CARD.replace("card-mounted", "card-edited"),
    )
    .unwrap();
    let updated = page
        .poll(
            r##"document.querySelector("#card")?.textContent === "card-edited""##,
            Duration::from_secs(60),
        )
        .await;
    let kept: bool = page.eval("window.__kept === true").await;
    assert!(
        updated && kept,
        "the edit did not hot-update the harness (updated {updated}, kept {kept}); page errors: {:?}\n{}",
        page.errors(),
        dev.output()
    );
    page.close().await;

    // A document oj rendered: Start's client holds the page's one socket.
    let page = Page::open_with_script(&dev.url("/"), Some(SOCKETS)).await;
    page.wait_for(
        r#"window.__sockets.some((u) => u.endsWith("/@oj-start/hmr"))"#,
        Duration::from_secs(60),
    )
    .await;
    let sockets: Vec<String> = page
        .eval(
            r#"import("/@vite/client")
                .then(() => new Promise((r) => setTimeout(r, 1000)))
                .then(() => window.__sockets)"#,
        )
        .await;
    assert_eq!(
        sockets,
        [format!("ws://127.0.0.1:{}/@oj-start/hmr", dev.port)],
        "the sockets a Start document opened"
    );
    page.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn under_a_base_a_start_document_keeps_one_socket() {
    let Some(app) = app("vite-client-base") else {
        return;
    };
    let config = app.0.join("vite.config.ts");
    let text = std::fs::read_to_string(&config).unwrap();
    let with_base = text.replacen(
        "export default defineConfig({",
        "export default defineConfig({\n  base: \"/app/\",",
        1,
    );
    assert_ne!(text, with_base, "the fixture's config changed shape");
    std::fs::write(&config, with_base).unwrap();
    let dev = Server::oj_dev(&app.0);

    let (status, body) = dev.request("GET", "/app/@vite/client", &[]);
    assert_eq!(
        status,
        200,
        "GET /app/@vite/client\n{body}\n{}",
        dev.output()
    );
    assert_eq!(
        body,
        "import \"/app/@oj-start/page-client.js\";\nexport * from \"/app/@oj/client.js\";\n"
    );

    if browser::skip() {
        return;
    }
    let page = Page::open_with_script(&dev.url("/app/"), Some(SOCKETS)).await;
    page.wait_for(
        r#"window.__sockets.some((u) => u.endsWith("/@oj-start/hmr"))"#,
        Duration::from_secs(60),
    )
    .await;
    let sockets: Vec<String> = page
        .eval(
            r#"import("/app/@vite/client")
                .then(() => new Promise((r) => setTimeout(r, 1000)))
                .then(() => window.__sockets)"#,
        )
        .await;
    assert_eq!(
        sockets,
        [format!("ws://127.0.0.1:{}/app/@oj-start/hmr", dev.port)],
        "the sockets a Start document under the base opened"
    );
    page.close().await;
}

/// Records the url (without its query) of every WebSocket the page opens.
const SOCKETS: &str = r#"window.__sockets = [];
const OjWebSocket = window.WebSocket;
window.WebSocket = class extends OjWebSocket {
  constructor(url, protocols) {
    super(url, protocols);
    window.__sockets.push(String(url).split("?")[0]);
  }
};"#;
