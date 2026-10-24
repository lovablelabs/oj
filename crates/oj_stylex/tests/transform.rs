//! End-to-end tests for the transform integrator: transform_source over inline
//! modules (visitor order, splice output, DCE, props merge, CLI-facing shapes).

use std::path::{Path, PathBuf};

use oj_stylex::api::{FileContext, might_contain_stylex, transform_source};
use oj_stylex::errors::ErrorCode;
use oj_stylex::module_resolution::BoundedFs;
use oj_stylex::options::{CompilerOptions, ResolvedOptions};

const IMP: &str = "import * as stylex from '@stylexjs/stylex';\n";

fn fixture_root() -> PathBuf {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("transform-fixture");
    // Tests run in parallel; a concurrent rewrite of package.json races the
    // package-walk reads, so the fixture is materialized exactly once.
    ONCE.call_once(|| {
        let src = root.join("src");
        std::fs::create_dir_all(&src).expect("fixture dirs");
        std::fs::write(root.join("package.json"), "{ \"name\": \"fixture-pkg\" }\n").expect("pkg");
        std::fs::write(
            src.join("tokens.stylex.ts"),
            format!(
                "{IMP}export const breakpoints = stylex.defineConsts({{ md: '@media (min-width: 768px)' }});\n"
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

fn compile_with(
    source: &str,
    filename: &str,
    opts: &ResolvedOptions,
) -> Result<oj_stylex::api::SourceCompileResult, oj_stylex::errors::StylexError> {
    let root = fixture_root();
    let fs = BoundedFs { root: root.clone() };
    let filename = root.join(filename);
    let ctx = FileContext {
        filename: Path::new(&filename),
        source_text: source,
        cwd: &root,
    };
    transform_source(&ctx, opts, &fs).map(|r| r.expect("pre-gate must pass for stylex sources"))
}

fn compile(source: &str) -> oj_stylex::api::SourceCompileResult {
    compile_with(source, "src/input.ts", &options(serde_json::json!({}))).expect("compile")
}

fn compile_err(source: &str) -> oj_stylex::errors::StylexError {
    compile_with(source, "src/input.ts", &options(serde_json::json!({}))).expect_err("must error")
}

#[test]
fn exported_create_is_replaced_and_rules_collected() {
    let out = compile(&format!(
        "{IMP}export const styles = stylex.create({{ root: {{ color: 'blue' }} }});\n"
    ));
    assert!(out.modified);
    assert!(
        out.code
            .contains("export const styles = ({root: {kMwMTN: \"xju2f9n\", $$css: true}});"),
        "code was: {}",
        out.code
    );
    assert_eq!(out.rules.len(), 1);
    assert_eq!(&*out.rules[0].ltr, ".xju2f9n{color:blue}");
    assert_eq!(out.create_objects.len(), 1);
    assert_eq!(out.create_objects[0].0.as_deref(), Some("styles"));
}

#[test]
fn unused_unexported_create_is_removed_but_rules_stay() {
    let out = compile(&format!(
        "{IMP}const styles = stylex.create({{ root: {{ color: 'blue' }} }});\nexport const other = 1;\n"
    ));
    assert!(!out.code.contains("styles"), "code was: {}", out.code);
    assert_eq!(out.rules.len(), 1);
}

#[test]
fn export_specifier_keeps_the_var_alive() {
    let out = compile(&format!(
        "{IMP}const styles = stylex.create({{ root: {{ color: 'blue' }} }});\nexport {{ styles }};\n"
    ));
    assert!(out.code.contains("kMwMTN"), "code was: {}", out.code);
}

#[test]
fn props_inline_merge_replaces_the_call() {
    let out = compile(&format!(
        "{IMP}const styles = stylex.create({{ a: {{ color: 'blue' }}, b: {{ color: 'red' }} }});\n\
         export const merged = stylex.props(styles.a, styles.b);\n"
    ));
    // Last wins: only b's color class survives the merge.
    assert!(
        out.code
            .contains("export const merged = ({className: \"x1e2nbdu\"});"),
        "code was: {}",
        out.code
    );
    // Both namespaces were only used by the inlined call: declarator removed.
    assert!(!out.code.contains("const styles"), "code was: {}", out.code);
}

#[test]
fn props_conditional_builds_the_bitmask_table() {
    let out = compile(&format!(
        "{IMP}const styles = stylex.create({{ a: {{ color: 'blue' }}, b: {{ color: 'red' }} }});\n\
         export const f = (on, hot) => stylex.props(on ? styles.a : null, hot && styles.b);\n"
    ));
    let code = &out.code;
    assert!(
        code.contains("[!!(on) << 1 | !!(hot) << 0]"),
        "code: {code}"
    );
    assert!(code.contains("0: {}"), "code: {code}");
    assert!(
        code.contains("3: {className: \"x1e2nbdu\"}"),
        "code: {code}"
    );
}

#[test]
fn props_bail_keeps_call_and_null_prop_retention() {
    let out = compile(&format!(
        "{IMP}const styles = stylex.create({{ a: {{ color: 'blue', margin: null }} }});\n\
         export const f = (ext) => stylex.props(styles.a, ext);\n"
    ));
    let code = &out.code;
    assert!(code.contains("stylex.props(styles.a, ext)"), "code: {code}");
    // Bail collector saw a's props before ext: the null prop may be pruned.
    assert!(code.contains("kMwMTN"), "code: {code}");
    assert!(!code.contains("kogj98: null"), "code: {code}");
}

#[test]
fn props_bail_after_unknown_arg_keeps_null_props() {
    let out = compile(&format!(
        "{IMP}const styles = stylex.create({{ a: {{ color: 'blue', margin: null }} }});\n\
         export const f = (ext) => stylex.props(ext, styles.a);\n"
    ));
    // The member sits past the bail index: everything is kept, nulls included.
    assert!(out.code.contains("kogj98: null"), "code: {}", out.code);
}

#[test]
fn keyframes_declarator_compiles_before_create() {
    let out = compile(&format!(
        "{IMP}const fade = stylex.keyframes({{ from: {{ opacity: 0 }}, to: {{ opacity: 1 }} }});\n\
         export const styles = stylex.create({{ a: {{ animationName: fade }} }});\n"
    ));
    assert!(
        out.code.contains("const fade = \"x18re5ia-B\";"),
        "code: {}",
        out.code
    );
    assert!(out.rules[0].ltr.starts_with("@keyframes x18re5ia-B{"));
    assert_eq!(out.rules.len(), 2);
}

#[test]
fn define_consts_requires_named_export_and_theme_filename() {
    let opts = options(serde_json::json!({
        "unstable_moduleResolution": { "type": "commonJS", "rootDir": "<ROOT>" }
    }));
    let err = compile_with(
        &format!("{IMP}const c = stylex.defineConsts({{ a: '1px' }});\n"),
        "src/tokens2.stylex.ts",
        &opts,
    )
    .expect_err("non-export must fail");
    assert_eq!(err.code, ErrorCode::NonExportNamedDeclaration);

    let err = compile_with(
        &format!("{IMP}export const c = stylex.defineConsts({{ a: '1px' }});\n"),
        "src/plain.ts",
        &opts,
    )
    .expect_err("non-theme filename must fail");
    assert_eq!(err.code, ErrorCode::CannotGenerateHash);

    let out = compile_with(
        &format!("{IMP}export const c = stylex.defineConsts({{ a: '1px' }});\n"),
        "src/tokens2.stylex.ts",
        &opts,
    )
    .expect("theme file compiles");
    assert!(out.code.contains("({a: \"1px\"})"), "code: {}", out.code);
    assert_eq!(out.rules.len(), 1);
    assert_eq!(
        out.rules[0].const_val.as_deref(),
        Some(&serde_json::json!("1px"))
    );
    assert!(out.rules[0].const_key.is_some());
}

#[test]
fn markers_replace_in_place() {
    let out = compile(&format!(
        "{IMP}export const probe = stylex.defaultMarker();\n"
    ));
    assert!(
        out.code
            .contains("({\"x-default-marker\": \"x-default-marker\", $$css: true})"),
        "code: {}",
        out.code
    );
    assert!(out.rules.is_empty());
}

#[test]
fn create_validation_errors_match_upstream_text() {
    let err = compile_err(&format!("{IMP}stylex.create({{}});\n"));
    assert_eq!(
        err.message,
        "create() calls must be bound to a bare variable."
    );
    let err = compile_err(&format!("{IMP}export const s = stylex.create({{}}, 2);\n"));
    assert_eq!(err.message, "create() should have 1 argument.");
    let err = compile_err(&format!(
        "{IMP}const m = {{}};\nexport const s = stylex.create({{ ...m }});\n"
    ));
    assert_eq!(
        err.message,
        "Object spreads are not allowed in create() calls."
    );
}

#[test]
fn shared_binding_references_keep_object_identity() {
    let out = compile(&format!(
        "{IMP}const shared = {{ a: 1 }};\nexport const s = stylex.create({{ x: {{ color: shared === shared ? 'red' : 'blue', width: shared === {{ a: 1 }} ? 1 : 2 }} }});\n"
    ));
    let ltr: Vec<&str> = out.rules.iter().map(|r| &*r.ltr).collect();
    assert!(
        ltr.iter().any(|r| r.ends_with("{color:red}")),
        "rules: {ltr:?}"
    );
    assert!(
        ltr.iter().any(|r| r.ends_with("{width:2px}")),
        "rules: {ltr:?}"
    );
}

// Fresh threads (cold thread-local memos) racing on the same declarations must agree
// with a single-threaded compile and with each other through the process-wide memo level.
#[test]
fn declaration_memo_agrees_across_fresh_threads() {
    let source = format!(
        "{IMP}export const s = stylex.create({{ x: {{ paddingInlineStart: '13.37rem', ':is(.memo-probe)': {{ color: 'salmon' }} }} }});\n"
    );
    let expected = compile(&source).rules;
    assert_eq!(expected.len(), 2);
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let source = source.clone();
            std::thread::spawn(move || compile(&source).rules)
        })
        .collect();
    for thread in threads {
        assert_eq!(thread.join().expect("compile thread"), expected);
    }
}

const LEGACY_IMP: &str = "import stylex from '@stylexjs/stylex';\n";
const LEGACY_STYLES: &str = "const styles = stylex.create({ a: { color: 'blue' }, b: { color: 'red' }, e: { opacity: null } });\nconst s = styles.a;\n";

fn compile_legacy(tail: &str) -> String {
    compile(&format!("{LEGACY_IMP}{LEGACY_STYLES}{tail}")).code
}

#[test]
fn legacy_merge_inlines_a_bare_class_string() {
    let code = compile_legacy("export const x = stylex(styles.a, styles.b);\n");
    assert!(
        code.contains("export const x = \"x1e2nbdu\";"),
        "code: {code}"
    );
    // No args merges to the empty string, not to an empty object.
    let code = compile_legacy("export const x = stylex();\n");
    assert!(code.contains("export const x = \"\";"), "code: {code}");
}

#[test]
fn legacy_merge_conditionals_build_a_string_table() {
    let code = compile_legacy(
        "export const f = (on, hot) => stylex(on ? styles.a : null, hot && styles.b);\n",
    );
    assert!(
        code.contains("({0: \"\", 2: \"xju2f9n\", 1: \"x1e2nbdu\", 3: \"x1e2nbdu\"}"),
        "code: {code}"
    );
    assert!(
        code.contains("[!!(on) << 1 | !!(hot) << 0]"),
        "code: {code}"
    );
}

#[test]
fn legacy_merge_argument_classifier_is_narrower_than_props() {
    // Arrays, identifiers, object literals and calls all bail for legacy and
    // inline for props. parity: stylex-merge.js `switch (arg.type)`.
    for tail in [
        "export const x = stylex([styles.a, styles.b]);\n",
        "export const x = stylex(s);\n",
        "export const x = stylex({ color: 'red' });\n",
        "export const x = stylex(undefined);\n",
        "export const f = (on) => stylex(on ? s : null);\n",
        "export const f = (on) => stylex(on && { color: 'red' });\n",
    ] {
        let code = compile_legacy(tail);
        assert!(code.contains("stylex("), "expected a bail, code: {code}");
    }
    // Parenthesized members still inline: babel builds no paren node.
    let code = compile_legacy("export const x = stylex((styles.a));\n");
    assert!(
        code.contains("export const x = \"xju2f9n\";"),
        "code: {code}"
    );
}

#[test]
fn legacy_merge_bail_index_stops_at_the_first_bail() {
    // Legacy breaks on `s` (index 1); props keeps scanning and lands on the
    // non-`&&` logical at index 2, which prunes e's null prop.
    let legacy = compile_legacy("export const x = stylex(styles.a, s, flag || styles.e);\n");
    assert!(
        legacy.contains("e: {kSiTet: null, $$css: true}"),
        "{legacy}"
    );
    let props = compile_legacy("export const x = stylex.props(styles.a, s, flag || styles.e);\n");
    assert!(props.contains("e: {$$css: true}"), "{props}");
}

#[test]
fn legacy_merge_bail_does_not_hoist_css_objects() {
    let code = compile_legacy("export const x = stylex(other.q, { $$css: true, foo: 'bar' });\n");
    assert!(!code.contains("_temp"), "code: {code}");
    assert!(
        code.contains("stylex(other.q, { $$css: true, foo: 'bar' })"),
        "code: {code}"
    );
}

#[test]
fn legacy_merge_never_takes_the_jsx_spread_fast_path() {
    let code = compile_with(
        &format!(
            "{LEGACY_IMP}{LEGACY_STYLES}export const C = () => <div {{...stylex(styles.a)}} />;\n"
        ),
        "src/input.tsx",
        &options(serde_json::json!({})),
    )
    .expect("compiles")
    .code;
    assert!(code.contains("{...\"xju2f9n\"}"), "code: {code}");
}

#[test]
fn treeshake_compensation_inserts_a_side_effect_import() {
    let opts = options(serde_json::json!({
        "treeshakeCompensation": true,
        "unstable_moduleResolution": { "type": "commonJS", "rootDir": "<ROOT>" }
    }));
    let out = compile_with(
        &format!(
            "{IMP}import {{ breakpoints }} from './tokens.stylex';\n\
             export const styles = stylex.create({{ a: {{ color: {{ default: 'red', [breakpoints.md]: 'blue' }} }} }});\n"
        ),
        "src/input.ts",
        &opts,
    )
    .expect("compiles");
    // The compensation import reprints the source literal raw (quotes kept).
    assert!(
        out.code
            .contains("import './tokens.stylex';\nimport { breakpoints }"),
        "code: {}",
        out.code
    );
}

#[test]
fn treeshake_compensation_targets_the_exact_value_import() {
    let opts = options(serde_json::json!({
        "treeshakeCompensation": true,
        "unstable_moduleResolution": { "type": "commonJS", "rootDir": "<ROOT>" }
    }));
    // codex r3 finding 13: the type-only import and ./setup must stay ahead.
    let out = compile_with(
        "import type { Breakpoints } from './tokens.stylex';\n\
         import './setup';\n\
         import { breakpoints } from './tokens.stylex';\n\
         import * as stylex from '@stylexjs/stylex';\n\
         export const styles = stylex.create({ a: { color: { default: 'red', [breakpoints.md]: 'blue' } } });\n",
        "src/input.ts",
        &opts,
    )
    .expect("compiles");
    assert!(
        out.code.starts_with(
            "import type { Breakpoints } from './tokens.stylex';\n\
             import './setup';\n\
             import './tokens.stylex';\n\
             import { breakpoints } from './tokens.stylex';\n"
        ),
        "code: {}",
        out.code
    );
}

#[test]
fn nested_create_results_hoist_to_module_consts() {
    // codex r3 finding 6 — oracle-pinned output shapes (probe 2026-08-28).
    let out = compile(&format!(
        "{IMP}const _styles = 1;\n\
         export function f() {{ return stylex.create({{ a: {{ color: 'red' }} }}); }}\n\
         export const keep = _styles;\n"
    ));
    assert!(
        out.code.contains(
            "const _styles2 = ({a: {kMwMTN: \"x1e2nbdu\", $$css: true}});\nexport function f() { return _styles2; }"
        ),
        "code: {}",
        out.code
    );
    // Program-level sites (class fields included) stay inline.
    let inline = compile(&format!(
        "{IMP}class C {{ styles = stylex.create({{ a: {{ color: 'red' }} }}); }}\nexport const c = new C();\n"
    ));
    assert!(
        inline
            .code
            .contains("styles = ({a: {kMwMTN: \"x1e2nbdu\", $$css: true}});"),
        "code: {}",
        inline.code
    );
}

#[test]
fn sx_prop_inlines_on_lowercase_elements() {
    let out = compile_with(
        &format!(
            "{IMP}const styles = stylex.create({{ a: {{ color: 'blue' }} }});\n\
             export const App = () => <div sx={{styles.a}} />;\n"
        ),
        "src/input.tsx",
        &options(serde_json::json!({})),
    )
    .expect("compiles");
    assert!(
        out.code.contains("<div className=\"xju2f9n\" />"),
        "code: {}",
        out.code
    );
}

#[test]
fn sx_prop_bails_to_a_runtime_props_call() {
    let out = compile_with(
        &format!(
            "{IMP}export const App = (dyn) => <div sx={{dyn}} />;\n\
             export const styles = stylex.create({{ a: {{ color: 'blue' }} }});\n"
        ),
        "src/input.tsx",
        &options(serde_json::json!({})),
    )
    .expect("compiles");
    assert!(
        out.code.contains("<div {...stylex.props(dyn)} />"),
        "code: {}",
        out.code
    );
}

#[test]
fn pre_gate_skips_stylex_free_sources() {
    let opts = options(serde_json::json!({}));
    assert!(!might_contain_stylex("export const x = 1;\n", &opts));
    let root = fixture_root();
    let fs = BoundedFs { root: root.clone() };
    let ctx = FileContext {
        filename: &root.join("src/plain.ts"),
        source_text: "export const x = 1;\n",
        cwd: &root,
    };
    assert!(transform_source(&ctx, &opts, &fs).expect("ok").is_none());
}

#[test]
fn dev_mode_emits_debug_and_dev_class_names() {
    let out = compile_with(
        &format!("{IMP}export const styles = stylex.create({{ a: {{ color: 'blue' }} }});\n"),
        "src/input.ts",
        &options(serde_json::json!({ "dev": true })),
    )
    .expect("compiles");
    assert!(
        out.code
            .contains("\"input__styles.a\": \"input__styles.a\""),
        "code: {}",
        out.code
    );
    assert!(
        out.code.contains("$$css: \"fixture-pkg:src/input.ts:2\""),
        "code: {}",
        out.code
    );
}

// ---------------------------------------------------------------------------
// r3 findings 1 + 3: binding identity (oracle outputs probed live, 2026-08-28)

#[test]
fn function_local_shadow_compiles_the_inner_binding() {
    let out = compile(&format!(
        "{IMP}const color = \"red\";\n\
         export function f() {{\n  const color = \"blue\";\n  return stylex.create({{a: {{color}}}});\n}}\n"
    ));
    let ltrs: Vec<&str> = out.rules.iter().map(|r| r.ltr.as_ref()).collect();
    assert_eq!(ltrs, vec![".xju2f9n{color:blue}"], "code: {}", out.code);
}

#[test]
fn block_scope_shadow_compiles_the_inner_binding() {
    let out = compile(&format!(
        "{IMP}const color = \"red\";\n\
         export function f() {{\n  if (true) {{\n    const color = \"green\";\n    return stylex.create({{a: {{color}}}});\n  }}\n}}\n"
    ));
    let ltrs: Vec<&str> = out.rules.iter().map(|r| r.ltr.as_ref()).collect();
    assert_eq!(ltrs, vec![".x1prwzq3{color:green}"]);
}

#[test]
fn scope_shadow_error_reasons_match_babel() {
    // Each reason probed against @stylexjs/babel-plugin 0.19.0 live.
    let param = compile_err(&format!(
        "{IMP}const color = \"red\";\nexport function f(color) {{ return stylex.create({{a: {{color}}}}); }}\n"
    ));
    assert!(
        param
            .message
            .starts_with("Unsupported expression: Identifier"),
        "param: {}",
        param.message
    );
    let caught = compile_err(&format!(
        "{IMP}const color = \"red\";\n\
         export function f() {{ try {{ g(); }} catch (color) {{ return stylex.create({{a: {{color}}}}); }} }}\n"
    ));
    assert!(
        caught
            .message
            .starts_with("Referenced value is used before declaration."),
        "catch: {}",
        caught.message
    );
    let hoisted = compile_err(&format!(
        "{IMP}export function f() {{\n  function color() {{}}\n  return stylex.create({{a: {{color}}}});\n}}\n"
    ));
    assert!(
        hoisted
            .message
            .starts_with("Unsupported expression: FunctionDeclaration"),
        "fndecl: {}",
        hoisted.message
    );
    let infinity = compile_err(&format!(
        "{IMP}export function f() {{\n  const Infinity = 5;\n  return stylex.create({{a: {{opacity: Infinity}}}});\n}}\n"
    ));
    assert!(
        infinity
            .message
            .starts_with("Referenced constant is not initialized."),
        "infinity: {}",
        infinity.message
    );
}

#[test]
fn same_named_mutations_in_other_scopes_stay_isolated() {
    let out = compile(&format!(
        "{IMP}const theme = {{color: \"red\"}};\n\
         export function g(theme) {{ theme.color = \"blue\"; }}\n\
         export function h() {{ let theme = {{}}; theme.color = \"green\"; }}\n\
         export const s = stylex.create({{a: {{color: theme.color}}}});\n"
    ));
    let ltrs: Vec<&str> = out.rules.iter().map(|r| r.ltr.as_ref()).collect();
    assert_eq!(ltrs, vec![".x1e2nbdu{color:red}"]);
}

#[test]
fn shadowed_stylex_callee_still_transforms_by_name() {
    // parity: upstream create detection is name-based (no binding check).
    let out = compile(&format!(
        "{IMP}export function f(stylex) {{\n  return stylex.create({{a: {{color: \"red\"}}}});\n}}\n"
    ));
    let ltrs: Vec<&str> = out.rules.iter().map(|r| r.ltr.as_ref()).collect();
    assert_eq!(ltrs, vec![".x1e2nbdu{color:red}"]);
}

#[test]
fn sx_shadowed_param_generates_a_collision_free_import() {
    let out = compile_with(
        &format!(
            "{IMP}export function C(stylex, dynamicStyle) {{\n  return <div sx={{dynamicStyle}} />;\n}}\n"
        ),
        "src/input.tsx",
        &options(serde_json::json!({})),
    )
    .expect("compiles");
    assert!(
        out.code
            .starts_with("import * as _stylex from \"@stylexjs/stylex\";"),
        "code: {}",
        out.code
    );
    assert!(
        out.code.contains("{..._stylex.props(dynamicStyle)}"),
        "code: {}",
        out.code
    );
}

#[test]
fn sx_sites_resolve_their_own_scopes() {
    let out = compile_with(
        &format!(
            "{IMP}export function A(x) {{ return <div sx={{x}} />; }}\n\
             export function B(stylex, x) {{ return <div sx={{x}} />; }}\n"
        ),
        "src/input.tsx",
        &options(serde_json::json!({})),
    )
    .expect("compiles");
    assert!(
        out.code.contains("{...stylex.props(x)}") && out.code.contains("{..._stylex.props(x)}"),
        "code: {}",
        out.code
    );
}

#[test]
fn sx_generated_uid_skips_taken_names() {
    let out = compile_with(
        &format!(
            "{IMP}const _stylex = 1;\nexport function C(stylex, x) {{ return <div sx={{x}} used={{_stylex}} />; }}\n"
        ),
        "src/input.tsx",
        &options(serde_json::json!({})),
    )
    .expect("compiles");
    assert!(
        out.code.contains("{..._stylex2.props(x)}"),
        "code: {}",
        out.code
    );
}

#[test]
fn sx_without_any_import_prefers_the_plain_stylex_name() {
    // The comment keeps the raw-text pre-gate open (babel has no gate).
    let plain = compile_with(
        "// uses stylex via the sx prop\nexport function C(x) { return <div sx={x} />; }\n",
        "src/input.tsx",
        &options(serde_json::json!({})),
    )
    .expect("compiles");
    // The generated import lands at the first body statement (after the
    // comment line the splice keeps in place).
    assert!(
        plain
            .code
            .contains("import * as stylex from \"@stylexjs/stylex\";\nexport function C"),
        "code: {}",
        plain.code
    );
    let taken = compile_with(
        "// uses stylex via the sx prop\nconst stylex = 1;\nexport function C(x) { return <div sx={x} used={stylex} />; }\n",
        "src/input.tsx",
        &options(serde_json::json!({})),
    )
    .expect("compiles");
    assert!(
        taken.code.contains("{..._stylex.props(x)}"),
        "code: {}",
        taken.code
    );
}

// ---------------------------------------------------------------------------
// r4 findings 1 + 9 + m14 (oracle outputs probed live, 2026-08-28)

#[test]
fn dynamic_static_chunks_hoist_to_shared_module_consts() {
    // r4 F1: styles.root(1)[0] === styles.root(2)[0] must hold like upstream.
    let out = compile(&format!(
        "{IMP}export const styles = stylex.create({{\n  a: w => ({{ color: 'red', width: w }}),\n  b: h => ({{ color: 'blue', height: h }}),\n}});\n"
    ));
    let code = &out.code;
    assert!(
        code.contains("const _temp = {kMwMTN: \"x1e2nbdu\", \"$$css\": true};\n"),
        "code: {code}"
    );
    assert!(
        code.contains("const _temp2 = {kMwMTN: \"xju2f9n\", \"$$css\": true};\n"),
        "code: {code}"
    );
    assert!(
        code.contains("a: (w) => [_temp, {") && code.contains("b: (h) => [_temp2, {"),
        "code: {code}"
    );
}

#[test]
fn dynamic_hoists_take_temp_uids_and_survive_dce() {
    let collision = compile(&format!(
        "{IMP}const _temp = 1;\nexport const styles = stylex.create({{ root: w => ({{ color: 'red', width: w }}) }});\nexport const keep = _temp;\n"
    ));
    assert!(
        collision.code.contains("const _temp2 = {kMwMTN"),
        "code: {}",
        collision.code
    );
    // The hoisted chunk stays behind when DCE removes the create declarator.
    let removed = compile(&format!(
        "{IMP}const styles = stylex.create({{ root: w => ({{ color: 'red', width: w }}) }});\nexport const other = 1;\n"
    ));
    assert!(
        removed
            .code
            .contains("const _temp = {kMwMTN: \"x1e2nbdu\", \"$$css\": true};\n"),
        "code: {}",
        removed.code
    );
    assert!(
        !removed.code.contains("const styles"),
        "code: {}",
        removed.code
    );
    // Same when only a static namespace of the create is consumed.
    let pruned = compile(&format!(
        "{IMP}const styles = stylex.create({{ root: w => ({{ color: 'red', width: w }}), other: {{ color: 'blue' }} }});\nexport const out = stylex.props(styles.other);\n"
    ));
    assert!(
        pruned.code.contains("const _temp = {kMwMTN")
            && pruned
                .code
                .contains("export const out = ({className: \"xju2f9n\"});"),
        "code: {}",
        pruned.code
    );
}

#[test]
fn dynamic_arrow_chunk_shapes_match_upstream() {
    // Static-props-only fn: the vars object is empty but still present.
    let static_only = compile(&format!(
        "{IMP}export const styles = stylex.create({{ root: w => ({{ color: 'red' }}) }});\n"
    ));
    assert!(
        static_only.code.contains("root: (w) => [_temp, {}]"),
        "code: {}",
        static_only.code
    );
    // No static chunk: nothing hoists.
    let no_static = compile(&format!(
        "{IMP}export const styles = stylex.create({{ root: w => ({{ width: w }}) }});\n"
    ));
    assert!(
        !no_static.code.contains("_temp"),
        "code: {}",
        no_static.code
    );
    // Function-local dynamic create: chunk hoist precedes the styles hoist.
    let local = compile(&format!(
        "{IMP}export function make() {{\n  const styles = stylex.create({{ root: w => ({{ color: 'red', width: w }}) }});\n  return styles;\n}}\n"
    ));
    assert!(
        local
            .code
            .contains("const _temp = {kMwMTN: \"x1e2nbdu\", \"$$css\": true};\nconst _styles = ({root: (w) => [_temp, {"),
        "code: {}",
        local.code
    );
}

#[test]
fn local_create_props_consumption_matches_upstream() {
    // r4 F3 repro: upstream leaves the props call as a runtime merge (the
    // hoisted binding is not re-resolved); pinned live 2026-08-28.
    let out = compile(&format!(
        "{IMP}export function f() {{\n  const styles = stylex.create({{ a: {{ color: 'red' }} }});\n  return stylex.props(styles.a);\n}}\n"
    ));
    assert!(
        out.code
            .contains("const _styles = ({a: {kMwMTN: \"x1e2nbdu\", $$css: true}});"),
        "code: {}",
        out.code
    );
    assert!(
        out.code
            .contains("const styles = _styles;\n  return stylex.props(styles.a);"),
        "code: {}",
        out.code
    );
}

#[test]
fn sx_generated_binding_reused_only_when_unshadowed() {
    // r4 F9: the cached generated import must not be captured by a param.
    let out = compile_with(
        "const A = x => <div sx={x} />;\nfunction B(stylex, x) {\n  return <div sx={x} />;\n}\nconst C = x => <div sx={x} />;\nfunction D(stylex, x) {\n  return <div sx={x} />;\n}\nexport { A, B, C, D };\n",
        "src/input.tsx",
        &options(serde_json::json!({})),
    )
    .expect("compiles");
    // Later generated imports unshift above earlier ones (babel parity).
    assert!(
        out.code.starts_with(
            "import * as _stylex from \"@stylexjs/stylex\";\nimport * as stylex from \"@stylexjs/stylex\";\n"
        ),
        "code: {}",
        out.code
    );
    let stylex_uses = out.code.matches("{...stylex.props(x)}").count();
    let underscore_uses = out.code.matches("{..._stylex.props(x)}").count();
    assert_eq!((stylex_uses, underscore_uses), (2, 2), "code: {}", out.code);
    assert_eq!(
        out.code.matches("import * as").count(),
        2,
        "code: {}",
        out.code
    );
}

#[test]
fn sx_import_survives_dce_of_a_first_statement_create() {
    // Imports hoist, so the create may precede its import; removing the dead
    // first statement must not swallow the generated import at the body start.
    let out = compile_with(
        "const styles = stylex.create({ a: { color: 'red' } });\nimport * as stylex from '@stylexjs/stylex';\nexport function B(stylex, x) { return <div sx={x} />; }\nexport const other = 1;\n",
        "src/input.tsx",
        &options(serde_json::json!({})),
    )
    .expect("compiles");
    assert!(
        out.code
            .starts_with("import * as _stylex from \"@stylexjs/stylex\";"),
        "code: {}",
        out.code
    );
    assert!(
        out.code.contains("{..._stylex.props(x)}") && !out.code.contains("const styles"),
        "code: {}",
        out.code
    );
}

#[test]
fn uid_surface_counts_unreferenced_declarations() {
    // r4 m14: babel registerBinding marks program.references for every
    // declaration, used or not.
    let out = compile(&format!(
        "{IMP}function sibling() {{ const _styles = 0; }}\nexport function f() {{\n  return stylex.create({{a:{{color:'red'}}}});\n}}\n"
    ));
    assert!(
        out.code.contains("const _styles2 = ({a:"),
        "code: {}",
        out.code
    );
}

#[test]
fn uid_candidate_sequence_matches_babel() {
    // r4 m14: after _styles9 babel tries _styles0, then _styles1, then _styles10.
    let taken: Vec<String> = (0..9)
        .map(|i| {
            if i == 0 {
                "_styles = 0".to_string()
            } else {
                format!("_styles{} = 0", i + 1)
            }
        })
        .collect();
    let out = compile(&format!(
        "{IMP}const {};\nexport function f() {{\n  return stylex.create({{a:{{color:'red'}}}});\n}}\n",
        taken.join(", ")
    ));
    assert!(
        out.code.contains("const _styles0 = ({a:"),
        "code: {}",
        out.code
    );
    let out2 = compile(&format!(
        "{IMP}const {}, _styles0 = 0;\nexport function f() {{\n  return stylex.create({{a:{{color:'red'}}}});\n}}\n",
        taken.join(", ")
    ));
    assert!(
        out2.code.contains("const _styles1 = ({a:"),
        "code: {}",
        out2.code
    );
    // Labels are not part of the collision surface.
    let label = compile(&format!(
        "{IMP}_styles: for (;;) {{ break _styles; }}\nexport function f() {{\n  return stylex.create({{a:{{color:'red'}}}});\n}}\n"
    ));
    assert!(
        label.code.contains("const _styles = ({a:"),
        "code: {}",
        label.code
    );
}

#[test]
fn sx_falls_back_to_the_first_unshadowed_stylex_local() {
    let out = compile_with(
        "import sx1 from '@stylexjs/stylex';\nimport * as sx2 from '@stylexjs/stylex';\n\
         export function C(sx1, x) { return <div sx={x} />; }\n",
        "src/input.tsx",
        &options(serde_json::json!({})),
    )
    .expect("compiles");
    assert!(
        out.code.contains("{...sx2.props(x)}") && !out.code.contains("import * as _stylex"),
        "code: {}",
        out.code
    );
}

#[test]
fn unverified_pseudo_collation_on_the_hash_path_hard_errors() {
    // Documented rust-rejects-more divergence (codex r5#2): the oracle emits
    // x1ua4pqi; a collation-unverified char feeding the hash refuses loudly.
    let err = compile_err(&format!(
        "{IMP}export const s = stylex.create({{a: {{'[data-🐱]': {{'[data-a]': {{color: 'red'}}}}}}}});\n"
    ));
    assert_eq!(err.code, ErrorCode::UnsupportedApi);
    assert!(err.to_string().contains("pinned collation alphabet"));
    // A lone unverified pseudo is never comparator-sorted: exact parity
    // (oracle-pinned xxd33zb, regressions/r5-pseudo-lone-unverified-009).
    let out = compile(&format!(
        "{IMP}export const s = stylex.create({{a: {{'[data-🐱]': {{color: 'red'}}}}}});\n"
    ));
    assert!(out.code.contains("xxd33zb"), "code: {}", out.code);
}
