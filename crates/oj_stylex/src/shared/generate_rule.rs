//! Style entry → className + injectable CSS rule.
// parity: babel-plugin src/shared/utils/{convert-to-className,generate-css-rule}.js

use crate::errors::StylexError;
use crate::fxhash::FxHashMap;
use crate::hash::push_hash;
use crate::options::ResolvedOptions;
use crate::rules::StylexRule;
use crate::shared::dashify::dashed_key;
use crate::shared::fallbacks::{has_var_fallback, variable_fallbacks};
use crate::shared::flatten::{PreRuleValue, StyleScalar};
use crate::shared::normalize_value::CssValueError;
use crate::shared::priorities::get_priority;
use crate::shared::pseudo_sort::{sort_at_rules, sort_pseudos};
use crate::shared::rtl::{RtlContext, generate_ltr, generate_rtl};
use crate::shared::transform_value::{transform_value_num, transform_value_str};
use std::borrow::Cow;
use std::cell::RefCell;
use std::hash::{BuildHasher, BuildHasherDefault};
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

fn css_value_error(e: CssValueError) -> StylexError {
    match e {
        CssValueError::UnclosedFunction => StylexError::unclosed_function(),
        CssValueError::UnclosedString => StylexError::unclosed_string(),
        CssValueError::EmptyValue => StylexError::empty_value(),
    }
}

fn transform_scalar(
    key: &str,
    scalar: &StyleScalar,
    font_size_px_to_rem: bool,
) -> Result<String, StylexError> {
    match scalar {
        StyleScalar::Str(s) => {
            transform_value_str(key, s.as_ref(), font_size_px_to_rem).map_err(css_value_error)
        }
        StyleScalar::Num(n) => {
            transform_value_num(key, *n, font_size_px_to_rem).map_err(css_value_error)
        }
        // normalizeValue short-circuits on nullish, so the declaration is empty.
        StyleScalar::Undefined => Ok(String::new()),
    }
}

/// One compiled declaration, memo-shared behind `Rc`: a hit is a refcount bump.
pub type CompiledDecl = Rc<StylexRule>;

struct ConvertMemo {
    key: Vec<u8>,
    /// Miss-path scratch: the class-name hash input, then each rule's text.
    text: String,
    map: FxHashMap<Arc<[u8]>, CompiledDecl>,
}

thread_local! {
    static CONVERT_MEMO: RefCell<ConvertMemo> = RefCell::new(ConvertMemo {
        key: Vec::new(),
        text: String::new(),
        map: FxHashMap::default(),
    });
}

const CONVERT_MEMO_CAP: usize = 1 << 16;
const SHARD_COUNT: usize = 64;
const SHARD_CAP: usize = CONVERT_MEMO_CAP / SHARD_COUNT;

type SharedMemo = FxHashMap<Arc<[u8]>, StylexRule>;

// Second level behind the thread-local memo: worker pools (batch threads, oj's
// blocking pool) otherwise recompute every declaration once per thread.
static SHARED_MEMO: [Mutex<SharedMemo>; SHARD_COUNT] =
    [const { Mutex::new(FxHashMap::with_hasher(BuildHasherDefault::new())) }; SHARD_COUNT];

// Threads never share a rule's `Arc<str>`s: every registry clone/drop would
// then bounce the refcount line between cores (+20% create time at 18 threads).
fn private_copy(rule: &StylexRule) -> StylexRule {
    StylexRule {
        class_name: Arc::from(&*rule.class_name),
        ltr: Arc::from(&*rule.ltr),
        rtl: rule.rtl.as_deref().map(Arc::from),
        const_key: rule.const_key.as_deref().map(Arc::from),
        const_val: rule.const_val.clone(),
        priority: rule.priority,
    }
}

fn shard_for(key: &[u8]) -> &'static Mutex<SharedMemo> {
    let hash = BuildHasherDefault::<crate::fxhash::FxHasher>::new().hash_one(key);
    &SHARED_MEMO[(hash >> (64 - SHARD_COUNT.trailing_zeros())) as usize]
}

fn lock(shard: &Mutex<SharedMemo>) -> MutexGuard<'_, SharedMemo> {
    shard.lock().unwrap_or_else(PoisonError::into_inner)
}

// Memo keys are raw bytes, framed with two values UTF-8 never contains: every
// text segment ends with `KEY_END` and every list with `KEY_LIST_END`, and a
// number is a tag plus exactly eight bytes. That framing is injective, so
// equal keys mean equal inputs.
const KEY_END: u8 = 0xff;
const KEY_LIST_END: u8 = 0xfe;

fn push_memo_text(key: &mut Vec<u8>, text: &str) {
    key.extend_from_slice(text.as_bytes());
    key.push(KEY_END);
}

fn push_memo_scalar(key: &mut Vec<u8>, scalar: &StyleScalar) {
    match scalar {
        StyleScalar::Str(s) => {
            key.push(b's');
            push_memo_text(key, s);
        }
        StyleScalar::Num(n) => {
            key.push(b'n');
            key.extend_from_slice(&n.to_bits().to_le_bytes());
        }
        StyleScalar::Undefined => key.push(b'u'),
    }
}

// The keyPath carries the property too; the three condition classes are
// disjoint prefix families, so each list is a filter over the same slice.
fn pseudos_of<'k>(key_path: &'k [Cow<'_, str>]) -> impl Iterator<Item = &'k str> {
    key_path
        .iter()
        .map(Cow::as_ref)
        .filter(|k| k.starts_with(':') || k.starts_with('['))
}

fn at_rules_of<'k>(key_path: &'k [Cow<'_, str>]) -> impl Iterator<Item = &'k str> {
    key_path
        .iter()
        .map(Cow::as_ref)
        .filter(|k| k.starts_with('@'))
}

fn const_rules_of<'k>(key_path: &'k [Cow<'_, str>]) -> impl Iterator<Item = &'k str> {
    key_path
        .iter()
        .map(Cow::as_ref)
        .filter(|k| k.starts_with("var(--"))
}

// A condition opens with one of these bytes: a keyPath without any (the flat
// `property: value` entry) has three empty condition lists.
fn may_be_condition(segment: &str) -> bool {
    matches!(segment.as_bytes().first(), Some(b':' | b'[' | b'@' | b'v'))
}

// The fingerprint covers every option the uncached path reads: px-to-rem,
// the class-name prefix, and the three RtlContext fields.
fn encode_memo_key(
    key: &mut Vec<u8>,
    property: &str,
    value: &PreRuleValue<'_>,
    key_path: &[Cow<'_, str>],
    options: &ResolvedOptions,
) {
    let resolution = match options.style_resolution {
        crate::options::StyleResolution::PropertySpecificity => 0,
        crate::options::StyleResolution::ApplicationOrder => 1,
        crate::options::StyleResolution::LegacyExpandShorthands => 2,
    };
    key.push(
        u8::from(options.enable_font_size_px_to_rem)
            | resolution << 1
            | u8::from(options.enable_logical_styles_polyfill) << 3
            | u8::from(options.enable_legacy_value_flipping) << 4,
    );
    push_memo_text(key, &options.class_name_prefix);
    push_memo_text(key, property);
    match value {
        PreRuleValue::Single(scalar) => {
            key.push(b'S');
            push_memo_scalar(key, scalar);
        }
        PreRuleValue::Multi(scalars) => {
            key.push(b'M');
            for scalar in scalars {
                push_memo_scalar(key, scalar);
            }
            key.push(KEY_LIST_END);
        }
    }
    if !key_path.iter().any(|segment| may_be_condition(segment)) {
        key.extend_from_slice(&[KEY_LIST_END; 3]);
        return;
    }
    for item in pseudos_of(key_path) {
        push_memo_text(key, item);
    }
    key.push(KEY_LIST_END);
    for item in at_rules_of(key_path) {
        push_memo_text(key, item);
    }
    key.push(KEY_LIST_END);
    for item in const_rules_of(key_path) {
        push_memo_text(key, item);
    }
    key.push(KEY_LIST_END);
}

/// Conditions are read from `key_path` in keyPath order; sorting for the hash
/// input and selector happens here, on the miss path only.
pub fn convert_style_to_class_name(
    property: &str,
    value: &PreRuleValue<'_>,
    key_path: &[Cow<'_, str>],
    options: &ResolvedOptions,
) -> Result<CompiledDecl, StylexError> {
    CONVERT_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        let ConvertMemo { key, text, map } = &mut *memo;
        key.clear();
        encode_memo_key(key, property, value, key_path, options);
        if let Some(hit) = map.get(key.as_slice()) {
            return Ok(Rc::clone(hit));
        }
        convert_memo_miss(key, text, map, property, value, key_path, options)
    })
}

#[inline(never)]
fn convert_memo_miss(
    key: &[u8],
    text: &mut String,
    map: &mut FxHashMap<Arc<[u8]>, CompiledDecl>,
    property: &str,
    value: &PreRuleValue<'_>,
    key_path: &[Cow<'_, str>],
    options: &ResolvedOptions,
) -> Result<CompiledDecl, StylexError> {
    let shard = shard_for(key);
    let shared = lock(shard)
        .get_key_value(key)
        .map(|(k, v)| (Arc::clone(k), private_copy(v)));
    let (shared_key, rule) = match shared {
        Some(hit) => hit,
        None => {
            let _t = crate::timings::start(crate::timings::Stage::CreateMiss);
            let pseudos: Vec<&str> = pseudos_of(key_path).collect();
            let at_rules: Vec<&str> = at_rules_of(key_path).collect();
            let const_rules: Vec<&str> = const_rules_of(key_path).collect();
            let rule = convert_style_to_class_name_uncached(
                property,
                value,
                &pseudos,
                &at_rules,
                &const_rules,
                options,
                text,
            )?;
            let shared_key: Arc<[u8]> = Arc::from(key);
            let mut shard = lock(shard);
            if shard.len() >= SHARD_CAP {
                shard.clear();
            }
            shard
                .entry(Arc::clone(&shared_key))
                .or_insert_with(|| private_copy(&rule));
            (shared_key, rule)
        }
    };
    let computed = Rc::new(rule);
    if map.len() >= CONVERT_MEMO_CAP {
        map.clear();
    }
    map.insert(shared_key, Rc::clone(&computed));
    Ok(computed)
}

fn convert_style_to_class_name_uncached(
    property: &str,
    value: &PreRuleValue<'_>,
    pseudos: &[&str],
    at_rules: &[&str],
    const_rules: &[&str],
    options: &ResolvedOptions,
    text: &mut String,
) -> Result<StylexRule, StylexError> {
    // Upstream evaluates the sorting PreRule.pseudos/atRules getters as call
    // arguments, so a collation error precedes any value-normalization error.
    let sorted_pseudos = sort_pseudos(pseudos)?;
    let sorted_at_rules = sort_at_rules(at_rules);
    // A stable sort of an already sorted list is that list: the const-free
    // case reads the at-rule order as is.
    let sorted_at_and_const: Cow<[&str]> = if const_rules.is_empty() {
        Cow::Borrowed(&sorted_at_rules)
    } else {
        let mut at_and_const = sorted_at_rules.clone();
        at_and_const.extend(const_rules.iter().copied());
        Cow::Owned(sort_at_rules(&at_and_const))
    };

    let dashed = dashed_key(property);

    let px_to_rem = options.enable_font_size_px_to_rem;
    let single;
    let multi;
    let mut values: &[String] = match value {
        PreRuleValue::Single(scalar) => {
            single = transform_scalar(property, scalar, px_to_rem)?;
            std::slice::from_ref(&single)
        }
        PreRuleValue::Multi(scalars) => {
            multi = scalars
                .iter()
                .map(|s| transform_scalar(property, s, px_to_rem))
                .collect::<Result<Vec<_>, _>>()?;
            &multi
        }
    };
    let fallbacks;
    if matches!(value, PreRuleValue::Multi(_)) && has_var_fallback(values) {
        fallbacks = variable_fallbacks(values)?;
        values = &fallbacks;
    }

    // The hash concatenates with `+` (so a JS `undefined` stringifies) while
    // the declaration is built with `Array.join`, which renders it empty.
    let undefined_single = matches!(value, PreRuleValue::Single(StyleScalar::Undefined));

    // parity: the '<>' prefix and 'null' modifier keep upstream hashes stable;
    // built in place, byte-for-byte `<>{dashed}{values.join(", ")}{modifier}`.
    let modifier_len: usize = sorted_pseudos.iter().map(|s| s.len()).sum::<usize>()
        + sorted_at_and_const.iter().map(|s| s.len()).sum::<usize>();
    let hash_input = &mut *text;
    hash_input.clear();
    hash_input.push_str("<>");
    hash_input.push_str(&dashed);
    if undefined_single {
        hash_input.push_str("undefined");
    } else {
        for (i, v) in values.iter().enumerate() {
            if i > 0 {
                hash_input.push_str(", ");
            }
            hash_input.push_str(v);
        }
    }
    if modifier_len == 0 {
        hash_input.push_str("null");
    } else {
        for p in &sorted_pseudos {
            hash_input.push_str(p);
        }
        for a in sorted_at_and_const.iter() {
            hash_input.push_str(a);
        }
    }
    let mut class_name = String::with_capacity(options.class_name_prefix.len() + 7);
    class_name.push_str(&options.class_name_prefix);
    push_hash(&mut class_name, hash_input);

    Ok(css_rule_in(
        text,
        &class_name,
        &dashed,
        values,
        &sorted_pseudos,
        &sorted_at_rules,
        const_rules,
        RtlContext::of(options),
    ))
}

pub fn generate_css_rule(
    class_name: &str,
    key: &str,
    values: &[String],
    pseudos: &[&str],
    at_rules: &[&str],
    const_rules: &[&str],
    ctx: RtlContext,
) -> StylexRule {
    css_rule_in(
        &mut String::new(),
        class_name,
        key,
        values,
        pseudos,
        at_rules,
        const_rules,
        ctx,
    )
}

// `generate_css_rule` with the rule text assembled in the caller's scratch
// buffer: each side costs one allocation, its `Arc<str>`.
#[allow(clippy::too_many_arguments)]
fn css_rule_in(
    text: &mut String,
    class_name: &str,
    key: &str,
    values: &[String],
    pseudos: &[&str],
    at_rules: &[&str],
    const_rules: &[&str],
    ctx: RtlContext,
) -> StylexRule {
    let mut rtl_decls = String::new();
    for v in values {
        if let Some((k, v)) = generate_rtl(key, v, ctx) {
            if !rtl_decls.is_empty() {
                rtl_decls.push(';');
            }
            rtl_decls.push_str(&k);
            rtl_decls.push(':');
            rtl_decls.push_str(&v);
        }
    }

    text.clear();
    write_nested_css_rule(text, class_name, pseudos, at_rules, const_rules, |out| {
        for (i, v) in values.iter().enumerate() {
            if i > 0 {
                out.push(';');
            }
            let (k, v) = generate_ltr(key, v, ctx);
            out.push_str(&k);
            out.push(':');
            out.push_str(&v);
        }
    });
    let ltr: Arc<str> = Arc::from(text.as_str());
    let rtl: Option<Arc<str>> = if rtl_decls.is_empty() {
        None
    } else {
        text.clear();
        write_nested_css_rule(text, class_name, pseudos, at_rules, const_rules, |out| {
            out.push_str(&rtl_decls);
        });
        Some(Arc::from(text.as_str()))
    };

    let priority = get_priority(key)
        + pseudos.iter().map(|p| get_priority(p)).sum::<f64>()
        + at_rules.iter().map(|a| get_priority(a)).sum::<f64>()
        + const_rules.iter().map(|c| get_priority(c)).sum::<f64>();

    StylexRule {
        class_name: class_name.into(),
        ltr,
        rtl,
        const_key: None,
        const_val: None,
        priority,
    }
}

const THUMB_VARIANTS: [&str; 3] = [
    "::-webkit-slider-thumb",
    "::-moz-range-thumb",
    "::-ms-thumb",
];

// parity: generate-css-rule.js buildNestedCSSRule, appended to `out`.
fn write_nested_css_rule(
    out: &mut String,
    class_name: &str,
    pseudos: &[&str],
    at_rules: &[&str],
    const_rules: &[&str],
    decls: impl FnOnce(&mut String),
) {
    // Pseudo-classes before pseudo-elements, insertion order within each —
    // the two-buffer concat of upstream, built as one string in two passes.
    let mut pseudo = String::new();
    for p in pseudos {
        if *p != "::thumb" && !p.starts_with("::") {
            pseudo.push_str(p);
        }
    }
    for p in pseudos {
        if *p != "::thumb" && p.starts_with("::") {
            pseudo.push_str(p);
        }
    }
    let combined_len = at_rules.len() + const_rules.len();
    let class_copies = 1 + usize::from(pseudo.contains(":where(")) + combined_len;

    // Nesting parity: iterating at_rules then const_rules and wrapping each
    // time leaves the last-iterated outermost; emit them reversed in one pass.
    for at_rule in at_rules.iter().chain(const_rules.iter()).rev() {
        out.push_str(at_rule);
        out.push('{');
    }
    let selector = |out: &mut String| {
        for _ in 0..class_copies {
            out.push('.');
            out.push_str(class_name);
        }
        out.push_str(&pseudo);
    };
    if pseudos.contains(&"::thumb") {
        for (i, suffix) in THUMB_VARIANTS.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            selector(out);
            out.push_str(suffix);
        }
    } else {
        selector(out);
    }
    out.push('{');
    decls(out);
    out.push('}');
    for _ in 0..combined_len {
        out.push('}');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> ResolvedOptions {
        ResolvedOptions::default()
    }

    fn memo_key(property: &str, value: PreRuleValue<'_>, key_path: &[&str]) -> Vec<u8> {
        let key_path: Vec<Cow<'_, str>> = key_path.iter().map(|k| Cow::Borrowed(*k)).collect();
        let mut key = Vec::new();
        encode_memo_key(&mut key, property, &value, &key_path, &defaults());
        key
    }

    fn text(value: &str) -> StyleScalar<'_> {
        StyleScalar::Str(Cow::Borrowed(value))
    }

    // Two declarations may share a memo entry only if they compile alike, so
    // moving bytes between neighbouring fields must change the key.
    #[test]
    fn memo_keys_keep_their_fields_apart() {
        let single = |v| PreRuleValue::Single(v);
        let keys = [
            memo_key("color", single(text("red")), &["color"]),
            memo_key("colo", single(text("rred")), &["colo"]),
            memo_key("color", single(text("1")), &["color"]),
            memo_key("color", single(StyleScalar::Num(1.0)), &["color"]),
            memo_key("color", single(StyleScalar::Num(-1.0)), &["color"]),
            memo_key("color", PreRuleValue::Multi(vec![text("red")]), &["color"]),
            memo_key(
                "color",
                PreRuleValue::Multi(vec![text("r"), text("ed")]),
                &["color"],
            ),
            memo_key("color", PreRuleValue::Multi(vec![]), &["color"]),
            memo_key("color", single(text("red")), &[":hover", "color"]),
            memo_key(
                "color",
                single(text("red")),
                &[":hover", ":active", "color"],
            ),
            memo_key(
                "color",
                single(text("red")),
                &[":active", ":hover", "color"],
            ),
            memo_key("color", single(text("red")), &[":hover:active", "color"]),
            memo_key("color", single(text("red")), &["@media print", "color"]),
            memo_key("color", single(text("red")), &["var(--x)", "color"]),
            memo_key("color", single(text("red")), &["[data-x]", "color"]),
        ];
        for (i, a) in keys.iter().enumerate() {
            for b in &keys[..i] {
                assert_ne!(a, b);
            }
        }

        let mut prefixed = Vec::new();
        let options = ResolvedOptions {
            class_name_prefix: "y".into(),
            ..defaults()
        };
        encode_memo_key(
            &mut prefixed,
            "color",
            &single(text("red")),
            &[Cow::Borrowed("color")],
            &options,
        );
        assert_ne!(prefixed, keys[0]);
    }

    // A keyPath whose segments merely look like conditions takes the long
    // encoding and must land on the same key as the shortcut.
    #[test]
    fn memo_key_shortcut_matches_the_full_encoding() {
        let value = || PreRuleValue::Single(text("red"));
        assert_eq!(
            memo_key("color", value(), &["color"]),
            memo_key("color", value(), &["vertical", "color"]),
        );
        assert_eq!(
            memo_key("color", value(), &[]),
            memo_key("color", value(), &["default", "color"]),
        );
    }

    #[test]
    fn known_answer_color_red() {
        let decl = convert_style_to_class_name(
            "color",
            &PreRuleValue::Single(StyleScalar::Str(Cow::Borrowed("red"))),
            &[],
            &defaults(),
        )
        .unwrap();
        assert_eq!(&*decl.class_name, "x1e2nbdu");
        assert_eq!(&*decl.ltr, ".x1e2nbdu{color:red}");
        assert_eq!(decl.rtl, None);
        assert_eq!(decl.priority, 3000.0);
    }

    #[test]
    fn thumb_and_where_selector_shapes() {
        let decl = convert_style_to_class_name(
            "width",
            &PreRuleValue::Single(StyleScalar::Num(16.0)),
            &[Cow::Borrowed(":hover"), Cow::Borrowed("::thumb")],
            &defaults(),
        )
        .unwrap();
        assert_eq!(
            &*decl.ltr,
            format!(
                ".{c}:hover::-webkit-slider-thumb, .{c}:hover::-moz-range-thumb, .{c}:hover::-ms-thumb{{width:16px}}",
                c = decl.class_name
            )
        );
        assert_eq!(decl.priority, 9130.0);

        let decl = convert_style_to_class_name(
            "color",
            &PreRuleValue::Single(StyleScalar::Str(Cow::Borrowed("blue"))),
            &[Cow::Borrowed(":where(.x-default-marker:hover *)")],
            &defaults(),
        )
        .unwrap();
        assert_eq!(&*decl.class_name, "xobp4yc");
        assert_eq!(
            &*decl.ltr,
            ".xobp4yc.xobp4yc:where(.x-default-marker:hover *){color:blue}"
        );
        assert_eq!(decl.priority, 3011.3);
    }
}
