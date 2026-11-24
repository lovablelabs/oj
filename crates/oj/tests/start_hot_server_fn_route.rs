// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! An edit to a route that declares a server function reaches an open page
//! as a hot update, and the server runs the edited handler from then on.
//! Every save of such a route used to reload the page: a declaring edit
//! counted as a change to the server functions whatever the resolver it
//! regenerated held, although an edit inside a handler keeps every function
//! id. The browser half skips without Chrome, unless `OJ_REQUIRE_BROWSER`
//! is set.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;
#[allow(dead_code)]
mod start_app;

use std::time::Duration;

use browser::Page;
use start_app::{Server, WAIT};

const ROOT: &str = r#"import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";

export const Route = createRootRoute({
  head: () => ({ meta: [{ title: "server fn route" }] }),
  component: () => (
    <html lang="en">
      <head>
        <HeadContent />
      </head>
      <body>
        <Outlet />
        <Scripts />
      </body>
    </html>
  ),
});
"#;

const ROUTER: &str = r#"import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
  return createRouter({ routeTree });
}
"#;

/// The index route: a loader backed by a server function, and a button that
/// calls the function again. An edit changes `handler`, the value the
/// function answers with, `shown`, what the component renders, or both.
fn index_route(handler: u32, shown: u32) -> String {
    format!(
        r#"import {{ createFileRoute }} from "@tanstack/react-router";
import {{ createServerFn }} from "@tanstack/react-start";
import {{ useEffect, useState }} from "react";

const getAnswer = createServerFn({{ method: "GET" }}).handler(async () => {{
  return {{ answer: "answer-v{handler}" }};
}});

export const Route = createFileRoute("/")({{
  loader: () => getAnswer(),
  component: Index,
}});

function Index() {{
  const {{ answer }} = Route.useLoaderData();
  const [called, setCalled] = useState("");
  const [mounted, setMounted] = useState(false);
  useEffect(() => setMounted(true), []);
  return (
    <main>
      <p data-testid="mounted">{{mounted ? "mounted" : "ssr"}}</p>
      <p data-testid="shown">shown-v{shown}</p>
      <p data-testid="loaded">{{answer}}</p>
      <button id="call" onClick={{() => getAnswer().then((r) => setCalled("called:" + r.answer))}}>call</button>
      <p data-testid="called">{{called}}</p>
    </main>
  );
}}
"#
    )
}

fn server_fn_app(name: &str) -> Option<start_app::App> {
    let files: &[(&str, String)] = &[
        (
            "package.json",
            r#"{ "name": "server-fn-route", "private": true, "type": "module", "dependencies": { "@tanstack/react-start": "*", "@tanstack/react-router": "*", "react": "*", "react-dom": "*" } }"#.to_string(),
        ),
        (
            "tsconfig.json",
            r#"{ "compilerOptions": { "jsx": "react-jsx", "module": "ESNext", "moduleResolution": "Bundler", "target": "ES2022", "strict": true, "skipLibCheck": true } }"#.to_string(),
        ),
        (
            "vite.config.ts",
            r#"import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig({
  plugins: [tanstackStart(), react()],
});
"#
            .to_string(),
        ),
        ("src/router.tsx", ROUTER.to_string()),
        ("src/routes/__root.tsx", ROOT.to_string()),
        ("src/routes/index.tsx", index_route(1, 1)),
    ];
    start_app::app_from(name, files)
}

fn text(testid: &str) -> String {
    format!("(document.querySelector('[data-testid=\"{testid}\"]')?.textContent ?? '')")
}

async fn shows(page: &Page, dev: &Server, testid: &str, expected: &str) {
    let shown = page
        .poll(
            &format!(
                "{} === {}",
                text(testid),
                serde_json::to_string(expected).unwrap()
            ),
            Duration::from_secs(40),
        )
        .await;
    assert!(
        shown,
        "the page never showed {expected:?} in {testid}: shows {:?}; page errors: {:?}; console: {:?}\n{}",
        page.eval::<String>(&text(testid)).await,
        page.errors(),
        page.console(),
        dev.output()
            .lines()
            .filter(|l| l.contains("oj start:") || l.contains("oj:"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// An edit inside a server function's handler and its route's component,
/// then one to the handler alone: the page takes each in place, its loader
/// runs again on the edited handler, and so does the next call.
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_to_a_route_with_a_server_function_is_a_hot_update() {
    let Some(app) = server_fn_app("hot-server-fn-route") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let dev = Server::oj_dev(&root);
    let page = Page::open(&dev.url("/")).await;
    page.wait_for(&format!("{} === 'mounted'", text("mounted")), WAIT)
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    shows(&page, &dev, "loaded", "answer-v1").await;
    page.eval::<bool>("(window.__mark = 'kept', true)").await;

    std::fs::write(root.join("src/routes/index.tsx"), index_route(2, 2)).unwrap();
    shows(&page, &dev, "shown", "shown-v2").await;
    assert!(
        page.eval::<bool>("window.__mark === 'kept'").await,
        "the page reloaded instead of taking the edit in place\n{}",
        dev.output()
    );
    assert!(
        dev.logs("oj start: hot updated"),
        "the edit was not a hot update\n{}",
        dev.output()
    );
    // The route's loader ran again with the update, on the edited handler.
    shows(&page, &dev, "loaded", "answer-v2").await;
    // The server runs the edited handler.
    page.eval::<bool>("(document.querySelector('#call').click(), true)")
        .await;
    shows(&page, &dev, "called", "called:answer-v2").await;

    // An edit to the handler alone, which the client's copy of the module
    // does not hold: still a hot update, and the server runs it.
    let hot_before = dev.output().matches("oj start: hot updated").count();
    std::fs::write(root.join("src/routes/index.tsx"), index_route(3, 2)).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(40);
    while dev.output().matches("oj start: hot updated").count() == hot_before {
        assert!(
            std::time::Instant::now() < deadline,
            "the handler's edit was not a hot update\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // The server revalidates the module after the patch goes out: the call
    // is made again until it answers with the edited handler.
    let called = format!("{} === 'called:answer-v3'", text("called"));
    let mut answered = false;
    while !answered && std::time::Instant::now() < deadline {
        page.eval::<bool>("(document.querySelector('#call').click(), true)")
            .await;
        answered = page.poll(&called, Duration::from_secs(2)).await;
    }
    assert!(
        answered,
        "the server never ran the edited handler: shows {:?}\n{}",
        page.eval::<String>(&text("called")).await,
        dev.output()
    );
    // The loader ran again for it too, though no module of the page changed.
    shows(&page, &dev, "loaded", "answer-v3").await;
    assert!(
        page.eval::<bool>("window.__mark === 'kept'").await,
        "the page reloaded for an edit to the handler alone\n{}",
        dev.output()
    );

    // A router from before `_refreshRoute` (router-core 1.171.18) loads its
    // routes again through `invalidate`, as its own accept handler did.
    assert!(
        page.eval::<bool>(
            "(() => { let p = self.__TSR_ROUTER__, had = false; \
             while (p) { if (Object.hasOwn(p, '_refreshRoute')) { delete p._refreshRoute; had = true; } p = Object.getPrototypeOf(p); } \
             return had && typeof self.__TSR_ROUTER__._refreshRoute === 'undefined'; })()"
        )
        .await,
        "the router had no _refreshRoute to take away"
    );
    std::fs::write(root.join("src/routes/index.tsx"), index_route(4, 2)).unwrap();
    shows(&page, &dev, "loaded", "answer-v4").await;
    assert!(
        page.eval::<bool>("window.__mark === 'kept'").await,
        "the page reloaded for an edit to the handler alone\n{}",
        dev.output()
    );
    assert!(
        page.errors().is_empty(),
        "the page threw: {:?}",
        page.errors()
    );
    page.close().await;
}
