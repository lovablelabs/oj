//! `stylex.createTheme` over evaluated theme vars + overrides.
// parity: babel-plugin src/shared/stylex-create-theme.js + visitors/stylex-create-theme.js

use crate::errors::StylexError;
use crate::eval::JsValue;
use crate::eval::value::{EvalValue, JsObjectMap};
use crate::hash::hash;
use crate::jsrt::{js_slice_utf16, locale_cmp, unverified_collation_char, utf16_cmp};
use crate::options::ResolvedOptions;
use crate::rules::StylexRule;
use crate::shared::define_vars::{
    collect_vars_by_at_rule, es_ordered_entries, priority_for_at_rule, wrap_with_at_rules,
};
use crate::shared::types::as_css_type_js;

#[derive(Debug, Clone, PartialEq)]
pub struct CreateThemeOutput {
    /// Replaces the call: `{[varGroupHash]: "overrideClass varGroupHash", $$css}`.
    pub js_output: JsObjectMap,
    /// One rule per at-rule bucket, priority 0.4 + n/10, "default" first.
    pub rules: Vec<StylexRule>,
}

pub fn create_theme(
    theme_vars: &JsValue,
    overrides: &JsValue,
    options: &ResolvedOptions,
) -> Result<CreateThemeOutput, StylexError> {
    let override_entries: Vec<(String, JsValue)> = match overrides {
        JsValue::Obj(obj) => es_ordered_entries(obj)
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
        JsValue::Arr(items) => items
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v.clone()))
            .collect(),
        JsValue::Proxy(_) => Vec::new(),
        _ => return Err(StylexError::non_style_object("createTheme")),
    };
    let var_group_hash = match theme_vars {
        JsValue::Proxy(proxy) => proxy.var_group_hash().to_string(),
        JsValue::Obj(obj) => match obj.get("__varGroupHash__") {
            Some(JsValue::Str(s)) if !s.is_empty() => s.clone(),
            _ => return Err(StylexError::theme_without_var_group()),
        },
        _ => return Err(StylexError::theme_without_var_group()),
    };

    // Sorted with the default comparator (plain UTF-16), not localeCompare.
    let mut sorted_keys: Vec<&str> = override_entries.iter().map(|(k, _)| k.as_str()).collect();
    sorted_keys.sort_by(|a, b| utf16_cmp(a, b));

    let mut collection: Vec<(String, Vec<String>)> = Vec::new();
    for key in sorted_keys {
        let raw = override_entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .expect("key comes from the entry list");
        let value = match as_css_type_js(&raw) {
            Some((_, inner)) => inner,
            None => raw,
        };
        let name_hash = theme_var_name_hash(theme_vars, key)?;
        collect_vars_by_at_rule(key, &name_hash, &value, &mut collection, &[])?;
    }

    let mut sorted_at_rules: Vec<&str> = collection.iter().map(|(k, _)| k.as_str()).collect();
    // This sort feeds atRulesStringForHash: an unverified collation char would
    // silently change class names, so it hard-errors (r4#4 policy).
    for at_rule in &sorted_at_rules {
        if let Some(c) = unverified_collation_char(at_rule) {
            return Err(StylexError::new(
                crate::errors::ErrorCode::UnsupportedApi,
                format!(
                    "createTheme() cannot sort at-rule {at_rule:?}: {c:?} is outside the pinned collation alphabet and the order feeds the theme hash"
                ),
            ));
        }
    }
    sorted_at_rules.sort_by(|a, b| {
        if *a == "default" {
            std::cmp::Ordering::Less
        } else if *b == "default" {
            std::cmp::Ordering::Greater
        } else {
            locale_cmp(a, b).expect("unverified chars rejected above")
        }
    });

    let decls_of = |at_rule: &str| -> String {
        collection
            .iter()
            .find(|(k, _)| k == at_rule)
            .map(|(_, decls)| decls.concat())
            .unwrap_or_default()
    };
    // The hash input wraps every bucket, the "default" one literally as default{…}.
    let at_rules_string_for_hash: String = sorted_at_rules
        .iter()
        .map(|at_rule| wrap_with_at_rules(&decls_of(at_rule), at_rule))
        .collect();
    let override_class_name = format!(
        "{}{}",
        options.class_name_prefix,
        hash(&at_rules_string_for_hash)
    );

    let mut rules = Vec::new();
    for at_rule in &sorted_at_rules {
        let decls = decls_of(at_rule);
        let rule = format!(".{override_class_name}, .{override_class_name}:root{{{decls}}}");
        let (ltr, suffix) = if *at_rule == "default" {
            (rule, String::new())
        } else {
            (
                wrap_with_at_rules(&rule, at_rule),
                format!("-{}", hash(at_rule)),
            )
        };
        rules.push(StylexRule {
            class_name: format!("{override_class_name}{suffix}").into(),
            ltr: ltr.into(),
            rtl: None,
            const_key: None,
            const_val: None,
            priority: 0.4 + priority_for_at_rule(at_rule) / 10.0,
        });
    }

    let mut js_output = JsObjectMap::new();
    js_output.insert(
        var_group_hash.clone(),
        EvalValue::Str(format!("{override_class_name} {var_group_hash}")),
    );
    js_output.insert("$$css", EvalValue::Bool(true));
    Ok(CreateThemeOutput { js_output, rules })
}

// parity: `themeVars[key].slice(6, -1)` — trims `var(--` and `)`.
fn theme_var_name_hash(theme_vars: &JsValue, key: &str) -> Result<String, StylexError> {
    match theme_vars {
        JsValue::Proxy(proxy) => {
            // Mirror the proxy traps: __varGroupHash__ answers the hash string.
            let resolved = match key {
                "__varGroupHash__" => proxy.var_group_hash().to_string(),
                "__IS_PROXY" | "toString" => {
                    return Err(StylexError::upstream_type_crash(
                        "a proxy trap value without .slice in createTheme",
                    ));
                }
                _ => proxy.resolve_key(key),
            };
            Ok(js_slice_utf16(&resolved, 6, -1))
        }
        JsValue::Obj(obj) => match obj.get(key) {
            Some(JsValue::Str(s)) => Ok(js_slice_utf16(s, 6, -1)),
            None | Some(JsValue::Undefined) => Err(StylexError::new(
                crate::errors::ErrorCode::NonStaticValue,
                "Cannot read properties of undefined (reading 'slice')",
            )),
            Some(_) => Err(StylexError::upstream_type_crash(
                "a non-string theme variable in createTheme",
            )),
        },
        _ => Err(StylexError::theme_without_var_group()),
    }
}

/// Visitor-level JS-output rewrite for `dev`/`test`; no-op otherwise.
// parity: visitors/stylex-create-theme.js isTest/isDev branches
pub fn apply_theme_dev_naming(
    js_output: JsObjectMap,
    filename: Option<&str>,
    var_name: &str,
    options: &ResolvedOptions,
) -> JsObjectMap {
    if !options.test && !options.dev {
        return js_output;
    }
    let filename = filename.unwrap_or("UnknownFile");
    let basename = filename
        .rsplit('/')
        .next()
        .unwrap_or(filename)
        .split('.')
        .next()
        .unwrap_or_default();
    let dev_class_name = format!("{basename}__{var_name}");
    let mut out = JsObjectMap::new();
    out.insert(
        dev_class_name.clone(),
        EvalValue::Str(dev_class_name.clone()),
    );
    if options.test {
        out.insert("$$css", EvalValue::Bool(true));
        return out;
    }
    for (k, v) in js_output.entries() {
        out.insert(k, v.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::JsObj;
    use crate::eval::cross_file::VarGroupProxy;

    fn probe_proxy() -> VarGroupProxy {
        // Pinned via live-oracle probe 2026-08-28 (probe3, /tmp fixture pkg).
        VarGroupProxy::new(
            "probe-pkg:src/tokens.stylex.ts".to_string(),
            "vars".to_string(),
            &ResolvedOptions::default(),
        )
    }

    fn overrides(entries: &[(&str, JsValue)]) -> JsValue {
        let mut obj = JsObj::default();
        for (k, v) in entries {
            obj.insert((*k).to_string(), v.clone());
        }
        JsValue::object(obj)
    }

    #[test]
    fn cross_file_basic_matches_oracle() {
        let options = ResolvedOptions::default();
        let proxy = JsValue::proxy(probe_proxy());
        let out = create_theme(
            &proxy,
            &overrides(&[
                ("accent", JsValue::Str("rebeccapurple".into())),
                ("gap", JsValue::Str("12px".into())),
            ]),
            &options,
        )
        .unwrap();
        assert_eq!(out.rules.len(), 1);
        assert_eq!(&*out.rules[0].class_name, "xxlf0b4");
        assert_eq!(
            &*out.rules[0].ltr,
            ".xxlf0b4, .xxlf0b4:root{--x15glzcj:rebeccapurple;--x1e9wu2u:12px;}"
        );
        assert!(out.rules[0].priority == 0.5);
        assert_eq!(
            out.js_output.get("xf9pnhg"),
            Some(&EvalValue::Str("xxlf0b4 xf9pnhg".to_string()))
        );
        assert_eq!(out.js_output.get("$$css"), Some(&EvalValue::Bool(true)));
    }

    #[test]
    fn empty_and_all_null_overrides_hash_the_empty_string() {
        // Pinned via live-oracle probe 2026-08-28: both compile to xph554m.
        let options = ResolvedOptions::default();
        let proxy = JsValue::proxy(probe_proxy());
        let empty = create_theme(&proxy, &overrides(&[]), &options).unwrap();
        assert!(empty.rules.is_empty());
        assert_eq!(
            empty.js_output.get("xf9pnhg"),
            Some(&EvalValue::Str("xph554m xf9pnhg".to_string()))
        );
        let all_null =
            create_theme(&proxy, &overrides(&[("accent", JsValue::Null)]), &options).unwrap();
        assert_eq!(
            all_null.js_output.get("xf9pnhg"),
            empty.js_output.get("xf9pnhg")
        );
    }

    #[test]
    fn missing_var_group_hash_is_rejected() {
        let options = ResolvedOptions::default();
        let err = create_theme(
            &JsValue::object(JsObj::default()),
            &overrides(&[]),
            &options,
        )
        .unwrap_err();
        assert_eq!(
            err.message,
            "Can only override variables theme created with defineVars()."
        );
        let err =
            create_theme(&JsValue::Str("nope".into()), &overrides(&[]), &options).unwrap_err();
        assert_eq!(err.code, crate::errors::ErrorCode::ThemeWithoutVarGroup);
    }

    #[test]
    fn same_file_map_lookup_and_unknown_key() {
        let options = ResolvedOptions::default();
        let mut vars = JsObj::default();
        vars.insert("accent".to_string(), JsValue::Str("var(--xx91c37)".into()));
        vars.insert(
            "__varGroupHash__".to_string(),
            JsValue::Str("x7tlvcs".into()),
        );
        let theme_vars = JsValue::object(vars);
        // Pinned via live-oracle probe 2026-08-28 (probe3 same-file green theme).
        let out = create_theme(
            &theme_vars,
            &overrides(&[("accent", JsValue::Str("green".into()))]),
            &options,
        )
        .unwrap();
        assert_eq!(&*out.rules[0].class_name, "x7u453x");
        assert_eq!(
            &*out.rules[0].ltr,
            ".x7u453x, .x7u453x:root{--xx91c37:green;}"
        );
        let err = create_theme(
            &theme_vars,
            &overrides(&[("missing", JsValue::Str("green".into()))]),
            &options,
        )
        .unwrap_err();
        assert_eq!(
            err.message,
            "Cannot read properties of undefined (reading 'slice')"
        );
    }

    #[test]
    fn dev_and_test_naming() {
        let dev_options = ResolvedOptions {
            dev: true,
            debug: true,
            ..ResolvedOptions::default()
        };
        let mut js = JsObjectMap::new();
        js.insert("x7tlvcs", EvalValue::Str("x7u453x x7tlvcs".into()));
        js.insert("$$css", EvalValue::Bool(true));
        let dev = apply_theme_dev_naming(
            js.clone(),
            Some("/fix/src/both.stylex.ts"),
            "myT",
            &dev_options,
        );
        let keys: Vec<&str> = dev.keys().collect();
        assert_eq!(keys, vec!["both__myT", "x7tlvcs", "$$css"]);
        assert_eq!(
            dev.get("both__myT"),
            Some(&EvalValue::Str("both__myT".into()))
        );

        let test_options = ResolvedOptions {
            test: true,
            ..ResolvedOptions::default()
        };
        let test =
            apply_theme_dev_naming(js.clone(), Some("/x/theme.ts"), "myTheme", &test_options);
        let keys: Vec<&str> = test.keys().collect();
        assert_eq!(keys, vec!["theme__myTheme", "$$css"]);

        let plain = apply_theme_dev_naming(js.clone(), None, "t", &ResolvedOptions::default());
        assert_eq!(plain, js);
    }
}
