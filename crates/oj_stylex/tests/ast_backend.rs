//! AST-backend tests: every replacement kind through both backends (reprints
//! must byte-match), plus span-invariant and sourcemap probes.

use std::path::{Path, PathBuf};

use oj_stylex::api::{
    CompileResult, FileContext, parse_program, transform_program, transform_source,
};
use oj_stylex::module_resolution::BoundedFs;
use oj_stylex::options::{CompilerOptions, ResolvedOptions};
use oj_stylex::transform::ast_backend::assert_spans_in_range;
use oxc_allocator::Allocator;
use oxc_codegen::{Codegen, CodegenOptions};
use oxc_parser::Parser;
use oxc_span::SourceType;

const IMP: &str = "import * as stylex from '@stylexjs/stylex';\n";

fn fixture_root() -> PathBuf {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ast-backend-fixture");
    ONCE.call_once(|| {
        let src = root.join("src");
        std::fs::create_dir_all(&src).expect("fixture dirs");
        std::fs::write(root.join("package.json"), "{ \"name\": \"fixture-pkg\" }\n").expect("pkg");
        std::fs::write(
            src.join("tokens.stylex.ts"),
            format!(
                "{IMP}export const colors = stylex.defineVars({{ accent: 'red' }});\n\
                 export const breakpoints = stylex.defineConsts({{ md: '@media (min-width: 768px)' }});\n"
            ),
        )
        .expect("tokens fixture");
    });
    root
}

fn options(json: serde_json::Value) -> ResolvedOptions {
    CompilerOptions::from_json(&json)
        .expect("options parse")
        .resolve()
        .expect("options resolve")
}

fn reprint(code: &str) -> String {
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, code, SourceType::tsx()).parse();
    assert!(
        !ret.fatal_error && !ret.diagnostics.has_errors(),
        "reprint parse failed for: {code}"
    );
    Codegen::new().build(&ret.program).code
}

struct AbOutput {
    ast_code: String,
    result: CompileResult,
}

/// Runs both backends and asserts the internal A/B invariants: identical
/// rules, reprint-identical code, in-range spans on the mutated program.
fn ab_with(source: &str, filename: &str, opts: &ResolvedOptions) -> AbOutput {
    let root = fixture_root();
    let fs = BoundedFs { root: root.clone() };
    let abs = root.join(filename);
    let ctx = FileContext {
        filename: Path::new(&abs),
        source_text: source,
        cwd: &root,
    };

    let splice = transform_source(&ctx, opts, &fs)
        .expect("splice compile")
        .expect("pre-gate must pass");

    let allocator = Allocator::default();
    let mut program = parse_program(&allocator, source, filename).expect("parse");
    let result = transform_program(&allocator, &mut program, &ctx, opts, &fs).expect("ast compile");
    assert_spans_in_range(&program, source.len() as u32)
        .unwrap_or_else(|bad| panic!("out-of-range spans: {bad}"));
    let ast_code = Codegen::new().build(&program).code;

    assert_eq!(result.modified, splice.modified, "modified flags diverge");
    let splice_rules: Vec<_> = splice.rules.iter().map(|r| r.to_metadata_tuple()).collect();
    let ast_rules: Vec<_> = result.rules.iter().map(|r| r.to_metadata_tuple()).collect();
    assert_eq!(splice_rules, ast_rules, "rule metadata diverges");

    let splice_reprint = reprint(&splice.code);
    let ast_reprint = reprint(&ast_code);
    assert_eq!(
        splice_reprint, ast_reprint,
        "reprint divergence\n-- splice --\n{}\n-- ast --\n{}",
        splice.code, ast_code
    );

    AbOutput { ast_code, result }
}

fn ab(source: &str) -> AbOutput {
    ab_with(source, "src/input.tsx", &options(serde_json::json!({})))
}

// ---------------------------------------------------------------------------
// Replacement kinds

#[test]
fn create_static_object() {
    let out = ab(&format!(
        "{IMP}export const styles = stylex.create({{ root: {{ color: 'blue', margin: 4 }}, \"data-x\": {{ padding: 0 }} }});\n"
    ));
    assert!(
        out.ast_code.contains("$$css: true"),
        "code: {}",
        out.ast_code
    );
    assert_eq!(out.result.create_objects.len(), 1);
}

#[test]
fn create_value_shapes_round_trip() {
    // arrays (indexed objects), null, negative + fractional numbers, escapes.
    ab(&format!(
        "{IMP}export const styles = stylex.create({{\n\
           a: {{ fontFamily: ['Arial', 'sans-serif'], zIndex: -4, opacity: 0.5 }},\n\
           b: {{ content: 'a\"b', color: null }},\n\
         }});\n"
    ));
}

#[test]
fn create_dynamic_arrow_with_hoisted_static_chunk() {
    let out = ab(&format!(
        "{IMP}export const styles = stylex.create({{\n\
           root: (width) => ({{ color: 'blue', width }}),\n\
         }});\n"
    ));
    assert!(
        out.ast_code.contains("const _temp"),
        "hoisted chunk missing: {}",
        out.ast_code
    );
    assert!(
        out.ast_code.contains("=>"),
        "dynamic arrow missing: {}",
        out.ast_code
    );
}

#[test]
fn create_dynamic_arrow_with_unit_wrapper_and_conditional_chunk() {
    ab(&format!(
        "{IMP}export const styles = stylex.create({{\n\
           root: (w, h) => ({{ width: w, marginTop: h, color: w ? 'red' : 'blue' }}),\n\
         }});\n"
    ));
}

#[test]
fn create_non_program_level_hoists_a_uid() {
    let out = ab(&format!(
        "{IMP}export function make() {{ return stylex.create({{ a: {{ color: 'red' }} }}); }}\n"
    ));
    assert!(
        out.ast_code.contains("const _styles"),
        "hoist missing: {}",
        out.ast_code
    );
}

#[test]
fn keyframes_replacement() {
    ab(&format!(
        "{IMP}const fade = stylex.keyframes({{ from: {{ opacity: 0 }}, to: {{ opacity: 1 }} }});\n\
         export const styles = stylex.create({{ a: {{ animationName: fade }} }});\n"
    ));
}

#[test]
fn position_try_and_view_transition_class() {
    ab(&format!(
        "{IMP}export const fallback = stylex.positionTry({{ top: '0' }});\n"
    ));
    ab(&format!(
        "{IMP}export const vt = stylex.viewTransitionClass({{ group: {{ transitionProperty: 'none' }} }});\n"
    ));
}

#[test]
fn define_vars_and_consts_and_theme() {
    let opts = options(serde_json::json!({
        "unstable_moduleResolution": { "type": "commonJS", "rootDir": fixture_root() },
    }));
    ab_with(
        &format!("{IMP}export const vars = stylex.defineVars({{ accent: 'red' }});\n"),
        "src/tokens2.stylex.ts",
        &opts,
    );
    ab_with(
        &format!(
            "{IMP}export const consts = stylex.defineConsts({{ md: '@media (min-width: 768px)' }});\n"
        ),
        "src/consts2.stylex.ts",
        &opts,
    );
    ab_with(
        &format!(
            "{IMP}import {{ colors }} from './tokens.stylex';\n\
             export const theme = stylex.createTheme(colors, {{ accent: 'blue' }});\n"
        ),
        "src/theme.ts",
        &opts,
    );
}

#[test]
fn markers() {
    let opts = options(serde_json::json!({
        "unstable_moduleResolution": { "type": "commonJS", "rootDir": fixture_root() },
    }));
    ab_with(
        &format!(
            "{IMP}export const marker = stylex.defineMarker();\n\
             const dm = stylex.defaultMarker();\n\
             export const styles = stylex.create({{ a: {{ color: 'red' }} }});\n\
             export const probe = [dm];\n"
        ),
        "src/markers.stylex.ts",
        &opts,
    );
}

#[test]
fn props_inline_merge_and_table_and_bail() {
    // inline
    ab(&format!(
        "{IMP}const styles = stylex.create({{ a: {{ color: 'red' }}, b: {{ margin: 0 }} }});\n\
         export const out = stylex.props(styles.a, styles.b);\n"
    ));
    // table (one conditional)
    ab(&format!(
        "{IMP}const styles = stylex.create({{ a: {{ color: 'red' }}, b: {{ margin: 0 }} }});\n\
         export const f = (on) => stylex.props(styles.a, on && styles.b);\n"
    ));
    // table (ternary + two conditionals)
    ab(&format!(
        "{IMP}const styles = stylex.create({{ a: {{ color: 'red' }}, b: {{ margin: 0 }}, c: {{ padding: 0 }} }});\n\
         export const f = (x, y) => stylex.props(x ? styles.a : styles.b, y && styles.c);\n"
    ));
    // bail (non-static member) keeps the runtime call and DCE keeps used parts
    ab(&format!(
        "{IMP}const styles = stylex.create({{ a: {{ color: 'red' }}, b: {{ margin: 0, padding: null }} }});\n\
         export const f = (k) => stylex.props(styles[k]);\n"
    ));
}

#[test]
fn attrs_inline_merge() {
    ab(&format!(
        "{IMP}const styles = stylex.create({{ a: {{ color: 'red' }} }});\n\
         export const out = stylex.attrs(styles.a);\n"
    ));
}

#[test]
fn jsx_spread_props_inline_becomes_attributes() {
    ab(&format!(
        "{IMP}const styles = stylex.create({{ a: {{ color: 'red' }} }});\n\
         export const App = () => <div {{...stylex.props(styles.a)}} />;\n"
    ));
}

#[test]
fn sx_inline_table_and_bail() {
    // inline
    ab(&format!(
        "{IMP}const styles = stylex.create({{ a: {{ color: 'blue' }} }});\n\
         export const App = () => <div sx={{styles.a}} />;\n"
    ));
    // empty inline
    ab(&format!(
        "{IMP}export const App = () => <div sx={{null}} />;\n"
    ));
    // table
    ab(&format!(
        "{IMP}const styles = stylex.create({{ a: {{ color: 'blue' }}, b: {{ margin: 0 }} }});\n\
         export const App = (on) => <div sx={{[styles.a, on && styles.b]}} />;\n"
    ));
    // bail generates the runtime import + props call
    ab(&format!(
        "{IMP}export const App = (dyn) => <div sx={{dyn}} />;\n"
    ));
}

#[test]
fn sx_bail_without_source_import_unshifts_a_generated_import() {
    // No stylex import at all: the runtime import is generated and unshifted.
    ab("// uses stylex via the sx prop\nexport function C(x) { return <div sx={x} />; }\n");
    // Shadowed name forces a collision-free uid.
    ab(&format!(
        "{IMP}export function C(stylex, x) {{ return <div sx={{x}} />; }}\n"
    ));
}

#[test]
fn sx_import_lands_after_hashbang_and_directives() {
    ab(
        "#!/usr/bin/env node\n'use strict';\n// stylex sx consumer\nexport function C(x) { return <div sx={x} />; }\n",
    );
}

#[test]
fn dce_removes_unused_creates_and_prunes_namespaces() {
    // whole statement removed
    ab(&format!(
        "{IMP}const styles = stylex.create({{ a: {{ color: 'red' }} }});\n\
         export const keep = 1;\n"
    ));
    // one of two declarators removed
    ab(&format!(
        "{IMP}const styles = stylex.create({{ a: {{ color: 'red' }} }}), other = 2;\n\
         export const keep = other;\n"
    ));
    // namespace pruning with kept null props
    ab(&format!(
        "{IMP}const styles = stylex.create({{ a: {{ color: 'red' }}, b: {{ margin: 0 }} }});\n\
         export const f = () => stylex.props(styles.a);\n"
    ));
}

#[test]
fn dce_keeps_hoisted_chunks_when_the_declarator_dies() {
    ab(&format!(
        "{IMP}const styles = stylex.create({{ root: (w) => ({{ width: w, color: 'red' }}) }});\n\
         export const keep = 1;\n"
    ));
}

#[test]
fn treeshake_compensation_inserts_a_side_effect_import() {
    let opts = options(serde_json::json!({
        "treeshakeCompensation": true,
        "unstable_moduleResolution": { "type": "commonJS", "rootDir": fixture_root() },
    }));
    ab_with(
        &format!(
            "{IMP}import {{ colors }} from './tokens.stylex';\n\
             export const styles = stylex.create({{ a: {{ color: colors.accent }} }});\n"
        ),
        "src/consumer.ts",
        &opts,
    );
}

#[test]
fn comments_around_replacements_survive_like_the_splice_output() {
    ab(&format!(
        "{IMP}// leading comment\nexport const styles = stylex.create({{ a: {{ color: 'red' }} }});\n// trailing comment\nexport const keep = 1;\n"
    ));
    // comment above a DCE-removed statement re-attaches to the next statement
    ab(&format!(
        "{IMP}// vanishing anchor\nconst styles = stylex.create({{ a: {{ color: 'red' }} }});\nexport const keep = 1;\n"
    ));
}

#[test]
fn untouched_files_report_unmodified_and_leave_the_program_alone() {
    let root = fixture_root();
    let fs = BoundedFs { root: root.clone() };
    let source = "export const nothing = 1;\n";
    let abs = root.join("src/plain.ts");
    let ctx = FileContext {
        filename: Path::new(&abs),
        source_text: source,
        cwd: &root,
    };
    let allocator = Allocator::default();
    let mut program = parse_program(&allocator, source, "src/plain.ts").expect("parse");
    let opts = options(serde_json::json!({}));
    let result = transform_program(&allocator, &mut program, &ctx, &opts, &fs).expect("compile");
    assert!(!result.modified);
    assert_eq!(Codegen::new().build(&program).code, reprint(source));
}

#[test]
fn errors_match_the_splice_backend() {
    let root = fixture_root();
    let fs = BoundedFs { root: root.clone() };
    let source = format!("{IMP}stylex.create({{ a: {{ color: 'red' }} }});\n");
    let abs = root.join("src/err.ts");
    let ctx = FileContext {
        filename: Path::new(&abs),
        source_text: &source,
        cwd: &root,
    };
    let opts = options(serde_json::json!({}));
    let splice_err = transform_source(&ctx, &opts, &fs).expect_err("must error");
    let allocator = Allocator::default();
    let mut program = parse_program(&allocator, &source, "src/err.ts").expect("parse");
    let ast_err =
        transform_program(&allocator, &mut program, &ctx, &opts, &fs).expect_err("must error");
    assert_eq!(splice_err, ast_err);
}

// ---------------------------------------------------------------------------
// Synthesized-span invariants + sourcemap probe

#[test]
fn synthesized_nodes_carry_the_replaced_call_span() {
    use oxc_ast::ast::{Declaration, Expression, Statement};
    let root = fixture_root();
    let fs = BoundedFs { root: root.clone() };
    let source =
        format!("{IMP}export const styles = stylex.create({{ a: {{ color: 'red' }} }});\n");
    let call_start = source.find("stylex.create").expect("call") as u32;
    let call_end = source.rfind("})").expect("call end") as u32 + 2;
    let abs = root.join("src/span.ts");
    let ctx = FileContext {
        filename: Path::new(&abs),
        source_text: &source,
        cwd: &root,
    };
    let allocator = Allocator::default();
    let mut program = parse_program(&allocator, &source, "src/span.ts").expect("parse");
    let opts = options(serde_json::json!({}));
    transform_program(&allocator, &mut program, &ctx, &opts, &fs).expect("compile");
    let Some(Statement::ExportDeclaration(export)) = program.body.get(1) else {
        panic!("expected export statement");
    };
    let Declaration::VariableDeclaration(decl) = &export.declaration else {
        panic!("expected variable declaration");
    };
    let init = decl.declarations[0].init.as_ref().expect("init");
    let Expression::ParenthesizedExpression(paren) = init else {
        panic!("expected parenthesized compiled object, got {init:?}");
    };
    assert_eq!(paren.span.start, call_start);
    assert_eq!(paren.span.end, call_end);
    let Expression::ObjectExpression(object) = &paren.expression else {
        panic!("expected object");
    };
    assert_eq!(object.span.start, call_start);
    assert_eq!(object.span.end, call_end);
}

fn sourcemap_probe(source: &str, filename: &str, opts: &ResolvedOptions) {
    // Cases whose create call survives DCE must map back to its line; removed
    // declarations map nowhere, exactly like the splice/babel output.
    let expect_create_mapping = matches!(
        filename,
        "src/m1.ts" | "src/m2.ts" | "src/m6.ts" | "src/m8.ts" | "src/m9.ts"
    );
    let root = fixture_root();
    let fs = BoundedFs { root: root.clone() };
    let abs = root.join(filename);
    let ctx = FileContext {
        filename: Path::new(&abs),
        source_text: source,
        cwd: &root,
    };
    let allocator = Allocator::default();
    let mut program = parse_program(&allocator, source, filename).expect("parse");
    transform_program(&allocator, &mut program, &ctx, opts, &fs).expect("compile");
    let ret = Codegen::new()
        .with_options(CodegenOptions {
            source_map_path: Some(PathBuf::from(filename)),
            ..CodegenOptions::default()
        })
        .build(&program);
    let map = ret.map.expect("sourcemap built");
    let src_lines = source.lines().count() as u32 + 1;
    let mut tokens = 0usize;
    for token in map.get_tokens() {
        assert!(
            token.get_src_line() < src_lines,
            "mapping out of range: line {} of {src_lines} in {filename}",
            token.get_src_line()
        );
        tokens += 1;
    }
    assert!(tokens > 0, "no mappings produced for {filename}");

    // The stylex-replaced region maps back to the original call position:
    // some mapping points at the create call's (0-based) source line.
    if let Some(call_offset) = source.find("stylex.create(")
        && expect_create_mapping
    {
        let call_line = source[..call_offset].matches('\n').count() as u32;
        let hit = map
            .get_tokens()
            .any(|token| token.get_src_line() == call_line);
        assert!(
            hit,
            "no mapping back to the create call's line in {filename}"
        );
    }
}

#[test]
fn sourcemaps_build_over_ast_transformed_programs() {
    let default_opts = options(serde_json::json!({}));
    let resolution_opts = options(serde_json::json!({
        "treeshakeCompensation": true,
        "unstable_moduleResolution": { "type": "commonJS", "rootDir": fixture_root() },
    }));
    let cases: Vec<(String, &str, &ResolvedOptions)> = vec![
        (
            format!("{IMP}export const styles = stylex.create({{ a: {{ color: 'red' }} }});\n"),
            "src/m1.ts",
            &default_opts,
        ),
        (
            format!(
                "{IMP}export const styles = stylex.create({{ root: (w) => ({{ width: w, color: 'red' }}) }});\n"
            ),
            "src/m2.ts",
            &default_opts,
        ),
        (
            format!(
                "{IMP}const styles = stylex.create({{ a: {{ color: 'red' }}, b: {{ margin: 0 }} }});\n\
                 export const f = (on) => stylex.props(styles.a, on && styles.b);\n"
            ),
            "src/m3.ts",
            &default_opts,
        ),
        (
            format!(
                "{IMP}const styles = stylex.create({{ a: {{ color: 'blue' }} }});\n\
                 export const App = () => <div sx={{styles.a}} />;\n"
            ),
            "src/m4.tsx",
            &default_opts,
        ),
        (
            "// stylex sx consumer\nexport function C(x) { return <div sx={x} />; }\n".to_string(),
            "src/m5.tsx",
            &default_opts,
        ),
        (
            format!(
                "{IMP}const fade = stylex.keyframes({{ from: {{ opacity: 0 }} }});\n\
                 export const styles = stylex.create({{ a: {{ animationName: fade }} }});\n"
            ),
            "src/m6.ts",
            &default_opts,
        ),
        (
            format!(
                "{IMP}const styles = stylex.create({{ a: {{ color: 'red' }} }});\nexport const keep = 1;\n"
            ),
            "src/m7.ts",
            &default_opts,
        ),
        (
            format!(
                "{IMP}export function make() {{ return stylex.create({{ a: {{ color: 'red' }} }}); }}\n"
            ),
            "src/m8.ts",
            &default_opts,
        ),
        (
            format!(
                "{IMP}import {{ colors }} from './tokens.stylex';\n\
                 export const styles = stylex.create({{ a: {{ color: colors.accent }} }});\n"
            ),
            "src/m9.ts",
            &resolution_opts,
        ),
        (
            format!(
                "{IMP}export const App = (dyn) => <div {{...stylex.props(dyn ? {{}} : null)}} />;\n"
            ),
            "src/m10.tsx",
            &default_opts,
        ),
    ];
    for (source, filename, opts) in &cases {
        sourcemap_probe(source, filename, opts);
    }
}
