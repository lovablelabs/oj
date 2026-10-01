// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Every environment variable oj reads, from one immutable snapshot: oj's own
//! settings ([`Knobs`]) and the app's env as Vite resolves it ([`AppEnv`]:
//! `.env` files, NODE_ENV, `import.meta.env` defines, `%KEY%` in index.html).

mod app;
mod defines;
mod dotenv;
mod html;
mod knobs;
mod snapshot;

pub use app::AppEnv;
pub use defines::{
    import_meta_env_defines, import_meta_env_defines_with, resolve_node_env, with_process_env,
};
pub use dotenv::{load, load_with, parse};
pub use html::{html_env_map, replace_html_env};
pub use knobs::Knobs;
pub use snapshot::{get, init, Env};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

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
