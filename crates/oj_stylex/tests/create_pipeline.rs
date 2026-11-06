use oj_stylex::errors::ErrorCode;
use oj_stylex::eval::value::{EvalValue, JsObjectMap};
use oj_stylex::options::{CompilerOptions, PropertyValidationMode, ResolvedOptions};
use oj_stylex::shared::create::{CreateContext, compile_namespaces};
use serde_json::Value;
use std::collections::BTreeMap;

// Tagged pin encoding: {"$obj": [[k, v], ...]} preserves JS ownKeys order
// through serde_json's alphabetizing Value maps.
fn decode(value: &Value) -> EvalValue {
    match value {
        Value::Null => EvalValue::Null,
        Value::Bool(b) => EvalValue::Bool(*b),
        Value::Number(n) => EvalValue::Num(n.as_f64().expect("pin numbers are f64")),
        Value::String(s) => EvalValue::Str(s.clone()),
        Value::Array(items) => EvalValue::Arr(items.iter().map(decode).collect()),
        Value::Object(map) => {
            let pairs = map
                .get("$obj")
                .and_then(Value::as_array)
                .expect("pin objects use the $obj tagged encoding");
            let mut obj = JsObjectMap::new();
            for pair in pairs {
                let [k, v] = pair.as_array().expect("pair").as_slice() else {
                    panic!("$obj pairs are [key, value]");
                };
                obj.insert(k.as_str().expect("string key").to_string(), decode(v));
            }
            EvalValue::Obj(obj.into())
        }
    }
}

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
    namespaces: Value,
    options: Value,
    filename: String,
    var_name: Option<String>,
    lines: BTreeMap<String, u32>,
    file_package: Option<(String, String)>,
    cwd_package_name: Option<String>,
    #[serde(default)]
    expected_error: Option<ExpectedError>,
    #[serde(default)]
    expected_compiled: Option<Value>,
    #[serde(default)]
    expected_rules: Option<Vec<ExpectedRule>>,
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

fn load_pins() -> Vec<Pin> {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/testdata/pins/create/create.json"
    ))
    .expect("run crates/stylex/conformance/src/gen-pins-create.mjs to generate pins");
    serde_json::from_str(&raw).unwrap()
}

fn resolve_pin_options(options: &Value) -> ResolvedOptions {
    let mut options = options.clone();
    // 'throw' resolves to UnsupportedOption in v1; pins reach it directly.
    let validation_mode = options
        .as_object_mut()
        .and_then(|map| map.remove("propertyValidationMode"));
    let mut resolved = CompilerOptions::from_json(&options)
        .expect("pin options parse")
        .resolve()
        .expect("pin options resolve");
    if let Some(mode) = validation_mode {
        assert_eq!(
            mode.as_str(),
            Some("throw"),
            "unexpected pin validation mode"
        );
        resolved.property_validation_mode = PropertyValidationMode::Throw;
    }
    resolved
}

fn code_name(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::IllegalNamespaceValue => "IllegalNamespaceValue",
        ErrorCode::IllegalPropValue => "IllegalPropValue",
        ErrorCode::IllegalPropArrayValue => "IllegalPropArrayValue",
        ErrorCode::DuplicateConditional => "DuplicateConditional",
        ErrorCode::InvalidPseudoOrAtRule => "InvalidPseudoOrAtRule",
        ErrorCode::InvalidMediaQuerySyntax => "InvalidMediaQuerySyntax",
        ErrorCode::UnclosedFunction => "UnclosedFunction",
        ErrorCode::UnclosedString => "UnclosedString",
        ErrorCode::NonContiguousVars => "NonContiguousVars",
        ErrorCode::ShorthandFallback => "ShorthandFallback",
        ErrorCode::BannedShorthand => "BannedShorthand",
        ErrorCode::EmptyValue => "EmptyValue",
        _ => "other",
    }
}

#[test]
fn oracle_create_pins() {
    let pins = load_pins();
    assert!(
        pins.len() >= 250,
        "suspiciously small pin set: {}",
        pins.len()
    );
    let mut error_pins = 0;
    for pin in &pins {
        let label = &pin.name;
        let options = resolve_pin_options(&pin.options);
        let EvalValue::Obj(namespaces) = decode(&pin.namespaces) else {
            panic!("{label}: namespaces must decode to an object");
        };
        let ctx = CreateContext {
            options: &options,
            filename: Some(pin.filename.clone()),
            cwd: String::new(),
            var_name: pin.var_name.clone(),
            namespace_lines: Some(pin.lines.clone()),
            file_package: pin.file_package.clone(),
            cwd_package_name: pin.cwd_package_name.clone(),
        };
        let result = compile_namespaces(&namespaces, &ctx, false);
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
                assert_eq!(err.message, expected.message, "{label}: error message");
            }
            continue;
        }
        let output = result.unwrap_or_else(|e| panic!("{label}: unexpected error: {e}"));
        let expected_compiled = pin.expected_compiled.as_ref().expect("compiled pin");
        assert_eq!(
            &encode(&EvalValue::Obj(output.compiled)),
            expected_compiled,
            "{label}: compiled namespace mismatch"
        );
        let expected_rules = pin.expected_rules.as_ref().expect("rules pin");
        assert_eq!(
            output.rules.len(),
            expected_rules.len(),
            "{label}: rule count"
        );
        for (rule, expected) in output.rules.iter().zip(expected_rules) {
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
            assert_eq!(rule.const_key, None, "{label}: constKey");
            assert_eq!(rule.const_val, None, "{label}: constVal");
        }
    }
    assert!(
        error_pins >= 30,
        "suspiciously few error pins: {error_pins}"
    );
}
