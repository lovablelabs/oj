// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The `ViteDevServer` a `configureServer` hook receives under `oj dev`, for
//! the pieces plugins use beyond `middlewares.use`: `server.ssrLoadModule`
//! runs a module of the app for a middleware (Vite's ssr/ssrModuleLoader.ts),
//! `server.ssrFixStacktrace` is callable on an error a middleware caught
//! (ssr/ssrStacktrace.ts), a hook may call a plugin chain through
//! `server.environments.<name>.pluginContainer` while the host is still
//! booting, and a middleware that rewrites `req.url` before `next()` has the
//! document it named served, as Vite's later middlewares serve the rewritten
//! url (server/index.ts: user middlewares run ahead of the html fallback).
//!
//! The app is the Start fixture (see `start_app`), under a config that adds
//! the probing plugin. Skips without the fixture, unless `OJ_REQUIRE_FIXTURES`.

#[allow(dead_code)]
mod start_app;

use start_app::{app, Server};

/// The fixture under `vite.shim.config.ts`: its own config (the routes need
/// its plugins) with one more plugin, whose configureServer registers the
/// probes; the hook-time probe writes to the returned log file.
fn shim_app(name: &str) -> Option<(start_app::App, std::path::PathBuf)> {
    let app = app(name)?;
    let root = app.0.clone();
    let log = root.join("shim.log");
    std::fs::write(
        root.join("src/server/shim-greeting.ts"),
        r#"import path from "node:path";
import { shout } from "../lib/format";

// A TypeScript module with a relative import and a builtin import, loaded
// by the shim plugin's middleware through server.ssrLoadModule.
type Greeting = { text: string; file: string; ssr: boolean };
export function greeting(): Greeting {
  return { text: shout("shim loaded"), file: path.basename("/x/y/greeting.ts"), ssr: import.meta.env.SSR };
}
"#,
    )
    .unwrap();
    let shim = format!(
        r#"import fs from "node:fs";
import path from "node:path";

const log = (line) => fs.appendFileSync({log:?}, line + "\n");
const shim = {{
  name: "shim",
  configureServer(server) {{
    // At hook time, while the host boots: a load chain through the
    // environment's plugin container, as a plugin collecting styles does.
    const file = path.resolve(__dirname, "src/lib/format.ts");
    server.environments.client.pluginContainer
      .load(file)
      .then((r) => log(`load ${{r == null ? "null" : "result"}}`), (e) => log(`load error ${{e.message}}`));
    server.middlewares.use(async (req, res, next) => {{
      const url = req.url.split("?")[0];
      if (url === "/__shim/ssr-load") {{
        try {{
          const mod = await server.ssrLoadModule("/src/server/shim-greeting.ts");
          res.setHeader("content-type", "application/json");
          res.end(JSON.stringify(mod.greeting()));
        }} catch (e) {{
          res.statusCode = 500;
          res.end(`ssrLoadModule failed: ${{e.stack || e}}`);
        }}
        return;
      }}
      if (url === "/__shim/fix-stack") {{
        try {{
          throw new Error("shim middleware error");
        }} catch (e) {{
          server.ssrFixStacktrace(e);
          res.setHeader("content-type", "text/plain");
          res.end(`fixed: ${{e.message}}`);
        }}
        return;
      }}
      // A clean route mapped onto a document, then passed on.
      if (url === "/__shim/clean") req.url = "/about" + req.url.slice(url.length);
      next();
    }});
  }},
}};
"#
    );
    let base = std::fs::read_to_string(root.join("vite.config.ts")).unwrap();
    let marker = "export default defineConfig({";
    assert!(
        base.contains(marker) && base.contains("plugins: ["),
        "the fixture config changed shape"
    );
    let config = base
        .replacen(marker, &format!("{shim}\n{marker}"), 1)
        .replacen("plugins: [", "plugins: [\n    shim,", 1);
    std::fs::write(root.join("vite.shim.config.ts"), config).unwrap();
    Some((app, log))
}

fn shim_dev(root: &std::path::Path) -> Server {
    Server::oj_dev_with_args(root, &["--config", "vite.shim.config.ts"], &[])
}

/// `server.ssrLoadModule` evaluates a TypeScript module of the app, with its
/// relative import, a builtin import and `import.meta.env.SSR`, and returns
/// its namespace to the middleware.
#[test]
fn ssr_load_module_runs_a_module_of_the_app_for_a_middleware() {
    let Some((app, _log)) = shim_app("shim-ssr-load") else {
        return;
    };
    let dev = shim_dev(&app.0);
    let (status, body) = dev.request("GET", "/__shim/ssr-load", &[]);
    assert_eq!(
        (status, body.as_str()),
        (
            200,
            r#"{"text":"SHIM LOADED!","file":"greeting.ts","ssr":true}"#
        ),
        "{}",
        dev.output()
    );
}

/// `server.ssrFixStacktrace` exists and leaves the error usable, so a
/// middleware's `catch` that calls it answers instead of throwing a
/// TypeError of its own (which would hang or 500 the request).
#[test]
fn ssr_fix_stacktrace_is_callable_on_a_caught_error() {
    let Some((app, _log)) = shim_app("shim-fix-stack") else {
        return;
    };
    let dev = shim_dev(&app.0);
    let (status, body) = dev.request("GET", "/__shim/fix-stack", &[]);
    assert_eq!(
        (status, body.as_str()),
        (200, "fixed: shim middleware error"),
        "{}",
        dev.output()
    );
}

/// A configureServer hook reaching a plugin chain through
/// `server.environments.client.pluginContainer` while the host boots gets an
/// answer, not a ReferenceError that has the host skip the hook.
#[test]
fn a_hook_may_call_a_plugin_chain_while_the_host_boots() {
    let Some((app, log)) = shim_app("shim-boot-chain") else {
        return;
    };
    let dev = shim_dev(&app.0);
    let (status, _) = dev.request("GET", "/", &[]);
    assert_eq!(status, 200, "{}", dev.output());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let heard = loop {
        let heard = std::fs::read_to_string(&log).unwrap_or_default();
        if heard.contains("load ") || std::time::Instant::now() > deadline {
            break heard;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    assert_eq!(heard.trim(), "load null", "{}", dev.output());
    assert!(
        !dev.output().contains("configureServer(shim) skipped"),
        "the hook was skipped:\n{}",
        dev.output()
    );
}

/// A middleware that rewrites `req.url` and calls `next()` has the document
/// it named rendered, as Vite's own middlewares after it serve the rewritten
/// url; the query travels with it.
#[test]
fn a_middleware_rewriting_the_url_has_that_document_served() {
    let Some((app, _log)) = shim_app("shim-rewrite") else {
        return;
    };
    let dev = shim_dev(&app.0);
    let (status, body) = dev.request("GET", "/__shim/clean?tab=1", &[]);
    assert_eq!(status, 200, "{}\n{}", body, dev.output());
    assert!(
        body.contains("about-page-marker"),
        "the rewritten document was not rendered:\n{body}\n{}",
        dev.output()
    );
    // The url the middleware did not touch still renders as asked.
    let (status, body) = dev.request("GET", "/about", &[]);
    assert_eq!(status, 200, "{}", dev.output());
    assert!(body.contains("about-page-marker"), "{body}");
}
