use crate::schema::*;

/// Export conditions for the dev server (Vite's `development` condition active).
pub fn resolve_conditions(config: &OjConfig, env_name: &str) -> Vec<String> {
    resolve_conditions_for(config, env_name, true)
}

/// The user's own `resolve.conditions` for an environment (its
/// `environments.<name>.resolve.conditions` first, then — for the ssr
/// environment — the `ssr.resolve` sugar, then the top-level list), verbatim,
/// or None when the config leaves the defaults in place. The ssr sugar must
/// win over the top-level list: the extractor publishes the resolved ssr
/// environment's conditions there (e.g. a Cloudflare workerd set), while the
/// resolved top-level list carries Vite's client defaults (`browser` et al),
/// which must never steer server-side resolution.
pub fn user_resolve_conditions(config: &OjConfig, env_name: &str) -> Option<Vec<String>> {
    let str_list = |c: &serde_json::Value| {
        c.as_array().map(|c| {
            c.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
    };
    config
        .environments
        .as_ref()
        .and_then(|e| e.get(env_name))
        .and_then(|e| e.get("resolve"))
        .and_then(|r| r.get("conditions"))
        .and_then(&str_list)
        .or_else(|| {
            if env_name != "ssr" {
                return None;
            }
            config
                .ssr
                .as_ref()
                .and_then(|s| s.get("resolve"))
                .and_then(|r| r.get("conditions"))
                .and_then(&str_list)
        })
        .or_else(|| config.resolve.as_ref().and_then(|r| r.conditions.clone()))
}

/// The user's `resolve.externalConditions` for an environment (Vite: the
/// conditions externalized SSR deps resolve with, replacing — never merging —
/// the environment's `resolve.conditions`).
pub fn user_external_conditions(config: &OjConfig, env_name: &str) -> Option<Vec<String>> {
    let str_list = |c: &serde_json::Value| {
        c.as_array().map(|c| {
            c.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
    };
    // Most specific wins, as in Vite: the environment's list, then (for the ssr
    // environment) the `ssr.resolve` sugar, then the top-level `resolve` list.
    config
        .environments
        .as_ref()
        .and_then(|e| e.get(env_name))
        .and_then(|e| e.get("resolve"))
        .and_then(|r| r.get("externalConditions"))
        .and_then(&str_list)
        .or_else(|| {
            if env_name != "ssr" {
                return None;
            }
            config
                .ssr
                .as_ref()
                .and_then(|s| s.get("resolve"))
                .and_then(|r| r.get("externalConditions"))
                .and_then(&str_list)
        })
        .or_else(|| {
            config
                .resolve
                .as_ref()
                .and_then(|r| r.external_conditions.clone())
        })
}

/// Export conditions for an environment, as Vite resolves them: the default set
/// is `browser`/`node`, `module`, and `development` or `production` (per `dev`),
/// plus `import` and `default`, which the resolver always matches. A user
/// `resolve.conditions` list replaces the defaults (Vite parity: no implicit
/// `module` or dev/prod) but Vite's `development|production` placeholder is
/// mapped to the active one, and `import`/`default` are always kept so a
/// dual-package `exports` map still resolves.
pub fn resolve_conditions_for(config: &OjConfig, env_name: &str, dev: bool) -> Vec<String> {
    let dev_prod = if dev { "development" } else { "production" };
    if let Some(user) = user_resolve_conditions(config, env_name) {
        let mut out: Vec<String> = Vec::new();
        for c in user {
            let c = if c == "development|production" {
                dev_prod.to_string()
            } else {
                c
            };
            if !out.contains(&c) {
                out.push(c);
            }
        }
        for always in ["import", "default"] {
            if !out.iter().any(|c| c == always) {
                out.push(always.to_string());
            }
        }
        return out;
    }
    let base = if env_name == "ssr" { "node" } else { "browser" };
    [base, "import", "module", dev_prod, "default"]
        .map(String::from)
        .to_vec()
}

/// Conditions for a Node-executing SSR consumer (the Start loader, the
/// unbundled SSR resolver) when the ssr environment is runner-backed: Vite's
/// Node server semantics instead of the foreign runtime's list.
/// DEFAULT_SERVER_CONDITIONS (`module`, `node`, `development|production` —
/// vite 8.2.1 dist node.js) plus the user's RAW top-level `resolve.conditions`
/// (user-authored and runtime-neutral; the resolved top-level list is the
/// client environment's and never crosses), plus `import`/`default`, which the
/// resolver always matches.
pub fn node_server_conditions(config: &OjConfig, dev: bool) -> Vec<String> {
    let dev_prod = if dev { "development" } else { "production" };
    let mut out: Vec<String> = ["module", "node", dev_prod].map(String::from).to_vec();
    let user = config
        .raw_resolve
        .as_ref()
        .and_then(|r| r.conditions.clone())
        .unwrap_or_default();
    for c in user {
        let c = if c == "development|production" {
            dev_prod.to_string()
        } else {
            c
        };
        if !out.contains(&c) {
            out.push(c);
        }
    }
    for always in ["import", "default"] {
        if !out.iter().any(|c| c == always) {
            out.push(always.to_string());
        }
    }
    out
}

/// `externalConditions` for the same consumers: the user's RAW top-level
/// `resolve.externalConditions` when set (in Vite, top-level externalConditions
/// DO inherit into every environment and a user list replaces the default),
/// else Vite's DEFAULT_EXTERNAL_CONDITIONS (`node`, `module-sync`).
pub fn node_server_external_conditions(config: &OjConfig, dev: bool) -> Vec<String> {
    let dev_prod = if dev { "development" } else { "production" };
    match config
        .raw_resolve
        .as_ref()
        .and_then(|r| r.external_conditions.clone())
    {
        Some(user) => user
            .into_iter()
            .map(|c| {
                if c == "development|production" {
                    dev_prod.to_string()
                } else {
                    c
                }
            })
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

pub fn resolve_alias(config: &OjConfig, env_name: &str) -> Vec<(String, String)> {
    let mut merged: std::collections::BTreeMap<String, String> = config
        .resolve
        .as_ref()
        .and_then(|r| r.alias.as_ref())
        .map(|a| a.clone().into_iter().collect())
        .unwrap_or_default();
    if let Some(env_alias) = config
        .environments
        .as_ref()
        .and_then(|e| e.get(env_name))
        .and_then(|e| e.get("resolve"))
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

    // `resolve.conditions` falls back the same way: environment, then the ssr
    // sugar, then the top-level list. The sugar carries the resolved ssr
    // environment's conditions (e.g. the Cloudflare plugin's workerd set),
    // while the resolved top-level list is Vite's client defaults (`browser`);
    // reading the top-level list for the ssr environment steered the Node SSR
    // loader into browser builds (`document is not defined`).
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

    // The exact composition the unbundled SSR path (oj_server's ssr_resolver)
    // uses on a runner-backed (workerd) config: the ssr environment's workerd
    // set (browser included) never crosses into the Node resolver — Vite's
    // DEFAULT_SERVER_CONDITIONS equivalents apply, plus the user's RAW
    // top-level extras, plus import/default.
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

        // The user's RAW top-level resolve lists are the one user-authored,
        // runtime-neutral source: conditions join the Node defaults (deduped,
        // dev|prod mapped); externalConditions replace the default, as a user
        // list does in Vite.
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

        // NOT runner-backed: the environment chain passes verbatim — an
        // explicit user `browser` (happy-dom-style Node SSR) stays honored, as
        // Vite honors user conditions.
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
        // Vite never validates config-file keys; a config carrying options oj
        // doesn't model must load and keep its known fields, not hard-fail.
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
