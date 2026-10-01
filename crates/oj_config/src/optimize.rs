use crate::schema::*;

/// An `optimizeDeps.*` list, empty when unset.
fn od_list(
    config: &OjConfig,
    f: impl FnOnce(&OptimizeDepsConfig) -> Option<&Vec<String>>,
) -> Vec<String> {
    config
        .optimize_deps
        .as_ref()
        .and_then(f)
        .cloned()
        .unwrap_or_default()
}

/// `optimizeDeps.include`, `exclude` and `entries`, in that order.
pub fn optimize_deps_lists(config: &OjConfig) -> (Vec<String>, Vec<String>, Vec<String>) {
    (
        od_list(config, |o| o.include.as_ref()),
        od_list(config, |o| o.exclude.as_ref()),
        od_list(config, |o| o.entries.as_ref()),
    )
}

/// `optimizeDeps.needsInterop`: deps forced through CJS->ESM interop.
pub fn optimize_deps_needs_interop(config: &OjConfig) -> Vec<String> {
    od_list(config, |o| o.needs_interop.as_ref())
}

/// `optimizeDeps.force`: ignore any cached pre-bundle and rebuild.
pub fn optimize_deps_force(config: &OjConfig) -> bool {
    config
        .optimize_deps
        .as_ref()
        .and_then(|o| o.force)
        .unwrap_or(false)
}

/// `optimizeDeps.rolldownOptions` (Vite 8) falling back to `esbuildOptions`
/// (Vite <=7): opaque bundler options forwarded to oj's dep bundling.
pub fn optimize_deps_bundler_options(config: &OjConfig) -> Option<serde_json::Value> {
    let od = config.optimize_deps.as_ref()?;
    let Some(options) = od.rolldown_options.as_ref() else {
        return od.esbuild_options.clone();
    };
    // The optimizer takes esbuild-shaped options: lift rolldown's nested
    // `transform` and `resolve` keys to their esbuild names.
    let mut out = options.as_object()?.clone();
    if let Some(transform) = options.get("transform").and_then(|v| v.as_object()) {
        for key in ["define", "target", "keepNames", "drop"] {
            if let Some(value) = transform.get(key) {
                out.insert(key.to_string(), value.clone());
            }
        }
        // esbuild's `jsx` is a string enum; rolldown's object form would fail
        // the whole optimizer run, so only a string crosses.
        if let Some(jsx) = transform.get("jsx").filter(|v| v.is_string()) {
            out.insert("jsx".to_string(), jsx.clone());
        }
    }
    if let Some(resolve) = options.get("resolve").and_then(|v| v.as_object()) {
        for (from, to) in [
            ("conditionNames", "conditions"),
            ("mainFields", "mainFields"),
            ("extensions", "resolveExtensions"),
            ("alias", "alias"),
        ] {
            if let Some(value) = resolve.get(from) {
                out.insert(to.to_string(), value.clone());
            }
        }
    }
    Some(serde_json::Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::resolve::resolve_dedupe;
    use crate::server::server_warmup_files;

    #[test]
    fn optimize_deps_absent_is_empty() {
        let cfg: OjConfig = serde_json::from_str("{}").unwrap();
        assert!(resolve_dedupe(&cfg).is_empty());
        let (inc, exc, ent) = optimize_deps_lists(&cfg);
        assert!(inc.is_empty() && exc.is_empty() && ent.is_empty());
        assert!(optimize_deps_needs_interop(&cfg).is_empty());
        assert!(!optimize_deps_force(&cfg));
        assert!(optimize_deps_bundler_options(&cfg).is_none());
        assert_eq!(server_warmup_files(&cfg), (vec![], vec![]));
    }

    #[test]
    fn optimize_deps_full_surface_parses() {
        let json = r#"{
            "optimizeDeps": {
                "include": ["object-inspect", "@apollo/client"],
                "exclude": ["big-esm"],
                "entries": ["src/main.tsx"],
                "needsInterop": ["object-inspect"],
                "force": true,
                "rolldownOptions": { "define": { "X": "1" } }
            },
            "server": { "warmup": { "clientFiles": ["./src/App.tsx"], "ssrFiles": ["./src/entry-server.tsx"] } }
        }"#;
        let cfg: OjConfig = serde_json::from_str(json).unwrap();
        let (inc, exc, _) = optimize_deps_lists(&cfg);
        assert_eq!(
            inc,
            vec!["object-inspect".to_string(), "@apollo/client".to_string()]
        );
        assert_eq!(exc, vec!["big-esm".to_string()]);
        assert_eq!(
            optimize_deps_needs_interop(&cfg),
            vec!["object-inspect".to_string()]
        );
        assert!(optimize_deps_force(&cfg));
        assert!(optimize_deps_bundler_options(&cfg)
            .unwrap()
            .get("define")
            .is_some());
        let (client, ssr) = server_warmup_files(&cfg);
        assert_eq!(client, vec!["./src/App.tsx".to_string()]);
        assert_eq!(ssr, vec!["./src/entry-server.tsx".to_string()]);
    }

    #[test]
    fn optimize_deps_bundler_options_prefers_rolldown_then_esbuild() {
        // esbuildOptions is honored when rolldownOptions is absent (Vite <=7 configs).
        let cfg: OjConfig =
            serde_json::from_str(r#"{"optimizeDeps":{"esbuildOptions":{"target":"es2020"}}}"#)
                .unwrap();
        assert_eq!(
            optimize_deps_bundler_options(&cfg)
                .unwrap()
                .get("target")
                .unwrap(),
            "es2020"
        );
    }

    #[test]
    fn rolldown_transform_jsx_crosses_only_as_a_string() {
        // esbuild's `jsx` is a string enum; the oxc object shape would fail
        // the whole optimizer run, so it must be dropped at translation.
        let obj: OjConfig = serde_json::from_str(
            r#"{"optimizeDeps":{"rolldownOptions":{"transform":{"jsx":{"runtime":"automatic"},"target":"es2020"}}}}"#,
        )
        .unwrap();
        let out = optimize_deps_bundler_options(&obj).unwrap();
        assert!(out.get("jsx").is_none(), "object jsx must not cross: {out}");
        assert_eq!(out.get("target").unwrap(), "es2020");
        let string: OjConfig = serde_json::from_str(
            r#"{"optimizeDeps":{"rolldownOptions":{"transform":{"jsx":"preserve"}}}}"#,
        )
        .unwrap();
        assert_eq!(
            optimize_deps_bundler_options(&string)
                .unwrap()
                .get("jsx")
                .unwrap(),
            "preserve"
        );
    }
}
