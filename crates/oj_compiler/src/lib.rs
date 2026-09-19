// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

pub mod assets;
pub mod bundle;
pub mod cjs;
pub mod glob;
pub mod html;
pub mod interop;
pub mod ssr;
pub mod json;
pub mod pkgbundle;

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, RwLock};

use memchr::memmem::Finder;
use oxc_allocator::Allocator;
use oxc_ast::ast::{Program, Statement, StringLiteral};
use oxc_codegen::{Codegen, CodegenOptions, CodegenReturn};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{JsxRuntime, ReactRefreshOptions, TransformOptions, Transformer};
use oxc_transformer_plugins::{ReplaceGlobalDefines, ReplaceGlobalDefinesConfig};

pub type ImportRewriter<'r> = dyn FnMut(&str) -> Option<String> + 'r;

pub const COMPILE_STACK_SIZE: usize = 16 * 1024 * 1024;

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

pub(crate) fn detect_refresh_registrations(program: &Program) -> bool {
    use oxc_ast::ast::{CallExpression, Expression};
    use oxc_ast_visit::{walk, Visit};

    struct Detector {
        found: bool,
    }
    impl<'a> Visit<'a> for Detector {
        fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
            if self.found {
                return;
            }
            if let Expression::Identifier(id) = &call.callee {
                if id.name == "$RefreshReg$" {
                    self.found = true;
                    return;
                }
            }
            walk::walk_call_expression(self, call);
        }
    }
    let mut detector = Detector { found: false };
    detector.visit_program(program);
    detector.found
}

/// Everything a compile derives from one resolved define list, built once per
/// `set_*` call instead of once per compile: the non-`import.meta` keys that
/// gate the replacer on plain source text, and the oxc config, which validates
/// and parses every value in its own arena and is `Clone` over an `Arc`.
pub(crate) struct EnvDefines {
    /// Keys other than `import.meta*`; `import.meta.env` itself is gated by the
    /// SIMD `F_IMPORT_META_ENV` scan, so these are the only scalar scans left.
    plain_keys: Vec<String>,
    /// `None` when oxc rejected the list, which skips the replacer for that
    /// compile (unchanged from the uncached behaviour).
    config: Option<ReplaceGlobalDefinesConfig>,
}

impl EnvDefines {
    fn build(pairs: Vec<(String, String)>) -> Arc<Self> {
        let plain_keys = pairs
            .iter()
            .filter(|(k, _)| !k.starts_with("import.meta"))
            .map(|(k, _)| k.clone())
            .collect();
        let config = ReplaceGlobalDefinesConfig::new(&pairs).ok();
        Arc::new(Self { plain_keys, config })
    }

    /// True when `source_text` can mention any define: `import.meta.env` via
    /// the SIMD finder, other keys via a scalar scan each.
    pub(crate) fn needed_by(&self, source_text: &str) -> bool {
        scan(&F_IMPORT_META_ENV, source_text)
            || self
                .plain_keys
                .iter()
                .any(|k| source_text.contains(k.as_str()))
    }

    /// The prebuilt replacer config (an `Arc` bump to clone), or `None` when
    /// oxc rejected the define list.
    pub(crate) fn config(&self) -> Option<ReplaceGlobalDefinesConfig> {
        self.config.clone()
    }
}

/// Server-provided defines: the raw inputs and both compile variants derived
/// from them. Variants are rebuilt eagerly in the setters (a few times per
/// process) so `import_meta_env_defines` is a read lock and an `Arc` clone.
struct EnvState {
    /// `set_import_meta_env`; `None` until the server sets it (the `oj build`
    /// path and unit tests never do, and use the mode-derived fallback).
    client_pairs: Option<Vec<(String, String)>>,
    /// `environments.ssr.define`: layered over the shared list for SSR
    /// compiles only, so a key defined differently per side keeps both values.
    ssr_overrides: Vec<(String, String)>,
    client: Option<Arc<EnvDefines>>,
    ssr: Option<Arc<EnvDefines>>,
}

impl EnvState {
    /// Rederives both variants from the raw inputs. Runs
    /// `ReplaceGlobalDefinesConfig::new` (an oxc parse of every value) twice on
    /// the caller's thread, roughly 100 us with 40 vars, so it belongs in the
    /// setters, not on the compile path.
    fn rebuild(&mut self) {
        let Some(pairs) = &self.client_pairs else {
            self.client = None;
            self.ssr = None;
            return;
        };
        let mut ssr = pairs.clone();
        for (k, v) in ssr.iter_mut() {
            if k == "import.meta.env.SSR" {
                *v = "true".into();
            } else if k == "import.meta.env" {
                *v = v.replace("\"SSR\":false", "\"SSR\":true");
            }
        }
        for (k, v) in &self.ssr_overrides {
            if let Some(slot) = ssr.iter_mut().find(|(ek, _)| ek == k) {
                slot.1 = v.clone();
            } else {
                ssr.push((k.clone(), v.clone()));
            }
        }
        self.client = Some(EnvDefines::build(pairs.clone()));
        self.ssr = Some(EnvDefines::build(ssr));
    }
}

// RwLock, not OnceLock: the server re-sets these once the plugin host reports
// config()-hook env mutations, which land after the initial dotenv-based set.
static ENV_DEFINES: RwLock<EnvState> = RwLock::new(EnvState {
    client_pairs: None,
    ssr_overrides: Vec::new(),
    client: None,
    ssr: None,
});

// The three setters are boot-time calls: each rebuilds both compile variants
// under the write lock, so the next compile sees the new list. Do not call
// them per request.
pub fn set_import_meta_env(defines: Vec<(String, String)>) {
    let mut state = ENV_DEFINES.write().expect("ENV_DEFINES poisoned");
    state.client_pairs = Some(defines);
    state.rebuild();
}

pub fn set_import_meta_env_ssr(overrides: Vec<(String, String)>) {
    let mut state = ENV_DEFINES.write().expect("ENV_DEFINES poisoned");
    state.ssr_overrides = overrides;
    state.rebuild();
}

/// Folds more ssr-environment defines over the current set (later wins): the
/// resolved-config defines arrive when the lazy ssr plugin host spawns, after
/// the boot-time set from the oj config.
pub fn merge_import_meta_env_ssr(overrides: Vec<(String, String)>) {
    let mut state = ENV_DEFINES.write().expect("ENV_DEFINES poisoned");
    for (k, v) in overrides {
        if let Some(slot) = state.ssr_overrides.iter_mut().find(|(ek, _)| *ek == k) {
            slot.1 = v;
        } else {
            state.ssr_overrides.push((k, v));
        }
    }
    state.rebuild();
}

/// Mode-derived defines used until the server calls `set_import_meta_env`
/// (and always by `oj build` and the unit tests), one per (dev, ssr) variant.
/// Indexed by `fallback_idx(dev, ssr)`.
static FALLBACK_DEFINES: [LazyLock<Arc<EnvDefines>>; 4] = [
    LazyLock::new(|| fallback_defines(false, false)),
    LazyLock::new(|| fallback_defines(false, true)),
    LazyLock::new(|| fallback_defines(true, false)),
    LazyLock::new(|| fallback_defines(true, true)),
];

/// `dev << 1 | ssr`, the order of `FALLBACK_DEFINES`.
fn fallback_idx(dev: bool, ssr: bool) -> usize {
    ((dev as usize) << 1) | ssr as usize
}

fn fallback_defines(dev: bool, ssr: bool) -> Arc<EnvDefines> {
    let mode = if dev { "development" } else { "production" };
    EnvDefines::build(vec![
        ("import.meta.env.BASE_URL".into(), "\"/\"".into()),
        ("import.meta.env.MODE".into(), format!("\"{mode}\"")),
        ("import.meta.env.DEV".into(), dev.to_string()),
        ("import.meta.env.PROD".into(), (!dev).to_string()),
        ("import.meta.env.SSR".into(), ssr.to_string()),
        (
            "import.meta.env".into(),
            format!(
                "({{\"BASE_URL\":\"/\",\"MODE\":\"{mode}\",\"DEV\":{dev},\"PROD\":{prod},\"SSR\":{ssr}}})",
                prod = !dev
            ),
        ),
    ])
}

pub(crate) fn import_meta_env_defines(dev: bool, ssr: bool) -> Arc<EnvDefines> {
    {
        let state = ENV_DEFINES.read().expect("ENV_DEFINES poisoned");
        let variant = if ssr { &state.ssr } else { &state.client };
        if let Some(defines) = variant {
            return Arc::clone(defines);
        }
    }
    Arc::clone(&FALLBACK_DEFINES[fallback_idx(dev, ssr)])
}

/// JSX compile settings: Vite's `oxc.jsx` (what `@vitejs/plugin-react` sets from
/// its `jsxRuntime`/`jsxImportSource` options) or the older `esbuild.jsx*` form.
/// A file's own `@jsx`, `@jsxRuntime`, `@jsxImportSource` and `@jsxFrag` pragma
/// comments still win, since oxc applies them on top of these.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JsxConfig {
    /// `"classic"` emits `pragma(...)` calls; anything else is the automatic runtime.
    pub runtime: Option<String>,
    /// Package the automatic runtime is imported from (default `react`).
    pub import_source: Option<String>,
    /// Classic-runtime element factory (default `React.createElement`).
    pub pragma: Option<String>,
    /// Classic-runtime fragment (default `React.Fragment`).
    pub pragma_frag: Option<String>,
}

impl JsxConfig {
    pub fn is_classic(&self) -> bool {
        self.runtime.as_deref() == Some("classic")
    }
}

#[derive(Debug, Clone)]
pub struct CompileOptions {
    pub dev: bool,
    pub refresh: bool,
    pub sourcemap: bool,
    pub ssr: bool,
    pub jsx: JsxConfig,
}

impl CompileOptions {
    pub fn dev() -> Self {
        Self {
            dev: true,
            refresh: true,
            sourcemap: true,
            ssr: false,
            jsx: JsxConfig::default(),
        }
    }

    pub fn prod() -> Self {
        Self {
            dev: false,
            refresh: false,
            sourcemap: true,
            ssr: false,
            jsx: JsxConfig::default(),
        }
    }
}

#[derive(Debug)]
pub struct CompileOutput {
    pub code: String,
    pub map_data_url: Option<String>,
    pub imports: Vec<String>,
    pub dynamic_imports: Vec<String>,
    pub is_refresh_boundary: bool,
    /// `Some` when the module references `import.meta.hot` (it needs a hot
    /// context injected); what its `accept` calls declared.
    pub hot_accept: Option<HotAccept>,
}

/// The `import.meta.hot.accept(...)` forms a module uses (Vite's
/// `lexAcceptedHmrDeps`): `accept()` / `accept(cb)` make it self-accepting,
/// `accept('./dep', cb)` / `accept(['./a', './b'], cb)` make it the boundary for
/// updates of those dependencies (specifiers already rewritten to served urls).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HotAccept {
    pub self_accepting: bool,
    pub deps: Vec<String>,
}

impl CompileOutput {
    pub fn code_with_inline_map(&self) -> String {
        match &self.map_data_url {
            Some(url) => format!("{}\n//# sourceMappingURL={}\n", self.code, url),
            None => self.code.clone(),
        }
    }

    pub fn has_refresh_registrations(&self) -> bool {
        self.is_refresh_boundary
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CompileError {
    #[error("unsupported file type: {0}")]
    UnsupportedFileType(PathBuf),
    #[error("parse error in {path}:\n{message}")]
    Parse { path: PathBuf, message: String },
    #[error("transform error in {path}:\n{message}")]
    Transform { path: PathBuf, message: String },
}

pub fn compile(
    path: &Path,
    source_text: &str,
    opts: &CompileOptions,
) -> Result<CompileOutput, CompileError> {
    compile_module(path, source_text, opts, None)
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
    let mut names = Vec::new();
    for stmt in &parsed.program.body {
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

pub fn compile_module(
    path: &Path,
    source_text: &str,
    opts: &CompileOptions,
    rewriter: Option<&mut ImportRewriter>,
) -> Result<CompileOutput, CompileError> {
    compile_module_with_maps(path, source_text, opts, rewriter, &[])
}

pub fn compile_module_with_maps(
    path: &Path,
    source_text: &str,
    opts: &CompileOptions,
    mut rewriter: Option<&mut ImportRewriter>,
    input_maps: &[String],
) -> Result<CompileOutput, CompileError> {
    let source_type = SourceType::from_path(path)
        .map_err(|_| CompileError::UnsupportedFileType(path.to_path_buf()))?;

    let allocator = Allocator::default();

    let parsed = Parser::new(&allocator, source_text, source_type).parse();
    if parsed.panicked || !parsed.diagnostics.is_empty() {
        let message = parsed
            .diagnostics
            .into_iter()
            .map(|d| format!("{:?}", d.with_source_code(source_text.to_string())))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(CompileError::Parse {
            path: path.to_path_buf(),
            message,
        });
    }
    let mut program = parsed.program;

    let semantic_ret = SemanticBuilder::new()
        .with_excess_capacity(2.0)
        .with_enum_eval(true)
        .build(&program);
    let scoping = semantic_ret.semantic.into_scoping();

    let mut transform_options = TransformOptions::default();
    transform_options.jsx.jsx_plugin = true;
    if opts.jsx.is_classic() {
        transform_options.jsx.runtime = JsxRuntime::Classic;
        // oxc rejects pragma/pragmaFrag under the automatic runtime, so they are
        // only set for classic.
        transform_options.jsx.pragma = opts.jsx.pragma.clone();
        transform_options.jsx.pragma_frag = opts.jsx.pragma_frag.clone();
    } else {
        transform_options.jsx.runtime = JsxRuntime::Automatic;
        transform_options.jsx.import_source = opts.jsx.import_source.clone();
    }
    transform_options.jsx.development = opts.dev;
    transform_options.jsx.jsx_self_plugin = opts.dev;
    transform_options.jsx.jsx_source_plugin = opts.dev;
    if opts.dev && opts.refresh {
        transform_options.jsx.refresh = Some(ReactRefreshOptions::default());
    }

    let transform_ret = Transformer::new(&allocator, path, &transform_options)
        .build_with_scoping(scoping, &mut program);
    if !transform_ret.diagnostics.is_empty() {
        let message = transform_ret
            .diagnostics
            .into_iter()
            .map(|d| format!("{:?}", d.with_source_code(source_text.to_string())))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(CompileError::Transform {
            path: path.to_path_buf(),
            message,
        });
    }

    let defines = import_meta_env_defines(opts.dev, opts.ssr);
    if defines.needed_by(source_text) {
        if let Some(config) = defines.config() {
            let scoping = SemanticBuilder::new()
                .build(&program)
                .semantic
                .into_scoping();
            let _ = ReplaceGlobalDefines::new(&allocator, config).build(scoping, &mut program);
        }
    }

    // Track synthesized expansions: they splice generated nodes with no faithful
    // source origin, so their sourcemap must be skipped (see codegen below).
    let mut synthesized = false;
    if F_IMPORT_META_GLOB.find(source_text.as_bytes()).is_some() {
        let dir = path.parent().unwrap_or(path);
        glob::expand(&allocator, dir, &mut program);
        synthesized = true;
    }

    // Expand dynamic-import-vars (import(`./x/${v}.js`)) in dev too — the build
    // path already does; without it these work in `oj build` but throw in dev.
    if scan(&F_IMPORT_PAREN, source_text) {
        let dir = path.parent().unwrap_or(path);
        synthesized |= glob::expand_dynamic_import_vars(&allocator, dir, &mut program, source_text);
    }

    // new URL("./asset", import.meta.url) -> a hoisted ?url asset import.
    if source_text.contains("import.meta.url") {
        let dir = path.parent().unwrap_or(path);
        synthesized |= glob::expand_new_url_asset(&allocator, dir, &mut program, source_text);
    }

    let (imports, dynamic_imports) =
        rewrite_module_specifiers(&allocator, &mut program, &mut rewriter);
    let hot_accept = lex_hot_accept(&allocator, &mut program, &mut rewriter);

    let is_refresh_boundary = opts.refresh && detect_refresh_registrations(&program);

    // Synthesized nodes (glob / dynamic-import-vars) carry generated-string spans
    // with no origin in this module's source; sourcemapping them panics oxc's
    // builder on out-of-range spans, so skip the map for those modules.
    let codegen_options = CodegenOptions {
        source_map_path: (opts.sourcemap && !synthesized).then(|| path.to_path_buf()),
        ..CodegenOptions::default()
    };
    let CodegenReturn { code, map, .. } =
        Codegen::new().with_options(codegen_options).build(&program);

    let map_data_url = map.map(|oj_map| {
        if input_maps.is_empty() {
            oj_map.to_data_url()
        } else {
            compose_input_maps_data_url(&oj_map, input_maps)
        }
    });

    Ok(CompileOutput {
        code,
        map_data_url,
        imports,
        dynamic_imports,
        is_refresh_boundary,
        hot_accept,
    })
}

/// Find `import.meta.hot` references and lex the `accept` calls, rewriting the
/// dependency specifiers they name with the same rewriter as the imports so the
/// client can match them against the update's `acceptedPath`.
fn lex_hot_accept<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    rewriter: &mut Option<&mut ImportRewriter>,
) -> Option<HotAccept> {
    use oxc_ast::ast::{Argument, ArrayExpressionElement, CallExpression, Expression, StaticMemberExpression};
    use oxc_ast_visit::{walk_mut, VisitMut};

    fn is_import_meta_hot(e: &Expression) -> bool {
        match e {
            Expression::StaticMemberExpression(m) => {
                m.property.name == "hot" && matches!(&m.object, Expression::ImportMeta(_))
            }
            Expression::ParenthesizedExpression(p) => is_import_meta_hot(&p.expression),
            _ => false,
        }
    }

    struct Lexer<'a, 'r> {
        allocator: &'a Allocator,
        rewriter: &'r mut ImportRewriter<'r>,
        uses_hot: bool,
        accept: HotAccept,
    }
    impl<'a> Lexer<'a, '_> {
        fn rewrite(&mut self, lit: &mut StringLiteral<'a>) -> String {
            if let Some(new_spec) = (self.rewriter)(lit.value.as_str()) {
                lit.value = self.allocator.alloc_str(&new_spec).into();
                lit.raw = None;
            }
            lit.value.to_string()
        }
    }
    impl<'a> VisitMut<'a> for Lexer<'a, '_> {
        fn visit_static_member_expression(&mut self, it: &mut StaticMemberExpression<'a>) {
            if it.property.name == "hot" && matches!(&it.object, Expression::ImportMeta(_)) {
                self.uses_hot = true;
            }
            walk_mut::walk_static_member_expression(self, it);
        }
        fn visit_call_expression(&mut self, call: &mut CallExpression<'a>) {
            let is_accept = match &call.callee {
                Expression::StaticMemberExpression(m) => {
                    (m.property.name == "accept" || m.property.name == "acceptExports")
                        && is_import_meta_hot(&m.object)
                }
                _ => false,
            };
            if is_accept {
                self.uses_hot = true;
                let is_exports = matches!(&call.callee, Expression::StaticMemberExpression(m) if m.property.name == "acceptExports");
                match call.arguments.first_mut() {
                    None => self.accept.self_accepting = true,
                    Some(Argument::StringLiteral(lit)) if !is_exports => {
                        let dep = self.rewrite(lit);
                        self.accept.deps.push(dep);
                    }
                    Some(Argument::ArrayExpression(arr)) if !is_exports => {
                        for el in arr.elements.iter_mut() {
                            if let ArrayExpressionElement::StringLiteral(lit) = el {
                                let dep = self.rewrite(lit);
                                self.accept.deps.push(dep);
                            }
                        }
                    }
                    // accept(cb), acceptExports(names, cb), or anything dynamic
                    Some(_) => self.accept.self_accepting = true,
                }
            }
            walk_mut::walk_call_expression(self, call);
        }
    }

    let mut noop = |_: &str| None;
    let accept = match rewriter.as_deref_mut() {
        Some(rw) => {
            let mut lexer = Lexer { allocator, rewriter: rw, uses_hot: false, accept: HotAccept::default() };
            lexer.visit_program(program);
            (lexer.uses_hot, lexer.accept)
        }
        None => {
            let mut lexer = Lexer { allocator, rewriter: &mut noop, uses_hot: false, accept: HotAccept::default() };
            lexer.visit_program(program);
            (lexer.uses_hot, lexer.accept)
        }
    };
    let (uses_hot, mut accept) = accept;
    if !uses_hot {
        return None;
    }
    accept.deps.sort();
    accept.deps.dedup();
    Some(accept)
}

fn compose_input_maps_data_url(oj_map: &oxc_sourcemap::SourceMap, input_maps: &[String]) -> String {
    let mut acc = oj_map.to_json_string();
    for pm in input_maps.iter().rev() {
        let outer = match oxc_sourcemap::SourceMap::from_json_string(&acc) {
            Ok(m) => m,
            Err(_) => return oj_map.to_data_url(),
        };
        let inner = match oxc_sourcemap::SourceMap::from_json_string(pm) {
            Ok(m) => m,
            Err(_) => continue,
        };
        acc = compose_two(&outer, &inner).to_json_string();
    }
    match oxc_sourcemap::SourceMap::from_json_string(&acc) {
        Ok(m) => m.to_data_url(),
        Err(_) => oj_map.to_data_url(),
    }
}

fn compose_two<'i>(
    outer: &oxc_sourcemap::SourceMap,
    inner: &'i oxc_sourcemap::SourceMap,
) -> oxc_sourcemap::SourceMap<'i> {
    let lut = inner.generate_lookup_table();
    let mut b = oxc_sourcemap::SourceMapBuilder::default();
    for t in outer.get_tokens() {
        if t.get_source_id().is_none() {
            continue;
        }
        if let Some(vt) =
            inner.lookup_source_view_token_approx(&lut, t.get_src_line(), t.get_src_col())
        {
            let src_id = vt
                .get_source()
                .map(|s| b.add_source_and_content(s, vt.get_source_content().unwrap_or("")));
            let name_id = vt.get_name().map(|n| b.add_name(n));
            b.add_token(
                t.get_dst_line(),
                t.get_dst_col(),
                vt.get_src_line(),
                vt.get_src_col(),
                src_id,
                name_id,
            );
        }
    }
    b.into_sourcemap()
}

pub(crate) fn rewrite_module_specifiers_pub<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    rewriter: &mut ImportRewriter,
) -> (Vec<String>, Vec<String>) {
    let mut opt: Option<&mut ImportRewriter> = Some(rewriter);
    rewrite_module_specifiers(allocator, program, &mut opt)
}

fn rewrite_module_specifiers<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    rewriter: &mut Option<&mut ImportRewriter>,
) -> (Vec<String>, Vec<String>) {
    let mut imports = Vec::new();
    let mut dynamic_imports = Vec::new();
    for stmt in program.body.iter_mut() {
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
        imports.push(lit.value.to_string());
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
    (imports, dynamic_imports)
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
    use super::*;

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
        assert_eq!(got, vec!["react".to_string(), "./b".to_string(), "./c".to_string()]);
    }

    #[test]
    fn imports_empty_for_no_imports() {
        assert!(imports("export const x = 1;", Path::new("m.ts")).is_empty());
    }

    #[test]
    fn compose_two_traces_served_position_back_to_original_source() {
        use oxc_sourcemap::SourceMapBuilder;
        // inner (plugin map): intermediate (0,0) -> original src.tsx (5,2).
        let mut ib = SourceMapBuilder::default();
        let sid = ib.add_source_and_content("src.tsx", "original source");
        ib.add_token(0, 0, 5, 2, Some(sid), None);
        let inner = ib.into_sourcemap();
        // outer (oj/oxc map): served (1,4) -> intermediate (0,0).
        let mut ob = SourceMapBuilder::default();
        let iid = ob.add_source_and_content("intermediate.js", "intermediate");
        ob.add_token(1, 4, 0, 0, Some(iid), None);
        let outer = ob.into_sourcemap();

        let composed = compose_two(&outer, &inner);
        let lut = composed.generate_lookup_table();
        let vt = composed
            .lookup_source_view_token(&lut, 1, 4)
            .expect("served (1,4) should map");
        // The served position now points at the ORIGINAL source, not the intermediate.
        assert_eq!(vt.get_source(), Some("src.tsx"), "source is the original file");
        assert_eq!(vt.get_src_line(), 5, "original line preserved through compose");
        assert_eq!(vt.get_src_col(), 2, "original column preserved through compose");
    }

    #[test]
    fn compose_input_maps_data_url_folds_and_degrades_gracefully() {
        use oxc_sourcemap::{SourceMap, SourceMapBuilder};
        let mut ib = SourceMapBuilder::default();
        let sid = ib.add_source_and_content("app.tsx", "let x = 1;");
        ib.add_token(0, 0, 9, 3, Some(sid), None);
        let plugin_map = ib.into_sourcemap().to_json_string();

        let mut ob = SourceMapBuilder::default();
        let iid = ob.add_source_and_content("app.plugin.js", "intermediate");
        ob.add_token(0, 0, 0, 0, Some(iid), None);
        let oj_map = ob.into_sourcemap();

        // The fold traces through the plugin map to the original file.
        let folded =
            compose_two(&oj_map, &SourceMap::from_json_string(&plugin_map).unwrap()).to_json_string();
        assert!(folded.contains("app.tsx"), "folded map references the original source: {folded}");

        // The public entry point emits an inline JSON sourcemap data URL and never panics.
        let url = compose_input_maps_data_url(&oj_map, &[plugin_map]);
        assert!(
            url.starts_with("data:application/json") && url.contains("base64,"),
            "emits an inline data URL: {url}",
        );

        // Garbage input maps degrade to oj's own map rather than erroring.
        let fallback = compose_input_maps_data_url(&oj_map, &["not json".to_string()]);
        assert!(fallback.starts_with("data:application/json") && fallback.contains("base64,"));
    }

    #[test]
    fn defines_apply_after_jsx_transform_without_reference_id_panic() {
        // import.meta.env inside JSX: the JSX/TS transform introduces
        // IdentifierReferences without reference_ids, and ReplaceGlobalDefines
        // reads reference_id() while walking the whole program. Before scoping
        // was rebuilt on the transformed program this panicked; it must compile
        // and still apply the defines.
        let src = r#"
export function App() {
  return <div className={import.meta.env.DEV ? "dev" : "prod"}>{import.meta.env.MODE}</div>;
}
"#;
        let out =
            compile_module(Path::new("App.tsx"), src, &CompileOptions::prod(), None).unwrap();
        assert!(out.code.contains("false"), "import.meta.env.DEV -> false: {}", out.code);
        assert!(out.code.contains("production"), "MODE -> production: {}", out.code);
    }

    #[test]
    fn enum_declarations_compile() {
        // oxc's TS enum transform requires with_enum_eval(true); without it the
        // transformer panics on any enum.
        let src = "export enum Dir { Up, Down }\nexport const d = Dir.Up;";
        let out = compile_module(Path::new("e.ts"), src, &CompileOptions::prod(), None).unwrap();
        assert!(!out.code.is_empty(), "{}", out.code);
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

    const APP_TSX: &str = r#"
interface Props { label: string }

export function Counter({ label }: Props) {
  const [n, setN] = React.useState<number>(0);
  return <button onClick={() => setN(n + 1)}>{label}: {n}</button>;
}

import React from "react";
"#;

    #[test]
    fn strips_types_and_uses_automatic_runtime_in_prod() {
        let out = compile(Path::new("App.tsx"), APP_TSX, &CompileOptions::prod()).unwrap();
        assert!(!out.code.contains("interface"), "types must be stripped");
        assert!(!out.code.contains("<button"), "JSX must be transformed");
        assert!(
            out.code.contains("react/jsx-runtime"),
            "prod uses the automatic runtime:\n{}",
            out.code
        );
        assert!(out.map_data_url.is_some());
    }

    #[test]
    fn dev_uses_jsx_dev_runtime_and_instruments_fast_refresh() {
        let out = compile(Path::new("App.tsx"), APP_TSX, &CompileOptions::dev()).unwrap();
        assert!(
            out.code.contains("react/jsx-dev-runtime"),
            "dev uses jsxDEV:\n{}",
            out.code
        );
        assert!(
            out.code.contains("$RefreshReg$"),
            "components must be registered for Fast Refresh:\n{}",
            out.code
        );
        assert!(
            out.code.contains("$RefreshSig$"),
            "hook users must be signed for Fast Refresh:\n{}",
            out.code
        );
    }

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
    fn reports_parse_errors_instead_of_panicking() {
        let err = compile(
            Path::new("Broken.tsx"),
            "const = <div>;",
            &CompileOptions::dev(),
        )
        .unwrap_err();
        assert!(matches!(err, CompileError::Parse { .. }));
    }

    #[test]
    fn fallback_variants_are_cached_per_dev_ssr() {
        // The unit tests never call set_import_meta_env, so every lookup lands
        // on the fallback table: the same (dev, ssr) hands back the same Arc,
        // and each variant is its own entry.
        assert!(Arc::ptr_eq(
            &import_meta_env_defines(true, false),
            &import_meta_env_defines(true, false)
        ));
        assert!(Arc::ptr_eq(
            &import_meta_env_defines(false, true),
            &import_meta_env_defines(false, true)
        ));
        for (a, b) in [
            ((true, false), (true, true)),
            ((true, false), (false, false)),
            ((false, true), (true, true)),
            ((false, false), (false, true)),
        ] {
            assert!(
                !Arc::ptr_eq(
                    &import_meta_env_defines(a.0, a.1),
                    &import_meta_env_defines(b.0, b.1)
                ),
                "{a:?} and {b:?} must be distinct fallback variants"
            );
        }
    }

    #[test]
    fn replaces_import_meta_env_flags_per_mode() {
        let src = "export const mode = import.meta.env.MODE;\n\
                   export const dev = import.meta.env.DEV;\n\
                   export const prod = import.meta.env.PROD;";
        let prod = compile(Path::new("env.ts"), src, &CompileOptions::prod()).unwrap();
        assert!(
            !prod.code.contains("import.meta.env"),
            "defines must be replaced:\n{}",
            prod.code
        );
        assert!(
            prod.code.contains("\"production\""),
            "MODE is production:\n{}",
            prod.code
        );
        assert!(
            prod.code.contains("prod = true"),
            "PROD is true in prod:\n{}",
            prod.code
        );
        assert!(
            prod.code.contains("dev = false"),
            "DEV is false in prod:\n{}",
            prod.code
        );

        let dev = compile(Path::new("env.ts"), src, &CompileOptions::dev()).unwrap();
        assert!(
            dev.code.contains("\"development\""),
            "MODE is development:\n{}",
            dev.code
        );
        assert!(
            dev.code.contains("dev = true"),
            "DEV is true in dev:\n{}",
            dev.code
        );
        assert!(
            dev.code.contains("prod = false"),
            "PROD is false in dev:\n{}",
            dev.code
        );
    }

    #[test]
    fn dev_compile_expands_dynamic_import_vars() {
        // import(`./x/${v}.js`) must expand in the DEV compile path too — the
        // build path already does, so without this it works in `oj build` and
        // throws at runtime under `oj dev`.
        let dir = std::env::temp_dir().join(format!("oj-dynimport-{}", std::process::id()));
        let loc = dir.join("locales");
        std::fs::create_dir_all(&loc).unwrap();
        std::fs::write(loc.join("en.json"), "{}").unwrap();
        std::fs::write(loc.join("fr.json"), "{}").unwrap();
        let src = "export const load = (l) => import(`./locales/${l}.json`);\n";
        let out = compile(&dir.join("main.js"), src, &CompileOptions::dev()).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            out.code.contains("./locales/en.json"),
            "dyn-import-var must expand in dev:\n{}",
            out.code
        );
        assert!(out.code.contains("./locales/fr.json"), "{}", out.code);
        assert!(
            out.code.contains("case "),
            "expanded to a switch over matches:\n{}",
            out.code
        );
    }

    #[test]
    fn dev_compile_rewrites_new_url_import_meta_url() {
        // new URL("./x", import.meta.url) -> a hoisted ?url asset import ref, so
        // the asset flows through oj's pipeline instead of 404-ing.
        let src = "export const w = new URL(\"./worker.js\", import.meta.url);\n";
        let out = compile(std::path::Path::new("m.js"), src, &CompileOptions::dev()).unwrap();
        assert!(
            out.code.contains("__oj_url_0"),
            "rewritten to hoisted asset ref:\n{}",
            out.code
        );
        assert!(
            out.code.contains("worker.js?url"),
            "hoisted ?url import:\n{}",
            out.code
        );
        assert!(
            !out.code.contains("new URL(\"./worker.js\""),
            "original literal replaced:\n{}",
            out.code
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
    fn fast_refresh_only_in_dev_with_refresh_enabled() {
        let prod = compile(Path::new("C.tsx"), APP_TSX, &CompileOptions::prod()).unwrap();
        assert!(!prod.has_refresh_registrations());
        let dev_no_refresh = compile_module(
            Path::new("C.tsx"),
            APP_TSX,
            &CompileOptions {
                dev: true,
                refresh: false,
                sourcemap: false,
                ssr: false,
                jsx: JsxConfig::default(),
            },
            None,
        )
        .unwrap();
        assert!(!dev_no_refresh.has_refresh_registrations());
        assert!(
            dev_no_refresh.code.contains("jsx-dev-runtime"),
            "dev runtime regardless of refresh"
        );
    }

    #[test]
    fn rejects_unsupported_file_types() {
        let err = compile(Path::new("styles.css"), "body{}", &CompileOptions::prod()).unwrap_err();
        assert!(
            matches!(err, CompileError::UnsupportedFileType(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn lexes_import_meta_hot_accept_forms_and_rewrites_dep_specifiers() {
        let mut rw = |spec: &str| spec.strip_prefix("./").map(|r| format!("/src/{r}"));
        let none = compile_module(Path::new("a.ts"), "export const x = 1;", &CompileOptions::dev(), Some(&mut rw)).unwrap();
        assert_eq!(none.hot_accept, None);

        let self_accept = compile_module(
            Path::new("a.ts"),
            "export const x = 1;\nif (import.meta.hot) { import.meta.hot.accept(); }",
            &CompileOptions::dev(),
            Some(&mut rw),
        )
        .unwrap();
        assert_eq!(self_accept.hot_accept, Some(HotAccept { self_accepting: true, deps: vec![] }));

        let cb = compile_module(Path::new("a.ts"), "import.meta.hot?.accept((m) => m);", &CompileOptions::dev(), Some(&mut rw)).unwrap();
        assert!(cb.hot_accept.unwrap().self_accepting);

        let deps = compile_module(
            Path::new("a.ts"),
            "import { v } from './util.js';\nimport.meta.hot.accept(['./util.js', './other.js'], ([u]) => u);\nimport.meta.hot.accept('./one.js', (m) => m);",
            &CompileOptions::dev(),
            Some(&mut rw),
        )
        .unwrap();
        let hot = deps.hot_accept.unwrap();
        assert!(!hot.self_accepting);
        assert_eq!(hot.deps, vec!["/src/one.js", "/src/other.js", "/src/util.js"]);
        assert!(deps.code.contains("\"/src/util.js\", \"/src/other.js\"") || deps.code.contains("\"/src/util.js\",\"/src/other.js\""), "{}", deps.code);
        assert!(deps.code.contains("\"/src/one.js\""), "{}", deps.code);

        let read_only = compile_module(Path::new("a.ts"), "console.log(import.meta.hot?.data);", &CompileOptions::dev(), Some(&mut rw)).unwrap();
        assert_eq!(read_only.hot_accept, Some(HotAccept::default()), "referencing import.meta.hot needs a context even without accept");
    }

    #[test]
    fn jsx_import_source_from_config_and_pragma_comment() {
        let src = "export const A = () => <div>hi</div>;\n";
        let mut opts = CompileOptions::prod();
        opts.jsx.import_source = Some("preact".into());
        let out = compile_module(Path::new("A.tsx"), src, &opts, None).unwrap();
        assert!(out.imports.iter().any(|i| i == "preact/jsx-runtime"), "{:?}", out.imports);
        assert!(!out.code.contains("\"react/jsx-runtime\""), "{}", out.code);

        // Dev uses the dev runtime of the same source.
        let mut dev = CompileOptions::dev();
        dev.jsx.import_source = Some("@emotion/react".into());
        let out = compile_module(Path::new("A.tsx"), src, &dev, None).unwrap();
        assert!(out.imports.iter().any(|i| i == "@emotion/react/jsx-dev-runtime"), "{:?}", out.imports);

        // A file pragma wins over the config (oxc reads leading comments).
        let pragma = format!("/** @jsxImportSource solid-js */\n{src}");
        let out = compile_module(Path::new("A.tsx"), &pragma, &opts, None).unwrap();
        assert!(out.imports.iter().any(|i| i == "solid-js/jsx-runtime"), "{:?}", out.imports);
    }

    #[test]
    fn classic_runtime_uses_configured_pragma() {
        let src = "import { h, Fragment } from 'preact';\nexport const A = () => <><b>x</b></>;\n";
        let mut opts = CompileOptions::prod();
        opts.jsx = JsxConfig {
            runtime: Some("classic".into()),
            import_source: None,
            pragma: Some("h".into()),
            pragma_frag: Some("Fragment".into()),
        };
        let out = compile_module(Path::new("A.tsx"), src, &opts, None).unwrap();
        assert!(out.code.contains("h(Fragment"), "{}", out.code);
        assert!(out.code.contains("h(\"b\""), "{}", out.code);
        assert!(!out.imports.iter().any(|i| i.contains("jsx-runtime")), "{:?}", out.imports);
    }

    #[test]
    fn sourcemap_toggle_and_inline_map_helper() {
        let no_map = compile_module(
            Path::new("a.ts"),
            "export const x = 1;",
            &CompileOptions {
                dev: false,
                refresh: false,
                sourcemap: false,
                ssr: false,
                jsx: JsxConfig::default(),
            },
            None,
        )
        .unwrap();
        assert!(no_map.map_data_url.is_none());
        assert_eq!(no_map.code_with_inline_map(), no_map.code);

        let with_map = compile(
            Path::new("a.ts"),
            "export const x = 1;",
            &CompileOptions::prod(),
        )
        .unwrap();
        assert!(with_map.map_data_url.is_some());
        assert!(with_map
            .code_with_inline_map()
            .contains("sourceMappingURL="));
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
