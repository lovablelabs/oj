// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite's build import analysis, as Vite 8 runs it on rolldown: every static
//! `import()` is wrapped in `__vitePreload(() => import(...), __VITE_PRELOAD__)`
//! and imports the helper from `\0vite/preload-helper.js` (rolldown's native
//! `builtin:vite-build-import-analysis`, ported from rolldown 1.2.15), the
//! `__VITE_IS_MODERN__` flag is set in renderChunk, and once the chunks are
//! written each marker becomes the chunk's preload list (`vite:build-import-
//! analysis`'s generateBundle). The helper and the modulepreload polyfill are
//! the exact texts Vite builds them from (`vite_preload/`), so the minified
//! chunks match Vite's byte for byte.

use std::borrow::Cow;
use std::path::Path;

use arcstr::ArcStr;
use oxc_allocator::{Allocator, CloneIn as _, GetAllocator, TakeIn as _};
use oxc_ast::ast::{
    Argument, ArrowFunctionBody, BindingIdentifier, BindingPattern, BindingProperty, Expression,
    FormalParameterKind, FormalParameters, FunctionBody, IdentifierName, ImportDeclaration,
    ImportDeclarationSpecifier, ImportOrExportKind, ModuleExportName, ObjectPattern, Program,
    PropertyKey, Statement, StaticMemberExpression, StringLiteral, VariableDeclaration,
    VariableDeclarationKind, VariableDeclarator,
};
use oxc_ast::builder::{AstBuilder, GetAstBuilder};
use oxc_ast_visit::{walk_js_mut, VisitJsMut};
use oxc_span::SPAN;
use oxc_syntax::scope::{ScopeFlags, ScopeId};
use rolldown_common::side_effects::HookSideEffects;
use rolldown_ecmascript_utils::{
    BindingIdentifierFactoryExt as _, BindingPatternExt as _, ExpressionFactoryExt as _,
};
use rolldown_plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, HookRenderChunkArgs, HookRenderChunkOutput,
    HookRenderChunkReturn, HookResolveIdArgs, HookResolveIdOutput, HookResolveIdReturn,
    HookTransformAstArgs, HookTransformAstReturn, HookTransformOutputMap, HookUsage, Plugin,
    PluginContext, SharedLoadPluginContext,
};

/// rolldown's `module-preload-polyfill.js`, which Vite 8 builds pages with.
pub(super) const MODULEPRELOAD_POLYFILL_JS: &str =
    include_str!("vite_preload/module-preload-polyfill.js");

const DETECT_SCRIPT_REL: &str = include_str!("vite_preload/detect-script-rel.js");
const PRELOAD: &str = include_str!("vite_preload/preload.js");
const IS_CSS_PRELOAD_URL: &str = include_str!("vite_preload/is-css-preload-url.js");
const PRELOAD_ONCE: &str = include_str!("vite_preload/preload-once.js");

const PRELOAD_HELPER_ID: &str = "\0vite/preload-helper.js";
const PRELOAD_METHOD: &str = "__vitePreload";
const PRELOAD_MARKER: &str = "__VITE_PRELOAD__";
const IS_MODERN_FLAG: &str = "__VITE_IS_MODERN__";

/// Vite's getPreloadCode: the `\0vite/preload-helper.js` module. With the
/// polyfill on, `scriptRel` is `'modulepreload'`; deps resolve against the
/// importer under a relative base, else against `base`.
pub(super) fn preload_code(base: &str, polyfill: bool, relative_base: bool) -> String {
    let script_rel = if polyfill {
        "'modulepreload'".to_string()
    } else {
        format!("/* @__PURE__ */ ({DETECT_SCRIPT_REL})()")
    };
    let assets_url = if relative_base {
        "function(dep, importerUrl) { return new URL(dep, importerUrl).href }".to_string()
    } else {
        format!(
            "function(dep) {{ return {}+dep }}",
            serde_json::Value::String(base.to_string())
        )
    };
    format!(
        "const scriptRel = {script_rel};const assetsURL = {assets_url};const seen = {{}};const isCssPreloadUrl = {IS_CSS_PRELOAD_URL};const preloadOnce = {PRELOAD_ONCE};export const {PRELOAD_METHOD} = {PRELOAD}"
    )
}

/// The client build's import analysis: the helper module, the `import()`
/// rewrite and the modern flag.
#[derive(Debug)]
pub(super) struct BuildImportAnalysisPlugin {
    pub preload_code: ArcStr,
    pub is_relative_base: bool,
}

impl Plugin for BuildImportAnalysisPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:build-import-analysis")
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::ResolveId | HookUsage::Load | HookUsage::TransformAst | HookUsage::RenderChunk
    }

    async fn resolve_id(
        &self,
        _ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> HookResolveIdReturn {
        Ok(
            (args.specifier == PRELOAD_HELPER_ID).then(|| HookResolveIdOutput {
                id: args.specifier.into(),
                ..Default::default()
            }),
        )
    }

    async fn load(&self, _ctx: SharedLoadPluginContext, args: &HookLoadArgs<'_>) -> HookLoadReturn {
        Ok((args.id == PRELOAD_HELPER_ID).then(|| HookLoadOutput {
            code: self.preload_code.clone(),
            side_effects: Some(HookSideEffects::False),
            ..Default::default()
        }))
    }

    async fn transform_ast(
        &self,
        ctx: &PluginContext,
        args: HookTransformAstArgs<'_>,
    ) -> HookTransformAstReturn {
        let mut ast = args.ast;
        let is_modern = ctx.options().format.is_esm();
        ast.program.with_mut(|fields| {
            let mut visitor = ImportAnalysisVisitor {
                ast_builder: AstBuilder::new(fields.allocator),
                scope_stack: Vec::new(),
                has_inserted_helper: false,
                need_prepend_helper: false,
                is_relative_base: self.is_relative_base,
                is_modern,
            };
            visitor.visit_program(fields.program);
        });
        Ok(ast)
    }

    /// The modern flag, padded to its own length so the chunk's mappings hold
    /// (`map: null`), before the minifier folds it away.
    async fn render_chunk(
        &self,
        _ctx: &PluginContext,
        args: &HookRenderChunkArgs<'_>,
    ) -> HookRenderChunkReturn {
        if !args.code.contains(IS_MODERN_FLAG) {
            return Ok(None);
        }
        let modern = if args.options.format.is_esm() {
            "true"
        } else {
            "false"
        };
        let padded = format!(
            "{modern}{}",
            " ".repeat(IS_MODERN_FLAG.len() - modern.len())
        );
        Ok(Some(HookRenderChunkOutput {
            code: args.code.replace(IS_MODERN_FLAG, &padded),
            map: HookTransformOutputMap::Null,
        }))
    }
}

struct ImportAnalysisVisitor<'a> {
    ast_builder: AstBuilder<'a>,
    scope_stack: Vec<ScopeFlags>,
    has_inserted_helper: bool,
    need_prepend_helper: bool,
    is_relative_base: bool,
    is_modern: bool,
}

impl<'a> GetAstBuilder<'a> for ImportAnalysisVisitor<'a> {
    type Builder = AstBuilder<'a>;

    fn builder(&self) -> &AstBuilder<'a> {
        &self.ast_builder
    }
}

impl<'a> GetAllocator<'a> for ImportAnalysisVisitor<'a> {
    fn allocator(&self) -> &'a Allocator {
        self.ast_builder.allocator()
    }
}

impl<'a> VisitJsMut<'a> for ImportAnalysisVisitor<'a> {
    fn visit_program(&mut self, it: &mut Program<'a>) {
        walk_js_mut::walk_program(self, it);
        if self.need_prepend_helper && !self.has_inserted_helper {
            it.body.push(Statement::new_import_declaration(
                SPAN,
                Some(oxc_allocator::Vec::from_value_in(
                    ImportDeclarationSpecifier::new_import_specifier(
                        SPAN,
                        ModuleExportName::new_identifier_name(SPAN, PRELOAD_METHOD, self),
                        BindingIdentifier::new_id(SPAN, PRELOAD_METHOD, self),
                        ImportOrExportKind::Value,
                        self,
                    ),
                    self,
                )),
                StringLiteral::new(SPAN, PRELOAD_HELPER_ID, None, self),
                None,
                None,
                ImportOrExportKind::Value,
                self,
            ));
        }
    }

    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        let rewritten = match expr {
            Expression::CallExpression(_) => self.rewrite_call_expr(expr),
            Expression::StaticMemberExpression(member) => self.rewrite_member_expr(member),
            _ => self.rewrite_import_expr(expr),
        };
        if rewritten {
            self.need_prepend_helper = true;
            return;
        }
        walk_js_mut::walk_expression(self, expr);
    }

    fn visit_import_declaration(&mut self, it: &mut ImportDeclaration<'a>) {
        it.with_clause.take();
    }

    /// `const {foo} = await import('foo')` becomes
    /// `const {foo} = await __vitePreload(async () => { const {foo} = await import('foo'); return {foo} }, ...)`.
    fn visit_variable_declaration(&mut self, decl: &mut VariableDeclaration<'a>) {
        for decl in &mut decl.declarations {
            let awaits_import = matches!(
                &decl.init,
                Some(Expression::AwaitExpression(expr)) if is_static_import(&expr.argument)
            );
            match &decl.id {
                BindingPattern::ObjectPattern(object_pat) if awaits_import => {
                    let object_pat = object_pat.clone_in(self.ast_builder.allocator());
                    decl.init = Some(Expression::new_await_expression(
                        SPAN,
                        self.preload_destructured(object_pat, decl.init.take().unwrap()),
                        self,
                    ));
                    self.need_prepend_helper = true;
                }
                _ => walk_js_mut::walk_variable_declarator(self, decl),
            }
        }
    }

    fn visit_variable_declarator(&mut self, it: &mut VariableDeclarator<'a>) {
        if self.is_top_level() {
            if let BindingPattern::BindingIdentifier(id) = &it.id {
                self.has_inserted_helper = id.name == PRELOAD_METHOD;
            }
        }
        walk_js_mut::walk_variable_declarator(self, it);
    }

    fn enter_scope(&mut self, flags: ScopeFlags, _scope_id: &std::cell::Cell<Option<ScopeId>>) {
        self.scope_stack.push(flags);
    }

    fn leave_scope(&mut self) {
        self.scope_stack.pop();
    }
}

/// Only static urls are preloaded: `import('foo')` or `` import(`foo`) ``.
fn is_static_import(expr: &Expression<'_>) -> bool {
    matches!(
        expr,
        Expression::ImportExpression(import)
            if matches!(import.source, Expression::StringLiteral(_) | Expression::TemplateLiteral(_))
    )
}

impl<'a> ImportAnalysisVisitor<'a> {
    fn is_top_level(&self) -> bool {
        self.scope_stack
            .last()
            .is_some_and(|flags| flags.contains(ScopeFlags::Top))
    }

    /// `(await import('foo')).foo`
    fn rewrite_member_expr(&self, member_expr: &mut StaticMemberExpression<'a>) -> bool {
        let mut await_expr = &mut member_expr.object;
        while let Expression::ParenthesizedExpression(paren) = await_expr {
            await_expr = &mut paren.expression;
        }
        if !matches!(await_expr, Expression::AwaitExpression(expr) if is_static_import(&expr.argument))
        {
            return false;
        }
        let (key, value) = match member_expr.property.name.as_str() {
            key @ "default" => (key, "__vite_default__"),
            name => (name, name),
        };
        *await_expr = Expression::new_await_expression(
            SPAN,
            self.preload_destructured(
                ObjectPattern::boxed(
                    SPAN,
                    [BindingProperty::new(
                        SPAN,
                        PropertyKey::new_static_identifier(SPAN, key, self),
                        BindingPattern::new_binding_identifier(SPAN, value, self),
                        true,
                        false,
                        self,
                    )],
                    None,
                    self,
                ),
                await_expr.take_in(self),
            ),
            self,
        );
        true
    }

    /// `import('foo').then(...)`: a destructuring callback preloads the
    /// import alone; any other wraps the whole `.then` chain.
    fn rewrite_call_expr(&mut self, expr: &mut Expression<'a>) -> bool {
        let Expression::CallExpression(call_expr) = expr else {
            return false;
        };
        let Expression::StaticMemberExpression(ref callee) = call_expr.callee else {
            return false;
        };
        if callee.property.name != "then" || !is_static_import(&callee.object) {
            return false;
        }
        let destructuring = call_expr.arguments.first().and_then(|arg| {
            let params = match arg {
                Argument::ArrowFunctionExpression(func) => &func.params,
                Argument::FunctionExpression(func) => &func.params,
                _ => return None,
            };
            match &params.items.first()?.pattern {
                BindingPattern::ObjectPattern(object_pat) => {
                    Some(object_pat.clone_in(self.ast_builder.allocator()))
                }
                _ => None,
            }
        });
        if let Some(object_pat) = destructuring {
            let Expression::StaticMemberExpression(callee) = &mut call_expr.callee else {
                unreachable!();
            };
            callee.object = self.preload_destructured(
                object_pat,
                Expression::new_await_expression(SPAN, callee.object.take_in(self), self),
            );
            walk_js_mut::walk_arguments(self, &mut call_expr.arguments);
            return true;
        }
        walk_js_mut::walk_arguments(self, &mut call_expr.arguments);
        let import_then = expr.take_in(self);
        *expr = self.preload_call(Argument::from(Expression::new_arrow_returning(
            import_then,
            self,
        )));
        true
    }

    /// `import('foo')`
    fn rewrite_import_expr(&self, expr: &mut Expression<'a>) -> bool {
        if !is_static_import(expr) {
            return false;
        }
        *expr = self.preload_call(Argument::from(Expression::new_arrow_returning(
            expr.take_in(self),
            self,
        )));
        true
    }

    fn preload_destructured(
        &self,
        object_pat: oxc_allocator::Box<'a, ObjectPattern<'a>>,
        await_expr: Expression<'a>,
    ) -> Expression<'a> {
        self.preload_call(Argument::new_arrow_function_expression(
            SPAN,
            true,
            None,
            FormalParameters::boxed(SPAN, FormalParameterKind::Signature, [], None, self),
            None,
            ArrowFunctionBody::FunctionBody(FunctionBody::boxed(
                SPAN,
                [],
                {
                    let mut statements = oxc_allocator::Vec::with_capacity_in(2, self);
                    statements.push(Statement::new_variable_declaration(
                        SPAN,
                        VariableDeclarationKind::Const,
                        [VariableDeclarator::new(
                            SPAN,
                            BindingPattern::ObjectPattern(
                                object_pat.clone_in(self.ast_builder.allocator()),
                            ),
                            None,
                            Some(await_expr),
                            false,
                            self,
                        )],
                        false,
                        self,
                    ));
                    statements.push(Statement::new_return_statement(
                        SPAN,
                        Some(BindingPattern::ObjectPattern(object_pat).into_expression(self)),
                        self,
                    ));
                    statements
                },
                self,
            )),
            self,
        ))
    }

    /// `__vitePreload(factory, __VITE_PRELOAD__[, import.meta.url])`: the
    /// importer's url rides along under a relative base.
    fn preload_call(&self, argument: Argument<'a>) -> Expression<'a> {
        let mut items = oxc_allocator::Vec::with_capacity_in(3, self);
        items.push(argument);
        items.push(Argument::from(if self.is_modern {
            Expression::new_identifier(SPAN, PRELOAD_MARKER, self)
        } else {
            Expression::new_void_0(SPAN, self)
        }));
        if self.is_relative_base {
            items.push(Argument::new_static_member_expression(
                SPAN,
                Expression::new_import_meta(SPAN, self),
                IdentifierName::new(SPAN, "url", self),
                false,
                self,
            ));
        }
        Expression::new_call_expression(
            SPAN,
            Expression::new_identifier(SPAN, PRELOAD_METHOD, self),
            None,
            items,
            false,
            self,
        )
    }
}

/// What a chunk contributes to a preload list (Vite's collectPreloadDependencies).
pub(super) struct Dependency<'d> {
    pub js: Option<&'d str>,
    pub imports: &'d [String],
    pub css: Option<&'d str>,
}

/// Vite's collectPreloadDependencies: the chunk, then what it imports
/// (depth-first), then its stylesheet, each once, never the importer.
fn collect_preload_dependencies<'d>(
    file: &str,
    owner: &str,
    dependency: &dyn Fn(&str) -> Option<Dependency<'d>>,
) -> Vec<String> {
    fn visit<'d>(
        file: &str,
        dependency: &dyn Fn(&str) -> Option<Dependency<'d>>,
        visited: &mut std::collections::HashSet<String>,
        out: &mut Vec<String>,
    ) {
        let Some(dep) = dependency(file) else { return };
        if !visited.insert(file.to_string()) {
            return;
        }
        if let Some(js) = dep.js {
            out.push(js.to_string());
        }
        for import in dep.imports {
            visit(import, dependency, visited, out);
        }
        if let Some(css) = dep.css {
            out.push(css.to_string());
        }
    }
    let mut visited = std::collections::HashSet::from([owner.to_string()]);
    let mut out = Vec::new();
    visit(file, dependency, &mut visited, &mut out);
    out
}

/// A dynamic `import()` in a finished chunk: where its specifier ends and its
/// static url (es-module-lexer's `e` and `n`).
struct DynamicImport {
    specifier_end: usize,
    url: Option<String>,
}

fn dynamic_imports(code: &str) -> Option<Vec<DynamicImport>> {
    use oxc_ast_visit::Visit;
    struct Collect(Vec<DynamicImport>);
    impl<'a> Visit<'a> for Collect {
        fn visit_import_expression(&mut self, it: &oxc_ast::ast::ImportExpression<'a>) {
            let url = match &it.source {
                Expression::StringLiteral(s) => Some(s.value.to_string()),
                Expression::TemplateLiteral(t) if t.expressions.is_empty() => t
                    .quasis
                    .first()
                    .and_then(|q| q.value.cooked.map(|c| c.to_string())),
                _ => None,
            };
            let span = oxc_span::GetSpan::span(&it.source);
            self.0.push(DynamicImport {
                specifier_end: span.end as usize,
                url,
            });
            oxc_ast_visit::walk::walk_import_expression(self, it);
        }
    }
    let allocator = Allocator::default();
    let parsed = oxc_parser::Parser::new(&allocator, code, oxc_span::SourceType::mjs()).parse();
    if parsed.fatal_error {
        return None;
    }
    let mut collect = Collect(Vec::new());
    collect.visit_program(&parsed.program);
    collect.0.sort_by_key(|i| i.specifier_end);
    Some(collect.0)
}

fn find_marker(code: &str, from: usize) -> Option<usize> {
    code.get(from..)?.find(PRELOAD_MARKER).map(|i| from + i)
}

/// Vite's matchImportsToPreloadMarkers: each import's marker is the first
/// one after its specifier that no later-opened import claimed.
fn match_markers(code: &str, imports: &[DynamicImport]) -> Vec<Option<usize>> {
    let mut out = vec![None; imports.len()];
    if imports.is_empty() {
        return out;
    }
    let mut open: Vec<usize> = Vec::new();
    let mut next = 0;
    let mut pos = find_marker(code, imports[0].specifier_end);
    while let Some(marker) = pos {
        while next < imports.len() && imports[next].specifier_end <= marker {
            open.push(next);
            next += 1;
        }
        if let Some(i) = open.pop() {
            out[i] = Some(marker);
        }
        pos = find_marker(code, marker + PRELOAD_MARKER.len());
    }
    if imports.len() == 1 && out[0].is_none() {
        out[0] = find_marker(code, 0);
    }
    out
}

/// `path.posix.join`'s normalization: `.` dropped, `..` popping a segment.
fn normalize_posix(path: &Path) -> String {
    let mut out: Vec<&str> = Vec::new();
    let path = path.to_string_lossy();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if out.last().is_some_and(|p| *p != "..") {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            p => out.push(p),
        }
    }
    out.join("/")
}

/// A `./`-led path from the importer's directory (Vite's toRelativePath).
fn relative_to(file: &str, importer: &str) -> String {
    let rel = super::relative_chunk_path(importer, file);
    if rel.starts_with('.') {
        rel
    } else {
        format!("./{rel}")
    }
}

/// An edit to the finished chunk: `[start, end)` replaced by `with`.
struct Edit {
    start: usize,
    end: usize,
    with: String,
}

/// A chunk with its markers filled: the code, the `__vite__mapDeps` table line
/// it needs (if any), and each edit as `(start, end, replacement length)` in
/// the original code, for its sourcemap.
pub(super) struct FilledChunk {
    pub code: String,
    pub table: Option<String>,
    pub spans: Vec<(usize, usize, usize)>,
}

/// The written chunk `code` with its markers filled as Vite's generateBundle
/// fills them; None when the chunk has no marker.
pub(super) fn fill_preload_markers<'d>(
    code: &str,
    file: &str,
    relative_base: bool,
    module_preload: bool,
    dependency: &dyn Fn(&str) -> Option<Dependency<'d>>,
) -> Option<FilledChunk> {
    if !code.contains(PRELOAD_MARKER) {
        return None;
    }
    let imports = dynamic_imports(code).unwrap_or_default();
    let markers = match_markers(code, &imports);
    let mut file_deps: Vec<String> = Vec::new();
    let mut edits: Vec<Edit> = Vec::new();
    let mut rewrote: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let dir = Path::new(file).parent().unwrap_or(Path::new(""));
    for (import, marker) in imports.iter().zip(&markers) {
        let deps: Vec<String> = match &import.url {
            Some(url) => {
                let normalized = normalize_posix(&dir.join(url));
                let mut seen = std::collections::HashSet::new();
                collect_preload_dependencies(&normalized, file, dependency)
                    .into_iter()
                    .filter(|d| seen.insert(d.clone()))
                    .collect()
            }
            None => Vec::new(),
        };
        let Some(marker) = *marker else { continue };
        let deps: Vec<String> = if deps.len() > 1 {
            if module_preload {
                deps
            } else {
                deps.into_iter().filter(|d| d.ends_with(".css")).collect()
            }
        } else {
            Vec::new()
        };
        let rendered: Vec<String> = deps
            .into_iter()
            .map(|d| {
                let url = if relative_base {
                    relative_to(&d, file)
                } else {
                    d
                };
                match file_deps.iter().position(|x| *x == url) {
                    Some(i) => i,
                    None => {
                        file_deps.push(url);
                        file_deps.len() - 1
                    }
                }
                .to_string()
            })
            .collect();
        edits.push(Edit {
            start: marker,
            end: marker + PRELOAD_MARKER.len(),
            with: if rendered.is_empty() {
                "[]".to_string()
            } else {
                format!("__vite__mapDeps([{}])", rendered.join(","))
            },
        });
        rewrote.insert(marker);
    }
    let mut pos = find_marker(code, 0);
    while let Some(marker) = pos {
        if !rewrote.contains(&marker) {
            edits.push(Edit {
                start: marker,
                end: marker + PRELOAD_MARKER.len(),
                with: "void 0".to_string(),
            });
        }
        pos = find_marker(code, marker + PRELOAD_MARKER.len());
    }
    edits.sort_by_key(|e| e.start);
    let mut out = String::with_capacity(code.len() + 64);
    let mut last = 0;
    let mut spans = Vec::with_capacity(edits.len());
    for e in &edits {
        out.push_str(&code[last..e.start]);
        spans.push((e.start, e.end, e.with.len()));
        out.push_str(&e.with);
        last = e.end;
    }
    out.push_str(&code[last..]);
    let map_deps = (!file_deps.is_empty()).then(|| {
        let list: Vec<String> = file_deps
            .iter()
            .map(|d| serde_json::Value::String(d.clone()).to_string())
            .collect();
        format!(
            "const __vite__mapDeps=(i,m=__vite__mapDeps,d=(m.f||(m.f=[{}])))=>i.map(i=>d[i]);\n",
            list.join(",")
        )
    });
    Some(FilledChunk {
        code: out,
        table: map_deps,
        spans,
    })
}

/// The chunk's sourcemap after `fill_preload_markers`: each replaced span's
/// width change shifts what follows it on its line, and a table line inserted
/// at `inserted` moves that line and the ones after it down one.
pub(super) fn shift_sourcemap(
    map_json: &str,
    code: &str,
    spans: &[(usize, usize, usize)],
    inserted: Option<u32>,
) -> Option<String> {
    let map = oxc_sourcemap::SourceMap::from_json_string(map_json).ok()?;
    // (line, utf16 column of the span's end, column delta) per edit.
    let mut shifts: Vec<(u32, u32, i64)> = Vec::with_capacity(spans.len());
    for &(start, end, new_len) in spans {
        let line_start = code[..start].rfind('\n').map_or(0, |i| i + 1);
        let line = code[..start].matches('\n').count() as u32;
        let end_col = code[line_start..end].encode_utf16().count() as u32;
        let old = code[start..end].encode_utf16().count() as i64;
        shifts.push((line, end_col, new_len as i64 - old));
    }
    let mut parts = map.into_parts();
    let tokens: Vec<oxc_sourcemap::Token> = parts
        .tokens
        .iter()
        .map(|t| {
            let line = t.get_dst_line();
            let mut col = t.get_dst_col() as i64;
            for &(l, end_col, delta) in &shifts {
                if l == line && t.get_dst_col() >= end_col {
                    col += delta;
                }
            }
            oxc_sourcemap::Token::new(
                line + u32::from(inserted.is_some_and(|at| line >= at)),
                col.max(0) as u32,
                t.get_src_line(),
                t.get_src_col(),
                t.get_source_id(),
                t.get_name_id(),
            )
        })
        .collect();
    parts.tokens = tokens.into_boxed_slice();
    parts.token_chunks = None;
    Some(oxc_sourcemap::SourceMap::from_parts(parts).to_json_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deps<'d>(
        graph: &'d [(&'d str, &'d [String], Option<&'d str>)],
    ) -> impl Fn(&str) -> Option<Dependency<'d>> + 'd {
        move |file| {
            graph
                .iter()
                .find(|(f, _, _)| *f == file)
                .map(|(f, imports, css)| Dependency {
                    js: Some(f),
                    imports,
                    css: *css,
                })
        }
    }

    #[test]
    fn markers_fill_with_vites_preload_lists() {
        let none: Vec<String> = Vec::new();
        let util = vec!["assets/util-x.js".to_string()];
        let graph = [
            (
                "assets/lazy-a.js",
                util.as_slice(),
                Some("assets/lazy-a.css"),
            ),
            ("assets/util-x.js", none.as_slice(), None),
            ("assets/solo-b.js", none.as_slice(), None),
        ];
        let dependency = deps(&graph);
        let code = "e(()=>import(`./lazy-a.js`),__VITE_PRELOAD__);e(()=>import(`./solo-b.js`),__VITE_PRELOAD__);e(__VITE_PRELOAD__)";
        let FilledChunk {
            code: out, table, ..
        } = fill_preload_markers(code, "assets/index-i.js", false, true, &dependency).unwrap();
        assert_eq!(
            out,
            "e(()=>import(`./lazy-a.js`),__vite__mapDeps([0,1,2]));e(()=>import(`./solo-b.js`),[]);e(void 0)"
        );
        assert_eq!(
            table.unwrap(),
            "const __vite__mapDeps=(i,m=__vite__mapDeps,d=(m.f||(m.f=[\"assets/lazy-a.js\",\"assets/util-x.js\",\"assets/lazy-a.css\"])))=>i.map(i=>d[i]);\n"
        );
        // A relative base names deps from the importer; modulePreload false
        // keeps only the stylesheets.
        let FilledChunk {
            code: out, table, ..
        } = fill_preload_markers(code, "assets/index-i.js", true, false, &dependency).unwrap();
        assert!(
            out.starts_with("e(()=>import(`./lazy-a.js`),__vite__mapDeps([0]))"),
            "{out}"
        );
        assert!(table.unwrap().contains("[\"./lazy-a.css\"]"));
    }

    #[test]
    fn the_helper_is_vites_getpreloadcode() {
        let code = preload_code("/", true, false);
        assert!(code.starts_with(
            "const scriptRel = 'modulepreload';const assetsURL = function(dep) { return \"/\"+dep };const seen = {};const isCssPreloadUrl = function isCssPreloadUrl(url) {"
        ));
        assert!(code.contains(
            ";export const __vitePreload = function preload(baseModule, deps, importerUrl) {"
        ));
        assert!(preload_code("./", false, true)
            .starts_with("const scriptRel = /* @__PURE__ */ (function detectScriptRel() {"));
    }
}
