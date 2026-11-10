// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The `ViteDevServer` a `configureServer` hook receives, against Vite's own
//! behaviour in plain dev (server/index.ts): a middleware that rewrites
//! `req.url` before `next()` has that url served by the middlewares after
//! it (the HTML through `indexHtmlMiddleware`'s transform, a module through
//! `transformMiddleware`, a public file verbatim), the user's middlewares see
//! the browser's url with its base since `baseMiddleware` runs after them,
//! and `server.ssrLoadModule` fetches modules as ssr/fetchModule.ts does: a
//! relative import and an aliased one are modules of the graph, a virtual id
//! is a plugin's, an installed package runs under Node, `import.meta.env`
//! has the mode's values with SSR on, a file change drops the module, and an
//! evaluation error reaches the caller.

use std::time::Duration;

use crate::plugin_middleware::middleware_ready;
use crate::{App, INDEX_HTML};

/// A plugin whose middleware maps clean urls onto what oj's own routing
/// serves, and answers one route itself.
const REWRITES: &str = r#"
const rewrites = {
  name: "rewrites",
  configureServer(server) {
    server.middlewares.use((req, res, next) => {
      const [path, query] = req.url.split("?");
      const q = query ? "?" + query : "";
      // What this middleware saw, for the test to read back.
      if (path.endsWith("/__seen")) {
        res.setHeader("content-type", "text/plain");
        res.end(req.url);
        return;
      }
      const base = path.slice(0, path.lastIndexOf("/") + 1);
      const name = path.slice(base.length);
      if (name === "clean") req.url = base + "pages/clean/index.html" + q;
      else if (name === "mod") req.url = base + "src/greet.ts" + q;
      else if (name === "pub") req.url = base + "logo.svg" + q;
      next();
    });
  },
  transformIndexHtml(html) {
    return html.replace("</head>", "<meta name=\"shim\" content=\"transformed\"></head>");
  },
};
"#;

const CLEAN_HTML: &str = r#"<!doctype html><html><head><title>clean</title></head><body><p>clean-page-marker</p><script type="module" src="/src/main.js"></script></body></html>"#;

fn rewrite_app(config_head: &str) -> App {
    App::new(&[
        (
            "package.json",
            r#"{"name":"shim-rewrites","version":"1.0.0"}"#,
        ),
        (
            "vite.config.mjs",
            &format!("{REWRITES}\nexport default {{ {config_head} plugins: [rewrites] }};\n"),
        ),
        ("index.html", INDEX_HTML),
        ("pages/clean/index.html", CLEAN_HTML),
        ("src/main.js", "console.log(1);\n"),
        (
            "src/greet.ts",
            "export const greet = (name: string): string => `hi ${name}`;\n",
        ),
        (
            "public/logo.svg",
            "<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>",
        ),
    ])
}

/// A rewritten url is served by what comes after the user's middlewares in
/// Vite: the HTML through the transform (with oj's client injected, as
/// `indexHtmlMiddleware` injects Vite's), a module compiled (as
/// `transformMiddleware`), a public file as it is (as
/// `servePublicMiddleware`). A url nothing rewrote is served as asked.
#[tokio::test(flavor = "multi_thread")]
async fn a_rewritten_url_is_served_by_the_middlewares_after_the_plugins() {
    let app = rewrite_app("");
    let server = app.serve().await;
    middleware_ready(&server).await;

    let page = server.get("/clean?tab=1").await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(page.body.contains("clean-page-marker"), "{}", page.body);
    assert!(
        page.body
            .contains(r#"<meta name="shim" content="transformed">"#),
        "the rewritten page skipped transformIndexHtml:\n{}",
        page.body
    );
    assert!(
        page.body.contains("/@oj/client.js"),
        "the rewritten page has no dev client:\n{}",
        page.body
    );

    let module = server.get("/mod").await;
    assert_eq!(module.status, 200, "{}", module.body);
    assert!(
        module.headers["content-type"]
            .to_str()
            .unwrap()
            .contains("javascript"),
        "{:?}",
        module.headers["content-type"]
    );
    assert!(
        module.body.contains("hi ${name}") && !module.body.contains(": string"),
        "the rewritten module was not compiled:\n{}",
        module.body
    );

    let public = server.get("/pub").await;
    assert_eq!(public.status, 200, "{}", public.body);
    assert_eq!(
        public.body, "<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>",
        "the public file was not served verbatim"
    );

    // A url nothing rewrote takes oj's own routing as asked: the SPA
    // fallback to the root page (Vite's htmlFallbackMiddleware), not the
    // rewritten page.
    let untouched = server.get("/nothing-here").await;
    assert_eq!(untouched.status, 200, "{}", untouched.body);
    assert!(
        untouched.body.contains("/src/main.js") && !untouched.body.contains("clean-page-marker"),
        "{}",
        untouched.body
    );
}

/// Under a non-root `base`, the user's middlewares see the browser's url
/// with the base (Vite's `baseMiddleware` runs after them), and a rewrite
/// they make in that form is served under the base, its page naming oj's
/// client under the base too.
#[tokio::test(flavor = "multi_thread")]
async fn a_rewrite_under_a_base_is_seen_and_served_in_the_browsers_form() {
    let app = rewrite_app("base: \"/app/\",");
    let server = app.serve().await;
    middleware_ready(&server).await;

    let seen = server.get("/app/x/__seen?q=1").await;
    assert_eq!(seen.status, 200, "{}", seen.body);
    assert_eq!(
        seen.body, "/app/x/__seen?q=1",
        "the middleware saw a stripped url"
    );

    let page = server.get("/app/clean?tab=1").await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(page.body.contains("clean-page-marker"), "{}", page.body);
    assert!(
        page.body
            .contains(r#"<meta name="shim" content="transformed">"#),
        "{}",
        page.body
    );
    assert!(
        page.body.contains("src=\"/app/@oj/client.js\""),
        "the client is not under the base:\n{}",
        page.body
    );

    // Untouched urls under the base serve as before: the page, a module, a
    // miss.
    let index = server.text("/app/").await;
    assert!(index.contains("src=\"/app/src/main.js\""), "{index}");
    let module = server.get("/app/src/greet.ts").await;
    assert_eq!(module.status, 200, "{}", module.body);
    let missing = server.get("/app/nothing-here").await;
    assert_eq!(missing.status, 200, "{}", missing.body);
    assert!(
        missing.body.contains("src=\"/app/src/main.js\"")
            && !missing.body.contains("clean-page-marker"),
        "the SPA fallback under the base:\n{}",
        missing.body
    );
}

/// A plugin whose middleware loads server modules through
/// `server.ssrLoadModule`, with a virtual module of its own for them to
/// import, and an alias into the app.
const SSR_PLUGIN: &str = r#"
import { fileURLToPath } from "node:url";
const src = fileURLToPath(new URL("./src", import.meta.url));
const ssr = {
  name: "ssr-shim",
  resolveId(id) {
    if (id === "virtual:shim") return "\0virtual:shim";
  },
  load(id) {
    if (id === "\0virtual:shim") return "export const virtual = \"virtual-ok\";";
  },
  configureServer(server) {
    server.middlewares.use(async (req, res, next) => {
      const url = req.url.split("?")[0];
      const answer = (status, body) => {
        res.statusCode = status;
        res.setHeader("content-type", "text/plain");
        res.end(body);
      };
      if (url === "/__ssr/load") {
        try {
          const mod = await server.ssrLoadModule("/src/server/entry.ts");
          return answer(200, JSON.stringify(mod.report()));
        } catch (e) {
          return answer(500, `ssrLoadModule failed: ${e.stack || e}`);
        }
      }
      if (url === "/__ssr/throw") {
        try {
          await server.ssrLoadModule("/src/server/throws.ts", { fixStacktrace: true });
          return answer(500, "no error");
        } catch (e) {
          server.ssrFixStacktrace(e);
          server.ssrFixStacktrace(e);
          return answer(200, `${e.constructor.name}: ${e.message}; stack: ${typeof e.stack}`);
        }
      }
      if (url === "/__ssr/missing") {
        try {
          await server.ssrLoadModule("/src/server/imports-missing.ts");
          return answer(500, "no error");
        } catch (e) {
          return answer(200, `${e.code || "no code"}: ${e.message}`);
        }
      }
      next();
    });
  },
};
export default { resolve: { alias: { "@": src } }, plugins: [ssr] };
"#;

const ENTRY: &str = r#"import path from "node:path";
import * as React from "react";
import { virtual } from "virtual:shim";
import { aliased } from "@/lib/aliased";
import { relative } from "./relative";

export function report() {
  return {
    relative,
    aliased,
    virtual,
    react: typeof (React.default ?? React).version,
    builtin: path.basename("/a/b.txt"),
    ssr: import.meta.env.SSR,
    mode: import.meta.env.MODE,
    dev: import.meta.env.DEV,
  };
}
"#;

fn ssr_app() -> App {
    let app = App::new(&[
        ("package.json", r#"{"name":"shim-ssr","version":"1.0.0"}"#),
        ("vite.config.mjs", SSR_PLUGIN),
        ("index.html", INDEX_HTML),
        ("src/main.js", "console.log(1);\n"),
        ("src/server/entry.ts", ENTRY),
        (
            "src/server/relative.ts",
            "export const relative: string = \"relative-v1\";\n",
        ),
        (
            "src/lib/aliased.ts",
            "export const aliased: string = \"aliased-ok\";\n",
        ),
        (
            "src/server/throws.ts",
            "export const x: number = 1;\nthrow new Error(\"boom from the module\");\n",
        ),
        (
            "src/server/imports-missing.ts",
            "import { nope } from \"./nope\";\nexport const y = nope;\n",
        ),
    ]);
    // The runner and the SSR transform come from the app's own Vite, as
    // Vite's ssrLoadModule uses its own; `react` is the installed package
    // the entry imports.
    app.link_fixture("vite");
    app.link_fixture("react");
    app
}

/// `ssrLoadModule` as Vite's fetchModule: the relative import, the aliased
/// import (importAnalysis never externalizes an aliased specifier) and the
/// plugin's virtual module are modules of the graph, through the plugins and
/// the TypeScript strip; an installed package runs under Node's loader; a
/// builtin is itself; `import.meta.env` carries the mode with SSR on.
#[tokio::test(flavor = "multi_thread")]
async fn ssr_load_module_fetches_modules_as_vite_does() {
    let app = ssr_app();
    let server = app.serve().await;
    middleware_ready(&server).await;

    let res = server.get("/__ssr/load").await;
    assert_eq!(res.status, 200, "{}", res.body);
    let got: serde_json::Value = serde_json::from_str(&res.body).unwrap();
    assert_eq!(
        got,
        serde_json::json!({
            "relative": "relative-v1",
            "aliased": "aliased-ok",
            "virtual": "virtual-ok",
            "react": "string",
            "builtin": "b.txt",
            "ssr": true,
            "mode": "development",
            "dev": true,
        })
    );
}

/// A change to a module drops it from the runner, as Vite's runner drops an
/// invalidated module over its HMR channel: the next load evaluates the
/// file as it is now.
#[tokio::test(flavor = "multi_thread")]
async fn ssr_load_module_sees_a_changed_module_on_the_next_load() {
    let app = ssr_app();
    let server = app.serve().await;
    middleware_ready(&server).await;

    let first = server.get("/__ssr/load").await;
    assert_eq!(first.status, 200, "{}", first.body);
    assert!(first.body.contains("relative-v1"), "{}", first.body);

    app.write(&[(
        "src/server/relative.ts",
        "export const relative: string = \"relative-v2\";\n",
    )]);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let again = server.get("/__ssr/load").await;
        assert_eq!(again.status, 200, "{}", again.body);
        if again.body.contains("relative-v2") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the runner kept the old module: {}",
            again.body
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// An error thrown while a module evaluates reaches the middleware as the
/// error it was, `fixStacktrace` or not, and `ssrFixStacktrace` on it is
/// safe to call more than once (Vite rewrites a stack once). A module whose
/// import resolves to nothing fails as Vite's fetchModule does, with
/// `ERR_MODULE_NOT_FOUND` naming the import.
#[tokio::test(flavor = "multi_thread")]
async fn ssr_load_module_errors_reach_the_middleware() {
    let app = ssr_app();
    let server = app.serve().await;
    middleware_ready(&server).await;

    let thrown = server.get("/__ssr/throw").await;
    assert_eq!(thrown.status, 200, "{}", thrown.body);
    assert_eq!(thrown.body, "Error: boom from the module; stack: string");

    let missing = server.get("/__ssr/missing").await;
    assert_eq!(missing.status, 200, "{}", missing.body);
    assert!(
        missing
            .body
            .starts_with("ERR_MODULE_NOT_FOUND: Cannot find module './nope'"),
        "{}",
        missing.body
    );
}
