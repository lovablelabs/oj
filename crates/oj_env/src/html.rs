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
pub fn replace_html_env(html: &str, env: &BTreeMap<String, String>) -> String {
    if env.is_empty() || !html.contains('%') {
        return html.to_string();
    }
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    loop {
        match rest.find('%') {
            Some(start) => {
                out.push_str(&rest[..start]);
                let after = &rest[start + 1..];
                if let Some(end) = after.find('%') {
                    let key = &after[..end];
                    if !key.is_empty() && !key.chars().any(char::is_whitespace) {
                        if let Some(val) = env.get(key) {
                            out.push_str(val);
                            rest = &after[end + 1..];
                            continue;
                        }
                    }
                }
                out.push('%');
                rest = after;
            }
            None => {
                out.push_str(rest);
                break;
            }
        }
    }
    out
}
