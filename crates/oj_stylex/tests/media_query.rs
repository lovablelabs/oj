// Drives testdata/pins/media-query.json, generated from the live
// @stylexjs/babel-plugin 0.19.2 oracle by conformance/src/gen-pins-mq.mjs.

use oj_stylex::shared::media_query::{
    MediaQuery, MediaQueryError, WidthBoundKind, is_media_key, last_media_query_wins_transform,
    width_sort_bound,
};
use serde::Deserialize;

#[derive(Deserialize)]
struct Pins {
    single: Vec<SinglePin>,
    siblings: Vec<SiblingPin>,
    #[serde(rename = "topLevel")]
    top_level: Vec<SinglePin>,
    nested: Vec<NestedPin>,
}

#[derive(Deserialize)]
struct SinglePin {
    input: String,
    emitted: Option<String>,
    error: Option<String>,
    #[serde(rename = "refixError")]
    refix_error: Option<String>,
    refix: Option<String>,
}

#[derive(Deserialize)]
struct SiblingPin {
    inputs: Vec<String>,
    emitted: Option<Vec<String>>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct NestedPin {
    #[serde(rename = "outerInputs")]
    outer_inputs: Vec<String>,
    #[serde(rename = "innerInputs")]
    inner_inputs: Vec<String>,
    ltrs: Vec<String>,
}

fn load_pins() -> Pins {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/testdata/pins/media-query.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).expect("pin file present"))
        .expect("pin file parses")
}

fn assert_syntax_error(result: Result<MediaQuery, MediaQueryError>, expected: &str, ctx: &str) {
    match result {
        Err(MediaQueryError::Syntax(e)) => {
            assert_eq!(e.message, expected, "error message for {ctx}");
        }
        other => panic!("expected syntax error for {ctx}, got {other:?}"),
    }
}

#[test]
fn single_pins_match_oracle() {
    let pins = load_pins();
    let mut parsed_ok = 0usize;
    let mut errored = 0usize;
    for pin in &pins.single {
        if !is_media_key(&pin.input) {
            // never reaches the parser: the transform's startsWith filter
            // passes these through verbatim
            assert_eq!(pin.emitted.as_deref(), Some(pin.input.as_str()));
            continue;
        }
        match (&pin.emitted, &pin.error) {
            (Some(emitted), None) => {
                let parsed = MediaQuery::parse(&pin.input)
                    .unwrap_or_else(|e| panic!("parse failed for {:?}: {e:?}", pin.input));
                assert_eq!(&parsed.to_string(), emitted, "input {:?}", pin.input);
                parsed_ok += 1;
            }
            (None, Some(error)) => {
                assert_syntax_error(MediaQuery::parse(&pin.input), error, &pin.input);
                errored += 1;
            }
            _ => panic!("malformed pin for {:?}", pin.input),
        }
    }
    assert!(
        parsed_ok > 200,
        "expected wide corpus coverage, got {parsed_ok}"
    );
    assert!(
        errored > 30,
        "expected error pins to be exercised, got {errored}"
    );
}

#[test]
fn serialization_fixed_points_match_oracle() {
    let pins = load_pins();
    for pin in &pins.single {
        let Some(emitted) = &pin.emitted else {
            continue;
        };
        if !is_media_key(emitted) {
            continue;
        }
        if let Some(refix_error) = &pin.refix_error {
            assert_syntax_error(MediaQuery::parse(emitted), refix_error, emitted);
        } else {
            let expected = pin.refix.as_deref().unwrap_or(emitted);
            let reparsed = MediaQuery::parse(emitted)
                .unwrap_or_else(|e| panic!("re-parse failed for {emitted:?}: {e:?}"));
            assert_eq!(
                &reparsed.to_string(),
                expected,
                "fixed point of {emitted:?}"
            );
        }
    }
}

#[test]
fn sibling_pins_match_oracle() {
    let pins = load_pins();
    for pin in &pins.siblings {
        assert!(pin.inputs.iter().all(|k| is_media_key(k)));
        match (&pin.emitted, &pin.error) {
            (Some(emitted), None) => {
                let rewritten = last_media_query_wins_transform(&pin.inputs)
                    .unwrap_or_else(|e| panic!("transform failed for {:?}: {e:?}", pin.inputs));
                assert_eq!(&rewritten, emitted, "inputs {:?}", pin.inputs);
            }
            (None, Some(error)) => match last_media_query_wins_transform(&pin.inputs) {
                Err(MediaQueryError::Syntax(e)) => assert_eq!(&e.message, error),
                other => panic!("expected error for {:?}, got {other:?}", pin.inputs),
            },
            _ => panic!("malformed sibling pin {:?}", pin.inputs),
        }
    }
}

#[test]
fn media_query_0_19_2_fixes_and_width_bounds() {
    // #1652: single px min-/max-width bound extraction for stylesheet sort.
    for (prelude, expected) in [
        (
            "@media (min-width: 600px)",
            Some((WidthBoundKind::Min, 600.0)),
        ),
        (
            "@media (max-width: 599.98px)",
            Some((WidthBoundKind::Max, 599.98)),
        ),
        (
            "@media (width >= 400px)",
            Some((WidthBoundKind::Min, 400.0)),
        ),
        (
            "@media (width <= 800px)",
            Some((WidthBoundKind::Max, 800.0)),
        ),
        ("@media (min-width: 40em)", None),
        ("@media (min-width: 600px) and (max-width: 900px)", None),
        ("@media not (min-width: 600px)", None),
        (
            "@media screen and (min-width: 600px)",
            Some((WidthBoundKind::Min, 600.0)),
        ),
    ] {
        let got = width_sort_bound(prelude).map(|b| (b.kind, b.value));
        assert_eq!(got, expected, "prelude {prelude:?}");
    }

    // #1858: 0.02px max-width epsilon closes gaps between sibling media queries.
    let rewritten = last_media_query_wins_transform(&[
        "@media (min-width: 600px)".to_string(),
        "@media (min-width: 900px)".to_string(),
    ])
    .expect("epsilon siblings");
    assert_eq!(
        rewritten,
        vec![
            "@media (min-width: 600px) and (max-width: 899.98px)",
            "@media (min-width: 900px)",
        ]
    );

    // #1888 & #2001: media-type resolution and preserving leading `not`.
    assert_eq!(
        MediaQuery::parse("@media not screen and (min-width: 600px)")
            .unwrap()
            .to_string(),
        "@media not screen and (min-width: 600px)"
    );
    let types = last_media_query_wins_transform(&[
        "@media screen and (min-width: 600px)".to_string(),
        "@media print".to_string(),
    ])
    .expect("media types");
    assert_eq!(types[1], "@media print");
}

#[test]
fn top_level_condition_first_keys_stay_verbatim() {
    // depth-0 keys never reach the transform, malformed ones included
    let pins = load_pins();
    for pin in &pins.top_level {
        assert_eq!(pin.emitted.as_deref(), Some(pin.input.as_str()));
    }
}

#[test]
fn nested_media_groups_transform_independently() {
    let pins = load_pins();
    for pin in &pins.nested {
        let outer = last_media_query_wins_transform(&pin.outer_inputs).expect("outer transforms");
        let inner = last_media_query_wins_transform(&pin.inner_inputs).expect("inner transforms");
        assert!(pin.ltrs[0].starts_with(&format!("{}{{{}{{", outer[0], inner[0])));
        assert!(pin.ltrs[1].starts_with(&format!("{}{{{}{{", outer[0], inner[1])));
        assert!(pin.ltrs[2].starts_with(&format!("{}{{", outer[1])));
    }
}

#[test]
fn transform_edge_shapes() {
    assert_eq!(
        last_media_query_wins_transform(&[]).expect("empty input"),
        Vec::<String>::new()
    );
    let single = vec!["@media (min-width:600px)".to_string()];
    assert_eq!(
        last_media_query_wins_transform(&single).expect("single key"),
        vec!["@media (min-width: 600px)".to_string()]
    );
}

#[test]
fn unverified_gate_refuses_unpinned_forms() {
    for input in [
        "@media (min-w\\69dth: 600px)",
        "@media (min-width: 600px) and (müde: 1px)",
    ] {
        match MediaQuery::parse(input) {
            Err(MediaQueryError::Unverified { .. }) => {}
            other => panic!("expected Unverified for {input:?}, got {other:?}"),
        }
    }
}

#[test]
fn transform_repeats_are_stable_and_errors_are_not_memoized() {
    let keys = vec![
        "@media (min-width: 900px)".to_string(),
        "@media (min-width: 700px)".to_string(),
    ];
    let first = last_media_query_wins_transform(&keys).expect("valid siblings");
    assert_eq!(
        first,
        last_media_query_wins_transform(&keys).expect("memo hit")
    );
    let bad = vec![
        "@media (min-width: 600px)".to_string(),
        "@media (min-width foo)".to_string(),
    ];
    for _ in 0..2 {
        assert!(matches!(
            last_media_query_wins_transform(&bad),
            Err(MediaQueryError::Syntax(_))
        ));
    }
}

#[test]
fn many_sibling_screen_media_queries_prune_conflicting_branches() {
    let keys: Vec<String> = (1..=18)
        .map(|i| format!("@media screen and (min-width: {}px)", i * 100))
        .collect();
    let started = std::time::Instant::now();
    let rewritten = last_media_query_wins_transform(&keys).expect("18 screen siblings");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "18 sibling screen queries should complete in milliseconds, took {:?}",
        started.elapsed()
    );
    assert_eq!(rewritten.len(), 18);
    for (i, got) in rewritten.iter().enumerate() {
        let mut expected = format!("@media screen and (min-width: {}px)", (i + 1) * 100);
        for j in (i + 1)..18 {
            expected.push_str(&format!(" and (not (min-width: {}px))", (j + 1) * 100));
        }
        assert_eq!(got, &expected, "sibling {i}");
    }

    // Negated media-type contradictions must not be pruned inside `not (...)`.
    assert_eq!(
        MediaQuery::parse("@media not (screen and print)")
            .unwrap()
            .to_string(),
        "@media not screen, not print"
    );
    assert_eq!(
        MediaQuery::parse("@media (not (screen and (not (screen and (min-width: 200px)))))")
            .unwrap()
            .to_string(),
        "@media print, print and (min-width: 200px), screen and (min-width: 200px)"
    );

    // Direct `resolve_media_types` And/Or distribution pruning with 18 `Or` terms.
    let mut query = String::from("@media screen");
    for i in 1..=18 {
        query.push_str(&format!(" and (print or (min-width: {}px))", i * 100));
    }
    let started = std::time::Instant::now();
    let parsed = MediaQuery::parse(&query).expect("18 Or terms");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "18 And/Or terms should complete in milliseconds, took {:?}",
        started.elapsed()
    );
    let mut expected = String::from("@media screen");
    for i in (1..=18).rev() {
        expected.push_str(&format!(" and (min-width: {}px)", i * 100));
    }
    assert_eq!(parsed.to_string(), expected);
}
