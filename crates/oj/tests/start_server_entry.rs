// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! An app's own server entry written the way `@tanstack/react-start`
//! documents it: `import handler, { createServerEntry } from
//! "@tanstack/react-start/server-entry"` and `export default
//! createServerEntry({ fetch })`. The package's `server-entry` module
//! (dist/default-entry/esm/server.js) exports `createServerEntry` beside the
//! default handler, so the entry must import both from oj's stand-in for it,
//! in dev and in the production bundle. Before, oj's module had only the
//! default export and every route answered 500 with "does not provide an
//! export named 'createServerEntry'".
//!
//! The app is the Start fixture (see `start_app`), its `src/ssr-entry.ts`
//! rewritten to the documented shape.

#[allow(dead_code)]
mod start_app;

use std::path::Path;

use start_app::{app, build, Server};

const ENTRY: &str = r#"import handler, { createServerEntry } from "@tanstack/react-start/server-entry";

export default createServerEntry({
  async fetch(request) {
    const res = await handler.fetch(request);
    const headers = new Headers(res.headers);
    headers.set("x-server-entry", "created");
    return new Response(res.body, { status: res.status, statusText: res.statusText, headers });
  },
});
"#;

fn write_entry(root: &Path) {
    std::fs::write(root.join("src/ssr-entry.ts"), ENTRY).unwrap();
}

fn answers_through_the_entry(label: &str, server: &Server) {
    let (status, headers, body) = server.fetch("GET", "/", &[]);
    let body = String::from_utf8_lossy(&body);
    assert!(
        status == 200,
        "{label}: / answered {status}, want the page through the app's entry\n{}\n{}",
        &body[..body.len().min(600)],
        server.output()
    );
    assert!(
        headers
            .iter()
            .any(|(name, value)| name == "x-server-entry" && value == "created"),
        "{label}: the entry's header is missing from {headers:?}\n{}",
        server.output()
    );
    assert!(
        !server.output().contains("createServerEntry"),
        "{label}: the entry's import failed\n{}",
        server.output()
    );
}

#[test]
fn the_server_entry_module_exports_create_server_entry() {
    let Some(app) = app("server-entry-create") else {
        return;
    };
    let root = &app.0;
    write_entry(root);

    let dev = Server::oj_dev(root);
    answers_through_the_entry("dev", &dev);
    drop(dev);

    build(root);
    let built = Server::node_server(root);
    answers_through_the_entry("build", &built);
}
