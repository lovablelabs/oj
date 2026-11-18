// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The importer a plugin's `resolveId` gets for an import inside a module a
//! plugin resolved is that module's id as the plugin returned it: one `\0`
//! when the id has one, none when it has none (Vite's importAnalysis passes
//! `importerModule.id`). oj passed `\0` + the id, so a plugin that wraps a
//! package and lets the wrapper's own import through by comparing the
//! importer with its id resolved that import back to the wrapper, and the
//! wrapper imported itself.

use regex::Regex;

use crate::{App, INDEX_HTML};

const PLUGINS: &str = r#"import fs from "node:fs";
const calls = new URL("./calls.log", import.meta.url);
const WRAPPED = "\0wrapped-jsx-dev-runtime";
const PLAIN = "virtual:wrapped-lib";
const wraps = { "react/jsx-dev-runtime": WRAPPED, "plain-lib": PLAIN };
export default [
  {
    name: "wrap",
    enforce: "pre",
    resolveId(source, importer) {
      const id = wraps[source];
      if (!id) return;
      fs.appendFileSync(calls, JSON.stringify([source, importer ?? null]) + "\n");
      if (importer !== id) return id;
    },
    load(id) {
      if (id === WRAPPED) {
        return `import * as rt from "react/jsx-dev-runtime";
export const Fragment = rt.Fragment;
export const jsxDEV = (...a) => ({ wrapped: rt.jsxDEV(...a) });`;
      }
      if (id === PLAIN) return `import lib from "plain-lib"; export default ["wrapped", lib];`;
    },
  },
];
"#;

fn app() -> App {
    App::new(&[
        (
            "package.json",
            r#"{"name":"virtual-importer","private":true,"type":"module"}"#,
        ),
        ("index.html", INDEX_HTML),
        ("oj.plugins.mjs", PLUGINS),
        (
            "src/main.js",
            "import { App } from \"./App.jsx\";\nimport lib from \"plain-lib\";\nwindow.__APP = [App(), lib];\n",
        ),
        ("src/App.jsx", "export const App = () => <div>hi</div>;\n"),
        (
            "node_modules/react/package.json",
            r#"{"name":"react","version":"1.0.0","type":"module","exports":{"./jsx-runtime":"./jsx-runtime.js","./jsx-dev-runtime":"./jsx-dev-runtime.js"}}"#,
        ),
        (
            "node_modules/react/jsx-runtime.js",
            "export const Fragment = Symbol();\nexport const jsx = (t, p) => ({ t, p });\nexport const jsxs = jsx;\n",
        ),
        (
            "node_modules/react/jsx-dev-runtime.js",
            "export const Fragment = Symbol();\nexport const jsxDEV = (t, p) => ({ t, p });\n",
        ),
        (
            "node_modules/plain-lib/package.json",
            r#"{"name":"plain-lib","version":"1.0.0","type":"module","main":"index.js"}"#,
        ),
        (
            "node_modules/plain-lib/index.js",
            "export default 'installed';\n",
        ),
    ])
}

/// The URL `code` imports from in the statement matching `binding`.
fn import_url(code: &str, binding: &str) -> String {
    let re = Regex::new(&format!(r#"import {binding} from "([^"]+)""#)).unwrap();
    match re.captures(code) {
        Some(c) => c[1].to_string(),
        None => panic!("no import of {binding}:\n{code}"),
    }
}

/// The importers `resolveId` was called with for `source`.
fn importers(app: &App, source: &str) -> Vec<Option<String>> {
    let log = std::fs::read_to_string(app.root.join("calls.log")).unwrap_or_default();
    log.lines()
        .map(|line| serde_json::from_str::<(String, Option<String>)>(line).unwrap())
        .filter(|(s, _)| s == source)
        .map(|(_, importer)| importer)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_import_inside_a_plugin_resolved_module_resolves_from_that_modules_id() {
    let app = app();
    let server = app.serve().await;

    // The JSX runtime wrapper: its id starts with `\0`.
    let app_js = server.text("/src/App.jsx").await;
    let wrapper_url = import_url(&app_js, r"\{[^}]*jsxDEV[^}]*\}");
    let wrapper = server.text(&wrapper_url).await;
    assert!(
        wrapper.contains("wrapped: rt.jsxDEV"),
        "App.jsx takes its JSX runtime from the wrapper: {wrapper_url}\n{wrapper}"
    );
    let seen = importers(&app, "react/jsx-dev-runtime");
    assert!(
        seen.contains(&Some("\0wrapped-jsx-dev-runtime".into())),
        "the wrapper's own import resolves from the wrapper's id, one NUL: {seen:?}"
    );
    assert!(
        !seen
            .iter()
            .flatten()
            .any(|importer| importer.starts_with("\0\0")),
        "no importer carries a second NUL: {seen:?}"
    );
    let inner = import_url(&wrapper, r"\* as rt");
    assert_ne!(
        inner, wrapper_url,
        "the wrapper imports the package, not itself:\n{wrapper}"
    );
    let runtime = server.text(&inner).await;
    assert!(
        runtime.contains("jsxDEV") && !runtime.contains("wrapped:"),
        "the wrapper's import is the package's runtime: {inner}\n{runtime}"
    );

    // An id with no `\0` is the importer unchanged.
    let main = server.text("/src/main.js").await;
    let plain_url = import_url(&main, "lib");
    let plain = server.text(&plain_url).await;
    assert!(
        plain.contains(r#"["wrapped", lib]"#),
        "main takes plain-lib from its wrapper: {plain_url}\n{plain}"
    );
    let seen = importers(&app, "plain-lib");
    assert!(
        seen.contains(&Some("virtual:wrapped-lib".into())),
        "an id without a NUL is passed as it is: {seen:?}"
    );
    assert!(
        !seen
            .iter()
            .flatten()
            .any(|importer| importer.contains('\0')),
        "no NUL is added to an id that has none: {seen:?}"
    );
    let inner = import_url(&plain, "lib");
    assert_ne!(
        inner, plain_url,
        "the wrapper imports the package, not itself:\n{plain}"
    );
}
