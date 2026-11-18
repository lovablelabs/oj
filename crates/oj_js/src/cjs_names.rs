// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Named exports of a CommonJS module the engine's export analysis drops
//! and Node keeps.
//!
//! The analysis the engine runs for `import` of a CommonJS file follows
//! cjs-module-lexer: an `Object.defineProperty(exports, "name", ...)` it
//! cannot read as a plain value or a side-effect-free getter marks `name`
//! unsafe and removes it from the module's names, even where an
//! `exports.name = ...` elsewhere in the file exports it. webpack's UMD
//! output does exactly that for every export of its entry (`exports.X = void
//! 0` in each inner module, then `Object.defineProperty(exports, "X", ({
//! enumerable: true, get: function () { ... } }))` with the descriptor in
//! parentheses), so `import * as ns` of pdf.js had every name but the ones it
//! really exports.
//!
//! Node did the same while it used cjs-module-lexer. The lexer current Node
//! releases use (merve, in Node 24.17) only declines to add a name for such
//! a call: a name another statement exports stays. The engine presents
//! itself as a current Node, and Vite's module runner imports an external
//! natively, so its namespace is Node's. The wrapper module the translation
//! produced is given the missing names here.

use std::collections::BTreeSet;

/// The names `source` (a CommonJS file) exports by assignment and also
/// defines with `Object.defineProperty(exports, "name", ...)`, less those
/// `wrapper` (its ES wrapper module) already exports. Only names that are
/// plain identifiers.
pub(crate) fn dropped_names(source: &str, wrapper: &str) -> Vec<String> {
    if !source.contains("defineProperty") {
        return Vec::new();
    }
    let mut names = BTreeSet::new();
    let mut rest = source;
    while let Some(at) = rest.find("defineProperty") {
        rest = &rest[at + "defineProperty".len()..];
        if let Some(name) = defined_name(rest) {
            names.insert(name);
        }
    }
    names
        .into_iter()
        .filter(|name| is_plain_identifier(name))
        .filter(|name| !wrapper.contains(&format!("export const {name} = mod[")))
        .filter(|name| assigns(source, name))
        .map(str::to_string)
        .collect()
}

/// `wrapper` with each of `names` exported off the module it loaded.
pub(crate) fn with_names(wrapper: &str, names: &[String]) -> String {
    let mut out = wrapper.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    for name in names {
        out.push_str(&format!("export const {name} = mod[\"{name}\"];\n"));
    }
    out
}

/// The literal name in `(exports, "name",` or `(module.exports, 'name',`
/// at the start of `text`.
fn defined_name(text: &str) -> Option<&str> {
    let text = text.trim_start().strip_prefix('(')?.trim_start();
    let text = match text.strip_prefix("module") {
        Some(rest) => rest.trim_start().strip_prefix('.')?.trim_start(),
        None => text,
    };
    let text = text.strip_prefix("exports")?.trim_start();
    let text = text.strip_prefix(',')?.trim_start();
    let quote = text.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    let text = &text[1..];
    let name = &text[..text.find(quote)?];
    text[name.len() + 1..]
        .trim_start()
        .starts_with(',')
        .then_some(name)
}

/// Whether `source` has `exports.name =` or `exports["name"] =` (an
/// assignment, not a comparison), as itself or after `module.`.
fn assigns(source: &str, name: &str) -> bool {
    let is_assignment = |after: &str| {
        let after = after.trim_start();
        after.starts_with('=') && !after.starts_with("==")
    };
    let mut rest = source;
    let mut before = None;
    while let Some(at) = rest.find("exports") {
        let previous = rest[..at].chars().next_back().or(before);
        let after = &rest[at + "exports".len()..];
        before = Some('s');
        rest = after;
        if previous.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$') {
            continue;
        }
        let after = after.trim_start();
        let member = if let Some(dotted) = after.strip_prefix('.') {
            dotted.trim_start().strip_prefix(name).filter(|tail| {
                !tail
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$')
            })
        } else {
            after.strip_prefix('[').and_then(|keyed| {
                let keyed = keyed.trim_start();
                ['"', '\''].iter().find_map(|quote| {
                    keyed
                        .strip_prefix(*quote)?
                        .strip_prefix(name)?
                        .strip_prefix(*quote)?
                        .trim_start()
                        .strip_prefix(']')
                })
            })
        };
        if member.is_some_and(is_assignment) {
            return true;
        }
    }
    false
}

fn is_plain_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
        && !RESERVED.contains(&name)
}

/// Words that cannot name a `const` in a module.
const RESERVED: &[&str] = &[
    "arguments",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "eval",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "mod",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "require",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

#[cfg(test)]
mod tests {
    use super::*;

    const WRAPPER: &str = "let mod;\nexport const kept = mod[\"kept\"];\nexport default mod;\n";

    #[test]
    fn a_name_assigned_and_defined_with_an_unread_descriptor_is_dropped() {
        let source = r#"
exports.Options = exports.kept = void 0;
exports . spaced = 1;
module.exports["keyed"] = 2;
Object.defineProperty(exports, "Options", ({ enumerable: true, get: function () { return _o.Options; } }));
Object.defineProperty(exports, "kept", ({ enumerable: true, get: function () { return _o.kept; } }));
Object.defineProperty(exports, 'spaced', descriptor);
Object . defineProperty ( module . exports , "keyed" , descriptor );
"#;
        assert_eq!(
            dropped_names(source, WRAPPER),
            ["Options", "keyed", "spaced"]
        );
    }

    #[test]
    fn a_name_only_defined_or_only_compared_is_not_added() {
        let source = r#"
Object.defineProperty(exports, "onlyDefined", ({ get: function () { return x(); } }));
if (exports.compared == 1 || exports.strict === 2) {}
Object.defineProperty(exports, "compared", descriptor);
Object.defineProperty(exports, "strict", descriptor);
myexports.other = 1;
Object.defineProperty(exports, "other", descriptor);
exports.prefixOfLonger = 1;
Object.defineProperty(exports, "prefix", descriptor);
Object.defineProperty(exports, key, descriptor);
Object.defineProperty(target, "elsewhere", descriptor);
exports.default = 1;
Object.defineProperty(exports, "default", descriptor);
exports["not-an-identifier"] = 1;
Object.defineProperty(exports, "not-an-identifier", descriptor);
"#;
        assert_eq!(dropped_names(source, WRAPPER), Vec::<String>::new());
        assert_eq!(
            dropped_names("exports.a = 1;", WRAPPER),
            Vec::<String>::new()
        );
    }

    #[test]
    fn the_names_are_exported_off_the_loaded_module() {
        let out = with_names(WRAPPER, &["Options".to_string()]);
        assert!(out.starts_with(WRAPPER), "{out}");
        assert!(
            out.ends_with("export const Options = mod[\"Options\"];\n"),
            "{out}"
        );
    }
}
