// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! VITE GAP, asserted strictly (this test FAILS until the compiler matches
//! Vite): Vite honors the nearest tsconfig's compilerOptions in its transform
//! (vite:oxc loads the tsconfig per file), so a project with
//! `useDefineForClassFields: false` gets [[Set]] assignment semantics for
//! class fields. oj's compiler reads no tsconfig at all, so such projects
//! (mobx and legacy-decorator apps are the common shape) silently get
//! [[Define]] semantics. The fixture below is a real on-disk tree with the
//! tsconfig next to the source, exactly what the implementation must consult.

use oj_compiler::{compile, CompileOptions};

// Compiles `class A { foo = 1 }` as src/a.ts under a fresh temp root holding
// the given extra files, returning the output.
fn compile_with(tag: &str, files: &[(&str, &str)]) -> String {
    let dir = std::env::temp_dir().join(format!("oj-tsconfig-gap-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    for (rel, s) in files {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, s).unwrap();
    }
    let file = src.join("a.ts");
    let source = "class A { foo = 1 }\nexport { A };";
    std::fs::write(&file, source).unwrap();
    let out = compile(&file, source, &CompileOptions::dev()).unwrap().code;
    let _ = std::fs::remove_dir_all(&dir);
    out
}

#[test]
fn use_define_for_class_fields_false_produces_assignment_semantics() {
    // Vite: with useDefineForClassFields:false the field lowers to a
    // constructor assignment, never a native field declaration.
    let out = compile_with(
        "explicit-false",
        &[(
            "tsconfig.json",
            r#"{ "compilerOptions": { "useDefineForClassFields": false } }"#,
        )],
    );
    assert!(
        out.contains("this.foo = 1"),
        "the nearest tsconfig's useDefineForClassFields was ignored: {out}"
    );
}

#[test]
fn tsconfig_without_target_defaults_to_assignment_semantics() {
    // rolldown (what Vite ships): when the option is unset, the target
    // decides, and NO target means useDefineForClassFields:false.
    let out = compile_with("bare", &[("tsconfig.json", r#"{ "compilerOptions": {} }"#)]);
    assert!(
        out.contains("this.foo = 1"),
        "a tsconfig without target must mean assignment semantics: {out}"
    );
}

#[test]
fn esnext_target_keeps_define_semantics() {
    let out = compile_with(
        "esnext",
        &[(
            "tsconfig.json",
            r#"{ "compilerOptions": { "target": "ESNext" } }"#,
        )],
    );
    assert!(
        !out.contains("this.foo = 1"),
        "target ESNext implies useDefineForClassFields:true: {out}"
    );
}

#[test]
fn numeric_targets_derive_the_default_like_vite() {
    // Verified against vite@8.3.0-beta.1 dev AND build: ES2022+ implies
    // define semantics, ES2021 and below imply assignment semantics.
    let out = compile_with(
        "es2022",
        &[(
            "tsconfig.json",
            r#"{ "compilerOptions": { "target": "ES2022" } }"#,
        )],
    );
    assert!(!out.contains("this.foo = 1"), "ES2022 implies define: {out}");
    let out = compile_with(
        "es2016",
        &[(
            "tsconfig.json",
            r#"{ "compilerOptions": { "target": "ES2016" } }"#,
        )],
    );
    assert!(out.contains("this.foo = 1"), "ES2016 implies assignment: {out}");
}

#[test]
fn no_tsconfig_keeps_define_semantics() {
    let out = compile_with("none", &[]);
    assert!(
        !out.contains("this.foo = 1"),
        "a file no tsconfig owns keeps the transform defaults: {out}"
    );
}

#[test]
fn extends_chain_is_merged() {
    // The option set in an extended base must reach the file (oxc_resolver
    // merges `extends` like Vite).
    let out = compile_with(
        "extends",
        &[
            (
                "tsconfig.base.json",
                r#"{ "compilerOptions": { "useDefineForClassFields": false, "target": "ESNext" } }"#,
            ),
            ("tsconfig.json", r#"{ "extends": "./tsconfig.base.json" }"#),
        ],
    );
    assert!(
        out.contains("this.foo = 1"),
        "extends-inherited useDefineForClassFields was ignored: {out}"
    );
}
