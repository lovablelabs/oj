// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Imports of a dependency that needs CommonJS interop, as Vite's import
//! analysis rewrites them (importAnalysis.ts: `transformCjsImport`,
//! `interopNamedImports`, `interopHelper`).
//!
//! A CommonJS dep's pre-bundle exports a lone `default` (its
//! `module.exports`), and a webpack UMD bundle assigns `module.exports =
//! factory()`, so its names exist only at runtime. Vite binds `import * as
//! ns` to `{ ...module.exports, default: module.exports }`, so `ns.Options`
//! is the package's own object and `ns.Options.workerSrc = "..."` configures
//! it (the pdf.js setup line). oj bound the ES namespace of the pre-bundle,
//! which has only `default`: the assignment threw on `undefined`.
//!
//! What a `module.exports` flagged `__esModule` gives depends on how Node
//! reads the importer (`isFilePathESM` for a static import,
//! `isFilePathFormatExplicit` for a dynamic one): to an ES module (this
//! `"type": "module"` app) it is spread beside `default` like any other and
//! is the default import itself; to any other importer (`legacy/`, under a
//! package.json without `type`, or every importer with
//! `legacy.inconsistentCjsInterop`) it is the namespace as it is and the
//! default import is its `default`, as `React.lazy(() => import(dep))`
//! needs. `export * as ns from` only has its URL rewritten, and the bindings
//! are hoisted above the module's first statement.

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
import esmDefault, { named as esmNamed } from "esm-kit";
import * as direct from "direct-cjs";
import directDefault from "direct-cjs";
import transpiledDefault, { named as transpiledNamed } from "transpiled-cjs";
import { default as transpiledAsDefault } from "transpiled-cjs";
import * as re from "./reexport.js";
import reDefault from "./reexport.js";
import { shadowed } from "./shadowed.js";
import { early } from "./hoisted.js";
import { probe } from "../legacy/probe.js";
import * as star from "./star.js";
import * as builtin from "node:fs";
import "side-effect-only";
import { typed } from "./typed.ts";
import * as undef from "undef-cjs";
import undefDefault from "undef-cjs";
import builtinDefault, { readFile } from "node:fs";

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
await run("flaggedEsm", () => [keys(esm), esm.default === esmDefault, esmDefault, typeof esmNamed]);
await run("direct", () => [
  keys(direct),
  direct.default === directDefault,
  typeof direct.default,
  direct.default.default(),
]);
await run("defaults", () => [
  keys(transpiledDefault),
  transpiledNamed,
  transpiledAsDefault === transpiledDefault,
  reDefault === transpiledDefault,
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
  return [keys(m), m.default, typeof m.named];
});
await run("dynamicTranspiled", async () => {
  const m = await import("transpiled-cjs");
  return [keys(m), typeof m.default, m.default.default()];
});
await run("exportStarAs", () => [keys(re.kit), re.kit.default === kitDefault]);
await run("exportStarAsServed", () => [
  keys(re.direct),
  re.direct.default === directDefault,
  keys(re.directUmd),
  typeof re.directUmd.Options,
]);
await run("bareExportStar", () => [keys(star)]);
await run("templateLiteral", async () => {
  const m = await import(`umd-kit`);
  return [keys(m), m.default === kitDefault];
});
await run("templateLiteralEsm", async () => {
  const m = await import(`plain-esm`);
  return [keys(m), m.value];
});
await run("typeOnlyImport", () => typed(1));
await run("undefinedExports", () => [keys(undef), String(undef.default), String(undefDefault)]);
await run("emptyEntry", () =>
  import("./empty.js").then(
    (m) => ["loaded: " + keys(m), globalThis.__sideEffect],
    (e) => [e.constructor.name, globalThis.__sideEffect],
  ),
);
await run("flaggedWithoutDefault", () =>
  import("./names.js").then(
    (m) => "loaded: " + keys(m),
    (e) => e.constructor.name,
  ),
);
await run("builtin", async () => [
  typeof builtinDefault,
  typeof readFile,
  typeof builtin.default,
  typeof (await import("node:fs")).default,
]);
await run("instances", () =>
  performance
    .getEntriesByType("resource")
    .map((e) => new URL(e.name).pathname)
    .filter((path) => path.includes("umd-kit") || path.includes("plain-esm"))
    .sort(),
);
await run("shadowed", () => shadowed());
await run("hoisted", () => early);
const legacy = await probe();
await run("legacyDynamic", () => legacy.dynamic);
await run("legacyNamespace", () => legacy.namespace);
await run("legacyDefaults", () => legacy.defaults);
window.__result = JSON.stringify(out);
"#;

/// An importer that is not an ES module to Node: every form of import of a
/// dep flagged `__esModule` and of one that is not.
const PROBE: &str = r#"import * as kit from "umd-kit";
import kitDefault from "umd-kit";
import * as transpiled from "transpiled-cjs";
import transpiledDefault, { named } from "transpiled-cjs";
import { default as asDefault } from "transpiled-cjs";
import * as plain from "plain-cjs";
import plainDefault from "plain-cjs";
import reDefault from "./reexport.js";
const keys = (o) => Object.keys(o).sort().join(",");
export const probe = async () => {
  const [t, k, p] = await Promise.all([
    import("transpiled-cjs"),
    import("umd-kit"),
    import("plain-cjs"),
  ]);
  return {
    dynamic: [keys(t), typeof t.default, t.default(), keys(k), typeof k.Options, keys(p)],
    namespace: [
      keys(transpiled),
      typeof transpiled.default,
      keys(kit),
      typeof kit.Options,
      keys(plain),
    ],
    defaults: [
      typeof transpiledDefault,
      transpiledDefault(),
      named,
      asDefault === transpiledDefault,
      reDefault === transpiledDefault,
      typeof kitDefault,
      keys(plainDefault),
    ],
  };
};
"#;

/// What [`PROBE`] reports: Vite's shapes for an importer outside Node mode.
fn probe_cases() -> [(&'static str, Value, &'static str); 3] {
    [
        (
            "Dynamic",
            json!([
                "default,named",
                "function",
                "comp",
                "Options,current,version",
                "object",
                "a,b,default"
            ]),
            "a dynamic import of a dep flagged __esModule is its module.exports, so `default` \
             is the package's own; an unflagged one is spread beside `default`",
        ),
        (
            "Namespace",
            json!([
                "default,named",
                "function",
                "Options,current,version",
                "object",
                "a,b,default"
            ]),
            "the same for a namespace import",
        ),
        (
            "Defaults",
            json!(["function", "comp", "n", true, true, "undefined", "a,b"]),
            "the default import of a dep flagged __esModule is the package's own `default` \
             in every form (`undefined` for one without), of an unflagged one module.exports",
        ),
    ]
}

const REEXPORT: &str = "export * as kit from \"umd-kit\";\nexport * as direct from \"direct-cjs\";\nexport * as directUmd from \"direct-umd\";\nexport { default } from \"transpiled-cjs\";\n";

/// The legacy importer's re-export, without the excluded deps.
const LEGACY_REEXPORT: &str =
    "export * as kit from \"umd-kit\";\nexport { default } from \"transpiled-cjs\";\n";

/// A module whose own top-level bindings are called `Object` and `Array`.
const SHADOWED: &str = r#"import * as kit from "umd-kit";
const Object = null;
const Array = null;
export const shadowed = async () => {
  const m = await import("umd-kit");
  return [Object, Array, typeof kit.Options, typeof m.Options];
};
"#;

/// A function declared in the module, called above the import statement.
const HOISTED: &str = r#"export const early = init();
import * as kit from "umd-kit";
import kitDefault, { version } from "umd-kit";
function init() {
  return [typeof kit.Options, typeof kitDefault, version];
}
"#;

/// The dependencies every app here installs.
fn packages() -> Vec<(&'static str, &'static str)> {
    vec![
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
        (
            "node_modules/direct-umd/package.json",
            r#"{"name":"direct-umd","version":"3.0.0","main":"build/kit.js"}"#,
        ),
        ("node_modules/direct-umd/build/kit.js", UMD),
        // A types-only entry, as tsup and esbuild emit one: an empty file.
        (
            "node_modules/types-only-pkg/package.json",
            r#"{"name":"types-only-pkg","version":"1.0.0","type":"module","main":"index.js","types":"index.d.ts"}"#,
        ),
        ("node_modules/types-only-pkg/index.js", ""),
        (
            "node_modules/types-only-pkg/index.d.ts",
            "export interface Foo { a: number }\nexport interface Bar { b: number }\ndeclare const Whole: Foo;\nexport default Whole;\n",
        ),
        (
            "node_modules/undef-cjs/package.json",
            r#"{"name":"undef-cjs","version":"1.0.0","main":"index.js"}"#,
        ),
        (
            "node_modules/undef-cjs/index.js",
            "module.exports = undefined;\n",
        ),
        (
            "node_modules/plain-esm/package.json",
            r#"{"name":"plain-esm","version":"1.0.0","type":"module","main":"index.js"}"#,
        ),
        (
            "node_modules/plain-esm/index.js",
            "export const value = \"plain-esm\";\n",
        ),
        (
            "node_modules/side-effect-only/package.json",
            r#"{"name":"side-effect-only","version":"1.0.0","type":"module","main":"index.js"}"#,
        ),
        (
            "node_modules/side-effect-only/index.js",
            "globalThis.__sideEffect = true;\n",
        ),
        (
            "node_modules/esm-names/package.json",
            r#"{"name":"esm-names","version":"1.0.0","type":"module","main":"index.js"}"#,
        ),
        (
            "node_modules/esm-names/index.js",
            "export const one = 1;\nexport const two = 2;\n",
        ),
        (
            "node_modules/transpiled-cjs/package.json",
            r#"{"name":"transpiled-cjs","version":"1.0.0","main":"index.js"}"#,
        ),
        ("node_modules/transpiled-cjs/index.js", TRANSPILED),
    ]
}

/// The page's `window.__result`, its errors and the resources it loaded.
async fn load(app: &App) -> (Value, Vec<String>, Vec<String>) {
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
    (serde_json::from_str(&result).unwrap(), errors, urls)
}

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
            r#"{"optimizeDeps":{"needsInterop":["esm-kit","esm-names"],"exclude":["direct-cjs","direct-umd","undef-cjs"]}}"#,
        ),
        ("index.html", INDEX),
        ("src/main.js", MAIN),
        ("src/reexport.js", REEXPORT),
        ("src/shadowed.js", SHADOWED),
        ("src/hoisted.js", HOISTED),
        (
            "legacy/package.json",
            r#"{"name":"legacy","version":"1.0.0"}"#,
        ),
        ("legacy/probe.js", PROBE),
        ("legacy/reexport.js", LEGACY_REEXPORT),
        ("src/star.js", "export * from \"umd-kit\";\n"),
        (
            "src/typed.ts",
            "import { Foo } from \"types-only-pkg\";\nimport Whole, * as All from \"types-only-pkg\";\nexport const typed = (x: Foo | Whole | All.Bar): string => \"typed:\" + typeof x;\n",
        ),
        (
            "src/empty.js",
            "import * as empty from \"side-effect-only\";\nexport const keys = Object.keys(empty);\n",
        ),
        (
            "src/names.js",
            "import * as names from \"esm-names\";\nexport const one = names.one;\n",
        ),
    ]);
    app.write(&packages());
    let (result, errors, urls) = load(&app).await;
    let pre_bundled = |dep: &str| urls.iter().any(|u| u.contains(&format!("/@oj-deps/{dep}")));
    for dep in [
        "umd-kit",
        "plain-cjs",
        "fn-cjs",
        "esm-kit",
        "transpiled-cjs",
    ] {
        assert!(pre_bundled(dep), "{dep} is pre-bundled: {urls:#?}");
    }
    assert!(
        !pre_bundled("direct-cjs"),
        "direct-cjs is served file by file: {urls:#?}"
    );

    let case = |name: &str, want: Value, what: &str| {
        assert_eq!(result[name], want, "{name}: {what}\nerrors: {errors:?}");
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
        json!(["default", true, "esm-default", "undefined"]),
        "an ES module listed in optimizeDeps.needsInterop is read off its `default`, as in \
         Vite: the namespace is built from it and a named import is a property of it",
    );
    case(
        "defaults",
        json!(["default,named", "n", true, true]),
        "in a \"type\": \"module\" package the default import of a dep flagged __esModule \
         is module.exports, in every form",
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
        json!(["default", "esm-default", "undefined"]),
        "a dynamic import of an ES module flagged for interop is built from its `default`",
    );
    case(
        "dynamicTranspiled",
        json!(["default,named", "object", "comp"]),
        "in a \"type\": \"module\" package, a dynamic import's `default` is module.exports \
         though it is flagged __esModule",
    );
    case(
        "exportStarAs",
        json!(["default", true]),
        "`export * as` is the pre-bundle's ES namespace, as in Vite: `default` alone, \
         which is module.exports",
    );
    case(
        "exportStarAsServed",
        json!([
            "default,named",
            true,
            "Options,current,default,version",
            "object"
        ]),
        "`export * as` of a dep served file by file is built from module.exports: no \
         `__cjs_exports` member, `default` is module.exports, a UMD's runtime names are there",
    );
    case(
        "bareExportStar",
        json!([""]),
        "a bare `export * from` a pre-bundled CommonJS dep re-exports nothing, as in Vite: \
         the pre-bundle's only export is `default`",
    );
    case(
        "templateLiteral",
        json!(["Options,current,default,version", true]),
        "a template literal without a substitution names the dep as a string literal does",
    );
    case(
        "templateLiteralEsm",
        json!(["value", "plain-esm"]),
        "a template literal names an ES dep that needs no interop too",
    );
    case(
        "typeOnlyImport",
        json!("typed:number"),
        "a TypeScript import used only as a type, of a package whose entry is empty, is \
         dropped as the TypeScript transform drops it: the module links",
    );
    case(
        "undefinedExports",
        json!(["default", "undefined", "undefined"]),
        "a dep served file by file whose module.exports is undefined is read as that",
    );
    case(
        "emptyEntry",
        json!(["SyntaxError", true]),
        "a dep with no import or export at all needs interop to Vite too, and its \
         pre-bundle has no `default`: a side-effect import of it runs, a namespace import \
         does not link",
    );
    case(
        "flaggedWithoutDefault",
        json!("SyntaxError"),
        "an ES module listed in optimizeDeps.needsInterop that has no `default` export \
         does not link, as in Vite (\"does not provide an export named 'default'\")",
    );
    case(
        "builtin",
        json!(["object", "undefined", "object", "object"]),
        "a Node builtin's stub",
    );
    assert_eq!(
        result["instances"]
            .as_array()
            .map(|urls| urls.iter().filter_map(Value::as_str).collect::<Vec<_>>()),
        Some(vec!["/@oj-deps/plain-esm.mjs", "/@oj-deps/umd-kit.mjs"]),
        "each dep is loaded once, from its pre-bundle, whatever form names it"
    );
    case(
        "hoisted",
        json!(["object", "object", "3.0.0"]),
        "a function declared in the module and called above the import statement reads \
         the bindings",
    );
    for (name, want, what) in probe_cases() {
        case(
            &format!("legacy{name}"),
            want,
            &format!("under a package.json without `type`, {what}"),
        );
    }
    case(
        "shadowed",
        json!([null, null, "object", "object"]),
        "a module with top-level bindings called Object and Array",
    );
    assert_eq!(errors, Vec::<String>::new(), "page errors");
}

// Vite's `legacy.inconsistentCjsInterop` (importAnalysis.ts: `isNodeMode` and
// `isNodeModeForDynamicImport` are false when it is set): no importer is in
// Node mode, so a `"type": "module"` app gets the shapes of one without.
#[tokio::test(flavor = "multi_thread")]
async fn legacy_inconsistent_cjs_interop_reads_a_flagged_dep_as_without_a_package_type() {
    if crate::browser::skip() {
        return;
    }
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"cjs-legacy-interop","version":"1.0.0","type":"module"}"#,
        ),
        (
            "vite.config.js",
            "export default { legacy: { inconsistentCjsInterop: true } };\n",
        ),
        ("index.html", INDEX),
        (
            "src/main.js",
            "import { probe } from \"./probe.js\";\nwindow.__result = JSON.stringify(await probe());\n",
        ),
        ("src/probe.js", PROBE),
        ("src/reexport.js", LEGACY_REEXPORT),
    ]);
    app.write(&packages());
    let (result, errors, _) = load(&app).await;
    for (name, want, what) in probe_cases() {
        assert_eq!(
            result[name.to_lowercase()],
            want,
            "{name}: with legacy.inconsistentCjsInterop, {what}\nerrors: {errors:?}"
        );
    }
    assert_eq!(errors, Vec::<String>::new(), "page errors");
}

// Vite's `isFilePathESM` reads the nearest package.json through a package
// cache its watcher invalidates, so one created, changed or deleted beside
// an importer while the server runs decides the importer's next transform.
#[tokio::test(flavor = "multi_thread")]
async fn a_package_json_created_beside_an_importer_takes_effect_on_its_next_request() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"cjs-package-type","version":"1.0.0","type":"module"}"#,
        ),
        ("index.html", INDEX),
        ("src/main.js", "import \"../sub/probe.js\";\n"),
        (
            "sub/probe.js",
            "import d from \"transpiled-cjs\";\nimport * as ns from \"transpiled-cjs\";\nexport const lazy = () => import(\"transpiled-cjs\");\nexport { d, ns };\n",
        ),
    ]);
    app.write(&packages());
    let server = app.serve().await;
    crate::crawled(&app.root).await;
    // [the default import is module.exports, the namespace's Node-mode flag,
    // the dynamic import's].
    let flags = |code: &str| {
        let flag = |pattern: &str| {
            regex::Regex::new(pattern)
                .unwrap()
                .captures(code)
                .unwrap_or_else(|| panic!("{pattern} in:\n{code}"))[1]
                .to_string()
        };
        (
            code.contains("const d = __ojcjs0;"),
            flag(r"const ns = __oj_dyn_interop\(__ojcjs1, ([01])\);"),
            flag(r"\.then\(\(m\) => __oj_dyn_interop\(m\.default, ([01])\)\)"),
        )
    };
    let served = |label: &'static str| {
        let server = &server;
        async move {
            let code = server.text("/sub/probe.js").await;
            assert!(code.contains("__ojcjs0"), "{label}:\n{code}");
            code
        }
    };
    let (one, zero) = ("1".to_string(), "0".to_string());
    assert_eq!(
        flags(&served("under the app's \"type\": \"module\"").await),
        (true, one.clone(), one.clone())
    );
    app.write(&[("sub/package.json", r#"{"name":"sub"}"#)]);
    assert_eq!(
        flags(&served("a package.json without `type` was created beside it").await),
        (false, zero.clone(), zero.clone()),
        "a package.json without `type` was created beside the importer"
    );
    app.write(&[("sub/package.json", r#"{"name":"sub","type":"commonjs"}"#)]);
    assert_eq!(
        flags(&served("its type became commonjs").await),
        (false, zero, one.clone()),
        "\"type\": \"commonjs\" is explicit: the dynamic import alone is in Node mode"
    );
    std::fs::remove_file(app.root.join("sub/package.json")).unwrap();
    assert_eq!(
        flags(&served("the package.json was deleted").await),
        (true, one.clone(), one),
        "the package.json was deleted"
    );
}

/// Standard base64, as an inline source map is encoded.
pub(crate) fn base64_decode(text: &str) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let (mut out, mut bits, mut have) = (Vec::new(), 0u32, 0);
    for byte in text.bytes().take_while(|b| *b != b'=') {
        let value = ALPHABET.iter().position(|a| *a == byte).expect("base64");
        bits = (bits << 6) | value as u32;
        have += 6;
        if have >= 8 {
            have -= 8;
            out.push((bits >> have) as u8);
        }
    }
    out
}

// Vite's rewrite keeps every line where it was (`getLineBreaks` pads a
// multi-line import, the hoisted bindings add no line), and the module's
// source map is made from the rewritten source: a line the rewrite moved
// would map every later position to the wrong source line.
#[tokio::test(flavor = "multi_thread")]
async fn lines_after_a_multi_line_interop_import_map_to_their_source_lines() {
    const LINES: &str = r#"// leading comment
init();
import {
  version,
  Options as O,
} from "umd-kit";
import * as kit
  from "umd-kit";
const lazy = () => import(
  "plain-cjs"
);
function init() { return [typeof kit.Options, version, typeof O, typeof lazy]; }
export const marker = 42;
export function thrower() { throw new Error("here"); }
"#;
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"cjs-lines","version":"1.0.0","type":"module"}"#,
        ),
        ("index.html", INDEX),
        ("src/main.js", "import \"./lines.js\";\n"),
        ("src/lines.js", LINES),
    ]);
    app.write(&packages());
    let server = app.serve().await;
    crate::crawled(&app.root).await;
    let code = server.text("/src/lines.js").await;
    let encoded = code
        .rsplit_once("sourceMappingURL=data:application/json;base64,")
        .unwrap_or_else(|| panic!("an inline source map in:\n{code}"))
        .1
        .trim();
    let json = String::from_utf8(base64_decode(encoded)).unwrap();
    let map = oxc_sourcemap::SourceMap::from_json_string(&json).unwrap();
    let table = map.generate_lookup_table();
    let source_line = |needle: &str| {
        LINES
            .lines()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("{needle} in the source"))
    };
    // The bindings sit at the start of the file, on the comment's line, so
    // the first statement keeps its line and its column too.
    let (line, text) = code
        .lines()
        .enumerate()
        .find(|(_, l)| l.trim() == "init();")
        .unwrap_or_else(|| panic!("the first statement in:\n{code}"));
    let token = map
        .lookup_token(&table, line as u32, text.find("init").unwrap() as u32)
        .expect("a mapping for the first statement");
    assert_eq!(
        (token.get_src_line(), token.get_src_col()),
        (1, 0),
        "the first statement maps to its source line and column:\n{code}"
    );
    for needle in ["const lazy", "function init", "marker", "thrower"] {
        let (line, text) = code
            .lines()
            .enumerate()
            .find(|(_, l)| l.contains(needle))
            .unwrap_or_else(|| panic!("{needle} in:\n{code}"));
        let column = text.find(needle).unwrap();
        let token = map
            .lookup_token(&table, line as u32, column as u32)
            .unwrap_or_else(|| panic!("a mapping for {needle}"));
        assert_eq!(
            token.get_src_line() as usize,
            source_line(needle),
            "`{needle}` maps to its source line (0-based):\n{code}"
        );
    }
}

// The bindings start the file, before a leading comment: a JSX pragma
// comment still decides the module's JSX runtime.
#[tokio::test(flavor = "multi_thread")]
async fn a_jsx_pragma_after_the_hoisted_bindings_still_takes_effect() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"cjs-pragma","version":"1.0.0","type":"module"}"#,
        ),
        ("index.html", INDEX),
        (
            "src/main.js",
            "import \"./pragma.jsx\";\nimport \"./directive.jsx\";\nimport \"./bom.jsx\";\nimport \"./second.jsx\";\nimport \"./sameline.jsx\";\nimport \"./aftercomment.jsx\";\nimport \"./combo.tsx\";\n",
        ),
        // The pragma after a directive prologue, a byte order mark, and in
        // a second comment after a hashbang-less license one.
        (
            "src/directive.jsx",
            "\"use client\";\n/** @jsxImportSource custom-jsx */\nimport * as kit from \"umd-kit\";\nexport const el = <p>{typeof kit.Options}</p>;\n",
        ),
        (
            "src/bom.jsx",
            "\u{feff}/** @jsxImportSource custom-jsx */\nimport * as kit from \"umd-kit\";\nexport const el = <p>{typeof kit.Options}</p>;\n",
        ),
        (
            "src/combo.tsx",
            "/**\n * @jsxRuntime classic\n * @jsx a\n */\nimport { a } from \"plain-cjs\";\nexport const el = <p />;\n",
        ),
        // The first statement on the pragma's own line.
        (
            "src/sameline.jsx",
            "'use strict'; /** @jsxImportSource custom-jsx */ import * as kit from \"umd-kit\";\nexport const el = <p>{typeof kit.Options}</p>;\n",
        ),
        (
            "src/aftercomment.jsx",
            "/* c */ /** @jsxImportSource custom-jsx */ import * as kit from \"umd-kit\";\nexport const el = <p>{typeof kit.Options}</p>;\n",
        ),
        (
            "src/second.jsx",
            "/* license */\n'use strict';\n// note\n/** @jsxImportSource custom-jsx */\nimport * as kit from \"umd-kit\";\nexport const el = <p>{typeof kit.Options}</p>;\n",
        ),
        (
            "src/pragma.jsx",
            "/** @jsxImportSource custom-jsx */\nimport * as kit from \"umd-kit\";\nexport const el = <p>{typeof kit.Options}</p>;\n",
        ),
        (
            "node_modules/custom-jsx/package.json",
            r#"{"name":"custom-jsx","version":"1.0.0","type":"module","exports":{"./jsx-dev-runtime":"./jsx-dev-runtime.js","./jsx-runtime":"./jsx-dev-runtime.js"}}"#,
        ),
        (
            "node_modules/custom-jsx/jsx-dev-runtime.js",
            "export const jsxDEV = (type, props) => ({ type, props });\nexport const jsx = jsxDEV;\nexport const jsxs = jsxDEV;\nexport const Fragment = \"fragment\";\n",
        ),
    ]);
    app.write(&packages());
    let server = app.serve().await;
    crate::crawled(&app.root).await;
    // Several pragmas in one comment: the runtime, then the factory, which
    // only the JSX refers to and a CommonJS dep supplies.
    let code = server.text("/src/combo.tsx").await;
    assert!(
        code.contains("const { a } = __ojcjs0;") && code.contains("a(\"p\""),
        "combo.tsx: the factory is read off the dep and called:\n{code}"
    );
    for file in [
        "pragma",
        "directive",
        "bom",
        "second",
        "sameline",
        "aftercomment",
    ] {
        let code = server.text(&format!("/src/{file}.jsx")).await;
        assert!(
            code.contains("const kit = __oj_dyn_interop(__ojcjs0, 1);"),
            "{file}.jsx: the namespace import is rewritten:\n{code}"
        );
        assert!(
            code.contains("custom-jsx") && !code.contains("react"),
            "{file}.jsx: the pragma names the JSX runtime:\n{code}"
        );
    }
}

// A classic JSX factory set in the config is referred to by the JSX
// transform alone: its import stays, as oxc's TypeScript transform keeps it,
// and is read off the CommonJS dep that supplies it.
#[tokio::test(flavor = "multi_thread")]
async fn a_configured_jsx_factory_from_a_commonjs_dep_is_kept_in_tsx() {
    if crate::browser::skip() {
        return;
    }
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"cjs-jsx-factory","version":"1.0.0","type":"module"}"#,
        ),
        (
            "vite.config.js",
            "export default { esbuild: { jsx: \"transform\", jsxFactory: \"h\", jsxFragment: \"Fragment\" } };\n",
        ),
        ("index.html", INDEX),
        (
            "src/main.js",
            "import { A } from \"./A.tsx\";\nwindow.__result = JSON.stringify(A());\n",
        ),
        (
            "src/A.tsx",
            "import { h, Fragment } from \"jsx-cjs\";\nexport const A = () => <><b>x</b></>;\n",
        ),
        (
            "node_modules/jsx-cjs/package.json",
            r#"{"name":"jsx-cjs","version":"1.0.0","main":"index.js"}"#,
        ),
        (
            "node_modules/jsx-cjs/index.js",
            "exports.Fragment = \"fragment\";\nexports.h = function (type, props) {\n  return [type].concat([].slice.call(arguments, 2));\n};\n",
        ),
    ]);
    let (result, errors, urls) = load(&app).await;
    assert_eq!(
        result,
        json!(["fragment", ["b", "x"]]),
        "the JSX is built by the dep's factory and fragment; errors: {errors:?}"
    );
    let dep: Vec<&String> = urls.iter().filter(|u| u.contains("jsx-cjs")).collect();
    assert!(
        dep.len() == 1 && dep[0].contains("/@oj-deps/jsx-cjs"),
        "the factory's dep is loaded once, from its pre-bundle: {dep:?}"
    );
    assert_eq!(errors, Vec::<String>::new(), "page errors");
}

// tsconfig `verbatimModuleSyntax` keeps an import written as a value, in the
// TypeScript transform (Vite hands it over as `onlyRemoveTypeImports`) and in
// the interop rewrite before it; without it an import no value refers to is
// removed, and its dep is not loaded.
#[tokio::test(flavor = "multi_thread")]
async fn verbatim_module_syntax_keeps_an_unused_value_import() {
    // An unused value import of a CommonJS dep and of an ES dep, an inline
    // type, and a type imported without `type`.
    const UNUSED: &str = "import { a } from \"plain-cjs\";\nimport { named } from \"esm-kit\";\nimport { type T, b } from \"plain-cjs\";\nimport { Shape } from \"plain-esm\";\nexport const n: number = 1;\nexport let shape: Shape;\n";
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"cjs-verbatim","version":"1.0.0","type":"module"}"#,
        ),
        ("index.html", INDEX),
        (
            "src/main.js",
            "import \"./elided.ts\";\nimport \"../verbatim/kept.ts\";\n",
        ),
        ("src/elided.ts", UNUSED),
        (
            "verbatim/tsconfig.json",
            r#"{"compilerOptions":{"verbatimModuleSyntax":true}}"#,
        ),
        ("verbatim/kept.ts", UNUSED),
        // create-vite's layout: a root tsconfig of references, the option
        // in the project that includes `src`.
        (
            "template/tsconfig.json",
            r#"{"files":[],"references":[{"path":"./tsconfig.app.json"}]}"#,
        ),
        (
            "template/tsconfig.app.json",
            r#"{"compilerOptions":{"verbatimModuleSyntax":true},"include":["src"]}"#,
        ),
        ("template/src/kept.ts", UNUSED),
        ("template/scripts/elided.ts", UNUSED),
    ]);
    app.write(&packages());
    let server = app.serve().await;
    crate::crawled(&app.root).await;
    let elided = server.text("/src/elided.ts").await;
    assert!(
        !elided.contains("plain-cjs") && !elided.contains("esm-kit"),
        "no value refers to the imports: both are removed\n{elided}"
    );
    let kept = server.text("/verbatim/kept.ts").await;
    assert!(
        kept.contains("const { a } = __ojcjs0;")
            && kept.contains("const { b } = __ojcjs1;")
            && !kept.contains(" T "),
        "the CommonJS dep's value imports are read through interop, the inline type is not:\n{kept}"
    );
    assert!(
        kept.contains("import { named } from \"/@oj-deps/esm-kit"),
        "the ES dep's import stays too:\n{kept}"
    );
    assert!(
        kept.contains("import { Shape } from \"/@oj-deps/plain-esm"),
        "a type imported without `type` stays, as under Vite (it fails to link in the browser):\n{kept}"
    );
    // The project that includes the file decides.
    let in_project = server.text("/template/src/kept.ts").await;
    assert!(
        in_project.contains("const { a } = __ojcjs0;"),
        "a file the referenced project includes is under its option:\n{in_project}"
    );
    let outside = server.text("/template/scripts/elided.ts").await;
    assert!(
        !outside.contains("plain-cjs"),
        "a file no project includes is not:\n{outside}"
    );
    // An edit to the tsconfig is picked up while the server runs.
    app.write(&[(
        "verbatim/tsconfig.json",
        r#"{"compilerOptions":{"verbatimModuleSyntax":false}}"#,
    )]);
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let code = server.text("/verbatim/kept.ts").await;
        if !code.contains("plain-cjs") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the edited tsconfig never took effect:\n{code}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// A template-literal specifier that resolves to nothing fails the compile,
// as the string form does.
#[tokio::test(flavor = "multi_thread")]
async fn an_unresolvable_template_literal_import_is_a_compile_error() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"cjs-template","version":"1.0.0","type":"module"}"#,
        ),
        ("index.html", INDEX),
        ("src/main.js", "import \"./tpl.js\";\n"),
        (
            "src/tpl.js",
            "export const load = () => import(`nope-pkg`);\n",
        ),
        (
            "src/str.js",
            "export const load = () => import(\"nope-pkg\");\n",
        ),
    ]);
    let server = app.serve().await;
    for file in ["tpl", "str"] {
        let res = server.get(&format!("/src/{file}.js")).await;
        assert_eq!(res.status, 500, "{file}.js:\n{}", res.body);
        assert!(
            res.body.contains("Failed to resolve import \"nope-pkg\""),
            "{file}.js:\n{}",
            res.body
        );
    }
}
