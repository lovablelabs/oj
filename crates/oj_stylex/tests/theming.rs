//! Oracle-pinned tests for the theming trio (defineVars / createTheme /
//! types.*) vs @stylexjs/babel-plugin 0.19.0, incl. CSS through assemble.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, CallExpression, Declaration, Expression, ModuleExportName, Program, Statement,
};
use oxc_parser::Parser;
use oxc_span::SourceType;
use serde_json::Value;

use oj_stylex::assemble::{AssembleConfig, assemble};
use oj_stylex::errors::{ErrorCode, StylexError};
use oj_stylex::eval::functions::{
    EvalCallables, StubCallables, StylexCallable, coerce_vars_keyframes_frames, vars_registry,
};
use oj_stylex::eval::value::{EvalValue, JsObjectMap};
use oj_stylex::eval::{EvalOutcome, Evaluator, FunctionRegistry, unwrap_parens};
use oj_stylex::imports::StylexNamedImport;
use oj_stylex::module_resolution::{
    BoundedFs, FsProvider, StdFs, canonical_file_path, matches_file_suffix,
};
use oj_stylex::options::{CompilerOptions, ResolvedOptions};
use oj_stylex::rules::StylexRule;
use oj_stylex::shared::create_theme::{apply_theme_dev_naming, create_theme};
use oj_stylex::shared::define_vars::define_vars;
use oj_stylex::shared::keyframes::keyframes;
use oj_stylex::shared::types::create_css_type;
use oj_stylex::state::CompileState;

fn pins_dir() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/testdata/pins/theming"
    ))
}

fn corpus_root() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/conformance/corpus/upstream/fixtures/default"
    ))
}

/// The real visitor's callables for defineVars/createTheme evaluation.
struct ThemingCallables;

impl EvalCallables for ThemingCallables {
    fn call(
        &self,
        callee: &StylexCallable,
        args: &[EvalValue],
        state: &mut CompileState<'_>,
    ) -> Result<EvalValue, StylexError> {
        match callee {
            StylexCallable::Keyframes => {
                let raw = args.first().cloned().unwrap_or(EvalValue::Undefined);
                let frames = coerce_vars_keyframes_frames(&raw)?;
                let (name, rule) = keyframes(&frames, state.options)?;
                state.record_other_injected_rule(rule);
                Ok(EvalValue::Str(name))
            }
            StylexCallable::Types(kind) => create_css_type(kind, args),
            other => StubCallables.call(other, args, state),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ThemingKind {
    DefineVars,
    CreateTheme,
}

impl ThemingKind {
    fn api(self) -> &'static str {
        match self {
            ThemingKind::DefineVars => "defineVars",
            ThemingKind::CreateTheme => "createTheme",
        }
    }
}

fn theming_call<'a>(
    state: &CompileState<'_>,
    expr: &'a Expression<'a>,
) -> Option<(ThemingKind, &'a CallExpression<'a>)> {
    let Expression::CallExpression(call) = unwrap_parens(expr) else {
        return None;
    };
    match unwrap_parens(&call.callee) {
        Expression::Identifier(id) => match state.imports.named_binding(&id.name) {
            Some(StylexNamedImport::DefineVars) => Some((ThemingKind::DefineVars, call)),
            Some(StylexNamedImport::CreateTheme) => Some((ThemingKind::CreateTheme, call)),
            _ => None,
        },
        Expression::StaticMemberExpression(member) => {
            let kind = match member.property.name.as_str() {
                "defineVars" => ThemingKind::DefineVars,
                "createTheme" => ThemingKind::CreateTheme,
                _ => return None,
            };
            match &member.object {
                Expression::Identifier(object)
                    if state.imports.is_stylex_namespace(&object.name) =>
                {
                    Some((kind, call))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

// parity: ast-helpers isVariableNamedExported — aliased specifiers don't count.
fn exported_names(program: &Program<'_>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for statement in &program.body {
        match statement {
            Statement::ExportDeclaration(export) => {
                if let Declaration::VariableDeclaration(decl) = &export.declaration {
                    for declarator in &decl.declarations {
                        if let Some(id) = declarator.id.get_binding_identifier() {
                            out.insert(id.name.to_string());
                        }
                    }
                }
            }
            Statement::ExportNamedDeclaration(export) => {
                for specifier in &export.specifiers {
                    let local = match &specifier.local {
                        ModuleExportName::IdentifierReference(id) => id.name.as_str(),
                        ModuleExportName::IdentifierName(id) => id.name.as_str(),
                        ModuleExportName::StringLiteral(_) => continue,
                    };
                    let exported = match &specifier.exported {
                        ModuleExportName::IdentifierReference(id) => id.name.as_str(),
                        ModuleExportName::IdentifierName(id) => id.name.as_str(),
                        ModuleExportName::StringLiteral(_) => continue,
                    };
                    if local == exported {
                        out.insert(local.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    out
}

// parity: state-manager fileNameForHashing.
fn file_name_for_hashing(
    fs: &dyn FsProvider,
    filename: &str,
    options: &ResolvedOptions,
) -> Option<String> {
    let module_resolution = options.unstable_module_resolution.as_ref()?;
    if !matches_file_suffix(&module_resolution.theme_file_extension, filename)
        && !matches_file_suffix(&module_resolution.consts_file_extension(), filename)
    {
        return None;
    }
    Some(canonical_file_path(
        fs,
        Path::new(filename),
        module_resolution.root_dir.as_deref(),
    ))
}

struct ModuleOutput {
    rules: Vec<StylexRule>,
    js_outputs: Vec<(String, JsObjectMap)>,
}

fn argument_expression<'a>(
    argument: &'a Argument<'a>,
    kind: ThemingKind,
) -> Result<&'a Expression<'a>, StylexError> {
    match argument {
        Argument::SpreadElement(_) => Err(StylexError::non_static_value(kind.api())),
        _ => Ok(argument.to_expression()),
    }
}

fn compile_theming_module(
    source: &str,
    filename: &str,
    cwd: &str,
    options: &ResolvedOptions,
    fs: &dyn FsProvider,
) -> Result<ModuleOutput, StylexError> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(filename).unwrap_or_else(|_| SourceType::tsx());
    let parsed = Parser::new(&allocator, source, source_type).parse();
    assert!(
        !parsed.fatal_error && !parsed.diagnostics.has_errors(),
        "parse failed for {filename}"
    );
    let mut state = CompileState::build(
        &parsed.program,
        options,
        Some(filename.to_string()),
        cwd.to_string(),
    )?;
    let exported = exported_names(&parsed.program);

    let mut rules: Vec<StylexRule> = Vec::new();
    let mut js_outputs: Vec<(String, JsObjectMap)> = Vec::new();

    for statement in &parsed.program.body {
        let declaration = match statement {
            Statement::ExpressionStatement(stmt) => {
                if let Some((kind, _)) = theming_call(&state, &stmt.expression) {
                    return Err(StylexError::unbound_call_value(kind.api()));
                }
                None
            }
            Statement::ExportDefaultDeclaration(export) => {
                if let Some(expr) = export.declaration.as_expression()
                    && let Some((kind, _)) = theming_call(&state, expr)
                {
                    return Err(StylexError::unbound_call_value(kind.api()));
                }
                None
            }
            Statement::VariableDeclaration(decl) => Some(&**decl),
            Statement::ExportDeclaration(export) => match &export.declaration {
                Declaration::VariableDeclaration(decl) => Some(&**decl),
                _ => None,
            },
            _ => None,
        };
        let Some(declaration) = declaration else {
            continue;
        };
        for declarator in &declaration.declarations {
            let Some(init) = &declarator.init else {
                continue;
            };
            let Some((kind, call)) = theming_call(&state, init) else {
                continue;
            };
            let Some(id) = declarator.id.get_binding_identifier() else {
                return Err(StylexError::unbound_call_value(kind.api()));
            };
            let name = id.name.to_string();
            match kind {
                ThemingKind::DefineVars => {
                    if !exported.contains(&name) {
                        return Err(StylexError::non_export_named_declaration("defineVars"));
                    }
                    if call.arguments.len() != 1 {
                        return Err(StylexError::illegal_argument_length("defineVars", 1));
                    }
                    let canonical = file_name_for_hashing(fs, filename, options)
                        .ok_or_else(|| StylexError::cannot_generate_hash("defineVars"))?;
                    let arg = argument_expression(&call.arguments[0], kind)?;
                    let injected_before = state.other_injected_rules.len();
                    let output = {
                        let registry = vars_registry(&state.imports, &options.env);
                        let mut ev = Evaluator::with_registry(
                            &mut state,
                            fs,
                            &ThemingCallables,
                            registry,
                            false,
                        );
                        let value = match ev.eval(arg)? {
                            EvalOutcome::Value(v) => v,
                            EvalOutcome::NonStatic(_) => {
                                return Err(StylexError::non_static_value("defineVars"));
                            }
                        };
                        define_vars(&mut ev, &value, &canonical, &name)?
                    };
                    rules.extend_from_slice(&state.other_injected_rules[injected_before..]);
                    rules.extend(output.rules);
                    state.record_root_override(
                        &name,
                        EvalValue::Obj(output.js_output.clone().into()),
                    );
                    js_outputs.push((name, output.js_output));
                }
                ThemingKind::CreateTheme => {
                    if call.arguments.len() != 2 {
                        return Err(StylexError::illegal_argument_length("createTheme", 2));
                    }
                    let arg0 = argument_expression(&call.arguments[0], kind)?;
                    let arg1 = argument_expression(&call.arguments[1], kind)?;
                    let injected_before = state.other_injected_rules.len();
                    let theme_vars = {
                        // First argument evaluates with no FunctionConfig upstream.
                        let mut ev = Evaluator::with_registry(
                            &mut state,
                            fs,
                            &ThemingCallables,
                            FunctionRegistry::default(),
                            false,
                        );
                        match ev.eval(arg0)? {
                            EvalOutcome::Value(v) => v,
                            EvalOutcome::NonStatic(_) => {
                                return Err(StylexError::non_static_value("createTheme"));
                            }
                        }
                    };
                    let overrides = {
                        let registry = vars_registry(&state.imports, &options.env);
                        let mut ev = Evaluator::with_registry(
                            &mut state,
                            fs,
                            &ThemingCallables,
                            registry,
                            false,
                        );
                        match ev.eval(arg1)? {
                            EvalOutcome::Value(v) => v,
                            EvalOutcome::NonStatic(_) => {
                                return Err(StylexError::non_static_value("createTheme"));
                            }
                        }
                    };
                    let output = create_theme(&theme_vars, &overrides, options)?;
                    let js_output =
                        apply_theme_dev_naming(output.js_output, Some(filename), &name, options);
                    rules.extend_from_slice(&state.other_injected_rules[injected_before..]);
                    rules.extend(output.rules);
                    state.record_root_override(&name, EvalValue::Obj(js_output.clone().into()));
                    js_outputs.push((name, js_output));
                }
            }
        }
    }
    Ok(ModuleOutput { rules, js_outputs })
}

// ---- pin replay ------------------------------------------------------------

fn encode(value: &EvalValue) -> Value {
    match value {
        EvalValue::Null | EvalValue::Undefined => Value::Null,
        EvalValue::Bool(b) => Value::Bool(*b),
        EvalValue::Num(_) => value.to_json(),
        EvalValue::Str(s) => Value::String(s.clone()),
        EvalValue::Arr(items) => Value::Array(items.iter().map(encode).collect()),
        EvalValue::Obj(map) => {
            let pairs: Vec<Value> = map
                .entries()
                .map(|(k, v)| Value::Array(vec![Value::String(k.to_string()), encode(v)]))
                .collect();
            let mut wrapper = serde_json::Map::new();
            wrapper.insert("$obj".to_string(), Value::Array(pairs));
            Value::Object(wrapper)
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pin {
    name: String,
    source: String,
    filename: String,
    cwd: String,
    options: Value,
    exports: Vec<String>,
    #[serde(default)]
    rust_expect: Option<Value>,
    #[serde(default)]
    expected_error: Option<ExpectedError>,
    #[serde(default)]
    expected_rules: Option<Vec<ExpectedRule>>,
    #[serde(default)]
    expected_js_outputs: Option<Value>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExpectedError {
    message: String,
    code: String,
    message_exact: bool,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExpectedRule {
    class_name: String,
    ltr: String,
    rtl: Option<String>,
    priority: f64,
}

fn code_name(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::NonStaticValue => "NonStaticValue",
        ErrorCode::NonStyleObject => "NonStyleObject",
        ErrorCode::IllegalArgumentLength => "IllegalArgumentLength",
        ErrorCode::UnboundCallValue => "UnboundCallValue",
        ErrorCode::NonExportNamedDeclaration => "NonExportNamedDeclaration",
        ErrorCode::CannotGenerateHash => "CannotGenerateHash",
        ErrorCode::InvalidDefineVarsValue => "InvalidDefineVarsValue",
        ErrorCode::ArrayInDefineVars => "ArrayInDefineVars",
        ErrorCode::MissingDefaultValue => "MissingDefaultValue",
        ErrorCode::UnknownDefineVarsReference => "UnknownDefineVarsReference",
        ErrorCode::CyclicDefineVarsReference => "CyclicDefineVarsReference",
        ErrorCode::InvalidDefineVarsFunctionValue => "InvalidDefineVarsFunctionValue",
        ErrorCode::ThemeWithoutVarGroup => "ThemeWithoutVarGroup",
        ErrorCode::UpstreamTypeError => "UpstreamTypeError",
        ErrorCode::NonObjectKeyframe => "NonObjectKeyframe",
        ErrorCode::UnsupportedOption => "UnsupportedOption",
        ErrorCode::UnsupportedApi => "UnsupportedApi",
        _ => "other",
    }
}

fn resolve_placeholders(value: &Value, placeholder: &str, root: &str) -> Value {
    match value {
        Value::String(s) => Value::String(s.replace(placeholder, root)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| resolve_placeholders(v, placeholder, root))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), resolve_placeholders(v, placeholder, root)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn assert_rules(label: &str, actual: &[StylexRule], expected: &[ExpectedRule]) {
    assert_eq!(actual.len(), expected.len(), "{label}: rule count");
    for (rule, expected) in actual.iter().zip(expected) {
        assert_eq!(&*rule.class_name, expected.class_name, "{label}: className");
        assert_eq!(
            &*rule.ltr, expected.ltr,
            "{label}: ltr for {}",
            rule.class_name
        );
        assert_eq!(
            rule.rtl.as_deref(),
            expected.rtl.as_deref(),
            "{label}: rtl for {}",
            rule.class_name
        );
        assert!(
            rule.priority == expected.priority,
            "{label}: priority for {} ({} != {})",
            rule.class_name,
            rule.priority,
            expected.priority
        );
    }
}

fn assert_error(label: &str, err: &StylexError, expected: &ExpectedError) {
    assert_eq!(
        code_name(err.code),
        expected.code,
        "{label}: error code mismatch ({})",
        err.message
    );
    if expected.message_exact {
        assert_eq!(
            err.message.lines().next().unwrap_or_default(),
            expected.message,
            "{label}: error message"
        );
    }
}

#[test]
fn oracle_theming_pins() {
    let raw = std::fs::read_to_string(pins_dir().join("theming.json"))
        .expect("run crates/stylex/conformance/src/gen-pins-theming.mjs to generate pins");
    let pins: Vec<Pin> = serde_json::from_str(&raw).unwrap();
    assert!(
        pins.len() >= 120,
        "suspiciously small pin set: {}",
        pins.len()
    );

    let fixroot = pins_dir().join("fixtures");
    let fixroot_str = fixroot.to_string_lossy().into_owned();
    let fs = BoundedFs {
        root: fixroot.clone(),
    };
    let mut error_pins = 0;
    for pin in &pins {
        let label = &pin.name;
        let filename = pin.filename.replace("<FIXROOT>", &fixroot_str);
        let cwd = pin.cwd.replace("<FIXROOT>", &fixroot_str);
        let options_json = resolve_placeholders(&pin.options, "<FIXROOT>", &fixroot_str);
        let options = CompilerOptions::from_json(&options_json)
            .unwrap_or_else(|e| panic!("{label}: options parse: {e}"))
            .resolve()
            .unwrap_or_else(|e| panic!("{label}: options resolve: {e}"));
        let result = compile_theming_module(&pin.source, &filename, &cwd, &options, &fs);

        if let Some(expect) = &pin.rust_expect {
            // Documented divergences, both directions (see the report/docs).
            if expect.get("compiles").and_then(Value::as_bool) == Some(true) {
                assert!(
                    result.is_ok(),
                    "{label}: expected rust to compile where the oracle errors"
                );
            } else {
                let code = expect["code"].as_str().expect("rustExpect code");
                let err = match result {
                    Err(err) => err,
                    Ok(_) => panic!("{label}: expected divergence error {code}"),
                };
                assert_eq!(code_name(err.code), code, "{label}: divergence code");
            }
            continue;
        }

        if let Some(expected) = &pin.expected_error {
            error_pins += 1;
            let err = match result {
                Err(err) => err,
                Ok(_) => panic!("{label}: expected error {:?}", expected.message),
            };
            assert_error(label, &err, expected);
            continue;
        }

        let output = result.unwrap_or_else(|e| panic!("{label}: unexpected error: {e}"));
        assert_rules(
            label,
            &output.rules,
            pin.expected_rules.as_ref().expect("rules pin"),
        );
        if let Some(expected_js) = &pin.expected_js_outputs {
            // Only pinned exports compare: the oracle cannot read non-exported
            // bindings, which are still legal for createTheme.
            let mut compiled = JsObjectMap::new();
            for export_name in &pin.exports {
                let js = output
                    .js_outputs
                    .iter()
                    .find(|(name, _)| name == export_name)
                    .map(|(_, js)| js.clone())
                    .unwrap_or_else(|| panic!("{label}: missing export {export_name}"));
                compiled.insert(export_name.clone(), EvalValue::Obj(js.into()));
            }
            assert_eq!(
                &encode(&EvalValue::Obj(compiled.into())),
                expected_js,
                "{label}: js output mismatch"
            );
        }
    }
    assert!(
        error_pins >= 50,
        "suspiciously few error pins: {error_pins}"
    );
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CorpusFile {
    skipped: Vec<String>,
    records: Vec<CorpusPin>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CorpusPin {
    id: String,
    filename: String,
    source: String,
    options: Value,
    mode: String,
    #[serde(default)]
    expected_error: Option<ExpectedError>,
    #[serde(default)]
    expected_rules: Option<Vec<ExpectedRule>>,
}

#[test]
fn oracle_theming_corpus_cross_check() {
    let raw = std::fs::read_to_string(pins_dir().join("corpus-theming.json"))
        .expect("run crates/stylex/conformance/src/gen-pins-theming.mjs to generate pins");
    let file: CorpusFile = serde_json::from_str(&raw).unwrap();
    assert!(file.records.len() >= 70, "suspiciously small corpus set");
    // Skips are runtimeInjection/haste/env jobs and unstable_* sources — all
    // sanctioned v1 option gaps.
    assert!(file.skipped.len() <= 10, "too many skipped corpus jobs");

    let root = corpus_root();
    assert!(
        root.join("package.json").is_file(),
        "corpus fixture missing"
    );
    let root_str = root.to_string_lossy().into_owned();
    for pin in &file.records {
        let label = &pin.id;
        let mut options_json = resolve_placeholders(&pin.options, "<CORPUSROOT>", &root_str);
        if pin.mode == "dev" {
            options_json["dev"] = Value::Bool(true);
        }
        let options = CompilerOptions::from_json(&options_json)
            .unwrap_or_else(|e| panic!("{label}: options parse: {e}"))
            .resolve()
            .unwrap_or_else(|e| panic!("{label}: options resolve: {e}"));
        let filename = root.join(&pin.filename).to_string_lossy().into_owned();
        let result = compile_theming_module(&pin.source, &filename, &root_str, &options, &StdFs);
        if let Some(expected) = &pin.expected_error {
            let err = match result {
                Err(err) => err,
                Ok(_) => panic!("{label}: expected error {:?}", expected.message),
            };
            assert_error(label, &err, expected);
            continue;
        }
        let output = result.unwrap_or_else(|e| panic!("{label}: unexpected error: {e}"));
        assert_rules(
            label,
            &output.rules,
            pin.expected_rules.as_ref().expect("rules pin"),
        );
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CssCase {
    name: String,
    sources: Vec<CssSource>,
    options: Value,
    cfg: Value,
    css: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CssSource {
    filename: String,
    source: String,
}

#[test]
fn oracle_theming_css_end_to_end() {
    let raw = std::fs::read_to_string(pins_dir().join("theming-css.json"))
        .expect("run crates/stylex/conformance/src/gen-pins-theming.mjs to generate pins");
    let cases: Vec<CssCase> = serde_json::from_str(&raw).unwrap();
    assert!(cases.len() >= 10, "suspiciously few css cases");

    let fixroot = pins_dir().join("fixtures");
    let fixroot_str = fixroot.to_string_lossy().into_owned();
    let fs = BoundedFs {
        root: fixroot.clone(),
    };
    for case in &cases {
        let label = &case.name;
        let options_json = resolve_placeholders(&case.options, "<FIXROOT>", &fixroot_str);
        let options = CompilerOptions::from_json(&options_json)
            .unwrap_or_else(|e| panic!("{label}: options parse: {e}"))
            .resolve()
            .unwrap_or_else(|e| panic!("{label}: options resolve: {e}"));
        let mut rules: Vec<StylexRule> = Vec::new();
        for source in &case.sources {
            let filename = source.filename.replace("<FIXROOT>", &fixroot_str);
            let output =
                compile_theming_module(&source.source, &filename, &fixroot_str, &options, &fs)
                    .unwrap_or_else(|e| panic!("{label}: compile error: {e}"));
            rules.extend(output.rules);
        }
        let cfg = AssembleConfig::from_json(&case.cfg)
            .unwrap_or_else(|e| panic!("{label}: cfg parse: {e}"));
        let css = assemble(&rules, &cfg).unwrap_or_else(|e| panic!("{label}: assemble: {e}"));
        assert_eq!(css, case.css, "{label}: css mismatch");
    }
}
