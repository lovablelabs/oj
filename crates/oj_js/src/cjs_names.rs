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

/// How the ES wrapper the translation writes for a CommonJS file begins.
pub(crate) const WRAPPER_FIRST_LINE: &str = "import { createRequire as __internalCreateRequire, Module as __internalModule } from \"node:module\";";

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
    let assigned = assigned_names(source);
    names
        .into_iter()
        // What the wrapper's own two exports are called, and names a string
        // literal here could not hold as they are (the wrapper writes such
        // a name escaped, so it could not be told from this one either).
        .filter(|name| !matches!(*name, "default" | "module.exports"))
        .filter(|name| !name.contains(['\\', '"']) && !name.chars().any(|c| c < ' '))
        .filter(|name| !exported.contains(*name))
        .filter(|name| assigned.contains(*name))
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
    // A binding name the wrapper does not hold: a package may export a
    // name spelled like oj's.
    let mut prefix = "__oj_export_".to_string();
    while wrapper.contains(&prefix) {
        prefix.push('_');
    }
    for (n, name) in names.iter().enumerate() {
        out.push_str(&format!(
            "let {prefix}{n}__; try {{ {prefix}{n}__ = mod[\"{name}\"]; }} catch {{}}\nexport {{ {prefix}{n}__ as \"{name}\" }};\n"
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

/// The names `source` assigns with `exports.name =` or `exports["name"] =`
/// (an assignment, not a comparison), on `exports` itself or on
/// `module.exports`: the two objects cjs-module-lexer reads assignments to.
/// webpack's inner modules take the first form, `exports` being their
/// parameter. `other.exports.name =` is not one. One pass over the file.
fn assigned_names(source: &str) -> BTreeSet<&str> {
    let mut names = BTreeSet::new();
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
            let dotted = dotted.trim_start();
            let end = dotted
                .find(|c: char| !is_identifier_char(c))
                .unwrap_or(dotted.len());
            (end > 0).then(|| (&dotted[..end], &dotted[end..]))
        } else {
            after.strip_prefix('[').and_then(|keyed| {
                let keyed = keyed.trim_start();
                let quote = keyed.chars().next().filter(|c| matches!(c, '"' | '\''))?;
                let keyed = &keyed[1..];
                let name = &keyed[..keyed.find(quote)?];
                let tail = keyed[name.len() + 1..].trim_start().strip_prefix(']')?;
                Some((name, tail))
            })
        };
        if let Some((name, tail)) = member {
            let tail = tail.trim_start();
            if tail.starts_with('=') && !tail.starts_with("==") {
                names.insert(name);
            }
        }
    }
    names
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

    /// The wrapper writes a name with a control character escaped, which
    /// the raw name here would not match: such a name is left out. A package
    /// exporting a name spelled like oj's bindings gets other bindings.
    #[test]
    fn a_name_that_cannot_be_compared_or_would_collide_is_safe() {
        let source = "exports[\"tab\tbed\"] = 1;\nObject.defineProperty(exports, \"tab\tbed\", descriptor);\nexports.added = 1;\nObject.defineProperty(exports, \"added\", descriptor);\n";
        let wrapper = "let mod;\nexport const __oj_export_0__ = mod[\"__oj_export_0__\"];\nexport default mod;\n";
        let names = dropped_names(source, wrapper);
        assert_eq!(names, ["added"]);
        let out = with_names(wrapper, &names);
        assert!(
            out.ends_with("let __oj_export__0__; try { __oj_export__0__ = mod[\"added\"]; } catch {}\nexport { __oj_export__0__ as \"added\" };\n"),
            "{out}"
        );
        assert_eq!(out.matches("__oj_export_0__ ").count(), 1, "{out}");
    }

    /// `__esModule` is a name like any other: Node lists it when the file
    /// assigns it.
    #[test]
    fn es_module_flag_is_added_when_assigned_and_defined() {
        let source = "exports.__esModule = true;\nObject.defineProperty(exports, \"__esModule\", ({ value: true }));\n";
        assert_eq!(dropped_names(source, WRAPPER), ["__esModule"]);
        let only_defined = "Object.defineProperty(exports, \"__esModule\", ({ value: true }));\n";
        assert_eq!(dropped_names(only_defined, WRAPPER), Vec::<String>::new());
    }

    /// Every candidate is looked up in one set of assigned names, built in
    /// one pass: a large bundle with many definitions is not rescanned per
    /// name.
    #[test]
    fn the_assigned_names_are_collected_in_one_pass() {
        let mut source = String::new();
        for n in 0..2000 {
            source.push_str(&format!(
                "exports.name{n} = void 0;\nObject.defineProperty(exports, \"name{n}\", ({{ get: function () {{ return m.name{n}; }} }}));\n"
            ));
        }
        let assigned = assigned_names(&source);
        assert_eq!(assigned.len(), 2000);
        assert_eq!(dropped_names(&source, WRAPPER).len(), 2000);
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
