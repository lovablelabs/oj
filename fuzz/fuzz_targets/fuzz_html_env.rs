//! `%KEY%` substitution runs over an `index.html` nobody validated, against a
//! map derived from a `.env` nobody validated. It must be total and must match
//! Vite's `html.replace(/%(\S+?)%/g, (text, key) => key in env ? env[key] : text)`
//! exactly, placeholder consumption included.
#![no_main]

use std::collections::BTreeMap;

use libfuzzer_sys::fuzz_target;
use oj_env::replace_html_env;

/// Vite's substitution written out as a scan, derived from the regex, not from
/// oj's code: at a `%`, the first `%` at least two characters later closes the
/// candidate (`\S+?` is non-greedy and must consume something, so a `%` right
/// after the opener is key content); JS whitespace before a close fails the
/// candidate and the engine retries from the next character; a matched
/// placeholder is consumed whole whether or not its key is in the map.
fn vite_reference(html: &str, env: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    let mut rest = html;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let mut close = None;
        for (off, ch) in rest.char_indices().skip(1) {
            if ch == '%' && off > 1 {
                close = Some(off);
                break;
            }
            if ch != '%' && js_regex_whitespace(ch) {
                break;
            }
        }
        let Some(close) = close else {
            out.push('%');
            rest = &rest[1..];
            continue;
        };
        match env.get(&rest[1..close]) {
            Some(val) => out.push_str(val),
            None => out.push_str(&rest[..close + 1]),
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

/// JavaScript's regex `\s` class.
fn js_regex_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{B}' | '\u{C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

fuzz_target!(|data: &[u8]| {
    let Ok(html) = std::str::from_utf8(data) else {
        return;
    };

    // With nothing to substitute, substitution is the identity.
    assert_eq!(replace_html_env(html, &BTreeMap::new()), html);

    let mut env = BTreeMap::new();
    env.insert("KNOWN".to_string(), "value".to_string());
    env.insert("RECURSIVE".to_string(), "%KNOWN%".to_string());
    env.insert("EMPTY".to_string(), String::new());
    let out = replace_html_env(html, &env);

    // Exactly the regex's answer: one left-to-right pass, placeholders
    // consumed whole, substituted values never rescanned (RECURSIVE's value
    // would turn into "value" if they were).
    assert_eq!(
        out,
        vite_reference(html, &env),
        "diverged from the regex on {html:?}"
    );

    // Substitution is a fixed point of itself once the placeholders are gone.
    let twice = replace_html_env(&out, &env);
    if !out.contains("%KNOWN%") && !out.contains("%RECURSIVE%") && !out.contains("%EMPTY%") {
        assert_eq!(twice, out, "not idempotent");
    }
});
