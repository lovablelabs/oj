// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! What a cold start does before its first page, as Vite's dev server does
//! it.
//!
//! The app's dependencies take none of its plugins: Vite pre-bundles them
//! with its own resolver and no plugin of the app, and the plugins see the
//! modules a page asks for, which are the app's. A package in
//! `optimizeDeps.exclude` is not pre-bundled and is served through the
//! plugins. The dev client bundle sent every module of its graph to the
//! plugins, the dependencies' thousands among them, on the plugin host's one
//! thread, before the server answered.
//!
//! And the port opens before the client bundle is built: Vite's `listen`
//! starts the dependency optimizer without waiting for it, and the requests
//! that need it wait. A host that waits for the port gave up on a large app.

#[allow(dead_code)]
mod start_app;

use start_app::Server;
use std::io::{Read, Write};
use std::path::Path;

/// A plugin that writes down, one line each, the importer of every
/// `resolveId` and the id of every `load` and `transform` it is given, with
/// the environment that asked.
fn recorder(log: &Path) -> String {
    format!(
        r#"import {{ appendFileSync }} from "node:fs";
function recordingPlugin() {{
  const note = (ctx, hook, id) =>
    appendFileSync({log:?}, `${{ctx.environment?.name ?? "?"}} ${{hook}} ${{id}}\n`);
  return {{
    name: "fixture-records-hooks",
    // Before Vite's resolver: a later plugin is only asked for what the
    // resolver could not find.
    enforce: "pre",
    resolveId(_source, importer) {{
      if (importer) note(this, "resolveId", importer);
    }},
    load(id) {{
      note(this, "load", id);
    }},
    transform(_code, id) {{
      note(this, "transform", id);
    }},
  }};
}}
"#,
        log = log.display().to_string()
    )
}

fn with_recorder(root: &Path, log: &Path, exclude: &str) {
    let config = root.join("vite.config.ts");
    let source = std::fs::read_to_string(&config).unwrap();
    assert!(
        source.contains("  plugins: [\n") && source.contains("  publicDir: "),
        "{source}"
    );
    let source = source
        .replacen(
            "  plugins: [\n",
            "  plugins: [\n    recordingPlugin(),\n",
            1,
        )
        .replacen(
            "export default defineConfig(",
            &format!("{}\nexport default defineConfig(", recorder(log)),
            1,
        )
        .replacen(
            "  publicDir: ",
            &format!("  optimizeDeps: {{ exclude: [{exclude}] }},\n  publicDir: "),
            1,
        );
    std::fs::write(&config, source).unwrap();
}

/// The lines of the client environment for `hook`: the ids it was given.
fn seen(log: &Path, hook: &str) -> Vec<String> {
    let prefix = format!("client {hook} ");
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.strip_prefix(&prefix).map(str::to_string))
        .collect()
}

#[test]
fn the_client_bundles_dependencies_take_no_plugin_and_an_excluded_package_takes_them() {
    let Some(app) = start_app::app("cold-boot-deps") else {
        return;
    };
    let dir = app.0.clone();
    let log = dir.join("hooks.log");
    with_recorder(&dir, &log, "\"@tanstack/react-router\"");
    let dev = Server::oj_dev(&dir);
    let (status, body) = dev.request("GET", "/", &[]);
    assert_eq!(status, 200, "{body}\n{}", dev.output());

    for hook in ["resolveId", "load", "transform"] {
        let ids = seen(&log, hook);
        assert!(
            ids.iter().any(|id| id.contains("/src/routes/")),
            "{hook}: the plugin was not given the app's own modules:\n{}",
            ids.join("\n")
        );
        // A dependency Vite pre-bundles: its scripts are the bundler's.
        let deps: Vec<&String> = ids
            .iter()
            .filter(|id| {
                id.contains("/node_modules/react/") || id.contains("/node_modules/react-dom/")
            })
            .filter(|id| !id.contains('?'))
            .collect();
        assert!(
            deps.is_empty(),
            "{hook}: the plugin was given a pre-bundled dependency's modules: {deps:?}"
        );
        // One it does not pre-bundle is served through the plugins.
        assert!(
            ids.iter()
                .any(|id| id.contains("/node_modules/@tanstack/react-router/")),
            "{hook}: the plugin was not given the modules of a package in optimizeDeps.exclude:\n{}",
            ids.join("\n")
        );
    }
}

#[test]
fn the_port_opens_before_the_client_bundle_is_built_and_a_request_made_then_is_answered() {
    let Some(app) = start_app::app("cold-boot-port") else {
        return;
    };
    let dir = app.0.clone();
    let port = start_app::free_port();
    let line = format!("listening (port {port})");
    let mut early = None;
    // `oj_dev_at_on` checks that the server has not printed its address
    // (its last step, once it serves) when the line shows, then runs the
    // step: a connection made while the client bundle is still to come.
    let dev = Server::oj_dev_at_on(&dir, port, &[("DEBUG", "oj:boot")], &line, || {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))
            .expect("the port takes no connection though the server says it listens");
        stream
            .write_all(
                format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
        early = Some(stream);
    });
    let out = dev.output();
    let (listening, joined) = (
        out.find(&line).expect("no listening phase"),
        out.find("bundle+build joined")
            .expect("no client bundle phase"),
    );
    assert!(
        listening < joined,
        "the port opened after the client bundle was built:\n{out}"
    );
    // The request made before the server served is answered once it does.
    let mut stream = early.expect("the step did not run");
    stream.set_read_timeout(Some(start_app::WAIT)).unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "the request made while the bundle was built was not answered:\n{}\n{}",
        response.chars().take(400).collect::<String>(),
        dev.output()
    );
}
