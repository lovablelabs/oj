// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A tsconfig's `module` setting never changes what the dev server serves:
//! every module is ES syntax for the browser's `type="module"` script,
//! whatever `compilerOptions.module` says and wherever it says it. Vite's
//! transform reads a short allow-list of tsconfig fields
//! (`meaningfulFields` in plugins/esbuild.ts: `jsx*`, `target`,
//! `useDefineForClassFields`, `verbatimModuleSyntax`, decorators and the
//! import-elision flags) and `module` is not among them, so a project whose
//! root `tsconfig.json` (a `references`-only one, as scaffolds write it) or
//! `tsconfig.node.json` says `commonjs`, or whose nearest tsconfig over a
//! directory does, still gets ES modules. oj's compiler consults the nearest
//! tsconfig for the class-field semantics alone; this pins that `module`
//! stays out of it, since a page served `var x = require(...)` dies at once
//! with "require is not defined".

use crate::App;

const INDEX_HTML: &str = r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.tsx"></script></body></html>"#;

/// A stand-in `react` with the two JSX runtimes, so the automatic runtime's
/// import resolves without the real package.
fn fake_react(app: &App) {
    app.write(&[
        (
            "node_modules/react/package.json",
            r#"{"name":"react","version":"19.0.0","exports":{".":"./index.js","./jsx-runtime":"./jsx-runtime.js","./jsx-dev-runtime":"./jsx-dev-runtime.js"}}"#,
        ),
        ("node_modules/react/index.js", "export default {};\n"),
        (
            "node_modules/react/jsx-runtime.js",
            "export const Fragment = Symbol(\"f\");\nexport const jsx = (t, p) => ({ t, p });\nexport const jsxs = jsx;\n",
        ),
        (
            "node_modules/react/jsx-dev-runtime.js",
            "export const Fragment = Symbol(\"f\");\nexport const jsxDEV = (t, p) => ({ t, p });\n",
        ),
    ]);
}

#[tokio::test(flavor = "multi_thread")]
async fn tsconfig_module_commonjs_never_flips_served_code_to_commonjs() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"tsconfig-module","version":"1.0.0","type":"module"}"#,
        ),
        // The scaffold shape: a references-only root whose own options say
        // commonjs, an app tsconfig that says ESNext, and a node tsconfig
        // for the config file that says commonjs.
        (
            "tsconfig.json",
            r#"{"files":[],"references":[{"path":"./tsconfig.app.json"},{"path":"./tsconfig.node.json"}],"compilerOptions":{"module":"commonjs","jsx":"react-jsx","baseUrl":"."}}"#,
        ),
        (
            "tsconfig.app.json",
            r#"{"compilerOptions":{"target":"ES2020","module":"ESNext","moduleResolution":"bundler","jsx":"react-jsx","noEmit":true},"include":["src"]}"#,
        ),
        (
            "tsconfig.node.json",
            r#"{"compilerOptions":{"target":"ES2022","module":"commonjs","noEmit":true},"include":["vite.config.ts"]}"#,
        ),
        // A directory whose nearest tsconfig itself says commonjs.
        (
            "src/legacy/tsconfig.json",
            r#"{"compilerOptions":{"module":"commonjs","jsx":"react-jsx","target":"ES2020"}}"#,
        ),
        (
            "src/legacy/Badge.tsx",
            "export const Badge = () => <b>legacy</b>;\n",
        ),
        ("src/App.tsx", "export const App = () => <div>app</div>;\n"),
        (
            "src/main.tsx",
            "import { App } from \"./App\";\nimport { Badge } from \"./legacy/Badge\";\n\nconst root: HTMLElement | null = document.getElementById(\"root\");\nconsole.log(root, <App />, <Badge />);\n",
        ),
        ("index.html", INDEX_HTML),
    ]);
    fake_react(&app);
    let server = app.serve().await;
    for module in ["/src/main.tsx", "/src/App.tsx", "/src/legacy/Badge.tsx"] {
        let code = server.text(module).await;
        assert!(
            !code.contains("require(") && !code.contains("exports."),
            "{module} is served as CommonJS:\n{code}"
        );
        assert!(
            code.contains("import ") || code.contains("import{"),
            "{module} has no ES imports:\n{code}"
        );
        if module != "/src/main.tsx" {
            assert!(
                code.contains("jsxDEV") && code.contains("jsx-dev-runtime"),
                "{module} did not compile its JSX for the dev runtime:\n{code}"
            );
        }
    }
    let main = server.text("/src/main.tsx").await;
    assert!(
        !main.contains(": HTMLElement"),
        "the type annotation survived the compile:\n{main}"
    );
}
