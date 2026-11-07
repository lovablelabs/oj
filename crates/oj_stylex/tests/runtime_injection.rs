//! `options.runtimeInjection` emit shape, pinned against @stylexjs/babel-plugin
//! 0.19.0 probes: the import + alias pair, per-rule calls, anchors, uid collisions.

use std::path::{Path, PathBuf};

use oj_stylex::api::{FileContext, parse_program, transform_program, transform_source};
use oj_stylex::module_resolution::BoundedFs;
use oj_stylex::options::{CompilerOptions, ResolvedOptions};
use oxc_allocator::Allocator;
use oxc_codegen::Codegen;
use oxc_parser::Parser;
use oxc_span::SourceType;

const IMP: &str = "import * as stylex from '@stylexjs/stylex';\n";
const DEFAULT_PATH: &str = "@stylexjs/stylex/lib/stylex-inject";

fn fixture_root() -> PathBuf {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("runtime-injection-fixture");
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

fn injection(value: serde_json::Value) -> ResolvedOptions {
    options(serde_json::json!({ "runtimeInjection": value }))
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

struct Out {
    code: String,
    rules: Vec<serde_json::Value>,
}

/// Both backends over one module; their reprints must agree, and the shared
/// reprint is what every expectation below reads.
fn compile(source: &str, filename: &str, opts: &ResolvedOptions) -> Out {
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
    let ast = transform_program(&allocator, &mut program, &ctx, opts, &fs).expect("ast compile");
    let ast_code = Codegen::new().build(&program).code;

    let splice_rules: Vec<_> = splice.rules.iter().map(|r| r.to_metadata_tuple()).collect();
    let ast_rules: Vec<_> = ast.rules.iter().map(|r| r.to_metadata_tuple()).collect();
    assert_eq!(splice_rules, ast_rules, "rule metadata diverges");
    let code = reprint(&splice.code);
    assert_eq!(
        code,
        reprint(&ast_code),
        "backend reprint divergence\n-- splice --\n{}\n-- ast --\n{ast_code}",
        splice.code
    );
    Out {
        code,
        rules: splice_rules,
    }
}

fn compile_tsx(source: &str, opts: &ResolvedOptions) -> Out {
    compile(source, "src/input.tsx", opts)
}

fn line_index(code: &str, needle: &str) -> usize {
    code.lines()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} missing from:\n{code}"))
}

const RED: &str = "export const styles = stylex.create({ a: { color: 'red' } });\n";

#[test]
fn default_form_imports_and_calls_the_alias_not_the_binding() {
    let out = compile_tsx(&format!("{IMP}{RED}"), &injection(serde_json::json!(true)));
    assert_eq!(
        out.code.lines().take(2).collect::<Vec<_>>(),
        vec![
            format!("import _inject from \"{DEFAULT_PATH}\";"),
            "var _inject2 = _inject;".to_string(),
        ],
        "code: {}",
        out.code
    );
    assert!(
        out.code
            .contains("_inject2({\n\tltr: \".x1e2nbdu{color:red}\",\n\tpriority: 3e3\n});"),
        "code: {}",
        out.code
    );
}

#[test]
fn string_value_is_the_import_specifier_verbatim() {
    for path in ["@custom/inject-path", "", "true", "./my-inject.js"] {
        let out = compile_tsx(
            &format!("{IMP}{RED}"),
            &injection(serde_json::Value::String(path.to_string())),
        );
        assert!(
            out.code
                .starts_with(&format!("import _inject from \"{path}\";\n")),
            "code: {}",
            out.code
        );
    }
}

#[test]
fn named_form_aliases_from_the_as_name() {
    let out = compile_tsx(
        &format!("{IMP}{RED}"),
        &injection(serde_json::json!({ "from": "@custom/p", "as": "injectStyle" })),
    );
    assert!(
        out.code.starts_with(
            "import { injectStyle as _inject } from \"@custom/p\";\nvar _injectStyle = _inject;\n"
        ),
        "code: {}",
        out.code
    );
    assert!(out.code.contains("_injectStyle({"), "code: {}", out.code);
}

#[test]
fn uids_skip_names_the_source_already_binds() {
    // The default form takes two `inject` uids in a row, so a user `_inject`
    // and `_inject2` push the pair to `_inject3` / `_inject4`.
    let out = compile_tsx(
        &format!(
            "{IMP}const _inject = 1;\nconst _inject2 = 2;\n{RED}export const u = [_inject, _inject2];\n"
        ),
        &injection(serde_json::json!(true)),
    );
    assert!(
        out.code.starts_with(&format!(
            "import _inject3 from \"{DEFAULT_PATH}\";\nvar _inject4 = _inject3;\n"
        )),
        "code: {}",
        out.code
    );
    assert!(out.code.contains("_inject4({"), "code: {}", out.code);

    // The named form's alias derives from `as`, so only that name collides.
    let out = compile_tsx(
        &format!("{IMP}const _injectStyle = 0;\n{RED}export const u = _injectStyle;\n"),
        &injection(serde_json::json!({ "from": "@p/i", "as": "injectStyle" })),
    );
    assert!(
        out.code.starts_with(
            "import { injectStyle as _inject } from \"@p/i\";\nvar _injectStyle2 = _inject;\n"
        ),
        "code: {}",
        out.code
    );
}

#[test]
fn one_import_serves_every_registration_site() {
    let out = compile_tsx(
        &format!(
            "{IMP}export const one = stylex.create({{ a: {{ color: 'red' }} }});\n\
             const zzz = 1;\n\
             export const two = stylex.create({{ b: {{ color: 'blue' }} }});\n"
        ),
        &injection(serde_json::json!(true)),
    );
    assert_eq!(out.code.matches("import _inject from").count(), 1);
    assert_eq!(out.code.matches("var _inject2 = _inject;").count(), 1);
    // Each call sits before its own anchor statement, in styles order.
    assert!(
        line_index(&out.code, ".x1e2nbdu{color:red}") < line_index(&out.code, "export const one")
            && line_index(&out.code, "export const one")
                < line_index(&out.code, ".xju2f9n{color:blue}")
            && line_index(&out.code, ".xju2f9n{color:blue}")
                < line_index(&out.code, "export const two"),
        "code: {}",
        out.code
    );
}

#[test]
fn rules_carry_rtl_and_const_keys_in_a_fixed_order() {
    let out = compile_tsx(
        &format!("{IMP}export const s = stylex.create({{ a: {{ float: 'start' }} }});\n"),
        &injection(serde_json::json!(true)),
    );
    let call = out
        .code
        .split("_inject2(")
        .nth(1)
        .expect("an inject call")
        .to_string();
    let keys: Vec<&str> = ["ltr:", "rtl:", "priority:"]
        .iter()
        .filter(|k| call.contains(**k))
        .copied()
        .collect();
    assert_eq!(keys, vec!["ltr:", "rtl:", "priority:"], "call: {call}");
    assert!(call.contains("{float:right}\""), "{call}");

    let out = compile(
        &format!("{IMP}export const c = stylex.defineConsts({{ a: '1px', n: 4 }});\n"),
        "src/c.stylex.ts",
        &options(serde_json::json!({
            "runtimeInjection": true,
            "unstable_moduleResolution": { "type": "commonJS" }
        })),
    );
    assert!(
        out.code.contains("constKey: ") && out.code.contains("constVal: \"1px\""),
        "code: {}",
        out.code
    );
    // Numbers stay numeric literals; everything else stringifies.
    assert!(out.code.contains("constVal: 4"), "code: {}", out.code);
    assert!(out.code.contains("ltr: \"\""), "code: {}", out.code);
}

#[test]
fn keyframes_anchors_at_its_own_statement() {
    let out = compile_tsx(
        &format!(
            "{IMP}const fade = stylex.keyframes({{ from: {{ opacity: 0 }} }});\n\
             export const s = stylex.create({{ a: {{ animationName: fade }} }});\n"
        ),
        &injection(serde_json::json!(true)),
    );
    assert!(
        line_index(&out.code, "@keyframes") < line_index(&out.code, "const fade =")
            && line_index(&out.code, "const fade =") < line_index(&out.code, "animation-name"),
        "code: {}",
        out.code
    );

    // Nested inside the create call they share one anchor, keyframes first.
    let out = compile_tsx(
        &format!(
            "{IMP}export const s = stylex.create({{ a: {{ animationName: stylex.keyframes({{ from: {{ opacity: 0 }} }}) }} }});\n"
        ),
        &injection(serde_json::json!(true)),
    );
    assert!(
        line_index(&out.code, "@keyframes") < line_index(&out.code, "animation-name")
            && line_index(&out.code, "animation-name") < line_index(&out.code, "export const s"),
        "code: {}",
        out.code
    );
}

#[test]
fn injects_survive_dce_of_the_create_they_came_from() {
    let out = compile_tsx(
        &format!("{IMP}const styles = stylex.create({{ a: {{ color: 'red' }} }});\n"),
        &injection(serde_json::json!(true)),
    );
    assert!(!out.code.contains("const styles"), "code: {}", out.code);
    assert!(out.code.contains("_inject2({"), "code: {}", out.code);
}

#[test]
fn zero_rules_emit_nothing() {
    for source in [
        format!("{IMP}export const styles = stylex.create({{}});\n"),
        format!("{IMP}stylex.keyframes;\n"),
    ] {
        let out = compile_tsx(&source, &injection(serde_json::json!(true)));
        assert!(!out.code.contains("_inject"), "code: {}", out.code);
    }
}

#[test]
fn off_and_type_invalid_values_emit_nothing_and_never_throw() {
    let on = compile_tsx(&format!("{IMP}{RED}"), &injection(serde_json::json!(true)));
    for value in [
        serde_json::json!(false),
        serde_json::Value::Null,
        serde_json::json!(1),
        serde_json::json!({ "from": "@custom/p" }),
    ] {
        let out = compile_tsx(&format!("{IMP}{RED}"), &injection(value.clone()));
        assert!(!out.code.contains("_inject"), "{value}: {}", out.code);
        // Injection is purely additive: metadata is byte-identical either way.
        assert_eq!(out.rules, on.rules, "{value}");
    }
}

#[test]
fn the_inject_import_block_hoists_above_the_generated_sx_import() {
    const SRC: &str = "import { create } from '@stylexjs/stylex';\n";
    let el = "export const El = <div sx={styles.a} />;\n";
    let create = "export const styles = create({ a: { color: 'red' } });\n";

    // JSX after the create: the sx import is unshifted last, and the hoist
    // sort lifts only the inject import above it.
    let out = compile_tsx(
        &format!("{SRC}{create}{el}"),
        &injection(serde_json::json!(true)),
    );
    assert_eq!(
        out.code.lines().take(3).collect::<Vec<_>>(),
        vec![
            format!("import _inject from \"{DEFAULT_PATH}\";"),
            "import * as _stylex from \"@stylexjs/stylex\";".to_string(),
            "var _inject2 = _inject;".to_string(),
        ],
        "code: {}",
        out.code
    );

    // JSX before the create: the alias was inserted after the inject import
    // while the sx import already sat below it.
    let out = compile_tsx(
        &format!("{SRC}{el}{create}"),
        &injection(serde_json::json!(true)),
    );
    assert_eq!(
        out.code.lines().take(3).collect::<Vec<_>>(),
        vec![
            format!("import _inject from \"{DEFAULT_PATH}\";"),
            "var _inject2 = _inject;".to_string(),
            "import * as _stylex from \"@stylexjs/stylex\";".to_string(),
        ],
        "code: {}",
        out.code
    );
}
