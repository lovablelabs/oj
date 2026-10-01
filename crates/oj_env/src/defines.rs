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
