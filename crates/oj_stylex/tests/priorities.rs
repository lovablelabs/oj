use oj_stylex::jsrt::{default_locale_cmp, locale_cmp};
use oj_stylex::shared::dashify::{dashify, sanitize_dev_class_name};
use oj_stylex::shared::priorities::get_priority;
use oj_stylex::shared::pseudo_sort::{sort_at_rules, sort_pseudos};
use std::cmp::Ordering;

fn read_pins(name: &str) -> serde_json::Value {
    let path = format!("{}/testdata/pins/{name}", env!("CARGO_MANIFEST_DIR"));
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("run conformance/src/gen-pins-priorities.mjs to generate {path}")
    });
    serde_json::from_str(&raw).unwrap()
}

#[derive(serde::Deserialize)]
struct PriorityCase {
    name: String,
    key: String,
    pseudos: Vec<String>,
    at_rules: Vec<String>,
    priority: f64,
}

#[derive(serde::Deserialize)]
struct SortCase {
    input: Vec<String>,
    expected: Vec<String>,
}

#[derive(serde::Deserialize)]
struct StrCase {
    input: String,
    expected: String,
}

#[test]
fn pinned_rule_priorities() {
    let pins = read_pins("priorities.json");
    let cases: Vec<PriorityCase> = serde_json::from_value(pins["cases"].clone()).unwrap();
    assert!(cases.len() >= 200, "suspiciously small pin set");
    for case in cases {
        // parity: babel-plugin src/shared/utils/generate-css-rule.js:106-110
        let computed = get_priority(&case.key)
            + case
                .pseudos
                .iter()
                .fold(0.0, |acc, p| acc + get_priority(p))
            + case
                .at_rules
                .iter()
                .fold(0.0, |acc, r| acc + get_priority(r));
        assert!(
            computed == case.priority,
            "{}: computed {computed}, oracle {}",
            case.name,
            case.priority
        );
    }
}

#[test]
fn pinned_pseudo_sort() {
    let pins = read_pins("priorities.json");
    let cases: Vec<SortCase> = serde_json::from_value(pins["pseudo_sort_cases"].clone()).unwrap();
    assert!(cases.len() >= 40);
    for case in cases {
        let input: Vec<&str> = case.input.iter().map(String::as_str).collect();
        assert_eq!(
            sort_pseudos(&input).unwrap(),
            case.expected,
            "input {:?}",
            case.input
        );
    }
}

#[test]
fn pinned_at_rule_sort() {
    let pins = read_pins("priorities.json");
    let cases: Vec<SortCase> = serde_json::from_value(pins["at_rule_sort_cases"].clone()).unwrap();
    assert!(cases.len() >= 5);
    for case in cases {
        let input: Vec<&str> = case.input.iter().map(String::as_str).collect();
        assert_eq!(
            sort_at_rules(&input),
            case.expected,
            "input {:?}",
            case.input
        );
    }
}

#[test]
fn pinned_dashify_and_sanitize() {
    let pins = read_pins("priorities.json");
    let dashify_cases: Vec<StrCase> =
        serde_json::from_value(pins["dashify_cases"].clone()).unwrap();
    for case in dashify_cases {
        assert_eq!(
            dashify(&case.input),
            case.expected,
            "dashify({:?})",
            case.input
        );
    }
    let sanitize_cases: Vec<StrCase> =
        serde_json::from_value(pins["sanitize_cases"].clone()).unwrap();
    for case in sanitize_cases {
        assert_eq!(
            sanitize_dev_class_name(&case.input),
            case.expected,
            "sanitize({:?})",
            case.input
        );
    }
}

#[test]
fn public_comparators_use_utf16_code_unit_order() {
    for (a, b) in [
        ("\"", "'"),
        ("B", "a"),
        ("z", "é"),
        ("\0", "a"),
        ("🐱", "\u{e000}"),
    ] {
        assert_eq!(default_locale_cmp(a, b), Ordering::Less);
        assert_eq!(locale_cmp(a, b), Ok(Ordering::Less));
        assert_eq!(default_locale_cmp(b, a), Ordering::Greater);
        assert_eq!(default_locale_cmp(a, a), Ordering::Equal);
    }
}
