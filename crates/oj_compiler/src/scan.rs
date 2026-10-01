use crate::bundle;
use memchr::memmem::Finder;
use oxc_allocator::Allocator;
use oxc_ast::ast::{Program, Statement};
use oxc_parser::Parser;
use oxc_span::SourceType;
use std::path::Path;
use std::sync::LazyLock;

// SIMD substring scanners (memchr), built once and reused as cheap gates before
// expensive transforms. Shared with `bundle.rs` so both compile paths scan the
// same way instead of falling back to scalar `str::contains`.
pub(crate) static F_IMPORT_META_ENV: LazyLock<Finder<'static>> =
    LazyLock::new(|| Finder::new("import.meta.env"));

pub(crate) static F_IMPORT_META_GLOB: LazyLock<Finder<'static>> =
    LazyLock::new(|| Finder::new("import.meta.glob"));

pub(crate) static F_IMPORT_PAREN: LazyLock<Finder<'static>> =
    LazyLock::new(|| Finder::new("import("));

/// True if `source` contains the needle, via the SIMD memmem finder.
pub(crate) fn scan(finder: &Finder<'static>, source: &str) -> bool {
    finder.find(source.as_bytes()).is_some()
}

pub fn exports(source_text: &str, path: &Path) -> Vec<String> {
    let Ok(source_type) = SourceType::from_path(path) else {
        return Vec::new();
    };
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source_text, source_type).parse();
    if parsed.panicked {
        return Vec::new();
    }
    export_names(&parsed.program)
}

pub(crate) fn export_names(program: &Program<'_>) -> Vec<String> {
    let mut names = Vec::new();
    for stmt in &program.body {
        match stmt {
            Statement::ExportDeclaration(decl) => {
                names.extend(bundle::binding_names(&decl.declaration));
            }
            Statement::ExportNamedDeclaration(decl) => {
                for spec in &decl.specifiers {
                    names.push(bundle::export_name(&spec.exported));
                }
            }
            Statement::ExportFromDeclaration(decl) => {
                for spec in &decl.specifiers {
                    names.push(bundle::export_name(&spec.exported));
                }
            }
            Statement::ExportAllDeclaration(decl) => {
                if let Some(exported) = &decl.exported {
                    names.push(bundle::export_name(exported));
                }
            }
            Statement::ExportDefaultDeclaration(_) => names.push("default".to_string()),
            _ => {}
        }
    }
    names
}

// Static import/export-from specifiers, in source order (deduped). Used to
// pre-resolve a module's imports before a plugin transform so a plugin's
// `this.resolve` is a local lookup instead of a per-import host round-trip.
pub fn imports(source_text: &str, path: &Path) -> Vec<String> {
    let Ok(source_type) = SourceType::from_path(path) else {
        return Vec::new();
    };
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source_text, source_type).parse();
    if parsed.panicked {
        return Vec::new();
    }
    let mut specs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |s: &str, specs: &mut Vec<String>| {
        if seen.insert(s.to_string()) {
            specs.push(s.to_string());
        }
    };
    for stmt in &parsed.program.body {
        match stmt {
            Statement::ImportDeclaration(decl) => push(decl.source.value.as_str(), &mut specs),
            Statement::ExportAllDeclaration(decl) => push(decl.source.value.as_str(), &mut specs),
            _ => {}
        }
    }
    specs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn imports_collects_static_specifiers_deduped() {
        let src = r#"
            import { a } from "react";
            import b from "./b";
            import "react";
            export * from "./c";
            const x = await import("./dynamic");
        "#;
        let got = imports(src, Path::new("m.tsx"));
        assert_eq!(
            got,
            vec!["react".to_string(), "./b".to_string(), "./c".to_string()]
        );
    }

    #[test]
    fn imports_empty_for_no_imports() {
        assert!(imports("export const x = 1;", Path::new("m.ts")).is_empty());
    }

    #[test]
    fn exports_lists_named_and_default() {
        let src = r#"
export const getUser = async (id) => ({ id });
export function listUsers() { return []; }
export class Thing {}
const x = 1;
export { x, x as y };
export default function () {}
"#;
        let mut names = exports(src, Path::new("api.server.ts"));
        names.sort();
        assert_eq!(
            names,
            ["Thing", "default", "getUser", "listUsers", "x", "y"]
        );
    }

    #[test]
    fn exports_handles_reexports_and_never_panics() {
        let mut local = exports(
            r#"export const a = 1; export { a as b };"#,
            Path::new("m.ts"),
        );
        local.sort();
        assert_eq!(local, ["a", "b"]);
        assert!(exports("export { = ;", Path::new("bad.ts")).is_empty());
        assert!(exports("body{}", Path::new("x.css")).is_empty());
    }

    #[test]
    fn exports_captures_reexport_from_and_namespace_star() {
        let mut names = exports(r#"export { a, b as c } from "./mod";"#, Path::new("m.ts"));
        names.sort();
        assert_eq!(names, ["a", "c"]);
        assert_eq!(
            exports(r#"export * as ns from "./mod";"#, Path::new("m.ts")),
            ["ns"]
        );
        assert!(exports(r#"export * from "./mod";"#, Path::new("m.ts")).is_empty());
        let mut mixed = exports(
            r#"export default function () {}
export { x } from "./a";
export * as z from "./b";"#,
            Path::new("m.ts"),
        );
        mixed.sort();
        assert_eq!(mixed, ["default", "x", "z"]);
    }
}
