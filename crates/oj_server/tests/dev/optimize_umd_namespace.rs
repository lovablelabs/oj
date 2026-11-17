// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `import * as ns from "<UMD package>"`: a webpack UMD bundle assigns
//! `module.exports = factory()`, so its names exist only at runtime and its
//! pre-bundle exports a lone `default`. Vite's import analysis
//! (`transformCjsImport`) binds the namespace of a dep that needs interop to
//! `{ ...module.exports, default: module.exports }` (for an importer that is
//! an ES module by Node's rules, as in this `"type": "module"` app), so `ns.Options` is the
//! package's own object and `ns.Options.workerSrc = "..."` configures it (the
//! pdf.js setup line). oj bound the ES namespace of the pre-bundle, which has
//! only `default`: the assignment threw on `undefined`.

use std::time::Duration;

use crate::browser::Page;
use crate::App;

const WAIT: Duration = Duration::from_secs(30);

const INDEX: &str = r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#;

/// webpack's `libraryTarget: "umd"` output: the exports are getters defined
/// on an object the factory returns, nothing a static scan can name, and the
/// object is flagged `__esModule`.
const UMD: &str = r#"(function webpackUniversalModuleDefinition(root, factory) {
  if (typeof exports === "object" && typeof module === "object") module.exports = factory();
  else if (typeof define === "function" && define.amd) define("umd-kit", [], factory);
  else if (typeof exports === "object") exports["umd-kit"] = factory();
  else root["umd-kit"] = factory();
})(globalThis, () => {
  return (() => {
    "use strict";
    var __webpack_exports__ = {};
    Object.defineProperty(__webpack_exports__, "__esModule", { value: true });
    var d = (target, defs) => {
      for (var key in defs) Object.defineProperty(target, key, { enumerable: true, get: defs[key] });
    };
    var Options = { workerSrc: "" };
    d(__webpack_exports__, {
      Options: () => Options,
      version: () => "3.0.0",
      current: () => Options.workerSrc,
    });
    return __webpack_exports__;
  })();
});
"#;

const MAIN: &str = r#"import * as kit from "umd-kit";
import kitDefault from "umd-kit";
import * as esm from "esm-kit";
const out = [];
try {
  out.push(Object.keys(kit).sort().join(","));
  out.push(kit.default === kitDefault);
  out.push(kit.Options === kitDefault.Options);
  kit.Options.workerSrc = "/worker.js";
  out.push(kitDefault.current);
  out.push(kit.version);
  out.push(Object.keys(esm).sort().join(","), esm.default, esm.named);
} catch (e) {
  out.push("error: " + e.message);
}
window.__result = out.join("|");
"#;

const EXPECTED: &str =
    "Options,current,default,version|true|true|/worker.js|3.0.0|default,named|esm-default|esm-named";

#[tokio::test(flavor = "multi_thread")]
async fn a_namespace_import_of_a_umd_dep_has_its_runtime_exports() {
    if crate::browser::skip() {
        return;
    }
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"umd-namespace","version":"1.0.0","type":"module"}"#,
        ),
        ("index.html", INDEX),
        ("src/main.js", MAIN),
        (
            "node_modules/umd-kit/package.json",
            r#"{"name":"umd-kit","version":"3.0.0","main":"build/kit.js"}"#,
        ),
        ("node_modules/umd-kit/build/kit.js", UMD),
        (
            "node_modules/esm-kit/package.json",
            r#"{"name":"esm-kit","version":"1.0.0","type":"module","main":"index.js"}"#,
        ),
        (
            "node_modules/esm-kit/index.js",
            "export const named = \"esm-named\";\nexport default \"esm-default\";\n",
        ),
    ]);
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    let done = page.poll("window.__result !== undefined", WAIT).await;
    let errors = page.errors();
    let console = page.console();
    assert!(
        done,
        "main.js never ran; errors: {errors:?}\nconsole: {console:?}"
    );
    let result: String = page.eval("window.__result").await;
    let urls: Vec<String> = page
        .eval(
            r#"performance.getEntriesByType("resource").map((e) => e.name).filter((n) => n.includes("/@oj-deps/"))"#,
        )
        .await;
    page.close().await;
    assert!(
        urls.iter().any(|u| u.contains("/@oj-deps/umd-kit")),
        "the UMD dep is served from the pre-bundle: {urls:#?}"
    );
    assert_eq!(result, EXPECTED, "errors: {errors:?}\nconsole: {console:?}");
    assert_eq!(errors, Vec::<String>::new(), "page errors");
}
