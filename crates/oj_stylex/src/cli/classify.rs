//! Error-class strings for batch results.
// parity: conformance/src/lib.mjs ERROR_CLASSES — same strings, same order.

use crate::errors::{ErrorCode, StylexError};

pub fn classify(error: &StylexError) -> &'static str {
    match error.code {
        ErrorCode::UnsupportedApi | ErrorCode::UnsupportedOption | ErrorCode::UnknownOption => {
            "UNSUPPORTED_API"
        }
        _ => classify_message(&error.message),
    }
}

const NON_STATIC: [&str; 13] = [
    "Only static values are allowed",
    "Unsupported expression:",
    "Unsupported operator:",
    "Unsupported object method",
    "Referenced constant is not",
    "Referenced value is not a constant",
    "Referenced value is used before declaration",
    "Could not resolve the path to the imported file",
    "Cannot use default imports",
    "Could not resolve the code being evaluated",
    "Could not determine the property being accessed",
    "attempting to parse the imported file",
    "attempting to evaluate the imported file",
];

const VALIDATION: [&str; 21] = [
    "can only accept an object",
    "must be bound to a named export",
    "calls must be bound to a bare variable",
    "Unable to generate hash",
    "must be zero-argument",
    "Unknown same-group reference",
    "cannot be used more than once",
    "Escaping a create() value",
    "Pseudo objects can't be nested",
    "A style value can only contain",
    "A style array value can only contain",
    "A StyleX namespace must be an object",
    "Invalid pseudo selector",
    "Invalid pseudo or at-rule",
    "should not be exported",
    "keyframes() call must be an object",
    "Only named parameters are allowed",
    "only allowed at the root of a program",
    "Unknown property key",
    "Invalid property in `positionTry()`",
    "Invalid property in `viewTransitionClass()`",
];

pub fn classify_message(message: &str) -> &'static str {
    if message.contains("Object spreads are not allowed") {
        return "noObjectSpreads";
    }
    if message.contains("must be contiguous") {
        return "nonContiguousVars";
    }
    if message.contains("Invalid media query syntax") {
        return "invalidMediaQuery";
    }
    if banned_shorthand_shape(message) {
        return "bannedShorthand";
    }
    if message.contains("is not supported in defineVars")
        || message.contains("Unsupported value type in defineVars")
    {
        return "defineVarsValue";
    }
    if message.contains("circular reference detected for constant")
        || message.contains("Cyclic same-group references")
    {
        return "cyclicConst";
    }
    if message.contains("Rule contains an unclosed function")
        || message.contains("Rule contains an unclosed string")
    {
        return "unclosedValue";
    }
    if NON_STATIC.iter().any(|needle| message.contains(needle)) {
        return "nonStaticValue";
    }
    if VALIDATION.iter().any(|needle| message.contains(needle)) || argument_count_shape(message) {
        return "validation";
    }
    "unknown"
}

// parity: /^[a-zA-Z-]+ is not supported($|\.)/
fn banned_shorthand_shape(message: &str) -> bool {
    let head_len = message
        .chars()
        .take_while(|c| c.is_ascii_alphabetic() || *c == '-')
        .count();
    if head_len == 0 {
        return false;
    }
    let rest = &message[head_len..];
    let Some(after) = rest.strip_prefix(" is not supported") else {
        return false;
    };
    after.is_empty() || after.starts_with('.')
}

// parity: /should have \d+ arguments?/
fn argument_count_shape(message: &str) -> bool {
    let mut rest = message;
    while let Some(idx) = rest.find("should have ") {
        let tail = &rest[idx + "should have ".len()..];
        let digits = tail.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && tail[digits..].starts_with(" argument") {
            return true;
        }
        rest = &rest[idx + 1..];
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirrors_lib_mjs_classes() {
        assert_eq!(
            classify_message("border is not supported. Use x"),
            "bannedShorthand"
        );
        assert_eq!(classify_message("all is not supported"), "bannedShorthand");
        assert_eq!(
            classify_message("Array is not supported in defineVars"),
            "defineVarsValue"
        );
        assert_eq!(
            classify_message("create() should have 1 argument."),
            "validation"
        );
        assert_eq!(
            classify_message("keyframes() should have 2 arguments."),
            "validation"
        );
        assert_eq!(
            classify_message("Only static values are allowed inside of a create() call."),
            "nonStaticValue"
        );
        assert_eq!(
            classify_message("Unsupported expression: CallExpression"),
            "nonStaticValue"
        );
        assert_eq!(
            classify_message("Object spreads are not allowed in create() calls."),
            "noObjectSpreads"
        );
        assert_eq!(classify_message("something novel"), "unknown");
    }
}
