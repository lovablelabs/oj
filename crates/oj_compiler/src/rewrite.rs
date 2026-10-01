use crate::bundle;
use crate::compile::ImportRewriter;
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Expression, ImportDeclarationSpecifier, ImportExpression, Program, Statement, StringLiteral,
};
use oxc_ast_visit::{walk_mut, VisitMut};
use std::collections::HashMap;

pub(crate) fn rewrite_module_specifiers_pub<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    rewriter: &mut ImportRewriter,
) -> (Vec<String>, Vec<String>) {
    let mut opt: Option<&mut ImportRewriter> = Some(rewriter);
    let specifiers = rewrite_module_specifiers(allocator, program, &mut opt);
    (specifiers.imports, specifiers.dynamic_imports)
}

/// What [`rewrite_module_specifiers`] collected, specifiers already rewritten.
pub(crate) struct ModuleSpecifiers {
    pub imports: Vec<String>,
    pub dynamic_imports: Vec<String>,
    /// Per specifier, the binding names the module uses from it (Vite's
    /// `importedBindings`), in first-seen order.
    pub bindings: Vec<(String, Vec<String>)>,
}

/// Rewrites static import/export-from sources (and, with a rewriter, dynamic
/// `import("...")` literals) and collects them.
pub(crate) fn rewrite_module_specifiers<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    rewriter: &mut Option<&mut ImportRewriter>,
) -> ModuleSpecifiers {
    let mut imports = Vec::new();
    let mut dynamic_imports = Vec::new();
    let mut bindings = BindingsBuilder::default();
    for stmt in program.body.iter_mut() {
        let names = imported_names(stmt);
        let Some(lit) = static_source(stmt) else {
            continue;
        };
        if let Some(rewriter) = rewriter.as_deref_mut() {
            rewrite_literal(allocator, lit, rewriter);
        }
        let spec = lit.value.to_string();
        bindings.extend(&spec, names);
        imports.push(spec);
    }
    if let Some(rewriter) = rewriter.as_deref_mut() {
        DynamicImportRewriter {
            allocator,
            rewriter,
            dynamic: &mut dynamic_imports,
        }
        .visit_program(program);
    }
    for spec in &dynamic_imports {
        bindings.extend(spec, vec!["*".to_string()]);
    }
    ModuleSpecifiers {
        imports,
        dynamic_imports,
        bindings: bindings.list,
    }
}

/// Swaps a string literal's value for the rewriter's answer, if any.
pub(crate) fn rewrite_literal<'a>(
    allocator: &'a Allocator,
    lit: &mut StringLiteral<'a>,
    rewriter: &mut ImportRewriter,
) {
    if let Some(new_spec) = rewriter(lit.value.as_str()) {
        lit.value = allocator.alloc_str(&new_spec).into();
        lit.raw = None;
    }
}

/// The names a statement pulls from its source, for partial-accept gating: a
/// side-effect import records an empty list (always within any accepted set),
/// a namespace import or star re-export records `*` (never within one).
fn imported_names(stmt: &Statement) -> Vec<String> {
    match stmt {
        Statement::ImportDeclaration(decl) => decl
            .specifiers
            .iter()
            .flatten()
            .map(|s| match s {
                ImportDeclarationSpecifier::ImportSpecifier(s) => s.imported.name().to_string(),
                ImportDeclarationSpecifier::ImportDefaultSpecifier(_) => "default".to_string(),
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(_) => "*".to_string(),
            })
            .collect(),
        Statement::ExportFromDeclaration(decl) => decl
            .specifiers
            .iter()
            .map(|s| bundle::export_name(&s.local))
            .collect(),
        Statement::ExportAllDeclaration(_) => vec!["*".to_string()],
        _ => Vec::new(),
    }
}

fn static_source<'s, 'a>(stmt: &'s mut Statement<'a>) -> Option<&'s mut StringLiteral<'a>> {
    match stmt {
        Statement::ImportDeclaration(decl) => Some(&mut decl.source),
        Statement::ExportFromDeclaration(decl) => Some(&mut decl.source),
        Statement::ExportAllDeclaration(decl) => Some(&mut decl.source),
        _ => None,
    }
}

/// Ordered per-specifier binding lists with a map index, so repeated imports
/// of one specifier merge without a linear scan.
#[derive(Default)]
struct BindingsBuilder {
    list: Vec<(String, Vec<String>)>,
    index: HashMap<String, usize>,
}

impl BindingsBuilder {
    fn extend(&mut self, spec: &str, names: Vec<String>) {
        match self.index.get(spec) {
            Some(&i) => self.list[i].1.extend(names),
            None => {
                self.index.insert(spec.to_string(), self.list.len());
                self.list.push((spec.to_string(), names));
            }
        }
    }
}

struct DynamicImportRewriter<'a, 'b> {
    allocator: &'a Allocator,
    rewriter: &'b mut ImportRewriter<'b>,
    dynamic: &'b mut Vec<String>,
}

impl<'a> VisitMut<'a> for DynamicImportRewriter<'a, '_> {
    fn visit_import_expression(&mut self, it: &mut ImportExpression<'a>) {
        if let Expression::StringLiteral(lit) = &mut it.source {
            rewrite_literal(self.allocator, lit, self.rewriter);
            self.dynamic.push(lit.value.to_string());
        }
        walk_mut::walk_import_expression(self, it);
    }
}

#[cfg(test)]
mod tests {

    use std::path::Path;

    use crate::compile::{compile, compile_module, CompileOptions};

    #[test]
    fn rewrites_relative_specifiers_and_collects_imports() {
        let src = r#"
import { App } from "./App";
export { helper } from "../lib/helper";
import { useState } from "react";
export function Root() {
  const [n] = useState(0);
  return <App key={n} />;
}
"#;
        let mut rewrite = |spec: &str| -> Option<String> {
            spec.starts_with('.')
                .then(|| format!("/resolved{}", spec.trim_start_matches('.')))
        };
        let out = compile_module(
            Path::new("Root.tsx"),
            src,
            &CompileOptions::prod(),
            Some(&mut rewrite),
        )
        .unwrap();
        assert!(out.code.contains("\"/resolved/App\""), "{}", out.code);
        assert!(
            out.code.contains("\"/resolved/lib/helper\""),
            "{}",
            out.code
        );
        assert!(
            out.code.contains("\"react\""),
            "bare imports stay untouched"
        );
        assert!(out.imports.contains(&"/resolved/App".to_string()));
        assert!(out.imports.contains(&"react".to_string()));
        assert!(
            out.imports.iter().any(|i| i.contains("jsx-runtime")),
            "{:?}",
            out.imports
        );
    }

    #[test]
    fn erases_type_only_imports_from_code_and_collected_imports() {
        let src = r#"
import type { A } from "./types";
import { type B, c } from "./mixed";
import { d } from "./real";
export const used: A extends B ? number : number = c + d;
"#;
        let out = compile(Path::new("m.ts"), src, &CompileOptions::prod()).unwrap();
        assert!(
            !out.imports.iter().any(|i| i.contains("types")),
            "type-only import erased: {:?}",
            out.imports
        );
        assert!(
            !out.code.contains("./types"),
            "type-only source gone:\n{}",
            out.code
        );
        assert!(
            out.imports.iter().any(|i| i.contains("mixed")),
            "mixed import kept: {:?}",
            out.imports
        );
        assert!(
            !out.code.contains("type B"),
            "inline type specifier erased:\n{}",
            out.code
        );
        assert!(out.imports.iter().any(|i| i.contains("real")));
    }

    #[test]
    fn rewrites_dynamic_import_specifiers() {
        let src = r#"export async function load() { return import("./chunk"); }"#;
        let mut rewrite = |s: &str| -> Option<String> {
            s.starts_with('.')
                .then(|| format!("/res{}", s.trim_start_matches('.')))
        };
        let out = compile_module(
            Path::new("d.ts"),
            src,
            &CompileOptions::prod(),
            Some(&mut rewrite),
        )
        .unwrap();
        assert!(
            out.code.contains("import(\"/res/chunk\")"),
            "dynamic import rewritten:\n{}",
            out.code
        );
        assert!(
            out.dynamic_imports.contains(&"/res/chunk".to_string()),
            "dynamic spec collected: {:?}",
            out.dynamic_imports
        );
        assert!(
            !out.imports.contains(&"/res/chunk".to_string()),
            "dynamic not in static imports"
        );
    }

    #[test]
    fn import_bindings_name_what_each_importer_uses() {
        let mut rw = |spec: &str| Some(spec.replace("./", "/src/"));
        let out = compile_module(
            Path::new("a.ts"),
            concat!(
                "import { a, b as c } from './mod.js';\n",
                "import def from './mod.js';\n",
                "import * as ns from './star.js';\n",
                "import './side.js';\n",
                "export { x } from './re.js';\n",
                "const d = await import('./dyn.js');\n",
                "console.log(a, c, def, ns, d);\n",
            ),
            &CompileOptions::dev(),
            Some(&mut rw),
        )
        .unwrap();
        let get = |spec: &str| {
            out.import_bindings
                .iter()
                .find(|(s, _)| s == spec)
                .map(|(_, names)| names.clone())
                .unwrap_or_else(|| panic!("{spec} missing from {:?}", out.import_bindings))
        };
        assert_eq!(get("/src/mod.js"), vec!["a", "b", "default"]);
        assert_eq!(get("/src/star.js"), vec!["*"]);
        assert_eq!(get("/src/side.js"), Vec::<String>::new());
        assert_eq!(get("/src/re.js"), vec!["x"]);
        assert_eq!(get("/src/dyn.js"), vec!["*"]);
    }
}
