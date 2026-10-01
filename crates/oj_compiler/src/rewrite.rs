use crate::bundle;
use crate::compile::ImportRewriter;
use oxc_allocator::Allocator;
use oxc_ast::ast::{Program, Statement, StringLiteral};

pub(crate) fn rewrite_module_specifiers_pub<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    rewriter: &mut ImportRewriter,
) -> (Vec<String>, Vec<String>) {
    let mut opt: Option<&mut ImportRewriter> = Some(rewriter);
    let (imports, dynamic_imports, _) = rewrite_module_specifiers(allocator, program, &mut opt);
    (imports, dynamic_imports)
}

/// Per import specifier, the binding names the module uses from it.
type ImportBindings = Vec<(String, Vec<String>)>;

pub(crate) fn rewrite_module_specifiers<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    rewriter: &mut Option<&mut ImportRewriter>,
) -> (Vec<String>, Vec<String>, ImportBindings) {
    let mut imports = Vec::new();
    let mut dynamic_imports = Vec::new();
    let mut bindings: ImportBindings = Vec::new();
    for stmt in program.body.iter_mut() {
        // The names each statement pulls from its source, for partial-accept
        // gating (Vite's importedBindings): a side-effect import records an
        // empty list (nothing used, always within any accepted set) and a
        // namespace or star re-export records `*` (never within one).
        let names: Vec<String> = match stmt {
            Statement::ImportDeclaration(decl) => decl
                .specifiers
                .iter()
                .flatten()
                .map(|s| {
                    use oxc_ast::ast::ImportDeclarationSpecifier as S;
                    match s {
                        S::ImportSpecifier(s) => s.imported.name().to_string(),
                        S::ImportDefaultSpecifier(_) => "default".to_string(),
                        S::ImportNamespaceSpecifier(_) => "*".to_string(),
                    }
                })
                .collect(),
            Statement::ExportFromDeclaration(decl) => decl
                .specifiers
                .iter()
                .map(|s| bundle::export_name(&s.local))
                .collect(),
            Statement::ExportAllDeclaration(_) => vec!["*".to_string()],
            _ => Vec::new(),
        };
        let source: Option<&mut StringLiteral> = match stmt {
            Statement::ImportDeclaration(decl) => Some(&mut decl.source),
            Statement::ExportFromDeclaration(decl) => Some(&mut decl.source),
            Statement::ExportAllDeclaration(decl) => Some(&mut decl.source),
            _ => None,
        };
        let Some(lit) = source else { continue };

        if let Some(rewriter) = rewriter.as_deref_mut() {
            if let Some(new_spec) = rewriter(lit.value.as_str()) {
                lit.value = allocator.alloc_str(&new_spec).into();
                lit.raw = None;
            }
        }
        let spec = lit.value.to_string();
        match bindings.iter_mut().find(|(s, _)| *s == spec) {
            Some((_, existing)) => existing.extend(names),
            None => bindings.push((spec.clone(), names)),
        }
        imports.push(spec);
    }
    if let Some(rewriter) = rewriter.as_deref_mut() {
        let mut dyn_rewriter = DynamicImportRewriter {
            allocator,
            rewriter,
            dynamic: &mut dynamic_imports,
        };
        use oxc_ast_visit::VisitMut;
        dyn_rewriter.visit_program(program);
    }
    for spec in &dynamic_imports {
        match bindings.iter_mut().find(|(s, _)| s == spec) {
            Some((_, existing)) => existing.push("*".to_string()),
            None => bindings.push((spec.clone(), vec!["*".to_string()])),
        }
    }
    (imports, dynamic_imports, bindings)
}

struct DynamicImportRewriter<'a, 'b> {
    allocator: &'a Allocator,
    rewriter: &'b mut ImportRewriter<'b>,
    dynamic: &'b mut Vec<String>,
}

impl<'a> oxc_ast_visit::VisitMut<'a> for DynamicImportRewriter<'a, '_> {
    fn visit_import_expression(&mut self, it: &mut oxc_ast::ast::ImportExpression<'a>) {
        if let oxc_ast::ast::Expression::StringLiteral(lit) = &mut it.source {
            if let Some(new_spec) = (self.rewriter)(lit.value.as_str()) {
                lit.value = self.allocator.alloc_str(&new_spec).into();
                lit.raw = None;
            }
            self.dynamic.push(lit.value.to_string());
        }
        oxc_ast_visit::walk_mut::walk_import_expression(self, it);
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
