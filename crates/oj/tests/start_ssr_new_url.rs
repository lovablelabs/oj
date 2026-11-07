// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `new URL("./data/", import.meta.url)` in server code, as Vite runs it:
//! its asset-import-meta-url rewrite applies to the client environment only
//! (`applyToEnvironment: consumer === "client"`), so on the server the
//! expression stays and resolves at run time against the module's own file
//! (the module runner's `import.meta.url` is the source file's href). A
//! server function reading a file next to its module through a directory URL
//! answers in dev, and the build passes. Before, oj rewrote the URL into a
//! `./data/?url` import in SSR, which failed to resolve and 500'd every page.
//!
//! The app is the Start fixture (see `start_app`).

#[allow(dead_code)]
mod start_app;

use std::path::Path;

use start_app::{app, build, Server};

fn write_app(root: &Path) {
    let server = root.join("src/server");
    std::fs::create_dir_all(server.join("data")).unwrap();
    std::fs::write(server.join("data/hello.txt"), "from-the-data-dir\n").unwrap();
    std::fs::write(
        server.join("geo.server.ts"),
        r#"import { readFileSync } from "node:fs";

const dir = new URL("./data/", import.meta.url);

export const greeting = () => readFileSync(new URL("hello.txt", dir), "utf8").trim();
"#,
    )
    .unwrap();
    std::fs::write(
        server.join("geo.functions.ts"),
        r#"import { createServerFn } from "@tanstack/react-start";
import { greeting } from "./geo.server";

export const readGeo = createServerFn({ method: "POST" }).handler(async () => greeting());
"#,
    )
    .unwrap();
}

#[test]
fn a_directory_url_in_server_code_resolves_next_to_its_module() {
    let Some(app) = app("ssr-new-url") else {
        return;
    };
    let root = &app.0;
    write_app(root);

    let dev = Server::oj_dev(root);
    let (status, _) = dev.request("GET", "/", &[]);
    assert_eq!(status, 200, "the page\n{}", dev.output());
    let id = oj_compiler::server_fns::server_fn_id("src/server/geo.functions.ts", "readGeo");
    let (status, body) = dev.call(&id);
    assert!(
        status == 200 && body.contains("from-the-data-dir"),
        "the server function answered {status} {body:?}\n{}",
        dev.output()
    );
    assert!(
        !dev.output().contains("?url"),
        "the server URL was rewritten into an import\n{}",
        dev.output()
    );
    drop(dev);

    build(root);
}
