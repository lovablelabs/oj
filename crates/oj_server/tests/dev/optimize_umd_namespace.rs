// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `import * as ns from "<CommonJS package>"`: a CommonJS dep's pre-bundle
//! exports a lone `default` (its `module.exports`), and a webpack UMD bundle
//! assigns `module.exports = factory()`, so its names exist only at runtime.
//! Vite's import analysis (`transformCjsImport`) binds the namespace of a dep
//! that needs interop to `{ ...module.exports, default: module.exports }`
//! (for an importer that is an ES module by Node's rules, as in this
//! `"type": "module"` app), so `ns.Options` is the package's own object and
//! `ns.Options.workerSrc = "..."` configures it (the pdf.js setup line). oj
//! bound the ES namespace of the pre-bundle, which has only `default`: the
//! assignment threw on `undefined`.
//!
//! A dynamic `import()` and `export * as ns from` of the same dep give the
//! same members (Vite's `interopNamedImports` wraps the dynamic import in the
//! same helper).

use std::time::Duration;

use serde_json::{json, Value};

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

/// A transpiled ES module: flagged `__esModule`, with a `default` of its own.
const TRANSPILED: &str = r#""use strict";
Object.defineProperty(exports, "__esModule", { value: true });
exports.default = function Comp() { return "comp"; };
exports.named = "n";
"#;

const MAIN: &str = r#"import * as kit from "umd-kit";
import kitDefault from "umd-kit";
import * as plain from "plain-cjs";
import plainDefault from "plain-cjs";
import * as fn from "fn-cjs";
import fnDefault from "fn-cjs";
import * as esm from "esm-kit";
import * as direct from "direct-cjs";
import directDefault from "direct-cjs";
import * as re from "./reexport.js";
import { shadowed } from "./shadowed.js";

const keys = (o) => Object.keys(o).sort().join(",");
const out = {};
const run = async (name, f) => {
  try {
    out[name] = await f();
  } catch (e) {
    out[name] = "error: " + e.message;
  }
};
await run("umd", () => {
  const before = [keys(kit), kit.default === kitDefault, kit.Options === kitDefault.Options];
  kit.Options.workerSrc = "/worker.js";
  return [...before, kitDefault.current, kit.version];
});
await run("plain", () => [keys(plain), plain.default === plainDefault, plain.a]);
await run("fn", () => [keys(fn), fn.default === fnDefault, fn.default(), fn.extra]);
await run("flaggedEsm", () => [keys(esm), esm.default, esm.named]);
await run("direct", () => [
  keys(direct),
  direct.default === directDefault,
  typeof direct.default,
  direct.default.default(),
]);
await run("dynamicUmd", async () => {
  const m = await import("umd-kit");
  return [keys(m), m.default === kitDefault, m.Options === kit.Options];
});
await run("dynamicPlain", async () => {
  const m = await import("plain-cjs");
  return [keys(m), m.default === plainDefault, Object.getPrototypeOf(m) === Object.prototype];
});
await run("dynamicFn", async () => {
  const m = await import("fn-cjs");
  return [keys(m), m.default === fnDefault];
});
await run("dynamicFlaggedEsm", async () => {
  const m = await import("esm-kit");
  return [keys(m), m.default, m.named];
});
await run("exportStarAs", () => [keys(re.kit), re.kit.default === kitDefault]);
await run("shadowed", () => shadowed());
window.__result = JSON.stringify(out);
"#;

const REEXPORT: &str = "export * as kit from \"umd-kit\";\n";

/// A module whose own top-level bindings are called `Object` and `Array`.
const SHADOWED: &str = r#"import * as kit from "umd-kit";
const Object = null;
const Array = null;
export const shadowed = async () => {
  const m = await import("umd-kit");
  return [Object, Array, typeof kit.Options, typeof m.Options];
};
"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_namespace_import_of_a_commonjs_dep_has_its_runtime_exports() {
    if crate::browser::skip() {
        return;
    }
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"cjs-namespace","version":"1.0.0","type":"module"}"#,
        ),
        // `esm-kit` is an ES module flagged for interop; `direct-cjs` is
        // served file by file instead of from a pre-bundle.
        (
            "oj.config.json",
            r#"{"optimizeDeps":{"needsInterop":["esm-kit"],"exclude":["direct-cjs"]}}"#,
        ),
        ("index.html", INDEX),
        ("src/main.js", MAIN),
        ("src/reexport.js", REEXPORT),
        ("src/shadowed.js", SHADOWED),
        (
            "node_modules/umd-kit/package.json",
            r#"{"name":"umd-kit","version":"3.0.0","main":"build/kit.js"}"#,
        ),
        ("node_modules/umd-kit/build/kit.js", UMD),
        (
            "node_modules/plain-cjs/package.json",
            r#"{"name":"plain-cjs","version":"1.0.0","main":"index.js"}"#,
        ),
        (
            "node_modules/plain-cjs/index.js",
            "exports.a = \"a\";\nexports.b = function () { return \"b\"; };\n",
        ),
        (
            "node_modules/fn-cjs/package.json",
            r#"{"name":"fn-cjs","version":"1.0.0","main":"index.js"}"#,
        ),
        (
            "node_modules/fn-cjs/index.js",
            "function fn() { return \"fn\"; }\nfn.extra = \"x\";\nmodule.exports = fn;\n",
        ),
        (
            "node_modules/esm-kit/package.json",
            r#"{"name":"esm-kit","version":"1.0.0","type":"module","main":"index.js"}"#,
        ),
        (
            "node_modules/esm-kit/index.js",
            "export const named = \"esm-named\";\nexport default \"esm-default\";\n",
        ),
        (
            "node_modules/direct-cjs/package.json",
            r#"{"name":"direct-cjs","version":"1.0.0","main":"index.js"}"#,
        ),
        ("node_modules/direct-cjs/index.js", TRANSPILED),
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
        .eval(r#"performance.getEntriesByType("resource").map((e) => e.name)"#)
        .await;
    page.close().await;
    let result: Value = serde_json::from_str(&result).unwrap();
    let pre_bundled = |dep: &str| urls.iter().any(|u| u.contains(&format!("/@oj-deps/{dep}")));
    for dep in ["umd-kit", "plain-cjs", "fn-cjs", "esm-kit"] {
        assert!(pre_bundled(dep), "{dep} is pre-bundled: {urls:#?}");
    }
    assert!(
        !pre_bundled("direct-cjs"),
        "direct-cjs is served file by file: {urls:#?}"
    );

    let case = |name: &str, want: Value, what: &str| {
        assert_eq!(
            result[name], want,
            "{name}: {what}\nerrors: {errors:?}\nconsole: {console:?}"
        );
    };
    case(
        "umd",
        json!([
            "Options,current,default,version",
            true,
            true,
            "/worker.js",
            "3.0.0"
        ]),
        "the UMD dep's runtime exports are members, `default` is module.exports, and an \
         assignment to a member's property reaches the package",
    );
    case(
        "plain",
        json!(["a,b,default", true, "a"]),
        "a CommonJS dep that is not flagged __esModule",
    );
    case(
        "fn",
        json!(["default,extra", true, "fn", "x"]),
        "a function-valued module.exports with a static property",
    );
    case(
        "flaggedEsm",
        json!(["default,named", "esm-default", "esm-named"]),
        "an ES module flagged for interop keeps its ES namespace",
    );
    case(
        "direct",
        json!(["default,named", true, "object", "comp"]),
        "a dep served file by file, flagged __esModule with a default of its own: \
         `ns.default` is module.exports, as the default import is",
    );
    case(
        "dynamicUmd",
        json!(["Options,current,default,version", true, true]),
        "a dynamic import has the members of the static namespace",
    );
    case(
        "dynamicPlain",
        json!(["a,b,default", true, true]),
        "a dynamic import of a plain CommonJS dep, an ordinary object as in Vite",
    );
    case(
        "dynamicFn",
        json!(["default,extra", true]),
        "a dynamic import of a function-valued module.exports",
    );
    case(
        "dynamicFlaggedEsm",
        json!(["default,named", "esm-default", "esm-named"]),
        "a dynamic import of an ES module flagged for interop is its ES namespace",
    );
    case(
        "exportStarAs",
        json!(["Options,current,default,version", true]),
        "`export * as` has the members of the static namespace",
    );
    case(
        "shadowed",
        json!([null, null, "object", "object"]),
        "a module with top-level bindings called Object and Array",
    );
    assert_eq!(errors, Vec::<String>::new(), "page errors");
}
