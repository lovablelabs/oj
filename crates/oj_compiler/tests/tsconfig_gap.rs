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

#[test]
fn use_define_for_class_fields_false_produces_assignment_semantics() {
    let dir = std::env::temp_dir().join(format!("oj-tsconfig-gap-{}", std::process::id()));
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        dir.join("tsconfig.json"),
        r#"{ "compilerOptions": { "useDefineForClassFields": false } }"#,
    )
    .unwrap();
    let file = src.join("a.ts");
    let source = "class A { foo = 1 }\nexport { A };";
    std::fs::write(&file, source).unwrap();

    let out = compile(&file, source, &CompileOptions::dev()).unwrap().code;
    let _ = std::fs::remove_dir_all(&dir);

    // Vite: with useDefineForClassFields:false the field lowers to a
    // constructor assignment, never a native field declaration.
    assert!(
        out.contains("this.foo = 1"),
        "the nearest tsconfig's useDefineForClassFields was ignored: {out}"
    );
}
