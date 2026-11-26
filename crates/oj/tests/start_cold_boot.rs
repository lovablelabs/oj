// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! What a cold start does before its first page, as Vite's dev server does
//! it.
//!
//! The app's dependencies take none of its plugins: Vite pre-bundles them
//! with its own resolver and no plugin of the app, and the plugins see the
//! modules a page asks for, which are the app's. What Vite does not
//! pre-bundle it serves through the plugins: a package in
//! `optimizeDeps.exclude`, and what such a package imports (an import from a
//! module served out of `node_modules` is not pre-bundled). The dev client
//! bundle sent every module of its graph to the plugins, the dependencies'
//! thousands among them, on the plugin host's one thread, before the server
//! answered.
//!
//! And the server accepts before the client bundle is built: Vite's `listen`
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
fn the_client_bundles_dependencies_take_no_plugin_and_what_vite_serves_takes_them() {
    let Some(app) = start_app::app("cold-boot-deps") else {
        return;
    };
    let dir = app.0.clone();
    let log = dir.join("hooks.log");
    // `scheduler`: a package react-dom imports, excluded by the app. Start
    // excludes its own packages itself (`@tanstack/react-router` among
    // them), and `@tanstack/router-core` is one of those's dependencies.
    with_recorder(&dir, &log, "\"scheduler\"");
    let dev = Server::oj_dev(&dir);
    let (status, body) = dev.request("GET", "/", &[]);
    assert_eq!(status, 200, "{body}\n{}", dev.output());

    for hook in ["resolveId", "load", "transform"] {
        let ids = seen(&log, hook);
        let given = |package: &str| {
            let dir = format!("/node_modules/{package}/");
            ids.iter().any(|id| id.contains(&dir) && !id.contains('?'))
        };
        assert!(
            ids.iter().any(|id| id.contains("/src/routes/")),
            "{hook}: the plugin was not given the app's own modules:\n{}",
            ids.join("\n")
        );
        // Dependencies Vite pre-bundles: their scripts are the bundler's.
        for package in ["react", "react-dom"] {
            assert!(
                !given(package),
                "{hook}: the plugin was given the modules of {package}, a pre-bundled dependency:\n{}",
                ids.join("\n")
            );
        }
        // What it serves through the plugins: a package the app excludes,
        // one a plugin excludes, and one such a package depends on.
        for package in ["@tanstack/react-router", "@tanstack/router-core"] {
            assert!(
                given(package),
                "{hook}: the plugin was not given the modules of {package}, which Vite serves:\n{}",
                ids.join("\n")
            );
        }
    }
    // `scheduler` is one file with no import of its own: no `resolveId` has
    // it for an importer.
    for hook in ["load", "transform"] {
        let ids = seen(&log, hook);
        assert!(
            ids.iter().any(|id| id.contains("/node_modules/scheduler/")),
            "{hook}: the plugin was not given the modules of a package in the app's optimizeDeps.exclude:\n{}",
            ids.join("\n")
        );
    }
}

#[test]
fn the_server_accepts_before_the_client_bundle_is_built_and_answers_a_request_made_then() {
    let Some(app) = start_app::app("cold-boot-port") else {
        return;
    };
    let dir = app.0.clone();
    let line = "listening (port ";
    let mut early = None;
    // `oj_dev_at_with` checks that the server has not printed its address
    // (its last step, once it serves) when the line shows, then runs the
    // step: a request made while the client bundle is still being built.
    let dev = Server::oj_dev_at_with(&dir, &[("DEBUG", "oj:boot")], line, |dev| {
        let out = dev.output();
        let port: u16 = out[out.find(line).unwrap() + line.len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .unwrap();
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))
            .expect("the port takes no connection though the server says it listens");
        stream
            .write_all(
                format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
        // Still to come when the request was written: the bundle is what
        // the request waits for.
        assert!(
            !dev.output().contains("bundle+build joined"),
            "the client bundle was built before the server listened:\n{}",
            dev.output()
        );
        early = Some(stream);
    });
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
