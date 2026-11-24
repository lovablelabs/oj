//! Compile-time `props()`/`attrs()`/`stylex()` merge, oracle-pinned runtime port.
// parity: stylex-props.js + stylex-merge.js + @stylexjs/stylex over styleq@0.2.1

use std::borrow::Cow;

use crate::errors::StylexError;
use crate::eval::value::{EvalValue, JsObjectMap};
use crate::jsrt::js_number_to_string;
use crate::options::ResolvedOptions;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeMode {
    Props,
    Attrs,
}

/// parseNullableStyle output: null/`undefined` literal, an evaluated style
/// object, or the non-static `'other'` marker.
#[derive(Debug, Clone, PartialEq)]
pub enum NullableStyle {
    Null,
    Style(EvalValue),
    Other,
}

/// One flattened call argument (one level of array args expands beforehand),
/// classified by the AST-side caller.
#[derive(Debug, Clone, PartialEq)]
pub enum MergeArg {
    /// object / identifier / member / call argument, via parseNullableStyle
    Resolved(NullableStyle),
    /// `test ? a : b` — the test expression stays in the AST
    Conditional {
        primary: NullableStyle,
        fallback: NullableStyle,
    },
    /// `left && right`, both sides via parseNullableStyle
    LogicalAnd {
        left: NullableStyle,
        right: NullableStyle,
    },
    /// any other argument shape (bare null/boolean/number/string literals,
    /// spreads, arrays nested beyond one level, …)
    Unsupported,
    /// logical expression with an operator other than `&&`
    NonAndLogical,
}

/// `T` is the merged leaf: a props/attrs object, or the legacy class string.
#[derive(Debug, Clone, PartialEq)]
pub enum MergePlan<T> {
    /// no conditionals: one merged leaf replaces the call
    Inlined(T),
    /// 1..=4 conditionals, in upstream emission order; the lookup expression is
    /// `!!c0 << (n-1) | … | !!c(n-1) << 0` over the conditionals in arg order
    Table(Vec<(u32, T)>),
    /// keep the runtime call; consume with StyleVarsCollector for DCE
    Bail { bail_out_index: Option<usize> },
}

// parity: stylex-props.js argument loop (bail policy, conditional cap of 4,
// enableInlinedConditionalMerge gate).
pub fn plan_merge(
    args: &[MergeArg],
    mode: MergeMode,
    options: &ResolvedOptions,
) -> Result<MergePlan<JsObjectMap>, StylexError> {
    plan_with(args, options, false, |values| match mode {
        MergeMode::Props => props_of(values),
        MergeMode::Attrs => attrs_of(values),
    })
}

/// parity: stylex-merge.js — the same loop, except it breaks on the first bail,
/// which makes `bailOutIndex` differ from props on some argument lists.
pub fn plan_legacy_merge(
    args: &[MergeArg],
    options: &ResolvedOptions,
) -> Result<MergePlan<String>, StylexError> {
    plan_with(args, options, true, |values| Ok(styleq(values)?.0))
}

/// [`plan_merge`] without the merged leaves, for callers that read only the
/// plan's shape and its error; props and attrs share both.
pub fn plan_merge_shape(
    args: &[MergeArg],
    options: &ResolvedOptions,
) -> Result<MergePlan<()>, StylexError> {
    plan_with(args, options, false, merge_error)
}

/// [`plan_legacy_merge`] without the merged leaves.
pub fn plan_legacy_merge_shape(
    args: &[MergeArg],
    options: &ResolvedOptions,
) -> Result<MergePlan<()>, StylexError> {
    plan_with(args, options, true, merge_error)
}

/// The error merging `values` raises, without merging unless one can: styleq
/// fails only on a string argument (nested arrays flatten into arguments)
/// holding an astral character, whose UTF-16 halves it indexes one by one.
fn merge_error(values: &[&EvalValue]) -> Result<(), StylexError> {
    fn splits_a_pair(value: &EvalValue) -> bool {
        match value {
            EvalValue::Str(s) => s.bytes().any(|b| b >= 0xF0),
            EvalValue::Arr(items) => items.iter().any(splits_a_pair),
            _ => false,
        }
    }
    if values.iter().any(|value| splits_a_pair(value)) {
        styleq(values)?;
    }
    Ok(())
}

static NULL_VALUE: EvalValue = EvalValue::Null;

fn plan_with<T>(
    args: &[MergeArg],
    options: &ResolvedOptions,
    stop_on_first_bail: bool,
    merge: impl Fn(&[&EvalValue]) -> Result<T, StylexError>,
) -> Result<MergePlan<T>, StylexError> {
    enum Resolved<'a> {
        Static(&'a EvalValue),
        Conditional(&'a EvalValue, &'a EvalValue),
    }
    let mut bail = false;
    let mut bail_out_index: Option<usize> = None;
    let mut conditional = 0usize;
    let mut resolved: Vec<Resolved<'_>> = Vec::with_capacity(args.len());

    fn to_value(style: &NullableStyle) -> &EvalValue {
        match style {
            NullableStyle::Null => &NULL_VALUE,
            NullableStyle::Style(v) => v,
            NullableStyle::Other => unreachable!("Other never reaches merging"),
        }
    }

    for (index, arg) in args.iter().enumerate() {
        match arg {
            MergeArg::Resolved(NullableStyle::Other) | MergeArg::Unsupported => {
                bail_out_index.get_or_insert(index);
                bail = true;
            }
            MergeArg::Resolved(style) => resolved.push(Resolved::Static(to_value(style))),
            MergeArg::Conditional { primary, fallback } => {
                if matches!(primary, NullableStyle::Other)
                    || matches!(fallback, NullableStyle::Other)
                {
                    bail_out_index.get_or_insert(index);
                    bail = true;
                } else {
                    resolved.push(Resolved::Conditional(to_value(primary), to_value(fallback)));
                    conditional += 1;
                }
            }
            MergeArg::LogicalAnd { left, right } => {
                // Inlining needs a non-static left (the runtime test) and a static right.
                if !matches!(left, NullableStyle::Other) || matches!(right, NullableStyle::Other) {
                    bail_out_index.get_or_insert(index);
                    bail = true;
                } else {
                    resolved.push(Resolved::Conditional(to_value(right), &NULL_VALUE));
                    conditional += 1;
                }
            }
            MergeArg::NonAndLogical => {
                // Upstream overwrites bailOutIndex here (no null guard) and breaks.
                bail_out_index = Some(index);
                bail = true;
                break;
            }
        }
        if conditional > 4 {
            bail = true;
        }
        if bail && stop_on_first_bail {
            break;
        }
    }
    if !options.enable_inlined_conditional_merge && conditional > 0 {
        bail = true;
    }
    if bail {
        return Ok(MergePlan::Bail { bail_out_index });
    }

    let mut values: Vec<&EvalValue> = Vec::with_capacity(resolved.len());
    if conditional == 0 {
        values.extend(resolved.iter().map(|r| match r {
            Resolved::Static(v) => *v,
            Resolved::Conditional(..) => unreachable!("conditional == 0"),
        }));
        return Ok(MergePlan::Inlined(merge(&values)?));
    }

    let mut entries = Vec::with_capacity(1 << conditional);
    for i in 0u32..(1 << conditional) {
        let mut bit = 0u32;
        let mut key = 0u32;
        values.clear();
        values.extend(resolved.iter().map(|r| match r {
            Resolved::Static(v) => *v,
            Resolved::Conditional(primary, fallback) => {
                let taken = (i >> bit) & 1 == 1;
                bit += 1;
                if taken { *primary } else { *fallback }
            }
        }));
        for j in 0..conditional {
            key = (key << 1) | ((i >> j) & 1);
        }
        entries.push((key, merge(&values)?));
    }
    Ok(MergePlan::Table(entries))
}

fn refs(args: &[EvalValue]) -> Vec<&EvalValue> {
    args.iter().collect()
}

/// Runtime `props(...args)` over evaluated values; args are one level flat
/// (styleq flattens nested arrays itself).
pub fn merge_props(args: &[EvalValue]) -> Result<JsObjectMap, StylexError> {
    props_of(&refs(args))
}

fn props_of(args: &[&EvalValue]) -> Result<JsObjectMap, StylexError> {
    let (class_name, inline_style, debug) = styleq(args)?;
    let mut result = JsObjectMap::new();
    if !class_name.is_empty() {
        result.insert("className", EvalValue::Str(class_name));
    }
    if let Some(style) = inline_style
        && !style.is_empty()
    {
        result.insert("style", EvalValue::Obj(style.into()));
    }
    // `dataStyleSrc != null && dataStyleSrc !== ''` keeps non-string debug
    // values (a `$$css: false` chunk stays boolean).
    let keep_debug = match &debug {
        EvalValue::Str(s) => !s.is_empty(),
        EvalValue::Null | EvalValue::Undefined => false,
        _ => true,
    };
    if keep_debug {
        result.insert("data-style-src", debug);
    }
    Ok(result)
}

/// Runtime `attrs(...args)`: props reshaped, with the style object serialized.
pub fn merge_attrs(args: &[EvalValue]) -> Result<JsObjectMap, StylexError> {
    attrs_of(&refs(args))
}

fn attrs_of(args: &[&EvalValue]) -> Result<JsObjectMap, StylexError> {
    let (class_name, inline_style, debug) = styleq(args)?;
    let mut result = JsObjectMap::new();
    if !class_name.is_empty() {
        result.insert("class", EvalValue::Str(class_name));
    }
    if let Some(style) = inline_style
        && !style.is_empty()
    {
        let mut serialized = String::new();
        for (i, (k, v)) in style.entries().enumerate() {
            if i > 0 {
                serialized.push(';');
            }
            // parity: 0.19.2 attrs() — custom properties are case-sensitive
            // and keep their spelling.
            if k.starts_with("--") {
                serialized.push_str(k);
            } else {
                serialized.push_str(&to_kebab_case(k));
            }
            serialized.push(':');
            push_js_string(&mut serialized, v);
        }
        result.insert("style", EvalValue::Str(serialized));
    }
    let keep_debug = match &debug {
        EvalValue::Str(s) => !s.is_empty(),
        EvalValue::Null | EvalValue::Undefined => false,
        _ => true,
    };
    if keep_debug {
        result.insert("data-style-src", debug);
    }
    Ok(result)
}

/// Runtime `legacyMerge(...args)`: `styleq(args)[0]`. The inline-style object
/// and the debug source string are discarded, empty string included.
pub fn merge_legacy(args: &[EvalValue]) -> Result<String, StylexError> {
    Ok(styleq(&refs(args))?.0)
}

/// styleq's `definedProperties`. A style's own keys are unique, so each
/// membership test only has to look at the properties earlier styles defined.
struct Defined<'a> {
    props: Vec<Cow<'a, str>>,
    /// `key_mark` of every property an earlier style defined; filled when a
    /// later style arrives, so a lone style never computes one.
    marks: Vec<u64>,
}

impl<'a> Defined<'a> {
    fn with_capacity(capacity: usize) -> Self {
        Defined {
            props: Vec::with_capacity(capacity),
            marks: Vec::new(),
        }
    }

    fn begin_style(&mut self) {
        let marked = self.marks.len();
        if marked < self.props.len() {
            self.marks.reserve(self.props.capacity() - marked);
            self.marks
                .extend(self.props[marked..].iter().map(|prop| key_mark(prop)));
        }
    }

    fn contains(&self, prop: &str) -> bool {
        if self.marks.is_empty() {
            return false;
        }
        let mark = key_mark(prop);
        self.marks
            .iter()
            .zip(&self.props)
            .any(|(have, defined)| *have == mark && defined == prop)
    }

    fn push(&mut self, prop: Cow<'a, str>) {
        self.props.push(prop);
    }
}

/// A key's length and leading bytes in one word, as a filter ahead of the
/// string comparison: keys with different marks differ, and keys of up to
/// seven bytes (the hashed keys of compiled styles) each have their own.
fn key_mark(key: &str) -> u64 {
    let bytes = key.as_bytes();
    let mut word = [0u8; 8];
    for (slot, byte) in word.iter_mut().zip(bytes) {
        *slot = *byte;
    }
    u64::from_le_bytes(word) ^ ((bytes.len() as u64) << 56)
}

// parity: styleq@0.2.1 (cache-free path; the WeakMap cache is behaviorally
// transparent for identical style sequences).
fn styleq<'a>(
    args: &[&'a EvalValue],
) -> Result<(String, Option<JsObjectMap>, EvalValue), StylexError> {
    let props: usize = args
        .iter()
        .map(|arg| match arg {
            EvalValue::Obj(style) => style.len(),
            _ => 0,
        })
        .sum();
    let mut defined = Defined::with_capacity(props);
    // Class names in pop order; each chunk (one compiled style) is prepended
    // to the result, so the chunks are replayed back to front at the end.
    let mut classes: Vec<&'a str> = Vec::with_capacity(props);
    let mut chunk_ends: Vec<usize> = Vec::with_capacity(args.len());
    let mut class_len = 0usize;
    let mut inline_style: Option<JsObjectMap> = None;
    let mut debug = EvalValue::Str(String::new());
    // Last argument first; array elements go on the stack and drain before
    // the walk moves to the next argument, which is styleq's single LIFO.
    let mut stack: Vec<&'a EvalValue> = Vec::new();
    let mut remaining = args.iter().rev();

    while let Some(style) = stack.pop().or_else(|| remaining.next().copied()) {
        match style {
            EvalValue::Null | EvalValue::Undefined | EvalValue::Bool(false) => {}
            EvalValue::Arr(items) => stack.extend(items.iter()),
            // `true` and numbers have no enumerable own props upstream.
            EvalValue::Bool(true) | EvalValue::Num(_) => {}
            // styleq's `for..in` over a primitive string enumerates its
            // UTF-16 index keys ({0: 'a', 1: 'b'}) down the inline branch.
            EvalValue::Str(s) => {
                let mut sub_style = JsObjectMap::new();
                defined.begin_style();
                for (i, unit) in s.encode_utf16().enumerate() {
                    let prop = i.to_string();
                    if defined.contains(&prop) {
                        continue;
                    }
                    let value = String::from_utf16(&[unit])
                        .map_err(|_| StylexError::lone_surrogate("a styleq string argument"))?;
                    sub_style.insert(&prop, EvalValue::Str(value));
                    defined.push(Cow::Owned(prop));
                }
                if !sub_style.is_empty() {
                    if let Some(prev) = inline_style.take() {
                        for (k, v) in prev.entries() {
                            sub_style.insert(k, v.clone());
                        }
                    }
                    inline_style = Some(sub_style);
                }
            }
            EvalValue::Obj(style) => {
                let is_compiled = style
                    .get("$$css")
                    .is_some_and(|v| !matches!(v, EvalValue::Null | EvalValue::Undefined));
                defined.begin_style();
                if is_compiled {
                    let chunk_start = classes.len();
                    for (prop, value) in style.entries() {
                        if prop == "$$css" {
                            if !matches!(value, EvalValue::Bool(true)) {
                                debug = if js_truthy(&debug) {
                                    let mut joined = js_to_string(value);
                                    joined.push_str("; ");
                                    push_js_string(&mut joined, &debug);
                                    EvalValue::Str(joined)
                                } else {
                                    value.clone()
                                };
                            }
                            continue;
                        }
                        match value {
                            EvalValue::Str(s) if !defined.contains(prop) => {
                                defined.push(Cow::Borrowed(prop));
                                // An empty class still occupies a slot in the
                                // chunk only when it is not the first one.
                                if classes.len() > chunk_start || !s.is_empty() {
                                    classes.push(s);
                                    class_len += s.len() + 1;
                                }
                            }
                            EvalValue::Null if !defined.contains(prop) => {
                                defined.push(Cow::Borrowed(prop));
                            }
                            // other value types: styleq logs and skips
                            _ => {}
                        }
                    }
                    if classes.len() > chunk_start {
                        chunk_ends.push(classes.len());
                    }
                } else {
                    let mut sub_style = JsObjectMap::new();
                    for (prop, value) in style.entries() {
                        if matches!(value, EvalValue::Undefined) {
                            continue;
                        }
                        if defined.contains(prop) {
                            continue;
                        }
                        if !matches!(value, EvalValue::Null) {
                            sub_style.insert(prop, value.clone());
                        }
                        defined.push(Cow::Borrowed(prop));
                    }
                    if !sub_style.is_empty() {
                        if let Some(prev) = inline_style.take() {
                            for (k, v) in prev.entries() {
                                sub_style.insert(k, v.clone());
                            }
                        }
                        inline_style = Some(sub_style);
                    }
                }
            }
        }
    }

    let mut class_name = String::with_capacity(class_len.saturating_sub(1));
    for chunk in (0..chunk_ends.len()).rev() {
        let start = if chunk == 0 { 0 } else { chunk_ends[chunk - 1] };
        for class in &classes[start..chunk_ends[chunk]] {
            if !class_name.is_empty() {
                class_name.push(' ');
            }
            class_name.push_str(class);
        }
    }
    Ok((class_name, inline_style, debug))
}

// ---- styleVarsToKeep (the bail path's DCE contract) ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NonNullProps {
    /// keep every prop of the namespace
    True,
    /// null-valued props outside this list may be pruned
    Props(Vec<String>),
}

/// One `state.styleVarsToKeep` tuple: `[objName, propName | true, nonNullProps]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyleVarToKeep {
    pub var_name: String,
    /// `None` mirrors the literal `true` (keep all namespaces).
    pub namespace: Option<String>,
    pub non_null_props: NonNullProps,
}

/// The member-expression evaluation outcome: NonStatic covers `!confident`,
/// nullish results, and cross-file proxies.
#[derive(Debug, Clone, PartialEq)]
pub enum MemberEval {
    NonStatic,
    Value(EvalValue),
}

/// Bail-path per-member accumulator: callers record each unflattened argument's
/// members in traversal order. parity: stylex-props.js bailOut MemberExpression.
#[derive(Debug)]
pub struct StyleVarsCollector {
    bail_out_index: Option<usize>,
    acc: NonNullProps,
}

impl StyleVarsCollector {
    pub fn new(bail_out_index: Option<usize>) -> Self {
        Self {
            bail_out_index,
            acc: NonNullProps::Props(Vec::new()),
        }
    }

    /// `style_map_member` = (objName, propName) for tracked style maps (None =
    /// computed non-literal prop); `eval` runs lazily like upstream's evaluate().
    pub fn record(
        &mut self,
        arg_index: usize,
        style_map_member: Option<(&str, Option<&str>)>,
        eval: impl FnOnce() -> MemberEval,
    ) -> Option<StyleVarToKeep> {
        if self.bail_out_index.is_some_and(|b| arg_index > b) {
            self.acc = NonNullProps::True;
        }
        let style_non_null = if self.acc == NonNullProps::True {
            NonNullProps::True
        } else {
            match eval() {
                MemberEval::NonStatic => {
                    self.acc = NonNullProps::True;
                    NonNullProps::True
                }
                MemberEval::Value(value) => {
                    // Snapshot BEFORE extending: each tuple carries the props
                    // accumulated from previously-visited members only.
                    let NonNullProps::Props(list) = &mut self.acc else {
                        unreachable!("acc checked above");
                    };
                    let snapshot = NonNullProps::Props(list.clone());
                    push_non_null_keys(&value, list);
                    snapshot
                }
            }
        };
        style_map_member.map(|(var_name, namespace)| StyleVarToKeep {
            var_name: var_name.to_string(),
            namespace: namespace.map(str::to_string),
            non_null_props: style_non_null,
        })
    }
}

// `Object.keys(styleValue).filter((key) => styleValue[key] !== null)` — note
// undefined values pass the !== null filter.
fn push_non_null_keys(value: &EvalValue, keys: &mut Vec<String>) {
    match value {
        EvalValue::Obj(map) => {
            keys.reserve(map.len());
            keys.extend(
                map.entries()
                    .filter(|(_, v)| !matches!(v, EvalValue::Null))
                    .map(|(k, _)| k.to_owned()),
            );
        }
        EvalValue::Arr(items) => keys.extend(
            (0..items.len())
                .filter(|i| !matches!(items[*i], EvalValue::Null))
                .map(|i| i.to_string()),
        ),
        EvalValue::Str(s) => keys.extend((0..s.encode_utf16().count()).map(|i| i.to_string())),
        _ => {}
    }
}

fn js_truthy(value: &EvalValue) -> bool {
    match value {
        EvalValue::Null | EvalValue::Undefined | EvalValue::Bool(false) => false,
        EvalValue::Bool(true) | EvalValue::Obj(_) | EvalValue::Arr(_) => true,
        EvalValue::Str(s) => !s.is_empty(),
        EvalValue::Num(n) => *n != 0.0 && !n.is_nan(),
    }
}

fn push_js_string(out: &mut String, value: &EvalValue) {
    match value {
        EvalValue::Str(s) => out.push_str(s),
        other => out.push_str(&js_to_string(other)),
    }
}

// JS template-literal coercion (`${value}`).
fn js_to_string(value: &EvalValue) -> String {
    match value {
        EvalValue::Str(s) => s.clone(),
        EvalValue::Num(n) => js_number_to_string(*n),
        EvalValue::Bool(b) => b.to_string(),
        EvalValue::Null => "null".to_string(),
        EvalValue::Undefined => "undefined".to_string(),
        EvalValue::Obj(_) => "[object Object]".to_string(),
        EvalValue::Arr(items) => items
            .iter()
            .map(|v| match v {
                EvalValue::Null | EvalValue::Undefined => String::new(),
                other => js_to_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
    }
}

// parity: runtime toKebabCase — `str.replace(/([A-Z])/g, '-$1').toLowerCase()`.
// Whole-string lowercasing keeps ES final-sigma context ("ΟΣ" → "ος").
fn to_kebab_case(s: &str) -> String {
    let mut dashed = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            dashed.push('-');
        }
        dashed.push(c);
    }
    dashed.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> EvalValue {
        EvalValue::Str(v.to_string())
    }

    fn obj(entries: &[(&str, EvalValue)]) -> EvalValue {
        EvalValue::Obj(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect::<JsObjectMap>()
                .into(),
        )
    }

    fn compiled(entries: &[(&str, EvalValue)]) -> EvalValue {
        let mut all: Vec<(&str, EvalValue)> = entries.to_vec();
        all.push(("$$css", EvalValue::Bool(true)));
        obj(&all)
    }

    // The `styles` fixture from the oracle probes 2026-08-27:
    // a: color red + marginStart 4, b: color blue + width 10, c: color null + width 20.
    fn ns_a() -> EvalValue {
        compiled(&[("kMwMTN", s("x1e2nbdu")), ("keTefX", s("xdwrcjd"))])
    }
    fn ns_b() -> EvalValue {
        compiled(&[("kMwMTN", s("xju2f9n")), ("kzqmXN", s("x1fsd2vl"))])
    }
    fn ns_c() -> EvalValue {
        compiled(&[("kMwMTN", EvalValue::Null), ("kzqmXN", s("xw4jnvo"))])
    }

    fn class_of(result: &JsObjectMap) -> Option<String> {
        result.get("className").map(|v| match v {
            EvalValue::Str(s) => s.clone(),
            _ => panic!("className must be a string"),
        })
    }

    #[test]
    fn last_wins_and_null_deletes() {
        let merged = merge_props(&[ns_a(), ns_b()]).unwrap();
        assert_eq!(
            class_of(&merged).as_deref(),
            Some("xdwrcjd xju2f9n x1fsd2vl")
        );
        let merged = merge_props(&[ns_a(), ns_c()]).unwrap();
        assert_eq!(class_of(&merged).as_deref(), Some("xdwrcjd xw4jnvo"));
        assert_eq!(merge_props(&[ns_c()]).unwrap().len(), 1);
        let c_only: JsObjectMap = [("kMwMTN".to_string(), EvalValue::Null)]
            .into_iter()
            .collect();
        let mut with_css = c_only;
        with_css.insert("$$css", EvalValue::Bool(true));
        assert!(
            merge_props(&[EvalValue::Obj(with_css.into())])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn array_args_flatten_lifo() {
        let merged = merge_props(&[EvalValue::Arr(vec![ns_a(), ns_b()])]).unwrap();
        assert_eq!(
            class_of(&merged).as_deref(),
            Some("xdwrcjd xju2f9n x1fsd2vl")
        );
    }

    #[test]
    fn inline_styles_merge_and_shadow() {
        // props(styles.a, { kMwMTN: 'z' }) — oracle: className xdwrcjd, style {kMwMTN: z}.
        let merged = merge_props(&[ns_a(), obj(&[("kMwMTN", s("z"))])]).unwrap();
        assert_eq!(class_of(&merged).as_deref(), Some("xdwrcjd"));
        assert_eq!(merged.get("style"), Some(&obj(&[("kMwMTN", s("z"))])));
        // Two inline objects: first arg's key order, later values win.
        let merged = merge_props(&[
            obj(&[("color", s("x")), ("width", EvalValue::Num(1.0))]),
            obj(&[("color", s("y")), ("height", EvalValue::Num(2.0))]),
        ])
        .unwrap();
        let EvalValue::Obj(style) = merged.get("style").unwrap() else {
            panic!("style object expected");
        };
        assert_eq!(
            style.keys().collect::<Vec<_>>(),
            vec!["width", "color", "height"]
        );
        assert_eq!(style.get("color"), Some(&s("y")));
    }

    #[test]
    fn debug_strings_join_in_source_order() {
        let a = obj(&[("k1", s("c1")), ("$$css", s("f.ts:3"))]);
        let b = obj(&[("k2", s("c2")), ("$$css", s("f.ts:4"))]);
        let merged = merge_props(&[a, b]).unwrap();
        assert_eq!(merged.get("data-style-src"), Some(&s("f.ts:3; f.ts:4")));
        // Oracle-pinned quirk: `$$css: false` chunks survive as booleans.
        let weird = obj(&[("$$css", EvalValue::Bool(false)), ("foo", s("bar"))]);
        let merged = merge_props(&[weird]).unwrap();
        assert_eq!(class_of(&merged).as_deref(), Some("bar"));
        assert_eq!(merged.get("data-style-src"), Some(&EvalValue::Bool(false)));
    }

    #[test]
    fn string_args_enumerate_utf16_indices() {
        // parity: styleq's for..in over a primitive string — oracle emits
        // {style: {"0": "a", "1": "b"}} for props(['ab']).
        let merged = merge_props(&[s("ab")]).unwrap();
        let EvalValue::Obj(style) = merged.get("style").unwrap() else {
            panic!("style object expected");
        };
        assert_eq!(style.get("0"), Some(&s("a")));
        assert_eq!(style.get("1"), Some(&s("b")));
        assert!(merge_props(&[s("")]).unwrap().is_empty());
        // Index keys already defined by a later-popped arg stay first-wins.
        let merged = merge_props(&[s("xy"), s("ab")]).unwrap();
        let EvalValue::Obj(style) = merged.get("style").unwrap() else {
            panic!("style object expected");
        };
        assert_eq!(style.get("0"), Some(&s("a")));
        // Divergence pin (rust rejects more): astral chars would split into
        // lone surrogates upstream; we hard-error instead of corrupting.
        let err = merge_props(&[s("😀")]).unwrap_err();
        assert_eq!(err.code, crate::errors::ErrorCode::UnsupportedApi);
    }

    #[test]
    fn kebab_case_lowercases_whole_string() {
        // Oracle probe 2026-08-28: attrs({'ΟΣ': 'v'}) → style "ος:v" (final sigma).
        let merged = merge_attrs(&[obj(&[("ΟΣ", s("v"))])]).unwrap();
        assert_eq!(merged.get("style"), Some(&s("ος:v")));
        let merged = merge_attrs(&[obj(&[("backgroundColor", s("red"))])]).unwrap();
        assert_eq!(merged.get("style"), Some(&s("background-color:red")));
    }

    #[test]
    fn attrs_keeps_custom_property_keys_verbatim() {
        // Oracle probes against the 0.19.2 runtime: only a leading `--`
        // exempts a key from kebab-casing.
        let merged = merge_attrs(&[obj(&[
            ("--myVar", s("1")),
            ("--Mixed_Case-Name", s("2")),
            ("--", s("3")),
            ("-webkitBoxOrient", s("vertical")),
            ("WebkitLineClamp", s("4")),
            ("my--Var", s("5")),
        ])])
        .unwrap();
        assert_eq!(
            merged.get("style"),
            Some(&s(
                "--myVar:1;--Mixed_Case-Name:2;--:3;-webkit-box-orient:vertical;-webkit-line-clamp:4;my---var:5"
            ))
        );
    }

    #[test]
    fn attrs_reshapes_and_serializes() {
        let merged = merge_attrs(&[ns_a(), ns_b()]).unwrap();
        assert_eq!(merged.get("class"), Some(&s("xdwrcjd xju2f9n x1fsd2vl")));
        let merged = merge_attrs(&[
            ns_a(),
            obj(&[
                ("width", EvalValue::Num(5.0)),
                ("backgroundColor", s("red")),
            ]),
        ])
        .unwrap();
        assert_eq!(
            merged.get("style"),
            Some(&s("width:5;background-color:red"))
        );
        // Whole-map arg: object values coerce like the runtime template literal.
        let whole = obj(&[("a", ns_a())]);
        let merged = merge_attrs(&[whole]).unwrap();
        assert_eq!(merged.get("style"), Some(&s("a:[object Object]")));
    }

    #[test]
    fn plan_table_orders_keys_bit_reversed() {
        let options = ResolvedOptions::default();
        // props(x ? a : null, y && b) — oracle emits keys 0, 2, 1, 3.
        let args = [
            MergeArg::Conditional {
                primary: NullableStyle::Style(ns_a()),
                fallback: NullableStyle::Null,
            },
            MergeArg::LogicalAnd {
                left: NullableStyle::Other,
                right: NullableStyle::Style(ns_b()),
            },
        ];
        let MergePlan::Table(entries) = plan_merge(&args, MergeMode::Props, &options).unwrap()
        else {
            panic!("expected table");
        };
        let keys: Vec<u32> = entries.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec![0, 2, 1, 3]);
        assert!(entries[0].1.is_empty());
        assert_eq!(class_of(&entries[1].1).as_deref(), Some("x1e2nbdu xdwrcjd"));
        assert_eq!(class_of(&entries[2].1).as_deref(), Some("xju2f9n x1fsd2vl"));
        assert_eq!(
            class_of(&entries[3].1).as_deref(),
            Some("xdwrcjd xju2f9n x1fsd2vl")
        );
    }

    #[test]
    fn plan_bail_policies() {
        let options = ResolvedOptions::default();
        let cond = |v: EvalValue| MergeArg::Conditional {
            primary: NullableStyle::Style(v),
            fallback: NullableStyle::Null,
        };
        // five conditionals
        let args: Vec<MergeArg> = (0..5).map(|_| cond(ns_a())).collect();
        assert_eq!(
            plan_merge(&args, MergeMode::Props, &options).unwrap(),
            MergePlan::Bail {
                bail_out_index: None
            }
        );
        // non-static arg records the first bail index
        let args = [
            MergeArg::Resolved(NullableStyle::Style(ns_a())),
            MergeArg::Resolved(NullableStyle::Other),
            MergeArg::Resolved(NullableStyle::Other),
        ];
        assert_eq!(
            plan_merge(&args, MergeMode::Props, &options).unwrap(),
            MergePlan::Bail {
                bail_out_index: Some(1)
            }
        );
        // non-&& logical overwrites the index and stops parsing
        let args = [
            MergeArg::Resolved(NullableStyle::Other),
            MergeArg::NonAndLogical,
            MergeArg::Resolved(NullableStyle::Style(ns_a())),
        ];
        assert_eq!(
            plan_merge(&args, MergeMode::Props, &options).unwrap(),
            MergePlan::Bail {
                bail_out_index: Some(1)
            }
        );
        // static left of && bails
        let args = [MergeArg::LogicalAnd {
            left: NullableStyle::Style(ns_a()),
            right: NullableStyle::Style(ns_b()),
        }];
        assert_eq!(
            plan_merge(&args, MergeMode::Props, &options).unwrap(),
            MergePlan::Bail {
                bail_out_index: Some(0)
            }
        );
        // flag off + any conditional bails
        let flag_off = ResolvedOptions {
            enable_inlined_conditional_merge: false,
            ..ResolvedOptions::default()
        };
        assert_eq!(
            plan_merge(&[cond(ns_a())], MergeMode::Props, &flag_off).unwrap(),
            MergePlan::Bail {
                bail_out_index: None
            }
        );
    }

    #[test]
    fn legacy_projects_styleq_to_the_class_name_only() {
        assert_eq!(
            merge_legacy(&[ns_a(), ns_b()]).unwrap(),
            "xdwrcjd xju2f9n x1fsd2vl"
        );
        // Empty merges keep the empty string props() would have dropped.
        assert_eq!(merge_legacy(&[]).unwrap(), "");
        let only_null = compiled(&[("kMwMTN", EvalValue::Null)]);
        assert_eq!(merge_legacy(&[only_null]).unwrap(), "");
        // The debug channel props() surfaces as data-style-src is discarded.
        let debug = obj(&[("k1", s("c1")), ("$$css", s("f.ts:3"))]);
        assert_eq!(merge_legacy(&[debug]).unwrap(), "c1");
    }

    #[test]
    fn legacy_plan_stops_at_the_first_bail() {
        let options = ResolvedOptions::default();
        // legacy(styles.a, other, flag || styles.e): legacy records index 1,
        // props keeps scanning and the non-&& logical overwrites it with 2.
        let args = [
            MergeArg::Resolved(NullableStyle::Style(ns_a())),
            MergeArg::Resolved(NullableStyle::Other),
            MergeArg::NonAndLogical,
        ];
        assert_eq!(
            plan_legacy_merge(&args, &options).unwrap(),
            MergePlan::Bail {
                bail_out_index: Some(1)
            }
        );
        assert_eq!(
            plan_merge(&args, MergeMode::Props, &options).unwrap(),
            MergePlan::Bail {
                bail_out_index: Some(2)
            }
        );
    }

    #[test]
    fn legacy_plan_leaves_are_strings() {
        let options = ResolvedOptions::default();
        assert_eq!(
            plan_legacy_merge(
                &[MergeArg::Resolved(NullableStyle::Style(ns_a()))],
                &options
            )
            .unwrap(),
            MergePlan::Inlined("x1e2nbdu xdwrcjd".to_string())
        );
        let args = [MergeArg::Conditional {
            primary: NullableStyle::Style(ns_a()),
            fallback: NullableStyle::Null,
        }];
        assert_eq!(
            plan_legacy_merge(&args, &options).unwrap(),
            MergePlan::Table(vec![
                (0, String::new()),
                (1, "x1e2nbdu xdwrcjd".to_string())
            ])
        );
        // Five conditionals blow the cap before any argument bails, so the
        // index stays unset even though legacy breaks out of the loop.
        let args: Vec<MergeArg> = (0..5)
            .map(|_| MergeArg::Conditional {
                primary: NullableStyle::Style(ns_a()),
                fallback: NullableStyle::Null,
            })
            .collect();
        assert_eq!(
            plan_legacy_merge(&args, &options).unwrap(),
            MergePlan::Bail {
                bail_out_index: None
            }
        );
    }

    #[test]
    fn collector_mirrors_upstream_accumulator() {
        // props(styles.a, ext, styles.c) — bailOutIndex 1 (oracle: c keeps its null prop).
        let mut collector = StyleVarsCollector::new(Some(1));
        let kept = collector
            .record(0, Some(("styles", Some("a"))), || MemberEval::Value(ns_a()))
            .unwrap();
        assert_eq!(kept.non_null_props, NonNullProps::Props(vec![]));
        let kept = collector
            .record(2, Some(("styles", Some("c"))), || {
                panic!("eval must not run past the bail index")
            })
            .unwrap();
        assert_eq!(kept.non_null_props, NonNullProps::True);

        // props(ext, styles.c), bail at 0: members past the bail index keep
        // everything (oracle keeps c's null prop here).
        let mut collector = StyleVarsCollector::new(Some(0));
        let kept = collector
            .record(1, Some(("styles", Some("c"))), || MemberEval::Value(ns_c()))
            .unwrap();
        assert_eq!(kept.non_null_props, NonNullProps::True);

        // Non-static member poisons the accumulator globally.
        let mut collector = StyleVarsCollector::new(None);
        assert!(
            collector
                .record(0, None, || MemberEval::NonStatic)
                .is_none()
        );
        let kept = collector
            .record(0, Some(("styles", None)), || panic!("acc is already true"))
            .unwrap();
        assert_eq!(kept.namespace, None);
        assert_eq!(kept.non_null_props, NonNullProps::True);

        // Snapshot-before-extend: the second member sees the first's keys only.
        let mut collector = StyleVarsCollector::new(None);
        collector.record(0, Some(("styles", Some("c"))), || MemberEval::Value(ns_c()));
        let kept = collector
            .record(1, Some(("styles", Some("a"))), || MemberEval::Value(ns_a()))
            .unwrap();
        // Object.keys includes $$css: true (non-null), so it lands in the list.
        assert_eq!(
            kept.non_null_props,
            NonNullProps::Props(vec!["kzqmXN".to_string(), "$$css".to_string()])
        );
    }
}
