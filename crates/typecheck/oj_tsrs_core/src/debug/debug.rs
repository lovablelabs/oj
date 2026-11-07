use std::fmt::Display;

pub fn fail(reason: &str) -> ! {
    let reason = if reason.is_empty() {
        "Debug failure.".to_string()
    } else {
        format!("Debug failure. {reason}")
    };
    panic!("{}", reason);
}

/// Go takes the node (for its `KindString()`) and optional message parts.
pub fn fail_bad_syntax_kind(kind_string: &str, message: &[&dyn Display]) -> ! {
    let msg = if message.is_empty() {
        "Unexpected node.".to_string()
    } else {
        message.iter().map(std::string::ToString::to_string).collect::<String>()
    };
    fail(&format!("{msg}\nNode {kind_string} was unexpected."));
}

pub fn assert_never(member: &dyn Display, message: &[&dyn Display]) -> ! {
    let msg = if message.is_empty() {
        "Illegal value:".to_string()
    } else {
        message.iter().map(std::string::ToString::to_string).collect::<String>()
    };
    fail(&format!("{msg} {member}"));
}

#[inline]
pub fn assert(value: bool, message: &[&dyn Display]) {
    if value {
        return;
    }
    assert_slow(message);
}

#[cold]
fn assert_slow(message: &[&dyn Display]) {
    let msg = if message.is_empty() {
        "False expression.".to_string()
    } else {
        format!(
            "False expression: {}",
            message.iter().map(std::string::ToString::to_string).collect::<String>()
        )
    };
    fail(&msg);
}
