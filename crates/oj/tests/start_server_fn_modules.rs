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

    // Created while the server runs, then imported by the component.
    std::fs::write(
        root.join("shared/later.functions.ts"),
        functions("later", "later-answer"),
    )
    .unwrap();
    std::fs::write(
        root.join("src/components/Panel.tsx"),
        component("later", "@shared/later.functions"),
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

    // A restart over the warm cache keeps it once the page loads it; the
    // module the component no longer imports is refused, as under Vite.
    let dev = Server::oj_dev(root);
    renders(&dev);
    answers(
        &dev,
        "shared/later.functions.ts",
        "later",
        "later-answer",
        Duration::ZERO,
    );
    let (status, _) = dev.call(&oj_compiler::server_fns::server_fn_id(
        "shared/notes.functions.ts",
        "notes",
    ));
    assert_ne!(status, 200);
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

/// Every file the built client holds, concatenated.
fn built_client_code(root: &Path) -> String {
    let mut all = String::new();
    let mut stack = vec![root.join("dist/client")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "js") {
                all.push_str(&std::fs::read_to_string(&path).unwrap());
            }
        }
    }
    all
}

/// A component importing `name` from `src/lib/<name>.functions.<ext>`: the
/// client calls it through its stub, never with the handler in the browser,
/// in dev and from the built server.
fn a_module_of_kind_is_stubbed_and_answers(ext: &str) {
    let name = format!("{ext}Fn");
    let rel = format!("src/lib/{name}.functions.{ext}");
    let answer = format!("{ext}-answer");
    let Some(app) = app_from(
        &format!("server-fn-{ext}-module"),
        &[
            ("package.json", PACKAGE.to_string()),
            ("tsconfig.json", TSCONFIG.to_string()),
            ("vite.config.ts", config("")),
            ("src/router.tsx", ROUTER.to_string()),
            ("src/routes/__root.tsx", ROOT.to_string()),
            ("src/routes/index.tsx", INDEX.to_string()),
            (
                "src/components/Panel.tsx",
                component(&name, &format!("../lib/{name}.functions.{ext}")),
            ),
            (&rel, functions(&name, &answer)),
        ],
    ) else {
        return;
    };
    let root: &Path = &app.0;
    let dev = Server::oj_dev(root);
    renders(&dev);
    stub_sends(&dev, &rel, &name);
    assert!(
        !client_code(&dev).contains(&answer),
        "the handler of {rel} reached the browser"
    );
    answers(&dev, &rel, &name, &answer, Duration::ZERO);
    drop(dev);

    build(root);
    let id = oj_compiler::server_fns::server_fn_id(&rel, &name);
    let client = built_client_code(root);
    assert!(
        client.contains(&id) && !client.contains(&answer),
        "the built client has no stub for {rel}, or its handler"
    );
    let built = Server::node_server(root);
    answers(&built, &rel, &name, &answer, Duration::ZERO);
}

#[test]
fn an_mts_functions_module_is_stubbed_on_the_client_and_answers() {
    a_module_of_kind_is_stubbed_and_answers("mts");
}

#[test]
fn a_cts_functions_module_is_stubbed_on_the_client_and_answers() {
    a_module_of_kind_is_stubbed_and_answers("cts");
}

/// The app's files, a component calling `name` from `module`.
fn app_files(name: &str, module: &str) -> Vec<(String, String)> {
    [
        ("package.json", PACKAGE.to_string()),
        ("tsconfig.json", TSCONFIG.to_string()),
        ("src/router.tsx", ROUTER.to_string()),
        ("src/routes/__root.tsx", ROOT.to_string()),
        ("src/routes/index.tsx", INDEX.to_string()),
        ("src/components/Panel.tsx", component(name, module)),
    ]
    .into_iter()
    .map(|(rel, body)| (rel.to_string(), body))
    .collect()
}

fn files_ref(files: &[(String, String)]) -> Vec<(&str, String)> {
    files
        .iter()
        .map(|(rel, body)| (rel.as_str(), body.clone()))
        .collect()
}

#[test]
fn a_functions_module_in_a_root_level_directory_nothing_names_answers() {
    let mut files = app_files("rootFn", "../../server/root.functions");
    files.push(("vite.config.ts".into(), config("")));
    files.push((
        "server/root.functions.ts".into(),
        functions("rootFn", "root-answer"),
    ));
    let Some(app) = app_from("server-fn-root-level", &files_ref(&files)) else {
        return;
    };
    let root: &Path = &app.0;
    let rel = "server/root.functions.ts";
    let dev = Server::oj_dev(root);
    renders(&dev);
    stub_sends(&dev, rel, "rootFn");
    answers(&dev, rel, "rootFn", "root-answer", Duration::ZERO);
    drop(dev);

    build(root);
    let built = Server::node_server(root);
    answers(&built, rel, "rootFn", "root-answer", Duration::ZERO);
}

/// A workspace: the app in `app/`, a `shared/` package beside it that an
/// alias names, and the install at the workspace's root.
#[test]
fn a_functions_module_outside_the_root_an_alias_names_answers() {
    let mut files: Vec<(String, String)> = app_files("outFn", "@shared/out.functions")
        .into_iter()
        .map(|(rel, body)| (format!("app/{rel}"), body))
        .collect();
    files.push((
        "app/vite.config.ts".into(),
        r#"import { fileURLToPath } from "node:url";
import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig({
  resolve: { alias: { "@shared": fileURLToPath(new URL("../shared", import.meta.url)) } },
  plugins: [tanstackStart(), react()],
});
"#
        .into(),
    ));
    files.push((
        "shared/out.functions.ts".into(),
        functions("outFn", "out-answer"),
    ));
    let Some(workspace) = app_from("server-fn-outside-root", &files_ref(&files)) else {
        return;
    };
    let root = workspace.0.join("app");
    let rel = workspace.0.join("shared/out.functions.ts");
    let rel = rel.to_str().unwrap();
    let dev = Server::oj_dev(&root);
    renders(&dev);
    stub_sends(&dev, rel, "outFn");
    answers(&dev, rel, "outFn", "out-answer", Duration::ZERO);
    // Start's own dev id for it, `/@fs/<abs>`.
    let start_id = oj_compiler::server_fns::vite_dev_server_fn_id(rel, "outFn");
    let (status, body) = dev.call(&start_id);
    assert!(
        status == 200 && body.contains("out-answer"),
        "{status} {body}"
    );
    drop(dev);

    build(&root);
    let built = Server::node_server(&root);
    answers(&built, rel, "outFn", "out-answer", Duration::ZERO);
}

/// Start's dev id for `file` and `export`, as its compiler encodes it.
fn start_dev_id(file: &str, export: &str) -> String {
    use base64::Engine;
    let json = serde_json::json!({ "file": file, "export": export }).to_string();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
}

/// An id no client stub sends is answered only when it is the id of a server
/// function its file declares, spelled as the file's real path, as Start's
/// `validate-server-fn-id` accepts it: never by running another module.
#[test]
fn a_forged_server_fn_id_is_refused_and_runs_no_module() {
    let mut files: Vec<(String, String)> = app_files("rootFn", "../../server/root.functions")
        .into_iter()
        .map(|(rel, body)| (format!("app/{rel}"), body))
        .collect();
    files.push(("app/vite.config.ts".into(), config("")));
    files.push((
        "app/server/root.functions.ts".into(),
        functions("rootFn", "root-answer"),
    ));
    files.push((
        "app/src/lib/plain.ts".into(),
        "console.log(\"plain-module-ran\");\nexport const x = () => \"plain\";\n".into(),
    ));
    files.push((
        "shared/out.functions.ts".into(),
        functions("outFn", "out-answer"),
    ));
    let Some(workspace) = app_from("server-fn-forged-id", &files_ref(&files)) else {
        return;
    };
    let root = workspace.0.join("app");
    let oj = oj_compiler::server_fns::server_fn_id;
    let in_root = root.join("server/root.functions.ts");
    let elsewhere_dir = workspace.0.with_extension("elsewhere");
    std::fs::create_dir_all(&elsewhere_dir).unwrap();
    let elsewhere = elsewhere_dir.join("x.functions.ts");
    for file in [
        &elsewhere,
        &root.join(".git/x.functions.ts"),
        &root.join("server/lone.functions.ts"),
    ] {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, functions("rootFn", "stray-answer")).unwrap();
    }
    let dev = Server::oj_dev(&root);
    renders(&dev);
    for id in [
        // A file that declares no such function, or none at all.
        oj("server/root.functions.ts", "other"),
        oj("src/lib/plain.ts", "x"),
        // Any spelling but the real path's.
        oj("src/../server/root.functions.ts", "rootFn"),
        oj("../shared/out.functions.ts", "outFn"),
        oj(in_root.to_str().unwrap(), "rootFn"),
        start_dev_id(
            &format!("/@fs{}?tss-serverfn-split", in_root.display()),
            "rootFn_createServerFn_handler",
        ),
        start_dev_id(
            "/../shared/out.functions.ts?tss-serverfn-split",
            "outFn_createServerFn_handler",
        ),
        // Start's dev id without its split query.
        start_dev_id("/server/root.functions.ts", "rootFn_createServerFn_handler"),
        // Modules that declare it and nothing imports: one outside the
        // workspace, in oj's form and Start's; one under `.git/`; one in the
        // app.
        oj(elsewhere.to_str().unwrap(), "rootFn"),
        start_dev_id(
            &format!("/@fs{}?tss-serverfn-split", elsewhere.display()),
            "rootFn_createServerFn_handler",
        ),
        oj(".git/x.functions.ts", "rootFn"),
        oj("server/lone.functions.ts", "rootFn"),
    ] {
        let (status, body) = dev.call(&id);
        assert!(
            status != 200 && !body.contains("-answer"),
            "{id} answered {status} {body}"
        );
    }
    assert!(
        !dev.output().contains("plain-module-ran"),
        "a forged id ran a module\n{}",
        dev.output()
    );

    // A lazily looked-up id answers, and again on the next call.
    let rel = "server/root.functions.ts";
    answers(&dev, rel, "rootFn", "root-answer", Duration::ZERO);
    answers(&dev, rel, "rootFn", "root-answer", Duration::ZERO);

    // A module created while the server runs is refused while nothing
    // imports it, as under Vite, and answers once the component does.
    let later = "server/later.functions.ts";
    std::fs::write(root.join(later), functions("later", "later-answer")).unwrap();
    std::thread::sleep(Duration::from_secs(2));
    let (status, _) = dev.call(&oj(later, "later"));
    assert_ne!(status, 200, "{later} answered while nothing imports it");
    std::fs::write(
        root.join("src/components/Panel.tsx"),
        component("later", "../../server/later.functions"),
    )
    .unwrap();
    answers(&dev, later, "later", "later-answer", SETTLE);

    // Its function taken out, then the file deleted: the lookup refuses it
    // again (the resolver reports no entry, not a failed import).
    std::fs::write(root.join(later), "export const later = 1;\n").unwrap();
    lookup_refuses(&dev, &oj(later, "later"), SETTLE);
    std::fs::remove_file(root.join(later)).unwrap();
    lookup_refuses(&dev, &oj(later, "later"), SETTLE);
    let _ = std::fs::remove_dir_all(&elsewhere_dir);
}

/// Within `within`, a call of `id` fails because the dev lookup refused it:
/// the resolver logs that it has no entry for `id`.
fn lookup_refuses(dev: &Server, id: &str, within: Duration) {
    let refused = format!("Server function info not found for {id}");
    let before = dev.output().matches(&refused).count();
    let deadline = Instant::now() + within;
    loop {
        let (status, _) = dev.call(id);
        if status != 200 && dev.output().matches(&refused).count() > before {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{id} was not refused by the lookup\n{}",
            dev.output()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// A module no import reaches is never read by the build, as `vite build`
/// never reads one: a `.cjs` written with `import` (which the bundler refuses
/// as CommonJS) or a functions module that does not parse.
#[test]
fn a_module_nothing_imports_never_breaks_the_build() {
    let mut files = app_files("used", "../lib/used.functions");
    files.push(("vite.config.ts".into(), config("")));
    files.push((
        "src/lib/used.functions.ts".into(),
        functions("used", "used-answer"),
    ));
    files.push((
        "src/lib/stray.functions.cjs".into(),
        functions("stray", "stray-answer"),
    ));
    files.push((
        "src/lib/broken.functions.ts".into(),
        "import { createServerFn } from \"@tanstack/react-start\";\nexport const broken = createServerFn().handler(async () => {\n".into(),
    ));
    let Some(app) = app_from("server-fn-unimported", &files_ref(&files)) else {
        return;
    };
    let root: &Path = &app.0;
    build(root);
    let built = Server::node_server(root);
    answers(
        &built,
        "src/lib/used.functions.ts",
        "used",
        "used-answer",
        Duration::ZERO,
    );
}

/// `src/lib/link.functions.ts` links to `real/real.functions.ts`. The id is
/// the real path's by default and the link's under
/// `resolve.preserveSymlinks`, as Start's is under Vite; both answer in dev
/// and from the built server.
#[cfg(unix)]
#[test]
fn a_symlinked_functions_module_answers_with_and_without_preserve_symlinks() {
    for (preserve, rel) in [
        (false, "real/real.functions.ts"),
        (true, "src/lib/link.functions.ts"),
    ] {
        let mut files = app_files("linked", "../lib/link.functions");
        files.push((
            "vite.config.ts".into(),
            format!(
                r#"import {{ tanstackStart }} from "@tanstack/react-start/plugin/vite";
import react from "@vitejs/plugin-react";
import {{ defineConfig }} from "vite";

export default defineConfig({{ resolve: {{ preserveSymlinks: {preserve} }}, plugins: [tanstackStart(), react()] }});
"#
            ),
        ));
        files.push((
            "real/real.functions.ts".into(),
            functions("linked", "linked-answer"),
        ));
        let Some(app) = app_from(&format!("server-fn-symlink-{preserve}"), &files_ref(&files))
        else {
            return;
        };
        let root: &Path = &app.0;
        std::fs::create_dir_all(root.join("src/lib")).unwrap();
        std::os::unix::fs::symlink(
            "../../real/real.functions.ts",
            root.join("src/lib/link.functions.ts"),
        )
        .unwrap();
        let dev = Server::oj_dev(root);
        renders(&dev);
        stub_sends(&dev, rel, "linked");
        answers(&dev, rel, "linked", "linked-answer", Duration::ZERO);
        drop(dev);

        build(root);
        let built = Server::node_server(root);
        answers(&built, rel, "linked", "linked-answer", Duration::ZERO);
    }
}
