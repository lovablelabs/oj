// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Runtime env keys ([`RuntimeDefines`]): a client compile reads a redirected
//! key through the served global and imports the env module first; the SSR
//! variant, other keys and modules that never read one are untouched.

use std::path::Path;
use std::sync::Arc;

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;
use proptest::prelude::*;

use oj_compiler::{compile, CompileOptions, ImportMetaEnv, RuntimeDefines};

const ENV_URL: &str = "/@oj/env.js";
const GLOBAL: &str = "__oj_env";

fn client() -> Vec<(String, String)> {
    vec![
        ("import.meta.env.MODE".into(), "\"development\"".into()),
        ("import.meta.env.DEV".into(), "true".into()),
        ("import.meta.env.SSR".into(), "false".into()),
        ("import.meta.env.VITE_URL".into(), "\"https://baked.test\"".into()),
        ("import.meta.env.VITE_OTHER".into(), "\"other\"".into()),
        (
            "import.meta.env".into(),
            "({\"MODE\":\"development\",\"DEV\":true,\"SSR\":false,\"VITE_URL\":\"https://baked.test\",\"VITE_OTHER\":\"other\"})".into(),
        ),
        ("process.env.NODE_ENV".into(), "\"development\"".into()),
    ]
}

fn runtime(keys: &[&str]) -> RuntimeDefines {
    RuntimeDefines::new(keys.iter().copied(), GLOBAL, ENV_URL)
}

fn env(keys: &[&str]) -> Arc<ImportMetaEnv> {
    Arc::new(ImportMetaEnv::with_runtime(client(), vec![], runtime(keys)))
}

fn dev(env: &Arc<ImportMetaEnv>) -> CompileOptions {
    CompileOptions {
        env: Some(Arc::clone(env)),
        ..CompileOptions::dev()
    }
}

fn ssr(env: &Arc<ImportMetaEnv>) -> CompileOptions {
    CompileOptions {
        ssr: true,
        ..dev(env)
    }
}

fn compiled(path: &str, src: &str, opts: &CompileOptions) -> String {
    compile(Path::new(path), src, opts).unwrap().code
}

fn import_count(code: &str) -> usize {
    code.matches(&format!("import {ENV_URL:?}")).count()
}

/// The output's first statement, as the browser would see it.
fn first_statement(code: &str) -> &str {
    code.trim_start().split_inclusive(';').next().unwrap_or("")
}

fn parses_as_module(code: &str) {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, code, SourceType::mjs()).parse();
    assert!(
        parsed.diagnostics.is_empty() && !parsed.fatal_error,
        "output does not parse: {:?}\n{code}",
        parsed.diagnostics
    );
}

#[test]
fn a_runtime_key_read_becomes_the_global_member_and_imports_the_module_first() {
    let env = env(&["VITE_URL"]);
    let code = compiled(
        "a.ts",
        "export const url = import.meta.env.VITE_URL;",
        &dev(&env),
    );
    parses_as_module(&code);
    assert!(
        code.contains("globalThis.__oj_env.VITE_URL"),
        "the read goes through the global:\n{code}"
    );
    assert!(
        !code.contains("baked.test"),
        "no baked literal survives for a runtime key:\n{code}"
    );
    assert_eq!(
        import_count(&code),
        1,
        "one import of the env module:\n{code}"
    );
    assert_eq!(
        first_statement(&code),
        format!("import {ENV_URL:?};"),
        "the import comes first, so the global exists before any read:\n{code}"
    );
}

#[test]
fn the_whole_object_read_becomes_the_global() {
    let env = env(&["VITE_URL"]);
    for src in [
        "export const all = import.meta.env;",
        "export const { VITE_URL } = import.meta.env;",
        "export const copy = { ...import.meta.env };",
        "export const v = import.meta.env[\"VITE_URL\"];",
    ] {
        let code = compiled("a.ts", src, &dev(&env));
        parses_as_module(&code);
        assert!(
            code.contains("globalThis.__oj_env"),
            "{src}: the object read goes through the global:\n{code}"
        );
        assert!(
            !code.contains("baked.test"),
            "{src}: no baked literal survives:\n{code}"
        );
        assert_eq!(import_count(&code), 1, "{src}:\n{code}");
    }
}

#[test]
fn a_key_outside_the_runtime_list_keeps_its_literal_and_needs_no_import() {
    let env = env(&["VITE_URL"]);
    let code = compiled(
        "a.ts",
        "export const other = import.meta.env.VITE_OTHER; export const mode = import.meta.env.MODE;",
        &dev(&env),
    );
    parses_as_module(&code);
    assert!(
        code.contains("\"other\"") && code.contains("\"development\""),
        "{code}"
    );
    assert!(!code.contains("__oj_env"), "{code}");
    assert_eq!(import_count(&code), 0, "{code}");
}

#[test]
fn a_module_that_never_reads_the_env_is_untouched() {
    let env = env(&["VITE_URL"]);
    let src = "export const n = 1;\nexport function f() { return import.meta.url; }\n";
    let with = compiled("a.ts", src, &dev(&env));
    let without = compiled("a.ts", src, &CompileOptions::dev());
    assert_eq!(with, without, "a runtime list changes nothing for it");
    assert_eq!(import_count(&with), 0);
}

#[test]
fn a_mention_of_another_import_meta_property_is_not_a_read() {
    let env = env(&["VITE_URL"]);
    let code = compiled(
        "a.ts",
        "export const e = (import.meta as any).environment;",
        &dev(&env),
    );
    assert_eq!(import_count(&code), 0, "{code}");
}

#[test]
fn the_ssr_variant_keeps_the_literal_and_imports_nothing() {
    let env = env(&["VITE_URL"]);
    let src = "export const url = import.meta.env.VITE_URL; export const all = import.meta.env;";
    let code = compiled("a.ts", src, &ssr(&env));
    parses_as_module(&code);
    assert!(code.contains("\"https://baked.test\""), "{code}");
    assert!(!code.contains("__oj_env"), "{code}");
    assert_eq!(import_count(&code), 0, "{code}");
    assert!(
        code.contains("\"SSR\": true"),
        "the ssr flag still flips:\n{code}"
    );
}

#[test]
fn an_ssr_merge_never_reaches_the_client_redirect() {
    let env = env(&["VITE_URL"]);
    env.merge_ssr(vec![(
        "import.meta.env.VITE_URL".into(),
        "\"https://ssr.test\"".into(),
    )]);
    let src = "export const url = import.meta.env.VITE_URL;";
    let client = compiled("a.ts", src, &dev(&env));
    assert!(client.contains("globalThis.__oj_env.VITE_URL"), "{client}");
    let server = compiled("a.ts", src, &ssr(&env));
    assert!(server.contains("\"https://ssr.test\""), "{server}");
}

#[test]
fn a_runtime_key_with_no_baked_value_is_still_redirected() {
    let env = env(&["VITE_LATER"]);
    let code = compiled(
        "a.ts",
        "export const later = import.meta.env.VITE_LATER;",
        &dev(&env),
    );
    parses_as_module(&code);
    assert!(
        code.contains("globalThis.__oj_env.VITE_LATER"),
        "a value set at runtime reaches a module compiled before it:\n{code}"
    );
    assert_eq!(import_count(&code), 1);
}

#[test]
fn the_module_url_is_taken_as_given() {
    let under_base = Arc::new(ImportMetaEnv::with_runtime(
        client(),
        vec![],
        RuntimeDefines::new(["VITE_URL"], GLOBAL, "/app/@oj/env.js"),
    ));
    let code = compiled(
        "a.ts",
        "export const url = import.meta.env.VITE_URL;",
        &dev(&under_base),
    );
    assert_eq!(
        first_statement(&code),
        "import \"/app/@oj/env.js\";",
        "{code}"
    );
}

#[test]
fn jsx_with_refresh_keeps_the_env_import_first_and_valid() {
    let env = env(&["VITE_URL"]);
    let src = "import React from \"react\";\nexport function App() { return <a href={import.meta.env.VITE_URL}>x</a>; }\n";
    let code = compiled("App.tsx", src, &dev(&env));
    parses_as_module(&code);
    assert_eq!(
        first_statement(&code),
        format!("import {ENV_URL:?};"),
        "{code}"
    );
    assert!(code.contains("globalThis.__oj_env.VITE_URL"), "{code}");
    assert_eq!(import_count(&code), 1, "{code}");
}

#[test]
fn a_tiny_module_with_a_sourcemap_compiles() {
    // Shorter than the injected import: its spans must not reach into the
    // source, or the sourcemap builder would reject them.
    let env = env(&["VITE_URL"]);
    let out = compile(Path::new("t.js"), "import.meta.env", &dev(&env)).unwrap();
    parses_as_module(&out.code);
    assert!(out.map_json.is_some(), "the sourcemap is kept");
    assert_eq!(import_count(&out.code), 1);
}

#[test]
fn the_output_is_deterministic() {
    let env = env(&["VITE_URL"]);
    let src = "export const url = import.meta.env.VITE_URL; export const o = import.meta.env;";
    let a = compile(Path::new("a.ts"), src, &dev(&env)).unwrap();
    let b = compile(Path::new("a.ts"), src, &dev(&env)).unwrap();
    assert_eq!(a.code, b.code);
    assert_eq!(a.map_json, b.map_json);
}

#[test]
fn the_env_module_import_is_not_a_graph_import() {
    let env = env(&["VITE_URL"]);
    let out = compile(
        Path::new("a.ts"),
        "import x from \"./x.js\"; export const url = import.meta.env.VITE_URL + x;",
        &dev(&env),
    )
    .unwrap();
    assert_eq!(
        out.imports,
        ["./x.js"],
        "the served module is a route, not a module-graph edge"
    );
    assert_eq!(import_count(&out.code), 1);
}

#[test]
fn redirect_rewrites_only_the_runtime_keys_and_adds_missing_ones() {
    let rt = runtime(&["VITE_URL", "VITE_LATER"]);
    let redirected = rt.redirect(client());
    let get = |k: &str| {
        redirected
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("{k} missing"))
    };
    assert_eq!(
        get("import.meta.env.VITE_URL"),
        "globalThis.__oj_env.VITE_URL"
    );
    assert_eq!(
        get("import.meta.env.VITE_LATER"),
        "globalThis.__oj_env.VITE_LATER"
    );
    assert_eq!(get("import.meta.env"), "globalThis.__oj_env");
    assert_eq!(get("import.meta.env.VITE_OTHER"), "\"other\"");
    assert_eq!(get("import.meta.env.MODE"), "\"development\"");
    assert_eq!(get("process.env.NODE_ENV"), "\"development\"");
    assert_eq!(
        redirected.len(),
        client().len() + 1,
        "one key added, none duplicated"
    );
    // Redirecting again is a fixed point.
    assert_eq!(rt.redirect(redirected.clone()), redirected);
}

#[test]
fn redirect_of_an_empty_list_defines_the_runtime_keys_and_the_object() {
    let rt = runtime(&["VITE_URL"]);
    let redirected = rt.redirect(vec![]);
    assert_eq!(
        redirected,
        vec![
            (
                "import.meta.env.VITE_URL".to_string(),
                "globalThis.__oj_env.VITE_URL".to_string()
            ),
            (
                "import.meta.env".to_string(),
                "globalThis.__oj_env".to_string()
            ),
        ]
    );
}

#[test]
fn read_by_follows_the_key_boundaries() {
    // As built by `with_runtime`: MODE, DEV, SSR and VITE_OTHER are literals.
    let rt = env(&["VITE_URL"]).runtime().unwrap().clone();
    assert!(rt.read_by("import.meta.env.VITE_URL"));
    assert!(rt.read_by("x(import.meta.env)"));
    assert!(rt.read_by("import.meta.env[\"VITE_URL\"]"));
    assert!(rt.read_by("const { a } = import.meta.env;"));
    assert!(rt.read_by("import.meta.env?.VITE_URL"));
    assert!(
        rt.read_by("import.meta.env"),
        "a read at the end of the source"
    );
    assert!(
        rt.read_by("import.meta.env.VITE_URL_2"),
        "a longer key is another key, and one no define bakes: it reads the global"
    );
    assert!(!rt.read_by("import.meta.env.VITE_OTHER"));
    assert!(!rt.read_by("import.meta.env.MODE"));
    assert!(!rt.read_by("import.meta.environment"));
    assert!(!rt.read_by("import.meta.url"));
    assert!(!rt.read_by(""));
    assert!(
        rt.read_by("import.meta.env.MODE; import.meta.env.VITE_URL"),
        "the second read counts"
    );
}

#[test]
fn the_client_defines_report_the_redirect_and_the_runtime() {
    let env = env(&["VITE_URL"]);
    assert_eq!(
        env.runtime().map(|r| r.keys.clone()),
        Some(vec!["VITE_URL".to_string()])
    );
    let defines = env.client_defines();
    assert!(defines
        .iter()
        .any(|(k, v)| k == "import.meta.env.VITE_URL" && v == "globalThis.__oj_env.VITE_URL"));
    let plain = Arc::new(ImportMetaEnv::new(client(), vec![]));
    assert!(plain.runtime().is_none());
    assert_eq!(plain.client_defines(), client());
}

// ---- review follow-ups ---------------------------------------------------------------

#[test]
fn a_read_of_a_key_no_define_bakes_goes_through_the_global_with_the_import() {
    // The replacer rewrites the `import.meta.env` under `.VITE_UNDEFINED` to
    // the global (no member define matches), so the output must import the
    // module, or the read throws instead of giving `undefined` as Vite does.
    let env = env(&["VITE_URL"]);
    let code = compiled(
        "a.ts",
        "export const dsn = import.meta.env.VITE_UNDEFINED;",
        &dev(&env),
    );
    parses_as_module(&code);
    assert!(
        code.contains("globalThis.__oj_env.VITE_UNDEFINED"),
        "{code}"
    );
    assert_eq!(import_count(&code), 1, "{code}");
    assert!(!code.contains("baked.test"), "{code}");
}

#[test]
fn with_runtime_records_the_baked_keys_as_literals() {
    let env = env(&["VITE_URL"]);
    let rt = env.runtime().unwrap();
    let mut literals = rt.literal_keys.clone();
    literals.sort();
    assert_eq!(literals, ["DEV", "MODE", "SSR", "VITE_OTHER"]);
    assert!(
        !rt.read_by("import.meta.env.VITE_OTHER"),
        "a baked key needs no import"
    );
    assert!(
        rt.read_by("import.meta.env.VITE_UNDEFINED"),
        "an unbaked key reads the global"
    );
    assert!(rt.read_by("import.meta.env.VITE_URL"));
    // A bare RuntimeDefines knows no literals: every dotted read counts.
    assert!(runtime(&["VITE_URL"]).read_by("import.meta.env.VITE_OTHER"));
}

proptest::proptest! {
    /// Whatever the module reads, an output that names the global imports
    /// the module that defines it, exactly once and first.
    #[test]
    fn the_global_never_appears_without_its_import(
        reads in proptest::collection::vec(
            proptest::sample::select(vec![
                "import.meta.env.VITE_URL",
                "import.meta.env.VITE_OTHER",
                "import.meta.env.MODE",
                "import.meta.env.VITE_UNDEFINED",
                "import.meta.env",
                "import.meta.env[\"VITE_URL\"]",
                "import.meta.env?.VITE_URL",
                "{ ...import.meta.env }",
                "import.meta.url",
                "1",
            ]),
            0..6,
        )
    ) {
        let env = env(&["VITE_URL"]);
        let src: String = reads
            .iter()
            .enumerate()
            .map(|(i, read)| format!("export const v{i} = {read};\n"))
            .collect();
        let code = compiled("p.ts", &src, &dev(&env));
        parses_as_module(&code);
        let imports = import_count(&code);
        if code.contains(&format!("globalThis.{GLOBAL}")) {
            prop_assert_eq!(imports, 1, "{}", code);
            prop_assert_eq!(first_statement(&code), format!("import {ENV_URL:?};"), "{}", code);
        } else {
            prop_assert_eq!(imports, 0, "{}", code);
        }
        // The SSR variant never touches the global.
        let server = compiled("p.ts", &src, &ssr(&env));
        prop_assert!(!server.contains(GLOBAL), "{}", server);
    }
}

// ---- second review ---------------------------------------------------------------------

// A chain a formatter broke across lines: the replacer reads the tree and
// redirects it, so the module needs the import though the source never
// holds `import.meta.env` unbroken. Without it the read threw when no other
// module had loaded the env module first.
#[test]
fn a_read_broken_across_lines_imports_the_env_module() {
    let env = env(&["VITE_URL"]);
    let rt = env.runtime().unwrap().clone();
    for src in [
        "import.meta\n  .env.VITE_URL",
        "import.meta\n  .env\n  .VITE_URL",
        "import . meta . env . VITE_URL",
        "import.meta .env",
        "x(import.meta\n.env)",
    ] {
        assert!(rt.read_by(src), "{src:?}");
    }
    for src in [
        "import.meta.url",
        "import.meta\n  .url",
        "import.meta .environment",
        "import meta from \"./env\";",
        "important.meta .env",
        "await import(\"./meta.env\")",
    ] {
        // `important.meta .env` holds `import` only as part of a longer word
        // that is not followed by `.meta`.
        assert!(!rt.read_by(src), "{src:?}");
    }

    // With nothing else in the module to bring the replacer in, and with
    // another define that does: redirected, and imported.
    for src in [
        "export const url = import.meta\n  .env.VITE_URL;\n",
        "export const mode = process.env.NODE_ENV;\nexport const url = import.meta\n  .env.VITE_URL;\n",
    ] {
        let out = compiled("/src/broken.js", src, &dev(&env));
        assert!(out.starts_with("import \"/@oj/env.js\";"), "{out}");
        assert!(out.contains("globalThis.__oj_env.VITE_URL"), "{out}");
        assert!(!out.contains("import.meta"), "{out}");
    }
    // Without runtime keys the gate is as it was.
    let plain = Arc::new(ImportMetaEnv::new(client(), vec![]));
    let out = compiled(
        "/src/broken.js",
        "export const url = import.meta\n  .env.VITE_URL;\n",
        &dev(&plain),
    );
    assert!(!out.contains(GLOBAL), "{out}");
}
