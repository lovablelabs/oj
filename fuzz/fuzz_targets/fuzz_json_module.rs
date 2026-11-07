//! A `.json` import is turned into JavaScript by splicing the document into a
//! module. Anything that parses as JSON must therefore come out as valid JS,
//! with a single default export and named exports that are real identifiers.
#![no_main]

use std::path::Path;

use libfuzzer_sys::fuzz_target;
use oj_compiler::{compile, json, CompileOptions};

fn parses_as_js(code: &str) -> bool {
    compile(Path::new("/src/verify.mjs"), code, &CompileOptions::prod()).is_ok()
}

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    // Only well-formed JSON reaches the interesting code; the error path is
    // covered by the assertion that both entry points agree below.
    let value: serde_json::Value = match serde_json::from_str(source) {
        Ok(value) => value,
        Err(_) => {
            assert!(json::to_esm(source, "/data.json").is_err());
            return;
        }
    };

    let esm = json::to_esm(source, "/data.json").expect("valid JSON converts");
    assert!(parses_as_js(&esm), "invalid module for {source:?}:\n{esm}");
    // Counted per line, not as a substring: a JSON string value can contain
    // the text "export default" inside the emitted literal (the crash input),
    // while the real statement always starts its own line.
    assert_eq!(
        esm.lines()
            .filter(|l| l.trim_start().starts_with("export default"))
            .count(),
        1,
        "exactly one default export:\n{esm}"
    );

    // `__proto__` in an object literal would set the prototype instead of
    // defining a property, so it must never appear as a bare key or as an
    // export name.
    if json_has_proto(&value) {
        assert!(
            !esm.contains("\"__proto__\":") || esm.contains("JSON.parse("),
            "a bare __proto__ key reached an object literal:\n{esm}"
        );
    }
    // Exact binding only: a key that merely starts with __proto__ is a legal
    // identifier and may export (the round-4 crash input).
    assert!(!esm.contains("export const __proto__ "), "{esm}");
});

fn json_has_proto(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => {
            map.contains_key("__proto__") || map.values().any(json_has_proto)
        }
        serde_json::Value::Array(items) => items.iter().any(json_has_proto),
        _ => false,
    }
}
