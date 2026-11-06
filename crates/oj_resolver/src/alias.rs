//! `resolve.alias` matching as Vite's alias plugin (@rollup/plugin-alias)
//! does it, for every consumer of the list: a string `find` takes the id
//! itself or the id plus `/…`, and the id becomes `replacement` plus the rest;
//! a RegExp `find` takes an id it tests true on, and the id becomes
//! `id.replace(find, replacement)` with JavaScript's replacement patterns.
//!
//! A RegExp `find` travels in the `(find, replacement)` list as
//! [`regex_find`] encodes it, a form no import specifier can take.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, LazyLock, Mutex};

const REGEX_FIND: &str = "\0regex:";

/// The list entry for a RegExp `find` with `source` and `flags`.
pub fn regex_find(source: &str, flags: &str) -> String {
    format!("{REGEX_FIND}{flags}:{source}")
}

/// A RegExp `find`'s `(source, flags)`, when `find` is one.
pub fn regex_parts(find: &str) -> Option<(&str, &str)> {
    let rest = find.strip_prefix(REGEX_FIND)?;
    let (flags, source) = rest.split_once(':')?;
    Some((source, flags))
}

/// A compiled RegExp `find` and whether it replaces every match (`g`).
struct Compiled {
    re: regress::Regex,
    global: bool,
}

/// Compiled finds by their encoded form; `None` for one that does not compile
/// (it matches nothing, as Vite would never have loaded such a config).
static COMPILED: LazyLock<Mutex<HashMap<String, Option<Arc<Compiled>>>>> =
    LazyLock::new(Default::default);

fn compiled(find: &str) -> Option<Arc<Compiled>> {
    let (source, flags) = regex_parts(find)?;
    let mut cache = COMPILED.lock().unwrap_or_else(|e| e.into_inner());
    cache
        .entry(find.to_string())
        .or_insert_with(|| {
            // `y` (sticky, from `lastIndex` 0) anchors at the start; `g` and
            // `d` change nothing about one match.
            let own: String = flags
                .chars()
                .filter(|c| matches!(c, 'i' | 'm' | 's' | 'u' | 'v'))
                .collect();
            let source = if flags.contains('y') {
                format!("^(?:{source})")
            } else {
                source.to_string()
            };
            let re = regress::Regex::with_flags(&source, own.as_str()).ok()?;
            Some(Arc::new(Compiled {
                re,
                global: flags.contains('g'),
            }))
        })
        .clone()
}

/// Whether `find` takes `id` (the plugin's `matches`).
pub fn matches(find: &str, id: &str) -> bool {
    if regex_parts(find).is_some() {
        return compiled(find).is_some_and(|c| c.re.find(id).is_some());
    }
    id == find
        || id
            .strip_prefix(find)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// `id` after `find` → `replacement`, when `find` takes it.
pub fn rewrite(find: &str, replacement: &str, id: &str) -> Option<String> {
    if regex_parts(find).is_none() {
        let rest = id.strip_prefix(find)?;
        return (rest.is_empty() || rest.starts_with('/')).then(|| format!("{replacement}{rest}"));
    }
    let compiled = compiled(find)?;
    let mut matches = compiled.re.find_iter(id).peekable();
    matches.peek()?;
    let mut out = String::with_capacity(id.len() + replacement.len());
    let mut last = 0;
    for m in matches {
        let groups: Vec<Option<Range<usize>>> =
            (0..=m.captures.len()).map(|i| m.group(i)).collect();
        out.push_str(&id[last..m.start()]);
        expand(&mut out, replacement, id, &groups, &m);
        last = m.end();
        if !compiled.global {
            break;
        }
    }
    out.push_str(&id[last..]);
    Some(out)
}

/// The first entry's rewrite of `id`, in the written order.
pub fn first_rewrite(entries: &[(String, String)], id: &str) -> Option<String> {
    entries
        .iter()
        .find_map(|(find, replacement)| rewrite(find, replacement, id))
}

/// One match's replacement: `$$`, `$&`, `` $` ``, `$'`, `$n` / `$nn` (two
/// digits when they name a group, JS's rule), `$<name>`.
fn expand(
    out: &mut String,
    replacement: &str,
    id: &str,
    groups: &[Option<Range<usize>>],
    m: &regress::Match,
) {
    let whole = m.range();
    let bytes = replacement.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' || i + 1 >= bytes.len() {
            let next = replacement[i..].chars().next().expect("a char");
            out.push(next);
            i += next.len_utf8();
            continue;
        }
        match bytes[i + 1] {
            b'$' => {
                out.push('$');
                i += 2;
            }
            b'&' => {
                out.push_str(&id[whole.clone()]);
                i += 2;
            }
            b'`' => {
                out.push_str(&id[..whole.start]);
                i += 2;
            }
            b'\'' => {
                out.push_str(&id[whole.end..]);
                i += 2;
            }
            b'<' => match replacement[i + 2..].split_once('>') {
                Some((name, _)) if m.named_groups().next().is_some() => {
                    if let Some(range) = m.named_group(name) {
                        out.push_str(&id[range]);
                    }
                    i += 2 + name.len() + 1;
                }
                _ => {
                    out.push('$');
                    i += 1;
                }
            },
            d if d.is_ascii_digit() => {
                let one = (d - b'0') as usize;
                let two = bytes
                    .get(i + 2)
                    .filter(|c| c.is_ascii_digit())
                    .map(|c| one * 10 + (c - b'0') as usize)
                    .filter(|n| *n > 0 && *n < groups.len());
                let (n, len) = match two {
                    Some(n) => (n, 3),
                    None => (one, 2),
                };
                if n > 0 && n < groups.len() {
                    if let Some(range) = &groups[n] {
                        out.push_str(&id[range.clone()]);
                    }
                    i += len;
                } else {
                    out.push('$');
                    i += 1;
                }
            }
            _ => {
                out.push('$');
                i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_string_find_takes_the_id_or_a_slash_prefix() {
        assert_eq!(rewrite("@", "/src", "@/a").as_deref(), Some("/src/a"));
        assert_eq!(rewrite("@", "/src", "@").as_deref(), Some("/src"));
        assert_eq!(rewrite("@", "/src", "@scope/pkg"), None);
        assert_eq!(
            rewrite("@/", "/src/", "@/a"),
            None,
            "`@/` takes `@//…` only"
        );
        assert!(matches("react", "react/jsx-runtime"));
    }

    // Vite's alias plugin tests a RegExp and applies `id.replace(find, r)`.
    #[test]
    fn a_regex_find_tests_and_replaces_as_javascript() {
        let exact = regex_find("^react$", "");
        assert_eq!(
            rewrite(&exact, "preact/compat", "react").as_deref(),
            Some("preact/compat")
        );
        assert_eq!(rewrite(&exact, "preact/compat", "react/jsx-runtime"), None);
        assert!(!matches(&exact, "react-dom"));

        let group = regex_find("^~(.*)$", "");
        assert_eq!(
            rewrite(&group, "/src/$1", "~lib/a").as_deref(),
            Some("/src/lib/a")
        );
        let suffix = regex_find("^(.*)\\.web$", "");
        assert_eq!(
            rewrite(&suffix, "$1_web", "x.web").as_deref(),
            Some("x_web")
        );
        let named = regex_find("^@(?<rest>.*)$", "");
        assert_eq!(
            rewrite(&named, "/src$<rest>", "@/a").as_deref(),
            Some("/src/a")
        );
        let lookahead = regex_find("^lodash(?!-es)", "");
        assert_eq!(
            rewrite(&lookahead, "lodash-es", "lodash/get").as_deref(),
            Some("lodash-es/get")
        );
        assert_eq!(rewrite(&lookahead, "lodash-es", "lodash-es"), None);
        let case = regex_find("^REACT$", "i");
        assert!(matches(&case, "react"));
        let global = regex_find("a", "g");
        assert_eq!(rewrite(&global, "b", "aXa").as_deref(), Some("bXb"));
        assert_eq!(
            rewrite(&regex_find("a", ""), "b", "aXa").as_deref(),
            Some("bXa")
        );
        assert_eq!(
            rewrite(&regex_find("(", ""), "x", "("),
            None,
            "a find that does not compile"
        );
    }

    #[test]
    fn the_first_entry_that_takes_the_id_wins() {
        let entries = vec![
            (regex_find("^react$", ""), "preact/compat".to_string()),
            ("react".to_string(), "/vendor/react".to_string()),
        ];
        assert_eq!(
            first_rewrite(&entries, "react").as_deref(),
            Some("preact/compat")
        );
        assert_eq!(
            first_rewrite(&entries, "react/jsx-runtime").as_deref(),
            Some("/vendor/react/jsx-runtime")
        );
    }
}
