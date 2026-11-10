// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! An app's own server entry hands Start's handler a request context,
//! `handler.fetch(request, { context: { nonce } })`, and the app reads it back
//! with `getGlobalStartContext()` while the request renders: in a route's
//! loader (the document) and in a server route's handler. Start's
//! `createStartHandler` takes the context from its second argument
//! (`requestOpts.context`, start-server-core's createStartHandler.js) and the
//! package's `server-entry` default passes every argument of `fetch` on to
//! it. oj's stand-in for that module dropped the second argument, so the
//! context was always empty: an app putting its CSP nonce there rendered its
//! bootstrap scripts without it and never hydrated.
//!
//! The app is the Start fixture (see `start_app`) with a context-reading
//! route added and its `src/ssr-entry.ts` passing the context.

#[allow(dead_code)]
mod start_app;

use std::path::Path;

use start_app::{app, build, Server};

const ENTRY: &str = r#"import serverEntry from "@tanstack/react-start/server-entry";

type Handler = {
  fetch: (request: Request, opts?: { context?: Record<string, unknown> }) => Promise<Response> | Response;
};

export default {
  async fetch(request: Request): Promise<Response> {
    return await (serverEntry as Handler).fetch(request, { context: { nonce: "fixture-nonce-7" } });
  },
};
"#;

const ROUTE: &str = r#"import { createRoute } from "@tanstack/react-router";
import { getGlobalStartContext } from "@tanstack/react-start";

import { rootRoute } from "./__root";

const nonce = () => (getGlobalStartContext() as { nonce?: string } | undefined)?.nonce ?? "none";

export const contextRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/context",
  loader: () => ({ nonce: nonce() }),
  component: ContextPage,
});

function ContextPage() {
  const data = contextRoute.useLoaderData();
  return <p id="nonce">{`context-nonce:${data.nonce}`}</p>;
}

export const contextApiRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/api/context",
  server: {
    handlers: {
      GET: () => Response.json({ nonce: nonce() }),
    },
  },
});
"#;

fn write_app(root: &Path) {
    std::fs::write(root.join("src/ssr-entry.ts"), ENTRY).unwrap();
    std::fs::write(root.join("src/routes/context.tsx"), ROUTE).unwrap();
    let tree = root.join("src/routeTree.ts");
    let source = std::fs::read_to_string(&tree).unwrap();
    assert!(source.contains("resolveErrorRoute]"), "{source}");
    let source = source
        .replacen(
            "import { rootRoute }",
            "import { contextRoute, contextApiRoute } from \"./routes/context\";\nimport { rootRoute }",
            1,
        )
        .replacen(
            "resolveErrorRoute]",
            "resolveErrorRoute, contextRoute, contextApiRoute]",
            1,
        );
    std::fs::write(&tree, source).unwrap();
}

fn reads_the_context(label: &str, server: &Server) {
    let (status, body) = server.request("GET", "/context", &[]);
    assert!(
        status == 200 && body.contains("context-nonce:fixture-nonce-7"),
        "{label}: /context answered {status}, want the loader's nonce from the entry's context\n{}\n{}",
        &body[..body.len().min(800)],
        server.output()
    );
    let (status, body) = server.request("GET", "/api/context", &[]);
    assert!(
        status == 200 && body.contains(r#""nonce":"fixture-nonce-7""#),
        "{label}: /api/context answered {status} {body:?}, want the handler's nonce from the entry's context\n{}",
        server.output()
    );
}

#[test]
fn the_entrys_request_context_reaches_get_global_start_context() {
    let Some(app) = app("request-context") else {
        return;
    };
    let root = &app.0;
    write_app(root);

    let dev = Server::oj_dev(root);
    reads_the_context("dev", &dev);
    drop(dev);

    build(root);
    let built = Server::node_server(root);
    reads_the_context("build", &built);
}
