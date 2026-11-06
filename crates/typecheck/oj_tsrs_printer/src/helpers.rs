// helpers.go: the EmitHelper data type and its ordering; the helper definitions are in helpers_defs.rs.

use std::cmp::Ordering;

pub struct Priority {
    pub(crate) value: u32,
}

pub struct EmitHelper {
    pub scoped: bool, // Indicates whether the helper MUST be emitted in the current scope.
    pub(crate) text: &'static str, // ES3-compatible raw script text
    pub(crate) text_callback:
        Option<fn(make_unique_name: &mut dyn FnMut(&str) -> String) -> String>, // A function yielding an ES3-compatible raw script text.
    pub(crate) priority: Option<&'static Priority>, // Helpers with a higher priority are emitted earlier than other helpers on the node.
    pub(crate) dependencies: &'static [tsrs_core::SP<EmitHelper>], // Emit helpers this helper depends on
    pub import_name: &'static str, // The name of the helper to use when importing via `--importHelpers`.
}

pub(crate) fn compare_emit_helpers(
    x: tsrs_core::SP<EmitHelper>,
    y: tsrs_core::SP<EmitHelper>,
) -> Ordering {
    if x == y {
        return Ordering::Equal;
    }
    let same_priority = match (x.priority, y.priority) {
        (None, None) => true,
        (Some(a), Some(b)) => std::ptr::eq(a, b),
        _ => false,
    };
    if same_priority {
        return Ordering::Equal;
    }
    if x.priority.is_none() {
        return Ordering::Greater;
    }
    if y.priority.is_none() {
        return Ordering::Less;
    }
    x.priority.unwrap().value.cmp(&y.priority.unwrap().value)
}
