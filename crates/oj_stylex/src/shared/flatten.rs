//! Raw style namespace → flat `[key, PreRule]` entries, plus basic validation.
// parity: babel-plugin src/shared/preprocess-rules/{flatten-raw-style-obj,basic-validation,PreRule}.js

use crate::errors::StylexError;
use crate::eval::value::{EvalValue, JsObjectMap};
use crate::options::ResolvedOptions;
use crate::shared::generate_rule::{CompiledDecl, convert_style_to_class_name};
use crate::shared::media_query::{MediaQueryError, is_media_key, last_media_query_wins_transform};
use crate::shared::resolution::flat_map_expanded_shorthands;
use std::borrow::Cow;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq)]
pub enum StyleScalar<'a> {
    Str(Cow<'a, str>),
    Num(f64),
    /// A legacy shorthand whose split produced no part for this side; it
    /// renders as an empty declaration instead of throwing on an empty value.
    Undefined,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PreRuleValue<'a> {
    Single(StyleScalar<'a>),
    Multi(Vec<StyleScalar<'a>>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum PreRule<'a> {
    Null,
    Rule {
        property: Cow<'a, str>,
        value: PreRuleValue<'a>,
        key_path: Option<Box<[Cow<'a, str>]>>,
    },
    Set(Vec<PreRule<'a>>),
}

/// The property-value branch's per-property (condition, rule) groupings.
type ConditionGroups<'a> = Vec<(Cow<'a, str>, Vec<(&'a str, PreRule<'a>)>)>;

impl<'a> PreRule<'a> {
    // parity: PreRule.js PreRuleSet.create
    fn create_set(rules: Vec<PreRule<'a>>) -> PreRule<'a> {
        let flat = if rules.iter().any(|rule| matches!(rule, PreRule::Set(_))) {
            let mut flat: Vec<PreRule> = Vec::with_capacity(rules.len());
            for rule in rules {
                match rule {
                    PreRule::Set(inner) => flat.extend(inner),
                    other => flat.push(other),
                }
            }
            flat
        } else {
            rules
        };
        match flat.len() {
            0 => PreRule::Null,
            1 => flat.into_iter().next().expect("len checked"),
            _ => PreRule::Set(flat),
        }
    }

    /// Visits the compiled declarations in upstream's ComputedStyle order with
    /// each one's classesToOriginalPath keyPath; `null` slots are skipped.
    pub fn for_each_compiled(
        &self,
        options: &ResolvedOptions,
        f: &mut dyn FnMut(CompiledDecl, &[Cow<'a, str>]),
    ) -> Result<(), StylexError> {
        match self {
            PreRule::Null => Ok(()),
            PreRule::Rule {
                property,
                value,
                key_path,
            } => {
                let key_path = key_path
                    .as_deref()
                    .unwrap_or_else(|| std::slice::from_ref(property));
                let decl = convert_style_to_class_name(property, value, key_path, options)?;
                f(decl, key_path);
                Ok(())
            }
            PreRule::Set(rules) => {
                for rule in rules {
                    rule.for_each_compiled(options, f)?;
                }
                Ok(())
            }
        }
    }
}

// Upstream copies via `obj[key]=value`, so "__proto__" is a [[Set]]: primitives
// vanish, objects become the prototype, array/null protos break isPlainObject.
fn is_plain(obj: &JsObjectMap) -> bool {
    match obj.get("__proto__") {
        None => true,
        Some(EvalValue::Obj(proto)) => is_plain(proto),
        Some(EvalValue::Null | EvalValue::Arr(_)) => false,
        Some(_) => true,
    }
}

enum ForIn<'a, I> {
    Own(I),
    Chain(std::vec::IntoIter<(&'a str, &'a EvalValue)>),
}

impl<'a, I: Iterator<Item = (&'a str, &'a EvalValue)>> Iterator for ForIn<'a, I> {
    type Item = (&'a str, &'a EvalValue);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            ForIn::Own(own) => own.next(),
            ForIn::Chain(chain) => chain.next(),
        }
    }
}

// for..in order: own keys, then unshadowed prototype-chain keys — which is the
// map's own order, iterated in place, unless a "__proto__" key is present.
fn for_in_entries(obj: &JsObjectMap) -> impl Iterator<Item = (&str, &EvalValue)> {
    if obj.get("__proto__").is_none() {
        ForIn::Own(obj.entries())
    } else {
        ForIn::Chain(for_in_chain(obj).into_iter())
    }
}

fn for_in_chain(obj: &JsObjectMap) -> Vec<(&str, &EvalValue)> {
    let mut entries = Vec::new();
    let mut cursor = Some(obj);
    while let Some(map) = cursor {
        cursor = None;
        for (key, val) in map.entries() {
            if key == "__proto__" {
                if let EvalValue::Obj(proto) = val {
                    cursor = Some(proto);
                }
            } else if !entries.iter().any(|(k, _)| *k == key) {
                entries.push((key, val));
            }
        }
    }
    entries
}

/// Enclosing condition keys, innermost first, linked through the call stack.
struct Conditions<'p> {
    key: &'p str,
    outer: Option<&'p Conditions<'p>>,
}

fn is_enclosing_condition(conditions: Option<&Conditions<'_>>, key: &str) -> bool {
    let mut cursor = conditions;
    while let Some(condition) = cursor {
        if condition.key == key {
            return true;
        }
        cursor = condition.outer;
    }
    false
}

// parity: basic-validation.js validateNamespace
pub fn validate_namespace(namespace: &EvalValue) -> Result<(), StylexError> {
    validate_namespace_in(namespace, None)
}

fn validate_namespace_in(
    namespace: &EvalValue,
    conditions: Option<&Conditions<'_>>,
) -> Result<(), StylexError> {
    let EvalValue::Obj(ns) = namespace else {
        return Err(StylexError::illegal_namespace_value());
    };
    if !is_plain(ns) {
        return Err(StylexError::illegal_namespace_value());
    }
    for (key, val) in for_in_entries(ns) {
        match val {
            EvalValue::Null | EvalValue::Str(_) | EvalValue::Num(_) => {}
            EvalValue::Arr(items) => {
                for item in items {
                    match item {
                        EvalValue::Null | EvalValue::Str(_) | EvalValue::Num(_) => {}
                        _ => return Err(StylexError::illegal_prop_array_value()),
                    }
                }
            }
            // isPlainObject gates the object branches; non-plain values fall
            // through to ILLEGAL_PROP_VALUE exactly as upstream's last throw.
            EvalValue::Obj(inner) if is_plain(inner) => {
                if key.starts_with('@') || key.starts_with(':') || key.starts_with('[') {
                    if is_enclosing_condition(conditions, key) {
                        return Err(StylexError::duplicate_conditional());
                    }
                    let nested = Conditions {
                        key,
                        outer: conditions,
                    };
                    validate_namespace_in(val, Some(&nested))?;
                } else {
                    validate_conditional_styles(val, None)?;
                }
            }
            EvalValue::Obj(_) | EvalValue::Undefined | EvalValue::Bool(_) => {
                return Err(StylexError::illegal_prop_value());
            }
        }
    }
    Ok(())
}

// parity: basic-validation.js validateConditionalStyles
fn validate_conditional_styles(
    val: &EvalValue,
    conditions: Option<&Conditions<'_>>,
) -> Result<(), StylexError> {
    let EvalValue::Obj(obj) = val else {
        unreachable!("callers only pass objects");
    };
    for (key, v) in for_in_entries(obj) {
        if !(key.starts_with('@')
            || key.starts_with(':')
            || key.starts_with('[')
            || key.starts_with("var(--")
            || key == "default")
        {
            return Err(StylexError::invalid_pseudo_or_at_rule());
        }
        if is_enclosing_condition(conditions, key) {
            return Err(StylexError::duplicate_conditional());
        }
        match v {
            EvalValue::Null | EvalValue::Str(_) | EvalValue::Num(_) => {}
            EvalValue::Arr(items) => {
                // parity quirk: arrays inside conditions report ILLEGAL_PROP_VALUE,
                // not ILLEGAL_PROP_ARRAY_VALUE.
                for item in items {
                    match item {
                        EvalValue::Null | EvalValue::Str(_) | EvalValue::Num(_) => {}
                        _ => return Err(StylexError::illegal_prop_value()),
                    }
                }
            }
            EvalValue::Obj(inner) if is_plain(inner) => {
                let nested = Conditions {
                    key,
                    outer: conditions,
                };
                validate_conditional_styles(v, Some(&nested))?;
            }
            EvalValue::Obj(_) | EvalValue::Undefined | EvalValue::Bool(_) => {
                return Err(StylexError::illegal_prop_value());
            }
        }
    }
    Ok(())
}

/// The `enableMediaQueryOrder` pre-pass: `Some(rebuilt)` when anything can
/// change, so the caller can hold the rebuild and flatten borrows either way.
pub fn media_order_transform(
    style: &JsObjectMap,
    options: &ResolvedOptions,
) -> Result<Option<JsObjectMap>, StylexError> {
    if options.enable_media_query_order && dfs_can_change(style, 0) {
        Ok(Some(dfs_process_map(style, 0)?))
    } else {
        Ok(None)
    }
}

pub fn flatten_raw_style_object<'a>(
    style: &'a JsObjectMap,
    options: &ResolvedOptions,
) -> Result<Vec<(Cow<'a, str>, PreRule<'a>)>, StylexError> {
    flatten_inner(style, &[], options)
}

/// Anything that would make `dfs_process_queries` differ from its input: a
/// rewritable media key, a "__proto__" entry, or a CSSType brand it would drop.
fn dfs_can_change(obj: &JsObjectMap, depth: usize) -> bool {
    obj.css_type().is_some()
        || obj.entries().any(|(key, val)| {
            key == "__proto__"
                || (depth >= 1 && is_media_key(key))
                || matches!(val, EvalValue::Obj(inner) if dfs_can_change(inner, depth + 1))
        })
}

// parity: style-value-parser media-query-transform.js dfsProcessQueries; the
// delete+reinsert moves every rewritten @media key to the end of its siblings.
// Map-in skips a redundant top-level deep clone the EvalValue wrapper needed.
fn dfs_process_map(obj: &JsObjectMap, depth: usize) -> Result<JsObjectMap, StylexError> {
    let mut result = JsObjectMap::with_capacity(obj.len());
    // Object.entries-style clone: own keys only — a "__proto__"-carried
    // prototype (and its inherited entries) is stripped here, before flatten.
    for (key, val) in obj.entries().filter(|(key, _)| *key != "__proto__") {
        let processed = match val {
            EvalValue::Obj(inner) if dfs_can_change(inner, depth + 1) => {
                EvalValue::Obj(Arc::new(dfs_process_map(inner, depth + 1)?))
            }
            other => other.clone(),
        };
        result.insert(key, processed);
    }
    if depth >= 1 {
        let media_keys: Vec<String> = result
            .keys()
            .filter(|k| is_media_key(k))
            .map(str::to_string)
            .collect();
        if !media_keys.is_empty() {
            let rewritten = last_media_query_wins_transform(&media_keys).map_err(|e| match e {
                MediaQueryError::Syntax(err) => err,
                // Never claim upstream's syntax error for a form we cannot verify.
                MediaQueryError::Unverified { input } => StylexError::unsupported_api(&format!(
                    "media query `{input}` (unverified tokenizer form)"
                )),
            })?;
            for (old_key, new_key) in media_keys.iter().zip(rewritten) {
                let value = result.remove(old_key).expect("key collected from map");
                result.insert(new_key, value);
            }
        }
    }
    Ok(result)
}

/// The unanchored `/var\(--[a-z0-9]+\)/` key unwrap in flatten-raw-style-obj.js:
/// any key CONTAINING that pattern is sliced `[4..len-1]` wholesale.
fn unwrap_var_key(key: &str) -> Cow<'_, str> {
    let bytes = key.as_bytes();
    let mut search = 0;
    while let Some(pos) = key[search..].find("var(--") {
        let start = search + pos;
        let mut i = start + 6;
        while i < bytes.len() && (bytes[i].is_ascii_lowercase() || bytes[i].is_ascii_digit()) {
            i += 1;
        }
        if i > start + 6 && i < bytes.len() && bytes[i] == b')' {
            // Upstream does key.slice(4, -1) in UTF-16 units regardless of
            // where the match sits; byte slicing panics on multibyte keys.
            return Cow::Owned(crate::jsrt::js_slice_utf16(key, 4, -1));
        }
        search = start + 1;
    }
    Cow::Borrowed(key)
}

fn js_truthy(scalar: &StyleScalar) -> bool {
    match scalar {
        StyleScalar::Str(s) => !s.is_empty(),
        StyleScalar::Num(n) => *n != 0.0 && !n.is_nan(),
        StyleScalar::Undefined => false,
    }
}

fn same_value_zero(a: &StyleScalar, b: &StyleScalar) -> bool {
    match (a, b) {
        (StyleScalar::Str(x), StyleScalar::Str(y)) => x == y,
        (StyleScalar::Num(x), StyleScalar::Num(y)) => x == y || (x.is_nan() && y.is_nan()),
        (StyleScalar::Undefined, StyleScalar::Undefined) => true,
        _ => false,
    }
}

/// `None` is JS `null`; validation rejected every other non-scalar already.
fn scalar_of<'a>(value: &'a EvalValue) -> Option<StyleScalar<'a>> {
    match value {
        EvalValue::Str(s) => Some(StyleScalar::Str(Cow::Borrowed(s))),
        EvalValue::Num(n) => Some(StyleScalar::Num(*n)),
        _ => None,
    }
}

fn rule_key_path<'a>(
    key_path: &[Cow<'a, str>],
    includes_key: &str,
    property: Cow<'a, str>,
) -> Option<Box<[Cow<'a, str>]>> {
    if key_path.is_empty() {
        return None;
    }
    Some(if key_path.iter().any(|k| k == includes_key) {
        key_path
            .iter()
            .map(|k| {
                if k == includes_key {
                    property.clone()
                } else {
                    k.clone()
                }
            })
            .collect()
    } else {
        let mut path = Vec::with_capacity(key_path.len() + 1);
        path.extend_from_slice(key_path);
        path.push(property);
        path.into_boxed_slice()
    })
}

fn flatten_inner<'a>(
    style: &'a JsObjectMap,
    key_path: &[Cow<'a, str>],
    options: &ResolvedOptions,
) -> Result<Vec<(Cow<'a, str>, PreRule<'a>)>, StylexError> {
    let mut flattened: Vec<(Cow<'a, str>, PreRule<'a>)> = Vec::with_capacity(style.len());
    // for..in reach: prototype entries planted by "__proto__" keys stay
    // visible here unless the media-query clone above already stripped them.
    for (raw_key, value) in for_in_entries(style) {
        let key: Cow<'a, str> = unwrap_var_key(raw_key);

        match value {
            EvalValue::Null | EvalValue::Str(_) | EvalValue::Num(_) => {
                let scalar = scalar_of(value);
                for (property, expanded) in
                    flat_map_expanded_shorthands(key.clone(), scalar, false, options)?
                {
                    match expanded {
                        None => flattened.push((property, PreRule::Null)),
                        Some(expanded) => {
                            let path = rule_key_path(key_path, &key, property.clone());
                            flattened.push((
                                property.clone(),
                                PreRule::Rule {
                                    property,
                                    value: PreRuleValue::Single(expanded),
                                    key_path: path,
                                },
                            ));
                        }
                    }
                }
            }
            EvalValue::Arr(items) => {
                // Fallback arrays: each element expands on its own, then the
                // per-property value lists are merged in first-seen order.
                let mut equivalent: Vec<(Cow<'a, str>, Vec<StyleScalar<'a>>)> = Vec::new();
                for item in items {
                    let scalar = scalar_of(item);
                    for (property, expanded) in
                        flat_map_expanded_shorthands(key.clone(), scalar, false, options)?
                    {
                        let slot = match equivalent.iter_mut().find(|(p, _)| *p == property) {
                            Some(slot) => &mut slot.1,
                            None => {
                                equivalent.push((property, Vec::new()));
                                &mut equivalent.last_mut().expect("just pushed").1
                            }
                        };
                        if let Some(value) = expanded {
                            slot.push(value);
                        }
                    }
                }
                for (property, values) in equivalent {
                    let mut deduped: Vec<StyleScalar> = Vec::new();
                    for value in values {
                        if js_truthy(&value) && !deduped.iter().any(|d| same_value_zero(d, &value))
                        {
                            deduped.push(value);
                        }
                    }
                    let pre_rule = match deduped.len() {
                        0 => PreRule::Null,
                        1 => PreRule::Rule {
                            property: property.clone(),
                            value: PreRuleValue::Single(deduped.into_iter().next().expect("len 1")),
                            key_path: rule_key_path(key_path, raw_key, property.clone()),
                        },
                        _ => PreRule::Rule {
                            property: property.clone(),
                            value: PreRuleValue::Multi(deduped),
                            key_path: rule_key_path(key_path, raw_key, property.clone()),
                        },
                    };
                    flattened.push((property, pre_rule));
                }
            }
            EvalValue::Obj(obj)
                if !key.starts_with(':') && !key.starts_with('@') && !key.starts_with('[') =>
            {
                // Property-value objects, e.g. color: { default, ':hover' }.
                let mut equivalent: ConditionGroups<'a> = Vec::new();
                let mut leaf = Vec::new();
                for (condition, inner_value) in for_in_entries(obj) {
                    let nested_path: Vec<Cow<'a, str>> = if key_path.is_empty() {
                        vec![key.clone(), Cow::Borrowed(condition)]
                    } else {
                        let mut p = Vec::with_capacity(key_path.len() + 1);
                        p.extend_from_slice(key_path);
                        p.push(Cow::Borrowed(condition));
                        p
                    };
                    leaf.clear();
                    flatten_value_as_property(
                        key.clone(),
                        inner_value,
                        &nested_path,
                        options,
                        &mut leaf,
                    )?;
                    for (property, pre_rule) in leaf.drain(..) {
                        match equivalent.iter_mut().find(|(p, _)| *p == property) {
                            Some((_, conds)) => {
                                match conds.iter_mut().find(|(c, _)| *c == condition) {
                                    Some((_, slot)) => *slot = pre_rule,
                                    None => conds.push((condition, pre_rule)),
                                }
                            }
                            None => equivalent.push((property, vec![(condition, pre_rule)])),
                        }
                    }
                }
                for (property, conds) in equivalent {
                    let rules: Vec<PreRule> = conds.into_iter().map(|(_, r)| r).collect();
                    flattened.push((property, PreRule::create_set(rules)));
                }
            }
            EvalValue::Obj(obj) => {
                // Pseudo / at-rule / attribute objects, e.g. ':hover': { … }.
                let mut nested_path = Vec::with_capacity(key_path.len() + 1);
                nested_path.extend_from_slice(key_path);
                nested_path.push(Cow::Borrowed(raw_key));
                for (property, pre_rule) in flatten_inner(obj, &nested_path, options)? {
                    let mut joined = String::with_capacity(key.len() + 1 + property.len());
                    joined.push_str(&key);
                    joined.push('_');
                    joined.push_str(&property);
                    flattened.push((Cow::Owned(joined), pre_rule));
                }
            }
            // Upstream's flatten silently skips other types (validation ran first).
            EvalValue::Undefined | EvalValue::Bool(_) => {}
        }
    }
    Ok(flattened)
}

/// The property-value-object recursion, without upstream's intermediate
/// `{key: value}` object: identical to flatten_inner on that single-key map.
fn flatten_value_as_property<'a>(
    key: Cow<'a, str>,
    value: &'a EvalValue,
    nested_path: &[Cow<'a, str>],
    options: &ResolvedOptions,
    out: &mut Vec<(Cow<'a, str>, PreRule<'a>)>,
) -> Result<(), StylexError> {
    match value {
        EvalValue::Null | EvalValue::Str(_) | EvalValue::Num(_) => {
            let scalar = scalar_of(value);
            for (property, expanded) in
                flat_map_expanded_shorthands(key.clone(), scalar, false, options)?
            {
                match expanded {
                    None => out.push((property, PreRule::Null)),
                    Some(expanded) => {
                        let path = rule_key_path(nested_path, &key, property.clone());
                        out.push((
                            property.clone(),
                            PreRule::Rule {
                                property,
                                value: PreRuleValue::Single(expanded),
                                key_path: path,
                            },
                        ));
                    }
                }
            }
            Ok(())
        }
        EvalValue::Arr(items) => {
            let mut equivalent: Vec<(Cow<'a, str>, Vec<StyleScalar<'a>>)> = Vec::new();
            for item in items {
                let scalar = scalar_of(item);
                for (property, expanded) in
                    flat_map_expanded_shorthands(key.clone(), scalar, false, options)?
                {
                    let slot = match equivalent.iter_mut().find(|(p, _)| *p == property) {
                        Some(slot) => &mut slot.1,
                        None => {
                            equivalent.push((property, Vec::new()));
                            &mut equivalent.last_mut().expect("just pushed").1
                        }
                    };
                    if let Some(value) = expanded {
                        slot.push(value);
                    }
                }
            }
            for (property, values) in equivalent {
                let mut deduped: Vec<StyleScalar> = Vec::new();
                for value in values {
                    if js_truthy(&value) && !deduped.iter().any(|d| same_value_zero(d, &value)) {
                        deduped.push(value);
                    }
                }
                let pre_rule = match deduped.len() {
                    0 => PreRule::Null,
                    1 => PreRule::Rule {
                        property: property.clone(),
                        value: PreRuleValue::Single(deduped.into_iter().next().expect("len 1")),
                        key_path: rule_key_path(nested_path, &key, property.clone()),
                    },
                    _ => PreRule::Rule {
                        property: property.clone(),
                        value: PreRuleValue::Multi(deduped),
                        key_path: rule_key_path(nested_path, &key, property.clone()),
                    },
                };
                out.push((property, pre_rule));
            }
            Ok(())
        }
        // Deeper nesting (condition inside a property-value object): group by
        // property and condition into per-property sets, exactly the
        // property-value branch's shape one level down.
        EvalValue::Obj(obj) => {
            let mut equivalent: ConditionGroups<'a> = Vec::new();
            let mut leaf = Vec::new();
            for (condition, inner) in for_in_entries(obj) {
                let mut p = Vec::with_capacity(nested_path.len() + 1);
                p.extend_from_slice(nested_path);
                p.push(Cow::Borrowed(condition));
                leaf.clear();
                flatten_value_as_property(key.clone(), inner, &p, options, &mut leaf)?;
                for (property, pre_rule) in leaf.drain(..) {
                    match equivalent.iter_mut().find(|(pr, _)| *pr == property) {
                        Some((_, conds)) => match conds.iter_mut().find(|(c, _)| *c == condition) {
                            Some((_, slot)) => *slot = pre_rule,
                            None => conds.push((condition, pre_rule)),
                        },
                        None => equivalent.push((property, vec![(condition, pre_rule)])),
                    }
                }
            }
            for (property, conds) in equivalent {
                let rules: Vec<PreRule> = conds.into_iter().map(|(_, r)| r).collect();
                out.push((property, PreRule::create_set(rules)));
            }
            Ok(())
        }
        EvalValue::Undefined | EvalValue::Bool(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> EvalValue {
        EvalValue::Str(v.to_string())
    }

    fn obj(entries: Vec<(&str, EvalValue)>) -> Arc<JsObjectMap> {
        let mut map = JsObjectMap::new();
        for (key, val) in entries {
            map.insert(key, val);
        }
        Arc::new(map)
    }

    // the pre-sharing rebuild: every level copied, every scalar cloned
    fn deep_rebuild(map: &JsObjectMap, depth: usize) -> JsObjectMap {
        let mut result = JsObjectMap::new();
        for (key, val) in map.entries().filter(|(key, _)| *key != "__proto__") {
            let processed = match val {
                EvalValue::Obj(inner) => EvalValue::Obj(Arc::new(deep_rebuild(inner, depth + 1))),
                other => other.clone(),
            };
            result.insert(key, processed);
        }
        if depth >= 1 {
            let media_keys: Vec<String> = result
                .keys()
                .filter(|k| is_media_key(k))
                .map(str::to_string)
                .collect();
            if !media_keys.is_empty() {
                let rewritten =
                    last_media_query_wins_transform(&media_keys).expect("valid fixture");
                for (old_key, new_key) in media_keys.iter().zip(rewritten) {
                    let value = result.remove(old_key).expect("collected from map");
                    result.insert(new_key, value);
                }
            }
        }
        result
    }

    #[test]
    fn dfs_process_map_shares_unchangeable_subtrees() {
        let hover = obj(vec![
            ("color", s("blue")),
            (":focus", EvalValue::Obj(obj(vec![("color", s("green"))]))),
        ]);
        let media = obj(vec![
            ("color", s("a")),
            (
                "@media (min-width: 900px)",
                EvalValue::Obj(obj(vec![("color", s("b"))])),
            ),
            (
                "@media (min-width: 700px)",
                EvalValue::Obj(obj(vec![("color", s("c"))])),
            ),
            (
                "__proto__",
                EvalValue::Obj(obj(vec![("inherited", s("x"))])),
            ),
        ]);
        let root = obj(vec![
            ("color", s("red")),
            (":hover", EvalValue::Obj(Arc::clone(&hover))),
            (
                "@media (min-width: 600px)",
                EvalValue::Obj(Arc::clone(&media)),
            ),
            (
                "__proto__",
                EvalValue::Obj(obj(vec![("inherited", s("y"))])),
            ),
        ]);
        assert!(dfs_can_change(&root, 0));
        let processed = dfs_process_map(&root, 0).expect("fixture transforms");
        assert_eq!(processed, deep_rebuild(&root, 0));
        assert!(!processed.contains_key("__proto__"));

        let Some(EvalValue::Obj(shared)) = processed.get(":hover") else {
            panic!(":hover survives");
        };
        assert!(Arc::ptr_eq(shared, &hover));

        let Some(EvalValue::Obj(rebuilt)) = processed.get("@media (min-width: 600px)") else {
            panic!("depth-0 media key stays verbatim");
        };
        assert!(!Arc::ptr_eq(rebuilt, &media));
        assert!(!rebuilt.contains_key("__proto__"));
        assert_eq!(
            rebuilt.keys().collect::<Vec<_>>(),
            vec!["color", "@media not all", "@media (min-width: 700px)"]
        );
    }

    #[test]
    fn unwrap_var_key_matches_upstream_regex() {
        assert_eq!(unwrap_var_key("var(--abc)"), Cow::Borrowed("--abc"));
        assert_eq!(unwrap_var_key("var(--abc123)"), Cow::Borrowed("--abc123"));
        // Uppercase var names do not match the flatten-level regex.
        assert_eq!(
            unwrap_var_key("var(--myVar)"),
            Cow::Borrowed("var(--myVar)")
        );
        assert_eq!(unwrap_var_key("var(--abc"), Cow::Borrowed("var(--abc"));
        assert_eq!(unwrap_var_key("var(--)"), Cow::Borrowed("var(--)"));
        // Unanchored match slices the whole key.
        assert_eq!(
            unwrap_var_key("xxvar(--ab)yy"),
            Cow::<str>::Owned("r(--ab)y".to_string())
        );
        assert_eq!(unwrap_var_key("color"), Cow::Borrowed("color"));
    }
}
