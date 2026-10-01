use crate::schema::*;
use std::path::{Path, PathBuf};

fn define_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
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

pub fn server_strict_port(config: &OjConfig) -> bool {
    config
        .server
        .as_ref()
        .and_then(|s| s.strict_port)
        .unwrap_or(false)
}

pub fn env_prefixes(config: &OjConfig) -> Vec<String> {
    config
        .env_prefix
        .as_ref()
        .map(|p| p.to_vec())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec!["VITE_".to_string()])
}

pub fn server_fs_deny(config: &OjConfig) -> Vec<String> {
    config
        .server
        .as_ref()
        .and_then(|s| s.fs.as_ref())
        .and_then(|f| f.deny.as_ref())
        .cloned()
        .unwrap_or_default()
}

pub fn config_defines(config: &OjConfig) -> Vec<(String, String)> {
    config
        .define
        .as_ref()
        .map(|d| {
            d.iter()
                .map(|(k, v)| (k.clone(), define_value(v)))
                .collect()
        })
        .unwrap_or_default()
}

/// `server.warmup.clientFiles` / `server.warmup.ssrFiles`: modules to compile
/// eagerly at startup so their first request is already warm.
pub fn server_warmup_files(config: &OjConfig) -> (Vec<String>, Vec<String>) {
    let w = config.server.as_ref().and_then(|s| s.warmup.as_ref());
    let take = |f: Option<&Vec<String>>| f.cloned().unwrap_or_default();
    (
        take(w.and_then(|w| w.client_files.as_ref())),
        take(w.and_then(|w| w.ssr_files.as_ref())),
    )
}

pub fn environment_defines(config: &OjConfig, env_name: &str) -> Vec<(String, String)> {
    config
        .environments
        .as_ref()
        .and_then(|envs| envs.get(env_name))
        .and_then(|env| env.get("define"))
        .and_then(|d| d.as_object())
        .map(|d| {
            d.iter()
                .map(|(k, v)| (k.clone(), define_value(v)))
                .collect()
        })
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
