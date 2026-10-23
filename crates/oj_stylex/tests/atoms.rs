//! `@stylexjs/atoms` end-to-end pins, oracle-pinned against
//! @stylexjs/babel-plugin 0.19.0 (conformance corpus/regressions/atoms).

use std::path::{Path, PathBuf};

use oj_stylex::api::{FileContext, SourceCompileResult, transform_source};
use oj_stylex::errors::StylexError;
use oj_stylex::module_resolution::BoundedFs;
use oj_stylex::options::{CompilerOptions, ResolvedOptions};

const ATOMS: &str = "import x from '@stylexjs/atoms';\n";
const STYLEX: &str = "import * as stylex from '@stylexjs/stylex';\n";

fn fixture_root() -> PathBuf {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("atoms-fixture");
    ONCE.call_once(|| {
        std::fs::create_dir_all(root.join("src")).expect("fixture dirs");
        std::fs::write(root.join("package.json"), "{ \"name\": \"fixture-pkg\" }\n").expect("pkg");
    });
    root
}

fn options(json: serde_json::Value) -> ResolvedOptions {
    CompilerOptions::from_json(&json)
        .expect("options parse")
        .resolve()
        .expect("options resolve")
}

fn run(
    source: &str,
    filename: &str,
    opts: &ResolvedOptions,
) -> Result<SourceCompileResult, StylexError> {
    let root = fixture_root();
    let fs = BoundedFs { root: root.clone() };
    let path = root.join(filename);
    let ctx = FileContext {
        filename: Path::new(&path),
        source_text: source,
        cwd: &root,
    };
    transform_source(&ctx, opts, &fs).map(|r| r.expect("pre-gate must pass for atoms sources"))
}

fn compile(source: &str) -> SourceCompileResult {
    run(source, "src/input.ts", &options(serde_json::json!({}))).expect("compile")
}

fn compile_opts(source: &str, json: serde_json::Value) -> SourceCompileResult {
    run(source, "src/input.ts", &options(json)).expect("compile")
}

fn compile_err(source: &str) -> StylexError {
    run(source, "src/input.ts", &options(serde_json::json!({}))).expect_err("must error")
}

fn rules(out: &SourceCompileResult) -> Vec<(String, String, f64)> {
    out.rules
        .iter()
        .map(|r| (r.class_name.to_string(), r.ltr.to_string(), r.priority))
        .collect()
}

#[test]
fn two_level_static_atom_compiles_like_create() {
    let out = compile(&format!("{ATOMS}export const a = x.display.flex;\n"));
    assert!(
        out.code
            .contains("export const a = ({k1xSpc: \"x78zum5\", $$css: true});"),
        "code: {}",
        out.code
    );
    assert_eq!(
        rules(&out),
        vec![(
            "x78zum5".to_string(),
            ".x78zum5{display:flex}".to_string(),
            3000.0
        )]
    );
}

#[test]
fn every_import_form_binds_an_atom() {
    for (source, class_name) in [
        (
            "import x from '@stylexjs/atoms';\nexport const a = x.display.flex;\n",
            "x78zum5",
        ),
        (
            "import * as x from '@stylexjs/atoms';\nexport const a = x.display.flex;\n",
            "x78zum5",
        ),
        (
            "import { color } from '@stylexjs/atoms';\nexport const a = color.red;\n",
            "x1e2nbdu",
        ),
        (
            "import { 'color' as c } from '@stylexjs/atoms';\nexport const a = c.red;\n",
            "x1e2nbdu",
        ),
        (
            "import { type color } from '@stylexjs/atoms';\nexport const a = color.red;\n",
            "x1e2nbdu",
        ),
        (
            "const x = require('@stylexjs/atoms');\nexport const a = x.display.flex;\n",
            "x78zum5",
        ),
        (
            "let x = require('@stylexjs/atoms');\nexport const a = x.display.flex;\n",
            "x78zum5",
        ),
        (
            "const { color } = require('@stylexjs/atoms');\nexport const a = color.red;\n",
            "x1e2nbdu",
        ),
        (
            "const { color: c } = require('@stylexjs/atoms');\nexport const a = c.red;\n",
            "x1e2nbdu",
        ),
    ] {
        let out = compile(source);
        assert_eq!(rules(&out).len(), 1, "source: {source}");
        assert_eq!(&*out.rules[0].class_name, class_name, "source: {source}");
    }
}

#[test]
fn non_atom_import_shapes_stay_inert() {
    for source in [
        "import x from '@stylexjs/atoms/';\nexport const a = x.display.flex;\n",
        "import x from '@stylexjs/atoms/babel-transform';\nexport const a = x.display.flex;\n",
        "export { color } from '@stylexjs/atoms';\n",
        "const x = require('@stylexjs/atoms').default;\nexport const a = x.display.flex;\n",
        "export function f(){ const x = require('@stylexjs/atoms'); return x.display.flex; }\n",
        "import x from '@stylexjs/atoms';\nexport function f(){ let y = x; return y.display.flex; }\n",
        "import x from '@stylexjs/atoms';\nexport const a = x?.display?.flex;\n",
    ] {
        let out = compile(source);
        assert!(out.rules.is_empty(), "source: {source} code: {}", out.code);
    }
}

#[test]
fn single_level_reads_atom_imports_and_two_level_ignores_it() {
    // Namespace import: the property is the key itself, so `display: display`.
    let out = compile(&format!("{ATOMS}export const a = x.display;\n"));
    assert_eq!(&*out.rules[0].ltr, ".xillh77{display:display}");
    // Named import: the property is the imported name, the value the key.
    let out = compile("import { default as d } from '@stylexjs/atoms';\nexport const a = d.red;\n");
    assert_eq!(&*out.rules[0].ltr, ".xa1ppcm{default:red}");
    // Two-level never consults the imported name.
    let out =
        compile("import { color } from '@stylexjs/atoms';\nexport const a = color.foo.red;\n");
    assert_eq!(&*out.rules[0].ltr, ".x1db7jfu{foo:red}");
}

#[test]
fn normalize_value_strips_exactly_one_underscore() {
    for (source, ltr) in [
        (
            "export const a = x.padding._16px;\n",
            ".x1tamke2{padding:16px}",
        ),
        ("export const a = x.content._;\n", ".x14axycx{content:\"\"}"),
        (
            "export const a = x.content.__x;\n",
            ".xxo5q7g{content:\"_x\"}",
        ),
        (
            "export const a = x.padding['16px'];\n",
            ".x1tamke2{padding:16px}",
        ),
    ] {
        let out = compile(&format!("{ATOMS}{source}"));
        assert_eq!(&*out.rules[0].ltr, ltr, "source: {source}");
    }
}

#[test]
fn atom_imports_shadowing_is_scope_blind() {
    // Oracle bug, replicated: the local binding is ignored and left as dead code.
    let out = compile(&format!(
        "{ATOMS}export function f() {{ const x = {{ display: {{ flex: 1 }} }}; return x.display.flex; }}\n"
    ));
    assert_eq!(&*out.rules[0].class_name, "x78zum5");
    let out = compile(&format!("{ATOMS}export const f = (x) => x.display.flex;\n"));
    assert_eq!(&*out.rules[0].class_name, "x78zum5");
}

#[test]
fn type_import_compiles_through_the_binding_fallback_only() {
    let type_import = "import type x from '@stylexjs/atoms';\n";
    // Two-level and dynamic reach the scope fallback.
    let out = compile(&format!("{type_import}export const a = x.display.flex;\n"));
    assert_eq!(&*out.rules[0].class_name, "x78zum5");
    let out = compile(&format!(
        "{type_import}export const a = (c) => x.color(c);\n"
    ));
    assert_eq!(&*out.rules[0].class_name, "x14rh7hd");
    // Single-level needs an atomImports entry, which `import type` never makes.
    let out = compile(&format!("{type_import}export const a = x.display;\n"));
    assert!(out.rules.is_empty(), "code: {}", out.code);
}

#[test]
fn dynamic_atom_hoists_a_temp_and_rewrites_only_the_callee() {
    let out = compile(&format!("{ATOMS}export const a = (c) => x.color(c);\n"));
    assert!(
        out.code.contains(
            "const _temp = {color: _v => [{\"kMwMTN\": _v != null ? \"x14rh7hd\" : _v, \
             \"$$css\": true}, {\"--x-color\": _v != null ? _v : undefined}]};"
        ),
        "code: {}",
        out.code
    );
    assert!(
        out.code.contains("export const a = (c) => _temp.color(c);"),
        "code: {}",
        out.code
    );
    // The @property rule is appended after the style rules, always last.
    assert_eq!(
        rules(&out),
        vec![
            (
                "x14rh7hd".to_string(),
                ".x14rh7hd{color:var(--x-color)}".to_string(),
                3000.0
            ),
            (
                "--x-color".to_string(),
                "@property --x-color { syntax: \"*\"; inherits: false;}".to_string(),
                0.0
            ),
        ]
    );
}

#[test]
fn dynamic_atom_needs_exactly_one_expression_argument() {
    for source in [
        "export const a = () => x.color();\n",
        "export const a = (c) => x.color(c, c);\n",
        "export const a = (c) => x.color(...c);\n",
    ] {
        let out = compile(&format!("{ATOMS}{source}"));
        assert!(out.rules.is_empty(), "source: {source} code: {}", out.code);
    }
}

#[test]
fn dynamic_atom_takes_the_inner_key_through_a_member_callee() {
    let out = compile(&format!(
        "{ATOMS}export const a = (c) => x.color.something(c);\n"
    ));
    assert!(out.code.contains("_temp.color(c)"), "code: {}", out.code);
    // Under a named import the callee's own key still wins.
    let out = compile(
        "import { color } from '@stylexjs/atoms';\nexport const a = (c) => color.foo(c);\n",
    );
    assert_eq!(&*out.rules[0].ltr, ".xpvoh59{foo:var(--x-foo)}");
}

#[test]
fn dynamic_atom_reproduces_the_invalid_identifier_output() {
    // Oracle bug, replicated byte for byte: both the hoisted key and the member
    // are built with t.identifier(), so babel prints unparseable JS.
    let out = compile(&format!(
        "{ATOMS}export const a = (v) => x['--myVar'](v);\n"
    ));
    assert!(
        out.code.contains("const _temp = {--myVar: _v =>") && out.code.contains("_temp.--myVar(v)"),
        "code: {}",
        out.code
    );
    let out = compile(&format!(
        "{ATOMS}export const a = (c) => x['background-color'](c);\n"
    ));
    assert!(
        out.code.contains("_temp.background-color(c)"),
        "code: {}",
        out.code
    );
    // The static forms of the same properties are valid output.
    let out = compile(&format!("{ATOMS}export const a = x['--myVar'].red;\n"));
    assert!(!out.code.contains("--myVar:"), "code: {}", out.code);
}

#[test]
fn hoist_uid_skips_names_taken_in_scope() {
    let out = compile(&format!(
        "{ATOMS}const _temp = 1;\nexport const a = (c) => x.color(c);\nexport const b = _temp;\n"
    ));
    assert!(out.code.contains("_temp2.color(c)"), "code: {}", out.code);
    let out = compile(&format!(
        "{ATOMS}export const a = (c) => {{ const _temp = 1; return [x.color(c), _temp]; }};\n"
    ));
    assert!(out.code.contains("_temp2.color(c)"), "code: {}", out.code);
}

#[test]
fn nested_atoms_compile_outer_first() {
    // The dynamic call is rewritten, the trailing member survives.
    let out = compile(&format!("{ATOMS}export const a = (c) => x.color(c).red;\n"));
    assert!(
        out.code.contains("_temp.color(c).red"),
        "code: {}",
        out.code
    );
    // A three-level chain matches the inner member and dangles the outer key.
    let out = compile(&format!("{ATOMS}export const a = x.display.flex.bar;\n"));
    assert!(
        out.code
            .contains("({k1xSpc: \"x78zum5\", $$css: true}).bar"),
        "code: {}",
        out.code
    );
    // The dynamic's rules register before the static argument's.
    let out = compile(&format!(
        "{ATOMS}export const a = x.color(x.display.flex);\n"
    ));
    assert_eq!(
        out.rules
            .iter()
            .map(|r| r.class_name.as_ref())
            .collect::<Vec<_>>(),
        vec!["x14rh7hd", "--x-color", "x78zum5"]
    );
}

#[test]
fn dev_and_debug_shape_the_keys_but_test_does_nothing() {
    let dev = compile_opts(
        &format!("{ATOMS}export const a = x.display.flex;\n"),
        serde_json::json!({ "dev": true }),
    );
    assert!(
        dev.code.contains(
            "({input____inline__: \"input____inline__\", \"display-k1xSpc\": \"x78zum5\", \
             $$css: true})"
        ),
        "code: {}",
        dev.code
    );
    // debug names the keys but never replaces $$css with a source string.
    let debug = compile_opts(
        &format!("{ATOMS}export const a = x.display.flex;\n"),
        serde_json::json!({ "debug": true, "enableDebugDataProp": true }),
    );
    assert!(
        debug
            .code
            .contains("({\"display-k1xSpc\": \"x78zum5\", $$css: true})"),
        "code: {}",
        debug.code
    );
    // test:true converts create() styles but leaves atoms fully compiled.
    let test = compile_opts(
        &format!("{ATOMS}export const a = x.display.flex;\n"),
        serde_json::json!({ "test": true }),
    );
    assert!(
        test.code.contains("({k1xSpc: \"x78zum5\", $$css: true})"),
        "code: {}",
        test.code
    );
}

#[test]
fn dev_class_name_follows_the_file_basename() {
    for (filename, dev_class) in [
        ("src/tokens.stylex.ts", "tokens____inline__"),
        ("src/my-file.tsx", "my-file____inline__"),
        ("src/a.b.c.ts", "a____inline__"),
    ] {
        let out = run(
            &format!("{ATOMS}export const a = x.display.flex;\n"),
            filename,
            &options(serde_json::json!({ "dev": true })),
        )
        .expect("compile");
        assert!(out.code.contains(dev_class), "{filename}: {}", out.code);
    }
}

#[test]
fn atom_rules_follow_every_create_family_rule_and_never_dedupe() {
    let out = compile(&format!(
        "{STYLEX}{ATOMS}export const a = x.color.red;\n\
         export const s = stylex.create({{ r: {{ color: 'blue' }} }});\n"
    ));
    assert_eq!(
        out.rules
            .iter()
            .map(|r| r.class_name.as_ref())
            .collect::<Vec<_>>(),
        vec!["xju2f9n", "x1e2nbdu"]
    );
    // Each atom is its own styleXCreateSet call: no cross-call dedupe.
    let out = compile(&format!(
        "{ATOMS}export const a = [x.display.flex, x.display.flex];\n"
    ));
    assert_eq!(
        out.rules
            .iter()
            .map(|r| r.class_name.as_ref())
            .collect::<Vec<_>>(),
        vec!["x78zum5", "x78zum5"]
    );
}

#[test]
fn props_merges_atoms_as_plain_compiled_namespaces() {
    let out = compile(&format!(
        "{STYLEX}{ATOMS}const s = stylex.create({{ r: {{ color: 'blue' }} }});\n\
         export const p = stylex.props(s.r, x.color.red);\n"
    ));
    assert!(
        out.code.contains("({className: \"x1e2nbdu\"})"),
        "code: {}",
        out.code
    );
    let out = compile(&format!(
        "{STYLEX}{ATOMS}const s = stylex.create({{ r: {{ color: 'blue' }} }});\n\
         export const p = stylex.props(x.color.red, s.r);\n"
    ));
    assert!(
        out.code.contains("({className: \"xju2f9n\"})"),
        "code: {}",
        out.code
    );
    let out = compile(&format!(
        "{STYLEX}{ATOMS}export const p = stylex.attrs(x.display.flex);\n"
    ));
    assert!(
        out.code.contains("({class: \"x78zum5\"})"),
        "code: {}",
        out.code
    );
}

#[test]
fn a_bailing_props_call_hoists_its_static_atom_arguments() {
    let out = compile(&format!(
        "{STYLEX}{ATOMS}export const p = (c) => stylex.props(x.display.flex, x.color(c));\n"
    ));
    assert!(
        out.code
            .contains("const _temp2 = ({k1xSpc: \"x78zum5\", $$css: true});")
            && out.code.contains("stylex.props(_temp2, _temp.color(c))"),
        "code: {}",
        out.code
    );
}

#[test]
fn atom_errors_match_the_oracle_error_classes() {
    // Upstream crashes inside replaceWith on an assignment target.
    let err = compile_err(&format!(
        "{ATOMS}export function f(){{ x.display.flex = 1; }}\n"
    ));
    assert!(
        err.to_string().contains("assignment-target position"),
        "message: {err}"
    );
    // Value-level errors surface as ordinary StyleX errors.
    let err = compile_err(&format!(
        "{ATOMS}export const a = x.width['calc(100% - '];\n"
    ));
    assert_eq!(err.code, oj_stylex::errors::ErrorCode::UnclosedFunction);
    // An atom inside create() cannot be evaluated: the import never resolves.
    let err = compile_err(&format!(
        "{STYLEX}{ATOMS}export const s = stylex.create({{ r: {{ color: x.color.red }} }});\n"
    ));
    assert!(
        err.to_string().contains("attempting to evaluate"),
        "message: {err}"
    );
}

#[test]
fn atoms_take_no_property_validation_and_no_source_resolution() {
    // No property-name validation whatsoever, even under throw mode.
    let out = compile_opts(
        &format!("{ATOMS}export const a = x.notARealProperty.foo;\n"),
        serde_json::json!({ "propertyValidationMode": "throw" }),
    );
    assert_eq!(&*out.rules[0].ltr, ".x19s93ox{not-areal-property:foo}");
    let out = compile(&format!("{ATOMS}export const a = x[0].flex;\n"));
    assert_eq!(&*out.rules[0].ltr, ".x11meqmi{0:flex}");
    // importSources never creates atoms; the atoms source is hardcoded.
    let out = compile_opts(
        "import x from 'my-atoms';\nexport const a = x.display.flex;\n",
        serde_json::json!({ "importSources": ["my-atoms", "@stylexjs/stylex"] }),
    );
    assert!(out.rules.is_empty(), "code: {}", out.code);
}

#[test]
fn runtime_injection_covers_atom_rules() {
    let out = compile_opts(
        &format!("{ATOMS}export const a = (c) => x.color(c);\n"),
        serde_json::json!({ "runtimeInjection": true }),
    );
    assert!(
        out.code
            .contains("_inject2({ltr: \".x14rh7hd{color:var(--x-color)}\", priority: 3000});")
            && out.code.contains(
                "_inject2({ltr: \"@property --x-color { syntax: \\\"*\\\"; inherits: false;}\", \
                 priority: 0});"
            ),
        "code: {}",
        out.code
    );
}
