//! Oracle-pinned end-to-end tests for the eval front end: oxc parse → imports
//! + state + evaluator → compile_namespaces, vs @stylexjs/babel-plugin 0.19.0.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_ast::ast::{CallExpression, Declaration, Expression, Statement};
use oxc_parser::Parser;
use oxc_span::SourceType;
use serde_json::Value;

use oj_stylex::errors::{ErrorCode, StylexError};
use oj_stylex::eval::functions::StubCallables;
use oj_stylex::eval::value::{EvalValue, JsObjectMap};
use oj_stylex::eval::{Evaluator, evaluate_stylex_create_arg, unwrap_parens, validate_create_arg};
use oj_stylex::imports::StylexNamedImport;
use oj_stylex::module_resolution::{BoundedFs, FsProvider, StdFs, canonical_file_path};
use oj_stylex::options::{CompilerOptions, ResolvedOptions};
use oj_stylex::rules::StylexRule;
use oj_stylex::shared::create::{CreateContext, CreateOutput, compile_namespaces};
use oj_stylex::state::{CompileState, line_of_offset};

fn fixtures_root() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/testdata/pins/eval/fixtures"
    ))
}

fn corpus_root() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/conformance/corpus/upstream/fixtures/default"
    ))
}

// Tagged pin encoding: {"$obj": [[k, v], ...]} preserves JS ownKeys order
// through serde_json's alphabetizing Value maps.
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
    rust_expect: Option<RustExpect>,
    #[serde(default)]
    expected_error: Option<ExpectedError>,
    #[serde(default)]
    expected_rules: Option<Vec<ExpectedRule>>,
    #[serde(default)]
    expected_treeshake: Option<Vec<String>>,
    #[serde(default)]
    expected_compiled: Option<Value>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RustExpect {
    code: String,
    #[serde(default)]
    message: Option<String>,
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

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CorpusPin {
    id: String,
    filename: String,
    source: String,
    options: Value,
    #[serde(default)]
    expected_error: Option<ExpectedError>,
    #[serde(default)]
    expected_rules: Option<Vec<ExpectedRule>>,
}

struct ModuleResult {
    /// (export name pairing key, compiled output) per create call, in order.
    creates: Vec<(Option<String>, CreateOutput)>,
    treeshake: Vec<String>,
}

fn find_create_call<'a>(
    state: &CompileState<'_>,
    expr: &'a Expression<'a>,
) -> Option<&'a CallExpression<'a>> {
    let Expression::CallExpression(call) = unwrap_parens(expr) else {
        return None;
    };
    match unwrap_parens(&call.callee) {
        Expression::Identifier(id)
            if state.imports.named_binding(&id.name) == Some(StylexNamedImport::Create) =>
        {
            Some(call)
        }
        Expression::StaticMemberExpression(member) if member.property.name == "create" => {
            match &member.object {
                Expression::Identifier(object)
                    if state.imports.is_stylex_namespace(&object.name) =>
                {
                    Some(call)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn compile_module(
    source: &str,
    filename: &str,
    cwd: &str,
    options: &ResolvedOptions,
    fs: &dyn FsProvider,
) -> Result<ModuleResult, StylexError> {
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

    let mut calls: Vec<(Option<String>, &CallExpression<'_>)> = Vec::new();
    for statement in &parsed.program.body {
        let declaration = match statement {
            Statement::VariableDeclaration(decl) => Some(&**decl),
            Statement::ExportDeclaration(export) => match &export.declaration {
                Declaration::VariableDeclaration(decl) => Some(&**decl),
                _ => None,
            },
            Statement::ExportDefaultDeclaration(export) => {
                if let Some(expr) = export.declaration.as_expression()
                    && let Some(call) = find_create_call(&state, expr)
                {
                    calls.push((Some("default".to_string()), call));
                }
                None
            }
            _ => None,
        };
        let Some(declaration) = declaration else {
            continue;
        };
        for declarator in &declaration.declarations {
            if let Some(init) = &declarator.init
                && let Some(call) = find_create_call(&state, init)
            {
                let var_name = declarator
                    .id
                    .get_binding_identifier()
                    .map(|id| id.name.to_string());
                calls.push((var_name, call));
            }
        }
    }

    let mut creates = Vec::new();
    for (var_name, call) in calls {
        let object = validate_create_arg(call)?;
        let arg = {
            let mut evaluator = Evaluator::for_create(&mut state, fs, &StubCallables);
            evaluate_stylex_create_arg(&mut evaluator, object)?
        };
        let mut namespace_lines: BTreeMap<String, u32> = BTreeMap::new();
        for (key, offset) in &arg.key_spans {
            namespace_lines.insert(key.clone(), line_of_offset(source, *offset));
        }
        let file_package = fs
            .nearest_package(Path::new(filename))
            .map(|(name, dir)| (name, dir.to_string_lossy().into_owned()));
        let cwd_package_name = if file_package.is_some() {
            fs.nearest_package(Path::new(cwd)).map(|(name, _)| name)
        } else {
            None
        };
        let ctx = CreateContext {
            options,
            filename: Some(filename.to_string()),
            cwd: cwd.to_string(),
            var_name: var_name.clone().filter(|v| v != "default"),
            namespace_lines: Some(namespace_lines),
            file_package,
            cwd_package_name,
        };
        creates.push((var_name, compile_namespaces(&arg.namespaces, &ctx, false)?));
    }
    Ok(ModuleResult {
        creates,
        treeshake: state
            .treeshake_imports
            .iter()
            .map(|t| t.specifier.clone())
            .collect(),
    })
}

fn code_name(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::NonStaticValue => "NonStaticValue",
        ErrorCode::NoObjectSpreads => "NoObjectSpreads",
        ErrorCode::NonStyleObject => "NonStyleObject",
        ErrorCode::IllegalArgumentLength => "IllegalArgumentLength",
        ErrorCode::IllegalPropValue => "IllegalPropValue",
        ErrorCode::IllegalNamespaceValue => "IllegalNamespaceValue",
        ErrorCode::IllegalPropArrayValue => "IllegalPropArrayValue",
        ErrorCode::EmptyValue => "EmptyValue",
        ErrorCode::UnsupportedApi => "UnsupportedApi",
        ErrorCode::DuplicateConditional => "DuplicateConditional",
        ErrorCode::InvalidPseudoOrAtRule => "InvalidPseudoOrAtRule",
        ErrorCode::InvalidMediaQuerySyntax => "InvalidMediaQuerySyntax",
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

fn assert_rules(label: &str, actual: &[&StylexRule], expected: &[ExpectedRule]) {
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

#[test]
fn oracle_eval_pins() {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/testdata/pins/eval/eval.json"
    ))
    .expect("run crates/stylex/conformance/src/gen-pins-eval.mjs to generate pins");
    let pins: Vec<Pin> = serde_json::from_str(&raw).unwrap();
    assert!(
        pins.len() >= 80,
        "suspiciously small pin set: {}",
        pins.len()
    );

    let fixroot = fixtures_root();
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
        let result = compile_module(&pin.source, &filename, &cwd, &options, &fs);

        if let Some(expect) = &pin.rust_expect {
            // Documented v1 divergence: the oracle succeeds, we reject.
            let err = match result {
                Err(err) => err,
                Ok(_) => panic!("{label}: expected divergence error {}", expect.code),
            };
            assert_eq!(code_name(err.code), expect.code, "{label}: divergence code");
            if let Some(message) = &expect.message {
                assert_eq!(
                    err.message.lines().next().unwrap_or_default(),
                    message,
                    "{label}: divergence message"
                );
            }
            continue;
        }

        if let Some(expected) = &pin.expected_error {
            error_pins += 1;
            let err = match result {
                Err(err) => err,
                Ok(_) => panic!("{label}: expected error {:?}", expected.message),
            };
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
            continue;
        }

        let output = result.unwrap_or_else(|e| panic!("{label}: unexpected error: {e}"));
        let rules: Vec<&StylexRule> = output
            .creates
            .iter()
            .flat_map(|(_, out)| out.rules.iter())
            .collect();
        assert_rules(
            label,
            &rules,
            pin.expected_rules.as_ref().expect("rules pin"),
        );

        // The oracle pin records file order; recording order is evaluation
        // order, so compare as sets.
        let mut ours = output.treeshake.clone();
        let mut theirs = pin.expected_treeshake.clone().expect("treeshake pin");
        ours.sort();
        theirs.sort();
        assert_eq!(ours, theirs, "{label}: treeshake specifiers");

        if let Some(expected_compiled) = &pin.expected_compiled {
            let mut compiled = JsObjectMap::new();
            for (var_name, out) in &output.creates {
                let export_name = var_name.clone().unwrap_or_else(|| "default".to_string());
                compiled.insert(export_name, EvalValue::Obj(out.compiled.clone()));
            }
            let expected_keys: Vec<&str> = pin.exports.iter().map(String::as_str).collect();
            let actual_keys: Vec<&str> = compiled.keys().collect();
            assert_eq!(actual_keys, expected_keys, "{label}: compiled export set");
            assert_eq!(
                &encode(&EvalValue::Obj(compiled.into())),
                expected_compiled,
                "{label}: compiled mismatch"
            );
        }
    }
    assert!(
        error_pins >= 30,
        "suspiciously few error pins: {error_pins}"
    );
}

#[test]
fn oracle_corpus_cross_check() {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/testdata/pins/eval/corpus.json"
    ))
    .expect("run crates/stylex/conformance/src/gen-pins-eval.mjs to generate pins");
    let pins: Vec<CorpusPin> = serde_json::from_str(&raw).unwrap();
    assert!(pins.len() >= 10, "suspiciously small corpus set");

    let root = corpus_root();
    assert!(
        root.join("package.json").is_file(),
        "corpus fixture missing"
    );
    let root_str = root.to_string_lossy().into_owned();
    for pin in &pins {
        let label = &pin.id;
        let options_json = resolve_placeholders(&pin.options, "<CORPUSROOT>", &root_str);
        let options = CompilerOptions::from_json(&options_json)
            .unwrap_or_else(|e| panic!("{label}: options parse: {e}"))
            .resolve()
            .unwrap_or_else(|e| panic!("{label}: options resolve: {e}"));
        let filename = root.join(&pin.filename).to_string_lossy().into_owned();
        let result = compile_module(&pin.source, &filename, &root_str, &options, &StdFs);
        if let Some(expected) = &pin.expected_error {
            let err = match result {
                Err(err) => err,
                Ok(_) => panic!("{label}: expected error {:?}", expected.message),
            };
            assert_eq!(code_name(err.code), expected.code, "{label}: error code");
            continue;
        }
        let output = result.unwrap_or_else(|e| panic!("{label}: unexpected error: {e}"));
        let rules: Vec<&StylexRule> = output
            .creates
            .iter()
            .flat_map(|(_, out)| out.rules.iter())
            .collect();
        assert_rules(
            label,
            &rules,
            pin.expected_rules.as_ref().expect("rules pin"),
        );
    }
}

/// Every evaluation-import corpus job but the one commonJS case needs the
/// haste resolver; their source shapes are re-pinned as commonJS eval pins.
#[test]
fn evaluation_import_corpus_resolves_now_that_its_options_ship() {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/conformance/corpus/upstream/evaluation-import-test.jobs.json"
    ))
    .expect("mined corpus present");
    let jobs: Value = serde_json::from_str(&raw).unwrap();
    let jobs = jobs["jobs"].as_array().expect("jobs array");
    assert!(!jobs.is_empty());
    // This corpus needed haste and runtimeInjection, which both now ship; it
    // guards the other direction, that neither regresses to a hard error.
    let mut injecting = 0;
    for job in jobs {
        let id = job["id"].as_str().unwrap_or("?");
        let options = CompilerOptions::from_json(&job["options"])
            .and_then(|o| o.resolve())
            .unwrap_or_else(|err| panic!("{id} no longer resolves: {}", err.message));
        if job["options"]["runtimeInjection"] != Value::Null {
            assert!(options.runtime_injection.is_some(), "{id}");
            injecting += 1;
        }
    }
    assert!(injecting > 0, "corpus no longer exercises runtimeInjection");
}

#[test]
fn canonical_name_fallback_branches() {
    let fixroot = fixtures_root();
    let fs = BoundedFs {
        root: fixroot.clone(),
    };
    let theme = fixroot.join("bare/src/plain.stylex.ts");
    // No package.json inside the boundary: rootDir-relative, then the
    // _unknown_path_ basename fallback.
    assert_eq!(
        canonical_file_path(&fs, &theme, Some(&fixroot.join("bare"))),
        "src/plain.stylex.ts"
    );
    assert_eq!(
        canonical_file_path(&fs, &theme, None),
        "_unknown_path_:plain.stylex.ts"
    );
    let in_pkg = fixroot.join("pkg/src/tokens.stylex.ts");
    assert_eq!(
        canonical_file_path(&fs, &in_pkg, None),
        "eval-pins-pkg:src/tokens.stylex.ts"
    );
    let noname = fixroot.join("noname/tokens.stylex.ts");
    assert_eq!(
        canonical_file_path(&fs, &noname, None),
        "undefined:tokens.stylex.ts"
    );
}
