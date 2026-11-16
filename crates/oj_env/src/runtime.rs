//! The `import.meta.env` keys whose values a dev server takes at request time
//! (`OJ_RUNTIME_ENV_KEYS`) instead of baking them into compiled modules.

/// Parses `OJ_RUNTIME_ENV_KEYS`: comma-separated keys, whitespace around
/// each ignored, empty entries skipped, a repeated key listed once. A key
/// must be a JavaScript identifier name, since it becomes an
/// `import.meta.env.KEY` define. An entry holding `=` is refused by name:
/// there is no `KEY=field` mapping, and the entry would otherwise be taken
/// for a misspelled key.
pub fn parse_runtime_env_keys(spec: &str) -> Result<Vec<String>, String> {
    let mut keys: Vec<String> = Vec::new();
    for entry in spec.split(',') {
        let key = entry.trim();
        if key.is_empty() {
            continue;
        }
        if key.contains('=') {
            return Err(format!(
                "OJ_RUNTIME_ENV_KEYS: `{key}` maps a key to a field, which is no longer supported; list the key alone and set its value with POST /@oj/env"
            ));
        }
        if !is_identifier_name(key) || key == "__proto__" {
            return Err(format!(
                "OJ_RUNTIME_ENV_KEYS: `{key}` is not a valid import.meta.env key (an identifier name)"
            ));
        }
        if !keys.iter().any(|k| k == key) {
            keys.push(key.to_string());
        }
    }
    Ok(keys)
}

use unicode_id_start::{
    is_id_continue_unicode as is_id_continue, is_id_start_unicode as is_id_start,
};

/// ECMAScript IdentifierName, by the rule oxc's `is_identifier_name` holds a
/// define key part to (ID_Start and ID_Continue, `$`, `_`, and the two
/// joiners inside). A key oxc refuses fails the whole define config, and
/// with it every other define of the compile, so it is refused here, by name.
fn is_identifier_name(s: &str) -> bool {
    let start = |c: char| {
        c == '$' || c == '_' || c.is_ascii_alphabetic() || (!c.is_ascii() && is_id_start(c))
    };
    let part = |c: char| {
        start(c)
            || c.is_ascii_digit()
            || (!c.is_ascii() && is_id_continue(c))
            || c == '\u{200c}'
            || c == '\u{200d}'
    };
    let mut chars = s.chars();
    chars.next().is_some_and(start) && chars.all(part)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(spec: &str) -> Vec<String> {
        parse_runtime_env_keys(spec).unwrap()
    }

    #[test]
    fn keys_are_listed_in_order() {
        assert_eq!(keys("VITE_A,VITE_B"), ["VITE_A", "VITE_B"]);
    }

    #[test]
    fn whitespace_and_empty_entries_are_skipped() {
        assert_eq!(keys(" , VITE_A , ,VITE_B,, "), ["VITE_A", "VITE_B"]);
        assert!(keys("").is_empty());
        assert!(keys(" , , ").is_empty());
    }

    #[test]
    fn a_repeated_key_is_listed_once() {
        assert_eq!(keys("VITE_A,VITE_B, VITE_A"), ["VITE_A", "VITE_B"]);
    }

    #[test]
    fn a_key_that_is_not_an_identifier_is_refused() {
        // `__proto__` would be a prototype setter in the served object literal.
        for bad in ["VITE-A", "1VITE", "VITE A", "VITE.A", "a/b", "__proto__"] {
            let err = parse_runtime_env_keys(bad).unwrap_err();
            assert!(err.contains("not a valid"), "{bad}: {err}");
        }
        // Dollar, underscore, digits after the first char and non-ASCII letters are fine.
        assert_eq!(keys("$a,_b,c1,ünï").len(), 4);
    }

    // Letters by `char::is_alphabetic` or digits by `char::is_numeric` that
    // are no ID_Start or ID_Continue: oxc refuses a define key holding one,
    // and the config failing took every define of the compile with it.
    #[test]
    fn a_key_oxc_would_refuse_as_a_define_is_refused() {
        for bad in [
            "VITE_x\u{b2}",
            "VITE_\u{bd}",
            "VITE_\u{2460}",
            "\u{24b6}VITE",
            "\u{200d}VITE",
        ] {
            let err = parse_runtime_env_keys(bad).unwrap_err();
            assert!(err.contains("not a valid"), "{bad}: {err}");
        }
        for good in [
            "VITE_caf\u{e9}",
            "\u{540d}\u{524d}",
            "VITE_a\u{200d}b",
            "VITE_e\u{301}",
            "$_9",
        ] {
            assert_eq!(keys(good), [good]);
        }
    }

    // There is no `KEY=field` mapping: the entry is named, not read as a
    // key called `VITE_A=field`.
    #[test]
    fn a_mapped_entry_is_refused_by_name() {
        for bad in ["VITE_A=field", "VITE_B, VITE_A = field", "VITE_A=", "=url"] {
            let err = parse_runtime_env_keys(bad).unwrap_err();
            assert!(err.starts_with("OJ_RUNTIME_ENV_KEYS: `"), "{bad}: {err}");
            assert!(err.contains("no longer supported"), "{bad}: {err}");
            assert!(
                err.contains(bad.rsplit(',').next().unwrap().trim()),
                "{bad}: {err}"
            );
        }
    }
}
