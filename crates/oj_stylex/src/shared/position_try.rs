//! `stylex.positionTry` over an already-evaluated styles object.
// parity: babel-plugin src/shared/stylex-position-try.js + visitors/stylex-position-try.js

use crate::errors::StylexError;
use crate::eval::value::{EvalValue, JsObjectMap};
use crate::hash::hash;
use crate::jsrt::utf16_cmp;
use crate::options::ResolvedOptions;
use crate::rules::StylexRule;
use crate::shared::dashify::dashify;
use crate::shared::flatten::StyleScalar;
use crate::shared::normalize_value::CssValueError;
use crate::shared::resolution::flat_map_expanded_shorthands;
use crate::shared::rtl::{RtlContext, generate_ltr, generate_rtl};
use crate::shared::transform_value::{transform_value_num, transform_value_str};

// parity: visitors/stylex-position-try.js VALID_POSITION_TRY_PROPERTIES
const VALID_POSITION_TRY_PROPERTIES: [&str; 40] = [
    "anchorName",
    "positionAnchor",
    "positionArea",
    "top",
    "right",
    "bottom",
    "left",
    "inset",
    "insetBlock",
    "insetBlockEnd",
    "insetBlockStart",
    "insetInline",
    "insetInlineEnd",
    "insetInlineStart",
    "margin",
    "marginBlock",
    "marginBlockEnd",
    "marginBlockStart",
    "marginInline",
    "marginInlineEnd",
    "marginInlineStart",
    "marginTop",
    "marginBottom",
    "marginLeft",
    "marginRight",
    "width",
    "height",
    "minWidth",
    "minHeight",
    "maxWidth",
    "maxHeight",
    "blockSize",
    "inlineSize",
    "minBlockSize",
    "minInlineSize",
    "maxBlockSize",
    "maxInlineSize",
    "alignSelf",
    "justifySelf",
    "placeSelf",
];

/// Returns the `--`-prefixed name and its single injectable rule (priority 0).
pub fn position_try(
    styles: &EvalValue,
    options: &ResolvedOptions,
) -> Result<(String, StylexRule), StylexError> {
    let styles = assert_valid_position_try(styles)?;
    assert_valid_properties(styles)?;
    position_try_impl(styles, options)
}

/// The callable path (create/defineVars/createTheme FunctionConfig closures):
/// shared fn only — Object.entries coercion, no property validation.
pub fn position_try_shared(
    styles: &EvalValue,
    options: &ResolvedOptions,
) -> Result<(String, StylexRule), StylexError> {
    let entries = crate::shared::view_transition::object_entries_js(
        styles,
        "a nullish positionTry() argument",
    )?;
    position_try_impl(&entries, options)
}

fn position_try_impl(
    styles: &JsObjectMap,
    options: &ResolvedOptions,
) -> Result<(String, StylexRule), StylexError> {
    let expanded = transform_values(&dashify_keys(&expand_scalars(styles, options)?), options)?;

    // Each side is rebuilt from its generated pairs, so a key the generator
    // renames lands under the new name and a collision keeps the last value.
    let ctx = RtlContext::of(options);
    let mut ltr_styles = JsObjectMap::new();
    let mut rtl_styles = JsObjectMap::new();
    for (key, value) in expanded.entries() {
        let EvalValue::Str(value) = value else {
            unreachable!("transform stage only stores strings");
        };
        let (lk, lv) = generate_ltr(key, value, ctx);
        ltr_styles.insert(lk, EvalValue::Str(lv.into_owned()));
        let (rk, rv) = generate_rtl(key, value, ctx).map_or_else(
            || (key.to_string(), value.clone()),
            |(k, v)| (k.into_owned(), v.into_owned()),
        );
        rtl_styles.insert(rk, EvalValue::Str(rv));
    }
    let ltr_string = construct_position_try_string(&ltr_styles);
    let rtl_string = construct_position_try_string(&rtl_styles);

    let name = format!("--{}{}", options.class_name_prefix, hash(&ltr_string));
    let ltr = format!("@position-try {name} {{{ltr_string}}}");
    let rtl = (ltr_string != rtl_string).then(|| format!("@position-try {name} {{{rtl_string}}}"));

    let rule = StylexRule {
        class_name: name.as_str().into(),
        ltr: ltr.into(),
        rtl: rtl.map(Into::into),
        const_key: None,
        const_val: None,
        priority: 0.0,
    };
    Ok((name, rule))
}

// parity: visitors/stylex-position-try.js assertValidPositionTry
fn assert_valid_position_try(styles: &EvalValue) -> Result<&JsObjectMap, StylexError> {
    match styles {
        EvalValue::Obj(map) => Ok(map),
        _ => Err(StylexError::non_style_object("positionTry")),
    }
}

// parity: visitors/stylex-position-try.js assertValidProperties (original keys,
// before any alias expansion or dashify).
fn assert_valid_properties(styles: &JsObjectMap) -> Result<(), StylexError> {
    if styles
        .keys()
        .any(|key| !VALID_POSITION_TRY_PROPERTIES.contains(&key))
    {
        return Err(StylexError::position_try_invalid_property());
    }
    Ok(())
}

// parity: stylex-position-try.js constructPositionTryObj
fn construct_position_try_string(styles: &JsObjectMap) -> String {
    let mut keys: Vec<&str> = styles.keys().collect();
    keys.sort_by(|a, b| utf16_cmp(a, b));
    let mut out = String::new();
    for key in keys {
        let Some(EvalValue::Str(value)) = styles.get(key) else {
            unreachable!("generated declarations are strings");
        };
        out.push_str(&format!("{key}:{value};"));
    }
    out
}

// The three stages `positionTry`, `keyframes` and `viewTransitionClass` pipe a
// style object through. Each rebuilds the object, so duplicate keys collapse
// first-position last-value. positionTry dashifies before it transforms; the
// other two transform first, so their values see the camelCase key.

// parity: preprocessProperties / expandFrameShorthands
pub(crate) fn expand_scalars(
    styles: &JsObjectMap,
    options: &ResolvedOptions,
) -> Result<JsObjectMap, StylexError> {
    let mut expanded = JsObjectMap::new();
    for (key, value) in styles.entries() {
        let value_is_array = matches!(value, EvalValue::Arr(_));
        let scalar = match value {
            EvalValue::Str(s) => Some(StyleScalar::Str(std::borrow::Cow::Borrowed(s.as_str()))),
            EvalValue::Num(n) => Some(StyleScalar::Num(*n)),
            _ => None,
        };
        // Only string/number values survive; arrays, nulls, and objects drop.
        for (property, expanded_value) in flat_map_expanded_shorthands(
            std::borrow::Cow::Borrowed(key),
            scalar,
            value_is_array,
            options,
        )? {
            match expanded_value {
                Some(StyleScalar::Str(s)) => {
                    expanded.insert(property.to_string(), EvalValue::Str(s.into_owned()));
                }
                Some(StyleScalar::Num(n)) => {
                    expanded.insert(property, EvalValue::Num(n));
                }
                Some(StyleScalar::Undefined) | None => {}
            }
        }
    }
    Ok(expanded)
}

pub(crate) fn dashify_keys(styles: &JsObjectMap) -> JsObjectMap {
    let mut dashed = JsObjectMap::new();
    for (key, value) in styles.entries() {
        dashed.insert(dashify(key), value.clone());
    }
    dashed
}

// transformValue keys its ms-suffix and unitless tables by camelCase name, so
// a caller that dashified first misses them (`transition-duration: 500` is
// 500px).
pub(crate) fn transform_values(
    styles: &JsObjectMap,
    options: &ResolvedOptions,
) -> Result<JsObjectMap, StylexError> {
    let mut transformed = JsObjectMap::new();
    for (key, value) in styles.entries() {
        let out = match value {
            EvalValue::Str(s) => transform_value_str(key, s, options.enable_font_size_px_to_rem),
            EvalValue::Num(n) => transform_value_num(key, *n, options.enable_font_size_px_to_rem),
            _ => unreachable!("expand stage only stores scalars"),
        }
        .map_err(css_value_error)?;
        transformed.insert(key, EvalValue::Str(out));
    }
    Ok(transformed)
}

pub(crate) fn css_value_error(e: CssValueError) -> StylexError {
    match e {
        CssValueError::UnclosedFunction => StylexError::unclosed_function(),
        CssValueError::UnclosedString => StylexError::unclosed_string(),
        CssValueError::EmptyValue => StylexError::empty_value(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(entries: &[(&str, EvalValue)]) -> EvalValue {
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
        // Pinned against @stylexjs/babel-plugin 0.19.2 (misc/position-try.json
        // "basic numbers").
        let options = ResolvedOptions::default();
        let styles = obj(&[
            ("width", EvalValue::Num(100.0)),
            ("top", EvalValue::Num(0.0)),
        ]);
        let (name, rule) = position_try(&styles, &options).unwrap();
        assert_eq!(name, "--x1x22k8n");
        assert_eq!(&*rule.ltr, "@position-try --x1x22k8n {top:0;width:100px;}");
        assert_eq!(rule.rtl, None);
        assert!(rule.priority == 0.0);
        assert_eq!(rule.const_key, None);
    }

    #[test]
    fn empty_styles_have_no_rtl() {
        let options = ResolvedOptions::default();
        let (name, rule) = position_try(&obj(&[]), &options).unwrap();
        assert_eq!(name, "--xph554m");
        assert_eq!(&*rule.ltr, "@position-try --xph554m {}");
        assert_eq!(rule.rtl, None);
    }

    #[test]
    fn validation_errors() {
        let options = ResolvedOptions::default();
        let err = position_try(&EvalValue::Str("x".to_string()), &options).unwrap_err();
        assert_eq!(err.message, "positionTry() can only accept an object.");
        let err = position_try(
            &EvalValue::Arr(vec![EvalValue::Str("x".to_string())]),
            &options,
        )
        .unwrap_err();
        assert_eq!(err.message, "positionTry() can only accept an object.");
        let err = position_try(
            &obj(&[("color", EvalValue::Str("red".to_string()))]),
            &options,
        )
        .unwrap_err();
        assert_eq!(
            err.code,
            crate::errors::ErrorCode::PositionTryInvalidProperty
        );
        // Dashed spellings of valid properties are rejected too.
        let err = position_try(
            &obj(&[("position-anchor", EvalValue::Str("--x".to_string()))]),
            &options,
        )
        .unwrap_err();
        assert_eq!(
            err.code,
            crate::errors::ErrorCode::PositionTryInvalidProperty
        );
    }
}
