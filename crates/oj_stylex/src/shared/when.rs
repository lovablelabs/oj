//! `stylex.when.*` relational selectors -> `:where(...)` condition keys.
// parity: src/shared/when/when.js + visitors/stylex-create.js:174-180

use crate::errors::StylexError;
use crate::eval::value::EvalValue;
use crate::jsrt::js_number_to_string;
use crate::options::ResolvedOptions;
use crate::shared::markers::default_marker_class_name;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhenRelation {
    Ancestor,
    Descendant,
    SiblingBefore,
    SiblingAfter,
    AnySibling,
}

impl WhenRelation {
    /// The exported member name (`when.ancestor` …).
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "ancestor" => Self::Ancestor,
            "descendant" => Self::Descendant,
            "siblingBefore" => Self::SiblingBefore,
            "siblingAfter" => Self::SiblingAfter,
            "anySibling" => Self::AnySibling,
            _ => return None,
        })
    }
}

/// `marker` is the evaluated second argument (`None` when absent). Cross-file
/// marker proxies must be resolved to `EvalValue::Str` before this call.
pub fn when_selector(
    relation: WhenRelation,
    pseudo: &str,
    marker: Option<&EvalValue>,
    options: &ResolvedOptions,
) -> Result<String, StylexError> {
    validate_pseudo_selector(pseudo)?;
    let m = resolve_marker_class(marker, options);
    Ok(match relation {
        WhenRelation::Ancestor => format!(":where(.{m}{pseudo} *)"),
        WhenRelation::Descendant => format!(":where(:has(.{m}{pseudo}))"),
        WhenRelation::SiblingBefore => format!(":where(.{m}{pseudo} ~ *)"),
        WhenRelation::SiblingAfter => format!(":where(:has(~ .{m}{pseudo}))"),
        WhenRelation::AnySibling => {
            format!(":where(.{m}{pseudo} ~ *, :has(~ .{m}{pseudo}))")
        }
    })
}

// parity: when.js validatePseudoSelector (exact messages, exact check order).
fn validate_pseudo_selector(pseudo: &str) -> Result<(), StylexError> {
    if !(pseudo.starts_with(':') || pseudo.starts_with('[')) {
        return Err(StylexError::invalid_when_selector(
            "Pseudo selector must start with \":\" or \"[\"",
        ));
    }
    if pseudo.starts_with("::") {
        return Err(StylexError::invalid_when_selector(
            "Pseudo selector cannot start with \"::\" (pseudo-elements are not supported)",
        ));
    }
    if pseudo.starts_with('[') && !pseudo.ends_with(']') {
        return Err(StylexError::invalid_when_selector(
            "Attribute selector must end with \"]\"",
        ));
    }
    Ok(())
}

// parity: `marker ?? state.options` + getDefaultMarkerClassName; a `??` miss
// reads the real options, any other non-string goes through the object probes.
pub fn resolve_marker_class(marker: Option<&EvalValue>, options: &ResolvedOptions) -> String {
    let value = match marker {
        None | Some(EvalValue::Null) | Some(EvalValue::Undefined) => {
            return default_marker_class_name(options);
        }
        Some(EvalValue::Str(s)) => return s.clone(),
        Some(other) => other,
    };
    if let EvalValue::Obj(obj) = value {
        if matches!(obj.get("$$css"), Some(EvalValue::Bool(true)))
            && let Some(key) = obj.keys().find(|k| *k != "$$css")
        {
            return key.to_string();
        }
        // Fallback reads `classNamePrefix` off the marker object itself.
        if let Some(prefix) = obj.get("classNamePrefix")
            && !matches!(prefix, EvalValue::Null | EvalValue::Undefined)
        {
            return format!("{}-default-marker", coerce_to_string(prefix));
        }
    }
    "default-marker".to_string()
}

// JS template-literal coercion for the values realistically reaching the
// classNamePrefix fallback.
fn coerce_to_string(value: &EvalValue) -> String {
    match value {
        EvalValue::Str(s) => s.clone(),
        EvalValue::Num(n) => js_number_to_string(*n),
        EvalValue::Bool(b) => b.to_string(),
        EvalValue::Null => "null".to_string(),
        EvalValue::Undefined => "undefined".to_string(),
        EvalValue::Obj(_) => "[object Object]".to_string(),
        EvalValue::Arr(_) => "".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::value::JsObjectMap;

    fn opts() -> ResolvedOptions {
        ResolvedOptions::default()
    }

    #[test]
    fn five_shapes_with_default_marker() {
        // Pinned via live-oracle probe 2026-08-27 (legacy-keys key extraction).
        let o = opts();
        let cases = [
            (WhenRelation::Ancestor, ":where(.x-default-marker:hover *)"),
            (
                WhenRelation::Descendant,
                ":where(:has(.x-default-marker:hover))",
            ),
            (
                WhenRelation::SiblingBefore,
                ":where(.x-default-marker:hover ~ *)",
            ),
            (
                WhenRelation::SiblingAfter,
                ":where(:has(~ .x-default-marker:hover))",
            ),
            (
                WhenRelation::AnySibling,
                ":where(.x-default-marker:hover ~ *, :has(~ .x-default-marker:hover))",
            ),
        ];
        for (relation, expected) in cases {
            assert_eq!(
                when_selector(relation, ":hover", None, &o).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn marker_resolution() {
        let o = opts();
        assert_eq!(
            resolve_marker_class(Some(&EvalValue::Str("my-marker".to_string())), &o),
            "my-marker"
        );
        assert_eq!(resolve_marker_class(None, &o), "x-default-marker");
        assert_eq!(
            resolve_marker_class(Some(&EvalValue::Null), &o),
            "x-default-marker"
        );
        let marker: JsObjectMap = [
            ("xleysvp".to_string(), EvalValue::Str("xleysvp".to_string())),
            ("$$css".to_string(), EvalValue::Bool(true)),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            resolve_marker_class(Some(&EvalValue::Obj(marker.into())), &o),
            "xleysvp"
        );
        // Oracle-pinned quirks: non-marker objects and numbers lose the prefix.
        assert_eq!(
            resolve_marker_class(Some(&EvalValue::Obj(JsObjectMap::new().into())), &o),
            "default-marker"
        );
        assert_eq!(
            resolve_marker_class(Some(&EvalValue::Num(5.0)), &o),
            "default-marker"
        );
        let css_only: JsObjectMap = [("$$css".to_string(), EvalValue::Bool(true))]
            .into_iter()
            .collect();
        assert_eq!(
            resolve_marker_class(Some(&EvalValue::Obj(css_only.into())), &o),
            "default-marker"
        );
    }

    #[test]
    fn validation_messages() {
        let o = opts();
        let err = when_selector(WhenRelation::Ancestor, "hover", None, &o).unwrap_err();
        assert_eq!(
            err.message,
            "Pseudo selector must start with \":\" or \"[\""
        );
        let err = when_selector(WhenRelation::Ancestor, "::before", None, &o).unwrap_err();
        assert_eq!(
            err.message,
            "Pseudo selector cannot start with \"::\" (pseudo-elements are not supported)"
        );
        let err = when_selector(WhenRelation::Ancestor, "[data-open", None, &o).unwrap_err();
        assert_eq!(err.message, "Attribute selector must end with \"]\"");
        // Oracle-accepted degenerate forms.
        assert!(when_selector(WhenRelation::Ancestor, ":", None, &o).is_ok());
        assert!(when_selector(WhenRelation::Ancestor, "[]", None, &o).is_ok());
    }
}
