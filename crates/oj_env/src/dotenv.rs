//! `.env` files, read the way Vite's loadEnv does (dotenv + dotenv-expand).

use std::collections::BTreeMap;
use std::path::Path;

use crate::Env;

pub fn parse(contents: &str, base: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let mut acc = base.clone();
    let mut out = Vec::new();
    for raw in contents.lines() {
        let line = raw.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, rest)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = parse_value(rest.trim(), &acc);
        acc.insert(key.to_string(), value.clone());
        out.push((key.to_string(), value));
    }
    out
}

fn parse_value(raw: &str, vars: &BTreeMap<String, String>) -> String {
    let bytes = raw.as_bytes();
    if bytes.first() == Some(&b'\'') {
        let inner = &raw[1..];
        return inner
            .split_once('\'')
            .map(|(v, _)| v)
            .unwrap_or(inner)
            .to_string();
    }
    if bytes.first() == Some(&b'"') {
        let inner = &raw[1..];
        let inner = inner.split_once('"').map(|(v, _)| v).unwrap_or(inner);
        let unescaped = inner
            .replace("\\n", "\n")
            .replace("\\t", "\t")
            .replace("\\\"", "\"");
        return expand(&unescaped, vars);
    }
    let end = raw.find(" #").unwrap_or(raw.len());
    expand(raw[..end].trim(), vars)
}

fn expand(input: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\\' && bytes.get(i + 1) == Some(&b'$') {
            out.push('$');
            i += 2;
            continue;
        }
        if c == b'$' {
            let reference: Option<(&str, usize)> = if bytes.get(i + 1) == Some(&b'{') {
                input[i + 2..]
                    .find('}')
                    .map(|rel| (&input[i + 2..i + 2 + rel], i + 2 + rel + 1))
                    .filter(|(name, _)| !name.is_empty())
            } else {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
                {
                    end += 1;
                }
                (end > start).then(|| (&input[start..end], end))
            };
            if let Some((name, next)) = reference {
                out.push_str(vars.get(name).map(String::as_str).unwrap_or(""));
                i = next;
                continue;
            }
        }
        let ch = input[i..]
            .chars()
            .next()
            .expect("loop only advances to char boundaries");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// `.env`, `.env.local`, `.env.[mode]`, `.env.[mode].local` in `dir`, later
/// files winning, each expanded against `env` and the files before it.
pub fn load_with(env: &Env, dir: &Path, mode: &str) -> Vec<(String, String)> {
    let mut base: BTreeMap<String, String> = env
        .vars()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let mut merged: BTreeMap<String, String> = BTreeMap::new();
    for name in [
        ".env".to_string(),
        ".env.local".to_string(),
        format!(".env.{mode}"),
        format!(".env.{mode}.local"),
    ] {
        let Ok(contents) = std::fs::read_to_string(dir.join(name)) else {
            continue;
        };
        for (k, v) in parse(&contents, &base) {
            base.insert(k.clone(), v.clone());
            merged.insert(k, v);
        }
    }
    merged.into_iter().collect()
}

/// [`load_with`] against the process snapshot.
pub fn load(dir: &Path, mode: &str) -> Vec<(String, String)> {
    load_with(crate::get(), dir, mode)
}
