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
/// `wrapper` (its ES wrapper module) already exports.
///
/// The scan is textual, as Node's lexers are, and does not skip strings or
/// comments: a match there can only add a name that a `defineProperty` call
/// also names, bound to whatever module.exports holds under it.
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
    let exported = wrapper_exports(wrapper);
    names
        .into_iter()
        // What the wrapper's own two exports are called, and names a string
        // literal here could not hold as they are.
        .filter(|name| !matches!(*name, "default" | "module.exports"))
        .filter(|name| !name.contains(['\\', '"', '\n', '\r']))
        .filter(|name| !exported.contains(*name))
        .filter(|name| assigns(source, name))
        .map(str::to_string)
        .collect()
}

/// The names `wrapper` exports, in either form the translation writes:
/// `export const name = mod["name"];`, or for a name that cannot be a
/// binding (a reserved word, `get` and `async` among them, or no identifier)
/// `const __deno_export_1__ = mod["name"]; export { __deno_export_1__ as
/// "name" };`.
fn wrapper_exports(wrapper: &str) -> BTreeSet<&str> {
    let mut names = BTreeSet::new();
    for line in wrapper.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("export const ") {
            if let Some((name, _)) = rest.split_once(' ') {
                names.insert(name);
            }
        } else if let Some(rest) = line.strip_prefix("export {") {
            if let Some((_, alias)) = rest.split_once(" as ") {
                let alias = alias.trim_end().trim_end_matches(';').trim_end();
                let alias = alias.strip_suffix('}').unwrap_or(alias).trim();
                names.insert(alias.trim_matches('"'));
            }
        }
    }
    names
}

/// `wrapper` with each of `names` exported off the module it loaded. Every
/// name is exported through a binding of oj's own, so a reserved word or a
/// name that is no identifier is as good as any; a getter that throws leaves
/// the export `undefined` instead of failing the import.
pub(crate) fn with_names(wrapper: &str, names: &[String]) -> String {
    let mut out = wrapper.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    for (n, name) in names.iter().enumerate() {
        out.push_str(&format!(
            "let __oj_export_{n}__; try {{ __oj_export_{n}__ = mod[\"{name}\"]; }} catch {{}}\nexport {{ __oj_export_{n}__ as \"{name}\" }};\n"
        ));
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

fn is_identifier_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Whether `source` has `exports.name =` or `exports["name"] =` (an
/// assignment, not a comparison), on `exports` itself or on
/// `module.exports`: the two objects cjs-module-lexer reads assignments to.
/// webpack's inner modules take the first form, `exports` being their
/// parameter. `other.exports.name =` is not one.
fn assigns(source: &str, name: &str) -> bool {
    let is_assignment = |after: &str| {
        let after = after.trim_start();
        after.starts_with('=') && !after.starts_with("==")
    };
    let mut from = 0;
    while let Some(found) = source[from..].find("exports") {
        let at = from + found;
        from = at + "exports".len();
        let before = source[..at].trim_end();
        let on_exports = match before.chars().next_back() {
            Some('.') => {
                let object = before[..before.len() - 1].trim_end();
                object.strip_suffix("module").is_some_and(|rest| {
                    !rest
                        .chars()
                        .next_back()
                        .is_some_and(|c| is_identifier_char(c) || c == '.')
                })
            }
            Some(c) => !is_identifier_char(c) || before.len() != at,
            None => true,
        };
        if !on_exports {
            continue;
        }
        let after = source[from..].trim_start();
        let member = if let Some(dotted) = after.strip_prefix('.') {
            dotted
                .trim_start()
                .strip_prefix(name)
                .filter(|tail| !tail.chars().next().is_some_and(is_identifier_char))
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

#[cfg(test)]
mod tests {
    use super::*;

    const WRAPPER: &str = "let mod;\nexport const kept = mod[\"kept\"];\nconst __deno_export_1__ = mod[\"get\"];\nexport { __deno_export_1__ as \"get\" };\nexport default mod;\nconst __deno_export_2__ = mod;\nexport { __deno_export_2__ as \"module.exports\" };\n";

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

    /// A TypeScript barrel (`export { get, set, async } from "./ops"`): the
    /// translation exports a reserved word through an alias, which counts
    /// as exported; one it left out is added, and is valid as an export.
    #[test]
    fn a_reserved_word_the_wrapper_exports_by_alias_is_not_added_again() {
        let source = r#"
exports.async = exports.set = exports.get = void 0;
Object.defineProperty(exports, "get", { enumerable: true, get: function () { return ops_1.get; } });
Object.defineProperty(exports, "set", ({ enumerable: true, get: function () { return ops_1.set; } }));
Object.defineProperty(exports, "async", ({ enumerable: true, get: function () { return ops_1.async; } }));
"#;
        let names = dropped_names(source, WRAPPER);
        assert_eq!(names, ["async", "set"], "`get` is exported by alias");
        let out = with_names(WRAPPER, &names);
        assert_eq!(out.matches("as \"get\"").count(), 1, "{out}");
        assert!(
            out.contains("let __oj_export_0__; try { __oj_export_0__ = mod[\"async\"]; } catch {}\nexport { __oj_export_0__ as \"async\" };\n"),
            "{out}"
        );
        assert!(
            out.contains("export { __oj_export_1__ as \"set\" };"),
            "{out}"
        );
        assert!(!out.contains("export const async") && !out.contains("export const set"));
    }

    #[test]
    fn a_name_only_defined_or_only_compared_is_not_added() {
        let source = r#"
Object.defineProperty(exports, "onlyDefined", ({ get: function () { return x(); } }));
if (exports.compared == 1 || exports.strict === 2) {}
Object.defineProperty(exports, "compared", descriptor);
Object.defineProperty(exports, "strict", descriptor);
myexports.other = 1;
other.exports.other = 1;
other.module.exports.other = 1;
Object.defineProperty(exports, "other", descriptor);
exports.prefixOfLonger = 1;
Object.defineProperty(exports, "prefix", descriptor);
Object.defineProperty(exports, key, descriptor);
Object.defineProperty(target, "elsewhere", descriptor);
exports.default = 1;
Object.defineProperty(exports, "default", descriptor);
exports["quo\"ted"] = 1;
Object.defineProperty(exports, 'quo"ted', descriptor);
"#;
        assert_eq!(dropped_names(source, WRAPPER), Vec::<String>::new());
        assert_eq!(
            dropped_names("exports.a = 1;", WRAPPER),
            Vec::<String>::new()
        );
    }

    /// A name that is no identifier is exported under its string name.
    #[test]
    fn a_name_that_is_no_identifier_is_exported_by_its_string() {
        let source = "exports[\"not-an-identifier\"] = 1;\nObject.defineProperty(exports, \"not-an-identifier\", descriptor);\n";
        let names = dropped_names(source, WRAPPER);
        assert_eq!(names, ["not-an-identifier"]);
        assert!(with_names(WRAPPER, &names)
            .ends_with("export { __oj_export_0__ as \"not-an-identifier\" };\n"));
    }
}
