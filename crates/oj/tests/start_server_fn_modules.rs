// SPDX-License-Identifier: MIT

//! A TanStack Start server function in a `.js` module, in a module under
//! `tanstackStart({ srcDirectory })`, or in a directory outside `src/` that
//! an alias or a tsconfig `paths` entry names, can be called in dev, and from
//! the built server. Start's compiler finds server
//! functions in any module an environment loads, and its dev resolver loads
//! an id's module in the server environment on demand
//! (`environment.transformRequest`), so under `vite dev` such a call answers,
//! after an edit to the module and for a module created while it runs.

#[allow(dead_code)]
mod start_app;

use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant};

use start_app::{app_from, build, Server};

const ROOT: &str = r#"import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";

export const Route = createRootRoute({
  component: () => (
    <html lang="en">
      <head>
        <HeadContent />
      </head>
      <body>
        <Outlet />
        <Scripts />
      </body>
    </html>
  ),
});
"#;

const ROUTER: &str = r#"import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
  return createRouter({ routeTree });
}
"#;

const PACKAGE: &str = r#"{ "name": "server-fn-modules", "private": true, "type": "module",
  "dependencies": { "@tanstack/react-router": "*", "@tanstack/react-start": "*", "react": "*", "react-dom": "*" },
  "devDependencies": { "@vitejs/plugin-react": "*", "vite": "*" } }
"#;

const TSCONFIG: &str = r#"{ "compilerOptions": { "jsx": "react-jsx", "module": "ESNext", "moduleResolution": "Bundler", "target": "ES2022", "strict": true, "skipLibCheck": true, "allowJs": true } }
"#;

fn config(start_options: &str) -> String {
    format!(
        r#"import {{ tanstackStart }} from "@tanstack/react-start/plugin/vite";
import react from "@vitejs/plugin-react";
import {{ defineConfig }} from "vite";

export default defineConfig({{ plugins: [tanstackStart({start_options}), react()] }});
"#
    )
}

/// A functions module whose one function answers `answer`.
fn functions(name: &str, answer: &str) -> String {
    format!(
        r#"import {{ createServerFn }} from "@tanstack/react-start";

export const {name} = createServerFn({{ method: "POST" }}).handler(async () => "{answer}");
"#
    )
}

/// A component calling `name` from `module` on a click, never while it
/// renders.
fn component(name: &str, module: &str) -> String {
    format!(
        r#"import {{ useState }} from "react";
import {{ {name} }} from "{module}";

export default function Panel() {{
  const [out, setOut] = useState("pending");
  return <button onClick={{() => {name}().then(setOut)}}>{{out}}</button>;
}}
"#
    )
}

/// The index route, reaching the component through `React.lazy` alone.
const INDEX: &str = r#"import { createFileRoute } from "@tanstack/react-router";
import { lazy, Suspense } from "react";

const Panel = lazy(() => import("../components/Panel"));

export const Route = createFileRoute("/")({ component: Page });

function Page() {
  return (
    <main>
      <h1>home-marker</h1>
      <Suspense fallback={<p>loading</p>}>
        <Panel />
      </Suspense>
    </main>
  );
}
"#;

/// Calls the function `name` of `rel` until it answers `want`, within
/// `within`: a module edited or created while the server runs reaches the
/// resolver after the batch settles.
fn answers(dev: &Server, rel: &str, name: &str, want: &str, within: Duration) {
    let id = oj_compiler::server_fns::server_fn_id(rel, name);
    let deadline = Instant::now() + within;
    loop {
        let (status, body) = dev.call(&id);
        if status == 200 && body.contains(want) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{rel}#{name} answered {status} {body:?}, want {want:?}\n{}",
            dev.output()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The scripts the page loads and every chunk they import, transitively.
fn client_code(dev: &Server) -> String {
    let (status, html) = dev.request("GET", "/", &[]);
    assert_eq!(status, 200, "/\n{}", dev.output());
    let mut queue: Vec<String> = html
        .split('"')
        .filter(|s| s.starts_with("/@oj-start/") && s.ends_with(".js"))
        .map(str::to_string)
        .collect();
    assert!(!queue.is_empty(), "the page loads no client script\n{html}");
    let mut seen = HashSet::new();
    let mut all = String::new();
    while let Some(path) = queue.pop() {
        if !seen.insert(path.clone()) {
            continue;
        }
        let (status, code) = dev.request("GET", &path, &[]);
        assert_eq!(status, 200, "{path}\n{}", dev.output());
        let dir = &path[..path.rfind('/').unwrap_or(0)];
        for name in code
            .split('"')
            .filter(|s| s.starts_with("./") && s.ends_with(".js"))
        {
            queue.push(format!("{dir}/{}", &name[2..]));
        }
        all.push_str(&code);
    }
    all
}

/// The browser's stub for the function `name` of `rel` sends the id the
/// resolver maps.
fn stub_sends(dev: &Server, rel: &str, name: &str) {
    let id = oj_compiler::server_fns::server_fn_id(rel, name);
    let code = client_code(dev);
    assert!(
        code.contains(&format!("createClientRpc(\"{id}\")")),
        "no client stub sends {id} ({rel}#{name})\n{}",
        dev.output()
    );
}

/// The page renders, which bundles the client with its lazy chunk.
fn renders(dev: &Server) {
    let (status, body) = dev.request("GET", "/", &[]);
    assert!(
        status == 200 && body.contains("home-marker"),
        "/ answered {status}\n{}",
        dev.output()
    );
}

const SETTLE: Duration = Duration::from_secs(45);

#[test]
fn a_js_functions_module_a_lazy_component_imports_can_be_called() {
    let Some(app) = app_from(
        "server-fn-js-module",
        &[
            ("package.json", PACKAGE.to_string()),
            ("tsconfig.json", TSCONFIG.to_string()),
            ("vite.config.ts", config("")),
            ("src/router.tsx", ROUTER.to_string()),
            ("src/routes/__root.tsx", ROOT.to_string()),
            ("src/routes/index.tsx", INDEX.to_string()),
            (
                "src/components/Panel.jsx",
                component("listNotes", "../lib/notes.functions"),
            ),
            (
                "src/lib/notes.functions.js",
                functions("listNotes", "notes-first"),
            ),
        ],
    ) else {
        return;
    };
    let root: &Path = &app.0;
    let dev = Server::oj_dev(root);
    renders(&dev);
    stub_sends(&dev, "src/lib/notes.functions.js", "listNotes");
    answers(
        &dev,
        "src/lib/notes.functions.js",
        "listNotes",
        "notes-first",
        Duration::ZERO,
    );

    // Edited: the server runs the new handler.
    std::fs::write(
        root.join("src/lib/notes.functions.js"),
        functions("listNotes", "notes-second"),
    )
    .unwrap();
    answers(
        &dev,
        "src/lib/notes.functions.js",
        "listNotes",
        "notes-second",
        SETTLE,
    );

    // Created while the server runs.
    std::fs::write(
        root.join("src/lib/later.functions.js"),
        functions("later", "later-answer"),
    )
    .unwrap();
    answers(
        &dev,
        "src/lib/later.functions.js",
        "later",
        "later-answer",
        SETTLE,
    );
    drop(dev);

    // A restart over the warm cache keeps both.
    let dev = Server::oj_dev(root);
    answers(
        &dev,
        "src/lib/later.functions.js",
        "later",
        "later-answer",
        Duration::ZERO,
    );
    answers(
        &dev,
        "src/lib/notes.functions.js",
        "listNotes",
        "notes-second",
        Duration::ZERO,
    );
}

#[test]
fn a_functions_module_under_starts_src_directory_can_be_called() {
    let Some(app) = app_from(
        "server-fn-src-directory",
        &[
            ("package.json", PACKAGE.to_string()),
            ("tsconfig.json", TSCONFIG.to_string()),
            ("vite.config.ts", config(r#"{ srcDirectory: "app" }"#)),
            ("app/router.tsx", ROUTER.to_string()),
            ("app/routes/__root.tsx", ROOT.to_string()),
            ("app/routes/index.tsx", INDEX.to_string()),
            (
                "app/components/Panel.tsx",
                component("account", "../lib/account.functions"),
            ),
            (
                "app/lib/account.functions.ts",
                functions("account", "account-answer"),
            ),
        ],
    ) else {
        return;
    };
    let root: &Path = &app.0;
    let dev = Server::oj_dev(root);
    renders(&dev);
    answers(
        &dev,
        "app/lib/account.functions.ts",
        "account",
        "account-answer",
        Duration::ZERO,
    );
    std::fs::write(
        root.join("app/lib/later.functions.ts"),
        functions("later", "later-answer"),
    )
    .unwrap();
    answers(
        &dev,
        "app/lib/later.functions.ts",
        "later",
        "later-answer",
        SETTLE,
    );
}

/// An app whose component imports `name` from `shared/<file>` through
/// `@shared`, which `resolve` (Vite config) makes reachable.
fn shared_app(
    label: &str,
    resolve: &str,
    tsconfig: &str,
    file: &str,
    name: &str,
) -> Option<start_app::App> {
    let config = format!(
        r#"import {{ fileURLToPath }} from "node:url";
import {{ tanstackStart }} from "@tanstack/react-start/plugin/vite";
import react from "@vitejs/plugin-react";
import {{ defineConfig }} from "vite";

export default defineConfig({{
  resolve: {resolve},
  plugins: [tanstackStart(), react()],
}});
"#
    );
    let module = format!("shared/{file}.ts");
    app_from(
        label,
        &[
            ("package.json", PACKAGE.to_string()),
            ("tsconfig.json", tsconfig.to_string()),
            ("vite.config.ts", config),
            ("src/router.tsx", ROUTER.to_string()),
            ("src/routes/__root.tsx", ROOT.to_string()),
            ("src/routes/index.tsx", INDEX.to_string()),
            (
                "src/components/Panel.tsx",
                component(name, &format!("@shared/{file}")),
            ),
            (&module, functions(name, &format!("{name}-answer"))),
        ],
    )
}

const ALIAS: &str =
    r#"{ alias: { "@shared": fileURLToPath(new URL("./shared", import.meta.url)) } }"#;

#[test]
fn a_functions_module_in_a_directory_an_alias_names_can_be_called() {
    let Some(app) = shared_app(
        "server-fn-aliased-directory",
        ALIAS,
        TSCONFIG,
        "account.functions",
        "account",
    ) else {
        return;
    };
    let root: &Path = &app.0;
    let dev = Server::oj_dev(root);
    renders(&dev);
    stub_sends(&dev, "shared/account.functions.ts", "account");
    answers(
        &dev,
        "shared/account.functions.ts",
        "account",
        "account-answer",
        Duration::ZERO,
    );
}

#[test]
fn a_functions_module_in_a_directory_tsconfig_paths_names_can_be_called() {
    let tsconfig = r#"{ "compilerOptions": { "jsx": "react-jsx", "module": "ESNext", "moduleResolution": "Bundler", "target": "ES2022", "strict": true, "skipLibCheck": true, "paths": { "@shared/*": ["./shared/*"] } } }
"#;
    let Some(app) = shared_app(
        "server-fn-paths-directory",
        "{ tsconfigPaths: true }",
        tsconfig,
        "notes.functions",
        "notes",
    ) else {
        return;
    };
    let root: &Path = &app.0;
    let dev = Server::oj_dev(root);
    renders(&dev);
    stub_sends(&dev, "shared/notes.functions.ts", "notes");
    answers(
        &dev,
        "shared/notes.functions.ts",
        "notes",
        "notes-answer",
        Duration::ZERO,
    );

    // Created while the server runs.
    std::fs::write(
        root.join("shared/later.functions.ts"),
        functions("later", "later-answer"),
    )
    .unwrap();
    answers(
        &dev,
        "shared/later.functions.ts",
        "later",
        "later-answer",
        SETTLE,
    );
    drop(dev);

    // A restart over the warm cache keeps both.
    let dev = Server::oj_dev(root);
    answers(
        &dev,
        "shared/later.functions.ts",
        "later",
        "later-answer",
        Duration::ZERO,
    );
    answers(
        &dev,
        "shared/notes.functions.ts",
        "notes",
        "notes-answer",
        Duration::ZERO,
    );
}

#[test]
fn a_functions_module_in_an_aliased_directory_can_be_called_from_the_built_server() {
    let Some(app) = shared_app(
        "server-fn-aliased-build",
        ALIAS,
        TSCONFIG,
        "x.functions",
        "x",
    ) else {
        return;
    };
    let root: &Path = &app.0;
    build(root);
    let built = Server::node_server(root);
    renders(&built);
    answers(
        &built,
        "shared/x.functions.ts",
        "x",
        "x-answer",
        Duration::ZERO,
    );
}
