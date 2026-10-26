// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The optimizer over real packages in a temp app: what the scan finds, what
//! the bundle writes, and the metadata the dev server gets. The scan runs
//! without a plugin host here, but for how it paces its calls into one;
//! e2e/dep-optimize-rolldown.mjs covers the plugin-aware half.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;

use super::{Prebuilt, Prebundle};
use crate::optimize::OptimizeInput;

struct App {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl App {
    fn new() -> App {
        App::in_dir("app")
    }

    /// An app in a directory of this name (glob syntax in a path is literal).
    fn in_dir(name: &str) -> App {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join(name);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("package.json"), r#"{"name":"fx"}"#).unwrap();
        App { _dir: dir, root }
    }

    fn root(&self) -> PathBuf {
        self.root.clone()
    }

    fn write(&self, rel: &str, content: &str) -> &App {
        let path = self.root().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
        self
    }

    /// A package under node_modules: `main` plus its files.
    fn pkg(
        &self,
        name: &str,
        main: &str,
        extra: serde_json::Value,
        files: &[(&str, &str)],
    ) -> &App {
        let mut meta = json!({ "name": name, "version": "1.0.0", "main": main });
        for (k, v) in extra.as_object().into_iter().flatten() {
            meta[k] = v.clone();
        }
        self.write(
            &format!("node_modules/{name}/package.json"),
            &meta.to_string(),
        );
        for (file, content) in files {
            self.write(&format!("node_modules/{name}/{file}"), content);
        }
        self
    }

    fn cjs(&self, name: &str, files: &[(&str, &str)]) -> &App {
        self.pkg(name, "index.js", json!({}), files)
    }

    fn esm(&self, name: &str, files: &[(&str, &str)]) -> &App {
        self.pkg(name, "index.js", json!({ "type": "module" }), files)
    }

    fn out_dir(&self) -> PathBuf {
        self.root().join(".oj-cache/deps")
    }

    fn read_out(&self, file: &str) -> String {
        std::fs::read_to_string(self.out_dir().join(file)).unwrap()
    }

    fn entry(&self, rel: &str) -> Vec<String> {
        vec![self.root().join(rel).to_string_lossy().into_owned()]
    }

    fn prebundle(&self, input: &OptimizeInput, implicit: &[&str]) -> Arc<Prebundle> {
        let input = OptimizeInput {
            node_env: if input.node_env.is_empty() {
                "development".into()
            } else {
                input.node_env.clone()
            },
            ..input.clone()
        };
        Arc::new(Prebundle::new(
            &self.root(),
            &self.out_dir(),
            &input,
            implicit.iter().map(|s| s.to_string()),
            false,
            // The test's own runtime: these check what the bundle holds.
            &tokio::runtime::Handle::current(),
        ))
    }

    /// One optimizer run, as optimize.rs drives it without a host.
    async fn optimize(&self, input: OptimizeInput) -> BTreeMap<String, Prebuilt> {
        let pre = self.prebundle(&input, &[]);
        let scanned = pre.scan(None).await;
        pre.bundle(&pre.plan(&scanned, true)).await.unwrap()
    }

    async fn scan(&self, input: OptimizeInput) -> Vec<String> {
        let pre = self.prebundle(&input, &[]);
        pre.scan(None)
            .await
            .into_iter()
            .map(|(dep, _)| dep)
            .collect()
    }
}

fn keys(map: &BTreeMap<String, Prebuilt>) -> Vec<&str> {
    map.keys().map(String::as_str).collect()
}

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

const DEFPROP: &str = "\"use strict\";\nObject.defineProperty(exports, \"__esModule\", { value: true });\nObject.defineProperty(exports, \"greet\", { enumerable: true, get: function () { return greet; } });\nfunction greet(n) { return \"hi \" + n; }\n";

#[tokio::test(flavor = "multi_thread")]
async fn commonjs_deps_bundle_with_interop() {
    let app = App::new();
    app.cjs("defprop", &[("index.js", DEFPROP)])
        .cjs(
            "babeldefault",
            &[(
                "index.js",
                "\"use strict\";\nObject.defineProperty(exports, \"__esModule\", { value: true });\nexports.default = void 0;\nvar _default = function () { return 42; };\nexports.default = _default;\n",
            )],
        )
        .cjs("plaincjs", &[("index.js", "exports.a = 1;\nexports.b = 2;\n")])
        .write(
            "entry.js",
            "import { greet } from \"defprop\";\nimport fortytwo from \"babeldefault\";\nimport { a, b } from \"plaincjs\";\nexport const out = greet(\"x\") + fortytwo() + a + b;\n",
        );
    let meta = app
        .optimize(OptimizeInput {
            entries: app.entry("entry.js"),
            ..Default::default()
        })
        .await;
    assert_eq!(keys(&meta), ["babeldefault", "defprop", "plaincjs"]);
    for m in meta.values() {
        assert!(m.needs_interop, "a CJS dep needs interop: {m:?}");
        assert_eq!(m.file_hash.len(), 16);
        let code = app.read_out(&m.file);
        assert!(code.contains("export default require_"), "{code}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn tsconfig_paths_reach_deps_and_a_deps_css_stays_external() {
    let app = App::new();
    // A `paths` value spells `/*`, the shape a naive comment stripper eats.
    app.write(
        "tsconfig.json",
        "{ // comment\n \"compilerOptions\": { \"baseUrl\": \".\", \"paths\": { \"@/*\": [\"./src/*\"] } } }",
    )
    .cjs("defprop", &[("index.js", DEFPROP)])
    .esm(
        "uikit",
        &[
            ("index.js", "import \"./style.css\";\nexport const ok = 1;\n"),
            ("style.css", "@font-face { src: url(./f.woff2) }\n"),
            ("f.woff2", "font"),
        ],
    )
    .write("src/aliased.js", "import { greet } from \"defprop\";\nexport const v = greet(\"z\");\n")
    .write(
        "entry.js",
        "import { v } from \"@/aliased\";\nimport { ok } from \"uikit\";\nexport const out = v + ok;\n",
    );
    let meta = app
        .optimize(OptimizeInput {
            entries: app.entry("entry.js"),
            ..Default::default()
        })
        .await;
    assert_eq!(
        keys(&meta),
        ["defprop", "uikit"],
        "defprop is reached only through the alias"
    );
    let uikit = app.read_out(&meta["uikit"].file);
    assert!(
        uikit.contains("import \"/node_modules/uikit/style.css\""),
        "{uikit}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_queried_specifier_is_never_prebundled() {
    let app = App::new();
    app.cjs("plaincjs", &[("index.js", "exports.a = 1;\n")])
        .esm("wk", &[("index.js", "export default 1;\n"), ("worker.js", "self.onmessage = () => {};\n")])
        .write(
            "entry.js",
            "import { a } from \"plaincjs\";\nimport W from \"wk/worker.js?worker\";\nexport const out = a + typeof W;\n",
        );
    let meta = app
        .optimize(OptimizeInput {
            entries: app.entry("entry.js"),
            ..Default::default()
        })
        .await;
    assert_eq!(keys(&meta), ["plaincjs"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn no_discovery_leaves_only_the_include_list() {
    let app = App::new();
    app.cjs("plaincjs", &[("index.js", "exports.a = 1;\n")])
        .cjs("other", &[("index.js", "exports.b = 2;\n")])
        .write("entry.js", "import { a } from \"plaincjs\";\nimport { b } from \"other\";\nexport const out = a + b;\n");
    let base = OptimizeInput {
        entries: app.entry("entry.js"),
        ..Default::default()
    };
    assert_eq!(
        keys(&app.optimize(base.clone()).await),
        ["other", "plaincjs"]
    );
    let off = OptimizeInput {
        no_discovery: Some(true),
        ..base.clone()
    };
    assert!(app.optimize(off.clone()).await.is_empty());
    let included = OptimizeInput {
        include: strings(&["plaincjs"]),
        ..off
    };
    assert_eq!(keys(&app.optimize(included).await), ["plaincjs"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn include_globs_expand_like_vite_and_needs_interop_forces() {
    let app = App::new();
    app.cjs(
        "plainglob",
        &[
            ("index.js", "exports.root = 1;\n"),
            ("alpha.js", "exports.alpha = 1;\n"),
            ("beta.js", "exports.beta = 2;\n"),
        ],
    )
    .pkg(
        "exportsglob",
        "dist/index.js",
        json!({ "exports": { ".": "./dist/index.js", "./icons/*": "./dist/icons/*.js", "./internal": null } }),
        &[
            ("dist/index.js", "exports.idx = 1;\n"),
            ("dist/icons/sun.js", "exports.sun = 1;\n"),
            ("dist/icons/moon.js", "exports.moon = 1;\n"),
        ],
    )
    .esm("esmlib", &[("index.js", "export const named = 1;\nexport default { named };\n")])
    .write("entry.js", "export const out = 1;\n");
    let run = |include: &[&str], needs_interop: &[&str]| OptimizeInput {
        entries: app.entry("entry.js"),
        include: strings(include),
        needs_interop: strings(needs_interop),
        ..Default::default()
    };
    let globbed = app
        .optimize(run(&["plainglob/*.js", "exportsglob/icons/*"], &[]))
        .await;
    assert_eq!(
        keys(&globbed),
        [
            "exportsglob",
            "exportsglob/icons/moon",
            "exportsglob/icons/sun",
            "plainglob",
            "plainglob/alpha.js",
            "plainglob/beta.js",
            "plainglob/index.js",
        ]
    );
    assert!(!app.optimize(run(&["esmlib"], &[])).await["esmlib"].needs_interop);
    assert!(app.optimize(run(&["esmlib"], &["esmlib"])).await["esmlib"].needs_interop);
}

#[tokio::test(flavor = "multi_thread")]
async fn dedupe_bundles_the_root_copy_and_nested_includes_the_nested_one() {
    let app = App::new();
    app.esm(
        "shared",
        &[("index.js", "export const copy = \"ROOT_COPY\";\n")],
    )
    .esm(
        "consumer",
        &[
            (
                "index.js",
                "import { copy } from \"shared\";\nexport const via = copy;\n",
            ),
            (
                "node_modules/shared/package.json",
                r#"{"name":"shared","type":"module","main":"index.js"}"#,
            ),
            (
                "node_modules/shared/index.js",
                "export const copy = \"NESTED_COPY\";\n",
            ),
        ],
    )
    .write(
        "src/pages/home.js",
        "import { via } from \"consumer\";\nexport const out = via;\n",
    );
    let run = |include: &[&str], dedupe: &[&str]| OptimizeInput {
        entries: app.entry("src/pages/home.js"),
        include: strings(include),
        dedupe: strings(dedupe),
        no_discovery: Some(true),
        ..Default::default()
    };
    let plain = app.optimize(run(&["consumer"], &[])).await;
    assert!(app
        .read_out(&plain["consumer"].file)
        .contains("NESTED_COPY"));
    let deduped = app.optimize(run(&["consumer"], &["shared"])).await;
    let code = app.read_out(&deduped["consumer"].file);
    assert!(
        code.contains("ROOT_COPY") && !code.contains("NESTED_COPY"),
        "{code}"
    );
    let nested = app.optimize(run(&["consumer > shared"], &[])).await;
    assert_eq!(keys(&nested), ["shared"]);
    assert!(app.read_out(&nested["shared"].file).contains("NESTED_COPY"));
    let globbed = app
        .optimize(OptimizeInput {
            entries: strings(&["src/**/*.js"]),
            ..Default::default()
        })
        .await;
    assert_eq!(keys(&globbed), ["consumer"], "a glob entry is scanned");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_commonjs_react_dep_gets_a_named_export_facade() {
    let app = App::new();
    app.cjs(
        "react",
        &[(
            "index.js",
            "exports.useState = function () { return \"state\"; };\n",
        )],
    )
    .write(
        "entry.js",
        "import { useState } from \"react\";\nexport const out = useState();\n",
    );
    let input = OptimizeInput {
        entries: app.entry("entry.js"),
        ..Default::default()
    };
    let pre = app.prebundle(&input, &[]);
    let scanned = pre.scan(None).await;
    let planned = pre.provisional(&pre.plan(&scanned, false));
    assert_eq!(
        planned["react"].file, "react__oj_named.mjs",
        "the plan names the facade up front"
    );
    let meta = pre.bundle(&pre.plan(&scanned, true)).await.unwrap();
    assert_eq!(meta["react"].file, "react__oj_named.mjs");
    assert!(!meta["react"].needs_interop, "the facade links as ESM");
    let facade = app.read_out(&meta["react"].file);
    assert!(
        facade.contains("export const { useState } = __m;"),
        "{facade}"
    );
    assert!(
        facade.contains("import __m from \"./react.mjs\";"),
        "{facade}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn what_a_bundle_keeps_external_is_a_url_oj_serves() {
    let app = App::new();
    app.esm(
        "styled",
        &[
            (
                "index.js",
                "import \"./s.css\";\nexport const wasm = new URL(\"./w.wasm\", import.meta.url).href;\nexport const remote = new URL(\"https://example.com/x.png\", import.meta.url).href;\nexport { x } from \"excluded\";\n",
            ),
            ("s.css", "a{}"),
            ("w.wasm", ""),
        ],
    )
    .esm("excluded", &[("index.js", "export const x = 1;\n")])
    .cjs("usesfs", &[("index.js", "exports.read = require(\"fs\").readFileSync;\n")])
    .pkg(
        "peery",
        "index.js",
        json!({
            "type": "module",
            "peerDependencies": { "missing-peer": "*" },
            "peerDependenciesMeta": { "missing-peer": { "optional": true } },
        }),
        &[("index.js", "import m from \"missing-peer\";\nexport const p = m;\n")],
    )
    .write(
        "entry.js",
        "import { wasm, x } from \"styled\";\nimport { read } from \"usesfs\";\nimport { p } from \"peery\";\nexport default [wasm, x, read, p];\n",
    );
    let meta = app
        .optimize(OptimizeInput {
            entries: app.entry("entry.js"),
            exclude: strings(&["excluded"]),
            ..Default::default()
        })
        .await;
    assert_eq!(keys(&meta), ["peery", "styled", "usesfs"]);
    let styled = app.read_out(&meta["styled"].file);
    assert!(
        styled.contains("import \"/node_modules/styled/s.css\";"),
        "{styled}"
    );
    assert!(
        styled.contains("from \"/node_modules/excluded/index.js\""),
        "{styled}"
    );
    assert!(
        styled.contains("\"/node_modules/styled/w.wasm\", import.meta.url)"),
        "{styled}"
    );
    assert!(
        styled.contains("new URL(\"https://example.com/x.png\", import.meta.url)"),
        "{styled}"
    );
    let usesfs = app.read_out(&meta["usesfs"].file);
    assert!(
        usesfs.contains("has been externalized for browser compatibility"),
        "{usesfs}"
    );
    assert!(
        !usesfs.contains("from \"fs\"") && !usesfs.contains("require(\"fs\")"),
        "{usesfs}"
    );
    let peery = app.read_out(&meta["peery"].file);
    assert!(
        peery.contains(r#"Could not resolve \"missing-peer\" imported by \"peery\""#),
        "{peery}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn esm_entries_need_no_interop_and_linked_packages_are_crawled() {
    let app = App::new();
    app.esm("esmdefault", &[("index.js", "export default function hi() { return 1; }\n")])
        .esm(
            "reexports",
            &[("index.js", "export * from \"./inner.js\";\n"), ("inner.js", "export const z = 1;\n")],
        )
        .cjs("deep", &[("index.js", "exports.d = 1;\n")])
        .write("packages/linked/package.json", r#"{"name":"linked","type":"module","main":"index.js"}"#)
        .write("packages/linked/index.js", "export { d } from \"deep\";\n")
        .write(
            "entry.js",
            "import hi from \"esmdefault\";\nimport { z } from \"reexports\";\nimport { d } from \"linked\";\nexport default [hi, z, d];\n",
        );
    symlink(
        &app.root().join("packages/linked"),
        &app.root().join("node_modules/linked"),
    );
    let meta = app
        .optimize(OptimizeInput {
            entries: app.entry("entry.js"),
            ..Default::default()
        })
        .await;
    assert_eq!(keys(&meta), ["deep", "esmdefault", "reexports"]);
    assert!(!meta["esmdefault"].needs_interop);
    assert!(!meta["reexports"].needs_interop);
    assert!(meta["deep"].needs_interop);
}

#[tokio::test(flavor = "multi_thread")]
async fn rolldown_options_and_node_env_reach_the_bundle() {
    let app = App::new();
    app.esm(
        "envy",
        &[(
            "index.js",
            "export const env = process.env.NODE_ENV;\nexport const flag = __FLAG__;\n",
        )],
    )
    .write(
        "index.html",
        "<script type=\"module\" src=\"/main.js\"></script>",
    )
    .write(
        "main.js",
        "import { env, flag } from \"envy\";\nconsole.log(env, flag);\n",
    );
    let meta = app
        .optimize(OptimizeInput {
            node_env: "production".into(),
            rolldown_options: Some(
                json!({ "transform": { "define": { "__FLAG__": "\"from-rolldown-options\"" } } }),
            ),
            ..Default::default()
        })
        .await;
    let code = app.read_out(&meta["envy"].file);
    assert!(code.contains("\"production\""), "{code}");
    assert!(code.contains("from-rolldown-options"), "{code}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_require_of_an_excluded_dep_imports_it_through_a_facade() {
    let app = App::new();
    app.cjs(
        "cjsdep",
        &[(
            "index.js",
            "const e = require(\"excluded\");\nmodule.exports = { v: e.v };\n",
        )],
    )
    .cjs("excluded", &[("index.js", "exports.v = 1;\n")])
    .write(
        "index.html",
        "<script type=\"module\" src=\"/main.js\"></script>",
    )
    .write("main.js", "import x from \"cjsdep\";\nconsole.log(x);\n");
    let meta = app
        .optimize(OptimizeInput {
            exclude: strings(&["excluded"]),
            ..Default::default()
        })
        .await;
    assert_eq!(keys(&meta), ["cjsdep"]);
    let code = app.read_out(&meta["cjsdep"].file);
    assert!(
        !code.contains("__require("),
        "a browser bundle cannot require() at run time:\n{code}"
    );
    let import =
        regex::Regex::new(r#"import \* as \w+ from "/node_modules/excluded/index\.js""#).unwrap();
    assert!(import.is_match(&code), "{code}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dep_shipping_jsx_in_a_js_file_still_prebundles() {
    let app = App::new();
    app.esm(
        "jsxdep",
        &[(
            "index.js",
            "export default function J() { return <div>hi</div>; }\n",
        )],
    )
    .esm("plain", &[("index.js", "export const p = 1;\n")])
    .write(
        "index.html",
        "<script type=\"module\" src=\"/main.js\"></script>",
    )
    .write(
        "main.js",
        "import J from \"jsxdep\";\nimport { p } from \"plain\";\nconsole.log(J, p);\n",
    );
    assert_eq!(
        keys(&app.optimize(OptimizeInput::default()).await),
        ["jsxdep", "plain"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn non_js_resolutions_and_vites_asset_list_stay_external() {
    let app = App::new();
    app.pkg("theme", "theme.css", json!({}), &[("theme.css", "body { color: red; }\n")])
        .esm(
            "widget",
            &[
                (
                    "index.js",
                    "import \"theme\";\nimport captions from \"./captions.vtt\";\nimport notes from \"./notes.txt\";\nexport const w = [captions, notes];\n",
                ),
                ("captions.vtt", "WEBVTT\n"),
                ("notes.txt", "notes\n"),
            ],
        )
        .write("index.html", "<script type=\"module\" src=\"/main.js\"></script>")
        .write("main.js", "import { w } from \"widget\";\nconsole.log(w);\n");
    let meta = app.optimize(OptimizeInput::default()).await;
    assert_eq!(keys(&meta), ["widget"]);
    let code = app.read_out(&meta["widget"].file);
    assert!(
        code.contains("import \"/node_modules/theme/theme.css\""),
        "{code}"
    );
    assert!(
        code.contains("from \"/node_modules/widget/captions.vtt\""),
        "{code}"
    );
    assert!(
        code.contains("from \"/node_modules/widget/notes.txt\""),
        "{code}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn new_url_inside_a_string_comment_or_vite_ignore_is_left_alone() {
    let app = App::new();
    app.esm(
        "urls",
        &[
            (
                "index.js",
                concat!(
                    "export const re = /['\"]/.test(\"q\");\n",
                    "export function f(s) { if (s) /['\"}]/.test(s); return `${\"}\"}`; }\n",
                    "export const real = new URL(\"./data.bin\", import.meta.url).href;\n",
                    "export const doc = \"example: new URL('./fake.bin', import.meta.url)\";\n",
                    "// new URL('./comment.bin', import.meta.url)\n",
                    "export const skipped = new URL(/* @vite-ignore */ \"./skipped.bin\", import.meta.url).href;\n",
                ),
            ),
            ("data.bin", "x"),
            ("skipped.bin", "x"),
        ],
    )
    .write("index.html", "<script type=\"module\" src=\"/main.js\"></script>")
    .write("main.js", "import { real } from \"urls\";\nconsole.log(real);\n");
    let meta = app.optimize(OptimizeInput::default()).await;
    let code = app.read_out(&meta["urls"].file);
    assert!(
        code.contains("\"/node_modules/urls/data.bin\", import.meta.url)"),
        "{code}"
    );
    assert!(
        code.contains("new URL('./fake.bin', import.meta.url)"),
        "{code}"
    );
    assert!(!code.contains("/node_modules/urls/fake.bin"), "{code}");
    assert!(!code.contains("/node_modules/urls/comment.bin"), "{code}");
    assert!(
        code.contains("./skipped.bin") && !code.contains("/node_modules/urls/skipped.bin"),
        "{code}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deduped_dep_reached_from_a_linked_package_pins_the_root_copy() {
    let app = App::new();
    app.esm(
        "shared",
        &[("index.js", "export const copy = \"ROOT_COPY\";\n")],
    )
    .write(
        "lib/linked-lib/package.json",
        r#"{"name":"linked-lib","type":"module","main":"index.js"}"#,
    )
    .write(
        "lib/linked-lib/index.js",
        "import { copy } from \"shared\";\nexport const via = copy;\n",
    )
    .write(
        "lib/linked-lib/node_modules/shared/package.json",
        r#"{"name":"shared","type":"module","main":"index.js"}"#,
    )
    .write(
        "lib/linked-lib/node_modules/shared/index.js",
        "export const copy = \"NESTED_COPY\";\n",
    )
    .write(
        "index.html",
        "<script type=\"module\" src=\"/main.js\"></script>",
    )
    .write(
        "main.js",
        "import { via } from \"linked-lib\";\nconsole.log(via);\n",
    );
    symlink(
        &app.root().join("lib/linked-lib"),
        &app.root().join("node_modules/linked-lib"),
    );
    let meta = app
        .optimize(OptimizeInput {
            dedupe: strings(&["shared"]),
            ..Default::default()
        })
        .await;
    let code = app.read_out(&meta["shared"].file);
    assert!(
        code.contains("ROOT_COPY") && !code.contains("NESTED_COPY"),
        "{code}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unresolved_include_is_dropped_and_the_implicit_one_is_quiet() {
    let app = App::new();
    app.write("entry.js", "export const x = 1;\n");
    let input = OptimizeInput {
        entries: app.entry("entry.js"),
        include: strings(&["ghost-pkg"]),
        ..Default::default()
    };
    let pre = app.prebundle(&input, &["react/jsx-dev-runtime"]);
    let scanned = pre.scan(None).await;
    assert!(scanned.is_empty());
    let plan = pre.plan(&scanned, true);
    assert!(plan.names.is_empty(), "{plan:?}");
    assert!(pre.warn_includes.contains("ghost-pkg"));
    assert!(!pre.warn_includes.contains("react/jsx-dev-runtime"));
}

#[tokio::test(flavor = "multi_thread")]
async fn entries_follow_vites_compute_entries() {
    let app = App::new();
    for dep in [
        "fromroot",
        "frompage",
        "frominline",
        "fromdist",
        "frominput",
    ] {
        app.esm(dep, &[("index.js", "export const v = 1;\n")]);
    }
    app.write(
        "index.html",
        "<script type=\"module\" src=\"/main.js\"></script>",
    )
    .write(
        "main.js",
        "import { v } from \"fromroot\";\nconsole.log(v);\n",
    )
    .write(
        "pages/about.html",
        concat!(
            "<script type=\"module\" src=\"./about.js\"></script>\n",
            "<script type=\"module\">import { v } from \"frominline\";\nconsole.log(v);</script>\n",
            "<!-- <script type=\"module\">import { v } from \"fromdist\";</script> -->\n",
            "<script>var legacy = 1;</script>",
        ),
    )
    .write(
        "pages/about.js",
        "import { v } from \"frompage\";\nconsole.log(v);\n",
    )
    .write(
        "dist/index.html",
        "<script type=\"module\">import { v } from \"fromdist\";</script>",
    )
    .write(
        "src/entry.js",
        "import { v } from \"frominput\";\nconsole.log(v);\n",
    );
    let mut found = app.scan(OptimizeInput::default()).await;
    found.sort();
    assert_eq!(
        found,
        ["frominline", "frompage", "fromroot"],
        "every html file but the build output"
    );
    let from_input = app
        .scan(OptimizeInput {
            build_inputs: strings(&["src/entry.js"]),
            ..Default::default()
        })
        .await;
    assert_eq!(
        from_input,
        ["frominput"],
        "build inputs win over the html glob"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn import_meta_glob_targets_are_crawled() {
    let app = App::new();
    for dep in [
        "frompage",
        "fromadmin",
        "fromskipped",
        "frominline",
        "fromsibling",
        "fromclass",
        "fromother",
    ] {
        app.esm(dep, &[("index.js", "export const v = 1;\n")]);
    }
    app.write(
        "index.html",
        concat!(
            "<script type=\"module\" src=\"/main.ts\"></script>\n",
            "<script type=\"module\" src=\"/routes/[id]/page.ts\"></script>\n",
            "<script type=\"module\">const w = import.meta.glob(\"/inline/*.js\");\nconsole.log(w);</script>",
        ),
    )
    .write(
        "main.ts",
        concat!(
            "// const dead = import.meta.glob(\"./pages/skip.js\");\n",
            "const pages = import.meta.glob<Record<string, unknown>>([\"./pages/**/*.js\", \"!**/skip.js\"], { eager: true });\n",
            "const admin = import.meta.glob(\"/src/admin/*.js\");\n",
            "export default [pages, admin];\n",
        ),
    )
    .write("pages/home.js", "import { v } from \"frompage\";\nconsole.log(v);\n")
    .write("pages/skip.js", "import { v } from \"fromskipped\";\nconsole.log(v);\n")
    .write("src/admin/panel.js", "import { v } from \"fromadmin\";\nconsole.log(v);\n")
    .write("inline/widget.js", "import { v } from \"frominline\";\nconsole.log(v);\n")
    .write(
        "routes/[id]/page.ts",
        "const near = import.meta.glob(\"./*.js\");\nconst picked = import.meta.glob(\"/parts/[ab]/{x,y}.js\");\nexport default [near, picked];\n",
    )
    .write("routes/[id]/sibling.js", "import { v } from \"fromsibling\";\nconsole.log(v);\n")
    .write("parts/a/x.js", "import { v } from \"fromclass\";\nconsole.log(v);\n")
    .write("parts/c/x.js", "import { v } from \"fromother\";\nconsole.log(v);\n");
    let mut found = app.scan(OptimizeInput::default()).await;
    found.sort();
    assert_eq!(
        found,
        [
            "fromadmin",
            "fromclass",
            "frominline",
            "frompage",
            "fromsibling"
        ],
        "glob-matched modules crawled; negated, commented and unmatched ones not"
    );
}

/// Every emitted file, so an assertion does not depend on which chunk the
/// code landed in.
fn emitted(app: &App) -> String {
    let mut code = String::new();
    for entry in std::fs::read_dir(app.out_dir()).unwrap().flatten() {
        if entry.path().extension().is_some_and(|e| e == "mjs") {
            code.push_str(&std::fs::read_to_string(entry.path()).unwrap());
        }
    }
    code
}

// optimizeDeps.esbuildOptions is deprecated but still honored, the way Vite 8
// maps it onto the rolldown run; keys it never mapped are left out.
#[tokio::test(flavor = "multi_thread")]
async fn legacy_esbuild_options_reach_the_bundle() {
    let app = App::new();
    app.esm(
        "flagdep",
        &[(
            "index.js",
            "export const flag = __OJ_FLAG__;\nexport const pow = globalThis.__oj_base__ ** 10;\nexport const mode = process.env.NODE_ENV;\n",
        )],
    )
    .write("entry.js", "import { flag, pow, mode } from \"flagdep\";\nexport const out = flag + pow + mode;\n");
    let meta = app
        .optimize(OptimizeInput {
            entries: app.entry("entry.js"),
            bundler_options: Some(json!({
                "define": { "__OJ_FLAG__": "\"injected-by-define\"" },
                "target": "es2015",
                "output": { "format": "cjs" },
                "transform": { "define": { "nope": "1" } },
            })),
            ..Default::default()
        })
        .await;
    assert_eq!(keys(&meta), ["flagdep"]);
    let code = emitted(&app);
    assert!(
        code.contains("injected-by-define") && !code.contains("__OJ_FLAG__"),
        "{code}"
    );
    assert!(
        !code.contains("** 10"),
        "es2015 lowers the ** operator:\n{code}"
    );
    assert!(
        code.contains("\"development\"") && !code.contains("process.env.NODE_ENV"),
        "{code}"
    );
}

/// Top-level `let`, `const` and `class` declarations across the emitted files:
/// the bindings that throw a TDZ ReferenceError when read before they run.
fn lexical_top_level_bindings(app: &App) -> usize {
    use oxc_ast::ast::{Declaration, Statement, VariableDeclarationKind};
    let lexical = |d: &Declaration| match d {
        Declaration::VariableDeclaration(v) => v.kind != VariableDeclarationKind::Var,
        Declaration::ClassDeclaration(_) => true,
        _ => false,
    };
    let mut count = 0;
    for entry in std::fs::read_dir(app.out_dir()).unwrap().flatten() {
        if entry.path().extension().is_none_or(|e| e != "mjs") {
            continue;
        }
        let code = std::fs::read_to_string(entry.path()).unwrap();
        let allocator = oxc_allocator::Allocator::default();
        let parsed =
            oxc_parser::Parser::new(&allocator, &code, oxc_span::SourceType::mjs()).parse();
        assert!(!parsed.diagnostics.has_errors(), "{code}");
        count += parsed
            .program
            .body
            .iter()
            .filter(|s| match s {
                Statement::VariableDeclaration(v) => v.kind != VariableDeclarationKind::Var,
                Statement::ClassDeclaration(_) => true,
                Statement::ExportDeclaration(e) => lexical(&e.declaration),
                _ => false,
            })
            .count();
    }
    count
}

// Vite's optimizer sets `output.topLevelVar ??= true`, keeping the hoisting
// esbuild gave every pre-bundle: a dep that reads a binding before its
// declaration (here through a cycle) gets `undefined` instead of throwing.
#[tokio::test(flavor = "multi_thread")]
async fn pre_bundled_top_level_bindings_hoist_like_vite() {
    let hoisty = |app: &App| {
        app.esm(
            "hoisty",
            &[
                (
                    "index.js",
                    "import { label } from \"./label.js\";\nexport const config = { name: \"cfg\" };\nexport class Store {}\nexport function getConfig() { return config; }\nexport { label };\n",
                ),
                (
                    "label.js",
                    "import { getConfig } from \"./index.js\";\nexport let label = String(getConfig());\n",
                ),
            ],
        )
        .write(
            "entry.js",
            "import { label, Store } from \"hoisty\";\nexport const out = [label, Store];\n",
        );
    };

    let app = App::new();
    hoisty(&app);
    let meta = app
        .optimize(OptimizeInput {
            entries: app.entry("entry.js"),
            ..Default::default()
        })
        .await;
    assert_eq!(keys(&meta), ["hoisty"]);
    assert_eq!(lexical_top_level_bindings(&app), 0, "{}", emitted(&app));

    // `false` is Vite's opt-out, and keeps the declarations as written.
    let app = App::new();
    hoisty(&app);
    app.optimize(OptimizeInput {
        entries: app.entry("entry.js"),
        rolldown_options: Some(json!({ "output": { "topLevelVar": false } })),
        ..Default::default()
    })
    .await;
    assert!(lexical_top_level_bindings(&app) > 0, "{}", emitted(&app));
}

// A root or package path with glob syntax in it (`[v2]`, `{work}`) is a
// literal prefix, as tinyglobby's `cwd` is: the html entries and an include
// glob still find their files.
#[tokio::test(flavor = "multi_thread")]
async fn glob_syntax_in_the_root_path_is_literal() {
    let app = App::in_dir("app [v2] {work}");
    app.esm("fromhtml", &[("index.js", "export const h = 1;\n")])
        .cjs(
            "parts",
            &[
                ("index.js", "exports.a = 1;\n"),
                ("extra.js", "exports.b = 2;\n"),
            ],
        )
        .write(
            "index.html",
            "<script type=\"module\" src=\"/main.js\"></script>",
        )
        .write(
            "main.js",
            "import { h } from \"fromhtml\";\nconsole.log(h);\n",
        );
    let found = app.scan(OptimizeInput::default()).await;
    assert_eq!(found, ["fromhtml"], "the html glob found the entry");
    let meta = app
        .optimize(OptimizeInput {
            include: strings(&["parts/*.js"]),
            no_discovery: Some(true),
            ..Default::default()
        })
        .await;
    assert_eq!(keys(&meta), ["parts", "parts/extra.js", "parts/index.js"]);
}

#[test]
fn rolldown_options_map_onto_the_run() {
    let input = OptimizeInput {
        rolldown_options: Some(json!({
            "resolve": { "conditionNames": ["custom"], "bogus": 1 },
            "moduleTypes": { ".svg": "text" },
            "experimentalThing": true,
        })),
        ..Default::default()
    };
    let options = super::DepOptions::new(&input).bundler_options(false);
    let resolve = options.resolve.unwrap();
    assert_eq!(resolve.condition_names, Some(vec!["custom".to_string()]));
    assert!(options.module_types.unwrap().contains_key(".svg"));
    assert!(options.define.unwrap().contains_key("process.env.NODE_ENV"));

    // `keepNames` is an output option in rolldown (and Vite's docs).
    let input = OptimizeInput {
        rolldown_options: Some(json!({ "output": { "keepNames": true } })),
        ..Default::default()
    };
    assert_eq!(
        super::DepOptions::new(&input)
            .bundler_options(false)
            .keep_names,
        Some(true)
    );

    // The scan writes nothing, so only the pre-bundle takes topLevelVar.
    let options = super::DepOptions::new(&OptimizeInput::default());
    assert_eq!(options.bundler_options(false).top_level_var, Some(true));
    assert_eq!(options.bundler_options(true).top_level_var, None);
}

fn symlink(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(target, link).unwrap();
}

// Rolldown hands the scan a whole crawl's resolves at once. The plugin host
// takes at most SCAN_HOST_TURNS of them at a time, so a dev request's hook
// never queues behind the whole crawl (issue #332), while enough overlap
// that a plugin awaiting its own work keeps the crawl near its pace.
#[tokio::test(flavor = "multi_thread")]
async fn the_scan_bounds_the_resolves_waiting_in_the_plugin_host() {
    let app = App::new();
    let mut main = String::new();
    for i in 0..200 {
        app.write(
            &format!("src/m{i}.js"),
            &format!("export const v{i} = {i};\n"),
        );
        main.push_str(&format!("import \"./src/m{i}.js\";\n"));
    }
    app.write("main.js", &main);
    let peak = app.root().join("peak.txt");
    app.write(
        "oj.plugins.mjs",
        &format!(
            r#"import {{ writeFileSync }} from "node:fs";
let inFlight = 0;
let peak = 0;
export default [{{
  name: "in-flight",
  async resolveId(id, importer, options) {{
    if (!options?.scan) return null;
    peak = Math.max(peak, ++inFlight);
    writeFileSync({peak:?}, String(peak));
    await new Promise((r) => setTimeout(r, 5));
    inFlight--;
    return null;
  }},
}}];
"#
        ),
    );
    let config = json!({
        "config": { "root": app.root().display().to_string() },
        "env": { "command": "serve", "mode": "development" },
    })
    .to_string();
    let host = crate::plugins::PluginHost::spawn(
        &app.root(),
        &app.root().join("oj.plugins.mjs"),
        &config,
        None,
    )
    .await
    .expect("the plugin host spawns");
    let input = OptimizeInput {
        entries: app.entry("main.js"),
        ..Default::default()
    };
    let pre = app.prebundle(&input, &[]);
    pre.scan(Some(&host)).await;
    host.shutdown();
    let peak: usize = std::fs::read_to_string(&peak).unwrap().parse().unwrap();
    assert!(
        peak > 1 && peak <= super::scan::SCAN_HOST_TURNS,
        "scan resolves in the plugin host at once: {peak}"
    );
}

// Vite's ctx.resolve passes the chain's `scan` on (`scan: this._scan`,
// pluginContainer.ts): a plugin that this.resolves during the scan reaches
// the other plugins as a scan resolve too.
#[tokio::test(flavor = "multi_thread")]
async fn a_nested_resolve_in_the_scan_is_a_scan_resolve() {
    let app = App::new();
    app.write(
        "oj.plugins.mjs",
        r#"export default [
  {
    name: "outer",
    async resolveId(id, importer, options) {
      if (id !== "probe") return null;
      return this.resolve("nested", importer, { skipSelf: true });
    },
  },
  {
    name: "inner",
    resolveId(id, importer, options) {
      if (id === "nested") return { id: "/scan-" + options.scan, external: true };
      return null;
    },
  },
];
"#,
    );
    let config = json!({
        "config": { "root": app.root().display().to_string() },
        "env": { "command": "serve", "mode": "development" },
    })
    .to_string();
    let host = crate::plugins::PluginHost::spawn(
        &app.root(),
        &app.root().join("oj.plugins.mjs"),
        &config,
        None,
    )
    .await
    .expect("the plugin host spawns");
    let importer = app.root().join("main.js").to_string_lossy().into_owned();
    let scanned = host.scan_resolve_id("probe", &importer).await;
    let served = host.resolve_id("probe", &importer).await;
    host.shutdown();
    assert_eq!(
        scanned.unwrap().map(|r| r.id),
        Some("/scan-true".to_string())
    );
    assert_eq!(served.unwrap().as_deref(), Some("/scan-false"));
}
