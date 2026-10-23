//! `stylex.defineVars` over an evaluated variables object.
// parity: stylex-define-vars.js + stylex-vars-utils.js + its visitor (normalize)

use std::collections::{BTreeMap, BTreeSet};

use crate::errors::StylexError;
use crate::eval::cross_file::VarGroupProxy;
use crate::eval::functions::{DepTracker, arrow_info, register_self_reference};
use crate::eval::value::{EvalValue, JsObjectMap, array_index};
use crate::eval::{Callable, EvalOutcome, Evaluator, JsObj, JsValue};
use crate::hash::hash;
use crate::jsrt::{js_number_to_string, utf16_cmp};
use crate::module_resolution::gen_file_based_identifier;
use crate::options::ResolvedOptions;
use crate::rules::StylexRule;
use crate::shared::types::as_css_type_js;

pub const SPLIT_TOKEN: &str = "__$$__";

#[derive(Debug, Clone, PartialEq)]
pub struct DefineVarsOutput {
    /// Replaces the call: `{key: "var(--nameHash)", …, __varGroupHash__}`.
    pub js_output: JsObjectMap,
    /// `@property` rules (priority 0) then the grouped var rules (0.1 + n/10).
    pub rules: Vec<StylexRule>,
}

/// ES OwnPropertyKeys order over the evaluator-local object representation.
pub fn es_ordered_entries<'v>(obj: &'v JsObj) -> Vec<(&'v str, &'v JsValue)> {
    let mut index: Vec<(u32, &'v str, &'v JsValue)> = Vec::new();
    let mut named: Vec<(&'v str, &'v JsValue)> = Vec::new();
    for (k, v) in obj.entries() {
        match array_index(k) {
            Some(n) => index.push((n, k, v)),
            None => named.push((k, v)),
        }
    }
    index.sort_by_key(|(n, _, _)| *n);
    index
        .into_iter()
        .map(|(_, k, v)| (k, v))
        .chain(named)
        .collect()
}

/// `canonical_file_name` is the state-manager fileNameForHashing output; the
/// caller maps a missing one to `cannot_generate_hash("defineVars")` first.
pub fn define_vars<'a>(
    ev: &mut Evaluator<'a, '_>,
    value: &JsValue,
    canonical_file_name: &str,
    export_name: &str,
) -> Result<DefineVarsOutput, StylexError> {
    let entries: Vec<(String, JsValue)> = match value {
        JsValue::Obj(obj) => es_ordered_entries(obj)
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
        JsValue::Arr(items) => items
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v.clone()))
            .collect(),
        // Object.entries over the upstream proxy sees no own keys.
        JsValue::Proxy(_) => Vec::new(),
        _ => return Err(StylexError::non_style_object("defineVars")),
    };
    let options: &ResolvedOptions = ev.state.options;
    let proxy = VarGroupProxy::new(
        canonical_file_name.to_string(),
        export_name.to_string(),
        options,
    );
    register_self_reference(ev, export_name, proxy.clone());

    let mut normalized: Vec<(String, JsValue)> = Vec::new();
    let mut dep_map: Vec<(String, Vec<String>)> = Vec::new();
    for (key, v) in &entries {
        let mut deps = Vec::new();
        let nv = normalize_value(ev, &proxy, v, key, &mut deps, true)?;
        normalized.push((key.clone(), nv));
        dep_map.push((key.clone(), deps));
    }

    let keys: BTreeSet<&str> = normalized.iter().map(|(k, _)| k.as_str()).collect();
    for (key, deps) in &dep_map {
        for dep in deps {
            if dep != "__varGroupHash__" && !keys.contains(dep.as_str()) {
                return Err(StylexError::unknown_define_vars_reference(key, dep));
            }
        }
    }
    assert_no_define_vars_cycles(&dep_map)?;

    define_vars_core(&normalized, canonical_file_name, export_name, options)
}

// parity: visitors/stylex-define-vars.js normalizeDefineVarsValue.
fn normalize_value<'a>(
    ev: &mut Evaluator<'a, '_>,
    self_proxy: &VarGroupProxy,
    value: &JsValue,
    root_key: &str,
    deps: &mut Vec<String>,
    allow_css_type: bool,
) -> Result<JsValue, StylexError> {
    match value {
        JsValue::Callable(Callable::Arrow(key)) => {
            evaluate_define_vars_function(ev, self_proxy, *key, root_key, deps)
        }
        // Injected callables carry fn.length >= 1 upstream.
        JsValue::Callable(Callable::Stylex(_)) => {
            Err(StylexError::invalid_define_vars_function_value())
        }
        // Opaque = shapes upstream deopts on before normalize; same message.
        JsValue::Callable(Callable::Opaque) => Err(StylexError::non_static_value("defineVars")),
        JsValue::Str(_) | JsValue::Num(_) | JsValue::Null => Ok(value.clone()),
        JsValue::Arr(_) => Err(StylexError::array_in_define_vars()),
        JsValue::Obj(_) if as_css_type_js(value).is_some() => {
            if allow_css_type {
                Ok(value.clone())
            } else {
                Err(StylexError::invalid_define_vars_function_value())
            }
        }
        JsValue::Obj(obj) => {
            if matches!(obj.get("default"), None | Some(JsValue::Undefined)) {
                return Err(StylexError::missing_default_value(Some(root_key)));
            }
            let nested: Vec<(String, JsValue)> = es_ordered_entries(obj)
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect();
            let mut out = JsObj::default();
            for (k, v) in nested {
                out.insert(
                    k,
                    normalize_value(ev, self_proxy, &v, root_key, deps, false)?,
                );
            }
            Ok(JsValue::object(out))
        }
        // The proxy's 'default' trap answers a var string; it has no own keys.
        JsValue::Proxy(_) => Ok(JsValue::object(JsObj::default())),
        JsValue::Undefined | JsValue::Bool(_) => Err(StylexError::invalid_define_vars_value()),
    }
}

// parity: visitors/stylex-define-vars.js evaluateDefineVarsFunction — every
// throw out of fn() is swallowed into nonStaticValue.
fn evaluate_define_vars_function<'a>(
    ev: &mut Evaluator<'a, '_>,
    self_proxy: &VarGroupProxy,
    arrow_key: u32,
    root_key: &str,
    deps: &mut Vec<String>,
) -> Result<JsValue, StylexError> {
    let Some((body, param_count)) = arrow_info(ev, arrow_key) else {
        return Err(StylexError::non_static_value("defineVars"));
    };
    if param_count != 0 {
        return Err(StylexError::invalid_define_vars_function_value());
    }
    let mut tracker = DepTracker::new(self_proxy);
    let _ = tracker.walk(ev, body);
    for dep in tracker.deps {
        if !deps.contains(&dep) {
            deps.push(dep);
        }
    }
    let result = match ev.eval(body) {
        Ok(EvalOutcome::Value(v)) => v,
        Ok(EvalOutcome::NonStatic(_)) | Err(_) => {
            return Err(StylexError::non_static_value("defineVars"));
        }
    };
    if let JsValue::Callable(_) = result {
        return Err(StylexError::invalid_define_vars_function_value());
    }
    if as_css_type_js(&result).is_some() {
        return Ok(result);
    }
    normalize_value(ev, self_proxy, &result, root_key, deps, false)
}

// parity: visitors/stylex-define-vars.js assertNoDefineVarsCycles.
fn assert_no_define_vars_cycles(dep_map: &[(String, Vec<String>)]) -> Result<(), StylexError> {
    let map: BTreeMap<&str, &[String]> = dep_map
        .iter()
        .map(|(k, deps)| (k.as_str(), deps.as_slice()))
        .collect();
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut in_stack: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = Vec::new();
    fn visit(
        key: &str,
        map: &BTreeMap<&str, &[String]>,
        visited: &mut BTreeSet<String>,
        in_stack: &mut BTreeSet<String>,
        stack: &mut Vec<String>,
    ) -> Result<(), StylexError> {
        if in_stack.contains(key) {
            let start = stack.iter().position(|k| k == key).unwrap_or(0);
            let mut parts: Vec<&str> = stack[start..].iter().map(String::as_str).collect();
            parts.push(key);
            return Err(StylexError::cyclic_define_vars_reference(
                &parts.join(" -> "),
            ));
        }
        if visited.contains(key) {
            return Ok(());
        }
        visited.insert(key.to_string());
        in_stack.insert(key.to_string());
        stack.push(key.to_string());
        if let Some(deps) = map.get(key) {
            for dep in deps.iter() {
                if map.contains_key(dep.as_str()) {
                    visit(dep, map, visited, in_stack, stack)?;
                }
            }
        }
        stack.pop();
        in_stack.remove(key);
        Ok(())
    }
    for (key, _) in dep_map {
        visit(key, &map, &mut visited, &mut in_stack, &mut stack)?;
    }
    Ok(())
}

/// Shared-core defineVars over pre-normalized entries (the nested wrapper feeds
/// flattened dot-keys). parity: shared/stylex-define-vars.js styleXDefineVars.
pub fn define_vars_core(
    normalized: &[(String, JsValue)],
    canonical_file_name: &str,
    export_name: &str,
    options: &ResolvedOptions,
) -> Result<DefineVarsOutput, StylexError> {
    let export_id = gen_file_based_identifier(canonical_file_name, export_name, None);
    let var_group_hash = format!("{}{}", options.class_name_prefix, hash(&export_id));
    let debug_names = options.debug && options.enable_debug_class_names;

    let mut js_output = JsObjectMap::new();
    let mut typed: EsKeyedMap<(Option<String>, String)> = EsKeyedMap::new();
    let mut css_entries: Vec<(String, String, JsValue)> = Vec::new();
    for (key, value) in normalized {
        let name_hash = if let Some(rest) = key.strip_prefix("--") {
            rest.to_string()
        } else {
            let hashed = hash(&gen_file_based_identifier(
                canonical_file_name,
                export_name,
                Some(key),
            ));
            if debug_names {
                format!(
                    "{}-{}{hashed}",
                    var_safe_key(key),
                    options.class_name_prefix
                )
            } else {
                format!("{}{hashed}", options.class_name_prefix)
            }
        };
        let css_value = match as_css_type_js(value) {
            Some((syntax, inner)) => {
                typed.insert(&name_hash, (get_default_value(&inner)?, syntax));
                inner
            }
            None => value.clone(),
        };
        js_output.insert(key.clone(), EvalValue::Str(format!("var(--{name_hash})")));
        css_entries.push((key.clone(), name_hash, css_value));
    }
    js_output.insert("__varGroupHash__", EvalValue::Str(var_group_hash.clone()));

    let mut collection: Vec<(String, Vec<String>)> = Vec::new();
    for (key, name_hash, value) in &css_entries {
        collect_vars_by_at_rule(key, name_hash, value, &mut collection, &[])?;
    }

    // parity: `{...injectableTypes, ...injectableStyles}` — an ES keyed object,
    // so a colliding group-rule key overwrites the @property rule in place.
    let mut merged: EsKeyedMap<StylexRule> = EsKeyedMap::new();
    for (name_hash, (initial, syntax)) in typed.entries() {
        let initial_part = initial
            .as_ref()
            .map(|iv| format!(" initial-value: {iv}"))
            .unwrap_or_default();
        merged.insert(
            &name_hash,
            StylexRule {
                class_name: name_hash.as_str().into(),
                ltr: format!(
                    "@property --{name_hash} {{ syntax: \"{syntax}\"; inherits: true;{initial_part} }}"
                )
                .into(),
                rtl: None,
                const_key: None,
                const_val: None,
                priority: 0.0,
            },
        );
    }
    // Object.entries(rulesByAtRule) enumerates numeric at-rule keys first.
    for (at_key, decls) in es_keyed_order(&collection) {
        let body = format!(":root, .{var_group_hash}{{{}}}", decls.join(""));
        let (ltr, suffix) = if at_key == "default" {
            (body, String::new())
        } else {
            (
                wrap_with_at_rules(&body, at_key),
                format!("-{}", hash(at_key)),
            )
        };
        let class_name = format!("{var_group_hash}{suffix}");
        merged.insert(
            &class_name.clone(),
            StylexRule {
                class_name: class_name.into(),
                ltr: ltr.into(),
                rtl: None,
                const_key: None,
                const_val: None,
                priority: priority_for_at_rule(at_key) / 10.0,
            },
        );
    }
    let rules = merged.into_values();
    Ok(DefineVarsOutput { js_output, rules })
}

/// ES keyed-object stand-in for the upstream rule maps: canonical index keys
/// ascending first, insertion order after, overwrite keeps the key's position.
struct EsKeyedMap<V> {
    index_entries: Vec<(u32, V)>,
    named_entries: Vec<(String, V)>,
}

impl<V> EsKeyedMap<V> {
    fn new() -> Self {
        Self {
            index_entries: Vec::new(),
            named_entries: Vec::new(),
        }
    }

    fn insert(&mut self, key: &str, value: V) {
        // parity: `obj[key] = v` — a "__proto__" key hits the prototype
        // setter and creates no own entry (typedVariables, r5#7).
        if key == "__proto__" {
            return;
        }
        if let Some(n) = array_index(key) {
            match self.index_entries.binary_search_by_key(&n, |e| e.0) {
                Ok(i) => self.index_entries[i].1 = value,
                Err(i) => self.index_entries.insert(i, (n, value)),
            }
        } else if let Some(entry) = self.named_entries.iter_mut().find(|(k, _)| k == key) {
            entry.1 = value;
        } else {
            self.named_entries.push((key.to_string(), value));
        }
    }

    fn entries(&self) -> impl Iterator<Item = (String, &V)> {
        self.index_entries
            .iter()
            .map(|(n, v)| (n.to_string(), v))
            .chain(self.named_entries.iter().map(|(k, v)| (k.clone(), v)))
    }

    fn into_values(self) -> Vec<V> {
        self.index_entries
            .into_iter()
            .map(|(_, v)| v)
            .chain(self.named_entries.into_iter().map(|(_, v)| v))
            .collect()
    }
}

/// Object.entries order over the insertion-ordered at-rule collection.
fn es_keyed_order(collection: &[(String, Vec<String>)]) -> Vec<(&str, &Vec<String>)> {
    let mut index: Vec<(u32, &str, &Vec<String>)> = Vec::new();
    let mut named: Vec<(&str, &Vec<String>)> = Vec::new();
    for (k, v) in collection {
        match array_index(k) {
            Some(n) => index.push((n, k, v)),
            None => named.push((k, v)),
        }
    }
    index.sort_by_key(|(n, _, _)| *n);
    index
        .into_iter()
        .map(|(_, k, v)| (k, v))
        .chain(named)
        .collect()
}

// parity: stylex-vars-utils.js collectVarsByAtRule (booleans, functions and
// undefined fall through every branch and drop silently).
pub fn collect_vars_by_at_rule(
    key: &str,
    name_hash: &str,
    value: &JsValue,
    collection: &mut Vec<(String, Vec<String>)>,
    at_rules: &[String],
) -> Result<(), StylexError> {
    let leaf = match value {
        JsValue::Str(s) => Some(s.clone()),
        JsValue::Num(n) => Some(js_number_to_string(*n)),
        _ => None,
    };
    if let Some(val) = leaf {
        let combo = if at_rules.is_empty() {
            "default".to_string()
        } else {
            let mut sorted = at_rules.to_vec();
            sorted.sort_by(|a, b| utf16_cmp(a, b));
            sorted.join(SPLIT_TOKEN)
        };
        let decl = format!("--{name_hash}:{val};");
        if let Some((_, decls)) = collection.iter_mut().find(|(k, _)| *k == combo) {
            decls.push(decl);
        } else {
            collection.push((combo, vec![decl]));
        }
        return Ok(());
    }
    match value {
        JsValue::Null => Ok(()),
        JsValue::Arr(_) => Err(StylexError::array_in_define_vars()),
        JsValue::Obj(obj) => {
            if matches!(obj.get("default"), None | Some(JsValue::Undefined)) {
                return Err(StylexError::missing_default_value(Some(key)));
            }
            let nested: Vec<(String, JsValue)> = es_ordered_entries(obj)
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect();
            for (at_rule, v) in nested {
                if at_rule == "default" {
                    collect_vars_by_at_rule(key, name_hash, &v, collection, at_rules)?;
                } else {
                    let mut extended = at_rules.to_vec();
                    extended.push(at_rule);
                    collect_vars_by_at_rule(key, name_hash, &v, collection, &extended)?;
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

// parity: stylex-vars-utils.js wrapWithAtRules — later split parts wrap outside.
pub fn wrap_with_at_rules(ltr: &str, at_rule_key: &str) -> String {
    at_rule_key
        .split(SPLIT_TOKEN)
        .fold(ltr.to_string(), |acc, at_rule| {
            format!("{at_rule}{{{acc}}}")
        })
}

pub fn priority_for_at_rule(at_rule_key: &str) -> f64 {
    if at_rule_key == "default" {
        1.0
    } else {
        1.0 + at_rule_key.split(SPLIT_TOKEN).count() as f64
    }
}

// parity: stylex-vars-utils.js getDefaultValue (keyless error message).
pub fn get_default_value(value: &JsValue) -> Result<Option<String>, StylexError> {
    match value {
        JsValue::Str(s) => Ok(Some(s.clone())),
        JsValue::Num(n) => Ok(Some(js_number_to_string(*n))),
        JsValue::Null | JsValue::Undefined => Ok(None),
        JsValue::Arr(_) => Err(StylexError::array_in_define_vars()),
        JsValue::Obj(obj) => match obj.get("default") {
            None | Some(JsValue::Undefined) => Err(StylexError::missing_default_value(None)),
            Some(inner) => {
                let inner = inner.clone();
                get_default_value(&inner)
            }
        },
        JsValue::Proxy(proxy) => {
            let resolved = proxy.resolve_key("default");
            Ok(Some(resolved))
        }
        JsValue::Bool(_) | JsValue::Callable(_) => Err(StylexError::invalid_define_vars_value()),
    }
}

// parity: `(key[0] in 0-9 ? '_'+key : key).replace(/[^a-zA-Z0-9]/g, '_')`.
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

    #[test]
    fn at_rule_grouping_helpers_match_oracle() {
        // Pinned via live-oracle probe 2026-08-28 (probe1 "basic multi-atrule").
        assert_eq!(
            wrap_with_at_rules(
                ":root, .x{--a:1;}",
                "@media (max-width: 600px)__$$__@supports (display: grid)"
            ),
            "@supports (display: grid){@media (max-width: 600px){:root, .x{--a:1;}}}"
        );
        assert!(priority_for_at_rule("default") == 1.0);
        assert!(priority_for_at_rule("@media print") == 2.0);
        assert!(priority_for_at_rule("@media a__$$__@supports b") == 3.0);
    }

    #[test]
    fn collect_skips_null_and_opaque_values() {
        let mut collection = Vec::new();
        collect_vars_by_at_rule("a", "xh", &JsValue::Null, &mut collection, &[]).unwrap();
        collect_vars_by_at_rule("a", "xh", &JsValue::Bool(true), &mut collection, &[]).unwrap();
        collect_vars_by_at_rule("a", "xh", &JsValue::Undefined, &mut collection, &[]).unwrap();
        collect_vars_by_at_rule(
            "a",
            "xh",
            &JsValue::Callable(Callable::Opaque),
            &mut collection,
            &[],
        )
        .unwrap();
        assert!(collection.is_empty());
        let err = collect_vars_by_at_rule("a", "xh", &JsValue::array(vec![]), &mut collection, &[])
            .unwrap_err();
        assert_eq!(err.message, "Array is not supported in defineVars");
    }

    #[test]
    fn missing_default_messages() {
        let mut obj = JsObj::default();
        obj.insert("@media print".to_string(), JsValue::Str("x".to_string()));
        let mut collection = Vec::new();
        let err = collect_vars_by_at_rule(
            "gap",
            "xh",
            &JsValue::object(obj.clone()),
            &mut collection,
            &[],
        )
        .unwrap_err();
        assert_eq!(
            err.message,
            "Default value is not defined for gap variable."
        );
        let err = get_default_value(&JsValue::object(obj)).unwrap_err();
        assert_eq!(err.message, "Default value is not defined for variable.");
        assert_eq!(get_default_value(&JsValue::Null).unwrap(), None);
        assert_eq!(
            get_default_value(&JsValue::Num(4.0)).unwrap().as_deref(),
            Some("4")
        );
        let err = get_default_value(&JsValue::Bool(true)).unwrap_err();
        assert_eq!(err.message, "Invalid value in defineVars");
    }

    #[test]
    fn cycle_error_text_matches_oracle() {
        // Pinned via live-oracle probe 2026-08-28 (probe2 cycle shapes).
        let dep_map = vec![
            ("a".to_string(), vec!["b".to_string()]),
            ("b".to_string(), vec!["c".to_string()]),
            ("c".to_string(), vec!["a".to_string()]),
        ];
        let err = assert_no_define_vars_cycles(&dep_map).unwrap_err();
        assert_eq!(
            err.message,
            "Cyclic same-group references in defineVars() are not allowed: a -> b -> c -> a."
        );
        let self_cycle = vec![("a".to_string(), vec!["a".to_string()])];
        let err = assert_no_define_vars_cycles(&self_cycle).unwrap_err();
        assert_eq!(
            err.message,
            "Cyclic same-group references in defineVars() are not allowed: a -> a."
        );
        let ok = vec![
            ("x".to_string(), vec!["y".to_string()]),
            ("y".to_string(), vec![]),
        ];
        assert!(assert_no_define_vars_cycles(&ok).is_ok());
    }

    #[test]
    fn es_ordered_entries_reorders_index_keys() {
        let mut obj = JsObj::default();
        obj.insert("b".to_string(), JsValue::Str("1".into()));
        obj.insert("2".to_string(), JsValue::Str("2".into()));
        obj.insert("05".to_string(), JsValue::Str("3".into()));
        let keys: Vec<&str> = es_ordered_entries(&obj)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(keys, vec!["2", "b", "05"]);
    }
}
