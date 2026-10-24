//! Deterministic ordering of pseudo selectors and at-rules before hashing.
// parity: babel-plugin src/shared/utils/rule-utils.js

use crate::errors::StylexError;
use crate::jsrt::{locale_cmp, unverified_collation_char, utf16_cmp};
use std::cmp::Ordering;

/// Sorts consecutive runs of pseudo-classes (and `[attr]` selectors);
/// pseudo-elements (`::`) are barriers. The result feeds the class-name hash.
pub fn sort_pseudos<'s>(pseudos: &[&'s str]) -> Result<Vec<&'s str>, StylexError> {
    if pseudos.len() < 2 {
        return Ok(pseudos.to_vec());
    }
    let mut out: Vec<&'s str> = Vec::with_capacity(pseudos.len());
    let mut run_start = 0;
    let sort_run = |out: &mut Vec<&'s str>, start: usize, end: usize| {
        let mut run = pseudos[start..end].to_vec();
        // A comparator-sorted run with a char outside the pinned collation
        // alphabet hard-errors instead of guessing an order (r4#4 policy).
        if run.len() >= 2 {
            for pseudo in &run {
                if let Some(c) = unverified_collation_char(pseudo) {
                    return Err(StylexError::new(
                        crate::errors::ErrorCode::UnsupportedApi,
                        format!(
                            "cannot sort pseudo selector {pseudo:?}: {c:?} is outside the pinned collation alphabet and the order feeds the class-name hash"
                        ),
                    ));
                }
            }
        }
        run.sort_by(|a, b| string_comparator(a, b));
        out.extend(run);
        Ok(())
    };
    for (i, pseudo) in pseudos.iter().enumerate() {
        if pseudo.starts_with("::") {
            sort_run(&mut out, run_start, i)?;
            out.push(pseudo);
            run_start = i + 1;
        }
    }
    sort_run(&mut out, run_start, pseudos.len())?;
    Ok(out)
}

/// JS default `Array.prototype.sort` over strings: UTF-16 code-unit order.
pub fn sort_at_rules<'s>(at_rules: &[&'s str]) -> Vec<&'s str> {
    let mut sorted = at_rules.to_vec();
    sorted.sort_by(|a, b| utf16_cmp(a, b));
    sorted
}

// 'default' first, then verified localeCompare (unverified chars were
// rejected before the sort — never the UTF-16 fallback on this hash path).
fn string_comparator(a: &str, b: &str) -> Ordering {
    if a == "default" {
        return Ordering::Less;
    }
    if b == "default" {
        return Ordering::Greater;
    }
    locale_cmp(a, b).expect("unverified chars rejected before sorting")
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
            sort_pseudos(&v(&[":hover", "::before", ":active"])).unwrap(),
            v(&[":hover", "::before", ":active"])
        );
        assert_eq!(
            sort_pseudos(&v(&[":hover", ":active"])).unwrap(),
            v(&[":active", ":hover"])
        );
        assert_eq!(
            sort_pseudos(&v(&[":b", ":a", "::x", ":d", ":c"])).unwrap(),
            v(&[":a", ":b", "::x", ":c", ":d"])
        );
    }

    #[test]
    fn default_comes_first() {
        assert_eq!(
            sort_pseudos(&v(&[":hover", "default", ":active"])).unwrap(),
            v(&["default", ":active", ":hover"])
        );
    }

    #[test]
    fn short_inputs_pass_through() {
        assert_eq!(sort_pseudos(&[]).unwrap(), Vec::<String>::new());
        assert_eq!(sort_pseudos(&v(&[":hover"])).unwrap(), v(&[":hover"]));
    }

    #[test]
    fn unverified_chars_error_only_in_sorted_runs() {
        // r5#2: the oracle emits x1ua4pqi here — rust refuses loudly instead
        // of silently hashing a UTF-16-fallback order.
        let err = sort_pseudos(&v(&["[data-a]", "[data-🐱]"])).unwrap_err();
        assert!(err.to_string().contains("pinned collation alphabet"));
        // A lone unverified pseudo is never comparator-sorted: exact parity.
        assert_eq!(sort_pseudos(&v(&["[data-🐱]"])).unwrap(), v(&["[data-🐱]"]));
        assert_eq!(
            sort_pseudos(&v(&["[data-🐱]", "::before", ":hover"])).unwrap(),
            v(&["[data-🐱]", "::before", ":hover"])
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
