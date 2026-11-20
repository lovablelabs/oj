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

enum Hardened {
    Rule(&'static str),
    Error(&'static str),
}

fn assert_hardened(source: &str, filename: &str, options: Value, want: &Hardened) {
    let root = corpus_root();
    let root_str = root.to_string_lossy().into_owned();
    let options = CompilerOptions::from_json(&options)
        .and_then(|o| o.resolve())
        .unwrap_or_else(|e| panic!("options: {}", e.message));
    let filename = root.join(filename).to_string_lossy().into_owned();
    let result = compile_module(source, &filename, &root_str, &options, &StdFs);
    match (result, want) {
        (Ok(output), Hardened::Rule(ltr)) => {
            let rules: Vec<&str> = output
                .creates
                .iter()
                .flat_map(|(_, out)| out.rules.iter())
                .map(|rule| &*rule.ltr)
                .collect();
            assert_eq!(rules, [*ltr], "{source}");
        }
        (Err(err), Hardened::Error(message)) => {
            assert_eq!(
                err.message.lines().next().unwrap_or_default(),
                *message,
                "{source}"
            );
        }
        (Ok(_), Hardened::Error(message)) => panic!("expected {message:?} for {source}"),
        (Err(err), Hardened::Rule(_)) => panic!("unexpected {:?} for {source}", err.message),
    }
}

/// The 0.19.2 evaluator hardening: only allowlisted static methods of
/// `String`/`Number`/`Math`/`Object`/`Array` run at compile time, the
/// prototype-chain properties are never read, and the function registries
/// only answer for names they own. Every row is the live 0.19.2 answer.
#[test]
fn evaluator_hardening_matches_0_19_2() {
    use Hardened::{Error, Rule};
    const BLOCKED: &str = "Access to this property is not allowed during compilation.";
    const CALL: &str = "Unsupported expression: CallExpression";
    const UNDEFINED: &str = "Referenced constant is not defined.";
    const X: Hardened = Rule(".x1qj0nkt{content:\"x\"}");
    const O: &str = "const o = { a: \"x\" };\n";
    const NESTED: &str = "const o = { a: { b: { c: \"x\" } } };\n";

    // (prelude, `content` value expression, outcome)
    let cases: &[(&str, &str, Hardened)] = &[
        // Allowlisted static methods run; data properties, mutators and
        // everything else on the five globals are no longer callable.
        (
            "",
            "String(Math.max(1, 2))",
            Rule(".xhs4kdw{content:\"2\"}"),
        ),
        ("", "Math.PI()", Error(CALL)),
        ("", "Math.random()", Error(CALL)),
        ("", "Math.sumPrecise([1, 2])", Error(CALL)),
        ("", "Math[\"max\"](1, 2)", Error(CALL)),
        (
            "",
            "Object.keys({ a: 1, b: 2 }).join(\" \")",
            Rule(".xybty1h{content:\"a b\"}"),
        ),
        (
            "",
            "Object.entries({ a: 1 }).join(\" \")",
            Rule(".x436hi9{content:\"a,1\"}"),
        ),
        (
            "",
            "String(Object.hasOwn({ a: 1 }, \"a\"))",
            Rule(".x1ez55b5{content:\"true\"}"),
        ),
        ("", "Object.assign({}, { a: 1 }).a", Error(CALL)),
        ("", "Object.freeze({ a: \"x\" }).a", Error(CALL)),
        ("", "Object.seal({ a: \"x\" }).a", Error(CALL)),
        ("", "Object.create(null)", Error(CALL)),
        ("", "Object.getPrototypeOf({})", Error(CALL)),
        ("", "Object.setPrototypeOf({}, null)", Error(CALL)),
        (
            "",
            "Object.defineProperty({}, \"a\", { value: 1 }).a",
            Error(CALL),
        ),
        ("", "Object.defineProperties({}, {})", Error(CALL)),
        (
            "",
            "Object.getOwnPropertyDescriptor({ a: \"x\" }, \"a\").value",
            Error(CALL),
        ),
        ("", "Object.preventExtensions({ a: \"x\" }).a", Error(CALL)),
        ("", "Object.prototype()", Error(CALL)),
        ("", "Object.length()", Error(CALL)),
        ("", "Object.name()", Error(CALL)),
        (
            "",
            "Array.of(\"a\", \"b\").join(\"-\")",
            Rule(".x1t42mo{content:\"a-b\"}"),
        ),
        ("", "Array.fromAsync([1])", Error(CALL)),
        ("", "Array.prototype()", Error(CALL)),
        (
            "",
            "String(Number.parseInt(\"12px\"))",
            Rule(".x1wkv6l8{content:\"12\"}"),
        ),
        ("", "Number.EPSILON()", Error(CALL)),
        (
            "",
            "String.fromCharCode(65, 66)",
            Rule(".xpf4ll6{content:\"AB\"}"),
        ),
        ("", "String.prototype()", Error(CALL)),
        ("", "String.name()", Error(CALL)),
        // A shadowing binding is ignored for allowlisted methods and is the
        // callee for the rest.
        (
            "const Math = { max: () => 9 };\n",
            "String(Math.max(1, 2))",
            Rule(".xhs4kdw{content:\"2\"}"),
        ),
        (
            "const Math = { PI: () => 4 };\n",
            "String(Math.PI())",
            Rule(".x1xiz97l{content:\"4\"}"),
        ),
        (
            "const Math = { random: () => 4 };\n",
            "String(Math.random())",
            Rule(".x1xiz97l{content:\"4\"}"),
        ),
        (
            "const Object = { assign: (a) => a };\n",
            "Object.assign(\"x\")",
            X,
        ),
        (
            "const Object = { freeze: (a) => a };\n",
            "Object[\"freeze\"](\"x\")",
            X,
        ),
        (
            "const Array = { prototype: (a) => a };\n",
            "Array.prototype(\"x\")",
            Error(BLOCKED),
        ),
        // Blocked property reads: inherited or own, static or computed.
        (O, "o.constructor", Error(BLOCKED)),
        (O, "o.__proto__", Error(BLOCKED)),
        (O, "o.prototype", Error(BLOCKED)),
        (
            "const o = { constructor: \"x\" };\n",
            "o.constructor",
            Error(BLOCKED),
        ),
        (
            "const o = { prototype: \"x\" };\n",
            "o[\"prototype\"]",
            Error(BLOCKED),
        ),
        (
            "const o = { a: \"x\" };\nconst k = \"constructor\";\n",
            "o[k]",
            Error(BLOCKED),
        ),
        (O, "o[\"__pro\" + \"to__\"]", Error(BLOCKED)),
        (O, "o[`constructor`]", Error(BLOCKED)),
        (NESTED, "o.a.constructor", Error(BLOCKED)),
        (NESTED, "o.constructor.name", Error(BLOCKED)),
        (NESTED, "o.a.b.__proto__.c", Error(BLOCKED)),
        (O, "o.missing.constructor", Error(BLOCKED)),
        ("", "\"abc\".constructor", Error(BLOCKED)),
        ("", "[1].constructor", Error(BLOCKED)),
        ("", "(5).constructor", Error(BLOCKED)),
        (
            "",
            "String(\"abc\".length)",
            Rule(".x1ih1qui{content:\"3\"}"),
        ),
        ("", "[\"x\"][0]", X),
        // The block is checked before the read, so it wins over a nullish
        // object; any other key still reports the TypeError.
        ("const n = null;\n", "n.constructor", Error(BLOCKED)),
        (
            "const n = null;\n",
            "n.foo",
            Error("Cannot read properties of null (reading 'foo')"),
        ),
        // Blocked method calls on literals and evaluated receivers.
        ("", "\"abc\".constructor(\"x\")", Error(BLOCKED)),
        ("", "\"abc\"[\"constructor\"](\"x\")", Error(BLOCKED)),
        ("", "(5).constructor(1)", Error(BLOCKED)),
        ("", "(5).__proto__()", Error(BLOCKED)),
        ("", "[1].constructor(2)", Error(BLOCKED)),
        (O, "o.constructor(\"x\")", Error(BLOCKED)),
        (O, "o[\"constructor\"](\"x\")", Error(BLOCKED)),
        (O, "o.constructor(missing)", Error(BLOCKED)),
        (
            "const o = { constructor: (a) => a };\n",
            "o.constructor(\"x\")",
            Error(BLOCKED),
        ),
        (
            "const o = { prototype: (a) => a };\n",
            "o.prototype(\"x\")",
            Error(BLOCKED),
        ),
        (
            "",
            "\"abc\".toUpperCase().constructor(\"x\")",
            Error(BLOCKED),
        ),
        (NESTED, "o.a.constructor.keys(o)", Error(CALL)),
        ("", "missing.constructor(\"x\")", Error(CALL)),
        // The block applies wherever the member is evaluated, and only then.
        (O, "typeof o.constructor", Error(BLOCKED)),
        (O, "`${o.constructor}`", Error(BLOCKED)),
        (O, "({ ...o.constructor }).a", Error(BLOCKED)),
        (
            O,
            "o.a ? \"y\" : o.constructor",
            Rule(".x1t2hdmn{content:\"y\"}"),
        ),
        (O, "o.a || o.constructor", X),
        // The function registries answer only for their own names: an
        // `Object.prototype` name is neither a registered identifier nor a
        // registered member of the stylex namespace.
        ("", "toString()", Error(UNDEFINED)),
        ("", "valueOf()", Error(UNDEFINED)),
        ("", "constructor(\"x\")", Error(UNDEFINED)),
        ("", "String(hasOwnProperty(\"a\"))", Error(UNDEFINED)),
        ("", "String(isPrototypeOf({}))", Error(UNDEFINED)),
        (
            "",
            "stylex.toString()",
            Rule(".x12ljtz1{content:\"[object Object]\"}"),
        ),
        (
            "",
            "String(stylex.hasOwnProperty(\"create\"))",
            Rule(".x9g66vw{content:\"false\"}"),
        ),
        ("", "stylex.constructor(\"x\")", Error(BLOCKED)),
        ("", "constructor.keys({ a: 1 })", Error(CALL)),
        ("", "toString.call(\"x\")", Error(CALL)),
        ("", "__proto__.toString()", Error(CALL)),
        // Computed keys: a symbol never reaches the lookup, everything else
        // is read through its string form.
        (O, "o[Symbol.iterator]", Error(UNDEFINED)),
        (
            "",
            "String(Object.getOwnPropertySymbols({}).length)",
            Rule(".xemru37{content:\"0\"}"),
        ),
        ("const o = { 1: \"x\" };\n", "o[1]", X),
        ("const o = { true: \"x\" };\n", "o[true]", X),
        ("const o = { null: \"x\" };\n", "o[null]", X),
        ("const o = { undefined: \"x\" };\n", "o[undefined]", X),
        ("const o = { \"a,b\": \"x\" };\n", "o[[\"a\", \"b\"]]", X),
        ("const o = { \"[object Object]\": \"x\" };\n", "o[{}]", X),
        ("const o = { \"1.5\": \"x\" };\n", "o[1.50]", X),
    ];
    for (prelude, expr, want) in cases {
        let source = format!(
            "import * as stylex from '@stylexjs/stylex';\n{prelude}export const styles = stylex.create({{ a: {{ content: {expr} }} }});\n"
        );
        assert_hardened(&source, "src/input.js", serde_json::json!({}), want);
    }

    // An imported variable group: its own blocked keys are refused, while a
    // deeper chain is still one flattened variable name.
    let themed: &[(&str, Hardened)] = &[
        ("MyTheme.constructor", Error(BLOCKED)),
        ("MyTheme[\"__proto__\"]", Error(BLOCKED)),
        ("MyTheme.prototype", Error(BLOCKED)),
        ("MyTheme[k]", Error(BLOCKED)),
        ("MyTheme.constructor(\"x\")", Error(BLOCKED)),
        ("holder.t.constructor", Error(BLOCKED)),
        ("holder.t.foreground.constructor", Error(BLOCKED)),
        (
            "holder.t.foreground",
            Rule(".xp5qx1w{color:var(--x1jqb1tb)}"),
        ),
        (
            "MyTheme.foreground.constructor",
            Rule(".x16z1d6t{color:var(--x1p8oo1z)}"),
        ),
        (
            "MyTheme.__proto__.foreground",
            Rule(".x1icq27w{color:var(--x10kn4nc)}"),
        ),
        ("MyTheme.toString()", Rule(".xh4l241{color:xjvfbhb}")),
    ];
    for (expr, want) in themed {
        let source = format!(
            "const k = 'constructor';\nimport stylex from 'stylex';\nimport {{ MyTheme }} from 'otherFile.stylex';\nconst holder = {{ t: MyTheme }};\nexport const styles = stylex.create({{ a: {{ color: {expr} }} }});\n"
        );
        let options = serde_json::json!({
            "unstable_moduleResolution": { "type": "haste" },
            "classNamePrefix": "x",
        });
        assert_hardened(&source, "test.js", options, want);
    }
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
