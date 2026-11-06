use oj_stylex::jsrt::{default_locale_cmp, locale_cmp, utf16_cmp};
use oj_stylex::shared::dashify::{dashify, sanitize_dev_class_name};
use oj_stylex::shared::priorities::get_priority;
use oj_stylex::shared::pseudo_sort::{sort_at_rules, sort_pseudos};
use std::cmp::Ordering;

fn read_pins(name: &str) -> serde_json::Value {
    let path = format!("{}/testdata/pins/{name}", env!("CARGO_MANIFEST_DIR"));
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("run conformance/src/gen-pins-{{priorities,collation}}.mjs to generate {path}")
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

fn sign_to_ordering(sign: i64) -> Ordering {
    match sign {
        0 => Ordering::Equal,
        s if s < 0 => Ordering::Less,
        _ => Ordering::Greater,
    }
}

#[test]
fn pinned_collation_pairs() {
    let pins = read_pins("collation.json");
    let pairs = pins["pairs"].as_array().unwrap();
    assert!(pairs.len() >= 9000);
    for pair in pairs {
        let a = pair[0].as_str().unwrap();
        let b = pair[1].as_str().unwrap();
        let expected = sign_to_ordering(pair[2].as_i64().unwrap());
        assert_eq!(
            locale_cmp(a, b).unwrap(),
            expected,
            "locale_cmp({a:?}, {b:?})"
        );
    }
}

// Reviewer repro pairs from finding r2#2 must hold through the public comparator.
#[test]
fn reviewer_collation_repros() {
    assert_eq!(
        default_locale_cmp("{content:\"é\"}", "{content:\"z\"}"),
        Ordering::Less
    );
    assert_eq!(default_locale_cmp("[data-é]", "[data-z]"), Ordering::Less);
}

#[test]
fn unverified_strings_use_flagged_utf16_fallback() {
    let pins = read_pins("collation.json");
    let pairs = pins["unverified_pairs"].as_array().unwrap();
    assert!(pairs.len() >= 30);
    for pair in pairs {
        let a = pair[0].as_str().unwrap();
        let b = pair[1].as_str().unwrap();
        assert!(
            locale_cmp(a, b).is_err(),
            "expected UnverifiedChar for ({a:?}, {b:?})"
        );
        // the shared fallback is deterministic UTF-16 order, both paths alike
        assert_eq!(default_locale_cmp(a, b), utf16_cmp(a, b));
    }
}

#[test]
fn pinned_collation_singletons() {
    let pins = read_pins("collation.json");
    let pinned: Vec<String> = serde_json::from_value(pins["sorted_singletons"].clone()).unwrap();
    let mut ours: Vec<String> = serde_json::from_value(pins["alphabet"].clone()).unwrap();
    ours.sort_by(|a, b| locale_cmp(a, b).unwrap());
    assert_eq!(ours, pinned);
}

#[test]
fn locale_cmp_refuses_unverified_chars() {
    // é/ø/😀 are verified since r4#4; ignorables and unpinned emoji are not
    assert_eq!(locale_cmp("caf\u{e9}", "cafe").unwrap(), Ordering::Greater);
    assert_eq!(
        locale_cmp("sm\u{f8}rrebr\u{f8}d", "smorrebrod").unwrap(),
        Ordering::Greater
    );
    assert_eq!(locale_cmp("a", "\u{1F600}").unwrap(), Ordering::Greater);
    assert!(locale_cmp("a", "\u{1F921}").is_err());
    assert!(locale_cmp("a\u{ad}", "aa").is_err());
    assert!(locale_cmp("stra\u{df}e", "strasse").is_err());
    assert_eq!(locale_cmp("", "").unwrap(), Ordering::Equal);
}
