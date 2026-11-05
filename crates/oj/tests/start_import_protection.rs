// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! TanStack Start's import protection on the `oj` binary, which runs Start's
//! own plugin pair. Every outcome here is the one `vite build` / `vite dev`
//! gives the same app: a client-used `*.server.*` file, a
//! `@tanstack/react-start/server` import and a `server-only` marker fail the
//! build with Start's message (its code frame on the source, through
//! `getCombinedSourcemap`), as a `*.client.*` file does in the server build;
//! an import only a server function's handler uses compiles away and builds;
//! `importProtection` options reach the plugin. In dev the import is mocked,
//! its use logged in the page, and a page opened once the import is gone
//! hydrates.
//!
//! The app is the Start fixture (see `start_app`). The browser half skips
//! without Chrome, unless `OJ_REQUIRE_BROWSER` is set.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;
#[allow(dead_code)]
mod start_app;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use browser::Page;
use start_app::{app, Server};

const SECRET: &str = "export const secret = () => \"server-secret\";\n";

/// The about route, importing `import` and rendering `extra` (set by `body`).
fn about(import: &str, body: &str) -> String {
    format!(
        r#"import {{ createRoute }} from "@tanstack/react-router";
import {{ useEffect, useState }} from "react";
{import}
import {{ rootRoute }} from "./__root";

export const aboutRoute = createRoute({{
  getParentRoute: () => rootRoute,
  path: "/about",
  component: About,
}});

function About() {{
  const [state, setState] = useState("ssr");
  useEffect(() => setState("hydrated"), []);
  {body}
  return (
    <main>
      <h1 className="fixture-heading">about-page-marker</h1>
      <p id="hydrated">{{state}}</p>
      {{extra}}
    </main>
  );
}}
"#
    )
}

const USES_SECRET: &str = "const extra = <p id=\"secret\">{secret()}</p>;";

/// The fixture with `files` written over it and `options` added to
/// `tanstackStart({ ... })`.
fn app_with(name: &str, files: &[(&str, String)], options: Option<&str>) -> Option<start_app::App> {
    let app = app(name)?;
    for (rel, body) in files {
        let path = app.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    if let Some(options) = options {
        let config = app.0.join("vite.config.ts");
        let source = std::fs::read_to_string(&config).unwrap();
        let call = "tanstackStart({ server: { entry: \"ssr-entry\" } })";
        assert!(source.contains(call), "{source}");
        let with = format!("tanstackStart({{ server: {{ entry: \"ssr-entry\" }}, {options} }})");
        std::fs::write(&config, source.replacen(call, &with, 1)).unwrap();
    }
    Some(app)
}

/// `oj build`: whether it succeeded, and its output.
fn build(root: &Path) -> (bool, String) {
    let _ = std::fs::remove_dir_all(root.join(".oj-cache"));
    let out = Command::new(env!("CARGO_BIN_EXE_oj"))
        .arg("build")
        .arg(root)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// Whether any client asset holds `needle`.
fn client_has(root: &Path, needle: &str) -> bool {
    fn walk(dir: &Path, needle: &str) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        entries.flatten().any(|e| {
            let path = e.path();
            if path.is_dir() {
                walk(&path, needle)
            } else {
                std::fs::read_to_string(&path).is_ok_and(|s| s.contains(needle))
            }
        })
    }
    walk(&root.join("dist/client"), needle)
}

fn assert_denied(text: &str, lines: &[&str]) {
    for line in lines {
        assert!(
            text.contains(line),
            "the build output lacks {line:?}:\n{text}"
        );
    }
}

#[test]
fn a_server_only_file_the_client_uses_fails_the_build() {
    let files = [
        ("src/server/secret.server.ts", SECRET.to_string()),
        (
            "src/routes/about.tsx",
            about(
                "import { secret } from \"../server/secret.server\";",
                USES_SECRET,
            ),
        ),
    ];
    let Some(app) = app_with("ip-file", &files, None) else {
        return;
    };
    let (ok, text) = build(&app.0);
    assert!(!ok, "the build succeeded:\n{text}");
    assert_denied(
        &text,
        &[
            "[import-protection] Import denied in client environment",
            "Denied by file pattern: **/*.server.*",
            "Importer: src/routes/about.tsx:15:33",
            "Import: \"../server/secret.server\"",
            "Resolved: src/server/secret.server.ts",
            "1. src/router.tsx:2:28 (entry) (import \"./routeTree\")",
            // The frame is the source the importer wrote, not compiled JSX.
            "> 15 |   const extra = <p id=\"secret\">{secret()}</p>;",
        ],
    );
}

#[test]
fn a_server_only_import_only_a_server_fn_uses_builds() {
    let files = [
        ("src/server/secret.server.ts", SECRET.to_string()),
        (
            "src/routes/about.tsx",
            about(
                "import { createServerFn } from \"@tanstack/react-start\";\n\
                 import { secret } from \"../server/secret.server\";\n\
                 export const getSecret = createServerFn().handler(async () => secret());",
                "const extra = <button onClick={() => getSecret()}>go</button>;",
            ),
        ),
    ];
    let Some(app) = app_with("ip-server-fn", &files, None) else {
        return;
    };
    let (ok, text) = build(&app.0);
    assert!(ok, "the build failed:\n{text}");
    assert!(!text.contains("[import-protection]"), "{text}");
    assert!(
        !client_has(&app.0, "server-secret"),
        "the client holds the server module"
    );
}

#[test]
fn the_start_server_entry_in_client_code_fails_the_build() {
    let files = [(
        "src/routes/about.tsx",
        about(
            "import { getRequestHeader } from \"@tanstack/react-start/server\";",
            "const extra = <p>{String(getRequestHeader(\"x\"))}</p>;",
        ),
    )];
    let Some(app) = app_with("ip-specifier", &files, None) else {
        return;
    };
    let (ok, text) = build(&app.0);
    assert!(!ok, "the build succeeded:\n{text}");
    assert_denied(
        &text,
        &[
            "[import-protection] Import denied in client environment",
            "Denied by specifier pattern: @tanstack/react-start/server",
            "Import: \"@tanstack/react-start/server\"",
        ],
    );
}

#[test]
fn a_server_only_marked_module_the_client_uses_fails_the_build() {
    let files = [
        (
            "src/lib/secret.ts",
            format!("import \"@tanstack/react-start/server-only\";\n{SECRET}"),
        ),
        (
            "src/routes/about.tsx",
            about("import { secret } from \"../lib/secret\";", USES_SECRET),
        ),
    ];
    let Some(app) = app_with("ip-marker", &files, None) else {
        return;
    };
    let (ok, text) = build(&app.0);
    assert!(!ok, "the build succeeded:\n{text}");
    assert_denied(
        &text,
        &[
            "[import-protection] Import denied in client environment",
            "Denied by marker: module is restricted to the opposite environment",
            "Importer: src/lib/secret.ts:1:8",
        ],
    );
}

#[test]
fn a_client_only_file_server_code_uses_fails_the_build() {
    let files = [
        (
            "src/lib/widget.client.ts",
            "export const widget = () => \"client-widget\";\n".to_string(),
        ),
        (
            "src/routes/about.tsx",
            about(
                "import { widget } from \"../lib/widget.client\";",
                "const extra = <p>{widget()}</p>;",
            ),
        ),
    ];
    let Some(app) = app_with("ip-client-file", &files, None) else {
        return;
    };
    let (ok, text) = build(&app.0);
    assert!(!ok, "the build succeeded:\n{text}");
    assert_denied(
        &text,
        &[
            "[import-protection] Import denied in server environment",
            "Denied by file pattern: **/*.client.*",
        ],
    );
}

#[test]
fn import_protection_options_reach_the_plugin() {
    let files = [
        ("src/server/secret.server.ts", SECRET.to_string()),
        (
            "src/routes/about.tsx",
            about(
                "import { secret } from \"../server/secret.server\";",
                USES_SECRET,
            ),
        ),
    ];
    // `behavior: "mock"`: the build warns, and ships the mock.
    let Some(app) = app_with(
        "ip-mock",
        &files,
        Some("importProtection: { behavior: \"mock\" }"),
    ) else {
        return;
    };
    let (ok, text) = build(&app.0);
    assert!(ok, "the mock build failed:\n{text}");
    assert_denied(
        &text,
        &["[import-protection] Import denied in client environment"],
    );
    assert!(
        !client_has(&app.0, "server-secret"),
        "the client holds the server module"
    );
    drop(app);

    // `enabled: false`: no check at all, as under Vite.
    let Some(app) = app_with(
        "ip-off",
        &files,
        Some("importProtection: { enabled: false }"),
    ) else {
        return;
    };
    let (ok, text) = build(&app.0);
    assert!(ok, "the unprotected build failed:\n{text}");
    assert!(!text.contains("[import-protection]"), "{text}");
}

/// A page's hydration, its secret's text, its console and its errors.
async fn visit(server: &Server) -> (bool, Option<String>, Vec<String>, Vec<String>) {
    let page = Page::open(&server.url("/about")).await;
    let hydrated = page
        .poll(
            r##"document.querySelector("#hydrated")?.textContent === "hydrated""##,
            Duration::from_secs(30),
        )
        .await;
    let secret: Option<String> = page
        .eval("document.querySelector('#secret')?.textContent ?? null")
        .await;
    let (console, errors) = (page.console(), page.errors());
    page.close().await;
    (hydrated, secret, console, errors)
}

#[tokio::test(flavor = "multi_thread")]
async fn dev_mocks_a_server_only_import_and_recovers_once_it_is_gone() {
    let clean = about("", "const extra = null;");
    let Some(app) = app_with("ip-dev", &[("src/routes/about.tsx", clean.clone())], None) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root: &Path = &app.0;
    std::fs::write(root.join("src/server/secret.server.ts"), SECRET).unwrap();
    let route = root.join("src/routes/about.tsx");
    let dev = Server::oj_dev(root);
    let (hydrated, _, _, errors) = visit(&dev).await;
    assert!(
        hydrated,
        "the clean page did not hydrate: {errors:?}\n{}",
        dev.output()
    );

    std::fs::write(
        &route,
        about(
            "import { secret } from \"../server/secret.server\";",
            USES_SECRET,
        ),
    )
    .unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;
    let (_, secret, console, _) = visit(&dev).await;
    assert_ne!(
        secret.as_deref(),
        Some("server-secret"),
        "the page ran the server module\n{}",
        dev.output()
    );
    assert!(
        console
            .iter()
            .any(|c| c.contains("[import-protection] Mocked import used in dev client")),
        "the mock's use was not logged: {console:?}"
    );

    std::fs::write(&route, clean).unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;
    let (hydrated, _, _, errors) = visit(&dev).await;
    assert!(
        hydrated,
        "the page did not hydrate once the import was gone: {errors:?}\n{}",
        dev.output()
    );
    assert_eq!(
        errors,
        Vec::<String>::new(),
        "page errors once the import was gone"
    );
}

/// A middleware's server half, which calls into `@tanstack/react-start/server`,
/// compiles out of the client (Start's handleCreateMiddleware), so the client
/// neither ships it nor trips import protection.
#[test]
fn a_middlewares_server_half_stays_out_of_the_client() {
    let files = [
        (
            "src/server/auth.ts",
            "import { createMiddleware } from \"@tanstack/react-start\";\n\
             import { getRequestHeader } from \"@tanstack/react-start/server\";\n\
             export const authed = createMiddleware({ type: \"function\" }).server(({ next }) =>\n\
             \x20 next({ context: { who: getRequestHeader(\"x-who\") ?? \"server-secret\" } }),\n);\n"
                .to_string(),
        ),
        (
            "src/server/guarded.functions.ts",
            "import { createServerFn } from \"@tanstack/react-start\";\n\
             import { authed } from \"./auth\";\n\
             export const guarded = createServerFn({ method: \"POST\" })\n\
             \x20 .middleware([authed])\n\
             \x20 .handler(async ({ context }) => context.who);\n"
                .to_string(),
        ),
        (
            "src/routes/about.tsx",
            about(
                "import { guarded } from \"../server/guarded.functions\";",
                "const extra = <button onClick={() => guarded()}>go</button>;",
            ),
        ),
    ];
    let Some(app) = app_with("ip-middleware", &files, None) else {
        return;
    };
    let (ok, text) = build(&app.0);
    assert!(ok, "the build failed:\n{text}");
    assert!(!text.contains("[import-protection]"), "{text}");
    assert!(
        !client_has(&app.0, "server-secret"),
        "the client holds the middleware's server half"
    );
}

/// A `server-only` marker in dev: once Start's plugin knows the marked file,
/// the importer's next compile gets the mock; once the marker is gone, the
/// client gets the module itself again (the plugin forgets a changed file's
/// marker).
#[tokio::test(flavor = "multi_thread")]
async fn dev_serves_a_module_again_once_its_server_only_marker_is_gone() {
    let marked = format!("import \"@tanstack/react-start/server-only\";\n{SECRET}");
    let route = |n: u32| {
        about(
            &format!("import {{ secret }} from \"../lib/secret\"; // {n}"),
            USES_SECRET,
        )
    };
    let files = [
        ("src/lib/secret.ts", marked),
        ("src/routes/about.tsx", route(0)),
    ];
    let Some(app) = app_with("ip-dev-marker", &files, None) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let dev = Server::oj_dev(&app.0);
    // The first load resolves the importer before the marked file's own
    // imports reveal the marker, as in Vite; the importer's next compile
    // gets the mock.
    let _ = visit(&dev).await;
    std::fs::write(app.0.join("src/routes/about.tsx"), route(1)).unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;
    let (_, mocked, console, _) = visit(&dev).await;
    assert_ne!(
        mocked.as_deref(),
        Some("server-secret"),
        "the marked module ran in the page"
    );
    assert!(
        console
            .iter()
            .any(|c| c.contains("[import-protection] Mocked import used in dev client")),
        "the page did not use the mock: {console:?}"
    );

    std::fs::write(app.0.join("src/lib/secret.ts"), SECRET).unwrap();
    std::fs::write(app.0.join("src/routes/about.tsx"), route(2)).unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;
    let (hydrated, secret, _, errors) = visit(&dev).await;
    assert_eq!(
        secret.as_deref(),
        Some("server-secret"),
        "the unmarked module is still mocked; errors: {errors:?}\n{}",
        dev.output()
    );
    assert!(hydrated, "the page did not hydrate: {errors:?}");
}
