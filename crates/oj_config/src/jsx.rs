use crate::schema::*;

/// The JSX transform settings a config asks for, in Vite's precedence: `oxc.jsx`
/// (what `@vitejs/plugin-react` writes from `jsxRuntime`/`jsxImportSource`) first,
/// then the older `esbuild.jsx*` names (`jsx: "transform"` is the classic runtime,
/// `jsxFactory`/`jsxFragment` its pragmas). Unset fields mean oxc's defaults
/// (automatic runtime from `react`).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JsxSettings {
    pub runtime: Option<String>,
    pub import_source: Option<String>,
    pub pragma: Option<String>,
    pub pragma_frag: Option<String>,
}

pub fn jsx_settings(config: &OjConfig) -> JsxSettings {
    fn str_of(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
        obj.get(key).and_then(|v| v.as_str()).map(str::to_string)
    }
    let mut s = JsxSettings::default();
    if let Some(jsx) = config
        .oxc
        .as_ref()
        .and_then(|o| o.get("jsx"))
        .and_then(|j| j.as_object())
    {
        s.runtime = str_of(jsx, "runtime");
        s.import_source = str_of(jsx, "importSource");
        s.pragma = str_of(jsx, "pragma");
        s.pragma_frag = str_of(jsx, "pragmaFrag");
    }
    if let Some(es) = config.esbuild.as_ref().and_then(|e| e.as_object()) {
        let runtime = || match str_of(es, "jsx").as_deref() {
            Some("transform") => Some("classic".into()),
            Some("automatic") => Some("automatic".into()),
            _ => None,
        };
        s.runtime = s.runtime.or_else(runtime);
        s.import_source = s.import_source.or_else(|| str_of(es, "jsxImportSource"));
        s.pragma = s.pragma.or_else(|| str_of(es, "jsxFactory"));
        s.pragma_frag = s.pragma_frag.or_else(|| str_of(es, "jsxFragment"));
    }
    s
}

#[cfg(test)]
mod jsx_settings_tests {
    use super::*;

    use crate::resolve::resolve_conditions_for;

    #[test]
    fn oxc_jsx_wins_over_esbuild_and_esbuild_maps_its_names() {
        let mut c = OjConfig {
            esbuild: Some(
                serde_json::json!({ "jsx": "transform", "jsxImportSource": "preact", "jsxFactory": "h", "jsxFragment": "Fragment" }),
            ),
            ..Default::default()
        };
        let s = jsx_settings(&c);
        assert_eq!(s.runtime.as_deref(), Some("classic"));
        assert_eq!(s.import_source.as_deref(), Some("preact"));
        assert_eq!(s.pragma.as_deref(), Some("h"));
        assert_eq!(s.pragma_frag.as_deref(), Some("Fragment"));

        c.oxc = Some(
            serde_json::json!({ "jsx": { "runtime": "automatic", "importSource": "@emotion/react" } }),
        );
        let s = jsx_settings(&c);
        assert_eq!(s.runtime.as_deref(), Some("automatic"));
        assert_eq!(s.import_source.as_deref(), Some("@emotion/react"));
        assert_eq!(
            s.pragma.as_deref(),
            Some("h"),
            "esbuild fills what oxc left unset"
        );
    }

    #[test]
    fn oxc_false_and_missing_blocks_mean_defaults() {
        let mut c = OjConfig::default();
        assert_eq!(jsx_settings(&c), JsxSettings::default());
        c.oxc = Some(serde_json::Value::Bool(false));
        c.esbuild = Some(serde_json::Value::Bool(false));
        assert_eq!(jsx_settings(&c), JsxSettings::default());
    }

    #[test]
    fn user_conditions_map_vites_dev_prod_placeholder() {
        let cfg = OjConfig {
            resolve: Some(ResolveConfig {
                conditions: Some(vec![
                    "custom".into(),
                    "development|production".into(),
                    "import".into(),
                ]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            resolve_conditions_for(&cfg, "client", true),
            ["custom", "development", "import", "default"]
                .map(String::from)
                .to_vec()
        );
        assert_eq!(
            resolve_conditions_for(&cfg, "client", false),
            ["custom", "production", "import", "default"]
                .map(String::from)
                .to_vec()
        );
    }
}
