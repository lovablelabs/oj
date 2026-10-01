// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::collections::HashMap;
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    ExportDefaultDeclarationKind, Expression, ImportDeclarationSpecifier, ModuleExportName,
    Program, Statement,
};
use oxc_ast_visit::{walk_mut, VisitMut};
use oxc_codegen::Codegen;
use oxc_ecmascript::BoundNames;
use oxc_parser::{Parser, ParserReturn};
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_syntax::reference::ReferenceId;
use oxc_transformer::{JsxRuntime, ReactRefreshOptions, TransformOptions, Transformer};

use crate::{CompileError, ImportMetaEnv, ImportRewriter};

#[derive(Debug, Clone, PartialEq)]
pub enum FactoryKind {
    Esm,
    Cjs,
}

#[derive(Debug)]
pub struct FactoryOutput {
    pub code: String,
    pub imports: Vec<String>,
    pub require_map: Vec<(String, String)>,
    pub kind: FactoryKind,
    pub dynamic_imports: Vec<String>,
    pub is_refresh_boundary: bool,
    /// ESM only: the statically-known exported names (excluding `default`).
    /// Empty for CJS. Used by partial bundling to re-export an ESM entry.
    pub esm_named: Vec<String>,
    /// ESM only: the (already-rewritten) sources of `export *` declarations,
    /// so a bundler can follow star barrels for the transitive export set.
    pub esm_star_targets: Vec<String>,
}

impl FactoryOutput {
    pub fn is_boundary(&self) -> bool {
        self.kind == FactoryKind::Esm && self.is_refresh_boundary
    }
}

/// What an ESM factory compile varies on.
#[derive(Clone, Copy)]
struct FactoryOptions<'e> {
    refresh: bool,
    /// The dev server's defines (dev client variant); `None` uses the fallback.
    env: Option<&'e ImportMetaEnv>,
}

pub fn compile_factory(
    path: &Path,
    url: &str,
    source_text: &str,
    resolve: &mut ImportRewriter,
    env: Option<&ImportMetaEnv>,
) -> Result<FactoryOutput, CompileError> {
    let is_dep = url.starts_with("/node_modules/")
        || (url.starts_with("/@fs/") && url.contains("/node_modules/"));
    if !is_dep {
        // App source is ESM.
        let opts = FactoryOptions { refresh: true, env };
        return compile_esm_factory(path, url, source_text, resolve, opts);
    }

    // Dependency: parse once to decide ESM vs CJS and reuse that parse for the
    // ESM pipeline. A file that can't parse as a module, or has no top-level
    // import/export, is CommonJS; the CJS path re-parses it in sloppy mode
    // (which allows `with`, top-level `this` and other script-only forms).
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source_text, source_type_for(path)).parse();
    if parsed.panicked || !has_module_syntax(&parsed.program) {
        return compile_cjs_factory(path, source_text, resolve);
    }
    if !parsed.diagnostics.is_empty() {
        return Err(parse_error(path, source_text, parsed));
    }
    let opts = FactoryOptions {
        refresh: false,
        env,
    };
    compile_esm_factory_from_parsed(
        &allocator,
        parsed.program,
        path,
        url,
        source_text,
        resolve,
        opts,
    )
}

fn source_type_for(path: &Path) -> SourceType {
    SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs())
}

fn has_module_syntax(program: &Program) -> bool {
    program.body.iter().any(|stmt| {
        matches!(
            stmt,
            Statement::ImportDeclaration(_)
                | Statement::ExportDeclaration(_)
                | Statement::ExportNamedDeclaration(_)
                | Statement::ExportFromDeclaration(_)
                | Statement::ExportAllDeclaration(_)
                | Statement::ExportDefaultDeclaration(_)
        )
    })
}

fn parse_error(path: &Path, source_text: &str, parsed: ParserReturn) -> CompileError {
    let message = parsed
        .diagnostics
        .into_iter()
        .map(|d| format!("{:?}", d.with_source_code(source_text.to_string())))
        .collect::<Vec<_>>()
        .join("\n");
    CompileError::Parse {
        path: path.to_path_buf(),
        message,
    }
}

fn compile_cjs_factory(
    path: &Path,
    source_text: &str,
    resolve: &mut ImportRewriter,
) -> Result<FactoryOutput, CompileError> {
    let analyzed = crate::cjs::analyze_for_factory(path, source_text)?;
    let mut require_map = Vec::new();
    let mut imports = Vec::new();
    for spec in &analyzed.requires {
        if let Some(target) = resolve(spec) {
            if !imports.contains(&target) {
                imports.push(target.clone());
            }
            require_map.push((spec.clone(), target));
        }
    }
    Ok(FactoryOutput {
        code: analyzed.body,
        imports,
        require_map,
        kind: FactoryKind::Cjs,
        dynamic_imports: Vec::new(),
        is_refresh_boundary: false,
        esm_named: Vec::new(),
        esm_star_targets: Vec::new(),
    })
}

fn compile_esm_factory(
    path: &Path,
    url: &str,
    source_text: &str,
    resolve: &mut ImportRewriter,
    opts: FactoryOptions,
) -> Result<FactoryOutput, CompileError> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source_text, source_type_for(path)).parse();
    if parsed.panicked || !parsed.diagnostics.is_empty() {
        return Err(parse_error(path, source_text, parsed));
    }
    compile_esm_factory_from_parsed(
        &allocator,
        parsed.program,
        path,
        url,
        source_text,
        resolve,
        opts,
    )
}

/// The ESM factory pipeline over an already-parsed program, so `compile_factory`
/// can reuse the parse it made to detect module syntax.
fn compile_esm_factory_from_parsed<'a>(
    allocator: &'a Allocator,
    mut program: Program<'a>,
    path: &Path,
    url: &str,
    source_text: &str,
    resolve: &mut ImportRewriter,
    opts: FactoryOptions,
) -> Result<FactoryOutput, CompileError> {
    let FactoryOptions { refresh, env } = opts;

    transform_jsx(allocator, path, &mut program, refresh)?;
    let is_refresh_boundary = refresh && crate::detect_refresh_registrations(&program);

    if crate::scan(&crate::F_IMPORT_PAREN, source_text) {
        crate::glob::expand_dynamic_import_vars(
            allocator,
            path.parent().unwrap_or(path),
            &mut program,
            source_text,
        );
    }
    replace_defines(allocator, &mut program, env, source_text);
    if crate::scan(&crate::F_IMPORT_META_GLOB, source_text) {
        crate::glob::expand(allocator, path.parent().unwrap_or(path), &mut program);
    }

    let (_, dynamic_imports) =
        crate::rewrite_module_specifiers_pub(allocator, &mut program, resolve);

    let shape = ModuleShape::collect(&program);
    RefRewriter {
        allocator,
        replacements: &shape.replacements,
        url,
    }
    .visit_program(&mut program);
    lower_to_factory_body(allocator, &mut program, &shape.prologue(), path)?;

    let esm_named = shape
        .getters
        .iter()
        .map(|g| g.name.clone())
        .filter(|n| n != "default")
        .collect();
    let esm_star_targets = shape
        .stars
        .iter()
        .filter_map(|&vi| shape.import_vars.get(vi).cloned())
        .collect();

    let code = Codegen::new().build(&program).code;
    Ok(FactoryOutput {
        code,
        imports: shape.import_vars,
        require_map: Vec::new(),
        kind: FactoryKind::Esm,
        dynamic_imports,
        is_refresh_boundary,
        esm_named,
        esm_star_targets,
    })
}

/// TS strip + automatic dev JSX, plus React Refresh registration for app code.
fn transform_jsx<'a>(
    allocator: &'a Allocator,
    path: &Path,
    program: &mut Program<'a>,
    refresh: bool,
) -> Result<(), CompileError> {
    let scoping = SemanticBuilder::new()
        .with_excess_capacity(2.0)
        .with_enum_eval(true)
        .build(program)
        .semantic
        .into_scoping();
    let mut transform_options = TransformOptions::default();
    transform_options.jsx.jsx_plugin = true;
    transform_options.jsx.runtime = JsxRuntime::Automatic;
    transform_options.jsx.development = true;
    if refresh {
        transform_options.jsx.refresh = Some(ReactRefreshOptions::default());
    }
    let ret =
        Transformer::new(allocator, path, &transform_options).build_with_scoping(scoping, program);
    if ret.diagnostics.is_empty() {
        return Ok(());
    }
    let message = ret
        .diagnostics
        .into_iter()
        .map(|d| format!("{d:?}"))
        .collect::<Vec<_>>()
        .join("\n");
    Err(CompileError::Transform {
        path: path.to_path_buf(),
        message,
    })
}

/// `needed_by` also matches the plain-key defines (`process.env.NODE_ENV` and
/// friends), so an ESM dep mentioning only those still gets replaced instead of
/// hitting a bare `process` in the browser. One snapshot serves both the gate
/// and the config, so a concurrent re-set can't split them.
fn replace_defines<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    env: Option<&ImportMetaEnv>,
    source_text: &str,
) {
    use oxc_transformer_plugins::ReplaceGlobalDefines;
    let defines = crate::defines_for(env, true, false);
    if !defines.needed_by(source_text) {
        return;
    }
    let scoping = SemanticBuilder::new()
        .build(program)
        .semantic
        .into_scoping();
    if let Some(config) = defines.config() {
        let _ = ReplaceGlobalDefines::new(allocator, config).build(scoping, program);
    }
}

/// How an imported binding lowers: `_oj_mN` (namespace) or `_oj_mN.member`.
#[derive(Clone)]
struct Replacement {
    var: String,
    member: Option<String>,
}

impl Replacement {
    fn expr(&self) -> String {
        match &self.member {
            Some(member) => format!("{}.{}", self.var, member),
            None => self.var.clone(),
        }
    }
}

/// One `__oj_esm` getter: exported name -> the expression it reads.
struct Getter {
    name: String,
    expr: String,
}

impl Getter {
    fn new(name: impl Into<String>, expr: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            expr: expr.into(),
        }
    }
}

/// The module's import/export surface, read off the top-level statements.
#[derive(Default)]
struct ModuleShape {
    /// Required targets in first-seen order; index N is `_oj_mN`.
    import_vars: Vec<String>,
    var_of_url: HashMap<String, usize>,
    replacements: HashMap<ReferenceId, Replacement>,
    getters: Vec<Getter>,
    /// `export *` sources, as indices into `import_vars`.
    stars: Vec<usize>,
    /// An anonymous default export, assigned to `__oj_default`.
    has_default_expr: bool,
}

impl ModuleShape {
    fn collect(program: &Program) -> Self {
        let semantic = SemanticBuilder::new()
            .with_build_nodes(true)
            .build(program)
            .semantic;
        let mut shape = Self::default();
        // Imported local name -> its lowered expression. A re-export `export { x }`
        // of an imported `x` must read that expression, since the `import` is gone.
        let mut import_local_expr: HashMap<String, String> = HashMap::new();

        for stmt in &program.body {
            match stmt {
                Statement::ImportDeclaration(decl) => {
                    let vi = shape.var_for(decl.source.value.as_str());
                    for spec in decl.specifiers.iter().flatten() {
                        let (local, member) = match spec {
                            ImportDeclarationSpecifier::ImportSpecifier(s) => {
                                (&s.local, Some(export_name(&s.imported)))
                            }
                            ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                                (&s.local, Some("default".to_string()))
                            }
                            ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                                (&s.local, None)
                            }
                        };
                        let lowered = Replacement {
                            var: format!("_oj_m{vi}"),
                            member,
                        };
                        import_local_expr.insert(local.name.to_string(), lowered.expr());
                        let Some(symbol_id) = local.symbol_id.get() else {
                            continue;
                        };
                        for &reference_id in
                            semantic.scoping().get_resolved_reference_ids(symbol_id)
                        {
                            shape.replacements.insert(reference_id, lowered.clone());
                        }
                    }
                }
                Statement::ExportDeclaration(decl) => {
                    for name in binding_names(&decl.declaration) {
                        shape.getters.push(Getter::new(name.clone(), name));
                    }
                }
                Statement::ExportNamedDeclaration(decl) => {
                    for spec in &decl.specifiers {
                        let local = export_name(&spec.local);
                        let expr = import_local_expr.get(&local).cloned().unwrap_or(local);
                        shape
                            .getters
                            .push(Getter::new(export_name(&spec.exported), expr));
                    }
                }
                Statement::ExportFromDeclaration(decl) => {
                    let vi = shape.var_for(decl.source.value.as_str());
                    for spec in &decl.specifiers {
                        let local = export_name(&spec.local);
                        let expr = format!("_oj_m{vi}.{local}");
                        shape
                            .getters
                            .push(Getter::new(export_name(&spec.exported), expr));
                    }
                }
                Statement::ExportAllDeclaration(decl) => {
                    let vi = shape.var_for(decl.source.value.as_str());
                    shape.stars.push(vi);
                }
                Statement::ExportDefaultDeclaration(decl) => {
                    let expr = match named_default_decl(&decl.declaration) {
                        Some(name) => name,
                        None => {
                            shape.has_default_expr = true;
                            "__oj_default".to_string()
                        }
                    };
                    shape.getters.push(Getter::new("default", expr));
                }
                _ => {}
            }
        }
        shape
    }

    fn var_for(&mut self, url: &str) -> usize {
        if let Some(&i) = self.var_of_url.get(url) {
            return i;
        }
        let i = self.import_vars.len();
        self.import_vars.push(url.to_string());
        self.var_of_url.insert(url.to_string(), i);
        i
    }

    /// Factory prologue: export getters first (so circular importers see
    /// them), then
    /// one `__oj_require` per target, then star re-exports.
    fn prologue(&self) -> String {
        let mut prologue = String::new();
        if self.has_default_expr {
            prologue.push_str("var __oj_default;\n");
        }
        if self.getters.is_empty() {
            prologue.push_str("__oj_esm(__oj_exports, {});\n");
        } else {
            let entries: Vec<String> = self
                .getters
                .iter()
                .map(|g| format!("{:?}: () => {}", g.name, g.expr))
                .collect();
            prologue.push_str(&format!(
                "__oj_esm(__oj_exports, {{ {} }});\n",
                entries.join(", ")
            ));
        }
        for (i, target) in self.import_vars.iter().enumerate() {
            prologue.push_str(&format!("var _oj_m{i} = __oj_require({target:?});\n"));
        }
        for vi in &self.stars {
            prologue.push_str(&format!("__oj_export_star(_oj_m{vi}, __oj_exports);\n"));
        }
        prologue
    }
}

/// The local name of `export default function f` / `export default class C`.
fn named_default_decl(kind: &ExportDefaultDeclarationKind) -> Option<String> {
    match kind {
        ExportDefaultDeclarationKind::FunctionDeclaration(f) => f.id.as_ref(),
        ExportDefaultDeclarationKind::ClassDeclaration(c) => c.id.as_ref(),
        _ => None,
    }
    .map(|id| id.name.to_string())
}

/// Replace the body with the prologue followed by the original statements, with
/// imports and re-exports dropped and export declarations unwrapped.
fn lower_to_factory_body<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    prologue: &str,
    path: &Path,
) -> Result<(), CompileError> {
    let old_body = std::mem::replace(&mut program.body, oxc_allocator::Vec::new_in(&allocator));
    let mut new_body = oxc_allocator::Vec::new_in(&allocator);
    new_body.extend(parse_snippet(allocator, prologue, path)?);

    for stmt in old_body {
        match stmt {
            Statement::ImportDeclaration(_)
            | Statement::ExportNamedDeclaration(_)
            | Statement::ExportFromDeclaration(_)
            | Statement::ExportAllDeclaration(_) => {}
            Statement::ExportDeclaration(decl) => {
                new_body.push(Statement::from(decl.unbox().declaration));
            }
            Statement::ExportDefaultDeclaration(decl) => {
                use ExportDefaultDeclarationKind as K;
                let expr = match decl.unbox().declaration {
                    K::FunctionDeclaration(f) if f.id.is_some() => {
                        new_body.push(Statement::FunctionDeclaration(f));
                        continue;
                    }
                    K::ClassDeclaration(c) if c.id.is_some() => {
                        new_body.push(Statement::ClassDeclaration(c));
                        continue;
                    }
                    K::FunctionDeclaration(f) => Expression::FunctionExpression(f),
                    K::ClassDeclaration(c) => Expression::ClassExpression(c),
                    kind => kind.into_expression(),
                };
                push_default_assignment(allocator, &mut new_body, expr, path)?;
            }
            other => new_body.push(other),
        }
    }
    program.body = new_body;
    Ok(())
}

fn push_default_assignment<'a>(
    allocator: &'a Allocator,
    body: &mut oxc_allocator::Vec<'a, Statement<'a>>,
    expr: Expression<'a>,
    path: &Path,
) -> Result<(), CompileError> {
    let mut stmts = parse_snippet(allocator, "__oj_default = 0;", path)?;
    let mut stmt = stmts.pop().expect("snippet has one statement");
    if let Statement::ExpressionStatement(es) = &mut stmt {
        if let Expression::AssignmentExpression(assign) = &mut es.expression {
            assign.right = expr;
        }
    }
    body.push(stmt);
    Ok(())
}

fn parse_snippet<'a>(
    allocator: &'a Allocator,
    source: &str,
    path: &Path,
) -> Result<Vec<Statement<'a>>, CompileError> {
    let source: &'a str = allocator.alloc_str(source);
    let parsed = Parser::new(allocator, source, SourceType::cjs()).parse();
    if parsed.panicked {
        return Err(CompileError::Transform {
            path: path.to_path_buf(),
            message: format!("internal: snippet failed to parse: {source}"),
        });
    }
    Ok(parsed.program.body.into_iter().collect())
}

pub(crate) fn export_name(name: &ModuleExportName) -> String {
    match name {
        ModuleExportName::IdentifierName(n) => n.name.to_string(),
        ModuleExportName::IdentifierReference(n) => n.name.to_string(),
        ModuleExportName::StringLiteral(s) => s.value.to_string(),
    }
}

pub(crate) fn binding_names(declaration: &oxc_ast::ast::Declaration) -> Vec<String> {
    use oxc_ast::ast::Declaration as D;
    let mut names = Vec::new();
    match declaration {
        D::VariableDeclaration(var) => {
            for declarator in &var.declarations {
                declarator
                    .id
                    .bound_names(&mut |ident| names.push(ident.name.to_string()));
            }
        }
        D::FunctionDeclaration(f) => {
            if let Some(id) = &f.id {
                names.push(id.name.to_string());
            }
        }
        D::ClassDeclaration(c) => {
            if let Some(id) = &c.id {
                names.push(id.name.to_string());
            }
        }
        _ => {}
    }
    names
}

/// Rewrites imported-binding references to their `_oj_mN` form, `import.meta.url`
/// / `import.meta.hot` to factory-safe forms, and literal `import()` to
/// `__oj_import_lazy`.
struct RefRewriter<'a, 'b> {
    allocator: &'a Allocator,
    replacements: &'b HashMap<ReferenceId, Replacement>,
    url: &'b str,
}

impl<'a> RefRewriter<'a, '_> {
    fn parse_expression(&self, source: &str) -> Option<Expression<'a>> {
        let source: &'a str = self.allocator.alloc_str(source);
        let parsed = Parser::new(self.allocator, source, SourceType::cjs()).parse();
        match parsed.program.body.into_iter().next() {
            Some(Statement::ExpressionStatement(es)) => Some(es.unbox().expression),
            _ => None,
        }
    }
}

impl<'a> VisitMut<'a> for RefRewriter<'a, '_> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        let replacement_src = match &*expr {
            Expression::Identifier(ident) => ident
                .reference_id
                .get()
                .and_then(|id| self.replacements.get(&id))
                .map(Replacement::expr),
            Expression::StaticMemberExpression(member)
                if matches!(member.object, Expression::ImportMeta(_))
                    && member.property.name == "url" =>
            {
                Some(format!(
                    "(typeof location !== \"undefined\" ? location.origin + {0:?} : {0:?})",
                    self.url
                ))
            }
            Expression::StaticMemberExpression(member)
                if matches!(member.object, Expression::ImportMeta(_))
                    && member.property.name == "hot" =>
            {
                Some("module.hot".to_string())
            }
            Expression::ImportExpression(imp) => match &imp.source {
                Expression::StringLiteral(lit) => {
                    Some(format!("__oj_import_lazy({:?})", lit.value.as_str()))
                }
                _ => None,
            },
            _ => None,
        };
        if let Some(src) = replacement_src {
            if let Some(new_expr) = self.parse_expression(&src) {
                *expr = new_expr;
            }
            return;
        }
        walk_mut::walk_expression(self, expr);
    }

    fn visit_object_property(&mut self, prop: &mut oxc_ast::ast::ObjectProperty<'a>) {
        if prop.shorthand {
            if let Expression::Identifier(ident) = &prop.value {
                let replaced = ident
                    .reference_id
                    .get()
                    .is_some_and(|id| self.replacements.contains_key(&id));
                if replaced {
                    prop.shorthand = false;
                }
            }
        }
        walk_mut::walk_object_property(self, prop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn factory(src: &str) -> FactoryOutput {
        let mut resolve = |spec: &str| -> Option<String> {
            (spec.starts_with('.')).then(|| format!("/src{}.tsx", spec.trim_start_matches('.')))
        };
        compile_esm_factory(
            Path::new("Mod.tsx"),
            "/src/Mod.tsx",
            src,
            &mut resolve,
            FactoryOptions {
                refresh: true,
                env: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn reexported_import_binding_uses_lowered_expr() {
        // `export { x }` where `x` is an imported binding must emit a getter over
        // the lowered `_oj_mN.member`, not the bare local (whose import is stripped).
        // Regression: date-fns format.mjs re-exports its imported `longFormatters`.
        let out = factory(
            r#"
import { longFormatters } from "./_lib/longFormatters";
export { longFormatters };
export const x = 1;
"#,
        );
        let code = &out.code;
        assert!(
            code.contains("() => _oj_m0.longFormatters"),
            "re-exported import must reference lowered expr: {code}"
        );
        assert!(
            !code.contains("() => longFormatters"),
            "must not reference the stripped local binding: {code}"
        );
        assert!(
            out.esm_named.contains(&"longFormatters".to_string())
                && out.esm_named.contains(&"x".to_string()),
            "both names exported: {:?}",
            out.esm_named
        );
    }

    #[test]
    fn imports_become_requires_with_member_rewriting() {
        let out = factory(
            r#"
import { useState } from "react";
import Def, { helper as h } from "./util";
import * as NS from "./ns";
export function Widget() {
  const [n] = useState(0);
  return h(Def, NS.thing, n, { h });
}
"#,
        );
        let code = &out.code;
        assert!(
            code.contains(r#"var _oj_m0 = __oj_require("react")"#),
            "{code}"
        );
        assert!(code.contains("_oj_m0.useState(0)"), "{code}");
        assert!(
            code.contains("_oj_m1.helper(_oj_m1.default, _oj_m2.thing"),
            "{code}"
        );
        assert!(
            code.contains("h: _oj_m1.helper"),
            "shorthand must expand: {code}"
        );
        assert!(!code.contains("import "), "{code}");
    }

    #[test]
    fn shadowed_names_are_not_rewritten() {
        let out = factory(
            r#"
import { x } from "./a";
export function f(x) { return x + 1; }
export const y = x;
"#,
        );
        assert!(
            out.code.contains("return x + 1"),
            "param x must stay: {}",
            out.code
        );
        assert!(out.code.contains("const y = _oj_m0.x"), "{}", out.code);
    }

    #[test]
    fn exports_become_getters_installed_before_body() {
        let out = factory(
            r#"
export const a = 1;
export default function App() { return null; }
export { a as b };
export * from "./other";
export { c } from "./third";
"#,
        );
        let code = &out.code;
        let esm_at = code.find("__oj_esm").unwrap();
        let body_at = code.find("const a = 1").unwrap();
        assert!(
            esm_at < body_at,
            "getters must be installed before the body: {code}"
        );
        for expected in [
            r#""a": () => a"#,
            r#""default": () => App"#,
            r#""b": () => a"#,
            "__oj_export_star(",
            r#""c": () => _oj_m"#,
        ] {
            assert!(code.contains(expected), "missing {expected:?} in: {code}");
        }
        assert!(code.contains("function App()"), "hoisted decl kept: {code}");
    }

    #[test]
    fn anonymous_default_export_is_assigned() {
        let out = factory(r#"export default () => 42;"#);
        assert!(out.code.contains("__oj_default = () => 42"), "{}", out.code);
        assert!(
            out.code.contains(r#""default": () => __oj_default"#),
            "{}",
            out.code
        );
    }

    #[test]
    fn side_effect_imports_still_require() {
        let out = factory(r#"import "./global-setup"; export const x = 1;"#);
        assert!(
            out.code
                .contains(r#"__oj_require("/src/global-setup.tsx")"#),
            "{}",
            out.code
        );
    }

    #[test]
    fn cjs_factory_keeps_body_and_maps_requires() {
        let mut resolve = |spec: &str| Some(format!("/node_modules/{spec}/index.js"));
        let out = compile_cjs_factory(
            Path::new("x.js"),
            "var r = require('react'); exports.go = () => r;",
            &mut resolve,
        )
        .unwrap();
        assert_eq!(out.kind, FactoryKind::Cjs);
        assert_eq!(
            out.require_map,
            vec![("react".into(), "/node_modules/react/index.js".into())]
        );
        assert!(out.code.contains("require('react')") || out.code.contains("require(\"react\")"));
        assert!(!out.is_boundary(), "cjs never a refresh boundary");
    }

    #[test]
    fn import_meta_and_refresh_are_factory_safe() {
        let out = factory(
            r#"
export function Thing() { return import.meta.url; }
"#,
        );
        assert!(
            out.code.contains(r#"location.origin + "/src/Mod.tsx""#),
            "{}",
            out.code
        );
        assert!(out.is_boundary(), "component module registers refresh");
    }

    #[test]
    fn plain_esm_module_without_a_component_is_not_a_boundary() {
        let out = factory("export const x = 1; export function add(a, b) { return a + b; }");
        assert_eq!(out.kind, FactoryKind::Esm);
        assert!(
            !out.is_boundary(),
            "no $RefreshReg$ means no boundary: {}",
            out.code
        );
    }
}
