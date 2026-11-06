//! `stylex.keyframes` over an already-evaluated frames object.
// parity: babel-plugin src/shared/stylex-keyframes.js + visitors/stylex-keyframes.js

use crate::errors::StylexError;
use crate::eval::value::{EvalValue, JsObjectMap};
use crate::hash::hash;
use crate::options::ResolvedOptions;
use crate::rules::StylexRule;
use crate::shared::dashify::dashify;
use crate::shared::flatten::StyleScalar;
use crate::shared::normalize_value::CssValueError;
use crate::shared::resolution::flat_map_expanded_shorthands;
use crate::shared::rtl::{RtlContext, generate_ltr, generate_rtl};
use crate::shared::transform_value::{transform_value_num, transform_value_str};

/// Returns the animation name and its single injectable rule (priority 0).
pub fn keyframes(
    frames: &EvalValue,
    options: &ResolvedOptions,
) -> Result<(String, StylexRule), StylexError> {
    let frames = assert_valid_keyframes(frames)?;

    let ctx = RtlContext::of(options);
    let mut ltr_frames: Vec<(String, JsObjectMap)> = Vec::new();
    let mut rtl_frames: Vec<(String, JsObjectMap)> = Vec::new();
    let mut stable_frames: Vec<(String, JsObjectMap)> = Vec::new();
    for (frame_name, frame_value) in frames.entries() {
        let EvalValue::Obj(frame) = frame_value else {
            // Validation admits `null` frames (typeof null is 'object'); upstream
            // then crashes in Object.keys.
            return Err(StylexError::upstream_type_crash("a null keyframes frame"));
        };
        let transformed = expand_dashify_transform(frame, options)?;
        let mut ltr = JsObjectMap::new();
        let mut rtl = JsObjectMap::new();
        let mut stable = JsObjectMap::new();
        for (key, value) in transformed.entries() {
            let EvalValue::Str(value) = value else {
                unreachable!("transform stage only stores strings");
            };
            let (lk, lv) = generate_ltr(key, value, ctx);
            ltr.insert(lk, EvalValue::Str(lv.into_owned()));
            let (rk, rv) = generate_rtl(key, value, ctx).map_or_else(
                || (key.to_string(), value.clone()),
                |(k, v)| (k.into_owned(), v.into_owned()),
            );
            rtl.insert(rk, EvalValue::Str(rv));
            let (sk, sv) = generate_ltr(key, value, RtlContext::DEFAULTS);
            stable.insert(sk, EvalValue::Str(sv.into_owned()));
        }
        ltr_frames.push((frame_name.to_string(), ltr));
        rtl_frames.push((frame_name.to_string(), rtl));
        stable_frames.push((frame_name.to_string(), stable));
    }

    let ltr_string = construct_keyframes_string(&ltr_frames);
    let rtl_string = construct_keyframes_string(&rtl_frames);
    // The name hashes a third serialization built with DEFAULT options, so it
    // is direction- and option-agnostic (`objMapEntry(frame, generateLtr)`).
    let stable_string = construct_keyframes_string(&stable_frames);
    let animation_name = format!(
        "{}{}-B",
        options.class_name_prefix,
        hash(&format!("<>{stable_string}"))
    );

    let ltr = format!("@keyframes {animation_name}{{{ltr_string}}}");
    let rtl =
        (ltr_string != rtl_string).then(|| format!("@keyframes {animation_name}{{{rtl_string}}}"));

    let rule = StylexRule {
        class_name: animation_name.as_str().into(),
        ltr: ltr.into(),
        rtl: rtl.map(Into::into),
        const_key: None,
        const_val: None,
        priority: 0.0,
    };
    Ok((animation_name, rule))
}

// parity: visitors/stylex-keyframes.js assertValidKeyframes
fn assert_valid_keyframes(frames: &EvalValue) -> Result<&JsObjectMap, StylexError> {
    let EvalValue::Obj(map) = frames else {
        return Err(StylexError::non_style_object("keyframes"));
    };
    for (_key, value) in map.entries() {
        // `typeof value === 'object' && !Array.isArray(value)`: null passes.
        match value {
            EvalValue::Obj(_) | EvalValue::Null => {}
            _ => return Err(StylexError::non_object_keyframe()),
        }
    }
    Ok(map)
}

// parity: stylex-keyframes.js expand+dashify+transformValue pipe; each stage
// rebuilds the object, so duplicate keys collapse first-position last-value.
fn expand_dashify_transform(
    frame: &JsObjectMap,
    options: &ResolvedOptions,
) -> Result<JsObjectMap, StylexError> {
    let mut expanded = JsObjectMap::new();
    for (key, value) in frame.entries() {
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
    let mut dashed = JsObjectMap::new();
    for (key, value) in expanded.entries() {
        dashed.insert(dashify(key), value.clone());
    }
    let mut transformed = JsObjectMap::new();
    for (key, value) in dashed.entries() {
        // transformValue sees the already-dashed key: dashed forms miss the
        // camelCase ms-suffix and unitless tables (e.g. transition-duration: 500px).
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

fn construct_keyframes_string(frames: &[(String, JsObjectMap)]) -> String {
    let mut out = String::new();
    for (name, decls) in frames {
        out.push_str(name);
        out.push('{');
        for (k, v) in decls.entries() {
            let EvalValue::Str(v) = v else {
                unreachable!("frames hold string declarations");
            };
            out.push_str(k);
            out.push(':');
            out.push_str(v);
            out.push(';');
        }
        out.push('}');
    }
    out
}

fn css_value_error(e: CssValueError) -> StylexError {
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
    fn basic_frames_match_oracle() {
        // Pinned via live-oracle probe 2026-08-27.
        let frames = obj(&[
            ("from", obj(&[("opacity", EvalValue::Num(0.0))])),
            ("to", obj(&[("opacity", EvalValue::Num(1.0))])),
        ]);
        let options = ResolvedOptions::default();
        let (name, rule) = keyframes(&frames, &options).unwrap();
        assert_eq!(name, "x18re5ia-B");
        assert_eq!(
            &*rule.ltr,
            "@keyframes x18re5ia-B{from{opacity:0;}to{opacity:1;}}"
        );
        assert_eq!(rule.rtl, None);
        assert!(rule.priority == 0.0);
    }

    #[test]
    fn rtl_emitted_only_when_different() {
        let options = ResolvedOptions::default();
        let frames = obj(&[
            (
                "from",
                obj(&[("float", EvalValue::Str("inline-start".to_string()))]),
            ),
            (
                "to",
                obj(&[("float", EvalValue::Str("inline-end".to_string()))]),
            ),
        ]);
        let (_, rule) = keyframes(&frames, &options).unwrap();
        assert_eq!(
            &*rule.ltr,
            "@keyframes x1uod70n-B{from{float:left;}to{float:right;}}"
        );
        assert_eq!(
            rule.rtl.as_deref(),
            Some("@keyframes x1uod70n-B{from{float:right;}to{float:left;}}")
        );
    }

    #[test]
    fn frame_validation() {
        let options = ResolvedOptions::default();
        let err = keyframes(&EvalValue::Str("x".to_string()), &options).unwrap_err();
        assert_eq!(err.message, "keyframes() can only accept an object.");
        let err =
            keyframes(&obj(&[("from", EvalValue::Str("x".to_string()))]), &options).unwrap_err();
        assert_eq!(
            err.message,
            "Every frame within a keyframes() call must be an object."
        );
        let err = keyframes(&obj(&[("from", EvalValue::Null)]), &options).unwrap_err();
        assert_eq!(err.code, crate::errors::ErrorCode::UpstreamTypeError);
    }
}
