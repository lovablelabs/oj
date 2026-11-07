//! `stylex.viewTransitionClass` over an already-evaluated styles object.
// parity: babel-plugin src/shared/stylex-view-transition-class.js + its visitor

use crate::errors::StylexError;
use crate::eval::value::{EvalValue, JsObjectMap};
use crate::hash::hash;
use crate::options::ResolvedOptions;
use crate::rules::StylexRule;
use crate::shared::dashify::dashify;
use crate::shared::position_try::expand_dashify_transform;

const VALID_VIEW_TRANSITION_CLASS_PROPERTIES: [&str; 4] = ["group", "imagePair", "old", "new"];

/// Returns the class name and its single injectable rule (priority 1, no rtl).
pub fn view_transition_class(
    styles: &EvalValue,
    options: &ResolvedOptions,
) -> Result<(String, StylexRule), StylexError> {
    let styles = assert_valid_view_transition_class(styles)?;
    assert_valid_properties(styles)?;

    // (selector, joined decls) per top-level key, in insertion order.
    let mut style_strings: Vec<(String, String)> = Vec::new();
    for (key, style) in styles.entries() {
        let entries = object_entries_js(style, "a nullish viewTransitionClass style value")?;
        let transformed = expand_dashify_transform(&entries, options)?;
        let mut decls = String::new();
        for (k, v) in transformed.entries() {
            let EvalValue::Str(v) = v else {
                unreachable!("transform stage only stores strings");
            };
            decls.push_str(&format!("{k}:{v};"));
        }
        style_strings.push((format!("::view-transition-{}", dashify(key)), decls));
    }

    // The hash recipe reuses the decl-joiner over the selector→string map.
    let mut recipe = String::new();
    for (selector, decls) in &style_strings {
        recipe.push_str(&format!("{selector}:{decls};"));
    }
    let class_name = format!("{}{}", options.class_name_prefix, hash(&recipe));

    let mut ltr = String::new();
    for (selector, decls) in &style_strings {
        ltr.push_str(&format!("{selector}(*.{class_name}){{{decls}}}"));
    }

    let rule = StylexRule {
        class_name: class_name.as_str().into(),
        ltr: ltr.into(),
        rtl: None,
        const_key: None,
        const_val: None,
        priority: 1.0,
    };
    Ok((class_name, rule))
}

// parity: visitor assertValidViewTransitionClass
fn assert_valid_view_transition_class(styles: &EvalValue) -> Result<&JsObjectMap, StylexError> {
    match styles {
        EvalValue::Obj(map) => Ok(map),
        _ => Err(StylexError::non_style_object("viewTransitionClass")),
    }
}

fn assert_valid_properties(styles: &JsObjectMap) -> Result<(), StylexError> {
    if styles
        .keys()
        .any(|key| !VALID_VIEW_TRANSITION_CLASS_PROPERTIES.contains(&key))
    {
        return Err(StylexError::view_transition_class_invalid_property());
    }
    Ok(())
}

// parity: Object.keys/entries semantics (strings split into UTF-16 units;
// numbers/bools have no own keys; nullish crashes upstream).
pub(crate) fn object_entries_js(
    value: &EvalValue,
    crash_context: &str,
) -> Result<JsObjectMap, StylexError> {
    let mut out = JsObjectMap::new();
    match value {
        EvalValue::Obj(map) => {
            for (k, v) in map.entries() {
                out.insert(k, v.clone());
            }
        }
        EvalValue::Arr(items) => {
            for (i, v) in items.iter().enumerate() {
                out.insert(i.to_string(), v.clone());
            }
        }
        EvalValue::Str(s) => {
            for (i, unit) in s.encode_utf16().enumerate() {
                let ch = String::from_utf16_lossy(&[unit]);
                out.insert(i.to_string(), EvalValue::Str(ch));
            }
        }
        EvalValue::Num(_) | EvalValue::Bool(_) => {}
        EvalValue::Null | EvalValue::Undefined => {
            return Err(StylexError::upstream_type_crash(crash_context));
        }
    }
    Ok(out)
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
        // Pinned via live-oracle probe 2026-08-28.
        let options = ResolvedOptions::default();
        let styles = obj(&[
            (
                "group",
                obj(&[("transitionProperty", EvalValue::Str("none".to_string()))]),
            ),
            ("imagePair", obj(&[("borderRadius", EvalValue::Num(16.0))])),
            (
                "old",
                obj(&[("animationDuration", EvalValue::Str("0.5s".to_string()))]),
            ),
            (
                "new",
                obj(&[(
                    "animationTimingFunction",
                    EvalValue::Str("ease-out".to_string()),
                )]),
            ),
        ]);
        let (name, rule) = view_transition_class(&styles, &options).unwrap();
        assert_eq!(name, "xchu1hv");
        assert_eq!(
            &*rule.ltr,
            "::view-transition-group(*.xchu1hv){transition-property:none;}::view-transition-image-pair(*.xchu1hv){border-radius:16px;}::view-transition-old(*.xchu1hv){animation-duration:.5s;}::view-transition-new(*.xchu1hv){animation-timing-function:ease-out;}"
        );
        assert_eq!(rule.rtl, None);
        assert!(rule.priority == 1.0);
    }

    #[test]
    fn key_order_changes_hash() {
        let options = ResolvedOptions::default();
        let styles = obj(&[
            (
                "new",
                obj(&[(
                    "animationTimingFunction",
                    EvalValue::Str("ease-out".to_string()),
                )]),
            ),
            (
                "old",
                obj(&[("animationDuration", EvalValue::Str("0.5s".to_string()))]),
            ),
        ]);
        let (name, _) = view_transition_class(&styles, &options).unwrap();
        assert_eq!(name, "x1ujng0t");
    }

    #[test]
    fn weird_style_values() {
        let options = ResolvedOptions::default();
        // Strings char-split; numbers keep an empty block; null crashes upstream.
        let (name, rule) = view_transition_class(
            &obj(&[("group", EvalValue::Str("ab".to_string()))]),
            &options,
        )
        .unwrap();
        assert_eq!(name, "xw9mpwj");
        assert_eq!(&*rule.ltr, "::view-transition-group(*.xw9mpwj){0:a;1:b;}");
        let (name, rule) =
            view_transition_class(&obj(&[("group", EvalValue::Num(5.0))]), &options).unwrap();
        assert_eq!(name, "x1od172d");
        assert_eq!(&*rule.ltr, "::view-transition-group(*.x1od172d){}");
        let err = view_transition_class(&obj(&[("group", EvalValue::Null)]), &options).unwrap_err();
        assert_eq!(err.code, crate::errors::ErrorCode::UpstreamTypeError);
    }

    #[test]
    fn validation_errors() {
        let options = ResolvedOptions::default();
        let err = view_transition_class(&EvalValue::Str("x".to_string()), &options).unwrap_err();
        assert_eq!(
            err.message,
            "viewTransitionClass() can only accept an object."
        );
        let err = view_transition_class(&obj(&[("groups", obj(&[]))]), &options).unwrap_err();
        assert_eq!(
            err.code,
            crate::errors::ErrorCode::ViewTransitionClassInvalidProperty
        );
    }
}
