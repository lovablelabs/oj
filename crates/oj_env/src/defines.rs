//! `import.meta.env` defines and Vite's NODE_ENV rule.

use std::collections::BTreeMap;

pub(crate) fn has_prefix(key: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|p| key.starts_with(p))
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

/// The define list as a bundler or compiler takes it: one entry per key, the
/// last occurrence winning (later layers override earlier ones, as in Vite's
/// define merge; the oxc replacer refuses a duplicate key outright, which
/// silently disables every define), and the `import.meta.env` object rebuilt
/// from every `import.meta.env.<KEY>` entry of that final list. Vite folds
/// the user's `import.meta.env.*` defines into the object it replaces
/// `import.meta.env` with, in the build (plugins/define.ts `definePlugin`:
/// `serializeDefine({ ...importMetaEnvKeys, SSR, ...userDefineEnv })`) and in
/// dev (plugins/importAnalysis.ts `getEnv`), so `import.meta.env["VITE_X"]`,
/// `const env = import.meta.env` and `{ ...import.meta.env }` see a key the
/// config's `define` adds, not only `import.meta.env.VITE_X`. The object's
/// keys are sorted as `serializeDefine` sorts them; the values are the JS
/// expressions the defines carry, written as they are. A list without an
/// `import.meta.env` entry gets none.
pub fn fold_env_keys_into_object(defines: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut index: std::collections::HashMap<String, usize> =
        std::collections::HashMap::with_capacity(defines.len());
    let mut out: Vec<(String, String)> = Vec::with_capacity(defines.len());
    for (k, v) in defines {
        match index.get(&k) {
            Some(&i) => out[i].1 = v,
            None => {
                index.insert(k.clone(), out.len());
                out.push((k, v));
            }
        }
    }
    let Some(&object) = index.get("import.meta.env") else {
        return out;
    };
    let keys: BTreeMap<&str, &str> = out
        .iter()
        .filter_map(|(k, v)| Some((k.strip_prefix("import.meta.env.")?, v.as_str())))
        .collect();
    let mut text = String::from("{");
    for (i, (k, v)) in keys.iter().enumerate() {
        if i > 0 {
            text.push(',');
        }
        text.push_str(&serde_json::Value::String((*k).to_string()).to_string());
        text.push(':');
        text.push_str(v);
    }
    text.push('}');
    out[object].1 = text;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'a>(defines: &'a [(String, String)], key: &str) -> &'a str {
        &defines.iter().find(|(k, _)| k == key).unwrap().1
    }

    #[test]
    fn a_user_define_joins_the_object() {
        let loaded = vec![("VITE_A".to_string(), "a".to_string())];
        let mut defines = import_meta_env_defines(&loaded, "production", false, "/", &["VITE_"]);
        defines.push((
            "import.meta.env.VITE_API_URL".into(),
            "\"https://api.example\"".into(),
        ));
        defines.push(("__APP__".into(), "\"app\"".into()));
        let folded = fold_env_keys_into_object(defines);
        assert_eq!(
            get(&folded, "import.meta.env"),
            r#"{"BASE_URL":"/","DEV":false,"MODE":"production","PROD":true,"SSR":false,"VITE_A":"a","VITE_API_URL":"https://api.example"}"#
        );
        assert_eq!(
            get(&folded, "import.meta.env.VITE_API_URL"),
            "\"https://api.example\""
        );
        assert_eq!(
            get(&folded, "__APP__"),
            "\"app\"",
            "a non-env define is kept as it is"
        );
    }

    #[test]
    fn a_later_layer_wins_in_the_object_and_the_key() {
        let loaded = vec![("VITE_A".to_string(), "file".to_string())];
        let mut defines = import_meta_env_defines(&loaded, "development", true, "/", &["VITE_"]);
        defines.push(("import.meta.env.VITE_A".into(), "\"config\"".into()));
        defines.push(("import.meta.env.VITE_A".into(), "\"plugin\"".into()));
        let folded = fold_env_keys_into_object(defines);
        assert_eq!(
            folded
                .iter()
                .filter(|(k, _)| k == "import.meta.env.VITE_A")
                .count(),
            1,
            "one entry per key"
        );
        assert_eq!(get(&folded, "import.meta.env.VITE_A"), "\"plugin\"");
        assert!(
            get(&folded, "import.meta.env").contains(r#""VITE_A":"plugin""#),
            "{}",
            get(&folded, "import.meta.env")
        );
    }

    #[test]
    fn the_ssr_object_keeps_its_flag_and_an_expression_value_is_written_as_is() {
        let defines = vec![
            ("import.meta.env.SSR".to_string(), "true".to_string()),
            ("import.meta.env".to_string(), r#"{"SSR":true}"#.to_string()),
            (
                "import.meta.env.VITE_HOST".to_string(),
                "window.location.host".to_string(),
            ),
        ];
        let folded = fold_env_keys_into_object(defines);
        assert_eq!(
            get(&folded, "import.meta.env"),
            r#"{"SSR":true,"VITE_HOST":window.location.host}"#
        );
    }

    #[test]
    fn a_list_without_the_object_only_dedups() {
        let defines = vec![
            ("__A__".to_string(), "1".to_string()),
            ("import.meta.env.VITE_X".to_string(), "\"x\"".to_string()),
            ("__A__".to_string(), "2".to_string()),
        ];
        assert_eq!(
            fold_env_keys_into_object(defines),
            vec![
                ("__A__".to_string(), "2".to_string()),
                ("import.meta.env.VITE_X".to_string(), "\"x\"".to_string()),
            ]
        );
    }
}
