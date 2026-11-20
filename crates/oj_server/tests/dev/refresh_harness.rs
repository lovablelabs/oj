// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A page oj never transformed (a test harness served by a plugin's
//! middleware) mounts one of the app's components the way Vite documents it.
//! `/@react-refresh` answers with the refresh runtime (plugin-react's
//! resolveId/load serve it in Vite), and a component module asks only for
//! `window.$RefreshReg$`, the check Vite's refresh wrapper makes, so both the
//! documented preamble and its stubs alone mount the component. The page also
//! loads `/@vite/client`, as Vite's docs pair it with the preamble: oj's client
//! under Vite's path, with the exports of Vite's `client.ts` that it has.

use std::time::Duration;

use crate::browser::Page;
use crate::App;

const WAIT: Duration = Duration::from_secs(20);

/// The preamble plugin-react documented before 6, as a harness page inlines it.
const PREAMBLE: &str = r#"import RefreshRuntime from "/@react-refresh";
RefreshRuntime.injectIntoGlobalHook(window);
window.$RefreshReg$ = () => {};
window.$RefreshSig$ = () => (type) => type;"#;

/// plugin-react 6's own `preambleCode` (base `/`): a named import.
const PREAMBLE_6: &str = r#"import { injectIntoGlobalHook } from "/@react-refresh";
injectIntoGlobalHook(window);
window.$RefreshReg$ = () => {};
window.$RefreshSig$ = () => (type) => type;"#;

/// The same preamble from an app module the page imports (a test setup
/// file), which oj compiles: its `/@react-refresh` import is oj's runtime.
const FROM_SOURCE: &str = r#"import "/src/preamble.ts";"#;

/// The preamble's stubs alone, which Vite's check accepts.
const STUBS: &str = r#"window.$RefreshReg$ = () => {};
window.$RefreshSig$ = () => (type) => type;"#;

fn config() -> String {
    let page = |preamble: &str| {
        format!(
            "<!doctype html><html><body><div id=\\\"root\\\"></div>\
             <script type=\\\"module\\\">{}</script>\
             <script type=\\\"module\\\" src=\\\"/@vite/client\\\"></script>\
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
        const pages = {{ "/harness": "{full}", "/stubs": "{stubs}", "/named": "{named}", "/source": "{source}" }};
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
        named = page(PREAMBLE_6),
        source = page(FROM_SOURCE),
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
        ("src/preamble.ts", PREAMBLE_6),
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

    // Vite's client path re-exports oj's client (one module, one socket),
    // after the env module Vite's client imports first.
    let client = server.text("/@vite/client").await;
    assert_eq!(
        client,
        "import \"/@vite/env\";\nexport * from \"/@oj/client.js\";\n"
    );

    let listening = server.listen().await;
    for path in ["/harness", "/stubs", "/named", "/source"] {
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
        let client: serde_json::Value = page
            .eval(
                r#"Promise.all([import("/@vite/client"), import("/@oj/client.js")]).then(([vite, oj]) => ({
                    exports: Object.keys(vite).sort(),
                    same: vite.createHotContext === oj.createHotContext,
                    query: [vite.injectQuery("/src/a.ts?x=1#h", "import"), vite.injectQuery("react", "import")],
                }))"#,
            )
            .await;
        assert_eq!(
            client,
            serde_json::json!({
                "exports": ["ErrorOverlay", "createHotContext", "injectQuery", "removeStyle", "updateStyle"],
                "same": true,
                "query": ["/src/a.ts?import&x=1#h", "react"],
            }),
            "{path}: /@vite/client"
        );
        page.close().await;
    }
}

/// A page under `public/`, served as it is: its inline scripts are never
/// compiled, so what they read of the app comes from `/@vite/client` alone.
fn public_page_app(page: &str) -> App {
    App::new(&[
        (
            "package.json",
            r#"{"name":"harness","private":true,"type":"module"}"#,
        ),
        (
            "oj.config.json",
            r#"{ "define": { "__HARNESS_DEFINE__": "\"harness-define\"", "app.nested.flag": "true", "__DEV_FLAG__": "import.meta.env.DEV", "import.meta.env.SKIPPED": "1" } }"#,
        ),
        ("index.html", crate::INDEX_HTML),
        ("src/main.js", "export {};\n"),
        ("public/page.html", page),
    ])
}

// Vite's client imports `@vite/env` (client.ts), which assigns the user
// `define`s, `import.meta.env.*` left out, onto globalThis (env.ts); a value
// reading `import.meta.env` finds the env object import analysis gave it.
#[tokio::test(flavor = "multi_thread")]
async fn a_define_is_a_global_once_vites_client_loads() {
    if crate::browser::skip() {
        return;
    }
    let app = public_page_app(
        r#"<!doctype html><html><head><script type="module" src="/@vite/client"></script></head><body>
<script type="module">window.__defines = [globalThis.__HARNESS_DEFINE__ ?? null, globalThis.app?.nested?.flag ?? null, globalThis.__DEV_FLAG__ ?? null, "import" in globalThis];</script>
</body></html>"#,
    );
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!(
        "http://127.0.0.1:{}/page.html",
        listening.addr.port()
    ))
    .await;
    let ran = page.poll("Array.isArray(window.__defines)", WAIT).await;
    assert!(ran, "the page never ran: {:?}", page.errors());
    let defines: serde_json::Value = page.eval("window.__defines").await;
    page.close().await;
    assert_eq!(
        defines,
        serde_json::json!(["harness-define", true, true, false])
    );
}

// Vite's client adopts the `style[data-vite-dev-id]` and
// `link[data-vite-dev-id]` tags already in the page: updateStyle replaces a
// style in place (identical css keeps its sheet) and skips a link's id,
// removeStyle removes either, and the tags it inserts carry the same
// attribute (client.ts sheetsMap, linkSheetsMap).
#[tokio::test(flavor = "multi_thread")]
async fn update_style_adopts_a_server_rendered_vite_style() {
    if crate::browser::skip() {
        return;
    }
    let app = public_page_app(
        r#"<!doctype html><html><head><style data-vite-dev-id="/src/x.css">p { color: red }</style><link rel="stylesheet" data-vite-dev-id="/src/z.css" href="data:text/css,"></head><body><p>x</p></body></html>"#,
    );
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!(
        "http://127.0.0.1:{}/page.html",
        listening.addr.port()
    ))
    .await;
    let styles: serde_json::Value = page
        .eval(
            r#"import("/@vite/client").then((m) => {
                const tags = () => [...document.querySelectorAll("style")].map((s) => s.outerHTML);
                const adopted = document.querySelector("style");
                const sheet = adopted.sheet;
                m.updateStyle("/src/x.css", "p { color: red }");
                const identical = adopted.sheet === sheet;
                m.updateStyle("/src/x.css", "p { color: blue }");
                const updated = [tags(), document.querySelector("style") === adopted];
                m.removeStyle("/src/x.css");
                const removed = tags();
                m.updateStyle("/src/z.css", "a {}");
                const linked = [tags(), document.querySelectorAll("link[data-vite-dev-id]").length];
                m.removeStyle("/src/z.css");
                const unlinked = document.querySelectorAll("link[data-vite-dev-id]").length;
                m.updateStyle("/src/y.css", "a { color: green }");
                return [identical, updated, removed, linked, unlinked, tags()];
            })"#,
        )
        .await;
    page.close().await;
    assert_eq!(
        styles,
        serde_json::json!([
            true,
            [
                [r#"<style data-vite-dev-id="/src/x.css">p { color: blue }</style>"#],
                true
            ],
            [],
            [[], 1],
            0,
            [r#"<style type="text/css" data-vite-dev-id="/src/y.css">a { color: green }</style>"#],
        ])
    );
}

// Vite's `clientAlias` (`^\/?@vite\/client`, `^\/?@vite\/env`) takes the
// bare specifiers too, which no package answers.
#[tokio::test(flavor = "multi_thread")]
async fn a_bare_vite_client_import_resolves() {
    let app = App::new(&[
        ("package.json", r#"{"name":"bare","private":true,"type":"module"}"#),
        ("index.html", crate::INDEX_HTML),
        (
            "src/main.js",
            "import { injectQuery } from \"@vite/client\";\nimport \"@vite/env\";\nexport const kind = typeof injectQuery;\n",
        ),
    ]);
    let server = app.serve().await;
    let main = server.text("/src/main.js").await;
    assert!(
        main.contains(r#"from "/@vite/client""#) && main.contains(r#"import "/@vite/env""#),
        "{main}"
    );
}

// Vite's transform middleware answers `/@vite/client` to a script and never
// to a page navigation, which gets the page any other path gets; its `send`
// marks the client `no-cache`.
#[tokio::test(flavor = "multi_thread")]
async fn a_page_visit_to_vites_client_gets_the_page_not_the_client() {
    let app = public_page_app("<!doctype html><html><body></body></html>");
    let server = app.serve().await;
    let fetch = |dest: &'static str| {
        let req = axum::http::Request::get("/@vite/client")
            .header("sec-fetch-dest", dest)
            .body(axum::body::Body::empty())
            .unwrap();
        server.request(req)
    };
    let script = fetch("script").await;
    assert_eq!(script.status, 200);
    assert_eq!(
        script.body,
        "import \"/@vite/env\";\nexport * from \"/@oj/client.js\";\n"
    );
    assert_eq!(
        script.headers.get("content-type").unwrap(),
        "text/javascript"
    );
    assert_eq!(script.headers.get("cache-control").unwrap(), "no-cache");

    let page = fetch("document").await;
    assert_eq!(page.status, 200, "{}", page.body);
    let kind = page.headers.get("content-type").unwrap().to_str().unwrap();
    assert!(kind.starts_with("text/html"), "{kind}");
    assert!(page.body.contains("/src/main.js"), "{}", page.body);
}

// The "pending" mark Start's page client leaves outlives the parse it was made
// in. A copy of the client that evaluates after the document has parsed has
// no DOMContentLoaded left to wait for: it decides at once, and on a page
// without Start's live-reload tag that means it connects.
#[tokio::test(flavor = "multi_thread")]
async fn a_pending_mark_met_after_the_parse_is_decided_at_once() {
    if crate::browser::skip() {
        return;
    }
    let app = public_page_app(
        r#"<!doctype html><html><head><script>
window.__sockets = [];
const Native = window.WebSocket;
window.WebSocket = class extends Native {
  constructor(url, protocols) { super(url, protocols); window.__sockets.push(String(url)); }
};
globalThis.__ojPageClient = "pending";
window.addEventListener("load", () => {
  window.__stateAtImport = document.readyState;
  import("/@oj/client.js").then(() => { window.__clientLoaded = true; });
});
</script></head><body></body></html>"#,
    );
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!(
        "http://127.0.0.1:{}/page.html",
        listening.addr.port()
    ))
    .await;
    let loaded = page.poll("window.__clientLoaded === true", WAIT).await;
    assert!(loaded, "the client never loaded: {:?}", page.errors());
    let connected = page
        .poll("window.__sockets.length === 1", Duration::from_secs(5))
        .await;
    let seen: serde_json::Value = page
        .eval("[window.__stateAtImport, window.__sockets.length]")
        .await;
    page.close().await;
    assert!(connected, "state at import and sockets: {seen}");
    assert_eq!(seen, serde_json::json!(["complete", 1]));
}
