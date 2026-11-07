//! `%KEY%` env placeholders in index.html (Vite's htmlEnvHook).

use std::collections::BTreeMap;

/// The `%KEY%` substitution map for index.html, derived from the import.meta.env
/// defines (Vite's htmlEnvHook: env = loadEnv + import.meta.env.* defines, with
/// string values unwrapped from their JSON literal).
pub fn html_env_map(defines: &[(String, String)]) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for (k, v) in defines {
        if let Some(name) = k.strip_prefix("import.meta.env.") {
            let raw = match serde_json::from_str::<serde_json::Value>(v) {
                Ok(serde_json::Value::String(s)) => s,
                Ok(other) => other.to_string(),
                Err(_) => v.clone(),
            };
            env.insert(name.to_string(), raw);
        }
    }
    env
}

/// Replace `%KEY%` placeholders in HTML with env values (Vite parity: regex
/// `/%(\S+?)%/g`, only keys present in the map; unknown placeholders are left).
///
/// The scan mirrors the regex exactly: non-greedy, so the first `%` at least
/// two characters past the opener closes the match (a `%` straight after the
/// opener is key content, since `\S+?` must consume something); whitespace
/// before a close fails the match at this opener and the engine retries from
/// the next character; a matched placeholder is consumed whole whether or not
/// its key is in the map, so its closing `%` never opens the next match.
pub fn replace_html_env(html: &str, env: &BTreeMap<String, String>) -> String {
    if env.is_empty() || !html.contains('%') {
        return html.to_string();
    }
    let mut out = String::with_capacity(html.len());
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
            if ch != '%' && js_whitespace(ch) {
                break;
            }
        }
        let Some(close) = close else {
            out.push('%');
            rest = &rest[1..];
            continue;
        };
        let key = &rest[1..close];
        match env.get(key) {
            Some(val) => out.push_str(val),
            None => out.push_str(&rest[..close + 1]),
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

/// JavaScript's `\s` class (the regex runs without the `u` flag, but `\s` is
/// the same set either way): Rust's `char::is_whitespace` differs on U+0085
/// (White_Space, not JS `\s`) and U+FEFF (JS `\s`, not White_Space).
fn js_whitespace(c: char) -> bool {
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
