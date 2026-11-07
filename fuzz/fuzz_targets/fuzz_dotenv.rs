//! `.env` files are untrusted text that a build reads without asking, and the
//! defines derived from them are shipped to the browser. Parsing must be total,
//! and confinement -- no unprefixed variable in anything the client sees -- must
//! hold for every input.
#![no_main]

use std::collections::BTreeMap;

use libfuzzer_sys::fuzz_target;
use oj_env::{html_env_map, import_meta_env_defines, parse};

fuzz_target!(|data: &[u8]| {
    let Ok(contents) = std::str::from_utf8(data) else {
        return;
    };
    let mut base = BTreeMap::new();
    base.insert("PRESET".to_string(), "preset-value".to_string());

    let loaded = parse(contents, &base);
    for (key, _) in &loaded {
        assert!(!key.is_empty(), "empty key");
        assert_eq!(key.trim(), key, "untrimmed key: {key:?}");
        assert!(!key.contains('='), "key holds an '=': {key:?}");
        assert!(!key.contains('\n'), "key holds a newline: {key:?}");
    }

    let prefix = "VITE_";
    let defines = import_meta_env_defines(&loaded, "production", false, "/", &[prefix]);

    // The aggregate object is always valid JSON, and its keys are exactly the
    // individual defines.
    let aggregate = defines
        .iter()
        .find(|(k, _)| k == "import.meta.env")
        .map(|(_, v)| v.clone())
        .expect("aggregate define");
    let json: serde_json::Value = serde_json::from_str(&aggregate).expect("valid JSON object");
    let object = json.as_object().expect("object");
    let mut individual: Vec<&str> = defines
        .iter()
        .filter_map(|(k, _)| k.strip_prefix("import.meta.env."))
        .collect();
    individual.sort_unstable();
    let mut aggregated: Vec<&str> = object.keys().map(String::as_str).collect();
    aggregated.sort_unstable();
    assert_eq!(individual, aggregated, "defines disagree with the object");

    // Confinement: only builtins and prefixed variables may appear.
    const BUILTINS: [&str; 5] = ["MODE", "BASE_URL", "DEV", "PROD", "SSR"];
    for key in object.keys() {
        assert!(
            key.starts_with(prefix) || BUILTINS.contains(&key.as_str()),
            "unprefixed variable exposed: {key:?}"
        );
    }
    // A variable in the file but not in the defines must not appear by value
    // either, unless some prefixed variable legitimately holds the same text.
    // Builtin values are constants that cannot carry a leak, and a short value
    // collides with their text ("c" is inside "production") or with a key's
    // own spelling, so the check runs over the loaded entries' values only.
    let exposed: Vec<String> = object
        .iter()
        .filter(|(k, _)| !BUILTINS.contains(&k.as_str()))
        .map(|(_, v)| match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect();
    let public: Vec<&String> = loaded
        .iter()
        .filter(|(k, _)| k.starts_with(prefix))
        .map(|(_, v)| v)
        .collect();
    for (key, value) in &loaded {
        if key.starts_with(prefix) || value.is_empty() {
            continue;
        }
        // Containment, not equality: `VITE_X=${SECRET}suffix` legitimately
        // embeds the value (dotenv-expand semantics, Vite ships the same).
        if public.iter().any(|v| v.contains(value.as_str())) {
            continue;
        }
        assert!(
            !exposed.iter().any(|s| s.contains(value.as_str())),
            "value of {key:?} leaked into the defines"
        );
    }

    // The html substitution map round trips the string values. A duplicate key
    // loads twice and the defines keep the last occurrence, so compare that.
    let env = html_env_map(&defines);
    let last: BTreeMap<&str, &String> = loaded.iter().map(|(k, v)| (k.as_str(), v)).collect();
    for (key, _) in &loaded {
        if key.starts_with(prefix) {
            assert_eq!(
                env.get(key.as_str()),
                last.get(key.as_str()).copied(),
                "{key:?} not recoverable"
            );
        }
    }
});
