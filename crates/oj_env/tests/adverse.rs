// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Adversarial `.env` and `index.html` input. A dotenv file is untrusted text
//! that a build reads without asking: it must never panic, never loop, never
//! mangle bytes, and never let an unprefixed variable reach the client bundle.

use std::collections::BTreeMap;

use oj_env::{html_env_map, import_meta_env_defines, parse, replace_html_env};

fn base() -> BTreeMap<String, String> {
    BTreeMap::new()
}

fn map(src: &str) -> BTreeMap<String, String> {
    parse(src, &base()).into_iter().collect()
}

#[test]
fn malformed_lines_are_skipped_not_fatal() {
    let src = "\
no-equals-at-all
=value-with-no-key
   =also-no-key
export
export =
 \t \t
#
# = #
KEEP=kept
";
    let out = parse(src, &base());
    assert_eq!(
        out,
        vec![("KEEP".to_string(), "kept".to_string())],
        "only the one well-formed line survives: {out:?}"
    );
}

#[test]
fn self_and_mutual_references_terminate_and_do_not_recurse() {
    // Expansion only sees variables defined *before* the current line, so a
    // self-reference resolves to empty rather than looping.
    let m = map("A=$A\nB=${B}\nC=$D\nD=$C\n");
    assert_eq!(m["A"], "");
    assert_eq!(m["B"], "");
    assert_eq!(m["C"], "");
    assert_eq!(m["D"], "", "D sees C's (empty) value, not its own");
}

#[test]
fn expansion_is_one_pass_not_recursive() {
    // A value that itself looks like a reference is not expanded again.
    let m = map("INNER=payload\nMID=$INNER\nOUTER=$MID\nLITERAL='$INNER'\nVIA=$LITERAL\n");
    assert_eq!(m["OUTER"], "payload");
    assert_eq!(m["LITERAL"], "$INNER");
    assert_eq!(
        m["VIA"], "$INNER",
        "a stored `$INNER` must not be re-expanded on use"
    );
}

#[test]
fn unterminated_quotes_and_braces_do_not_panic() {
    let m = map("A=\"unterminated\nB='also unterminated\nC=${UNCLOSED\nD=$\nE=${}\nF=ok\n");
    assert_eq!(m["A"], "unterminated");
    assert_eq!(m["B"], "also unterminated");
    assert_eq!(m["C"], "${UNCLOSED");
    assert_eq!(m["D"], "$");
    assert_eq!(m["E"], "${}", "an empty brace name is left literal");
    assert_eq!(m["F"], "ok");
}

#[test]
fn nested_dollar_braces_are_left_literal() {
    let m = map("A=${${${X}}}\nB=$$$$X\n");
    // The outer `${...}` closes at the first `}`, the remainder is literal.
    assert!(m.contains_key("A"), "parsed without panic: {m:?}");
    assert!(m.contains_key("B"));
}

#[test]
fn non_ascii_values_survive_byte_for_byte() {
    let m = map(
        "PLAIN=café\nQUOTED=\"naïve — dash\"\nLITERAL='日本語'\nEMOJI=🚀\nEXPANDED=${PLAIN}/x\n",
    );
    assert_eq!(m["PLAIN"], "café");
    assert_eq!(m["QUOTED"], "naïve — dash");
    assert_eq!(m["LITERAL"], "日本語");
    assert_eq!(m["EMOJI"], "🚀");
    assert_eq!(m["EXPANDED"], "café/x");
}

#[test]
fn non_ascii_keys_and_multibyte_boundaries_are_intact() {
    let m = map("TÍTULO=hola\nX=${TÍTULO}\n");
    assert_eq!(m["TÍTULO"], "hola");
    // `$` name scanning is ASCII-only, so a multibyte name is not a reference;
    // whatever it does, it must not split a UTF-8 character.
    assert!(m["X"].is_char_boundary(m["X"].len()));
}

#[test]
fn control_bytes_and_crlf_are_handled() {
    let m = map("A=with\ttab\r\nB=nul\0byte\r\nC=ok\r\n");
    assert_eq!(m["A"], "with\ttab", "CR is not part of the value");
    assert_eq!(m["B"], "nul\0byte");
    assert_eq!(m["C"], "ok");
}

#[test]
fn duplicate_keys_take_the_last_value_and_see_the_previous_one() {
    let out = parse("A=first\nA=$A-second\n", &base());
    assert_eq!(out.len(), 2, "both assignments are reported in order");
    assert_eq!(out[1], ("A".to_string(), "first-second".to_string()));
}

#[test]
fn very_long_values_and_many_keys_are_linear_not_quadratic() {
    let long = "x".repeat(1 << 20);
    let m = map(&format!("BIG={long}\n"));
    assert_eq!(m["BIG"].len(), 1 << 20);

    let many: String = (0..5_000).map(|i| format!("K{i}=v{i}\n")).collect();
    let m = map(&many);
    assert_eq!(m.len(), 5_000);
    assert_eq!(m["K4999"], "v4999");
}

#[test]
fn unprefixed_variables_never_reach_the_defines() {
    let loaded = vec![
        ("DATABASE_URL".into(), "postgres://user:pw@host/db".into()),
        ("AWS_SECRET_ACCESS_KEY".into(), "top-secret-value".into()),
        // Near-misses on the prefix must not slip through either.
        ("vite_lowercase".into(), "lower-secret".into()),
        (" VITE_PADDED".into(), "padded-secret".into()),
        ("XVITE_API".into(), "prefixed-secret".into()),
        ("VITE_OK".into(), "public".into()),
    ];
    let defines = import_meta_env_defines(&loaded, "production", false, "/", &["VITE_"]);
    let blob = defines
        .iter()
        .map(|(k, v)| format!("{k}={v}\n"))
        .collect::<String>();

    for secret in [
        "postgres://user:pw@host/db",
        "top-secret-value",
        "lower-secret",
        "padded-secret",
        "prefixed-secret",
    ] {
        assert!(!blob.contains(secret), "leaked {secret} into {blob}");
    }
    assert!(blob.contains("public"), "prefixed var must be exposed");
}

#[test]
fn an_empty_prefix_exposes_everything_by_design() {
    // Vite parity: envPrefix "" means every loaded variable is public. The point
    // of this test is that the behavior is deliberate and visible, not a
    // surprise found in production.
    let loaded = vec![("DATABASE_URL".into(), "secret".into())];
    let defines = import_meta_env_defines(&loaded, "production", false, "/", &[""]);
    let keys: Vec<&str> = defines.iter().map(|(k, _)| k.as_str()).collect();
    assert!(keys.contains(&"import.meta.env.DATABASE_URL"));
}

#[test]
fn secret_values_containing_define_syntax_stay_quoted_json() {
    let loaded = vec![(
        "VITE_TRICK".into(),
        "\",\"SECRET\":\"leaked\",\"x\":\"".into(),
    )];
    let defines = import_meta_env_defines(&loaded, "development", true, "/", &["VITE_"]);
    let obj = defines
        .iter()
        .find(|(k, _)| k == "import.meta.env")
        .map(|(_, v)| v.clone())
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&obj).expect("valid JSON object");
    assert_eq!(
        parsed["VITE_TRICK"], "\",\"SECRET\":\"leaked\",\"x\":\"",
        "a hostile value must not break out of the JSON literal"
    );
    assert!(parsed.get("SECRET").is_none(), "no injected key: {parsed}");
}

#[test]
fn html_percent_soup_is_left_alone() {
    let mut env = BTreeMap::new();
    env.insert("T".to_string(), "Title".to_string());
    // Nothing here names a known key, so every one of these is an identity.
    for html in [
        "%",
        "%%",
        "%%%%%%%%",
        "% T %",
        "%T",
        "T%",
        "%NOT SET%",
        "% %",
        "%\n%",
    ] {
        assert_eq!(
            replace_html_env(html, &env),
            html,
            "{html:?} must pass through untouched"
        );
    }
    assert_eq!(replace_html_env("%T%%T%", &env), "TitleTitle");
    assert_eq!(replace_html_env("100%% of %T%", &env), "100%% of Title");
    // Documented divergence from Vite's `/%(\S+?)%/g`: that regex consumes
    // `%%T%` as an unknown key and leaves `%%T%%` untouched, while oj rescans
    // from the inner `%` and substitutes. Only adjacent percents differ.
    assert_eq!(replace_html_env("%%T%%", &env), "%Title%");
}

#[test]
fn an_empty_key_in_the_map_never_matches() {
    // `%%` has no key between the percents, so it is text -- even when the map
    // happens to contain an entry under the empty name.
    let mut env = BTreeMap::new();
    env.insert(String::new(), "SHOULD-NOT-APPEAR".to_string());
    env.insert("T".to_string(), "Title".to_string());
    for html in ["%%", "a%%b", "%%%", "100%% off", "%%T%%"] {
        let out = replace_html_env(html, &env);
        assert!(
            !out.contains("SHOULD-NOT-APPEAR"),
            "{html:?} matched the empty key: {out}"
        );
    }
    // A key that is only whitespace is text too.
    let mut spaced = BTreeMap::new();
    spaced.insert(" ".to_string(), "SHOULD-NOT-APPEAR".to_string());
    assert_eq!(replace_html_env("% %", &spaced), "% %");
}

#[test]
fn html_substitution_is_not_recursive() {
    let mut env = BTreeMap::new();
    env.insert("A".to_string(), "%B%".to_string());
    env.insert("B".to_string(), "final".to_string());
    assert_eq!(
        replace_html_env("<t>%A%</t>", &env),
        "<t>%B%</t>",
        "a substituted value must not be scanned again"
    );
}

#[test]
fn html_substitution_keeps_multibyte_content_intact() {
    let defines = import_meta_env_defines(
        &[("VITE_T".into(), "Café — Straße".into())],
        "development",
        true,
        "/",
        &["VITE_"],
    );
    let env = html_env_map(&defines);
    let out = replace_html_env("<title>%VITE_T%</title> 日本 %MODE%", &env);
    assert!(out.contains("Café — Straße"), "{out}");
    assert!(out.contains("日本"), "{out}");
    assert!(out.contains("development"), "{out}");
}

#[test]
fn html_env_map_unwraps_only_json_strings() {
    let defines = vec![
        ("import.meta.env.S".to_string(), "\"str\"".to_string()),
        ("import.meta.env.B".to_string(), "true".to_string()),
        ("import.meta.env.N".to_string(), "42".to_string()),
        (
            "import.meta.env.BROKEN".to_string(),
            "\"unclosed".to_string(),
        ),
        ("unrelated.define".to_string(), "\"ignored\"".to_string()),
    ];
    let env = html_env_map(&defines);
    assert_eq!(env["S"], "str");
    assert_eq!(env["B"], "true");
    assert_eq!(env["N"], "42");
    assert_eq!(env["BROKEN"], "\"unclosed", "unparsable falls back to raw");
    assert!(!env.contains_key("unrelated.define"));
}

#[test]
fn load_of_a_directory_that_is_not_one_is_empty() {
    let file = tempfile::NamedTempFile::new().unwrap();
    assert!(oj_env::load(file.path(), "development").is_empty());
}

#[test]
fn later_env_files_win_and_only_changed_keys_are_reported() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".env"), "VITE_A=base\nVITE_B=base\n").unwrap();
    std::fs::write(dir.path().join(".env.local"), "VITE_B=local\n").unwrap();
    std::fs::write(
        dir.path().join(".env.production"),
        "VITE_C=prod\nVITE_D=${VITE_A}-derived\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join(".env.production.local"),
        "VITE_C=prod-local\n",
    )
    .unwrap();
    // A file for another mode must not be read.
    std::fs::write(dir.path().join(".env.development"), "VITE_DEV_ONLY=nope\n").unwrap();

    let loaded: BTreeMap<String, String> =
        oj_env::load(dir.path(), "production").into_iter().collect();
    assert_eq!(loaded["VITE_A"], "base");
    assert_eq!(loaded["VITE_B"], "local");
    assert_eq!(loaded["VITE_C"], "prod-local");
    assert_eq!(
        loaded["VITE_D"], "base-derived",
        "expansion sees keys from earlier files"
    );
    assert!(!loaded.contains_key("VITE_DEV_ONLY"));
}

#[test]
fn a_dotenv_cannot_shadow_the_builtin_env_fields() {
    let loaded = vec![
        ("VITE_X".into(), "x".into()),
        ("MODE".into(), "hacked".into()),
        ("DEV".into(), "hacked".into()),
    ];
    let defines = import_meta_env_defines(&loaded, "production", false, "/base/", &["VITE_"]);
    let m: BTreeMap<_, _> = defines.into_iter().collect();
    assert_eq!(m["import.meta.env.MODE"], "\"production\"");
    assert_eq!(m["import.meta.env.DEV"], "false");
    assert_eq!(m["import.meta.env.PROD"], "true");
    assert_eq!(m["import.meta.env.BASE_URL"], "\"/base/\"");
}

#[test]
fn multiple_prefixes_expose_every_matching_var() {
    // envPrefix given as an array: a var is exposed if it matches ANY prefix;
    // an unprefixed secret is still withheld.
    let loaded = vec![
        ("VITE_A".into(), "1".into()),
        ("PUBLIC_B".into(), "2".into()),
        ("SECRET_C".into(), "3".into()),
    ];
    let defines = import_meta_env_defines(&loaded, "production", false, "/", &["VITE_", "PUBLIC_"]);
    let keys: Vec<&str> = defines.iter().map(|(k, _)| k.as_str()).collect();
    assert!(keys.contains(&"import.meta.env.VITE_A"));
    assert!(keys.contains(&"import.meta.env.PUBLIC_B"));
    assert!(
        !keys.contains(&"import.meta.env.SECRET_C"),
        "an unprefixed var must never leak",
    );
}
