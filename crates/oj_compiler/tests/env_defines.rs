// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The server-set define list and its SSR layering, exercised through
//! `compile`. Its own integration binary because `set_import_meta_env*` is
//! process-global: the unit tests in lib.rs rely on the mode-derived fallback,
//! and these tests serialize on a mutex so they never observe each other's set.

use std::path::Path;
use std::sync::Mutex;

use oj_compiler::{
    compile, merge_import_meta_env_ssr, set_import_meta_env, set_import_meta_env_ssr,
    CompileOptions,
};

static SERIAL: Mutex<()> = Mutex::new(());

fn client() -> Vec<(String, String)> {
    vec![
        ("import.meta.env.SSR".into(), "false".into()),
        ("import.meta.env.VITE_X".into(), "\"x1\"".into()),
        (
            "import.meta.env".into(),
            "({\"SSR\":false,\"VITE_X\":\"x1\"})".into(),
        ),
        ("process.env.NODE_ENV".into(), "\"development\"".into()),
        ("__SIDE__".into(), "\"client\"".into()),
    ]
}

fn ssr_opts() -> CompileOptions {
    let mut o = CompileOptions::dev();
    o.ssr = true;
    o
}

#[test]
fn set_replaces_client_defines_and_a_re_set_wins() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    set_import_meta_env(client());
    set_import_meta_env_ssr(vec![]);
    let src = "export const x = import.meta.env.VITE_X; export const s = __SIDE__;";
    let out = compile(Path::new("a.ts"), src, &CompileOptions::dev()).unwrap();
    assert!(
        out.code.contains("\"x1\"") && out.code.contains("\"client\""),
        "{}",
        out.code
    );

    let mut again = client();
    again[1].1 = "\"x2\"".into();
    set_import_meta_env(again);
    let out = compile(Path::new("a.ts"), src, &CompileOptions::dev()).unwrap();
    assert!(
        out.code.contains("\"x2\"") && !out.code.contains("\"x1\""),
        "stale cached config:\n{}",
        out.code
    );
}

#[test]
fn ssr_compiles_flip_ssr_flag_and_layer_ssr_overrides_only() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    set_import_meta_env(client());
    set_import_meta_env_ssr(vec![("__SIDE__".into(), "\"server\"".into())]);
    let src = "export const s = import.meta.env.SSR; export const all = import.meta.env; export const side = __SIDE__;";
    let ssr = compile(Path::new("a.ts"), src, &ssr_opts()).unwrap();
    assert!(ssr.code.contains("s = true"), "{}", ssr.code);
    assert!(
        ssr.code.contains("\"SSR\": true"),
        "blob SSR flag flipped:\n{}",
        ssr.code
    );
    assert!(ssr.code.contains("\"server\""), "{}", ssr.code);
    let cli = compile(Path::new("a.ts"), src, &CompileOptions::dev()).unwrap();
    assert!(
        cli.code.contains("s = false") && cli.code.contains("\"client\""),
        "ssr override leaked to client:\n{}",
        cli.code
    );
}

#[test]
fn merge_ssr_overrides_later_wins_and_invalidates_the_ssr_variant() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    set_import_meta_env(client());
    set_import_meta_env_ssr(vec![("__SIDE__".into(), "\"server\"".into())]);
    let src = "export const side = __SIDE__; export const only = __SSR_ONLY__;";
    let before = compile(Path::new("a.ts"), src, &ssr_opts()).unwrap();
    assert!(
        before.code.contains("\"server\"") && before.code.contains("__SSR_ONLY__"),
        "{}",
        before.code
    );
    merge_import_meta_env_ssr(vec![
        ("__SIDE__".into(), "\"srv2\"".into()),
        ("__SSR_ONLY__".into(), "1".into()),
    ]);
    let after = compile(Path::new("a.ts"), src, &ssr_opts()).unwrap();
    assert!(
        after.code.contains("\"srv2\"") && after.code.contains("only = 1"),
        "merge not applied:\n{}",
        after.code
    );
}

#[test]
fn plain_key_gate_still_replaces_process_env_node_env() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    set_import_meta_env(client());
    set_import_meta_env_ssr(vec![]);
    let src = "export const dev = process.env.NODE_ENV !== \"production\";";
    let out = compile(Path::new("dep.js"), src, &CompileOptions::dev()).unwrap();
    assert!(!out.code.contains("process.env.NODE_ENV"), "{}", out.code);
}

#[test]
fn a_rejected_define_list_silently_skips_replacement() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // oxc rejects a value with a syntax error; that skips the replacer
    // for the compile rather than failing it, and the cache must keep that.
    set_import_meta_env(vec![("import.meta.env.BAD".into(), "{".into())]);
    set_import_meta_env_ssr(vec![]);
    let out = compile(
        Path::new("a.ts"),
        "export const b = import.meta.env.BAD;",
        &CompileOptions::dev(),
    )
    .unwrap();
    assert!(out.code.contains("import.meta.env.BAD"), "{}", out.code);
}

#[test]
fn ssr_overrides_set_before_the_client_list_still_layer() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // The server may hand over `environments.ssr.define` before the dotenv
    // list lands; the ssr variant must be derived once both inputs are known.
    set_import_meta_env(vec![]);
    set_import_meta_env_ssr(vec![("__SIDE__".into(), "\"server\"".into())]);
    set_import_meta_env(client());
    let src = "export const s = import.meta.env.SSR; export const side = __SIDE__;";
    let ssr = compile(Path::new("a.ts"), src, &ssr_opts()).unwrap();
    assert!(
        ssr.code.contains("s = true") && ssr.code.contains("\"server\""),
        "ssr overrides set first were lost:\n{}",
        ssr.code
    );
    let cli = compile(Path::new("a.ts"), src, &CompileOptions::dev()).unwrap();
    assert!(
        cli.code.contains("s = false") && cli.code.contains("\"client\""),
        "{}",
        cli.code
    );
}

#[test]
fn an_invalid_ssr_override_only_disables_the_ssr_variant() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // Each variant carries its own Option<config>: a value oxc rejects in the
    // ssr overrides skips replacement for ssr compiles only.
    set_import_meta_env(client());
    set_import_meta_env_ssr(vec![("__SIDE__".into(), "{".into())]);
    let src = "export const x = import.meta.env.VITE_X; export const side = __SIDE__;";
    let ssr = compile(Path::new("a.ts"), src, &ssr_opts()).unwrap();
    assert!(
        ssr.code.contains("import.meta.env.VITE_X") && ssr.code.contains("__SIDE__"),
        "ssr compile must skip replacement wholesale:\n{}",
        ssr.code
    );
    let cli = compile(Path::new("a.ts"), src, &CompileOptions::dev()).unwrap();
    assert!(
        cli.code.contains("\"x1\"") && cli.code.contains("\"client\""),
        "client variant must be unaffected:\n{}",
        cli.code
    );
}
