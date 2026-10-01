use crate::schema::*;

/// `css.modules` reduced to what oj applies (strings only: a function-valued
/// `localsConvention` / `generateScopedName` cannot cross from the config
/// and is reported by the extractor).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CssModulesSettings {
    pub locals_convention: Option<String>,
    pub generate_scoped_name: Option<String>,
    pub global_scope: bool,
    pub global_module_paths: Vec<String>,
}

pub fn css_modules(config: &OjConfig) -> CssModulesSettings {
    let Some(m) = config.css.as_ref().and_then(|c| c.modules.as_ref()) else {
        return CssModulesSettings::default();
    };
    let str_of = |v: &Option<serde_json::Value>| {
        v.as_ref()
            .and_then(|v| v.as_str())
            .filter(|s| *s != "__oj_fn__")
            .map(str::to_string)
    };
    CssModulesSettings {
        locals_convention: str_of(&m.locals_convention),
        generate_scoped_name: str_of(&m.generate_scoped_name),
        global_scope: m.scope_behaviour.as_deref() == Some("global"),
        global_module_paths: m
            .global_module_paths
            .iter()
            .flatten()
            .filter_map(|v| match v {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Object(o) => o
                    .get("__oj_regex__")
                    .and_then(|r| r.as_str())
                    .map(str::to_string),
                _ => None,
            })
            .collect(),
    }
}

pub fn css_additional_data(config: &OjConfig, lang: &str) -> Option<String> {
    config
        .css
        .as_ref()
        .and_then(|c| c.preprocessor_options.as_ref())
        .and_then(|m| m.get(lang))
        .and_then(|e| e.additional_data.clone())
}

/// `css.preprocessorOptions.<lang>` minus `additionalData`, as JSON for the
/// preprocessor (Less/Stylus run in a node sidecar and take the object as-is).
pub fn css_preprocessor_json(config: &OjConfig, lang: &str) -> serde_json::Value {
    config
        .css
        .as_ref()
        .and_then(|c| c.preprocessor_options.as_ref())
        .and_then(|m| m.get(lang))
        .map(|e| {
            serde_json::Value::Object(e.rest.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        })
        .unwrap_or(serde_json::Value::Null)
}

/// Sass `loadPaths` (Vite 5+) and the legacy `includePaths`, in order.
pub fn css_load_paths(config: &OjConfig, lang: &str) -> Vec<String> {
    let entry = config
        .css
        .as_ref()
        .and_then(|c| c.preprocessor_options.as_ref())
        .and_then(|m| m.get(lang));
    let mut out = Vec::new();
    if let Some(e) = entry {
        for key in ["loadPaths", "includePaths"] {
            if let Some(arr) = e.rest.get(key).and_then(|v| v.as_array()) {
                out.extend(arr.iter().filter_map(|v| v.as_str()).map(str::to_string));
            }
        }
    }
    out
}

#[cfg(test)]
mod preprocessor_options_tests {
    use super::*;

    #[test]
    fn preprocessor_options_keep_every_key_for_the_preprocessor() {
        let cfg: OjConfig = serde_json::from_str(
            r##"{"css":{"preprocessorOptions":{
                "scss":{"additionalData":"$x: 1;","loadPaths":["styles"],"includePaths":["legacy"]},
                "less":{"javascriptEnabled":true,"globalVars":{"brand":"#f00"},"paths":["less"]}}}}"##,
        )
        .unwrap();
        assert_eq!(css_additional_data(&cfg, "scss").as_deref(), Some("$x: 1;"));
        assert_eq!(
            css_load_paths(&cfg, "scss"),
            vec!["styles".to_string(), "legacy".to_string()]
        );
        let less = css_preprocessor_json(&cfg, "less");
        assert_eq!(less["javascriptEnabled"], true);
        assert_eq!(less["globalVars"]["brand"], "#f00");
        assert!(less.get("additionalData").is_none());
        assert!(css_preprocessor_json(&cfg, "stylus").is_null());
        assert!(css_load_paths(&cfg, "sass").is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css_additional_data_accessor_reads_per_language() {
        let json = r#"{"css":{"preprocessorOptions":{
            "scss":{"additionalData":"@use 'src/vars' as *;"},
            "sass":{"additionalData":"$x: 1"}}}}"#;
        let cfg: OjConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            css_additional_data(&cfg, "scss").as_deref(),
            Some("@use 'src/vars' as *;")
        );
        assert_eq!(css_additional_data(&cfg, "sass").as_deref(), Some("$x: 1"));
        // A language without an entry, and an absent css config, are both None.
        assert!(css_additional_data(&cfg, "less").is_none());
        let empty: OjConfig = serde_json::from_str("{}").unwrap();
        assert!(css_additional_data(&empty, "scss").is_none());
    }
}
