//! `stylex.defineConsts` over an already-evaluated constants object.
// parity: babel-plugin src/shared/stylex-define-consts.js + visitors/stylex-define-consts.js

use crate::errors::StylexError;
use crate::eval::value::{EvalValue, JsObjectMap};
use crate::hash::hash;
use crate::options::ResolvedOptions;
use crate::rules::StylexRule;

#[derive(Debug, Clone, PartialEq)]
pub struct DefineConstsOutput {
    /// Replaces the call: the input values verbatim.
    pub js_output: JsObjectMap,
    /// One `[constKey, {constKey, constVal, ltr: "", rtl: null}, 0]` per entry.
    pub rules: Vec<StylexRule>,
}

/// `canonical_file_name` is state-manager fileNameForHashing output; callers
/// map a missing one to `cannot_generate_hash("defineConsts")` first.
pub fn define_consts(
    constants: &EvalValue,
    canonical_file_name: &str,
    export_name: &str,
    options: &ResolvedOptions,
) -> Result<DefineConstsOutput, StylexError> {
    let entries = object_entries(constants, "defineConsts")?;
    let export_id = format!("{canonical_file_name}//{export_name}");

    let mut js_output = JsObjectMap::new();
    let mut rules = Vec::new();
    for (key, value) in entries {
        if matches!(value, EvalValue::Undefined) {
            return Err(StylexError::upstream_type_crash(
                "an undefined defineConsts value",
            ));
        }
        let const_key = if let Some(rest) = key.strip_prefix("--") {
            rest.to_string()
        } else if options.debug && options.enable_debug_class_names {
            format!(
                "{}-{}{}",
                var_safe_key(&key),
                options.class_name_prefix,
                hash(&format!("{export_id}.{key}"))
            )
        } else {
            format!(
                "{}{}",
                options.class_name_prefix,
                hash(&format!("{export_id}.{key}"))
            )
        };
        js_output.insert(key, value.clone());
        rules.push(StylexRule {
            class_name: const_key.as_str().into(),
            ltr: "".into(),
            rtl: None,
            const_key: Some(const_key.into()),
            const_val: Some(Box::new(value.to_json())),
            priority: 0.0,
        });
    }
    Ok(DefineConstsOutput { js_output, rules })
}

// parity: visitor `typeof value !== 'object' || value == null` — arrays pass
// and Object.entries turns them into index-keyed entries.
fn object_entries(
    value: &EvalValue,
    fn_name: &str,
) -> Result<Vec<(String, EvalValue)>, StylexError> {
    match value {
        EvalValue::Obj(map) => Ok(map
            .entries()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()),
        EvalValue::Arr(items) => Ok(items
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v.clone()))
            .collect()),
        _ => Err(StylexError::non_style_object(fn_name)),
    }
}

// parity: `(key[0] >= '0' && key[0] <= '9' ? '_' + key : key).replace(/[^a-zA-Z0-9]/g, '_')`
// — the replace runs per UTF-16 unit (astral chars become two underscores).
fn var_safe_key(key: &str) -> String {
    let prefixed: std::borrow::Cow<'_, str> =
        if key.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            format!("_{key}").into()
        } else {
            key.into()
        };
    prefixed
        .encode_utf16()
        .map(|unit| match char::from_u32(u32::from(unit)) {
            Some(c) if c.is_ascii_alphanumeric() => c,
            _ => '_',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn consts(entries: &[(&str, EvalValue)]) -> EvalValue {
        EvalValue::Obj(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect::<JsObjectMap>()
                .into(),
        )
    }

    #[test]
    fn basic_output_matches_oracle() {
        // Pinned via live-oracle probe 2026-08-27: tokens.stylex.ts under
        // rootDir /fake/root, export name `colors`.
        let options = ResolvedOptions::default();
        let input = consts(&[
            ("bg", EvalValue::Str("red".to_string())),
            ("size", EvalValue::Num(12.0)),
        ]);
        let out = define_consts(&input, "tokens.stylex.ts", "colors", &options).unwrap();
        assert_eq!(out.js_output.to_json(), input.to_json());
        assert_eq!(out.rules.len(), 2);
        assert_eq!(&*out.rules[0].class_name, "xjvywqn");
        assert_eq!(out.rules[0].const_key.as_deref(), Some("xjvywqn"));
        assert_eq!(
            out.rules[0].const_val.as_deref(),
            Some(&serde_json::json!("red"))
        );
        assert_eq!(&*out.rules[0].ltr, "");
        assert_eq!(out.rules[0].rtl, None);
        assert!(out.rules[0].priority == 0.0);
        assert_eq!(&*out.rules[1].class_name, "xont6ws");
        assert_eq!(
            out.rules[1].const_val.as_deref(),
            Some(&serde_json::json!(12))
        );
    }

    #[test]
    fn custom_property_keys_skip_hashing() {
        let options = ResolvedOptions::default();
        let out = define_consts(
            &consts(&[
                ("--already", EvalValue::Str("x".to_string())),
                ("--", EvalValue::Str("y".to_string())),
            ]),
            "tokens.stylex.ts",
            "c",
            &options,
        )
        .unwrap();
        assert_eq!(&*out.rules[0].class_name, "already");
        assert_eq!(&*out.rules[1].class_name, "");
    }

    #[test]
    fn var_safe_key_rules() {
        assert_eq!(var_safe_key("myColor"), "myColor");
        assert_eq!(var_safe_key("2xl"), "_2xl");
        assert_eq!(var_safe_key("foo-bar"), "foo_bar");
        assert_eq!(var_safe_key("ké y"), "k__y");
        assert_eq!(var_safe_key("a😀b"), "a__b");
    }

    #[test]
    fn non_object_rejected_and_arrays_pass() {
        let options = ResolvedOptions::default();
        let err = define_consts(
            &EvalValue::Str("x".to_string()),
            "tokens.stylex.ts",
            "c",
            &options,
        )
        .unwrap_err();
        assert_eq!(err.message, "defineConsts() can only accept an object.");
        let out = define_consts(
            &EvalValue::Arr(vec![
                EvalValue::Str("a".to_string()),
                EvalValue::Str("b".to_string()),
            ]),
            "t.stylex.ts",
            "c",
            &options,
        )
        .unwrap();
        assert_eq!(out.js_output.keys().collect::<Vec<_>>(), vec!["0", "1"],);
    }
}
