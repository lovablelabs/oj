use crate::schema::*;
use std::path::{Path, PathBuf};

/// `define` entries as `(key, replacement)`: strings verbatim, anything else
/// as its JSON text.
fn defines_of<'a>(
    entries: impl Iterator<Item = (&'a String, &'a serde_json::Value)>,
) -> Vec<(String, String)> {
    entries
        .map(|(k, v)| {
            let value = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            (k.clone(), value)
        })
        .collect()
}

/// `html.cspNonce`, when set and non-empty.
pub fn html_csp_nonce(config: &OjConfig) -> Option<String> {
    config
        .html
        .as_ref()
        .and_then(|h| h.csp_nonce.clone())
        .filter(|n| !n.is_empty())
}

/// The public directory, absolute: Vite's `publicDir` (default `<root>/public`),
/// or None when the config sets `publicDir: false`.
pub fn public_dir(config: &OjConfig, root: &Path) -> Option<PathBuf> {
    match config.public_dir.as_ref() {
        Some(BoolOrString::Bool(false)) => None,
        Some(BoolOrString::Str(s)) if !s.is_empty() => Some(root.join(s)),
        _ => Some(root.join("public")),
    }
}

/// Vite's `cacheDir`, absolute (resolveConfig in config.ts): the config's
/// value resolved against the root; else `node_modules/.vite` under the
/// nearest directory at or above the root holding a package.json (Vite's
/// findNearestPackageData, so a root inside a package caches above itself);
/// else, with no manifest anywhere, under the root's node_modules when it
/// has one and `<root>/.vite` otherwise. An empty string is unset, as Vite's
/// `if (config.cacheDir)` reads it.
pub fn cache_dir(config: &OjConfig, root: &Path) -> PathBuf {
    if let Some(dir) = config.cache_dir.as_deref().filter(|d| !d.is_empty()) {
        return root.join(dir);
    }
    match root.ancestors().find(|d| d.join("package.json").is_file()) {
        Some(pkg) => pkg.join("node_modules/.vite"),
        None if root.join("node_modules").is_dir() => root.join("node_modules/.vite"),
        None => root.join(".vite"),
    }
}

/// `server.strictPort` (Vite default false: try the next free port).
pub fn server_strict_port(config: &OjConfig) -> bool {
    config
        .server
        .as_ref()
        .and_then(|s| s.strict_port)
        .unwrap_or(false)
}

/// `envPrefix` as a list; unset or empty means Vite's `VITE_`.
pub fn env_prefixes(config: &OjConfig) -> Vec<String> {
    config
        .env_prefix
        .as_ref()
        .map(|p| p.to_vec())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec!["VITE_".to_string()])
}

/// `server.fs.deny` as written (Vite's defaults are applied by the caller).
pub fn server_fs_deny(config: &OjConfig) -> Vec<String> {
    config
        .server
        .as_ref()
        .and_then(|s| s.fs.as_ref())
        .and_then(|f| f.deny.as_ref())
        .cloned()
        .unwrap_or_default()
}

/// Top-level `define`.
pub fn config_defines(config: &OjConfig) -> Vec<(String, String)> {
    config
        .define
        .as_ref()
        .map(|d| defines_of(d.iter()))
        .unwrap_or_default()
}

/// `server.warmup.clientFiles` / `server.warmup.ssrFiles`: modules compiled
/// eagerly at startup.
pub fn server_warmup_files(config: &OjConfig) -> (Vec<String>, Vec<String>) {
    let w = config.server.as_ref().and_then(|s| s.warmup.as_ref());
    let take = |f: Option<&Vec<String>>| f.cloned().unwrap_or_default();
    (
        take(w.and_then(|w| w.client_files.as_ref())),
        take(w.and_then(|w| w.ssr_files.as_ref())),
    )
}

/// `environments.<env_name>.define`.
pub fn environment_defines(config: &OjConfig, env_name: &str) -> Vec<(String, String)> {
    config
        .environments
        .as_ref()
        .and_then(|envs| envs.get(env_name))
        .and_then(|env| env.get("define"))
        .and_then(|d| d.as_object())
        .map(|d| defines_of(d.iter()))
        .unwrap_or_default()
}

#[cfg(test)]
mod proxy_secure_tests {
    use super::*;

    #[test]
    fn proxy_secure_defaults_on_and_reads_false() {
        let cfg: OjConfig = serde_json::from_str(
            r#"{"server":{"proxy":{
            "/a": "https://a.test",
            "/b": { "target": "https://b.test", "ws": true },
            "/c": { "target": "https://c.test", "secure": false }
        }}}"#,
        )
        .unwrap();
        let proxy = cfg.server.unwrap().proxy.unwrap();
        assert!(proxy["/a"].secure());
        assert!(proxy["/b"].secure());
        assert!(!proxy["/c"].secure());
    }
}

#[cfg(test)]
mod public_dir_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    use crate::build::{build_css_minify, build_css_targets, build_targets};
    use crate::css::{css_modules, CssModulesSettings};

    #[test]
    fn css_target_and_minify_default_to_the_js_settings() {
        let none: OjConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(build_css_targets(&none), build_targets(&none));
        assert!(build_css_minify(&none, false));
        let js_off: OjConfig = serde_json::from_str(r#"{"build":{"minify":false}}"#).unwrap();
        assert!(
            !build_css_minify(&js_off, false),
            "cssMinify follows build.minify"
        );
        assert!(
            build_css_minify(&js_off, true),
            "server builds minify CSS by default"
        );
        let explicit: OjConfig =
            serde_json::from_str(r#"{"build":{"minify":false,"cssMinify":"lightningcss","cssTarget":["chrome120","modules"]}}"#).unwrap();
        assert!(build_css_minify(&explicit, false));
        let t = build_css_targets(&explicit);
        assert_eq!(t[0], "chrome120");
        assert!(
            t.contains(&"safari14".to_string()),
            "modules preset expands: {t:?}"
        );
        let off: OjConfig = serde_json::from_str(r#"{"build":{"cssMinify":false}}"#).unwrap();
        assert!(!build_css_minify(&off, false));
    }

    #[test]
    fn css_modules_settings_read_strings_and_regex_markers() {
        let cfg: OjConfig = serde_json::from_str(
            r#"{"css":{"modules":{
                "localsConvention":"camelCaseOnly",
                "generateScopedName":"__oj_fn__",
                "scopeBehaviour":"global",
                "globalModulePaths":[{"__oj_regex__":"global\\.css$"},"legacy"],
                "getJSON":"__oj_fn__"
            }}}"#,
        )
        .unwrap();
        let m = css_modules(&cfg);
        assert_eq!(m.locals_convention.as_deref(), Some("camelCaseOnly"));
        assert_eq!(m.generate_scoped_name, None, "function form is dropped");
        assert!(m.global_scope);
        assert_eq!(
            m.global_module_paths,
            vec!["global\\.css$".to_string(), "legacy".to_string()]
        );
        assert_eq!(
            css_modules(&serde_json::from_str("{}").unwrap()),
            CssModulesSettings::default()
        );
    }

    #[test]
    fn public_dir_reads_path_default_and_false() {
        let root = Path::new("/app");
        let none: OjConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(public_dir(&none, root), Some(PathBuf::from("/app/public")));
        let custom: OjConfig = serde_json::from_str(r#"{"publicDir":"static"}"#).unwrap();
        assert_eq!(
            public_dir(&custom, root),
            Some(PathBuf::from("/app/static"))
        );
        let off: OjConfig = serde_json::from_str(r#"{"publicDir":false}"#).unwrap();
        assert_eq!(public_dir(&off, root), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_prefix_accepts_string_or_array_and_defaults() {
        // A single string stays a one-element list (back-compat).
        let one: OjConfig = serde_json::from_str(r#"{"envPrefix":"PUBLIC_"}"#).unwrap();
        assert_eq!(env_prefixes(&one), vec!["PUBLIC_".to_string()]);
        // An array exposes every listed prefix.
        let many: OjConfig = serde_json::from_str(r#"{"envPrefix":["VITE_","PUBLIC_"]}"#).unwrap();
        assert_eq!(
            env_prefixes(&many),
            vec!["VITE_".to_string(), "PUBLIC_".to_string()]
        );
        // Absent (or empty) falls back to Vite's default VITE_.
        let none: OjConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(env_prefixes(&none), vec!["VITE_".to_string()]);
        let empty: OjConfig = serde_json::from_str(r#"{"envPrefix":[]}"#).unwrap();
        assert_eq!(env_prefixes(&empty), vec!["VITE_".to_string()]);
    }

    #[test]
    fn server_fs_deny_accessor() {
        let json = r#"{"server":{"fs":{"deny":["secrets/**","*.key"]}}}"#;
        let cfg: OjConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            server_fs_deny(&cfg),
            vec!["secrets/**".to_string(), "*.key".to_string()]
        );
        // Absent server / fs / deny is an empty list (defaults applied elsewhere).
        let empty: OjConfig = serde_json::from_str("{}").unwrap();
        assert!(server_fs_deny(&empty).is_empty());
    }

    #[test]
    fn server_strict_port_accessor_defaults_false() {
        let on: OjConfig = serde_json::from_str(r#"{"server":{"strictPort":true}}"#).unwrap();
        assert!(server_strict_port(&on));
        // Vite's default is false (auto-increment) when unset.
        let off: OjConfig = serde_json::from_str(r#"{"server":{"port":3000}}"#).unwrap();
        assert!(!server_strict_port(&off));
        let empty: OjConfig = serde_json::from_str("{}").unwrap();
        assert!(!server_strict_port(&empty));
    }
}

#[cfg(test)]
mod cache_dir_tests {
    use super::*;

    fn config(json: &str) -> OjConfig {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn the_configs_cache_dir_resolves_against_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("app");
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        assert_eq!(
            cache_dir(&config(r#"{"cacheDir":".cache/vite"}"#), &root),
            root.join(".cache/vite")
        );
        assert_eq!(
            cache_dir(&config(r#"{"cacheDir":"/abs/cache"}"#), &root),
            PathBuf::from("/abs/cache")
        );
        assert_eq!(
            cache_dir(&config(r#"{"cacheDir":""}"#), &root),
            root.join("node_modules/.vite"),
            "empty is unset"
        );
    }

    #[test]
    fn the_default_sits_under_the_nearest_package_json() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("pkg/app");
        std::fs::create_dir_all(&root).unwrap();
        let none = config("{}");
        assert_eq!(
            cache_dir(&none, &root),
            root.join(".vite"),
            "no manifest, no node_modules"
        );
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        assert_eq!(
            cache_dir(&none, &root),
            root.join("node_modules/.vite"),
            "no manifest, a node_modules dir in the root"
        );
        std::fs::write(dir.path().join("pkg/package.json"), "{}").unwrap();
        assert_eq!(
            cache_dir(&none, &root),
            dir.path().join("pkg/node_modules/.vite"),
            "the manifest above the root wins over the root's node_modules"
        );
        std::fs::write(root.join("package.json"), "{}").unwrap();
        assert_eq!(
            cache_dir(&none, &root),
            root.join("node_modules/.vite"),
            "the root's own manifest is the nearest"
        );
    }
}
