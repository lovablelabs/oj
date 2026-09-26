// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Boundary behaviour: the shapes that are unusual but entirely valid, where a
//! pipeline built around the common case quietly does the wrong thing. Empty
//! files, byte-order marks, syntax that only one extension allows, and the
//! points where dev and prod are supposed to differ.

use std::path::Path;

use oj_compiler::{
    bundle, cjs, compile, compile_module, exports, json, CompileError, CompileOptions,
};

fn dev(path: &str, source: &str) -> Result<oj_compiler::CompileOutput, CompileError> {
    compile(Path::new(path), source, &CompileOptions::dev())
}

fn prod(path: &str, source: &str) -> Result<oj_compiler::CompileOutput, CompileError> {
    compile(Path::new(path), source, &CompileOptions::prod())
}

#[test]
fn empty_and_trivial_modules_compile_to_nothing() {
    for source in ["", " ", "\n\n\n", "\u{feff}"] {
        let out = dev("/src/empty.ts", source).unwrap();
        assert!(
            out.code.trim().is_empty(),
            "{source:?} produced {:?}",
            out.code
        );
        assert!(out.imports.is_empty());
        assert!(!out.is_refresh_boundary);
        assert!(exports(source, Path::new("/src/empty.ts")).is_empty());
    }
}

#[test]
fn comments_are_carried_through_to_the_output() {
    for source in ["// just a comment", "/* block */", "/** @license MIT */"] {
        let out = dev("/src/comment.ts", source).unwrap();
        assert!(
            out.code.contains(source.trim()),
            "{source:?} -> {:?}",
            out.code
        );
        assert!(out.imports.is_empty());
    }
}

#[test]
fn a_byte_order_mark_does_not_become_part_of_the_first_statement() {
    let out = dev("/src/App.ts", "\u{feff}export const a = 1;").unwrap();
    assert!(out.code.contains("export const a = 1"), "{}", out.code);
    assert!(!out.code.starts_with('\u{feff}'), "BOM copied into output");
    assert_eq!(
        exports("\u{feff}export const a = 1;", Path::new("/src/App.ts")),
        ["a"]
    );
}

#[test]
fn a_shebang_is_preserved_and_a_hash_bang_elsewhere_is_an_error() {
    let out = dev("/src/cli.ts", "#!/usr/bin/env node\nexport const a = 1;").unwrap();
    assert!(out.code.starts_with("#!"), "shebang dropped: {}", out.code);
    assert!(dev("/src/cli.ts", "const a = 1;\n#!/usr/bin/env node").is_err());
}

#[test]
fn crlf_and_lone_cr_line_endings_are_accepted() {
    for (label, source) in [
        ("crlf", "export const a = 1;\r\nexport const b = 2;\r\n"),
        ("cr", "export const a = 1;\rexport const b = 2;\r"),
        ("mixed", "export const a = 1;\r\nexport const b = 2;\n"),
        ("no trailing newline", "export const a = 1;"),
    ] {
        let mut names = exports(source, Path::new("/src/App.ts"));
        names.sort();
        let expected: &[&str] = if source.contains("b = 2") {
            &["a", "b"]
        } else {
            &["a"]
        };
        assert_eq!(names, expected, "{label}");
        assert!(dev("/src/App.ts", source).is_ok(), "{label}");
    }
}

#[test]
fn typescript_enums_are_lowered_rather_than_dropped() {
    // The TypeScript transform needs `with_enum_eval`; without it, an `enum`
    // aborts the process instead of compiling.
    let out = dev("/src/App.ts", "export enum Direction { Up, Down }").unwrap();
    assert!(out.code.contains("Direction"), "{}", out.code);
    assert!(out.code.contains("\"Up\""), "reverse mapping: {}", out.code);
    assert!(!out.code.contains("enum "), "not lowered: {}", out.code);

    let out = dev("/src/App.ts", "export const enum Flag { On = 1 }").unwrap();
    assert!(out.code.contains("Flag"), "{}", out.code);

    let out = dev(
        "/src/App.ts",
        "enum S { A = \"a\", B = \"b\" }\nexport const s = S.A;",
    )
    .unwrap();
    assert!(out.code.contains("\"a\""), "{}", out.code);

    // Ambient declarations still vanish.
    let out = dev("/src/App.ts", "declare enum D { A }").unwrap();
    assert!(out.code.trim().is_empty(), "{}", out.code);

    // And in bundle mode, which runs the same transform through its own path.
    let mut resolve = |spec: &str| Some(format!("/resolved/{spec}"));
    let factory = bundle::compile_factory(
        Path::new("/src/App.ts"),
        "/src/App.ts",
        "export enum Direction { Up, Down }",
        &mut resolve,
    )
    .unwrap();
    assert!(factory.code.contains("Direction"), "{}", factory.code);
}

#[test]
fn typescript_only_syntax_is_erased_everywhere_it_can_appear() {
    let source = "\
import type { Only } from \"./types\";
export type { Only };
export interface I { a: string }
type Alias = I;
declare global { interface Window { x: number } }
declare const ambient: number;
export abstract class A<T extends object = {}> implements I {
  a!: string;
  private readonly p?: number;
  constructor(public q: number) { super(); }
  abstract m(): void;
  n<U>(x: U): U { return x as U; }
}
export const assertion = (<Alias>{}) satisfies I;
export const nonNull = ambient!;
function overloaded(a: string): void;
function overloaded(a: number): void;
function overloaded(a: unknown): void {}
export { overloaded };
export const generic = <T,>(x: T) => x;
namespace NS { export const inner = 1; }
export const used = NS.inner;
";
    let out = prod("/src/App.ts", source).unwrap();
    for leftover in [
        "interface",
        "declare",
        "abstract",
        "satisfies",
        " as U",
        "import type",
    ] {
        assert!(
            !out.code.contains(leftover),
            "{leftover:?} survived: {}",
            out.code
        );
    }
    // A type-only import is not a module edge.
    assert!(
        !out.imports.contains(&"./types".to_string()),
        "{:?}",
        out.imports
    );
}

#[test]
fn jsx_is_only_valid_where_the_extension_allows_it() {
    let jsx = "export const a = <div />;";
    assert!(dev("/src/App.tsx", jsx).is_ok());
    assert!(dev("/src/App.jsx", jsx).is_ok());
    // JSX in a plain `.js` file is a syntax error, as it is under Vite: the
    // file has to be named `.jsx`.
    assert!(dev("/src/App.js", jsx).is_err());
    // In `.ts`, `<div />` reads as a type assertion, so the JSX is an error too.
    assert!(dev("/src/App.ts", jsx).is_err());
}

#[test]
fn each_extension_gets_the_right_module_or_script_treatment() {
    // CommonJS syntax is accepted everywhere: `module.exports` is just a member
    // assignment, and oj decides how to treat it from the url, not the parse.
    assert!(dev("/src/a.cjs", "module.exports = 1;").is_ok());
    assert!(dev("/src/a.mjs", "export const a = 1;").is_ok());
    // `.mts`/`.cts` carry TypeScript.
    assert!(dev("/src/a.mts", "export const a: number = 1;").is_ok());
    assert!(dev("/src/a.cts", "const a: number = 1;").is_ok());
    // TypeScript syntax in a plain `.js` file is not.
    assert!(dev("/src/a.js", "const a: number = 1;").is_err());
    // Strict-mode-only errors such as `with` are not reported by the parser;
    // they are the browser's to report, like the other early errors above.
    assert!(dev("/src/a.mjs", "with (x) {}").is_ok());
}

#[test]
fn top_level_await_is_accepted_in_a_module() {
    let out = dev("/src/App.ts", "export const a = await Promise.resolve(1);").unwrap();
    assert!(out.code.contains("await"), "{}", out.code);
}

#[test]
fn every_export_form_is_reported_by_name() {
    let source = "\
export const named = 1;
export let mutable = 2;
export var legacy = 3;
export function fn() {}
export function* gen() {}
export async function afn() {}
export class Cls {}
export default class Def {}
const local = 4;
export { local, local as aliased, local as \"string name\" };
export * as namespaced from \"./m\";
export { other } from \"./n\";
export { other as renamed } from \"./n\";
";
    let mut names = exports(source, Path::new("/src/App.ts"));
    names.sort();
    assert_eq!(
        names,
        [
            "Cls",
            "afn",
            "aliased",
            "default",
            "fn",
            "gen",
            "legacy",
            "local",
            "mutable",
            "named",
            "namespaced",
            "other",
            "renamed",
            "string name",
        ]
    );
}

#[test]
fn export_star_without_a_name_contributes_no_names() {
    assert!(exports("export * from \"./m\";", Path::new("/src/App.ts")).is_empty());
}

#[test]
fn exports_of_an_unparsable_or_unknown_file_is_empty_not_a_panic() {
    assert!(exports("const a = (", Path::new("/src/App.ts")).is_empty());
    assert!(exports("export const a = 1;", Path::new("/src/App.css")).is_empty());
}

#[test]
fn dynamic_imports_are_tracked_separately_from_static_ones() {
    let source = "\
import statik from \"./statik\";
const lazy = () => import(\"./lazy\");
const nested = async () => (await import(\"./nested\")).default;
const dynamic = (n) => import(n);
export { statik, lazy, nested, dynamic };
";
    let mut rewriter = |spec: &str| Some(format!("/@id/{spec}"));
    let out = compile_module(
        Path::new("/src/App.ts"),
        source,
        &CompileOptions::dev(),
        Some(&mut rewriter),
    )
    .unwrap();
    assert_eq!(out.imports, vec!["/@id/./statik".to_string()]);
    assert_eq!(
        out.dynamic_imports,
        vec!["/@id/./lazy".to_string(), "/@id/./nested".to_string()],
        "a non-literal import() cannot be tracked"
    );
}

#[test]
fn dynamic_imports_are_only_collected_when_a_rewriter_is_present() {
    // The crawler passes a rewriter; a plain `compile` does not, and then only
    // static imports are known.
    let source = "const lazy = () => import(\"./lazy\");";
    let out = dev("/src/App.ts", source).unwrap();
    assert!(out.dynamic_imports.is_empty());
}

#[test]
fn refresh_instrumentation_only_happens_in_dev_and_only_for_components() {
    let component = "export function Counter() { return <div />; }";
    let dev_out = dev("/src/Counter.tsx", component).unwrap();
    assert!(dev_out.is_refresh_boundary, "{}", dev_out.code);
    assert!(dev_out.has_refresh_registrations());

    let prod_out = prod("/src/Counter.tsx", component).unwrap();
    assert!(!prod_out.is_refresh_boundary);
    assert!(!prod_out.code.contains("$RefreshReg$"), "{}", prod_out.code);

    // Not a component: no boundary, so a change forces a reload upstream.
    let plain = dev("/src/util.ts", "export const add = (a, b) => a + b;").unwrap();
    assert!(!plain.is_refresh_boundary);

    // Dev without refresh: the JSX dev runtime, but no registration.
    let no_refresh = compile(
        Path::new("/src/Counter.tsx"),
        component,
        &CompileOptions {
            dev: true,
            refresh: false,
            sourcemap: false,
            ssr: false,
            ..CompileOptions::dev()
        },
    )
    .unwrap();
    assert!(!no_refresh.is_refresh_boundary);
    assert!(!no_refresh.code.contains("$RefreshReg$"));
}

#[test]
fn a_source_map_is_omitted_when_a_module_gains_synthesized_nodes() {
    // Glob and dynamic-import-vars expansions splice generated spans, which the
    // source map builder cannot represent.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("locales")).unwrap();
    std::fs::write(dir.path().join("locales/en.json"), "{}").unwrap();
    let entry = dir.path().join("src.ts");

    let plain = compile(&entry, "export const a = 1;", &CompileOptions::dev()).unwrap();
    assert!(plain.map_data_url.is_some(), "a normal module has a map");

    let globbed = compile(
        &entry,
        "export const m = import.meta.glob(\"./locales/*.json\");",
        &CompileOptions::dev(),
    )
    .unwrap();
    assert!(
        globbed.map_data_url.is_none(),
        "a synthesized module must not claim a map"
    );
    assert_eq!(
        globbed.code_with_inline_map(),
        globbed.code,
        "no sourceMappingURL comment without a map"
    );
}

#[test]
fn sourcemap_can_be_turned_off_entirely() {
    let out = compile(
        Path::new("/src/App.ts"),
        "export const a = 1;",
        &CompileOptions {
            dev: true,
            refresh: true,
            sourcemap: false,
            ssr: false,
            ..CompileOptions::dev()
        },
    )
    .unwrap();
    assert!(out.map_data_url.is_none());
}

#[test]
fn a_json_module_with_no_keys_still_has_a_default_export() {
    for source in ["{}", "[]", "null", "0", "\"\""] {
        let esm = json::to_esm(source, "/data.json").unwrap();
        assert!(esm.contains("export default __oj_json"), "{source}: {esm}");
        assert_eq!(esm.matches("export const").count(), 0, "{source}: {esm}");
    }
}

#[test]
fn json_arrays_and_nested_objects_export_only_the_top_level_keys() {
    let esm = json::to_esm(r#"{"a":{"b":1},"list":[1,2]}"#, "/data.json").unwrap();
    assert!(esm.contains("export const a ="));
    assert!(esm.contains("export const list ="));
    assert_eq!(esm.matches("export const").count(), 2, "{esm}");
}

#[test]
fn a_cjs_dep_with_module_syntax_is_treated_as_esm() {
    let mut resolve = |spec: &str| Some(format!("/@id/{spec}"));
    let out = cjs::compile_dep(
        Path::new("/node_modules/pkg/index.js"),
        "/node_modules/pkg/index.js",
        "export const a = 1;\nimport b from \"./b\";",
        &mut resolve,
    )
    .unwrap();
    assert_eq!(out.imports, vec!["/@id/./b".to_string()]);
    assert!(out.code.contains("export"), "{}", out.code);
}

#[test]
fn a_cjs_dep_without_module_syntax_is_wrapped() {
    let mut resolve = |spec: &str| Some(format!("/@id/{spec}"));
    let out = cjs::compile_dep(
        Path::new("/node_modules/pkg/index.js"),
        "/node_modules/pkg/index.js",
        "const dep = require(\"./dep\");\nmodule.exports = dep;",
        &mut resolve,
    )
    .unwrap();
    assert!(
        out.imports.contains(&"/@id/./dep".to_string()),
        "{:?}",
        out.imports
    );
    // The body keeps its `require("./dep")` call, but it now resolves through a
    // local shim over the statically imported dependency map.
    assert!(out.code.contains("__oj_deps"), "{}", out.code);
    assert!(
        out.code.contains("import * as __oj_ns_0"),
        "the dep must become a static namespace import: {}",
        out.code
    );
    assert!(out.code.contains("function require(id)"), "{}", out.code);
}

#[test]
fn an_unresolvable_require_does_not_fail_the_module() {
    let mut resolve = |_: &str| None;
    let out = cjs::wrap_cjs(
        Path::new("/node_modules/pkg/index.js"),
        "/node_modules/pkg/index.js",
        "const missing = require(\"./missing\");\nmodule.exports = missing;",
        &mut resolve,
    )
    .unwrap();
    assert!(out.imports.is_empty(), "{:?}", out.imports);
    assert!(!out.code.is_empty());
}

#[test]
fn a_bundle_factory_of_an_app_module_is_an_esm_factory() {
    let mut resolve = |spec: &str| Some(format!("/@id/{spec}"));
    let factory = bundle::compile_factory(
        Path::new("/src/App.tsx"),
        "/src/App.tsx",
        "import React from \"react\";\nexport function App() { return <div />; }",
        &mut resolve,
    )
    .unwrap();
    assert_eq!(factory.kind, bundle::FactoryKind::Esm);
    assert!(factory.is_boundary(), "a component is a refresh boundary");
    assert!(factory.require_map.is_empty());
}

#[test]
fn a_bundle_factory_of_a_cjs_dep_is_a_cjs_factory() {
    let mut resolve = |spec: &str| Some(format!("/@id/{spec}"));
    let factory = bundle::compile_factory(
        Path::new("/node_modules/pkg/index.js"),
        "/node_modules/pkg/index.js",
        "module.exports = require(\"./inner\");",
        &mut resolve,
    )
    .unwrap();
    assert_eq!(factory.kind, bundle::FactoryKind::Cjs);
    assert!(!factory.is_boundary(), "a CJS dep is never a boundary");
    assert_eq!(
        factory.require_map,
        vec![("./inner".to_string(), "/@id/./inner".to_string())]
    );
}

#[test]
fn a_dep_reached_through_at_fs_is_still_a_dep() {
    let mut resolve = |spec: &str| Some(format!("/@id/{spec}"));
    let factory = bundle::compile_factory(
        Path::new("/link/node_modules/pkg/index.js"),
        "/@fs/link/node_modules/pkg/index.js",
        "module.exports = 1;",
        &mut resolve,
    )
    .unwrap();
    assert_eq!(factory.kind, bundle::FactoryKind::Cjs);
}
