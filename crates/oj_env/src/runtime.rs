//! The `import.meta.env` keys whose values a dev server takes at request time
//! (`OJ_RUNTIME_ENV_KEYS`) instead of baking them into compiled modules.

/// One runtime key: the `import.meta.env` name client code reads, and the
/// field of the runtime values file that supplies it (the key itself unless
/// the spec maps it with `KEY=field`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeKey {
    pub key: String,
    pub field: String,
}

/// Parses `OJ_RUNTIME_ENV_KEYS`: comma-separated entries, each `KEY` or
/// `KEY=field`, whitespace around either part ignored, empty entries skipped,
/// a repeated key keeping its first mapping. A key must be a JavaScript
/// identifier name, since it becomes an `import.meta.env.KEY` define; a
/// field may be any non-empty text.
pub fn parse_runtime_env_keys(spec: &str) -> Result<Vec<RuntimeKey>, String> {
    let mut keys: Vec<RuntimeKey> = Vec::new();
    for entry in spec.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (key, field) = match entry.split_once('=') {
            Some((key, field)) => (key.trim(), field.trim()),
            None => (entry, entry),
        };
        if !is_identifier_name(key) || key == "__proto__" {
            return Err(format!(
                "OJ_RUNTIME_ENV_KEYS: `{key}` is not a valid import.meta.env key (an identifier name)"
            ));
        }
        if field.is_empty() {
            return Err(format!(
                "OJ_RUNTIME_ENV_KEYS: `{entry}` names no field for `{key}`"
            ));
        }
        if keys.iter().any(|k| k.key == key) {
            continue;
        }
        keys.push(RuntimeKey {
            key: key.to_string(),
            field: field.to_string(),
        });
    }
    Ok(keys)
}

/// ECMAScript IdentifierName over ASCII plus any non-ASCII letter: what a
/// define key part may be (oxc's `is_identifier_name` for the keys oj builds).
fn is_identifier_name(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let start = |c: char| c == '$' || c == '_' || c.is_alphabetic();
    start(first) && chars.all(|c| start(c) || c.is_numeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(spec: &str) -> Vec<(String, String)> {
        parse_runtime_env_keys(spec)
            .unwrap()
            .into_iter()
            .map(|k| (k.key, k.field))
            .collect()
    }

    #[test]
    fn plain_keys_read_their_own_field() {
        assert_eq!(
            keys("VITE_A,VITE_B"),
            [("VITE_A", "VITE_A"), ("VITE_B", "VITE_B")]
                .map(|(k, f)| (k.to_string(), f.to_string()))
        );
    }

    #[test]
    fn mapped_keys_read_the_named_field() {
        assert_eq!(
            keys("VITE_SUPABASE_URL=url, VITE_SUPABASE_PUBLISHABLE_KEY = anonKey"),
            [
                ("VITE_SUPABASE_URL", "url"),
                ("VITE_SUPABASE_PUBLISHABLE_KEY", "anonKey")
            ]
            .map(|(k, f)| (k.to_string(), f.to_string()))
        );
    }

    #[test]
    fn whitespace_and_empty_entries_are_skipped() {
        assert_eq!(
            keys(" , VITE_A , ,VITE_B,, "),
            [("VITE_A", "VITE_A"), ("VITE_B", "VITE_B")]
                .map(|(k, f)| (k.to_string(), f.to_string()))
        );
        assert!(keys("").is_empty());
        assert!(keys(" , , ").is_empty());
    }

    #[test]
    fn a_repeated_key_keeps_its_first_mapping() {
        assert_eq!(
            keys("VITE_A=first,VITE_A=second,VITE_A"),
            [("VITE_A", "first")].map(|(k, f)| (k.to_string(), f.to_string()))
        );
    }

    #[test]
    fn a_key_that_is_not_an_identifier_is_refused() {
        // `__proto__` would be a prototype setter in the served object literal.
        for bad in [
            "VITE-A",
            "1VITE",
            "VITE A",
            "VITE.A",
            "=url",
            "a/b",
            "__proto__",
        ] {
            let err = parse_runtime_env_keys(bad).unwrap_err();
            assert!(err.contains("not a valid"), "{bad}: {err}");
        }
        // Dollar, underscore, digits after the first char and non-ASCII letters are fine.
        assert_eq!(keys("$a,_b,c1,ünï").len(), 4);
    }

    #[test]
    fn an_empty_field_is_refused() {
        let err = parse_runtime_env_keys("VITE_A=").unwrap_err();
        assert!(err.contains("names no field"), "{err}");
        let err = parse_runtime_env_keys("VITE_A=  ").unwrap_err();
        assert!(err.contains("names no field"), "{err}");
    }

    #[test]
    fn a_field_may_hold_any_text() {
        assert_eq!(
            keys("VITE_A=some.nested-field with spaces"),
            [("VITE_A", "some.nested-field with spaces")]
                .map(|(k, f)| (k.to_string(), f.to_string()))
        );
    }
}
