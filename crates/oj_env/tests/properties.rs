// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Property tests over the dotenv surface. The invariants that matter are
//! total-ness (arbitrary bytes in, no panic), byte fidelity (a value reaches
//! the app exactly as written), and confinement (an unprefixed variable can
//! never appear in anything shipped to the client).

use std::collections::BTreeMap;

use oj_env::{html_env_map, import_meta_env_defines, parse, replace_html_env};
use proptest::prelude::*;

/// Text that looks like a dotenv file: mostly assignment-shaped, with quotes,
/// comments, expansions and junk mixed in.
fn dotenv_line() -> impl Strategy<Value = String> {
    let key = "[A-Za-z_][A-Za-z0-9_]{0,8}";
    let value = prop_oneof![
        Just(String::new()),
        "[^\\n]{0,20}",
        r#"\$\{[A-Za-z_]{1,6}\}"#.prop_map(|s| s),
        "'[^'\\n]{0,10}'".prop_map(|s| s),
        "\"[^\"\\n]{0,10}\"".prop_map(|s| s),
    ];
    prop_oneof![
        8 => (key, value).prop_map(|(k, v)| format!("{k}={v}")),
        1 => Just("# comment".to_string()),
        1 => "[^\\n]{0,20}".prop_map(|s| s),
    ]
}

fn dotenv_file() -> impl Strategy<Value = String> {
    proptest::collection::vec(dotenv_line(), 0..12).prop_map(|lines| lines.join("\n"))
}

proptest! {
    /// Arbitrary bytes are a legal `.env`: parsing is total.
    #[test]
    fn parse_is_total(src in ".{0,400}") {
        let out = parse(&src, &BTreeMap::new());
        for (key, _) in &out {
            prop_assert!(!key.is_empty(), "empty key from {src:?}");
            prop_assert_eq!(key.trim(), key.as_str(), "key not trimmed: {:?}", key);
            prop_assert!(!key.contains('='), "key contains '=': {:?}", key);
        }
    }

    #[test]
    fn parse_is_total_over_dotenv_shaped_input(src in dotenv_file()) {
        let out = parse(&src, &BTreeMap::new());
        // Every assignment reported must come from a line that had an '='.
        prop_assert!(out.len() <= src.lines().filter(|l| l.contains('=')).count());
    }

    /// Assignments come back in file order, one per line, unchanged: a build
    /// applies them in sequence, so both the order and the count matter.
    #[test]
    fn well_formed_assignments_round_trip_in_order(
        lines in proptest::collection::vec(
            ("[A-Z][A-Z0-9_]{0,6}", "[^\\s$\\\\#'\"]{0,16}"),
            0..8,
        ),
    ) {
        let file: String = lines
            .iter()
            .map(|(k, v)| format!("{k}={v}\n"))
            .collect();
        prop_assert_eq!(parse(&file, &BTreeMap::new()), lines);
    }

    /// Single quotes are literal: the value is exactly the bytes between them,
    /// whatever they are.
    #[test]
    fn single_quoted_values_are_byte_exact(raw in "[^'\\n]{0,40}") {
        let out = parse(&format!("K='{raw}'\n"), &BTreeMap::new());
        prop_assert_eq!(out, vec![("K".to_string(), raw)]);
    }

    /// Values with no dotenv metacharacters survive verbatim, including
    /// multi-byte UTF-8.
    #[test]
    fn plain_values_survive_expansion_verbatim(raw in "[^\\s$\\\\#'\"]{0,40}") {
        let out = parse(&format!("K={raw}\n"), &BTreeMap::new());
        prop_assert_eq!(out, vec![("K".to_string(), raw)]);
    }

    /// Confinement: no unprefixed key or value can appear anywhere in the
    /// defines a build hands to the client.
    #[test]
    fn unprefixed_variables_are_never_exposed(
        public in proptest::collection::vec(("[A-Z]{1,6}", "[a-z]{1,8}"), 0..5),
        secret in proptest::collection::vec(("[A-Z]{1,6}", "[a-z]{8,16}"), 0..5),
    ) {
        let mut loaded: Vec<(String, String)> = public
            .iter()
            .map(|(k, v)| (format!("VITE_{k}"), v.clone()))
            .collect();
        // Secret names deliberately avoid the prefix.
        loaded.extend(secret.iter().map(|(k, v)| (format!("SECRET_{k}"), v.clone())));

        let defines = import_meta_env_defines(&loaded, "production", false, "/", &["VITE_"]);
        let blob: String = defines.iter().map(|(k, v)| format!("{k}={v};")).collect();
        for (k, v) in &secret {
            prop_assert!(!blob.contains(&format!("SECRET_{k}")), "leaked key in {blob}");
            // A secret value could coincide with a public one; only flag it
            // when it is genuinely absent from the public set.
            if !public.iter().any(|(_, pv)| pv == v) {
                prop_assert!(!blob.contains(v.as_str()), "leaked value in {blob}");
            }
        }
    }

    /// Every define is either a builtin or a prefixed variable, and the
    /// aggregate `import.meta.env` object always parses as JSON with exactly
    /// the same keys as the individual defines.
    #[test]
    fn defines_and_the_aggregate_object_agree(
        vars in proptest::collection::btree_map("VITE_[A-Z]{1,6}", "[^\"\\\\]{0,12}", 0..6),
        dev in any::<bool>(),
    ) {
        let vars: Vec<(String, String)> = vars.into_iter().collect();
        let defines = import_meta_env_defines(&vars, if dev { "development" } else { "production" }, dev, "/b/", &["VITE_"]);
        let object = defines.iter().find(|(k, _)| k == "import.meta.env").unwrap();
        let json: serde_json::Value = serde_json::from_str(&object.1).unwrap();
        let obj = json.as_object().unwrap();

        let mut individual: Vec<&str> = defines
            .iter()
            .filter_map(|(k, _)| k.strip_prefix("import.meta.env."))
            .collect();
        individual.sort_unstable();
        let mut aggregate: Vec<&str> = obj.keys().map(String::as_str).collect();
        aggregate.sort_unstable();
        prop_assert_eq!(individual, aggregate);

        prop_assert_eq!(&obj["DEV"], &serde_json::Value::Bool(dev));
        prop_assert_eq!(&obj["PROD"], &serde_json::Value::Bool(!dev));
        prop_assert_eq!(&obj["SSR"], &serde_json::Value::Bool(false));
        for (k, v) in &vars {
            prop_assert_eq!(&obj[k.as_str()], &serde_json::Value::String(v.clone()));
        }
    }

    /// `html_env_map` recovers exactly the string values that went in.
    #[test]
    fn html_env_map_roundtrips_string_values(
        vars in proptest::collection::btree_map("VITE_[A-Z]{1,6}", ".{0,12}", 0..6),
    ) {
        let vars: Vec<(String, String)> = vars.into_iter().collect();
        let defines = import_meta_env_defines(&vars, "development", true, "/", &["VITE_"]);
        let env = html_env_map(&defines);
        for (k, v) in &vars {
            prop_assert_eq!(env.get(k.as_str()), Some(v));
        }
        prop_assert_eq!(env.get("MODE").map(String::as_str), Some("development"));
    }

    /// Substitution is total, and with nothing to substitute it is the identity.
    #[test]
    fn html_substitution_is_total(html in ".{0,200}") {
        prop_assert_eq!(replace_html_env(&html, &BTreeMap::new()), html.clone());
        let mut env = BTreeMap::new();
        env.insert("KEY".to_string(), "value".to_string());
        let out = replace_html_env(&html, &env);
        // Only `%KEY%` occurrences may change, and each shortens the text.
        prop_assert!(out.len() <= html.len() + html.matches("%KEY%").count() * 5);
    }

    /// Placeholders are only recognised for keys that exist; unknown ones are
    /// preserved byte for byte.
    #[test]
    fn unknown_placeholders_are_preserved(key in "[A-Z]{1,8}", other in "[A-Z]{1,8}") {
        prop_assume!(key != other);
        let mut env = BTreeMap::new();
        env.insert(key.clone(), "V".to_string());
        let html = format!("<p>%{other}%</p>");
        prop_assert_eq!(replace_html_env(&html, &env), html.clone());
    }
}

proptest! {
    /// The runtime key spec parser is total, and every key it accepts is an
    /// identifier name with a non-empty field.
    #[test]
    fn runtime_key_specs_parse_totally(spec in "[A-Za-z0-9_$=, \\t.-]{0,64}") {
        match oj_env::parse_runtime_env_keys(&spec) {
            Ok(keys) => {
                let mut seen = std::collections::HashSet::new();
                for key in keys {
                    prop_assert!(!key.key.is_empty() && !key.field.is_empty());
                    prop_assert!(key.key != "__proto__");
                    let mut chars = key.key.chars();
                    let first = chars.next().unwrap();
                    prop_assert!(first == '$' || first == '_' || first.is_alphabetic());
                    prop_assert!(chars.all(|c| c == '$' || c == '_' || c.is_alphanumeric()));
                    prop_assert!(seen.insert(key.key.clone()), "a key appears once");
                }
            }
            Err(message) => prop_assert!(message.starts_with("OJ_RUNTIME_ENV_KEYS:")),
        }
    }
}
