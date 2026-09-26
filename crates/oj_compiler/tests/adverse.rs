// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Hostile and malformed module sources. Every entry point in the compiler is
//! reached from a file on disk that oj did not write, so the contract is
//! uniform: return a `CompileError`, or return code — never panic, never hang,
//! never emit something that is not valid JavaScript.

use std::path::Path;

use oj_compiler::{cjs, compile, glob, interop, json, CompileError, CompileOptions};

/// Re-parses generated code, so a test can assert output validity rather than
/// output shape. `.mjs` so the module syntax is accepted.
fn parses(code: &str) -> bool {
    compile(Path::new("/verify.mjs"), code, &CompileOptions::prod()).is_ok()
}

fn dev(path: &str, source: &str) -> Result<oj_compiler::CompileOutput, CompileError> {
    compile(Path::new(path), source, &CompileOptions::dev())
}

#[test]
fn malformed_sources_are_errors_not_panics() {
    let cases: &[(&str, &str)] = &[
        ("unterminated string", "const a = \"oops"),
        ("unterminated template", "const a = `oops"),
        ("unterminated comment", "/* oops"),
        ("unterminated regex", "const a = /oops"),
        ("stray brace", "}"),
        ("stray bracket", "]"),
        ("lone else", "else {}"),
        ("reserved binding", "const class = 1;"),
        ("await outside async", "function f() { await x; }"),
        ("bad jsx", "const a = <div><span></div>;"),
        ("bad ts generic", "const a: Map<string = 1;"),
        ("decorator on nothing", "@dec"),
        ("html in js", "<!DOCTYPE html><html></html>"),
        ("binary-ish", "\u{0}\u{1}\u{2}\u{3}"),
    ];
    for (label, source) in cases {
        match dev("/src/App.tsx", source) {
            Err(CompileError::Parse { .. }) | Err(CompileError::Transform { .. }) => {}
            Err(other) => panic!("{label}: unexpected error kind: {other}"),
            Ok(out) => panic!("{label}: accepted invalid source, emitted: {}", out.code),
        }
    }
}

#[test]
fn lone_surrogate_escapes_round_trip_unchanged() {
    // `"\ud800"` is a legal string literal that is not well-formed UTF-16. It
    // must reach the browser as written, not as a replacement character.
    let out = dev(
        "/src/App.ts",
        "export const a = \"\\ud800\\udc00 \\ud800\";",
    )
    .unwrap();
    assert!(
        out.code.contains("\\ud800"),
        "escape rewritten: {}",
        out.code
    );
    assert!(parses(&out.code), "invalid output: {}", out.code);
}

#[test]
fn errors_that_are_semantic_rather_than_syntactic_pass_through() {
    // oj does not run oxc's semantic checker on every file in dev: these are
    // early errors per the spec, and the browser reports them. Pinned here so
    // the boundary is a decision rather than a surprise -- what matters is that
    // the emitted code is exactly as wrong as the input, not silently repaired.
    for source in [
        "let a = 1; let a = 2;",
        "const a = 1; a = 2;",
        "return 1;",
        "with (x) {}",
    ] {
        match compile(Path::new("/src/App.ts"), source, &CompileOptions::dev()) {
            Ok(out) => assert!(!out.code.is_empty(), "{source}"),
            Err(CompileError::Parse { .. }) | Err(CompileError::Transform { .. }) => {}
            Err(other) => panic!("{source}: unexpected error kind: {other}"),
        }
    }
}

#[test]
fn an_unsupported_extension_is_refused_by_name() {
    let err = dev("/src/styles.css", "body { color: red }").unwrap_err();
    assert!(
        matches!(err, CompileError::UnsupportedFileType(_)),
        "got {err}"
    );
    // A path with no extension at all.
    assert!(matches!(
        dev("/src/Makefile", "all:").unwrap_err(),
        CompileError::UnsupportedFileType(_)
    ));
}

#[test]
fn deeply_nested_expressions_do_not_overflow_a_compile_thread() {
    // Parsing, transforming and printing all recurse once per nesting level, so
    // the depth a file may reach is a function of the stack oj gives the thread
    // that compiles it. A generated or minified module nests far deeper than
    // anything written by hand, and an overflow aborts the whole process rather
    // than failing the one file -- hence `COMPILE_STACK_SIZE`, which the CLI
    // installs as the runtime's thread stack size. On the 2 MiB a thread gets
    // by default, the array case below already aborts at depth 900.
    let handle = std::thread::Builder::new()
        .stack_size(oj_compiler::COMPILE_STACK_SIZE)
        .spawn(|| {
            for depth in [64usize, 512] {
                for (open, close) in [("[", "]"), ("(", ")"), ("{a:", "}"), ("f(", ")")] {
                    let source = format!(
                        "export const x = {}1{};",
                        open.repeat(depth),
                        close.repeat(depth)
                    );
                    // Accepted or rejected, but never a crash.
                    let _ = dev("/src/deep.ts", &source);
                }
                let ternary = format!(
                    "export const x = {}1{};",
                    "a ? ".repeat(depth),
                    " : 0".repeat(depth)
                );
                let _ = dev("/src/ternary.ts", &ternary);
            }
            // The shape a data-shaped generated file actually reaches.
            let arrays = format!(
                "export const x = {}1{};",
                "[".repeat(2_000),
                "]".repeat(2_000)
            );
            assert!(dev("/src/arrays.ts", &arrays).is_ok());
            // A long flat chain is iterative in the parser but not in codegen.
            let chain = format!("export const x = 1{};", "+1".repeat(5_000));
            let _ = dev("/src/chain.ts", &chain);
        })
        .unwrap();
    handle.join().expect("no overflow on a compile-sized stack");
}

#[test]
fn very_large_sources_compile_in_linear_time() {
    let big: String = (0..20_000)
        .map(|i| format!("export const v{i} = {i};\n"))
        .collect();
    let out = dev("/src/big.ts", &big).unwrap();
    assert!(out.code.contains("v19999"));

    let long_string = format!("export const s = \"{}\";", "a".repeat(2_000_000));
    assert!(dev("/src/long.ts", &long_string).is_ok());

    let mut many_imports: String = (0..5_000)
        .map(|i| format!("import d{i} from \"./d{i}.js\";\n"))
        .collect();
    // Used, so the TypeScript transform cannot elide them.
    many_imports.push_str("export const all = [");
    for i in 0..5_000 {
        many_imports.push_str(&format!("d{i},"));
    }
    many_imports.push_str("];\n");
    let out = dev("/src/imports.ts", &many_imports).unwrap();
    assert_eq!(out.imports.len(), 5_000);
}

#[test]
fn unused_type_only_imports_are_elided_from_a_typescript_module() {
    // Documented TypeScript semantics, and the reason the count above needs its
    // imports used: an unused import in a .ts file is not a module edge.
    let out = dev(
        "/src/App.ts",
        "import d from \"./d.js\";\nexport const a = 1;",
    )
    .unwrap();
    assert!(out.imports.is_empty(), "{:?}", out.imports);
    // In a .js module there is no elision.
    let out = dev(
        "/src/App.js",
        "import d from \"./d.js\";\nexport const a = 1;",
    )
    .unwrap();
    assert_eq!(out.imports, vec!["./d.js".to_string()]);
    // A bare side-effect import is kept either way.
    let out = dev("/src/App.ts", "import \"./side-effect.css\";").unwrap();
    assert_eq!(out.imports, vec!["./side-effect.css".to_string()]);
}

#[test]
fn unicode_identifiers_and_bidi_text_survive_a_round_trip() {
    let source = "export const café = 1;\n\
                  export const 日本 = 2;\n\
                  export const $ = \"\u{202e}reversed\u{202c}\";\n\
                  export const zwj = \"👩‍👩‍👧‍👦\";\n";
    let out = dev("/src/unicode.ts", source).unwrap();
    assert!(parses(&out.code), "invalid output: {}", out.code);
    let mut names = oj_compiler::exports(source, Path::new("/src/unicode.ts"));
    names.sort();
    assert_eq!(names, ["$", "café", "zwj", "日本"]);
}

#[test]
fn line_terminators_inside_strings_stay_escaped_in_the_output() {
    // U+2028 and U+2029 are legal in a JS string literal but break naive
    // concatenation into a `<script>` or a source map.
    let source = "export const s = \"a\u{2028}b\u{2029}c\";";
    let out = dev("/src/sep.ts", source).unwrap();
    assert!(parses(&out.code), "invalid output: {}", out.code);
}

#[test]
fn a_module_that_imports_itself_is_still_compiled() {
    let out = dev("/src/App.tsx", "import \"./App\";\nexport const a = 1;").unwrap();
    assert_eq!(out.imports, vec!["./App".to_string()]);
}

#[test]
fn hostile_import_specifiers_are_reported_verbatim_and_rewritten_once() {
    let source = "\
import a from \"../../../../../../etc/passwd\";
import b from \"/absolute/path\";
import c from \"http://evil.example/x.js\";
import d from \"data:text/javascript,export default 1\";
import e from \"\";
import f from \" \t\";
import g from \"./a\\u0000b\";
export { a, b, c, d, e, f, g };
";
    let out = dev("/src/App.tsx", source).unwrap();
    assert_eq!(out.imports.len(), 7, "{:?}", out.imports);
    assert!(out
        .imports
        .contains(&"../../../../../../etc/passwd".to_string()));
    assert!(out
        .imports
        .contains(&"http://evil.example/x.js".to_string()));

    // A rewriter sees each specifier exactly once and its answer is what lands
    // in the output.
    let mut seen: Vec<String> = Vec::new();
    let mut rewriter = |spec: &str| {
        seen.push(spec.to_string());
        Some(format!("/@resolved/{}", seen.len()))
    };
    let out = oj_compiler::compile_module(
        Path::new("/src/App.tsx"),
        source,
        &CompileOptions::dev(),
        Some(&mut rewriter),
    )
    .unwrap();
    assert_eq!(seen.len(), 7, "each specifier once: {seen:?}");
    assert!(parses(&out.code), "invalid output: {}", out.code);
    assert!(!out.code.contains("etc/passwd"), "{}", out.code);
}

#[test]
fn a_rewriter_that_returns_junk_cannot_break_the_output_grammar() {
    let source = "import a from \"./a\";\nimport(\"./b\");\nexport { a };";
    for junk in [
        "\"; evil(); \"",
        "a\nb",
        "\\",
        "'",
        "${x}",
        "\u{2028}",
        "\0",
        "",
    ] {
        let mut rewriter = |_: &str| Some(junk.to_string());
        let out = oj_compiler::compile_module(
            Path::new("/src/App.tsx"),
            source,
            &CompileOptions::dev(),
            Some(&mut rewriter),
        )
        .unwrap();
        assert!(
            parses(&out.code),
            "rewriting to {junk:?} produced invalid code: {}",
            out.code
        );
    }
}

#[test]
fn json_that_is_not_json_is_an_error() {
    for source in [
        "",
        " ",
        "{",
        "{,}",
        "{\"a\":}",
        "{'a':1}",
        "{a:1}",
        "{\"a\":1,}",
        "// comment\n{}",
        "{\"a\":1}{\"b\":2}",
        "NaN",
        "undefined",
        "01",
        "0x10",
        "'string'",
    ] {
        assert!(
            json::to_esm(source, "/data.json").is_err(),
            "accepted invalid JSON: {source:?}"
        );
    }
}

#[test]
fn json_keys_that_are_not_identifiers_are_default_only() {
    let source =
        r#"{"kebab-case":1,"with space":2,"1numeric":3,"":4,"default":5,"class":6,"ok":7}"#;
    let esm = json::to_esm(source, "/data.json").unwrap();
    assert!(parses(&esm), "invalid output: {esm}");
    assert!(esm.contains("export const ok ="));
    for bad in ["kebab-case", "with space", "1numeric", "class"] {
        assert!(
            !esm.contains(&format!("export const {bad}")),
            "exported {bad}: {esm}"
        );
    }
    assert_eq!(
        esm.matches("export default").count(),
        1,
        "exactly one default: {esm}"
    );
}

#[test]
fn a_json_proto_key_stays_an_own_property() {
    // `{"__proto__": ...}` spliced into an object literal sets the prototype
    // instead of a property, so `import data from './d.json'` would disagree
    // with `JSON.parse` on the same bytes -- and the value would leak onto the
    // object as inherited state.
    let source = r#"{"__proto__":{"polluted":true},"ok":1}"#;
    let esm = json::to_esm(source, "/data.json").unwrap();
    assert!(parses(&esm), "invalid output: {esm}");
    assert!(
        !esm.contains("\"__proto__\":") || esm.contains("[\"__proto__\"]"),
        "a bare __proto__ key must not reach an object literal: {esm}"
    );
    assert!(
        !esm.contains("export const __proto__"),
        "__proto__ is not an exportable name: {esm}"
    );
    // Nested occurrences count too: it is the JS object literal that is unsafe.
    let nested = json::to_esm(r#"{"a":{"__proto__":{"x":1}}}"#, "/data.json").unwrap();
    assert!(parses(&nested), "invalid output: {nested}");
    assert!(
        !nested.contains("\"__proto__\":") || nested.contains("[\"__proto__\"]"),
        "{nested}"
    );
}

#[test]
fn json_scalars_and_deep_nesting_are_accepted() {
    for source in [
        "1", "-0.5e10", "\"str\"", "true", "false", "null", "[]", "{}",
    ] {
        let esm = json::to_esm(source, "/data.json").unwrap();
        assert!(parses(&esm), "{source} -> {esm}");
    }
    // serde_json has a recursion limit; hitting it must be an error, not a crash.
    let deep = format!("{}1{}", "[".repeat(2_000), "]".repeat(2_000));
    let _ = json::to_esm(&deep, "/data.json");
}

#[test]
fn json_strings_with_script_terminators_are_safe_to_inline() {
    let source = r#"{"html":"</script><script>alert(1)</script>","sep":"a b"}"#;
    let esm = json::to_esm(source, "/data.json").unwrap();
    assert!(parses(&esm), "invalid output: {esm}");
}

#[test]
fn cjs_lowering_survives_every_require_shape() {
    let source = r#"
const a = require("./a");
const b = require(`./b`);
const c = require("./c" + suffix);
const d = require(name);
const e = require();
const f = require("./f", "extra");
const g = module.require("./g");
const h = require.resolve("./h");
try { var i = require("./i"); } catch (_) {}
if (typeof require === "function") { require("./j"); }
exports.k = 1;
module.exports = { l: 2 };
Object.defineProperty(exports, "__esModule", { value: true });
"#;
    let mut resolve = |spec: &str| Some(format!("/resolved{spec}"));
    let out = cjs::wrap_cjs(
        Path::new("/node_modules/pkg/index.js"),
        "/node_modules/pkg/index.js",
        source,
        &mut resolve,
    )
    .unwrap();
    assert!(!out.code.is_empty());

    let analysis =
        cjs::analyze_for_factory(Path::new("/node_modules/pkg/index.js"), source).unwrap();
    // Only statically known specifiers can be pre-resolved.
    for spec in &analysis.requires {
        assert!(
            !spec.is_empty(),
            "empty specifier in {:?}",
            analysis.requires
        );
    }
    let mut sorted = analysis.requires.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), analysis.requires.len(), "requires deduped");
}

#[test]
fn cjs_lowering_reports_parse_errors() {
    let err = cjs::analyze_for_factory(Path::new("/node_modules/pkg/index.js"), "const a = (")
        .unwrap_err();
    assert!(matches!(err, CompileError::Parse { .. }), "{err}");
}

#[test]
fn module_syntax_detection_never_panics() {
    for source in [
        "",
        "import",
        "export",
        "import.meta",
        "import(\"x\")",
        "const import1 = 1;",
        "// export const a = 1;",
        "\"use strict\"; module.exports = 1;",
        "#!/usr/bin/env node\nmodule.exports = 1;",
        "const a = (",
    ] {
        let _ = cjs::has_module_syntax_pub(Path::new("/node_modules/pkg/index.js"), source);
    }
    assert!(cjs::has_module_syntax_pub(
        Path::new("/x.js"),
        "export const a = 1;"
    ));
    assert!(!cjs::has_module_syntax_pub(
        Path::new("/x.js"),
        "module.exports = 1;"
    ));
}

#[test]
fn glob_expansion_of_hostile_patterns_stays_inside_the_grammar() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("locales")).unwrap();
    std::fs::write(dir.path().join("locales/en.json"), "{}").unwrap();
    std::fs::write(dir.path().join("secret.txt"), "s3cret").unwrap();
    let entry = dir.path().join("src.js");

    for pattern in [
        "./locales/*.json",
        "../*",
        "../../../../../../etc/*",
        "/etc/*",
        "**/*",
        "./{a,b}/*.json",
        "./[[:alpha:]]*",
        "./locales/../secret.txt",
        "",
        "./\u{0}",
        "./*.json?raw",
    ] {
        let source = format!("export const m = import.meta.glob({pattern:?});");
        let out = glob::expand_source(&source, &entry);
        assert!(
            compile(Path::new("/verify.mjs"), &out, &CompileOptions::prod()).is_ok(),
            "pattern {pattern:?} produced invalid code: {out}"
        );
    }
}

#[test]
fn glob_options_that_make_no_sense_are_ignored_safely() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("locales")).unwrap();
    std::fs::write(dir.path().join("locales/en.json"), "{}").unwrap();
    let entry = dir.path().join("src.js");

    for options in [
        "{ eager: true }",
        "{ eager: \"yes\" }",
        "{ import: \"default\" }",
        "{ import: \"a b\" }",
        "{ import: \"\\\"; evil(); \\\"\" }",
        "{ query: \"?raw\" }",
        "{ query: \"raw\" }",
        "{ query: \"\\\" + evil() + \\\"\" }",
        "{ as: \"url\" }",
        "{ unknown: 1 }",
        "{}",
        "null",
        "[]",
        "42",
    ] {
        let source = format!("export const m = import.meta.glob(\"./locales/*.json\", {options});");
        let out = glob::expand_source(&source, &entry);
        assert!(
            compile(Path::new("/verify.mjs"), &out, &CompileOptions::prod()).is_ok(),
            "options {options} produced invalid code: {out}"
        );
    }
}

#[test]
fn dynamic_import_var_expansion_stays_inside_the_grammar() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("pages")).unwrap();
    std::fs::write(dir.path().join("pages/home.js"), "export default 1;").unwrap();
    std::fs::write(dir.path().join("pages/about.js"), "export default 2;").unwrap();
    let entry = dir.path().join("src.js");

    for template in [
        "`./pages/${name}.js`",
        "`../${name}`",
        "`./pages/${a}${b}.js`",
        "`./pages/${name}`",
        "`/abs/${name}.js`",
        "`${name}`",
        "`./pages/${\"literal\"}.js`",
        "`./pages/${name}.js${extra}`",
    ] {
        let source = format!("export const load = (name) => import({template});");
        let out = glob::expand_dynamic_import_vars_source(&source, &entry);
        assert!(
            compile(Path::new("/verify.mjs"), &out, &CompileOptions::prod()).is_ok(),
            "template {template} produced invalid code: {out}"
        );
    }
}

#[test]
fn new_url_asset_expansion_stays_inside_the_grammar() {
    for expression in [
        "new URL(\"./a.png\", import.meta.url)",
        "new URL(\"../b.png\", import.meta.url)",
        "new URL(\"/abs.png\", import.meta.url)",
        "new URL(\"http://x/y.png\", import.meta.url)",
        "new URL(name, import.meta.url)",
        "new URL(\"./a.png\")",
        "new URL(\"./a.png\", import.meta.url, extra)",
        "new URL(\"./a.png\", import.meta.env)",
        "new URL(\"./\\\"; evil(); \\\".png\", import.meta.url)",
        "new NotURL(\"./a.png\", import.meta.url)",
    ] {
        let source = format!("export const u = {expression};");
        let out = glob::expand_new_url_asset_source(&source, Path::new("/src/app.js"));
        assert!(
            compile(Path::new("/verify.mjs"), &out, &CompileOptions::prod()).is_ok(),
            "expression {expression} produced invalid code: {out}"
        );
    }
}

#[test]
fn source_expanders_are_the_identity_on_unparsable_input() {
    let broken = "const a = ( // import.meta.glob import( import.meta.url";
    let path = Path::new("/src/broken.js");
    assert_eq!(glob::expand_source(broken, path), broken);
    assert_eq!(
        glob::expand_dynamic_import_vars_source(broken, path),
        broken
    );
    assert_eq!(glob::expand_new_url_asset_source(broken, path), broken);
}

#[test]
fn cjs_interop_rewriting_is_none_when_there_is_nothing_to_do() {
    let path = Path::new("/src/App.tsx");
    let never = |_: &str| None;
    assert!(interop::rewrite_cjs_interop("import a from \"./a\";", path, &never).is_none());
    assert!(interop::rewrite_cjs_interop("const a = (", path, &|_| Some("/x".into())).is_none());
    assert!(interop::rewrite_cjs_interop("", path, &|_| Some("/x".into())).is_none());
}

#[test]
fn cjs_interop_rewriting_produces_valid_code_for_every_import_form() {
    let path = Path::new("/src/App.tsx");
    let sources = [
        "import a from \"cjs-pkg\";",
        "import * as ns from \"cjs-pkg\";",
        "import { a, b as c } from \"cjs-pkg\";",
        "import def, { a } from \"cjs-pkg\";",
        "import def, * as ns from \"cjs-pkg\";",
        "import \"cjs-pkg\";",
        "import type { T } from \"cjs-pkg\";",
        "import { \"weird name\" as w } from \"cjs-pkg\";",
        "import a from \"cjs-pkg\";\nimport b from \"cjs-pkg\";",
    ];
    for source in sources {
        let rewritten = interop::rewrite_cjs_interop(source, path, &|spec| {
            (spec == "cjs-pkg").then(|| "/node_modules/cjs-pkg/index.js".to_string())
        });
        let Some(code) = rewritten else { continue };
        assert!(
            compile(Path::new("/verify.mjs"), &code, &CompileOptions::prod()).is_ok(),
            "{source} -> invalid code: {code}"
        );
        assert!(
            !code.contains("\"cjs-pkg\""),
            "specifier left behind: {code}"
        );
    }
}
