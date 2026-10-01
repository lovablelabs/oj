use crate::schema::*;

/// Export conditions for the dev server (Vite's `development` condition active).
pub fn resolve_conditions(config: &OjConfig, env_name: &str) -> Vec<String> {
    resolve_conditions_for(config, env_name, true)
}

/// `environments.<env_name>.resolve`, if set.
fn env_resolve<'a>(config: &'a OjConfig, env_name: &str) -> Option<&'a serde_json::Value> {
    config
        .environments
        .as_ref()
        .and_then(|e| e.get(env_name))
        .and_then(|e| e.get("resolve"))
}

/// The string items of a JSON array (non-strings skipped), or None when not an array.
fn str_list(v: &serde_json::Value) -> Option<Vec<String>> {
    v.as_array().map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    })
}

/// A resolve list's environment-level value: `environments.<name>.resolve.<key>`,
/// then for the ssr environment only the `ssr.resolve.<key>` sugar. Callers fall
/// back to the top-level `resolve` list.
fn env_resolve_list(config: &OjConfig, env_name: &str, key: &str) -> Option<Vec<String>> {
    env_resolve(config, env_name)
        .and_then(|r| r.get(key))
        .and_then(str_list)
        .or_else(|| {
            if env_name != "ssr" {
                return None;
            }
            config
                .ssr
                .as_ref()
                .and_then(|s| s.get("resolve"))
                .and_then(|r| r.get(key))
                .and_then(str_list)
        })
}

fn dev_prod(dev: bool) -> &'static str {
    if dev {
        "development"
    } else {
        "production"
    }
}

/// Vite's `development|production` placeholder mapped to the active one.
fn map_dev_prod(c: String, dev_prod: &str) -> String {
    if c == "development|production" {
        dev_prod.to_string()
    } else {
        c
    }
}

/// Appends `user` (placeholder mapped, deduped), then `import`/`default`, which
/// the resolver always matches.
fn extend_conditions(out: &mut Vec<String>, user: Vec<String>, dev_prod: &str) {
    for c in user {
        let c = map_dev_prod(c, dev_prod);
        if !out.contains(&c) {
            out.push(c);
        }
    }
    for always in ["import", "default"] {
        if !out.iter().any(|c| c == always) {
            out.push(always.to_string());
        }
    }
}

/// The user's own `resolve.conditions` for an environment, verbatim, or None
/// when the config leaves the defaults in place. Precedence: environment, then
/// (ssr only) the `ssr.resolve` sugar, then the top-level list. The sugar must
/// win over the top level: it carries the resolved ssr environment's conditions,
/// while the resolved top-level list is Vite's client defaults (`browser` et al),
/// which must never steer server-side resolution.
pub fn user_resolve_conditions(config: &OjConfig, env_name: &str) -> Option<Vec<String>> {
    env_resolve_list(config, env_name, "conditions")
        .or_else(|| config.resolve.as_ref().and_then(|r| r.conditions.clone()))
}

/// The user's `resolve.externalConditions` for an environment (Vite: the
/// conditions externalized SSR deps resolve with, replacing, never merging, the
/// environment's `resolve.conditions`). Same precedence as
/// `user_resolve_conditions`.
pub fn user_external_conditions(config: &OjConfig, env_name: &str) -> Option<Vec<String>> {
    env_resolve_list(config, env_name, "externalConditions").or_else(|| {
        config
            .resolve
            .as_ref()
            .and_then(|r| r.external_conditions.clone())
    })
}

/// Export conditions for an environment, as Vite resolves them. Default:
/// `browser`/`node`, `module`, `development` or `production` (per `dev`), plus
/// `import` and `default`. A user `resolve.conditions` list replaces the
/// defaults (no implicit `module` or dev/prod, like Vite), with the
/// `development|production` placeholder mapped and `import`/`default` always
/// kept so a dual-package `exports` map still resolves.
pub fn resolve_conditions_for(config: &OjConfig, env_name: &str, dev: bool) -> Vec<String> {
    let dev_prod = dev_prod(dev);
    if let Some(user) = user_resolve_conditions(config, env_name) {
        let mut out = Vec::new();
        extend_conditions(&mut out, user, dev_prod);
        return out;
    }
    let base = if env_name == "ssr" { "node" } else { "browser" };
    [base, "import", "module", dev_prod, "default"]
        .map(String::from)
        .to_vec()
}

/// Conditions for a Node-executing SSR consumer (Start loader, unbundled SSR
/// resolver) when the ssr environment is runner-backed: Vite's Node server
/// semantics, not the foreign runtime's list. DEFAULT_SERVER_CONDITIONS
/// (`module`, `node`, `development|production`) plus the user's RAW top-level
/// `resolve.conditions` (the resolved top-level list is the client's and never
/// crosses), plus `import`/`default`.
pub fn node_server_conditions(config: &OjConfig, dev: bool) -> Vec<String> {
    let dev_prod = dev_prod(dev);
    let mut out: Vec<String> = ["module", "node", dev_prod].map(String::from).to_vec();
    let user = config
        .raw_resolve
        .as_ref()
        .and_then(|r| r.conditions.clone())
        .unwrap_or_default();
    extend_conditions(&mut out, user, dev_prod);
    out
}

/// `externalConditions` for the same consumers: the user's RAW top-level
/// `resolve.externalConditions` when set (in Vite these inherit into every
/// environment and replace the default), else DEFAULT_EXTERNAL_CONDITIONS
/// (`node`, `module-sync`).
pub fn node_server_external_conditions(config: &OjConfig, dev: bool) -> Vec<String> {
    let dev_prod = dev_prod(dev);
    match config
        .raw_resolve
        .as_ref()
        .and_then(|r| r.external_conditions.clone())
    {
        Some(user) => user
            .into_iter()
            .map(|c| map_dev_prod(c, dev_prod))
            .collect(),
        None => ["node", "module-sync"].map(String::from).to_vec(),
    }
}

pub fn resolve_dedupe(config: &OjConfig) -> Vec<String> {
    config
        .resolve
        .as_ref()
        .and_then(|r| r.dedupe.as_ref())
        .cloned()
        .unwrap_or_default()
}

pub fn resolve_extensions(config: &OjConfig) -> Option<Vec<String>> {
    config.resolve.as_ref().and_then(|r| r.extensions.clone())
}

pub fn resolve_main_fields(config: &OjConfig) -> Option<Vec<String>> {
    config.resolve.as_ref().and_then(|r| r.main_fields.clone())
}

pub fn resolve_preserve_symlinks(config: &OjConfig) -> bool {
    config
        .resolve
        .as_ref()
        .and_then(|r| r.preserve_symlinks)
        .unwrap_or(false)
}

/// Top-level `resolve.alias`, overlaid by the environment's string entries.
pub fn resolve_alias(config: &OjConfig, env_name: &str) -> Vec<(String, String)> {
    let mut merged: std::collections::BTreeMap<String, String> = config
        .resolve
        .as_ref()
        .and_then(|r| r.alias.as_ref())
        .map(|a| a.clone().into_iter().collect())
        .unwrap_or_default();
    if let Some(env_alias) = env_resolve(config, env_name)
        .and_then(|r| r.get("alias"))
        .and_then(|a| a.as_object())
    {
        for (find, replacement) in env_alias {
            if let Some(s) = replacement.as_str() {
                merged.insert(find.clone(), s.to_string());
            }
        }
    }
    merged.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::build::environment_build_bool;
    use crate::load::load;
    use crate::optimize::optimize_deps_lists;
    use crate::server::{config_defines, environment_defines};

    #[test]
    fn external_conditions_fall_back_from_environment_to_ssr_sugar_to_top_level() {
        let from = |json: &str| -> OjConfig { serde_json::from_str(json).unwrap() };
        let env = from(
            r#"{ "environments": { "ssr": { "resolve": { "externalConditions": ["env"] } } },
                 "ssr": { "resolve": { "externalConditions": ["sugar"] } },
                 "resolve": { "externalConditions": ["top"] } }"#,
        );
        assert_eq!(
            crate::user_external_conditions(&env, "ssr"),
            Some(vec!["env".to_string()])
        );
        let sugar = from(
            r#"{ "ssr": { "resolve": { "externalConditions": ["sugar"] } },
                 "resolve": { "externalConditions": ["top"] } }"#,
        );
        assert_eq!(
            crate::user_external_conditions(&sugar, "ssr"),
            Some(vec!["sugar".to_string()])
        );
        // The ssr sugar names the ssr environment only.
        assert_eq!(
            crate::user_external_conditions(&sugar, "worker"),
            Some(vec!["top".to_string()])
        );
        let top = from(r#"{ "resolve": { "externalConditions": ["top"] } }"#);
        assert_eq!(
            crate::user_external_conditions(&top, "ssr"),
            Some(vec!["top".to_string()])
        );
        assert_eq!(
            crate::user_external_conditions(&OjConfig::default(), "ssr"),
            None
        );
    }

    // Same fallback as externalConditions. The sugar carries the resolved ssr
    // environment's conditions; the resolved top-level list is Vite's client
    // defaults (`browser`) and must not win for ssr.
    #[test]
    fn resolve_conditions_fall_back_from_environment_to_ssr_sugar_to_top_level() {
        let from = |json: &str| -> OjConfig { serde_json::from_str(json).unwrap() };
        let env = from(
            r#"{ "environments": { "ssr": { "resolve": { "conditions": ["env"] } } },
                 "ssr": { "resolve": { "conditions": ["sugar"] } },
                 "resolve": { "conditions": ["top"] } }"#,
        );
        assert_eq!(
            crate::user_resolve_conditions(&env, "ssr"),
            Some(vec!["env".to_string()])
        );
        let sugar = from(
            r#"{ "ssr": { "resolve": { "conditions": ["workerd", "worker", "module", "browser"] } },
                 "resolve": { "conditions": ["module", "browser", "development|production"] } }"#,
        );
        assert_eq!(
            crate::user_resolve_conditions(&sugar, "ssr"),
            Some(vec![
                "workerd".to_string(),
                "worker".to_string(),
                "module".to_string(),
                "browser".to_string()
            ])
        );
        // The ssr sugar names the ssr environment only.
        assert_eq!(
            crate::user_resolve_conditions(&sugar, "client"),
            Some(vec![
                "module".to_string(),
                "browser".to_string(),
                "development|production".to_string()
            ])
        );
        let top = from(r#"{ "resolve": { "conditions": ["top"] } }"#);
        assert_eq!(
            crate::user_resolve_conditions(&top, "ssr"),
            Some(vec!["top".to_string()])
        );
        assert_eq!(
            crate::user_resolve_conditions(&OjConfig::default(), "ssr"),
            None
        );
    }

    // Runner-backed (workerd) ssr: its set never crosses into the Node
    // resolver; Vite's DEFAULT_SERVER_CONDITIONS plus RAW top-level extras
    // plus import/default apply.
    #[test]
    fn a_runner_backed_workerd_config_gets_node_server_conditions() {
        let list = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let config: OjConfig = serde_json::from_str(
            r#"{ "ssr": { "runnerBacked": true, "resolve": { "conditions": ["workerd", "worker", "module", "browser", "development|production"] } } }"#,
        )
        .unwrap();
        assert!(crate::ssr_runner_backed(&config));
        assert_eq!(
            crate::node_server_conditions(&config, true),
            list(&["module", "node", "development", "import", "default"])
        );
        assert_eq!(
            crate::node_server_conditions(&config, false),
            list(&["module", "node", "production", "import", "default"])
        );
        // The default externalConditions mirror Vite's DEFAULT_EXTERNAL_CONDITIONS.
        assert_eq!(
            crate::node_server_external_conditions(&config, true),
            list(&["node", "module-sync"])
        );

        // RAW top-level lists: conditions join the Node defaults (deduped,
        // dev|prod mapped); externalConditions replace the default, as in Vite.
        let with_user: OjConfig = serde_json::from_str(
            r#"{ "ssr": { "runnerBacked": true },
                 "rawResolve": { "conditions": ["custom", "module", "development|production"],
                                 "externalConditions": ["custom-ext", "development|production"] },
                 "resolve": { "conditions": ["module", "browser", "development|production"] } }"#,
        )
        .unwrap();
        assert_eq!(
            crate::node_server_conditions(&with_user, true),
            list(&[
                "module",
                "node",
                "development",
                "custom",
                "import",
                "default"
            ])
        );
        assert_eq!(
            crate::node_server_external_conditions(&with_user, true),
            list(&["custom-ext", "development"])
        );

        // Not runner-backed: the environment chain passes verbatim, so an
        // explicit user `browser` stays honored, as in Vite.
        let browser: OjConfig = serde_json::from_str(
            r#"{ "ssr": { "resolve": { "conditions": ["browser", "module"] } } }"#,
        )
        .unwrap();
        assert!(!crate::ssr_runner_backed(&browser));
        assert_eq!(
            crate::resolve_conditions(&browser, "ssr"),
            list(&["browser", "module", "import", "default"])
        );
    }

    #[test]
    fn optimize_deps_and_dedupe_accessors() {
        let json = r#"{"resolve":{"dedupe":["react","react-dom"]},
            "optimizeDeps":{"include":["cjs-dep"],"exclude":["big-esm"],"entries":["src/main.tsx"]}}"#;
        let cfg: OjConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            resolve_dedupe(&cfg),
            vec!["react".to_string(), "react-dom".to_string()]
        );
        let (inc, exc, ent) = optimize_deps_lists(&cfg);
        assert_eq!(inc, vec!["cjs-dep".to_string()]);
        assert_eq!(exc, vec!["big-esm".to_string()]);
        assert_eq!(ent, vec!["src/main.tsx".to_string()]);
    }

    #[test]
    fn resolve_extensions_main_fields_and_preserve_symlinks_accessors() {
        let json = r#"{"resolve":{
            "extensions":[".vue",".ts",".js"],
            "mainFields":["main","module"],
            "preserveSymlinks":true}}"#;
        let cfg: OjConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            resolve_extensions(&cfg),
            Some(vec![
                ".vue".to_string(),
                ".ts".to_string(),
                ".js".to_string()
            ])
        );
        assert_eq!(
            resolve_main_fields(&cfg),
            Some(vec!["main".to_string(), "module".to_string()])
        );
        assert!(resolve_preserve_symlinks(&cfg));
        // Absent resolve.* leaves extensions/mainFields unset, symlinks followed.
        let empty: OjConfig = serde_json::from_str("{}").unwrap();
        assert!(resolve_extensions(&empty).is_none());
        assert!(resolve_main_fields(&empty).is_none());
        assert!(!resolve_preserve_symlinks(&empty));
    }

    #[test]
    fn unknown_vite_keys_are_ignored_not_rejected() {
        // Vite never validates config keys; unknown options must not fail the load.
        let json = r#"{
            "base": "/app/",
            "resolve": { "dedupe": ["react"], "mainFields": ["module","browser"], "preserveSymlinks": true },
            "optimizeDeps": { "include": ["cjs-dep"], "esbuildOptions": { "target": "es2020" }, "needsInterop": ["x"] },
            "build": { "outDir": "out", "cssCodeSplit": false },
            "css": { "modules": {} },
            "worker": { "format": "es" },
            "logLevel": "silent"
        }"#;
        let cfg: OjConfig =
            serde_json::from_str(json).expect("unknown Vite keys must not fail config load");
        assert_eq!(cfg.base.as_deref(), Some("/app/"));
        assert_eq!(resolve_dedupe(&cfg), vec!["react".to_string()]);
        let (inc, _, _) = optimize_deps_lists(&cfg);
        assert_eq!(inc, vec!["cjs-dep".to_string()]);
    }

    #[test]
    fn default_config_resolver_fallbacks() {
        let s = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let cfg = load(std::path::Path::new("/nonexistent-oj-root")).unwrap();
        assert!(config_defines(&cfg).is_empty());
        assert!(environment_defines(&cfg, "ssr").is_empty());
        assert!(resolve_alias(&cfg, "client").is_empty());
        assert_eq!(environment_build_bool(&cfg, "client", "minify"), None);
        assert_eq!(
            resolve_conditions(&cfg, "ssr"),
            s(&["node", "import", "module", "development", "default"])
        );
        assert_eq!(
            resolve_conditions(&cfg, "client"),
            s(&["browser", "import", "module", "development", "default"])
        );
        assert_eq!(
            resolve_conditions_for(&cfg, "client", false),
            s(&["browser", "import", "module", "production", "default"])
        );
    }
}
