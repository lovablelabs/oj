//! `stylex.types.*` CSS type wrappers for defineVars/createTheme.
// parity: babel-plugin src/shared/types/index.js

use std::sync::Arc;

use crate::errors::StylexError;
use crate::eval::JsValue;
use crate::eval::value::{EvalValue, JsObjectMap};
use crate::jsrt::js_number_to_string;

pub const TYPES_MEMBERS: [&str; 13] = [
    "angle",
    "color",
    "url",
    "image",
    "integer",
    "lengthPercentage",
    "length",
    "percentage",
    "number",
    "resolution",
    "time",
    "transformFunction",
    "transformList",
];

#[derive(Debug, Clone, PartialEq)]
pub struct CssType {
    pub syntax: String,
    pub value: EvalValue,
}

#[derive(Debug, Clone, Copy)]
enum Conversion {
    Raw,
    BareNumber,
    Length,
    Percentage,
    IntegerString,
}

fn kind_table(kind: &str) -> Option<(&'static str, Conversion)> {
    Some(match kind {
        "angle" => ("<angle>", Conversion::Raw),
        "color" => ("<color>", Conversion::Raw),
        "url" => ("<url>", Conversion::Raw),
        "image" => ("<image>", Conversion::Raw),
        "integer" => ("<integer>", Conversion::IntegerString),
        "lengthPercentage" => ("<length-percentage>", Conversion::Length),
        "length" => ("<length>", Conversion::Length),
        "percentage" => ("<percentage>", Conversion::Percentage),
        "number" => ("<number>", Conversion::BareNumber),
        "resolution" => ("<resolution>", Conversion::Raw),
        "time" => ("<time>", Conversion::Raw),
        "transformFunction" => ("<transform-function>", Conversion::Raw),
        "transformList" => ("<transform-list>", Conversion::Raw),
        _ => return None,
    })
}

/// Builds the CSSType stand-in: own `value`/`syntax` keys (class fields, so
/// spreads copy them) plus the out-of-band `instanceof BaseCSSType` brand.
pub fn create_css_type(kind: &str, args: &[EvalValue]) -> Result<EvalValue, StylexError> {
    let (syntax, conversion) =
        kind_table(kind).ok_or_else(|| StylexError::unsupported_api("types.*"))?;
    let raw = args.first().cloned().unwrap_or(EvalValue::Undefined);
    let value = convert(&raw, conversion)?;
    let mut obj = JsObjectMap::new();
    obj.insert("value", value);
    obj.insert("syntax", EvalValue::Str(syntax.to_string()));
    obj.set_css_type(syntax.to_string());
    Ok(EvalValue::Obj(Arc::new(obj)))
}

// parity: types/index.js convertNumberToStringUsing (null → Object.keys crash).
fn convert(value: &EvalValue, conversion: Conversion) -> Result<EvalValue, StylexError> {
    if matches!(conversion, Conversion::Raw) {
        return Ok(value.clone());
    }
    Ok(match value {
        EvalValue::Num(n) => EvalValue::Str(match conversion {
            Conversion::Raw => unreachable!("raw returns above"),
            Conversion::BareNumber | Conversion::IntegerString => js_number_to_string(*n),
            Conversion::Length => {
                if *n == 0.0 {
                    "0".to_string()
                } else {
                    format!("{}px", js_number_to_string(*n))
                }
            }
            Conversion::Percentage => {
                if *n == 0.0 {
                    "0".to_string()
                } else {
                    format!("{}%", js_number_to_string(*n * 100.0))
                }
            }
        }),
        EvalValue::Str(s) => EvalValue::Str(s.clone()),
        EvalValue::Null => {
            return Err(StylexError::upstream_type_crash(
                "a null value inside a numeric types.* wrapper",
            ));
        }
        EvalValue::Obj(map) => {
            let mut out = JsObjectMap::new();
            for (k, v) in map.entries() {
                out.insert(k.to_string(), convert(v, conversion)?);
            }
            EvalValue::Obj(Arc::new(out))
        }
        EvalValue::Arr(items) => {
            // typeof [] is 'object': Object.keys gives index keys upstream.
            let mut out = JsObjectMap::new();
            for (i, v) in items.iter().enumerate() {
                out.insert(i.to_string(), convert(v, conversion)?);
            }
            EvalValue::Obj(Arc::new(out))
        }
        EvalValue::Undefined | EvalValue::Bool(_) => value.clone(),
    })
}

/// `String(types.<kind>)` of the uninvoked wrapper — the 0.19.0 dist source of
/// each `create` static method, pinned live (r4#8 conditional-leaf coercion).
pub fn types_member_source(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "angle" => "create(value) {\n    return new Angle(value);\n  }",
        "color" => "create(value) {\n    return new Color(value);\n  }",
        "url" => "create(value) {\n    return new Url(value);\n  }",
        "image" => "create(value) {\n    return new Image(value);\n  }",
        "integer" => {
            "create(value) {\n    return new Integer(convertNumberToStringUsing(String)(value));\n  }"
        }
        "lengthPercentage" => {
            "createLength(value) {\n    return new LengthPercentage(convertNumberToLength(value));\n  }"
        }
        "length" => "create(value) {\n    return new Length(convertNumberToLength(value));\n  }",
        "percentage" => {
            "create(value) {\n    return new Percentage(convertNumberToPercentage(value));\n  }"
        }
        "number" => "create(value) {\n    return new Num(convertNumberToBareString(value));\n  }",
        "resolution" => "create(value) {\n    return new Resolution(value);\n  }",
        "time" => "create(value) {\n    return new Time(value);\n  }",
        "transformFunction" => "create(value) {\n    return new TransformFunction(value);\n  }",
        "transformList" => "create(value) {\n    return new TransformList(value);\n  }",
        _ => return None,
    })
}

// parity: types/index.js isCSSType — instanceof BaseCSSType (the out-of-band
// brand) && value != null && typeof syntax === 'string' (the own key).
pub fn as_css_type(value: &EvalValue) -> Option<CssType> {
    let EvalValue::Obj(map) = value else {
        return None;
    };
    map.css_type()?;
    let Some(EvalValue::Str(syntax)) = map.get("syntax") else {
        return None;
    };
    match map.get("value") {
        None | Some(EvalValue::Null | EvalValue::Undefined) => None,
        Some(inner) => Some(CssType {
            syntax: syntax.clone(),
            value: inner.clone(),
        }),
    }
}

/// `as_css_type` over the evaluator-local value model; the inner value is
/// returned in `JsValue` form for the collectors.
pub fn as_css_type_js(value: &JsValue) -> Option<(String, JsValue)> {
    let JsValue::Obj(obj) = value else {
        return None;
    };
    obj.css_type()?;
    let Some(JsValue::Str(syntax)) = obj.get("syntax") else {
        return None;
    };
    match obj.get("value") {
        None | Some(JsValue::Null | JsValue::Undefined) => None,
        Some(inner) => Some((syntax.clone(), inner.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value_of(kind: &str, arg: EvalValue) -> EvalValue {
        let EvalValue::Obj(map) = create_css_type(kind, &[arg]).unwrap() else {
            panic!("expected object");
        };
        map.get("value").unwrap().clone()
    }

    #[test]
    fn syntax_strings_cover_all_kinds() {
        // Pinned via live-oracle probe 2026-08-28 (@property syntax fields).
        let expected = [
            ("angle", "<angle>"),
            ("color", "<color>"),
            ("url", "<url>"),
            ("image", "<image>"),
            ("integer", "<integer>"),
            ("lengthPercentage", "<length-percentage>"),
            ("length", "<length>"),
            ("percentage", "<percentage>"),
            ("number", "<number>"),
            ("resolution", "<resolution>"),
            ("time", "<time>"),
            ("transformFunction", "<transform-function>"),
            ("transformList", "<transform-list>"),
        ];
        assert_eq!(TYPES_MEMBERS.len(), expected.len());
        for (kind, syntax) in expected {
            let created = create_css_type(kind, &[EvalValue::Str("x".into())]).unwrap();
            let css = as_css_type(&created).unwrap();
            assert_eq!(css.syntax, syntax, "{kind}");
        }
    }

    #[test]
    fn number_conversions_match_oracle() {
        // Pinned via live-oracle probe 2026-08-28.
        assert_eq!(
            value_of("length", EvalValue::Num(0.0)),
            EvalValue::Str("0".into())
        );
        assert_eq!(
            value_of("length", EvalValue::Num(5.0)),
            EvalValue::Str("5px".into())
        );
        assert_eq!(
            value_of("lengthPercentage", EvalValue::Num(0.5)),
            EvalValue::Str("0.5px".into())
        );
        assert_eq!(
            value_of("percentage", EvalValue::Num(0.0)),
            EvalValue::Str("0".into())
        );
        assert_eq!(
            value_of("percentage", EvalValue::Num(0.07)),
            EvalValue::Str("7.000000000000001%".into())
        );
        assert_eq!(
            value_of("integer", EvalValue::Num(1.5)),
            EvalValue::Str("1.5".into())
        );
        assert_eq!(
            value_of("number", EvalValue::Num(0.0)),
            EvalValue::Str("0".into())
        );
        // Raw kinds keep numbers untouched.
        assert_eq!(
            value_of("angle", EvalValue::Num(45.0)),
            EvalValue::Num(45.0)
        );
        assert_eq!(
            value_of("time", EvalValue::Num(500.0)),
            EvalValue::Num(500.0)
        );
        assert_eq!(
            value_of("percentage", EvalValue::Str("50%".into())),
            EvalValue::Str("50%".into())
        );
    }

    #[test]
    fn nullish_values_are_not_css_types() {
        let created = create_css_type("color", &[EvalValue::Null]).unwrap();
        assert!(as_css_type(&created).is_none());
        let none = create_css_type("color", &[]).unwrap();
        assert!(as_css_type(&none).is_none());
        let err = create_css_type("length", &[EvalValue::Null]).unwrap_err();
        assert_eq!(err.code, crate::errors::ErrorCode::UpstreamTypeError);
    }

    #[test]
    fn nested_objects_convert_recursively() {
        let mut inner = JsObjectMap::new();
        inner.insert("default", EvalValue::Num(0.5));
        inner.insert("@media (min-width: 900px)", EvalValue::Num(0.75));
        let value = value_of("percentage", EvalValue::Obj(inner.into()));
        let EvalValue::Obj(map) = value else {
            panic!("expected object");
        };
        assert_eq!(map.get("default"), Some(&EvalValue::Str("50%".into())));
        assert_eq!(
            map.get("@media (min-width: 900px)"),
            Some(&EvalValue::Str("75%".into()))
        );
    }
}
