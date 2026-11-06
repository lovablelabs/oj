use oj_stylex::hash::hash;
use oj_stylex::shared::normalize_value::CssValueError;
use oj_stylex::shared::transform_value::{transform_value_num, transform_value_str};

#[derive(serde::Deserialize)]
struct Pin {
    property: String,
    input: Input,
    expect: Option<Expect>,
    expect_error: Option<ExpectError>,
}

#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Input {
    String { value: String },
    Number { value: String },
    Array { values: Vec<Element> },
}

#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Element {
    String { value: String },
    Number { value: String },
}

#[derive(serde::Deserialize)]
struct Expect {
    decl_prop: String,
    class_name: String,
    value: Option<String>,
    decl_body: Option<String>,
}

#[derive(serde::Deserialize)]
struct ExpectError {
    kind: String,
    message: String,
}

fn load_pins() -> Vec<Pin> {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/testdata/pins/values.json"
    ))
    .expect("run crates/stylex/conformance/src/gen-pins-values.mjs to generate pins");
    serde_json::from_str(&raw).unwrap()
}

fn transform_element(property: &str, element: &Element) -> Result<String, CssValueError> {
    match element {
        Element::String { value } => transform_value_str(property, value, false),
        Element::Number { value } => {
            let n: f64 = value
                .parse()
                .unwrap_or_else(|_| panic!("bad number literal {value:?}"));
            transform_value_num(property, n, false)
        }
    }
}

fn class_name_for(decl_prop: &str, value: &str) -> String {
    format!("x{}", hash(&format!("<>{decl_prop}{value}null")))
}

#[test]
fn oracle_value_pins() {
    let pins = load_pins();
    assert!(pins.len() >= 150, "suspiciously small pin set");
    let mut error_pins = 0;
    for pin in &pins {
        let label = format!("{} <- {:?}", pin.property, describe(&pin.input));
        match (&pin.input, &pin.expect, &pin.expect_error) {
            (Input::Array { values }, Some(expect), None) => {
                let parts: Vec<String> = values
                    .iter()
                    .map(|e| {
                        transform_element(&pin.property, e)
                            .unwrap_or_else(|e| panic!("{label}: {e}"))
                    })
                    .collect();
                let body = parts
                    .iter()
                    .map(|v| format!("{}:{}", expect.decl_prop, v))
                    .collect::<Vec<_>>()
                    .join(";");
                assert_eq!(Some(&body), expect.decl_body.as_ref(), "{label}: decl body");
                let joined = parts.join(", ");
                assert_eq!(
                    class_name_for(&expect.decl_prop, &joined),
                    expect.class_name,
                    "{label}: className"
                );
            }
            (input, Some(expect), None) => {
                let result = match input {
                    Input::String { value } => transform_value_str(&pin.property, value, false),
                    Input::Number { value } => {
                        let n: f64 = value.parse().expect("bad number literal");
                        transform_value_num(&pin.property, n, false)
                    }
                    Input::Array { .. } => unreachable!(),
                };
                let got = result.unwrap_or_else(|e| panic!("{label}: unexpected error {e}"));
                let want = expect.value.as_ref().expect("non-array pin carries value");
                assert_eq!(&got, want, "{label}: declaration value");
                assert_eq!(
                    class_name_for(&expect.decl_prop, &got),
                    expect.class_name,
                    "{label}: className"
                );
            }
            (input, None, Some(err)) => {
                error_pins += 1;
                let value = match input {
                    Input::String { value } => value,
                    _ => panic!("{label}: error pins are string inputs"),
                };
                let got = transform_value_str(&pin.property, value, false)
                    .expect_err(&format!("{label}: expected an error"));
                match err.kind.as_str() {
                    "unclosed-function" => {
                        assert_eq!(got, CssValueError::UnclosedFunction, "{label}");
                        assert_eq!(got.to_string(), err.message, "{label}: message");
                    }
                    "unclosed-string" => {
                        assert_eq!(got, CssValueError::UnclosedString, "{label}");
                        assert_eq!(got.to_string(), err.message, "{label}: message");
                    }
                    // upstream TypeErrors (empty/whitespace-only) map to our
                    // structured EmptyValue; message intentionally differs
                    "other" => assert_eq!(got, CssValueError::EmptyValue, "{label}"),
                    other => panic!("{label}: unknown error kind {other}"),
                }
            }
            _ => panic!("{label}: pin needs exactly one of expect/expect_error"),
        }
    }
    assert!(error_pins >= 5, "error pins missing from the set");
}

fn describe(input: &Input) -> String {
    match input {
        Input::String { value } => format!("string {value:?}"),
        Input::Number { value } => format!("number {value}"),
        Input::Array { values } => format!("array of {}", values.len()),
    }
}
