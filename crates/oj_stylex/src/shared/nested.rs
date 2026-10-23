//! Nested-to-flat token transforms for the `unstable_*Nested` APIs.
// parity: babel-plugin src/shared/stylex-nested-utils.js + the *-nested wrappers

use crate::fxhash::FxHashMap;
use std::sync::Arc;

use crate::errors::StylexError;
use crate::eval::functions::StylexCallable;
use crate::eval::value::{EvalValue, JsObjectMap, array_index};
use crate::eval::{Callable, JsObj, JsValue, to_eval_value};
use crate::jsrt::js_number_to_string;
use crate::options::ResolvedOptions;
use crate::shared::define_consts::{DefineConstsOutput, define_consts};
use crate::shared::define_vars::es_ordered_entries;
use crate::shared::types::{as_css_type_js, types_member_source};
use crate::shared::view_transition::object_entries_js;

pub const NESTED_KEY_SEPARATOR: char = '.';

/// `String(fn)` of the closure the oracle's evaluator wraps every evaluatable
/// arrow in (pinned from the 0.19.0 dist; identical for every user arrow).
pub const EVALUATED_FN_SOURCE: &str = "(...args) => {\n        const identifierEntries = identParams.map((ident, index) => [ident, args[index]]);\n        const identifiersObj = Object.fromEntries(identifierEntries);\n        const result = evaluate(evaluatedExpr, state.traversalState, {\n          ...state.functions,\n          identifiers: {\n            ...state.functions.identifiers,\n            ...identifiersObj\n          }\n        });\n        if (!result.confident) {\n          throw new Error(result.reason ?? NON_CONSTANT);\n        }\n        return result.value;\n      }";

enum LeafKind {
    Strings,
    Consts,
}

/// unstable_createThemeNested themeVars flatten: only strings are leaves.
pub fn flatten_nested_string_config(value: &EvalValue) -> Result<JsObjectMap, StylexError> {
    flatten(value, &LeafKind::Strings)
}

/// unstable_defineConstsNested flatten: strings and numbers are leaves, every
/// object (even one with a `default` key) is a namespace.
pub fn flatten_nested_consts_config(value: &EvalValue) -> Result<JsObjectMap, StylexError> {
    flatten(value, &LeafKind::Consts)
}

fn flatten(value: &EvalValue, kind: &LeafKind) -> Result<JsObjectMap, StylexError> {
    let entries = object_entries_js(value, "Object.keys of a nullish nested config")?;
    let mut result = JsObjectMap::new();
    flatten_impl(&entries, "", &mut result, kind)?;
    Ok(result)
}

fn flatten_impl(
    obj: &JsObjectMap,
    prefix: &str,
    result: &mut JsObjectMap,
    kind: &LeafKind,
) -> Result<(), StylexError> {
    for (key, value) in obj.entries() {
        // The oracle's evaluator builds objects with `obj[key]=` [[Set]]s, so
        // an own "__proto__" never reaches its flatten (r4#7).
        if key == "__proto__" {
            continue;
        }
        if key.contains(NESTED_KEY_SEPARATOR) {
            return Err(StylexError::nested_key_contains_separator(key));
        }
        let full_key = if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{prefix}{NESTED_KEY_SEPARATOR}{key}")
        };
        let is_leaf = match kind {
            LeafKind::Strings => matches!(value, EvalValue::Str(_)),
            LeafKind::Consts => matches!(value, EvalValue::Str(_) | EvalValue::Num(_)),
        };
        if is_leaf {
            result.insert(full_key, value.clone());
        } else if let EvalValue::Obj(map) = value {
            flatten_impl(map, &full_key, result, kind)?;
        }
    }
    Ok(())
}

// Vars/overrides flatten over the evaluator model: callables must survive to
// conditional leaves, where the oracle String()s them (r4#8).

/// unstable_defineVarsNested flatten: strings, CSSTypes, and conditional
/// `{default, '@…'}` objects are leaves; other objects are namespaces.
pub fn flatten_nested_vars_config(value: &JsValue) -> Result<JsObjectMap, StylexError> {
    flatten_js(value, false, "unstable_defineVarsNested")
}

/// unstable_createThemeNested overrides flatten: vars leaves, CSSTypes unwrapped.
pub fn flatten_nested_overrides_config(value: &JsValue) -> Result<JsObjectMap, StylexError> {
    flatten_js(value, true, "unstable_createThemeNested")
}

fn flatten_js(
    value: &JsValue,
    unwrap_css_types: bool,
    api: &str,
) -> Result<JsObjectMap, StylexError> {
    let mut result = JsObjectMap::new();
    match value {
        JsValue::Obj(obj) => flatten_js_impl(obj, "", &mut result, unwrap_css_types, api)?,
        JsValue::Arr(items) => {
            for (i, item) in items.iter().enumerate() {
                flatten_js_entry(&i.to_string(), item, "", &mut result, unwrap_css_types, api)?;
            }
        }
        // Object.keys over the upstream proxy sees no own keys.
        JsValue::Proxy(_) => {}
        _ => return Err(StylexError::non_style_object(api)),
    }
    Ok(result)
}

fn flatten_js_impl(
    obj: &JsObj,
    prefix: &str,
    result: &mut JsObjectMap,
    unwrap_css_types: bool,
    api: &str,
) -> Result<(), StylexError> {
    for (key, value) in es_ordered_entries(obj) {
        flatten_js_entry(key, value, prefix, result, unwrap_css_types, api)?;
    }
    Ok(())
}

fn flatten_js_entry(
    key: &str,
    value: &JsValue,
    prefix: &str,
    result: &mut JsObjectMap,
    unwrap_css_types: bool,
    api: &str,
) -> Result<(), StylexError> {
    // parity: an own "__proto__" cannot survive the oracle's evaluation (r4#7).
    if key == "__proto__" {
        return Ok(());
    }
    if key.contains(NESTED_KEY_SEPARATOR) {
        return Err(StylexError::nested_key_contains_separator(key));
    }
    let full_key = if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}{NESTED_KEY_SEPARATOR}{key}")
    };
    if undefined_like(value) {
        return Ok(());
    }
    if is_vars_leaf_js(value) {
        let leaf = if let JsValue::Str(_) = value {
            to_eval_value(value)
        } else if let Some((_, inner)) = as_css_type_js(value) {
            if unwrap_css_types {
                to_vars_config_value_js(&inner, api)?
            } else {
                to_eval_value(value)
            }
        } else {
            to_vars_config_value_js(value, api)?
        };
        result.insert(full_key, leaf);
    } else if let JsValue::Obj(obj) = value {
        flatten_js_impl(obj, &full_key, result, unwrap_css_types, api)?;
    }
    Ok(())
}

/// Callables the oracle's evaluator resolves to `undefined` (uninvoked
/// keyframes/positionTry/… wrappers) behave as undefined everywhere here.
fn undefined_like(value: &JsValue) -> bool {
    match value {
        JsValue::Undefined => true,
        JsValue::Callable(Callable::Stylex(callee)) => !matches!(callee, StylexCallable::Types(_)),
        _ => false,
    }
}

// parity: stylex-nested-utils.js isVarsLeaf
fn is_vars_leaf_js(value: &JsValue) -> bool {
    if matches!(value, JsValue::Str(_)) || as_css_type_js(value).is_some() {
        return true;
    }
    let JsValue::Obj(obj) = value else {
        return false;
    };
    // `value.default === undefined` → namespace; else a conditional leaf only
    // when every other key is an @-rule.
    match obj.get("default") {
        None => false,
        Some(v) if undefined_like(v) => false,
        Some(_) => es_ordered_entries(obj)
            .into_iter()
            .all(|(k, _)| k == "default" || k == "__proto__" || k.starts_with('@')),
    }
}

// parity: stylex-nested-utils.js toVarsConfigValue — note the object branch
// seeds `default: ''` first, so `default` always sorts to the front.
fn to_vars_config_value_js(value: &JsValue, api: &str) -> Result<EvalValue, StylexError> {
    if undefined_like(value) {
        return Ok(EvalValue::Str(String::new()));
    }
    Ok(match value {
        JsValue::Str(s) => EvalValue::Str(s.clone()),
        JsValue::Obj(obj) => {
            let mut out = JsObjectMap::new();
            out.insert("default", EvalValue::Str(String::new()));
            for (k, v) in es_ordered_entries(obj) {
                if k == "__proto__" {
                    continue;
                }
                out.insert(k, to_vars_config_value_js(v, api)?);
            }
            EvalValue::Obj(Arc::new(out))
        }
        JsValue::Proxy(_) => {
            let mut out = JsObjectMap::new();
            out.insert("default", EvalValue::Str(String::new()));
            EvalValue::Obj(Arc::new(out))
        }
        // `String(value ?? '')`
        JsValue::Null => EvalValue::Str(String::new()),
        other => EvalValue::Str(js_coerce_string(other, api)?),
    })
}

// parity: JS String() coercion for the non-object leaves reachable above.
fn js_coerce_string(value: &JsValue, api: &str) -> Result<String, StylexError> {
    Ok(match value {
        JsValue::Str(s) => s.clone(),
        JsValue::Num(n) => js_number_to_string(*n),
        JsValue::Bool(b) => b.to_string(),
        JsValue::Null => "null".to_string(),
        JsValue::Undefined => "undefined".to_string(),
        JsValue::Obj(_) | JsValue::Proxy(_) => "[object Object]".to_string(),
        JsValue::Arr(items) => {
            let mut parts = Vec::with_capacity(items.len());
            for item in items.iter() {
                parts.push(match item {
                    JsValue::Null => String::new(),
                    v if undefined_like(v) => String::new(),
                    other => js_coerce_string(other, api)?,
                });
            }
            parts.join(",")
        }
        JsValue::Callable(Callable::Arrow(_)) => EVALUATED_FN_SOURCE.to_string(),
        JsValue::Callable(Callable::Stylex(StylexCallable::Types(kind))) => {
            types_member_source(kind)
                .ok_or_else(|| StylexError::non_static_value(api))?
                .to_string()
        }
        // The oracle's evaluator deopts wherever ours manufactures these.
        JsValue::Callable(_) => return Err(StylexError::non_static_value(api)),
    })
}

// parity: stylex-nested-utils.js SPECIAL_KEYS
const SPECIAL_KEYS: [&str; 2] = ["__varGroupHash__", "$$css"];

/// Rebuilds a nested object from dot-separated flat keys, including the
/// upstream conflict quirks (a scalar written over a namespace detaches it).
pub fn unflatten_object(flat: &JsObjectMap) -> JsObjectMap {
    let mut nodes: Vec<NodeMap> = vec![NodeMap::default()];
    let mut intermediates: FxHashMap<String, usize> = FxHashMap::default();
    for (key, value) in flat.entries() {
        if SPECIAL_KEYS.contains(&key) || !key.contains(NESTED_KEY_SEPARATOR) {
            nodes[0].insert(key, Slot::Value(value.clone()));
            continue;
        }
        let parts: Vec<&str> = key.split(NESTED_KEY_SEPARATOR).collect();
        let mut path_so_far = String::new();
        let mut current = 0usize;
        for part in &parts[..parts.len() - 1] {
            let next_path = if path_so_far.is_empty() {
                (*part).to_string()
            } else {
                format!("{path_so_far}{NESTED_KEY_SEPARATOR}{part}")
            };
            if let Some(&existing) = intermediates.get(&next_path) {
                current = existing;
            } else {
                let id = nodes.len();
                nodes.push(NodeMap::default());
                nodes[current].insert(part, Slot::Child(id));
                intermediates.insert(next_path.clone(), id);
                current = id;
            }
            path_so_far = next_path;
        }
        nodes[current].insert(parts[parts.len() - 1], Slot::Value(value.clone()));
    }
    materialize(&nodes, 0)
}

/// defineVarsNested JS-output shape: unflatten the flat var refs and re-append
/// `__varGroupHash__` last (`define_vars` itself is the theming slice's).
pub fn nest_define_vars_js_output(flat_result: &JsObjectMap) -> JsObjectMap {
    let mut refs = JsObjectMap::new();
    let mut hash = None;
    for (k, v) in flat_result.entries() {
        if k == "__varGroupHash__" {
            hash = Some(v.clone());
        } else {
            refs.insert(k, v.clone());
        }
    }
    let mut out = unflatten_object(&refs);
    if let Some(hash) = hash {
        out.insert("__varGroupHash__", hash);
    }
    out
}

/// unstable_defineConstsNested: flatten → the landed flat defineConsts → unflatten.
// parity: stylex-define-consts-nested.js + its visitor
pub fn define_consts_nested(
    constants: &EvalValue,
    canonical_file_name: &str,
    export_name: &str,
    options: &ResolvedOptions,
) -> Result<DefineConstsOutput, StylexError> {
    // parity: the visitor's `typeof value !== 'object' || value == null` gate.
    if !matches!(constants, EvalValue::Obj(_) | EvalValue::Arr(_)) {
        return Err(StylexError::non_style_object("unstable_defineConstsNested"));
    }
    let flat = flatten_nested_consts_config(constants)?;
    let out = define_consts(
        &EvalValue::Obj(Arc::new(flat)),
        canonical_file_name,
        export_name,
        options,
    )?;
    Ok(DefineConstsOutput {
        js_output: unflatten_object(&out.js_output),
        rules: out.rules,
    })
}

enum Slot {
    Value(EvalValue),
    Child(usize),
}

/// Arena node with JS ownPropertyKeys ordering (index keys ascending first);
/// writing a "__proto__" key is the ordinary-object [[Set]]: no own entry.
#[derive(Default)]
struct NodeMap {
    index_entries: Vec<(u32, String, Slot)>,
    named_entries: Vec<(String, Slot)>,
}

impl NodeMap {
    fn insert(&mut self, key: &str, slot: Slot) {
        if key == "__proto__" {
            return;
        }
        if let Some(n) = array_index(key) {
            match self.index_entries.binary_search_by_key(&n, |e| e.0) {
                Ok(i) => self.index_entries[i].2 = slot,
                Err(i) => self.index_entries.insert(i, (n, key.to_string(), slot)),
            }
        } else if let Some(entry) = self.named_entries.iter_mut().find(|(k, _)| k == key) {
            entry.1 = slot;
        } else {
            self.named_entries.push((key.to_string(), slot));
        }
    }

    fn entries(&self) -> impl Iterator<Item = (&str, &Slot)> {
        self.index_entries
            .iter()
            .map(|(_, k, v)| (k.as_str(), v))
            .chain(self.named_entries.iter().map(|(k, v)| (k.as_str(), v)))
    }
}

fn materialize(nodes: &[NodeMap], id: usize) -> JsObjectMap {
    let mut out = JsObjectMap::new();
    for (key, slot) in nodes[id].entries() {
        match slot {
            Slot::Value(v) => out.insert(key, v.clone()),
            Slot::Child(child) => {
                out.insert(key, EvalValue::Obj(Arc::new(materialize(nodes, *child))))
            }
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::types::create_css_type;

    fn obj(entries: &[(&str, EvalValue)]) -> EvalValue {
        EvalValue::Obj(map(entries).into())
    }

    fn map(entries: &[(&str, EvalValue)]) -> JsObjectMap {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    fn jobj(entries: &[(&str, JsValue)]) -> JsValue {
        let mut out = JsObj::default();
        for (k, v) in entries {
            out.insert((*k).to_string(), v.clone());
        }
        JsValue::object(out)
    }

    fn s(v: &str) -> EvalValue {
        EvalValue::Str(v.to_string())
    }

    fn js(v: &str) -> JsValue {
        JsValue::Str(v.to_string())
    }

    fn flat_pairs(map: &JsObjectMap) -> Vec<(String, EvalValue)> {
        map.entries()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn consts_flatten_drops_non_scalars_and_recurses_default_groups() {
        let input = obj(&[(
            "a",
            obj(&[
                ("b", EvalValue::Null),
                ("c", EvalValue::Bool(true)),
                ("d", EvalValue::Arr(vec![s("x")])),
                ("e", s("kept")),
                ("f", EvalValue::Num(5.0)),
                ("g", obj(&[("default", s("x")), ("hovered", s("y"))])),
            ]),
        )]);
        let flat = flatten_nested_consts_config(&input).unwrap();
        assert_eq!(
            flat_pairs(&flat),
            vec![
                ("a.e".to_string(), s("kept")),
                ("a.f".to_string(), EvalValue::Num(5.0)),
                ("a.g.default".to_string(), s("x")),
                ("a.g.hovered".to_string(), s("y")),
            ]
        );
    }

    #[test]
    fn vars_flatten_leaf_detection() {
        let cond = jobj(&[("default", js("blue")), ("@media x", js("dark"))]);
        let input = jobj(&[
            ("num", JsValue::Num(5.0)),
            ("b", jobj(&[("c", cond.clone())])),
            // default present but sibling key not @-prefixed → namespace
            ("mixed", jobj(&[("default", js("a")), ("hovered", js("b"))])),
            // default: undefined → namespace
            (
                "und",
                jobj(&[("default", JsValue::Undefined), ("x", js("y"))]),
            ),
        ]);
        let flat = flatten_nested_vars_config(&input).unwrap();
        assert_eq!(
            flat_pairs(&flat),
            vec![
                (
                    "b.c".to_string(),
                    obj(&[("default", s("blue")), ("@media x", s("dark"))])
                ),
                ("mixed.default".to_string(), s("a")),
                ("mixed.hovered".to_string(), s("b")),
                ("und.x".to_string(), s("y")),
            ]
        );
    }

    #[test]
    fn to_vars_config_value_reorders_default_first_and_string_coerces() {
        let leaf = jobj(&[
            ("@media x", JsValue::array(vec![JsValue::Num(1.0), js("x")])),
            ("default", JsValue::Num(5.0)),
        ]);
        let flat = flatten_nested_vars_config(&jobj(&[("c", leaf)])).unwrap();
        assert_eq!(
            flat.get("c"),
            Some(&obj(&[("default", s("5")), ("@media x", s("1,x"))]))
        );
    }

    #[test]
    fn dot_keys_are_rejected_at_any_depth() {
        let err = flatten_nested_consts_config(&obj(&[("a.b", s("x"))])).unwrap_err();
        assert_eq!(
            err.message,
            "Key \"a.b\" must not contain the \".\" character. Use nested objects instead of dots in key names. See: https://www.designtokens.org/tr/drafts/format/#character-restrictions"
        );
        let err =
            flatten_nested_vars_config(&jobj(&[("a", jobj(&[("b.c", js("x"))]))])).unwrap_err();
        assert!(err.message.starts_with("Key \"b.c\""));
        // The key check fires even when the value would be dropped.
        let err = flatten_nested_consts_config(&obj(&[("a.b", EvalValue::Null)])).unwrap_err();
        assert_eq!(err.code, crate::errors::ErrorCode::NestedKeySeparator);
    }

    #[test]
    fn unflatten_rebuilds_and_keeps_special_keys() {
        let flat = map(&[
            ("button.primary.bg", s("var(--x1)")),
            ("button.secondary.bg", s("var(--x2)")),
            ("flat", s("var(--x3)")),
            ("__varGroupHash__", s("xh")),
        ]);
        let nested = unflatten_object(&flat);
        assert_eq!(
            flat_pairs(&nested),
            vec![
                (
                    "button".to_string(),
                    obj(&[
                        ("primary", obj(&[("bg", s("var(--x1)"))])),
                        ("secondary", obj(&[("bg", s("var(--x2)"))])),
                    ])
                ),
                ("flat".to_string(), s("var(--x3)")),
                ("__varGroupHash__".to_string(), s("xh")),
            ]
        );
    }

    #[test]
    fn unflatten_conflict_quirks_mirror_upstream() {
        // "a.b" then "a": the scalar clobbers the namespace, and later "a.c"
        // writes land in the detached object (lost) — upstream-exact.
        let flat = map(&[("a.b", s("1")), ("a", s("2")), ("a.c", s("3"))]);
        let nested = unflatten_object(&flat);
        assert_eq!(flat_pairs(&nested), vec![("a".to_string(), s("2"))]);
        // "a" then "a.b": the namespace clobbers the scalar.
        let flat = map(&[("a", s("1")), ("a.b", s("2"))]);
        let nested = unflatten_object(&flat);
        assert_eq!(
            flat_pairs(&nested),
            vec![("a".to_string(), obj(&[("b", s("2"))]))]
        );
    }

    #[test]
    fn nest_define_vars_js_output_appends_hash_last() {
        let flat = map(&[("__varGroupHash__", s("xh")), ("a.b", s("var(--x1)"))]);
        let nested = nest_define_vars_js_output(&flat);
        assert_eq!(
            flat_pairs(&nested),
            vec![
                ("a".to_string(), obj(&[("b", s("var(--x1)"))])),
                ("__varGroupHash__".to_string(), s("xh")),
            ]
        );
    }

    fn live_css_type(kind: &str, value: JsValue) -> JsValue {
        let created = create_css_type(kind, &[to_eval_value(&value)]).unwrap();
        crate::eval::from_eval_value(&created)
    }

    #[test]
    fn css_type_leaves_keep_or_unwrap_by_variant() {
        let css = live_css_type("color", js("red"));
        let flat =
            flatten_nested_vars_config(&jobj(&[("a", jobj(&[("b", css.clone())]))])).unwrap();
        assert_eq!(
            flat_pairs(&flat),
            vec![("a.b".to_string(), to_eval_value(&css))]
        );
        let flat = flatten_nested_overrides_config(&jobj(&[("a", css)])).unwrap();
        assert_eq!(flat_pairs(&flat), vec![("a".to_string(), s("red"))]);
    }

    #[test]
    fn css_type_in_conditional_leaf_never_leaks_the_brand() {
        // Pinned via live-oracle probe 2026-08-28 (r4#6): the instance
        // enumerates as {default:'', value, syntax} — no marker key.
        let leaf = jobj(&[
            ("default", js("blue")),
            ("@media print", live_css_type("length", JsValue::Num(4.0))),
        ]);
        let flat = flatten_nested_vars_config(&jobj(&[("c", leaf)])).unwrap();
        assert_eq!(
            flat.get("c"),
            Some(&obj(&[
                ("default", s("blue")),
                (
                    "@media print",
                    obj(&[
                        ("default", s("")),
                        ("value", s("4px")),
                        ("syntax", s("<length>")),
                    ])
                ),
            ]))
        );
    }

    #[test]
    fn proto_keys_drop_like_the_oracle_evaluator() {
        // Pinned via live-oracle probe 2026-08-28 (r4#7): every shape drops.
        let flat =
            flatten_nested_vars_config(&jobj(&[("__proto__", js("red")), ("ok", js("blue"))]))
                .unwrap();
        assert_eq!(flat_pairs(&flat), vec![("ok".to_string(), s("blue"))]);
        let flat = flatten_nested_vars_config(&jobj(&[
            ("__proto__", jobj(&[("x", js("red"))])),
            ("ok", js("blue")),
        ]))
        .unwrap();
        assert_eq!(flat_pairs(&flat), vec![("ok".to_string(), s("blue"))]);
        let flat = flatten_nested_vars_config(&jobj(&[
            ("a", jobj(&[("__proto__", jobj(&[("b", js("red"))]))])),
            ("ok", js("blue")),
        ]))
        .unwrap();
        assert_eq!(flat_pairs(&flat), vec![("ok".to_string(), s("blue"))]);
        // conditional leaves ignore a __proto__ sibling and skip it on convert
        let leaf = jobj(&[
            ("default", JsValue::Num(1.0)),
            ("__proto__", js("x")),
            ("@media print", js("y")),
        ]);
        let flat = flatten_nested_vars_config(&jobj(&[("c", leaf)])).unwrap();
        assert_eq!(
            flat.get("c"),
            Some(&obj(&[("default", s("1")), ("@media print", s("y"))]))
        );
        let flat =
            flatten_nested_consts_config(&obj(&[("__proto__", s("red")), ("ok", s("blue"))]))
                .unwrap();
        assert_eq!(flat_pairs(&flat), vec![("ok".to_string(), s("blue"))]);
        let flat =
            flatten_nested_string_config(&obj(&[("__proto__", s("v")), ("ok", s("w"))])).unwrap();
        assert_eq!(flat_pairs(&flat), vec![("ok".to_string(), s("w"))]);
    }

    #[test]
    fn unflatten_proto_writes_are_ordinary_object_sets() {
        // Root leaf, root intermediate, and deep intermediate all vanish while
        // later writes land in the detached node — upstream Map bookkeeping.
        let flat = map(&[("__proto__", s("1")), ("ok", s("2"))]);
        assert_eq!(
            flat_pairs(&unflatten_object(&flat)),
            vec![("ok".to_string(), s("2"))]
        );
        let flat = map(&[("__proto__.x", s("1")), ("ok", s("2"))]);
        assert_eq!(
            flat_pairs(&unflatten_object(&flat)),
            vec![("ok".to_string(), s("2"))]
        );
        let flat = map(&[("a.__proto__.b", s("1")), ("a.c", s("2"))]);
        assert_eq!(
            flat_pairs(&unflatten_object(&flat)),
            vec![("a".to_string(), obj(&[("c", s("2"))]))]
        );
    }

    #[test]
    fn callable_leaves_string_coerce_like_the_oracle() {
        // Pinned via live-oracle probe 2026-08-28 (r4#8): the oracle wraps
        // every evaluatable arrow in one closure and String()s that.
        let arrow = JsValue::Callable(Callable::Arrow(0));
        let leaf = jobj(&[("default", arrow.clone()), ("@media print", js("blue"))]);
        let flat = flatten_nested_vars_config(&jobj(&[("c", leaf)])).unwrap();
        assert_eq!(
            flat.get("c"),
            Some(&obj(&[
                ("default", s(EVALUATED_FN_SOURCE)),
                ("@media print", s("blue")),
            ]))
        );
        // top-level callables are neither leaves nor namespaces → dropped
        let flat =
            flatten_nested_vars_config(&jobj(&[("c", arrow.clone()), ("ok", js("blue"))])).unwrap();
        assert_eq!(flat_pairs(&flat), vec![("ok".to_string(), s("blue"))]);
        // array elements coerce through the same String()
        let leaf = jobj(&[(
            "default",
            JsValue::array(vec![JsValue::Num(1.0), arrow.clone()]),
        )]);
        let flat = flatten_nested_vars_config(&jobj(&[("c", leaf)])).unwrap();
        assert_eq!(
            flat.get("c"),
            Some(&obj(&[("default", s(&format!("1,{EVALUATED_FN_SOURCE}")))]))
        );
        // overrides flatten shares the coercion
        let leaf = jobj(&[("default", arrow), ("@media print", js("x"))]);
        let flat = flatten_nested_overrides_config(&jobj(&[("c", leaf)])).unwrap();
        assert_eq!(
            flat.get("c"),
            Some(&obj(&[
                ("default", s(EVALUATED_FN_SOURCE)),
                ("@media print", s("x")),
            ]))
        );
    }

    #[test]
    fn stylex_callables_split_types_texts_from_undefined() {
        // Live-oracle pinned: uninvoked types.* members String() to the dist
        // source; keyframes resolves undefined, so the leaf lacks a default.
        let color = JsValue::Callable(Callable::Stylex(StylexCallable::Types("color".into())));
        let leaf = jobj(&[("default", color), ("@media print", js("blue"))]);
        let flat = flatten_nested_vars_config(&jobj(&[("c", leaf)])).unwrap();
        assert_eq!(
            flat.get("c"),
            Some(&obj(&[
                (
                    "default",
                    s("create(value) {\n    return new Color(value);\n  }")
                ),
                ("@media print", s("blue")),
            ]))
        );
        let kf = JsValue::Callable(Callable::Stylex(StylexCallable::Keyframes));
        let leaf = jobj(&[("default", kf), ("@media print", js("blue"))]);
        let flat = flatten_nested_vars_config(&jobj(&[("c", leaf)])).unwrap();
        assert_eq!(
            flat_pairs(&flat),
            vec![("c.@media print".to_string(), s("blue"))]
        );
    }
}
