// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Dependencies installed after the dev server started, reached through a
//! dynamic import in a handler (`const { default: shot } = await
//! import("late-shot")`), as adding an export-to-PDF feature does. The
//! startup scan never saw them: the edit that imports them lands as a hot
//! update, the first compile of the edited module registers the deps (Vite's
//! `registerMissingImport`) and rewrites the imports to their future bundles,
//! and the dep route holds their requests until the re-optimization commits.
//! One of the new deps shares a module with a dep the page already loaded,
//! so the rerun moves that dep's chunk and commits with a reload (a React
//! page's pre-bundles do the same through the runtime chunk they share):
//! Vite's optimizedDeps load serves the requests it held across that commit,
//! then the reload lands. oj answered them with the outdated 504, so the
//! handler's import rejected ("Failed to fetch dynamically imported module")
//! a moment before the page reloaded.
//!
//! The packages mirror what such a feature installs: a dual package whose
//! `browser` field is a UMD bundle and `module` an ES one (Vite reads the
//! browser entry and, finding no ESM syntax, takes `module`, as in
//! `resolvePackageEntry`), an ES package that imports a CommonJS helper and
//! dynamically imports an optional dependency that is not installed, and a
//! plain CommonJS package whose named members the handler destructures off
//! the dynamic import (Vite's `interopNamedImports` for a dynamic import of
//! a dep that needs interop).

use std::time::Duration;

use crate::browser::Page;
use crate::App;

const WAIT: Duration = Duration::from_secs(30);

const INDEX: &str = r#"<!doctype html><html><head><title>t</title></head><body><p id="a"></p><p id="out"></p><script type="module" src="/src/main.js"></script></body></html>"#;

const MAIN: &str = r#"import { a } from "served-a";
import { setup } from "./app.js";
sessionStorage.loads = String(Number(sessionStorage.loads || 0) + 1);
document.getElementById("a").textContent = a();
setup();
if (import.meta.hot) {
  import.meta.hot.accept("./app.js", (m) => m.setup());
}
"#;

const APP_BEFORE: &str = r#"export function setup() {
  document.getElementById("out").textContent = "waiting";
}
"#;

/// The edit that brings the installed packages in: a handler importing them
/// on demand, run as soon as the update lands (a click right after the
/// edit). Its imports go out together, so all three are in flight when the
/// rerun commits: those are the requests Vite serves across the commit. One
/// issued after it, under the version the module was compiled with, is
/// Vite's outdated request too, retried by the reload that follows. The
/// first outcome is kept in `sessionStorage`, where a reload cannot wipe it;
/// a reload mid-handler (the page dropped the pending promise) runs the
/// handler again, as a user would click again.
const APP_AFTER: &str = r#"export function setup() {
  document.getElementById("out").textContent = "ready";
  const exportPdf = async () => {
    sessionStorage.pending = "1";
    try {
      const [{ default: shot, version }, { jsPDF, default: Pdf }, { default: cjs, VERSION }] =
        await Promise.all([import("late-shot"), import("late-pdf"), import("late-cjs")]);
      const doc = new jsPDF();
      sessionStorage.result ??= [
        shot(),
        version,
        doc.kind,
        Pdf === jsPDF,
        doc.shared,
        cjs.render(),
        VERSION,
      ].join("|");
    } catch (e) {
      sessionStorage.result ??= "error: " + (e && e.message ? e.message : e);
    }
    delete sessionStorage.pending;
  };
  exportPdf();
}
"#;

const EXPECTED: &str = "shot-esm|1|object|true|s|cjs-render|2";

/// The dep the page loads from the start, with the module `late-pdf` will
/// share.
const SERVED: &[(&str, &str)] = &[
    (
        "node_modules/served-a/package.json",
        r#"{"name":"served-a","version":"1.0.0","type":"module","main":"index.js"}"#,
    ),
    (
        "node_modules/served-a/index.js",
        "import { store } from \"./shared.js\";\nexport { store };\nexport const a = () => \"a-\" + store.id;\n",
    ),
    (
        "node_modules/served-a/shared.js",
        "export const store = { id: \"s\" };\n",
    ),
];

/// The installed packages. `late-shot` is html2canvas's shape (`browser` a
/// UMD bundle, `module` an ES build); `late-pdf` is jspdf's (an ES build
/// importing a CommonJS helper, with `import()` of an optional dependency
/// that is not installed), and it imports `served-a`'s shared module;
/// `late-cjs` a plain CommonJS package.
const INSTALLED: &[(&str, &str)] = &[
    (
        "node_modules/late-shot/package.json",
        r#"{"name":"late-shot","version":"1.0.0","main":"dist/late-shot.js","module":"dist/late-shot.esm.js","browser":"dist/late-shot.js"}"#,
    ),
    (
        "node_modules/late-shot/dist/late-shot.js",
        "(function (global, factory) {\n  typeof exports === 'object' && typeof module !== 'undefined' ? module.exports = factory() :\n  typeof define === 'function' && define.amd ? define(factory) :\n  (global = typeof globalThis !== 'undefined' ? globalThis : global || self, global.lateShot = factory());\n}(this, (function () { 'use strict';\n  var shot = function () { return \"shot-umd\"; };\n  shot.version = \"1\";\n  return shot;\n})));\n",
    ),
    (
        "node_modules/late-shot/dist/late-shot.esm.js",
        "const shot = () => \"shot-esm\";\nexport const version = \"1\";\nexport default shot;\n",
    ),
    (
        "node_modules/late-pdf/package.json",
        r#"{"name":"late-pdf","version":"1.0.0","main":"dist/late-pdf.node.js","module":"dist/late-pdf.es.js","browser":"dist/late-pdf.es.js","dependencies":{"late-helper":"1.0.0","served-a":"1.0.0"},"optionalDependencies":{"late-svg":"^1.0.0"}}"#,
    ),
    (
        "node_modules/late-pdf/dist/late-pdf.es.js",
        "import _typeof from \"late-helper\";\nimport { store } from \"served-a/shared.js\";\nfunction loadSvg() {\n  return import(\"late-svg\");\n}\nclass jsPDF {\n  constructor() {\n    this.kind = _typeof({});\n    this.shared = store.id;\n  }\n  svg() {\n    return loadSvg();\n  }\n}\nexport { jsPDF, loadSvg };\nexport default jsPDF;\n",
    ),
    (
        "node_modules/late-pdf/dist/late-pdf.node.js",
        "module.exports = {};\n",
    ),
    (
        "node_modules/late-helper/package.json",
        r#"{"name":"late-helper","version":"1.0.0","main":"index.js"}"#,
    ),
    (
        "node_modules/late-helper/index.js",
        "module.exports = function _typeof(o) {\n  return typeof o;\n};\n",
    ),
    (
        "node_modules/late-cjs/package.json",
        r#"{"name":"late-cjs","version":"1.0.0","main":"lib/index.js"}"#,
    ),
    (
        "node_modules/late-cjs/lib/index.js",
        "module.exports = {\n  render: function () { return \"cjs-render\"; },\n  VERSION: \"2\",\n};\n",
    ),
];

fn app(app_js: &str) -> App {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"installed-later","version":"1.0.0"}"#,
        ),
        ("index.html", INDEX),
        ("src/main.js", MAIN),
        ("src/app.js", app_js),
    ]);
    app.write(SERVED);
    app
}

/// The page, loaded once and showing `served-a` and `app.js`'s `out`.
async fn open(app: &App, out: &str) -> (crate::Server, crate::listen::Listening, Page) {
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for(
        &format!(
            r#"document.getElementById("a")?.textContent === "a-s" && document.getElementById("out")?.textContent === "{out}""#
        ),
        WAIT,
    )
    .await;
    (server, listening, page)
}

/// The handler's first outcome.
async fn result(page: &Page) -> String {
    let done = page
        .poll(r#"sessionStorage.result !== undefined"#, WAIT)
        .await;
    assert!(
        done,
        "the handler never finished; errors: {:?}\nconsole: {:?}",
        page.errors(),
        page.console()
    );
    page.eval("sessionStorage.result").await
}

// The packages land after the server started, then the edit importing them
// arrives as a hot update: the handler's dynamic imports, held while the
// rerun bundles the deps, get their bundles when it commits, although the
// commit moves every version and reloads the page right after.
#[tokio::test(flavor = "multi_thread")]
async fn deps_installed_after_start_run_from_a_hot_updated_dynamic_import() {
    if crate::browser::skip() {
        return;
    }
    let app = app(APP_BEFORE);
    let (_server, _listening, page) = open(&app, "waiting").await;
    // The install, then the edit that uses it.
    app.write(INSTALLED);
    app.write(&[("src/app.js", APP_AFTER)]);
    let result = result(&page).await;
    // The rerun moved served-a's chunk: the commit reloads, once.
    let reloaded = page
        .poll(
            r#"sessionStorage.loads === "2" && document.getElementById("a")?.textContent === "a-s""#,
            WAIT,
        )
        .await;
    let errors = page.errors();
    let console = page.console();
    page.close().await;
    assert_eq!(result, EXPECTED, "errors: {errors:?}\nconsole: {console:?}");
    assert!(
        !console.iter().any(|m| m.contains("504")),
        "a held dep request was answered outdated: {console:?}"
    );
    assert_eq!(errors, Vec::<String>::new(), "page errors");
    assert!(reloaded, "the rerun's commit reloads the page: {console:?}");
}

// The same project after a restart: the packages are installed and the
// handler is in place when the server starts, so the scan sees the dynamic
// imports and the first load serves the pre-bundles.
#[tokio::test(flavor = "multi_thread")]
async fn deps_behind_a_dynamic_import_run_on_a_fresh_start() {
    if crate::browser::skip() {
        return;
    }
    let app = app(APP_AFTER);
    app.write(INSTALLED);
    let (_server, _listening, page) = open(&app, "ready").await;
    let result = result(&page).await;
    let errors = page.errors();
    let urls: Vec<String> = page
        .eval(
            r#"performance.getEntriesByType("resource").map((e) => e.name).filter((n) => n.includes("/@oj-deps/"))"#,
        )
        .await;
    let loads: String = page.eval("sessionStorage.loads").await;
    page.close().await;
    assert_eq!(
        result, EXPECTED,
        "deps loaded: {urls:#?}\nerrors: {errors:?}"
    );
    assert_eq!(errors, Vec::<String>::new(), "page errors");
    assert_eq!(loads, "1", "no reload: the scan saw every dep");
    assert!(
        urls.iter().any(|u| u.contains("/@oj-deps/late-shot")),
        "the deps are served from the pre-bundle: {urls:#?}"
    );
}
