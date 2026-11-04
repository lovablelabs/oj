// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A page oj never transformed (a test harness served by a plugin's
//! middleware) mounts one of the app's components the way Vite documents it.
//! `/@react-refresh` answers with the refresh runtime (plugin-react's
//! resolveId/load serve it in Vite), and a component module asks only for
//! `window.$RefreshReg$`, the check Vite's refresh wrapper makes, so both the
//! documented preamble and its stubs alone mount the component.

use std::time::Duration;

use crate::browser::Page;
use crate::App;

const WAIT: Duration = Duration::from_secs(20);

/// plugin-react's preamble (`preambleCode`), as a harness page inlines it.
const PREAMBLE: &str = r#"import RefreshRuntime from "/@react-refresh";
RefreshRuntime.injectIntoGlobalHook(window);
window.$RefreshReg$ = () => {};
window.$RefreshSig$ = () => (type) => type;"#;

/// The preamble's stubs alone, which Vite's check accepts.
const STUBS: &str = r#"window.$RefreshReg$ = () => {};
window.$RefreshSig$ = () => (type) => type;"#;

fn config() -> String {
    let page = |preamble: &str| {
        format!(
            "<!doctype html><html><body><div id=\\\"root\\\"></div>\
             <script type=\\\"module\\\">{}</script>\
             <script type=\\\"module\\\">import(\\\"/src/mount.tsx\\\").catch((e) => {{ window.__mountError = String(e); }});</script>\
             </body></html>",
            preamble.replace('\n', "\\n").replace('"', "\\\"")
        )
    };
    format!(
        r#"export default {{
  plugins: [{{
    name: "harness",
    configureServer(server) {{
      server.middlewares.use((req, res, next) => {{
        const pages = {{ "/harness": "{full}", "/stubs": "{stubs}" }};
        if (!(req.url in pages)) return next();
        res.setHeader("content-type", "text/html");
        res.end(pages[req.url]);
      }});
    }},
  }}],
}};
"#,
        full = page(PREAMBLE),
        stubs = page(STUBS),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_harness_page_mounts_a_component_with_vites_preamble() {
    crate::require_fixture!("vite", "react", "react-dom", "scheduler");
    if crate::browser::skip() {
        return;
    }
    let config = config();
    let app = App::new(&[
        ("package.json", r#"{"name":"harness","private":true,"type":"module"}"#),
        ("vite.config.js", &config),
        (
            "src/Card.tsx",
            "import { useState } from \"react\";\n\
             export function Card() {\n  const [text] = useState(\"card-mounted\");\n  return <p id=\"card\">{text}</p>;\n}\n",
        ),
        (
            "src/mount.tsx",
            "import { createRoot } from \"react-dom/client\";\nimport { Card } from \"./Card\";\n\
             createRoot(document.getElementById(\"root\")!).render(<Card />);\n",
        ),
    ]);
    app.link_fixture("vite");
    app.link_fixture("react");
    app.link_fixture("react-dom");
    app.link_fixture("scheduler");
    let server = app.serve().await;
    crate::plugin_middleware::middleware_ready(&server).await;

    // The runtime oj's compiled modules import, so a harness shares it.
    let runtime = server.text("/@react-refresh").await;
    assert!(
        runtime.contains("from \"/@oj/refresh-runtime.js\""),
        "/@react-refresh is not oj's refresh runtime:\n{runtime}"
    );

    let listening = server.listen().await;
    for path in ["/harness", "/stubs"] {
        let page = Page::open(&format!("http://127.0.0.1:{}{path}", listening.addr.port())).await;
        let mounted = page
            .poll(
                r##"document.querySelector("#card")?.textContent === "card-mounted""##,
                WAIT,
            )
            .await;
        let error: Option<String> = page.eval("window.__mountError ?? null").await;
        assert!(
            mounted,
            "{path}: the component never mounted; import error: {error:?}, page errors: {:?}",
            page.errors()
        );
        page.close().await;
    }
}
