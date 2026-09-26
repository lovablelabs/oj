// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::collections::BTreeMap;
use std::path::Path;

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

pub fn load(dir: &Path, mode: &str) -> Vec<(String, String)> {
    let mut base: BTreeMap<String, String> = std::env::vars().collect();
    let mut merged: BTreeMap<String, String> = BTreeMap::new();
    for name in [
        ".env",
        ".env.local",
        &format!(".env.{mode}"),
        &format!(".env.{mode}.local"),
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

/// Vite parity: the actual process environment (and any plugin `config()` env
/// mutations layered on top of it) wins over `.env` files for prefixed vars.
pub fn with_process_env(
    loaded: Vec<(String, String)>,
    process_env: impl IntoIterator<Item = (String, String)>,
    prefixes: &[&str],
) -> Vec<(String, String)> {
    let mut map: BTreeMap<String, String> = loaded.into_iter().collect();
    for (k, v) in process_env {
        if prefixes.iter().any(|p| k.starts_with(p)) {
            map.insert(k, v);
        }
    }
    map.into_iter().collect()
}

/// Vite's NODE_ENV rule (config.ts): the shell's `NODE_ENV` wins when set;
/// otherwise a `NODE_ENV=development` in a loaded `.env` file makes this a
/// development build (`vite build --mode development` with `.env.development`
/// carrying it), any other `.env` value is ignored with a warning as Vite does;
/// otherwise the command's default (`production` for build, `development` for
/// serve). `import.meta.env.DEV`/`PROD` and `process.env.NODE_ENV` follow it.
pub fn resolve_node_env(shell: Option<&str>, loaded: &[(String, String)], default: &str) -> String {
    if let Some(v) = shell.filter(|v| !v.is_empty()) {
        return v.to_string();
    }
    if let Some((_, v)) = loaded.iter().find(|(k, _)| k == "NODE_ENV") {
        if v == "development" {
            return v.clone();
        }
        // The dev server recomputes its defines after the plugin host boots; warn once.
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!(
                "oj: NODE_ENV={v} is not supported in the .env file. Only NODE_ENV=development is supported to create a development build of your project."
            );
        }
    }
    default.to_string()
}

pub fn import_meta_env_defines(
    loaded: &[(String, String)],
    mode: &str,
    dev: bool,
    base_url: &str,
    prefixes: &[&str],
) -> Vec<(String, String)> {
    import_meta_env_defines_with(loaded, mode, dev, base_url, prefixes, false)
}

/// `import_meta_env_defines` for a chosen environment: `ssr` sets
/// `import.meta.env.SSR` (Vite defines the same object for the ssr environment
/// with `SSR: true`).
pub fn import_meta_env_defines_with(
    loaded: &[(String, String)],
    mode: &str,
    dev: bool,
    base_url: &str,
    prefixes: &[&str],
    ssr: bool,
) -> Vec<(String, String)> {
    let mut obj = serde_json::Map::new();
    obj.insert("MODE".into(), mode.into());
    obj.insert("BASE_URL".into(), base_url.into());
    obj.insert("DEV".into(), dev.into());
    obj.insert("PROD".into(), (!dev).into());
    obj.insert("SSR".into(), ssr.into());
    for (k, v) in loaded {
        if prefixes.iter().any(|p| k.starts_with(p)) {
            obj.insert(k.clone(), serde_json::Value::String(v.clone()));
        }
    }

    let mut defines = Vec::new();
    for (k, v) in &obj {
        defines.push((format!("import.meta.env.{k}"), v.to_string()));
    }
    defines.push((
        "import.meta.env".into(),
        serde_json::Value::Object(obj).to_string(),
    ));
    defines
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssr_variant_flips_only_the_ssr_flag() {
        let loaded = vec![("VITE_X".to_string(), "1".to_string())];
        let ssr =
            import_meta_env_defines_with(&loaded, "staging", false, "/app/", &["VITE_"], true);
        let get = |k: &str| {
            ssr.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(get("import.meta.env.SSR"), "true");
        assert_eq!(get("import.meta.env.PROD"), "true");
        assert_eq!(get("import.meta.env.MODE"), "\"staging\"");
        assert_eq!(get("import.meta.env.BASE_URL"), "\"/app/\"");
        assert_eq!(get("import.meta.env.VITE_X"), "\"1\"");
        assert!(get("import.meta.env").contains("\"SSR\":true"));
        let client = import_meta_env_defines(&loaded, "staging", false, "/app/", &["VITE_"]);
        assert!(client
            .iter()
            .any(|(k, v)| k == "import.meta.env.SSR" && v == "false"));
    }

    #[test]
    fn node_env_shell_wins_then_dotenv_development_then_default() {
        let dev_file = vec![("NODE_ENV".to_string(), "development".to_string())];
        let prod_file = vec![("NODE_ENV".to_string(), "production".to_string())];
        assert_eq!(
            resolve_node_env(Some("production"), &dev_file, "production"),
            "production"
        );
        assert_eq!(resolve_node_env(Some("test"), &[], "production"), "test");
        assert_eq!(
            resolve_node_env(Some(""), &dev_file, "production"),
            "development",
            "empty shell value is unset"
        );
        assert_eq!(
            resolve_node_env(None, &dev_file, "production"),
            "development"
        );
        assert_eq!(
            resolve_node_env(None, &prod_file, "development"),
            "development",
            "only development flips"
        );
        assert_eq!(resolve_node_env(None, &[], "production"), "production");
        assert_eq!(resolve_node_env(None, &[], "development"), "development");
    }

    fn base() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    #[test]
    fn parses_basic_quotes_and_comments() {
        let src = "# comment\nexport A=1\nB=\"two words\"\nC='literal $A'\nD=trailing # note\n";
        let v = parse(src, &base());
        assert_eq!(
            v,
            vec![
                ("A".into(), "1".into()),
                ("B".into(), "two words".into()),
                ("C".into(), "literal $A".into()),
                ("D".into(), "trailing".into()),
            ]
        );
    }

    #[test]
    fn html_env_replaces_known_keys_only() {
        let defines = import_meta_env_defines(
            &[("VITE_TITLE".into(), "My App".into())],
            "development",
            true,
            "/",
            &["VITE_"],
        );
        let env = html_env_map(&defines);
        let html =
            "<title>%VITE_TITLE%</title><meta content=\"%MODE%\"><b>%VITE_MISSING%</b> 50%% off";
        let out = replace_html_env(html, &env);
        assert!(out.contains("<title>My App</title>"), "{out}");
        assert!(out.contains("content=\"development\""), "{out}");
        assert!(
            out.contains("%VITE_MISSING%"),
            "unknown key left as-is: {out}"
        );
        assert!(out.contains("50%% off"), "bare percents untouched: {out}");
    }

    #[test]
    fn expands_prior_vars_but_not_in_single_quotes() {
        let v = parse(
            "HOST=example.com\nURL=https://${HOST}/api\nRAW='${HOST}'\n",
            &base(),
        );
        assert_eq!(v[1], ("URL".into(), "https://example.com/api".into()));
        assert_eq!(v[2], ("RAW".into(), "${HOST}".into()));
    }

    #[test]
    fn escaped_dollar_is_literal() {
        let v = parse("PRICE=\"\\$5\"\n", &base());
        assert_eq!(v[0], ("PRICE".into(), "$5".into()));
    }

    #[test]
    fn defines_include_builtins_and_only_prefixed_vars() {
        let loaded = vec![
            ("VITE_API".into(), "https://api.test".into()),
            ("SECRET".into(), "nope".into()),
        ];
        let d = import_meta_env_defines(&loaded, "development", true, "/", &["VITE_"]);
        let map: std::collections::HashMap<_, _> = d.iter().cloned().collect();
        assert_eq!(map["import.meta.env.MODE"], "\"development\"");
        assert_eq!(map["import.meta.env.DEV"], "true");
        assert_eq!(map["import.meta.env.PROD"], "false");
        assert_eq!(map["import.meta.env.VITE_API"], "\"https://api.test\"");
        assert!(
            !map.contains_key("import.meta.env.SECRET"),
            "unprefixed var must not leak"
        );
        assert!(map["import.meta.env"].contains("VITE_API"));
        assert!(!map["import.meta.env"].contains("SECRET"));
    }

    #[test]
    fn expansion_boundary_undefined_base_and_unclosed_brace() {
        let mut b = base();
        b.insert("FROM_ENV".into(), "envval".into());
        let src = "A=first\n\
                   UNBRACED=$A/x\n\
                   MISSING=[$NOPE]\n\
                   FROMBASE=${FROM_ENV}\n\
                   UNCLOSED=${OOPS\n";
        let map: std::collections::HashMap<_, _> = parse(src, &b).into_iter().collect();
        assert_eq!(map["UNBRACED"], "first/x");
        assert_eq!(map["MISSING"], "[]");
        assert_eq!(map["FROMBASE"], "envval");
        assert_eq!(map["UNCLOSED"], "${OOPS");
    }

    #[test]
    fn process_env_wins_over_files_for_prefixed_vars() {
        let loaded = vec![
            ("VITE_A".into(), "file-a".into()),
            ("VITE_B".into(), "file-b".into()),
        ];
        let process_env = vec![
            ("VITE_A".into(), "proc-a".into()),
            ("VITE_C".into(), "proc-c".into()),
            ("SECRET".into(), "nope".into()),
        ];
        let merged = with_process_env(loaded, process_env, &["VITE_"]);
        let map: std::collections::HashMap<_, _> = merged.into_iter().collect();
        assert_eq!(map["VITE_A"], "proc-a", "process env wins over file value");
        assert_eq!(map["VITE_B"], "file-b", "file-only var survives");
        assert_eq!(
            map["VITE_C"], "proc-c",
            "process-only prefixed var is added"
        );
        assert!(
            !map.contains_key("SECRET"),
            "unprefixed process var excluded"
        );
    }

    #[test]
    fn process_env_overlay_flows_into_defines() {
        let merged = with_process_env(
            vec![("VITE_FLAG".into(), "off".into())],
            vec![("VITE_FLAG".into(), "true".into())],
            &["VITE_"],
        );
        let d = import_meta_env_defines(&merged, "development", true, "/", &["VITE_"]);
        let map: std::collections::HashMap<_, _> = d.iter().cloned().collect();
        assert_eq!(map["import.meta.env.VITE_FLAG"], "\"true\"");
        assert!(map["import.meta.env"].contains("\"VITE_FLAG\":\"true\""));
    }

    #[test]
    fn empty_process_env_is_a_no_op() {
        let loaded = vec![("VITE_A".into(), "file-a".into())];
        let merged = with_process_env(loaded.clone(), Vec::new(), &["VITE_"]);
        assert_eq!(merged, loaded);
    }

    #[test]
    fn missing_files_yield_empty() {
        let d = std::env::temp_dir().join("oj-env-none");
        assert!(load(&d, "development").is_empty());
    }
}
