use thiserror::Error;

// Upstream-mirrored codes carry the exact user-facing message text of
// @stylexjs/babel-plugin@0.19; the Unsupported*/Unknown/Invalid codes are ours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorCode {
    IllegalArgumentLength,
    NonStaticValue,
    NonStyleObject,
    IllegalNamespaceValue,
    IllegalPropValue,
    IllegalPropArrayValue,
    NoObjectSpreads,
    InvalidMediaQuerySyntax,
    CyclicConstReference,
    CyclicDefineVarsReference,
    UnclosedFunction,
    UnclosedString,
    BannedShorthand,
    UnsupportedOption,
    UnsupportedApi,
    UnknownOption,
    InvalidOptionValue,
    DuplicateConditional,
    InvalidPseudoOrAtRule,
    NonContiguousVars,
    ShorthandFallback,
    InvalidListStyleValue,
    EmptyValue,
    NonObjectKeyframe,
    UnboundCallValue,
    NonExportNamedDeclaration,
    CannotGenerateHash,
    InvalidWhenSelector,
    UpstreamTypeError,
    ParseError,
    PositionTryInvalidProperty,
    ViewTransitionClassInvalidProperty,
    NestedKeySeparator,
    NestedThemeInvalidVars,
    InvalidDefineVarsValue,
    ArrayInDefineVars,
    MissingDefaultValue,
    UnknownDefineVarsReference,
    InvalidDefineVarsFunctionValue,
    ThemeWithoutVarGroup,
    OnlyNamedParameters,
    AstBackend,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct StylexError {
    pub code: ErrorCode,
    pub message: String,
}

impl StylexError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    // parity: babel-plugin src/shared/messages.js illegalArgumentLength
    pub fn illegal_argument_length(fn_name: &str, arg_length: usize) -> Self {
        let plural = if arg_length == 1 { "" } else { "s" };
        Self::new(
            ErrorCode::IllegalArgumentLength,
            format!("{fn_name}() should have {arg_length} argument{plural}."),
        )
    }

    // parity: babel-plugin src/shared/messages.js nonStaticValue
    pub fn non_static_value(fn_name: &str) -> Self {
        Self::new(
            ErrorCode::NonStaticValue,
            format!("Only static values are allowed inside of a {fn_name}() call."),
        )
    }

    // parity: babel-plugin src/shared/messages.js nonStyleObject
    pub fn non_style_object(fn_name: &str) -> Self {
        Self::new(
            ErrorCode::NonStyleObject,
            format!("{fn_name}() can only accept an object."),
        )
    }

    pub fn illegal_namespace_value() -> Self {
        Self::new(
            ErrorCode::IllegalNamespaceValue,
            "A StyleX namespace must be an object.",
        )
    }

    pub fn illegal_prop_value() -> Self {
        Self::new(
            ErrorCode::IllegalPropValue,
            "A style value can only contain an array, string or number.",
        )
    }

    pub fn illegal_prop_array_value() -> Self {
        Self::new(
            ErrorCode::IllegalPropArrayValue,
            "A style array value can only contain strings or numbers.",
        )
    }

    pub fn no_object_spreads() -> Self {
        Self::new(
            ErrorCode::NoObjectSpreads,
            "Object spreads are not allowed in create() calls.",
        )
    }

    pub fn invalid_media_query_syntax() -> Self {
        Self::new(
            ErrorCode::InvalidMediaQuerySyntax,
            "Invalid media query syntax.",
        )
    }

    // parity: babel-plugin src/index.js:572 (const resolution)
    pub fn cyclic_const_reference(const_ref: &str) -> Self {
        Self::new(
            ErrorCode::CyclicConstReference,
            format!("circular reference detected for constant {const_ref}"),
        )
    }

    // parity: babel-plugin src/shared/messages.js cyclicDefineVarsReference
    pub fn cyclic_define_vars_reference(cycle: &str) -> Self {
        Self::new(
            ErrorCode::CyclicDefineVarsReference,
            format!("Cyclic same-group references in defineVars() are not allowed: {cycle}."),
        )
    }

    pub fn unclosed_function() -> Self {
        Self::new(
            ErrorCode::UnclosedFunction,
            "Rule contains an unclosed function",
        )
    }

    pub fn unclosed_string() -> Self {
        Self::new(
            ErrorCode::UnclosedString,
            "Rule contains an unclosed string",
        )
    }

    /// `None` when `property` is not a banned shorthand under property-specificity.
    pub fn banned_shorthand(property: &str) -> Option<Self> {
        banned_shorthand_message(property).map(|m| Self::new(ErrorCode::BannedShorthand, m))
    }

    pub fn unsupported_option(description: &str) -> Self {
        Self::new(
            ErrorCode::UnsupportedOption,
            format!(
                "Unsupported option `{description}`: a deliberate v1 gap of the Rust StyleX compiler. See crates/stylex/docs."
            ),
        )
    }

    pub fn unsupported_api(api: &str) -> Self {
        Self::new(
            ErrorCode::UnsupportedApi,
            format!(
                "Unsupported API `{api}`: a deliberate v1 gap of the Rust StyleX compiler. See crates/stylex/docs."
            ),
        )
    }

    /// Loud-over-lossy doctrine: upstream emits the corrupt lone surrogate;
    /// this compiler refuses (documented rust-rejects-more divergence).
    pub fn lone_surrogate(context: &str) -> Self {
        Self::new(
            ErrorCode::UnsupportedApi,
            format!(
                "Unsupported API `{context} producing a lone UTF-16 surrogate`: a deliberate v1 gap of the Rust StyleX compiler. See crates/stylex/docs."
            ),
        )
    }

    pub fn only_named_parameters() -> Self {
        Self::new(
            ErrorCode::OnlyNamedParameters,
            "Only named parameters are allowed in Dynamic Style functions. Destructuring, spreading or default values are not allowed.",
        )
    }

    pub fn unknown_option(key: &str) -> Self {
        Self::new(
            ErrorCode::UnknownOption,
            format!(
                "Unknown option `{key}`: not a recognized @stylexjs/babel-plugin option. See crates/stylex/docs."
            ),
        )
    }

    pub fn duplicate_conditional() -> Self {
        Self::new(
            ErrorCode::DuplicateConditional,
            "The same pseudo selector or at-rule cannot be used more than once.",
        )
    }

    pub fn invalid_pseudo_or_at_rule() -> Self {
        Self::new(
            ErrorCode::InvalidPseudoOrAtRule,
            "Invalid pseudo or at-rule.",
        )
    }

    pub fn non_contiguous_vars() -> Self {
        Self::new(
            ErrorCode::NonContiguousVars,
            "All variables passed to firstThatWorks() must be contiguous.",
        )
    }

    // parity: babel-plugin src/shared/preprocess-rules/index.js:50
    pub fn shorthand_fallback() -> Self {
        Self::new(
            ErrorCode::ShorthandFallback,
            "Cannot use fallbacks for shorthands. Use the expansion instead.",
        )
    }

    // parity: legacy-expand-shorthands.js listStyle; `text` is already
    // JSON-quoted by the caller (the two spellings differ by a quote pair).
    pub fn invalid_list_style(text: &str) -> Self {
        Self::new(
            ErrorCode::InvalidListStyleValue,
            format!("invalid \"listStyle\" value of {text}"),
        )
    }

    // Upstream crashes with a TypeError here; structured stand-in (W2 divergence).
    pub fn empty_value() -> Self {
        Self::new(ErrorCode::EmptyValue, "Cannot normalize an empty value")
    }

    pub fn non_object_keyframe() -> Self {
        Self::new(
            ErrorCode::NonObjectKeyframe,
            "Every frame within a keyframes() call must be an object.",
        )
    }

    // parity: babel-plugin src/shared/messages.js unboundCallValue
    pub fn unbound_call_value(fn_name: &str) -> Self {
        Self::new(
            ErrorCode::UnboundCallValue,
            format!("{fn_name}() calls must be bound to a bare variable."),
        )
    }

    // parity: babel-plugin src/shared/messages.js nonExportNamedDeclaration
    pub fn non_export_named_declaration(fn_name: &str) -> Self {
        Self::new(
            ErrorCode::NonExportNamedDeclaration,
            format!("The return value of {fn_name}() must be bound to a named export."),
        )
    }

    // parity: babel-plugin src/shared/messages.js cannotGenerateHash
    pub fn cannot_generate_hash(fn_name: &str) -> Self {
        Self::new(
            ErrorCode::CannotGenerateHash,
            format!(
                "Unable to generate hash for {fn_name}(). Check that the file has a valid extension and that unstable_moduleResolution is configured."
            ),
        )
    }

    /// The exact `message` comes from when.js validatePseudoSelector.
    pub fn invalid_when_selector(message: &str) -> Self {
        Self::new(ErrorCode::InvalidWhenSelector, message)
    }

    pub fn parse_error(detail: &str) -> Self {
        Self::new(
            ErrorCode::ParseError,
            format!("Failed to parse the source file: {detail}"),
        )
    }

    // parity: babel-plugin src/shared/messages.js POSITION_TRY_INVALID_PROPERTY
    pub fn position_try_invalid_property() -> Self {
        Self::new(
            ErrorCode::PositionTryInvalidProperty,
            "Invalid property in `positionTry()` call. It may only contain, positionAnchor, positionArea, inset properties (top, left, insetInline etc.), margin properties, size properties (height, inlineSize, etc.), and self-alignment properties (alignSelf, justifySelf, placeSelf)",
        )
    }

    // parity: babel-plugin src/shared/messages.js VIEW_TRANSITION_CLASS_INVALID_PROPERTY
    pub fn view_transition_class_invalid_property() -> Self {
        Self::new(
            ErrorCode::ViewTransitionClassInvalidProperty,
            "Invalid property in `viewTransitionClass()` call. It may only contain group, imagePair, old, and new properties",
        )
    }

    // parity: babel-plugin src/shared/stylex-nested-utils.js flattenImpl key check
    pub fn nested_key_contains_separator(key: &str) -> Self {
        Self::new(
            ErrorCode::NestedKeySeparator,
            format!(
                "Key \"{key}\" must not contain the \".\" character. Use nested objects instead of dots in key names. See: https://www.designtokens.org/tr/drafts/format/#character-restrictions"
            ),
        )
    }

    // parity: visitors/stylex-create-theme-nested.js __varGroupHash__ check
    pub fn nested_theme_invalid_vars() -> Self {
        Self::new(
            ErrorCode::NestedThemeInvalidVars,
            "Can only override variables theme created with unstable_defineVarsNested().",
        )
    }

    // parity: shared/stylex-vars-utils.js + visitors/stylex-define-vars.js
    pub fn invalid_define_vars_value() -> Self {
        Self::new(
            ErrorCode::InvalidDefineVarsValue,
            "Invalid value in defineVars",
        )
    }

    // parity: same plain-Error text in defineVars and createTheme collection.
    pub fn array_in_define_vars() -> Self {
        Self::new(
            ErrorCode::ArrayInDefineVars,
            "Array is not supported in defineVars",
        )
    }

    /// `key` is present everywhere except getDefaultValue's keyless variant.
    pub fn missing_default_value(key: Option<&str>) -> Self {
        let message = match key {
            Some(key) => format!("Default value is not defined for {key} variable."),
            None => "Default value is not defined for variable.".to_string(),
        };
        Self::new(ErrorCode::MissingDefaultValue, message)
    }

    // parity: babel-plugin src/shared/messages.js unknownDefineVarsReference
    pub fn unknown_define_vars_reference(key: &str, dependency: &str) -> Self {
        Self::new(
            ErrorCode::UnknownDefineVarsReference,
            format!(
                "Unknown same-group reference \"{dependency}\" found while resolving \"{key}\" in defineVars()."
            ),
        )
    }

    // parity: babel-plugin src/shared/messages.js invalidDefineVarsFunctionValue
    pub fn invalid_define_vars_function_value() -> Self {
        Self::new(
            ErrorCode::InvalidDefineVarsFunctionValue,
            "Function values in defineVars() must be zero-argument and return a static value supported by defineVars().",
        )
    }

    // parity: visitors/stylex-create-theme.js + shared/stylex-create-theme.js
    pub fn theme_without_var_group() -> Self {
        Self::new(
            ErrorCode::ThemeWithoutVarGroup,
            "Can only override variables theme created with defineVars().",
        )
    }

    // Upstream crashes with "Cannot convert undefined or null to object";
    // structured stand-in, same compile-fails outcome (W3 divergence).
    pub fn upstream_type_crash(context: &str) -> Self {
        Self::new(
            ErrorCode::UpstreamTypeError,
            format!(
                "Upstream crashes with a TypeError on {context}; the Rust compiler rejects it instead."
            ),
        )
    }
}

// parity: babel-plugin src/shared/preprocess-rules/property-specificity.js
// (throwing shorthands plus the aliases that point at them).
pub fn banned_shorthand_message(property: &str) -> Option<&'static str> {
    Some(match property {
        "all" => "all is not supported",
        "animation" => "animation is not supported",
        "background" => {
            "background is not supported. Use background-color, border-image etc. instead."
        }
        "border" => {
            "border is not supported. Use border-width, border-style and border-color instead."
        }
        "borderInline" | "borderHorizontal" => {
            "borderInline is not supported. Use borderInlineWidth, borderInlineStyle and borderInlineColor instead."
        }
        "borderBlock" | "borderVertical" => {
            "borderBlock is not supported. Use borderBlockWidth, borderBlockStyle and borderBlockColor instead."
        }
        "borderTop" | "borderBlockStart" => {
            "borderTop is not supported. Use borderTopWidth, borderTopStyle and borderTopColor instead."
        }
        "borderInlineEnd" | "borderEnd" => {
            "borderInlineEnd is not supported. Use borderInlineEndWidth, borderInlineEndStyle and borderInlineEndColor instead."
        }
        "borderRight" => {
            "borderRight is not supported. Use borderRightWidth, borderRightStyle and borderRightColor instead."
        }
        "borderBottom" | "borderBlockEnd" => {
            "borderBottom is not supported. Use borderBottomWidth, borderBottomStyle and borderBottomColor instead."
        }
        "borderInlineStart" | "borderStart" => {
            "borderInlineStart is not supported. Use borderInlineStartWidth, borderInlineStartStyle and borderInlineStartColor instead."
        }
        "borderLeft" => {
            "`borderLeft` is not supported. You could use `borderLeftWidth`, `borderLeftStyle` and `borderLeftColor`, but it is preferable to use `borderInlineStartWidth`, `borderInlineStartStyle` and `borderInlineStartColor`."
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_text_matches_upstream() {
        assert_eq!(
            StylexError::illegal_argument_length("create", 1).message,
            "create() should have 1 argument."
        );
        assert_eq!(
            StylexError::illegal_argument_length("keyframes", 2).message,
            "keyframes() should have 2 arguments."
        );
        assert_eq!(
            StylexError::non_static_value("create").message,
            "Only static values are allowed inside of a create() call."
        );
        assert_eq!(
            StylexError::non_style_object("create").message,
            "create() can only accept an object."
        );
        assert_eq!(
            StylexError::cyclic_const_reference("colors.bg").message,
            "circular reference detected for constant colors.bg"
        );
        assert_eq!(
            StylexError::cyclic_define_vars_reference("a -> b -> a").message,
            "Cyclic same-group references in defineVars() are not allowed: a -> b -> a."
        );
    }

    #[test]
    fn banned_shorthand_table() {
        assert_eq!(
            banned_shorthand_message("border").unwrap(),
            "border is not supported. Use border-width, border-style and border-color instead."
        );
        // Aliases throw with the message of the shorthand they point at.
        assert_eq!(
            banned_shorthand_message("borderHorizontal"),
            banned_shorthand_message("borderInline")
        );
        assert_eq!(
            banned_shorthand_message("borderBlockStart"),
            banned_shorthand_message("borderTop")
        );
        assert!(
            banned_shorthand_message("borderLeft")
                .unwrap()
                .starts_with("`borderLeft`")
        );
        assert_eq!(banned_shorthand_message("marginTop"), None);
        let err = StylexError::banned_shorthand("all").unwrap();
        assert_eq!(err.code, ErrorCode::BannedShorthand);
        assert_eq!(err.message, "all is not supported");
    }

    #[test]
    fn display_is_the_message() {
        let e = StylexError::unknown_option("fooBar");
        assert_eq!(e.to_string(), e.message);
        assert!(e.message.contains("fooBar"));
    }
}
