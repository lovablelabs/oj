//! `splitValue`: the value splitter the legacy shorthand expansions run on.
// parity: babel-plugin src/shared/utils/split-css-value.js

use crate::shared::css_value::{Kind, Node, parse};
use crate::shared::flatten::StyleScalar;
use std::borrow::Cow;

/// `null`, `undefined` and numbers pass through as a single part; a string
/// splits on the top-level space/div nodes, re-printed without their padding.
pub fn split_value<'a>(value: Option<&StyleScalar<'a>>) -> Vec<Option<StyleScalar<'a>>> {
    let Some(StyleScalar::Str(raw)) = value else {
        return vec![value.cloned()];
    };
    let nodes = parse(raw.trim());
    let mut parts: Vec<String> = nodes
        .iter()
        .filter(|n| n.kind != Kind::Space && n.kind != Kind::Div)
        .map(print_node)
        .collect();
    if parts.len() > 1
        && parts
            .last()
            .is_some_and(|p| p.eq_ignore_ascii_case("!important"))
    {
        parts.pop();
        for part in &mut parts {
            part.push_str(" !important");
        }
    }
    parts
        .into_iter()
        .map(|p| Some(StyleScalar::Str(Cow::Owned(p))))
        .collect()
}

// parity: split-css-value.js printNode — `before`/`after` padding is dropped
// and an unclosed function is re-printed closed.
fn print_node(node: &Node) -> String {
    match node.kind {
        Kind::Str => {
            let quote = node.quote as char;
            let mut out = String::with_capacity(node.value.len() + 2);
            out.push(quote);
            out.push_str(&node.value);
            out.push(quote);
            out
        }
        Kind::Func => {
            let mut out = String::with_capacity(node.value.len() + 2);
            out.push_str(&node.value);
            out.push('(');
            for child in &node.nodes {
                out.push_str(&print_node(child));
            }
            out.push(')');
            out
        }
        _ => node.value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(v: &str) -> Vec<String> {
        split_value(Some(&StyleScalar::Str(Cow::Borrowed(v))))
            .into_iter()
            .map(|p| match p {
                Some(StyleScalar::Str(s)) => s.into_owned(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn oracle_pinned_splits() {
        // Pinned via live-oracle probes 2026-08-29 against babel-plugin 0.19.0.
        assert_eq!(split("1px 2px"), vec!["1px", "2px"]);
        assert_eq!(split("  1px 2px  "), vec!["1px", "2px"]);
        assert_eq!(split("1px,2px"), vec!["1px", "2px"]);
        assert_eq!(split("1px 2px / 3px 4px"), vec!["1px", "2px", "3px", "4px"]);
        assert_eq!(
            split("calc(1px + 2px) min(1px, 2px)"),
            vec!["calc(1px + 2px)", "min(1px,2px)"]
        );
        // The parser closes the function; property-specificity throws instead.
        assert_eq!(split("calc(1px + 2px"), vec!["calc(1px + 2px)"]);
        assert_eq!(split("var(--a, 1px) 2px"), vec!["var(--a,1px)", "2px"]);
        assert_eq!(split("'a' \"b\""), vec!["'a'", "\"b\""]);
        assert!(split("").is_empty());
        assert!(split("   ").is_empty());
    }

    #[test]
    fn important_suffixing() {
        assert_eq!(
            split("1px 2px !IMPORTANT"),
            vec!["1px !important", "2px !important"]
        );
        // A lone "!important" is length 1, so nothing is sliced off.
        assert_eq!(split("!important"), vec!["!important"]);
        assert_eq!(
            split("1px !important 2px"),
            vec!["1px", "!important", "2px"]
        );
    }

    #[test]
    fn non_strings_pass_through() {
        assert_eq!(split_value(None), vec![None]);
        assert_eq!(
            split_value(Some(&StyleScalar::Num(5.0))),
            vec![Some(StyleScalar::Num(5.0))]
        );
        assert_eq!(
            split_value(Some(&StyleScalar::Undefined)),
            vec![Some(StyleScalar::Undefined)]
        );
    }
}
