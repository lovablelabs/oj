use crate::bundle;
use memchr::memmem::Finder;
use oxc_allocator::Allocator;
use oxc_ast::ast::{Expression, ImportExpression, Program, Statement, StringLiteral};
use oxc_ast_visit::{walk, Visit};
use oxc_parser::Parser;
use oxc_span::{SourceType, Span};
use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;

// SIMD substring finders (memchr), built once: cheap gates before expensive
// transforms, shared with `bundle.rs`.
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

/// Runs `f` on the parsed module; empty when the path has no JS/TS type or the
/// parser panicked (recoverable syntax errors still yield a program).
fn with_program<T: Default>(source_text: &str, path: &Path, f: impl FnOnce(&Program) -> T) -> T {
    let Ok(source_type) = SourceType::from_path(path) else {
        return T::default();
    };
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source_text, source_type).parse();
    if parsed.panicked {
        return T::default();
    }
    f(&parsed.program)
}

pub fn exports(source_text: &str, path: &Path) -> Vec<String> {
    with_program(source_text, path, export_names)
}

pub(crate) fn export_names(program: &Program<'_>) -> Vec<String> {
    let mut names = Vec::new();
    for stmt in &program.body {
        match stmt {
            Statement::ExportDeclaration(decl) => {
                names.extend(bundle::binding_names(&decl.declaration));
            }
            Statement::ExportNamedDeclaration(decl) => {
                names.extend(
                    decl.specifiers
                        .iter()
                        .map(|s| bundle::export_name(&s.exported)),
                );
            }
            Statement::ExportFromDeclaration(decl) => {
                names.extend(
                    decl.specifiers
                        .iter()
                        .map(|s| bundle::export_name(&s.exported)),
                );
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

/// Static import/export-from specifiers, in source order (deduped). Used to
/// pre-resolve a module's imports before a plugin transform so a plugin's
/// `this.resolve` is a local lookup instead of a per-import host round-trip.
pub fn imports(source_text: &str, path: &Path) -> Vec<String> {
    with_program(source_text, path, |program| {
        let mut seen = HashSet::new();
        program
            .body
            .iter()
            .filter_map(|stmt| match stmt {
                Statement::ImportDeclaration(decl) => Some(decl.source.value.as_str()),
                Statement::ExportAllDeclaration(decl) => Some(decl.source.value.as_str()),
                _ => None,
            })
            .filter(|s| seen.insert(*s))
            .map(str::to_string)
            .collect()
    })
}

/// Rewrites static import/export-from sources and string-literal dynamic
/// `import()` specifiers by splicing `rewrite`'s answers over the literals,
/// every other byte kept: a pre-bundled dep is too large to reprint per
/// request. `None` when nothing was rewritten or the source does not parse.
pub fn rewrite_specifiers(
    source_text: &str,
    path: &Path,
    mut rewrite: impl FnMut(&str) -> Option<String>,
) -> Option<String> {
    let mut edits = with_program(source_text, path, |program| {
        let mut found = SpecifierEdits {
            rewrite: &mut rewrite,
            edits: Vec::new(),
        };
        for stmt in &program.body {
            match stmt {
                Statement::ImportDeclaration(decl) => found.literal(&decl.source),
                Statement::ExportFromDeclaration(decl) => found.literal(&decl.source),
                Statement::ExportAllDeclaration(decl) => found.literal(&decl.source),
                _ => {}
            }
        }
        if scan(&F_IMPORT_PAREN, source_text) {
            found.visit_program(program);
        }
        found.edits
    });
    if edits.is_empty() {
        return None;
    }
    edits.sort_unstable_by_key(|(span, _)| span.start);
    let mut out = String::with_capacity(source_text.len() + edits.len() * 16);
    let mut last = 0;
    for (span, spec) in edits {
        out.push_str(&source_text[last..span.start as usize]);
        out.push_str(&serde_json::Value::String(spec).to_string());
        last = span.end as usize;
    }
    out.push_str(&source_text[last..]);
    Some(out)
}

struct SpecifierEdits<'r> {
    rewrite: &'r mut dyn FnMut(&str) -> Option<String>,
    edits: Vec<(Span, String)>,
}

impl SpecifierEdits<'_> {
    fn literal(&mut self, lit: &StringLiteral) {
        if let Some(spec) = (self.rewrite)(lit.value.as_str()) {
            self.edits.push((lit.span, spec));
        }
    }
}

impl<'a> Visit<'a> for SpecifierEdits<'_> {
    fn visit_import_expression(&mut self, it: &ImportExpression<'a>) {
        if let Expression::StringLiteral(lit) = &it.source {
            self.literal(lit);
        }
        walk::walk_import_expression(self, it);
    }
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

    #[test]
    fn rewrite_specifiers_splices_import_sources_and_nothing_else() {
        let src = concat!(
            "import { a } from \"./a.mjs\";\n",
            "export { b } from './a.mjs';\n",
            "export * from \"./a.mjs\";\n",
            "import \"./side.mjs\";\n",
            "const s = \"./a.mjs\"; // import \"./a.mjs\"\n",
            "const d = () => import(\"./a.mjs\");\n",
            "const e = (x) => import(x);\n",
        );
        let out = rewrite_specifiers(src, Path::new("chunk.mjs"), |spec| {
            (spec == "./a.mjs").then(|| "./a.mjs?v=1".to_string())
        });
        assert_eq!(
            out.as_deref(),
            Some(concat!(
                "import { a } from \"./a.mjs?v=1\";\n",
                "export { b } from \"./a.mjs?v=1\";\n",
                "export * from \"./a.mjs?v=1\";\n",
                "import \"./side.mjs\";\n",
                "const s = \"./a.mjs\"; // import \"./a.mjs\"\n",
                "const d = () => import(\"./a.mjs?v=1\");\n",
                "const e = (x) => import(x);\n",
            ))
        );
        assert!(
            rewrite_specifiers(src, Path::new("chunk.mjs"), |_| None).is_none(),
            "no edit, no copy"
        );
    }
}
