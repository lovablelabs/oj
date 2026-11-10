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

/// A `*.functions.ts` module outside the routes (so the router's code
/// splitter never sees it) whose only use of `@tanstack/react-start/server`
/// is inside a server function's handler. Start's compiler drops the handler
/// on the client and, with its dead-code pass, the import it used: the client
/// never imports the server entry, the build passes, and the dev page
/// hydrates without an import-protection error.
const HANDLER_ONLY_FNS: &str = r#"import { createServerFn } from "@tanstack/react-start";
import { getRequest } from "@tanstack/react-start/server";

export const whoAsks = createServerFn({ method: "POST" }).handler(async () => {
  return getRequest()?.method ?? "none";
});
"#;

fn handler_only_files() -> [(&'static str, String); 2] {
    [
        ("src/lib/who.functions.ts", HANDLER_ONLY_FNS.to_string()),
        (
            "src/routes/about.tsx",
            about(
                "import { whoAsks } from \"../lib/who.functions\";",
                "const extra = <button onClick={() => whoAsks()}>ask</button>;",
            ),
        ),
    ]
}

#[test]
fn a_functions_modules_server_import_only_its_handler_uses_builds() {
    let Some(app) = app_with("ip-fns-module", &handler_only_files(), None) else {
        return;
    };
    let (ok, text) = build(&app.0);
    assert!(ok, "the build failed:\n{text}");
    assert!(!text.contains("[import-protection]"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn dev_hydrates_a_page_whose_functions_module_imports_the_server_entry() {
    let Some(app) = app_with("ip-fns-module-dev", &handler_only_files(), None) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let dev = Server::oj_dev(&app.0);
    let (hydrated, _, console, errors) = visit(&dev).await;
    assert!(
        hydrated,
        "the page did not hydrate: {errors:?} {console:?}\n{}",
        dev.output()
    );
    assert!(
        !dev.output().contains("[import-protection]")
            && !console.iter().any(|c| c.contains("[import-protection]")),
        "import protection flagged a handler-only import: {console:?}\n{}",
        dev.output()
    );
}

/// The same, with the server module loaded by `await import()` inside the
/// handler, and that module importing `@tanstack/react-start/server`. Start's
/// compiler drops the handler, and the dynamic import with it, on the client.
const DYNAMIC_SERVER: &str = r#"import { getRequest } from "@tanstack/react-start/server";

export const inspect = () => getRequest()?.method ?? "dynamic-server-secret";
"#;

const DYNAMIC_FNS: &str = r#"import { createServerFn } from "@tanstack/react-start";

export const inspectFn = createServerFn({ method: "POST" }).handler(async () => {
  const { inspect } = await import("./inspect.server");
  return inspect();
});
"#;

fn dynamic_handler_files() -> [(&'static str, String); 3] {
    [
        ("src/lib/inspect.server.ts", DYNAMIC_SERVER.to_string()),
        ("src/lib/inspect.functions.ts", DYNAMIC_FNS.to_string()),
        (
            "src/routes/about.tsx",
            about(
                "import { inspectFn } from \"../lib/inspect.functions\";",
                "const extra = <button onClick={() => inspectFn()}>inspect</button>;",
            ),
        ),
    ]
}

#[test]
fn a_server_module_a_handler_imports_dynamically_builds() {
    let Some(app) = app_with("ip-dynamic", &dynamic_handler_files(), None) else {
        return;
    };
    let (ok, text) = build(&app.0);
    assert!(ok, "the build failed:\n{text}");
    assert!(!text.contains("[import-protection]"), "{text}");
    assert!(
        !client_has(&app.0, "dynamic-server-secret"),
        "the client holds the server module"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dev_hydrates_a_page_whose_handler_imports_a_server_module_dynamically() {
    let Some(app) = app_with("ip-dynamic-dev", &dynamic_handler_files(), None) else {
        return;
    };
    let dev = Server::oj_dev(&app.0);
    let (status, _) = dev.request("GET", "/@oj-start/client-entry.js", &[]);
    assert_eq!(status, 200, "the client entry\n{}", dev.output());
    assert!(
        !dev.output().contains("[import-protection]"),
        "import protection flagged a handler-only dynamic import\n{}",
        dev.output()
    );
    if browser::skip() {
        return;
    }
    let (hydrated, _, console, errors) = visit(&dev).await;
    assert!(
        hydrated,
        "the page did not hydrate: {errors:?} {console:?}\n{}",
        dev.output()
    );
    assert!(
        !console.iter().any(|c| c.contains("[import-protection]")),
        "import protection flagged a handler-only dynamic import: {console:?}"
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
        "the marked module ran in the page\n{}",
        dev.output()
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

/// A server-only module imported at the top of a `*.functions.ts` module
/// and used only inside a server function's handler, where the server-only
/// module itself imports `@tanstack/react-start/server`.
const DB_SERVER: &str = r#"import { getRequest } from "@tanstack/react-start/server";

export const db = { method: () => getRequest()?.method ?? "db-server-secret" };
"#;

fn data_fns(n: u32) -> String {
    format!(
        r#"import {{ createServerFn }} from "@tanstack/react-start";
import {{ db }} from "./db.server";

// edit {n}
export const readMethod = createServerFn({{ method: "POST" }})
  .inputValidator((input: {{ who: string }}) => input)
  .handler(async ({{ data }}) => {{
    return `${{data.who}}:${{db.method()}}`;
  }});
"#
    )
}

fn top_level_server_files() -> [(&'static str, String); 3] {
    [
        ("src/lib/db.server.ts", DB_SERVER.to_string()),
        ("src/lib/data.functions.ts", data_fns(0)),
        (
            "src/routes/about.tsx",
            about(
                "import { readMethod } from \"../lib/data.functions\";",
                "const extra = <button onClick={() => readMethod({ data: { who: \"me\" } })}>read</button>;",
            ),
        ),
    ]
}

/// The client bundle after an edit to the functions module: Start's compiler
/// drops the handler and the server-only import with it on the client, so the
/// rebuild must pass import protection as the first bundle did.
fn rebuild_keeps_handler_only_imports(name: &str, env: &[(&str, &str)]) {
    let Some(app) = app_with(
        name,
        &top_level_server_files(),
        Some("importProtection: { behavior: \"error\" }"),
    ) else {
        return;
    };
    let dev = Server::oj_dev_with(&app.0, env);
    let (status, _) = dev.request("GET", "/@oj-start/client-entry.js", &[]);
    assert_eq!(status, 200, "the client entry\n{}", dev.output());
    assert!(
        !dev.output().contains("[import-protection]"),
        "the first bundle flagged a handler-only import\n{}",
        dev.output()
    );
    for n in 1..=4 {
        match n {
            1 | 4 => std::fs::write(app.0.join("src/lib/data.functions.ts"), data_fns(n)).unwrap(),
            2 => {
                let route = app.0.join("src/routes/about.tsx");
                let text = std::fs::read_to_string(&route).unwrap();
                std::fs::write(&route, format!("{text}\n// edit {n}\n")).unwrap();
            }
            _ => {
                let server = app.0.join("src/lib/db.server.ts");
                std::fs::write(&server, format!("{DB_SERVER}// edit {n}\n")).unwrap();
            }
        }
        std::thread::sleep(Duration::from_secs(4));
        let (status, _) = dev.request("GET", "/@oj-start/client-entry.js", &[]);
        assert_eq!(
            status,
            200,
            "the client entry after edit {n}\n{}",
            dev.output()
        );
        let output = dev.output();
        assert!(
            !output.contains("[import-protection]") && !output.contains("client bundle failed"),
            "rebuild {n} flagged a handler-only import\n{output}"
        );
    }
}

#[test]
fn a_rebuild_keeps_a_functions_modules_handler_only_server_import() {
    rebuild_keeps_handler_only_imports("ip-fns-rebuild", &[]);
}

#[test]
fn a_rebuild_behind_a_granular_gate_keeps_a_handler_only_server_import() {
    rebuild_keeps_handler_only_imports(
        "ip-fns-rebuild-gate",
        &[("OJ_HMR_GATE", "1"), ("OJ_HMR_FULL_RELOAD", "false")],
    );
}

#[test]
fn a_rebuild_behind_a_full_reload_gate_keeps_a_handler_only_server_import() {
    rebuild_keeps_handler_only_imports(
        "ip-fns-rebuild-reload",
        &[("OJ_HMR_GATE", "1"), ("OJ_HMR_FULL_RELOAD", "true")],
    );
}

/// A file-routed app (the router generator writes `routeTree.gen.ts`), as
/// Start's own template is laid out: the root route imports a functions
/// module whose server function takes a middleware from a `*.server.ts`
/// module and reads a second one only inside its handler; both server modules
/// import `@tanstack/react-start/server`. Import protection is set to its
/// strict shape: errors, and a directory pattern in place of the default file
/// patterns (so `*.server.ts` is not denied by name, only what it imports).
fn file_routed_app(name: &str) -> Option<start_app::App> {
    let files: &[(&str, String)] = &[
        (
            "package.json",
            r#"{ "name": "file-routed", "private": true, "type": "module", "dependencies": { "@tanstack/react-start": "*", "@tanstack/react-router": "*", "react": "*", "react-dom": "*" } }"#.to_string(),
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
  plugins: [
    tanstackStart({
      importProtection: {
        behavior: "error",
        client: { files: ["**/server/**"], specifiers: ["server-only"] },
      },
    }),
    react(),
  ],
});
"#
            .to_string(),
        ),
        (
            "src/router.tsx",
            r#"import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
  return createRouter({ routeTree, scrollRestoration: true });
}
"#
            .to_string(),
        ),
        (
            "src/routes/__root.tsx",
            root_route(0),
        ),
        (
            "src/routes/index.tsx",
            r#"import { createFileRoute } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { readMethod } from "../lib/data.functions";

export const Route = createFileRoute("/")({ component: Home });

function Home() {
  const [state, setState] = useState("ssr");
  useEffect(() => setState("hydrated"), []);
  return (
    <main>
      <p id="hydrated">{state}</p>
      <button onClick={() => readMethod({ data: { who: "me" } })}>read</button>
    </main>
  );
}
"#
            .to_string(),
        ),
        ("src/lib/data.functions.ts", guarded_fns(0)),
        ("src/lib/account.functions.ts", account_fns(0)),
        ("src/lib/auth.server.ts", auth_server(0)),
        ("src/lib/session.server.ts", session_server(0)),
    ];
    start_app::app_from(name, files)
}

fn root_route(n: u32) -> String {
    format!(
        r#"import {{ createRootRoute, HeadContent, Outlet, Scripts }} from "@tanstack/react-router";
import {{ whoAmI }} from "../lib/account.functions";

// edit {n}
export const Route = createRootRoute({{
  beforeLoad: async () => ({{ who: await whoAmI() }}),
  component: RootComponent,
}});

function RootComponent() {{
  return (
    <html lang="en">
      <head>
        <HeadContent />
      </head>
      <body>
        <Outlet />
        <Scripts />
      </body>
    </html>
  );
}}
"#
    )
}

fn guarded_fns(n: u32) -> String {
    format!(
        r#"import {{ createServerFn }} from "@tanstack/react-start";
import {{ requireAuth }} from "./auth.server";
import {{ db }} from "./session.server";

// edit {n}
export const readMethod = createServerFn({{ method: "POST" }})
  .middleware([requireAuth])
  .inputValidator((input: {{ who: string }}) => input)
  .handler(async ({{ data, context }}) => {{
    return `${{data.who}}:${{context.user}}:${{db.method()}}`;
  }});
"#
    )
}

fn account_fns(n: u32) -> String {
    format!(
        r#"import {{ createServerFn }} from "@tanstack/react-start";
import {{ currentUser }} from "./session.server";

// edit {n}
export const whoAmI = createServerFn({{ method: "GET" }}).handler(async () => {{
  return currentUser();
}});
"#
    )
}

fn auth_server(n: u32) -> String {
    format!(
        r#"import {{ createMiddleware }} from "@tanstack/react-start";
import {{ getRequestHeader }} from "@tanstack/react-start/server";

// edit {n}
export const requireAuth = createMiddleware({{ type: "function" }}).server(({{ next }}) =>
  next({{ context: {{ user: getRequestHeader("x-user") ?? "nobody" }} }}),
);
"#
    )
}

fn session_server(n: u32) -> String {
    format!(
        r#"import {{ getRequest }} from "@tanstack/react-start/server";

// edit {n}
export const db = {{ method: () => getRequest()?.method ?? "session-server-secret" }};
export const currentUser = () => getRequest()?.headers.get("x-user") ?? "anonymous";
"#
    )
}

fn assert_no_denial(dev: &Server, when: &str) {
    let output = dev.output();
    assert!(
        !output.contains("[import-protection]") && !output.contains("client bundle failed"),
        "{when}: import protection denied a server import the client never uses\n{output}"
    );
}

/// Every edit of the chain (the functions module, the root route importing
/// it, a server-only module) rebuilds the client without a denial, with a
/// page connected (so the engine's hot path runs) and without one.
async fn file_routed_rebuilds(name: &str, env: &[(&str, &str)], with_page: bool) {
    let Some(app) = file_routed_app(name) else {
        return;
    };
    if with_page && browser::skip() {
        return;
    }
    let dev = Server::oj_dev_with(&app.0, env);
    let (status, _) = dev.request("GET", "/@oj-start/client-entry.js", &[]);
    assert_eq!(status, 200, "the client entry\n{}", dev.output());
    assert_no_denial(&dev, "the first bundle");
    let page = if with_page {
        let page = Page::open(&dev.url("/")).await;
        assert!(
            page.poll(
                r##"document.querySelector("#hydrated")?.textContent === "hydrated""##,
                Duration::from_secs(30),
            )
            .await,
            "the page did not hydrate: {:?}\n{}",
            page.errors(),
            dev.output()
        );
        Some(page)
    } else {
        None
    };
    type Edit = (&'static str, fn(u32) -> String);
    let edits: [Edit; 5] = [
        ("src/lib/data.functions.ts", guarded_fns),
        ("src/routes/__root.tsx", root_route),
        ("src/lib/auth.server.ts", auth_server),
        ("src/lib/session.server.ts", session_server),
        ("src/lib/account.functions.ts", account_fns),
    ];
    for (n, (rel, make)) in edits.iter().enumerate() {
        let n = n as u32 + 1;
        std::fs::write(app.0.join(rel), make(n)).unwrap();
        tokio::time::sleep(Duration::from_secs(4)).await;
        let (status, _) = dev.request("GET", "/@oj-start/client-entry.js", &[]);
        assert_eq!(
            status,
            200,
            "the client entry after editing {rel}\n{}",
            dev.output()
        );
        assert_no_denial(&dev, &format!("after editing {rel}"));
    }
    if let Some(page) = page {
        page.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_routed_app_rebuilds_without_denying_handler_only_server_imports() {
    file_routed_rebuilds("plain", &[], false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_routed_page_stays_hot_without_denying_handler_only_server_imports() {
    file_routed_rebuilds("hot", &[], true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_routed_page_behind_a_granular_gate_rebuilds_without_denials() {
    file_routed_rebuilds(
        "gate",
        &[("OJ_HMR_GATE", "1"), ("OJ_HMR_FULL_RELOAD", "false")],
        true,
    )
    .await;
}

/// Server functions declared in the shapes Start's compiler accepts beyond
/// `export const name = createServerFn()`: a type-annotated `const` through
/// a renamed import, a `let` through a namespace import, and a chain through
/// a variable holding the factory. Each handler loads a `*.server.ts` module
/// with `await import()`, so a handler left in the client is denied there.
const SHAPED_FNS: &str = r#"import { createServerFn as serverFn } from "@tanstack/react-start";
import * as start from "@tanstack/react-start";

type Plan = () => Promise<string>;

export const plan: Plan = serverFn({ method: "POST" }).handler(async () => {
  const { gate } = await import("./ai-gate.server");
  return `plan-handler:${gate()}`;
});

export let audit = start.createServerFn({ method: "POST" }).handler(async () => {
  const { gate } = await import("./ai-gate.server");
  return `audit-handler:${gate()}`;
});

const base = serverFn({ method: "POST" });
export const summarize = base.handler(async () => {
  const { gate } = await import("./ai-gate.server");
  return `summarize-handler:${gate()}`;
});
"#;

const SHAPED_NAMES: [&str; 3] = ["plan", "audit", "summarize"];

/// The server-only module the handlers load, importing the server entry.
const GATE_SERVER: &str = r#"import { getRequestHeader } from "@tanstack/react-start/server";

export const gate = () => getRequestHeader("x-gate") ?? "ai-gate-secret";
"#;

fn shaped_fn_files() -> [(&'static str, String); 3] {
    [
        ("src/lib/ai-gate.server.ts", GATE_SERVER.to_string()),
        ("src/lib/plan.functions.ts", SHAPED_FNS.to_string()),
        (
            "src/routes/about.tsx",
            about(
                "import { plan, audit, summarize } from \"../lib/plan.functions\";",
                r#"const [result, setResult] = useState("");
  useEffect(() => {
    Promise.all([plan(), audit(), summarize()]).then(
      (r) => setResult(r.join("|")),
      (e) => setResult("error " + String(e)),
    );
  }, []);
  const extra = <p id="secret">{result}</p>;"#,
            ),
        ),
    ]
}

#[test]
fn server_fns_declared_in_other_shapes_keep_their_handlers_out_of_the_client() {
    let Some(app) = app_with("sf-shapes", &shaped_fn_files(), None) else {
        return;
    };
    let (ok, text) = build(&app.0);
    assert!(ok, "the build failed:\n{text}");
    assert!(!text.contains("[import-protection]"), "{text}");
    assert!(
        !client_has(&app.0, "ai-gate-secret"),
        "the client holds the server module"
    );
    for name in SHAPED_NAMES {
        assert!(
            !client_has(&app.0, &format!("{name}-handler:")),
            "the client holds the handler of {name}"
        );
        assert!(
            client_has(
                &app.0,
                &oj_compiler::server_fns::server_fn_id("src/lib/plan.functions.ts", name)
            ),
            "the client does not call {name} by its id"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn dev_hydrates_a_page_calling_server_fns_declared_in_other_shapes() {
    let Some(app) = app_with("sf-shapes-dev", &shaped_fn_files(), None) else {
        return;
    };
    let dev = Server::oj_dev(&app.0);
    let (status, _) = dev.request("GET", "/@oj-start/client-entry.js", &[]);
    assert_eq!(status, 200, "the client entry\n{}", dev.output());
    assert!(
        !dev.output().contains("[import-protection]"),
        "import protection flagged a server-only import\n{}",
        dev.output()
    );
    // The server side registered each one under its id, with its handler.
    for name in SHAPED_NAMES {
        let id = oj_compiler::server_fns::server_fn_id("src/lib/plan.functions.ts", name);
        let (status, body) = dev.call(&id);
        assert!(
            status == 200 && body.contains(&format!("{name}-handler:ai-gate-secret")),
            "{name} answered {status} {body:?}\n{}",
            dev.output()
        );
    }
    if browser::skip() {
        return;
    }
    let page = Page::open(&dev.url("/about")).await;
    let hydrated = page
        .poll(
            r##"document.querySelector("#hydrated")?.textContent === "hydrated""##,
            Duration::from_secs(30),
        )
        .await;
    let answered = page
        .poll(
            r##"document.querySelector("#secret")?.textContent === "plan-handler:ai-gate-secret|audit-handler:ai-gate-secret|summarize-handler:ai-gate-secret""##,
            Duration::from_secs(30),
        )
        .await;
    let (console, errors) = (page.console(), page.errors());
    let result: Option<String> = page
        .eval("document.querySelector('#secret')?.textContent ?? null")
        .await;
    page.close().await;
    assert!(
        hydrated,
        "the page did not hydrate: {errors:?} {console:?}\n{}",
        dev.output()
    );
    assert!(
        answered,
        "the page's calls answered {result:?}: {errors:?} {console:?}\n{}",
        dev.output()
    );
    assert!(
        !console.iter().any(|c| c.contains("[import-protection]")),
        "import protection flagged a server-only import: {console:?}"
    );
}
