//! Deterministic ordering of pseudo selectors and at-rules before hashing.
// parity: babel-plugin src/shared/utils/rule-utils.js

use crate::jsrt::{default_first_cmp, utf16_cmp};

/// Sorts consecutive runs of pseudo-classes (and `[attr]` selectors);
/// pseudo-elements (`::`) are barriers. The result feeds the class-name hash.
pub fn sort_pseudos<'s>(pseudos: &[&'s str]) -> Vec<&'s str> {
    let mut sorted = pseudos.to_vec();
    for run in sorted.split_mut(|pseudo| pseudo.starts_with("::")) {
        run.sort_by(|a, b| default_first_cmp(a, b));
    }
    sorted
}

/// JS default `Array.prototype.sort` over strings: UTF-16 code-unit order.
pub fn sort_at_rules<'s>(at_rules: &[&'s str]) -> Vec<&'s str> {
    let mut sorted = at_rules.to_vec();
    sorted.sort_by(|a, b| utf16_cmp(a, b));
    sorted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v<'a>(items: &[&'a str]) -> Vec<&'a str> {
        items.to_vec()
    }

    #[test]
    fn classes_sort_elements_are_barriers() {
        assert_eq!(
            sort_pseudos(&v(&[":hover", "::before", ":active"])),
            v(&[":hover", "::before", ":active"])
        );
        assert_eq!(
            sort_pseudos(&v(&[":hover", ":active"])),
            v(&[":active", ":hover"])
        );
        assert_eq!(
            sort_pseudos(&v(&[":b", ":a", "::x", ":d", ":c"])),
            v(&[":a", ":b", "::x", ":c", ":d"])
        );
    }

    #[test]
    fn default_comes_first() {
        assert_eq!(
            sort_pseudos(&v(&[":hover", "default", ":active"])),
            v(&["default", ":active", ":hover"])
        );
    }

    #[test]
    fn short_inputs_pass_through() {
        assert_eq!(sort_pseudos(&[]), Vec::<String>::new());
        assert_eq!(sort_pseudos(&v(&[":hover"])), v(&[":hover"]));
    }

    #[test]
    fn pseudo_runs_sort_by_utf16_code_unit() {
        assert_eq!(
            sort_pseudos(&[
                "[data-\u{e000}]",
                "[data-a]",
                "[data-🐱]",
                "[data-B]",
                "::before",
                ":hover"
            ]),
            [
                "[data-B]",
                "[data-a]",
                "[data-🐱]",
                "[data-\u{e000}]",
                "::before",
                ":hover"
            ]
        );
    }

    #[test]
    fn at_rules_sort_by_code_unit() {
        assert_eq!(
            sort_at_rules(&v(&["@supports x", "@media y", "@Media z"])),
            v(&["@Media z", "@media y", "@supports x"])
        );
    }
}
