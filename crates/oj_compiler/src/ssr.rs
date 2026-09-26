// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::collections::{HashMap, HashSet};
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    BindingPattern, Declaration, ExportDefaultDeclarationKind, ExportSpecifier, Expression,
    ImportDeclarationSpecifier, ImportExpression, ImportMeta, ModuleExportName, ObjectProperty,
    PropertyKey, Statement,
};
use oxc_ast_visit::{walk, Visit};
use oxc_parser::Parser;
use oxc_semantic::{ReferenceId, Scoping, SemanticBuilder, SymbolId};
use oxc_span::{GetSpan, SourceType};

struct Edit {
    start: u32,
    end: u32,
    text: String,
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c == '_' || c == '$' || c.is_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c == '$' || c.is_alphanumeric())
}

fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("{s:?}"))
}

fn member(uid: usize, name: &str) -> String {
    if is_ident(name) {
        format!("__vite_ssr_import_{uid}__.{name}")
    } else {
        format!("__vite_ssr_import_{uid}__[{}]", json_str(name))
    }
}

fn export_name(name: &str, local_expr: &str) -> String {
    format!(
        "__vite_ssr_exportName__({}, () => {{ try {{ return {local_expr} }} catch {{}} }});",
        json_str(name)
    )
}

fn men_name(m: &ModuleExportName) -> String {
    match m {
        ModuleExportName::IdentifierName(i) => i.name.to_string(),
        ModuleExportName::IdentifierReference(i) => i.name.to_string(),
        ModuleExportName::StringLiteral(s) => s.value.to_string(),
    }
}

fn men_key(key: &PropertyKey) -> String {
    match key {
        PropertyKey::StaticIdentifier(i) => i.name.to_string(),
        PropertyKey::StringLiteral(s) => json_str(&s.value),
        _ => "?".into(),
    }
}

struct RefCollector<'a, 'b> {
    scoping: &'b Scoping,
    imports: &'b HashMap<SymbolId, String>,
    edits: Vec<Edit>,
    handled: HashSet<u32>,
    _marker: std::marker::PhantomData<&'a ()>,
}

impl<'a, 'b> RefCollector<'a, 'b> {
    fn repl_for_ref(&self, reference_id: Option<ReferenceId>) -> Option<&'b String> {
        let rid = reference_id?;
        let sym = self.scoping.get_reference(rid).symbol_id()?;
        self.imports.get(&sym)
    }
}

impl<'a, 'b> Visit<'a> for RefCollector<'a, 'b> {
    fn visit_expression(&mut self, expr: &Expression<'a>) {
        if let Expression::ImportMeta(m) = expr {
            self.visit_import_meta(m);
            return;
        }
        walk::walk_expression(self, expr);
    }

    fn visit_import_meta(&mut self, m: &ImportMeta) {
        self.edits.push(Edit {
            start: m.span.start,
            end: m.span.end,
            text: "__vite_ssr_import_meta__".into(),
        });
    }

    fn visit_object_property(&mut self, prop: &ObjectProperty<'a>) {
        if prop.shorthand {
            if let Expression::Identifier(id) = &prop.value {
                if let Some(repl) = self.repl_for_ref(id.reference_id.get()) {
                    let key = men_key(&prop.key);
                    self.edits.push(Edit {
                        start: id.span.start,
                        end: id.span.end,
                        text: format!("{key}: (0, {repl})"),
                    });
                    self.handled.insert(id.span.start);
                }
            }
        }
        walk::walk_object_property(self, prop);
    }

    fn visit_identifier_reference(&mut self, id: &oxc_ast::ast::IdentifierReference<'a>) {
        if self.handled.contains(&id.span.start) {
            return;
        }
        if let Some(repl) = self.repl_for_ref(id.reference_id.get()) {
            self.edits.push(Edit {
                start: id.span.start,
                end: id.span.end,
                text: format!("(0, {repl})"),
            });
        }
    }

    fn visit_import_expression(&mut self, e: &ImportExpression<'a>) {
        self.edits.push(Edit {
            start: e.span.start,
            end: e.span.start + 6,
            text: "__vite_ssr_dynamic_import__".into(),
        });
        walk::walk_import_expression(self, e);
    }
}

pub fn ssr_transform(source: &str, path: &Path) -> String {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs());
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        return source.to_string();
    }
    let program = parsed.program;
    let scoping = SemanticBuilder::new()
        .build(&program)
        .semantic
        .into_scoping();

    let mut imports: HashMap<SymbolId, String> = HashMap::new();
    let mut edits: Vec<Edit> = Vec::new();
    let mut hoisted: Vec<String> = Vec::new();
    let mut uid = 0usize;

    let import_const = |uid: usize, src: &str, names: &[String]| -> String {
        let meta = if names.is_empty() {
            String::new()
        } else {
            format!(
                ", {{\"importedNames\":[{}]}}",
                names
                    .iter()
                    .map(|n| json_str(n))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        };
        format!(
            "const __vite_ssr_import_{uid}__ = await __vite_ssr_import__({}{meta});",
            json_str(src)
        )
    };

    for stmt in &program.body {
        match stmt {
            Statement::ImportDeclaration(imp) => {
                if imp.import_kind.is_type() {
                    edits.push(Edit {
                        start: imp.span.start,
                        end: imp.span.end,
                        text: String::new(),
                    });
                    continue;
                }
                let src = imp.source.value.as_str();
                let mut names: Vec<String> = Vec::new();
                if let Some(specs) = &imp.specifiers {
                    for spec in specs {
                        match spec {
                            ImportDeclarationSpecifier::ImportSpecifier(s)
                                if s.import_kind.is_type() => {}
                            ImportDeclarationSpecifier::ImportSpecifier(s) => {
                                let name = men_name(&s.imported);
                                names.push(name.clone());
                                if let Some(sym) = s.local.symbol_id.get() {
                                    imports.insert(sym, member(uid, &name));
                                }
                            }
                            ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                                names.push("default".into());
                                if let Some(sym) = s.local.symbol_id.get() {
                                    imports.insert(sym, member(uid, "default"));
                                }
                            }
                            ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                                if let Some(sym) = s.local.symbol_id.get() {
                                    imports.insert(sym, format!("__vite_ssr_import_{uid}__"));
                                }
                            }
                        }
                    }
                }
                hoisted.push(import_const(uid, src, &names));
                edits.push(Edit {
                    start: imp.span.start,
                    end: imp.span.end,
                    text: String::new(),
                });
                uid += 1;
            }
            Statement::ExportDeclaration(exp) => {
                for name in declared_names(&exp.declaration) {
                    hoisted.push(export_name(&name, &name));
                }
                let decl_start = exp.declaration.span().start;
                edits.push(Edit {
                    start: exp.span.start,
                    end: decl_start,
                    text: String::new(),
                });
            }
            Statement::ExportFromDeclaration(exp) => {
                let value_specs: Vec<&ExportSpecifier> = exp
                    .specifiers
                    .iter()
                    .filter(|s| !s.export_kind.is_type())
                    .collect();
                if exp.export_kind.is_type() || value_specs.is_empty() {
                    edits.push(Edit {
                        start: exp.span.start,
                        end: exp.span.end,
                        text: String::new(),
                    });
                    continue;
                }
                let names: Vec<String> = value_specs.iter().map(|s| men_name(&s.local)).collect();
                let cur = uid;
                hoisted.push(import_const(cur, exp.source.value.as_str(), &names));
                uid += 1;
                for s in &value_specs {
                    let local = men_name(&s.local);
                    let exported = men_name(&s.exported);
                    hoisted.push(export_name(&exported, &member(cur, &local)));
                }
                edits.push(Edit {
                    start: exp.span.start,
                    end: exp.span.end,
                    text: String::new(),
                });
            }
            Statement::ExportNamedDeclaration(exp) => {
                if !exp.export_kind.is_type() {
                    for s in exp.specifiers.iter().filter(|s| !s.export_kind.is_type()) {
                        let exported = men_name(&s.exported);
                        let local_expr = resolve_local_symbol(&scoping, s)
                            .and_then(|sym| imports.get(&sym).cloned())
                            .unwrap_or_else(|| men_name(&s.local));
                        hoisted.push(export_name(&exported, &local_expr));
                    }
                }
                edits.push(Edit {
                    start: exp.span.start,
                    end: exp.span.end,
                    text: String::new(),
                });
            }
            Statement::ExportDefaultDeclaration(exp) => match &exp.declaration {
                ExportDefaultDeclarationKind::FunctionDeclaration(f) if f.id.is_some() => {
                    let name = f.id.as_ref().unwrap().name.to_string();
                    hoisted.push(export_name("default", &name));
                    edits.push(Edit {
                        start: exp.span.start,
                        end: f.span.start,
                        text: String::new(),
                    });
                }
                ExportDefaultDeclarationKind::ClassDeclaration(c) if c.id.is_some() => {
                    let name = c.id.as_ref().unwrap().name.to_string();
                    hoisted.push(export_name("default", &name));
                    edits.push(Edit {
                        start: exp.span.start,
                        end: c.span.start,
                        text: String::new(),
                    });
                }
                _ => {
                    hoisted.push(export_name("default", "__vite_ssr_export_default__"));
                    let expr_start = exp.declaration.span().start;
                    edits.push(Edit {
                        start: exp.span.start,
                        end: expr_start,
                        text: "const __vite_ssr_export_default__ = ".into(),
                    });
                }
            },
            Statement::ExportAllDeclaration(exp) => {
                if exp.export_kind.is_type() {
                    edits.push(Edit {
                        start: exp.span.start,
                        end: exp.span.end,
                        text: String::new(),
                    });
                    continue;
                }
                let cur = uid;
                hoisted.push(import_const(cur, exp.source.value.as_str(), &[]));
                uid += 1;
                if let Some(exported) = &exp.exported {
                    hoisted.push(export_name(
                        &men_name(exported),
                        &format!("__vite_ssr_import_{cur}__"),
                    ));
                } else {
                    hoisted.push(format!(
                        "__vite_ssr_exportAll__(__vite_ssr_import_{cur}__);"
                    ));
                }
                edits.push(Edit {
                    start: exp.span.start,
                    end: exp.span.end,
                    text: String::new(),
                });
            }
            _ => {}
        }
    }

    let mut collector = RefCollector {
        scoping: &scoping,
        imports: &imports,
        edits: Vec::new(),
        handled: HashSet::new(),
        _marker: std::marker::PhantomData,
    };
    collector.visit_program(&program);
    edits.extend(collector.edits);

    apply(source, edits, &hoisted)
}

pub fn ssr_transform_module(
    path: &Path,
    source: &str,
    opts: &crate::CompileOptions,
) -> Result<String, crate::CompileError> {
    ssr_transform_module_with_map(path, source, opts).map(|(code, _)| code)
}

/// The transformed module plus the compile step's source map (a data URL). The
/// SSR rewrite is line-preserving, so the map still locates every line of the
/// result in the original source (Vite's ssrFixStacktrace input).
pub fn ssr_transform_module_with_map(
    path: &Path,
    source: &str,
    opts: &crate::CompileOptions,
) -> Result<(String, Option<String>), crate::CompileError> {
    let compiled = crate::compile(path, source, opts)?;
    Ok((ssr_transform(&compiled.code, path), compiled.map_data_url))
}

fn resolve_local_symbol(scoping: &Scoping, s: &ExportSpecifier) -> Option<SymbolId> {
    if let ModuleExportName::IdentifierReference(r) = &s.local {
        let rid = r.reference_id.get()?;
        return scoping.get_reference(rid).symbol_id();
    }
    None
}

fn declared_names(decl: &Declaration) -> Vec<String> {
    let mut out = Vec::new();
    match decl {
        Declaration::VariableDeclaration(v) => {
            for d in &v.declarations {
                collect_pattern_names(&d.id, &mut out);
            }
        }
        Declaration::FunctionDeclaration(f) => {
            if let Some(id) = &f.id {
                out.push(id.name.to_string());
            }
        }
        Declaration::ClassDeclaration(c) => {
            if let Some(id) = &c.id {
                out.push(id.name.to_string());
            }
        }
        _ => {}
    }
    out
}

fn collect_pattern_names(pat: &BindingPattern, out: &mut Vec<String>) {
    match pat {
        BindingPattern::BindingIdentifier(id) => out.push(id.name.to_string()),
        BindingPattern::ObjectPattern(o) => {
            for p in &o.properties {
                collect_pattern_names(&p.value, out);
            }
            if let Some(rest) = &o.rest {
                collect_pattern_names(&rest.argument, out);
            }
        }
        BindingPattern::ArrayPattern(a) => {
            for el in a.elements.iter().flatten() {
                collect_pattern_names(el, out);
            }
            if let Some(rest) = &a.rest {
                collect_pattern_names(&rest.argument, out);
            }
        }
        BindingPattern::AssignmentPattern(a) => collect_pattern_names(&a.left, out),
    }
}

// Line-preserving: the hoisted imports/exports share the first line with the
// module's own code, and a removed span (an import statement, an `export`
// keyword) keeps its line breaks, so every line keeps its number and the
// compile step's source map still applies line for line (Vite gets the same
// effect from magic-string; the runner's stack rewriting relies on it).
fn apply(source: &str, mut edits: Vec<Edit>, hoisted: &[String]) -> String {
    edits.sort_by_key(|e| (e.start, e.end));
    let mut prefix_end = 0usize;
    if source.starts_with("#!") {
        prefix_end = source.find('\n').map(|i| i + 1).unwrap_or(source.len());
    }
    let mut out =
        String::with_capacity(source.len() + hoisted.iter().map(|h| h.len() + 1).sum::<usize>());
    out.push_str(&source[..prefix_end]);
    for line in hoisted {
        out.push_str(line);
        out.push(' ');
    }
    let mut pos = prefix_end as u32;
    for e in &edits {
        if e.start < pos {
            continue;
        }
        out.push_str(&source[pos as usize..e.start as usize]);
        out.push_str(&e.text);
        let removed = source[e.start as usize..e.end as usize]
            .matches('\n')
            .count();
        let kept = e.text.matches('\n').count();
        for _ in kept..removed {
            out.push('\n');
        }
        pos = e.end;
    }
    out.push_str(&source[pos as usize..]);
    out
}

#[cfg(test)]
mod tests {
    use super::ssr_transform;
    use std::path::Path;

    fn t(src: &str) -> String {
        ssr_transform(src, Path::new("m.js"))
    }

    fn tts(src: &str) -> String {
        ssr_transform(src, Path::new("m.ts"))
    }

    #[test]
    fn default_import() {
        let o = t("import foo from 'vue';console.log(foo.bar)");
        assert!(
            o.contains(r#"await __vite_ssr_import__("vue", {"importedNames":["default"]})"#),
            "{o}"
        );
        assert!(o.contains("__vite_ssr_import_0__.default"), "{o}");
        assert!(!o.contains("import foo from"), "{o}");
    }

    #[test]
    fn named_import_call_wrapped() {
        let o = t("import { ref } from 'vue';function foo() { return ref(0) }");
        assert!(o.contains(r#"{"importedNames":["ref"]}"#), "{o}");
        assert!(o.contains("(0, __vite_ssr_import_0__.ref)(0)"), "{o}");
    }

    #[test]
    fn namespace_import_no_metadata() {
        let o = t("import * as vue from 'vue';vue.ref(0)");
        assert!(o.contains(r#"await __vite_ssr_import__("vue")"#), "{o}");
        assert!(!o.contains("importedNames"), "{o}");
        assert!(o.contains("(0, __vite_ssr_import_0__).ref(0)"), "{o}");
    }

    #[test]
    fn export_function_decl() {
        let o = t("export function foo() {}");
        assert!(
            o.contains(r#"__vite_ssr_exportName__("foo", () => { try { return foo } catch {} });"#),
            "{o}"
        );
        assert!(o.contains("function foo() {}"), "{o}");
        assert!(!o.contains("export function"), "{o}");
    }

    #[test]
    fn export_const_multiple() {
        let o = t("export const a = 1, b = 2");
        assert!(o.contains(r#"__vite_ssr_exportName__("a""#), "{o}");
        assert!(o.contains(r#"__vite_ssr_exportName__("b""#), "{o}");
        assert!(o.contains("const a = 1, b = 2"), "{o}");
    }

    #[test]
    fn specifier_export() {
        let o = t("const a = 1, b = 2; export { a, b as c }");
        assert!(
            o.contains(r#"__vite_ssr_exportName__("a", () => { try { return a } catch {} });"#),
            "{o}"
        );
        assert!(
            o.contains(r#"__vite_ssr_exportName__("c", () => { try { return b } catch {} });"#),
            "{o}"
        );
    }

    #[test]
    fn re_export_from() {
        let o = t("export { ref, computed as c } from 'vue'");
        assert!(o.contains(r#"{"importedNames":["ref","computed"]}"#), "{o}");
        assert!(o.contains(r#"return __vite_ssr_import_0__.ref"#), "{o}");
        assert!(
            o.contains(r#"return __vite_ssr_import_0__.computed"#),
            "{o}"
        );
    }

    #[test]
    fn export_all() {
        let o = t("export * from 'vue'");
        assert!(o.contains(r#"await __vite_ssr_import__("vue")"#), "{o}");
        assert!(
            o.contains("__vite_ssr_exportAll__(__vite_ssr_import_0__);"),
            "{o}"
        );
    }

    #[test]
    fn export_all_as_ns() {
        let o = t("export * as foo from 'vue'");
        assert!(
            o.contains(
                r#"__vite_ssr_exportName__("foo", () => { try { return __vite_ssr_import_0__ }"#
            ),
            "{o}"
        );
    }

    #[test]
    fn export_default_expr() {
        let o = t("export default {}");
        assert!(o.contains("const __vite_ssr_export_default__ = {}"), "{o}");
        assert!(o.contains(r#"__vite_ssr_exportName__("default", () => { try { return __vite_ssr_export_default__ }"#), "{o}");
    }

    #[test]
    fn export_default_named_function() {
        let o = t("export default function foo() {}\nfoo.prototype = {};");
        assert!(
            o.contains(r#"__vite_ssr_exportName__("default", () => { try { return foo }"#),
            "{o}"
        );
        assert!(o.contains("function foo() {}"), "{o}");
        assert!(!o.contains("export default"), "{o}");
    }

    #[test]
    fn dynamic_import() {
        let o = t("export const i = () => import('./foo')");
        assert!(o.contains("__vite_ssr_dynamic_import__('./foo')"), "{o}");
    }

    #[test]
    fn import_meta() {
        let o = t("console.log(import.meta.url)");
        assert!(o.contains("__vite_ssr_import_meta__.url"), "{o}");
    }

    #[test]
    fn hoist_import_to_top() {
        let o = t("path.resolve('x');import path from 'node:path';");
        // the import const must precede the use
        let import_at = o.find("__vite_ssr_import__").unwrap();
        let use_at = o.find(".resolve").unwrap();
        assert!(import_at < use_at, "{o}");
        assert!(
            o.contains("(0, __vite_ssr_import_0__.default).resolve"),
            "{o}"
        );
    }

    #[test]
    fn shadowed_local_not_rewritten() {
        let o = t("import { fn } from 'vue';function A(){ const fn = () => {}; return fn; }");
        assert!(
            o.contains("const fn = () => {}; return fn;"),
            "shadowed fn rewritten: {o}"
        );
    }

    #[test]
    fn shorthand_property_expanded() {
        let o = t("import { inject } from 'vue';const a = { inject }");
        assert!(
            o.contains("{ inject: (0, __vite_ssr_import_0__.inject) }"),
            "{o}"
        );
    }

    #[test]
    fn method_key_not_rewritten_call_is() {
        let o = t("import { fn } from 'vue';class A { fn() { fn() } }");
        assert!(
            o.contains("fn() { (0, __vite_ssr_import_0__.fn)() }"),
            "{o}"
        );
    }

    #[test]
    fn type_only_import_is_dropped() {
        let o = tts("import type { X } from './t';\nconst y = 1;");
        assert!(
            !o.contains("__vite_ssr_import__"),
            "type import emitted a runtime import: {o}"
        );
        assert!(!o.contains("import type"), "{o}");
    }

    #[test]
    fn inline_type_specifier_is_skipped() {
        let o = tts("import { a, type B } from './m';\nconsole.log(a)");
        assert!(
            o.contains(r#"{"importedNames":["a"]}"#),
            "type spec leaked into importedNames: {o}"
        );
        assert!(o.contains("(0, __vite_ssr_import_0__.a)"), "{o}");
        assert!(
            !o.contains("__vite_ssr_import_0__.B"),
            "type spec was referenced: {o}"
        );
    }

    #[test]
    fn type_only_export_from_is_dropped() {
        let o = tts("export type { T } from './t';\nexport const v = 1;");
        assert!(
            !o.contains("__vite_ssr_import__"),
            "type re-export emitted a runtime import: {o}"
        );
        assert!(o.contains(r#"__vite_ssr_exportName__("v""#), "{o}");
    }

    #[test]
    fn type_only_export_star_is_dropped() {
        let o = tts("export type * from './t';\nexport const v = 1;");
        assert!(
            !o.contains("__vite_ssr_import__"),
            "type export* emitted a runtime import: {o}"
        );
        assert!(!o.contains("__vite_ssr_exportAll__"), "{o}");
        assert!(o.contains(r#"__vite_ssr_exportName__("v""#), "{o}");
    }

    #[test]
    fn inline_type_export_specifier_is_skipped() {
        let o = tts("const a = 1; export { a, type T }");
        assert!(o.contains(r#"__vite_ssr_exportName__("a""#), "{o}");
        assert!(
            !o.contains(r#"__vite_ssr_exportName__("T""#),
            "type export spec leaked: {o}"
        );
    }

    #[test]
    fn composes_ts_strip_then_ssr_transform() {
        use super::ssr_transform_module;
        use crate::CompileOptions;
        let src =
            "import { helper } from './u';\nexport const x: number = helper(1);\nexport default 2;";
        let o = ssr_transform_module(Path::new("c.ts"), src, &CompileOptions::prod()).unwrap();
        assert!(!o.contains("import { helper"), "import survived: {o}");
        assert!(o.contains(r#"await __vite_ssr_import__("./u""#), "{o}");
        assert!(o.contains(".helper)("), "ref rewritten to member call: {o}");
        assert!(o.contains(r#"__vite_ssr_exportName__("x""#), "{o}");
        assert!(o.contains(r#"__vite_ssr_exportName__("default""#), "{o}");
        assert!(!o.contains(": number"), "TS type survived: {o}");
    }

    #[test]
    fn rewrite_keeps_every_line_in_place() {
        let src = "import {\n  a,\n  b,\n} from './u';\nimport d from './d';\nexport const x = a(b);\nexport default function f() {\n  return d + x;\n}\n";
        let o = t(src);
        assert_eq!(o.lines().count(), src.lines().count(), "{o}");
        let out_lines: Vec<&str> = o.lines().collect();
        let src_lines: Vec<&str> = src.lines().collect();
        assert!(
            out_lines[0].contains("__vite_ssr_import__(\"./u\""),
            "hoisted on line 1: {o}"
        );
        assert!(
            out_lines[0].contains("__vite_ssr_exportName__(\"x\""),
            "{o}"
        );
        assert!(out_lines[5].contains("const x = "), "line 6 keeps `x`: {o}");
        assert_eq!(src_lines[5], "export const x = a(b);");
        assert!(
            out_lines[6].starts_with("function f() {"),
            "line 7 keeps `f`: {o}"
        );
        assert_eq!(
            out_lines[7],
            "  return (0, __vite_ssr_import_1__.default) + x;"
        );
    }

    #[test]
    fn module_with_map_returns_the_compile_map() {
        use super::ssr_transform_module_with_map;
        use crate::CompileOptions;
        let src = "import { helper } from './u';\nexport const x: number = helper(1);";
        let (code, map) =
            ssr_transform_module_with_map(Path::new("c.ts"), src, &CompileOptions::dev()).unwrap();
        assert!(code.contains("__vite_ssr_import__"), "{code}");
        let map = map.expect("dev compile carries a source map");
        assert!(
            map.starts_with("data:application/json;charset=utf-8;base64,"),
            "{map}"
        );
    }

    #[test]
    fn composes_jsx_strip_then_ssr_transform() {
        use super::ssr_transform_module;
        use crate::CompileOptions;
        let src = "import { wrap } from './ui';\nexport const C = () => wrap(<div className=\"x\">hi</div>);";
        let o = ssr_transform_module(Path::new("c.tsx"), src, &CompileOptions::prod()).unwrap();
        assert!(!o.contains("<div"), "JSX survived: {o}");
        assert!(
            o.contains(r#"await __vite_ssr_import__("./ui""#),
            "user import transformed: {o}"
        );
        assert!(o.contains(".wrap)("), "user import ref rewritten: {o}");
        assert!(o.contains(r#"__vite_ssr_exportName__("C""#), "{o}");
        assert!(
            !o.contains("\nimport "),
            "an import statement survived: {o}"
        );
    }
}
