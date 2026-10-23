//! Public transform surface (design-core.md §3): the memchr pre-gate, the
//! splice compile (CLI/harness/rolldown), and the AST compile (oj dev path).

use std::path::Path;

use memchr::memmem;
use oxc_allocator::Allocator;
use oxc_ast::ast::Program;
use oxc_parser::Parser;
use oxc_span::SourceType;

use crate::errors::StylexError;
use crate::eval::value::JsObjectMap;
use crate::imports::{ATOMS_SOURCE, ImportTable, scan_imports};
use crate::module_resolution::{FsProvider, THEME_FILE_EXTENSION};
use crate::options::{ImportSource, ResolvedOptions};
use crate::rules::StylexRule;
use crate::state::CompileState;
use crate::transform::ast_backend::apply_plan;
use crate::transform::visitor::transform_module;

pub struct FileContext<'s> {
    /// Absolute path the source pretends to live at.
    pub filename: &'s Path,
    pub source_text: &'s str,
    /// process.cwd() equivalent; pins `$$css` debug-string derivation.
    pub cwd: &'s Path,
}

#[derive(Debug)]
pub struct SourceCompileResult {
    pub code: String,
    pub map: Option<String>,
    pub rules: Vec<StylexRule>,
    pub modified: bool,
    /// (var name, compiled namespaces) per create call, pre-DCE, in order.
    pub create_objects: Vec<(Option<String>, std::sync::Arc<JsObjectMap>)>,
}

/// What [`transform_source_in_with_map`] renders into `SourceCompileResult::map`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapMode {
    Off,
    /// Mappings and `sources` only; the consumer supplies `sourcesContent`.
    Mappings,
    WithContent,
}

pub fn might_contain_stylex(source: &str, options: &ResolvedOptions) -> bool {
    pre_gate(source, options).is_some()
}

/// `None` = the file cannot reference any stylex import source; `Some(sx)`
/// carries whether the sx-prop needle hit, sparing the dormancy check a rescan.
fn pre_gate(source: &str, options: &ResolvedOptions) -> Option<bool> {
    let bytes = source.as_bytes();
    // The sx prop compiles with no stylex import in the file at all.
    if options
        .sx_prop_name
        .as_deref()
        .is_some_and(|sx| memmem::find(bytes, sx.as_bytes()).is_some())
    {
        return Some(true);
    }
    // Atoms compile off a hardcoded source no importSources setting covers; a
    // rewritable import source must carry the hardcoded `.stylex` suffix.
    let needles = || {
        options
            .import_sources
            .iter()
            .map(ImportSource::from_specifier)
            .chain(std::iter::once(ATOMS_SOURCE))
            .chain(options.rewrite_aliases.then_some(THEME_FILE_EXTENSION))
    };
    // A needle containing another (or repeating an earlier one) can only hit
    // where that one hits: the default set collapses to "stylex" alone.
    let hit = needles().enumerate().any(|(i, needle)| {
        let redundant = needles().enumerate().any(|(j, other)| {
            (other.len() < needle.len() && needle.contains(other)) || (j < i && other == needle)
        });
        !redundant && memmem::find(bytes, needle.as_bytes()).is_some()
    });
    // A `\u`/`\x` escape can cook into an import-source match the raw needles
    // miss (`"@stylexjs/stylex"`); parse and decide post-parse.
    let escapes =
        || memchr::memchr_iter(b'\\', bytes).any(|i| matches!(bytes.get(i + 1), Some(b'u' | b'x')));
    (hit || escapes()).then_some(false)
}

/// `None` = pre-gate skip (the file cannot reference any stylex import source).
pub fn transform_source(
    ctx: &FileContext<'_>,
    options: &ResolvedOptions,
    fs: &dyn FsProvider,
) -> Result<Option<SourceCompileResult>, StylexError> {
    let allocator = Allocator::default();
    transform_source_in(&allocator, ctx, options, fs)
}

/// [`transform_source`] parsing into the caller's arena (reusable across jobs
/// via `reset()` — nothing in the result borrows it).
pub fn transform_source_in(
    allocator: &Allocator,
    ctx: &FileContext<'_>,
    options: &ResolvedOptions,
    fs: &dyn FsProvider,
) -> Result<Option<SourceCompileResult>, StylexError> {
    transform_source_mapped_in(allocator, ctx, options, fs, false)
}

/// [`transform_source_in`] that also builds a v3 sourcemap; emitted text is
/// byte-identical either way, so the map stays opt-in.
pub fn transform_source_mapped_in(
    allocator: &Allocator,
    ctx: &FileContext<'_>,
    options: &ResolvedOptions,
    fs: &dyn FsProvider,
    want_map: bool,
) -> Result<Option<SourceCompileResult>, StylexError> {
    let mode = if want_map {
        MapMode::WithContent
    } else {
        MapMode::Off
    };
    transform_source_in_with_map(allocator, ctx, options, fs, mode)
}

/// [`transform_source_mapped_in`] with the map's content policy chosen by the caller.
pub fn transform_source_in_with_map(
    allocator: &Allocator,
    ctx: &FileContext<'_>,
    options: &ResolvedOptions,
    fs: &dyn FsProvider,
    map_mode: MapMode,
) -> Result<Option<SourceCompileResult>, StylexError> {
    let want_map = map_mode != MapMode::Off;
    let Some(sx_hit) = pre_gate(ctx.source_text, options) else {
        return Ok(None);
    };
    let filename = ctx.filename.to_string_lossy().replace('\\', "/");
    let program = {
        let _t = crate::timings::start(crate::timings::Stage::Parse);
        parse_program(allocator, ctx.source_text, &filename)?
    };
    let Some(imports) = scan_unless_dormant(&program, sx_hit, options)? else {
        return Ok(Some(SourceCompileResult {
            code: ctx.source_text.to_string(),
            map: None,
            rules: Vec::new(),
            modified: false,
            create_objects: Vec::new(),
        }));
    };
    let filename_for_map = want_map.then(|| filename.clone());
    let mut state = CompileState::build_with_imports(
        &program,
        options,
        Some(filename),
        ctx.cwd.to_string_lossy().replace('\\', "/"),
        imports,
    );
    let out = {
        let _t = crate::timings::start(crate::timings::Stage::Transform);
        transform_module(&program, ctx.source_text, &mut state, fs, false, want_map)?
    };
    let map = out.splice_map.as_ref().map(|m| {
        render_sourcemap(
            m,
            filename_for_map.as_deref().unwrap_or_default(),
            (map_mode == MapMode::WithContent).then_some(ctx.source_text),
        )
    });
    Ok(Some(SourceCompileResult {
        code: out.code,
        map,
        rules: state.rules,
        modified: out.modified,
        create_objects: out.create_objects,
    }))
}

/// [`transform_source`] that also returns the fs-dependency log the cache
/// needs (crate::cache); recording costs one package.json read per hit dir.
pub fn transform_source_with_dep_log(
    ctx: &FileContext<'_>,
    options: &ResolvedOptions,
    fs: &dyn FsProvider,
) -> Result<(Option<SourceCompileResult>, crate::cache::DepLog), StylexError> {
    let allocator = Allocator::default();
    transform_source_with_dep_log_in(&allocator, ctx, options, fs)
}

/// [`transform_source_with_dep_log`] in the caller's arena (see
/// [`transform_source_in`]).
pub fn transform_source_with_dep_log_in(
    allocator: &Allocator,
    ctx: &FileContext<'_>,
    options: &ResolvedOptions,
    fs: &dyn FsProvider,
) -> Result<(Option<SourceCompileResult>, crate::cache::DepLog), StylexError> {
    let recorder = crate::cache::RecordingFs::new(fs, ctx.cwd);
    let result = transform_source_in(allocator, ctx, options, &recorder)?;
    Ok((result, recorder.into_log()))
}

/// AST-backend result: rules plus whether the caller's program was mutated.
#[derive(Debug)]
pub struct CompileResult {
    pub modified: bool,
    pub rules: Vec<StylexRule>,
    /// (var name, compiled namespaces) per create call, pre-DCE, in order.
    pub create_objects: Vec<(Option<String>, std::sync::Arc<JsObjectMap>)>,
}

/// oj dev path: mutates the caller's AST in place; the program must be the
/// parse of `ctx.source_text` (synthesized-node spans point into it).
pub fn transform_program<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    ctx: &FileContext<'_>,
    options: &ResolvedOptions,
    fs: &dyn FsProvider,
) -> Result<CompileResult, StylexError> {
    let Some(sx_hit) = pre_gate(ctx.source_text, options) else {
        return Ok(CompileResult {
            modified: false,
            rules: Vec::new(),
            create_objects: Vec::new(),
        });
    };
    let Some(imports) = scan_unless_dormant(program, sx_hit, options)? else {
        return Ok(CompileResult {
            modified: false,
            rules: Vec::new(),
            create_objects: Vec::new(),
        });
    };
    let filename = ctx.filename.to_string_lossy().replace('\\', "/");
    let cwd = ctx.cwd.to_string_lossy().replace('\\', "/");
    // Read-only analysis at a shorter reborrow (the AST is covariant over
    // the arena lifetime) yields an owned plan; the mutation follows.
    let (plan, modified, rules, create_objects) = {
        let program_ref: &Program<'_> = &*program;
        let mut state =
            CompileState::build_with_imports(program_ref, options, Some(filename), cwd, imports);
        let out = {
            let _t = crate::timings::start(crate::timings::Stage::Transform);
            transform_module(program_ref, ctx.source_text, &mut state, fs, true, false)?
        };
        (out.plan, out.modified, state.rules, out.create_objects)
    };
    {
        let _t = crate::timings::start(crate::timings::Stage::ApplyPlan);
        apply_plan(allocator, program, plan)?;
    }
    Ok(CompileResult {
        modified,
        rules,
        create_objects,
    })
}

/// [`transform_program`] with the cache's fs-dependency log, mirroring
/// [`transform_source_with_dep_log`].
pub fn transform_program_with_dep_log<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    ctx: &FileContext<'_>,
    options: &ResolvedOptions,
    fs: &dyn FsProvider,
) -> Result<(CompileResult, crate::cache::DepLog), StylexError> {
    let recorder = crate::cache::RecordingFs::new(fs, ctx.cwd);
    let result = transform_program(allocator, program, ctx, options, &recorder)?;
    Ok((result, recorder.into_log()))
}

// Dormant (`None`): no stylex binding, no possible sx prop (it compiles with no
// import at all), and no `rewriteAliases` (rewrites imports with no binding).
fn scan_unless_dormant(
    program: &Program<'_>,
    sx_hit: bool,
    options: &ResolvedOptions,
) -> Result<Option<ImportTable>, StylexError> {
    let _t = crate::timings::start(crate::timings::Stage::ImportScan);
    let imports = scan_imports(program, options)?;
    Ok((options.rewrite_aliases || sx_hit || !imports.is_dormant()).then_some(imports))
}

/// Serializes splice positions as a v3 sourcemap, inlining the original when
/// `source_text` is given.
fn render_sourcemap(
    map: &crate::transform::js_out::SpliceMap,
    filename: &str,
    source_text: Option<&str>,
) -> String {
    let tokens: Vec<oxc_sourcemap::Token> = map
        .tokens
        .iter()
        .map(|&(dst_line, dst_col, src_line, src_col)| {
            oxc_sourcemap::Token::new(dst_line, dst_col, src_line, src_col, Some(0), None)
        })
        .collect();
    oxc_sourcemap::SourceMap::new(
        Some(filename.into()),
        Vec::new(),
        None,
        vec![filename.into()],
        vec![source_text.map(Into::into)],
        tokens.into_boxed_slice(),
        None,
    )
    .to_json_string()
}

// The oracle parses every file with the typescript+jsx plugins regardless of
// extension (`input.js` with `f<T>(0)` is a generic call, not comparisons).
pub fn parse_program<'a>(
    allocator: &'a Allocator,
    source: &'a str,
    _filename: &str,
) -> Result<Program<'a>, StylexError> {
    let ret = Parser::new(allocator, source, SourceType::tsx()).parse();
    if !ret.fatal_error && !ret.diagnostics.has_errors() {
        return Ok(ret.program);
    }
    let detail = ret
        .diagnostics
        .errors()
        .next()
        .map(|d| d.message.to_string())
        .unwrap_or_else(|| "unknown parse error".to_string());
    Err(StylexError::parse_error(&detail))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(json: serde_json::Value) -> ResolvedOptions {
        crate::options::CompilerOptions::from_json(&json)
            .unwrap()
            .resolve()
            .unwrap()
    }

    #[test]
    fn pre_gate_admits_every_needle_and_reports_sx() {
        let opts = ResolvedOptions::default();
        assert_eq!(pre_gate("export const x = 1;\n", &opts), None);
        for (source, sx) in [
            ("import * as s from '@stylexjs/stylex';", false),
            ("import s from 'stylex';", false),
            ("import { color } from '@stylexjs/atoms';", false),
            ("const t = './tokens.stylex';", false),
            ("<div sx={x} />", true),
            ("const tsx = 1;", true),
            ("const s = '\\u0040stylexjs/stylex';", false),
            ("const s = '\\x40';", false),
        ] {
            assert_eq!(pre_gate(source, &opts), Some(sx), "{source}");
        }
    }

    #[test]
    fn pre_gate_keeps_non_default_needles_and_honours_sx_off() {
        let opts = options(serde_json::json!({
            "importSources": ["foo-bar", { "from": "my-stylex-lib", "as": "css" }],
            "sxPropName": false,
        }));
        assert_eq!(
            pre_gate("import { css } from 'foo-bar';", &opts),
            Some(false)
        );
        assert_eq!(
            pre_gate("import { css } from 'my-stylex-lib';", &opts),
            Some(false)
        );
        assert_eq!(pre_gate("<div sx={x} />", &opts), None);
        let custom_sx = options(serde_json::json!({ "sxPropName": "css" }));
        assert_eq!(pre_gate("<div css={x} />", &custom_sx), Some(true));
        assert_eq!(pre_gate("<div sx={x} />", &custom_sx), None);
    }
}
