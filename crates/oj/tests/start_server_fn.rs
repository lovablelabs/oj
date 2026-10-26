// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! TanStack Start server functions end to end on the `oj` binary, in dev and
//! in a build: a function behind a middleware that imports
//! `@tanstack/react-start/server`. That import reaches Start's handler, which
//! imports `#tanstack-start-server-fn-resolver`; a resolver that imported
//! every `*.functions.ts` statically ran the function module inside that
//! import, while its middleware module was still mid-evaluation: `Cannot
//! access 'authed' before initialization` and every SSR request 500. Start's
//! own resolver loads each module on the first call. The SSR entry imports
//! the middleware first, which fixes the order (a real app hit it at random,
//! by which module the server graph reached first).
//!
//! The app is the Start fixture (see `start_app`). The browser half skips
//! without Chrome, unless `OJ_REQUIRE_BROWSER` is set.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;
#[allow(dead_code)]
mod start_app;

use std::path::Path;

use browser::Page;
use start_app::{app, build, Server, WAIT};

/// The middleware, the function behind it, an SSR entry that imports the
/// middleware before anything else, and the about page calling the function
/// once it hydrates.
fn write_guarded(root: &Path) {
    std::fs::write(
        root.join("src/server/auth.ts"),
        r#"import { createMiddleware } from "@tanstack/react-start";
import { getRequest } from "@tanstack/react-start/server";

export const authed = createMiddleware({ type: "function" }).server(({ next }) =>
  next({ context: { method: getRequest()?.method ?? "none" } }),
);
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("src/server/guarded.functions.ts"),
        r#"import { createServerFn } from "@tanstack/react-start";
import { authed } from "./auth";

export const guarded = createServerFn({ method: "POST" })
  .middleware([authed])
  .handler(async ({ context }) => `guarded-${context.method}`);
"#,
    )
    .unwrap();
    let entry = root.join("src/ssr-entry.ts");
    let source = std::fs::read_to_string(&entry).unwrap();
    std::fs::write(&entry, format!("import \"./server/auth\";\n{source}")).unwrap();

    let about = root.join("src/routes/about.tsx");
    let source = std::fs::read_to_string(&about).unwrap();
    let import = "import { rootRoute } from \"./__root\";";
    let heading = "<h1 className=\"fixture-heading\">about-page-marker</h1>";
    assert!(
        source.contains(import) && source.contains(heading),
        "{source}"
    );
    let source = source
        .replacen(
            import,
            &format!(
                r#"{import}
import {{ useEffect, useState }} from "react";
import {{ guarded }} from "../server/guarded.functions";
function Guarded() {{
  const [out, setOut] = useState("pending");
  useEffect(() => {{
    guarded().then((r) => setOut(r), (e) => setOut("error " + String(e)));
  }}, []);
  return <p id="guarded">{{out}}</p>;
}}"#
            ),
            1,
        )
        .replacen(heading, &format!("{heading}\n      <Guarded />"), 1);
    std::fs::write(&about, source).unwrap();
}

/// The page renders, the function runs behind its middleware (over HTTP and
/// from the hydrated page), an id naming an `Object.prototype` member fails
/// as unknown, and no module ran before its import.
async fn renders_and_calls(label: &str, server: &Server) {
    let (status, body) = server.request("GET", "/about", &[]);
    assert!(
        status == 200 && body.contains("about-page-marker"),
        "{label}: /about answered {status}, want the rendered page\n{}\n{}",
        &body[..body.len().min(500)],
        server.output()
    );
    let id = oj_compiler::server_fns::server_fn_id("src/server/guarded.functions.ts", "guarded");
    let (status, body) = server.call(&id);
    assert!(
        status == 200 && body.contains("guarded-POST"),
        "{label}: the server fn answered {status} {body:?}, want guarded-POST\n{}",
        server.output()
    );
    if !browser::skip() {
        let page = Page::open(&server.url("/about")).await;
        let ran = page
            .poll(
                "document.querySelector('#guarded')?.textContent === 'guarded-POST'",
                WAIT,
            )
            .await;
        let shown: Option<String> = page
            .eval("document.querySelector('#guarded')?.textContent ?? null")
            .await;
        assert!(
            ran,
            "{label}: the page's call answered {shown:?}, want guarded-POST; page errors: {:?}\n{}",
            page.errors(),
            server.output()
        );
        assert_eq!(page.errors(), Vec::<String>::new(), "{label}: page errors");
        page.close().await;
    }
    let (status, _) = server.call("constructor");
    assert_eq!(status, 500, "{label}: /_serverFn/constructor");
    assert!(
        server.logs("Server function info not found for constructor"),
        "{label}: /_serverFn/constructor did not fail as an unknown id\n{}",
        server.output()
    );
    assert!(
        !server.output().contains("before initialization"),
        "{label}: a module ran before its import\n{}",
        server.output()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_fn_whose_middleware_imports_starts_server_renders_and_runs() {
    let Some(app) = app("cycle") else {
        return;
    };
    let root = &app.0;
    write_guarded(root);

    let dev = Server::oj_dev(root);
    renders_and_calls("dev", &dev).await;
    drop(dev);

    build(root);
    let built = Server::node_server(root);
    renders_and_calls("build", &built).await;
}
