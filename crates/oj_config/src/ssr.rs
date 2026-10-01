use crate::schema::*;

/// Vite's `ssr.noExternal` / `ssr.external` / `ssr.target` as a matching rule
/// (external.ts): `external` names stay external; `noExternal` (`true`, or a
/// package name / glob / RegExp match on the import specifier or package name)
/// is bundled and transformed; everything else that resolves into
/// `node_modules` stays external.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SsrExternals {
    pub no_external_all: bool,
    /// Package names / globs (`@scope/*`) to bundle.
    pub no_external: Vec<String>,
    /// RegExp sources to bundle (from `noExternal: [/^@acme\//]`).
    pub no_external_regex: Vec<String>,
    /// `external: true` externalizes every dependency, even `noExternal` ones.
    pub external_all: bool,
    pub external: Vec<String>,
    pub target: Option<String>,
}

pub fn ssr_externals(config: &OjConfig) -> SsrExternals {
    let mut out = SsrExternals::default();
    let Some(ssr) = config.ssr.as_ref().and_then(|s| s.as_object()) else {
        return out;
    };
    fn entries(v: Option<&serde_json::Value>) -> (bool, Vec<String>, Vec<String>) {
        let mut names = Vec::new();
        let mut regexes = Vec::new();
        let items: Vec<&serde_json::Value> = match v {
            Some(serde_json::Value::Bool(true)) => return (true, names, regexes),
            Some(serde_json::Value::Array(a)) => a.iter().collect(),
            Some(other @ (serde_json::Value::String(_) | serde_json::Value::Object(_))) => {
                vec![other]
            }
            _ => Vec::new(),
        };
        for item in items {
            match item {
                serde_json::Value::String(s) => names.push(s.clone()),
                serde_json::Value::Object(o) => {
                    if let Some(src) = o.get("regex").and_then(|r| r.as_str()) {
                        regexes.push(src.to_string());
                    }
                }
                _ => {}
            }
        }
        (false, names, regexes)
    }
    let (all, names, regexes) = entries(ssr.get("noExternal"));
    out.no_external_all = all;
    out.no_external = names;
    out.no_external_regex = regexes;
    let (all, names, _) = entries(ssr.get("external"));
    out.external_all = all;
    out.external = names;
    out.target = ssr
        .get("target")
        .and_then(|t| t.as_str())
        .map(str::to_string);
    out
}

/// The package name an import specifier or a `node_modules` path names
/// (`@scope/pkg` or `pkg`).
pub fn package_name_of(spec_or_path: &str) -> Option<String> {
    let rest = match spec_or_path.rfind("/node_modules/") {
        Some(i) => &spec_or_path[i + "/node_modules/".len()..],
        None => spec_or_path,
    };
    let rest = rest.split('?').next().unwrap_or(rest);
    let mut parts = rest.split('/');
    let first = parts.next().filter(|s| !s.is_empty())?;
    if first.starts_with('@') {
        let second = parts.next().filter(|s| !s.is_empty())?;
        Some(format!("{first}/{second}"))
    } else {
        Some(first.to_string())
    }
}

fn glob_matches(pattern: &str, value: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == value;
    }
    // A `*` matches any run of characters (Vite's picomatch use on package
    // names is effectively this).
    let mut rest = value;
    let mut pieces = pattern.split('*').peekable();
    let first = pieces.next().unwrap_or("");
    if !rest.starts_with(first) {
        return false;
    }
    rest = &rest[first.len()..];
    let mut last_piece = "";
    while let Some(piece) = pieces.next() {
        if pieces.peek().is_none() {
            last_piece = piece;
            break;
        }
        match rest.find(piece) {
            Some(i) => rest = &rest[i + piece.len()..],
            None => return false,
        }
    }
    rest.ends_with(last_piece)
}

impl SsrExternals {
    /// The package name of a bare specifier (`@scope/pkg/sub` -> `@scope/pkg`).
    pub fn package_of(spec: &str) -> &str {
        let mut parts = spec.splitn(3, '/');
        let first = parts.next().unwrap_or(spec);
        if first.starts_with('@') {
            match parts.next() {
                Some(second) => &spec[..first.len() + 1 + second.len()],
                None => first,
            }
        } else {
            first
        }
    }

    /// The package name of a resolved `node_modules` path.
    pub fn package_of_path(path: &str) -> Option<&str> {
        let idx = path.rfind("node_modules/")?;
        Some(Self::package_of(&path[idx + "node_modules/".len()..]))
    }

    /// `ssr.target: "webworker"`: as in Vite, every dependency is bundled.
    pub fn webworker(&self) -> bool {
        self.target.as_deref() == Some("webworker")
    }

    /// Whether a dependency (by package name) stays external to an SSR bundle.
    /// `external` wins over `noExternal`; a webworker target bundles everything.
    pub fn is_external_pkg(&self, pkg: &str) -> bool {
        if self.external_all || self.external.iter().any(|e| e == pkg) {
            return true;
        }
        if self.webworker() {
            return false;
        }
        !self.is_no_external(pkg)
    }

    /// Whether `noExternal` claims this specifier / package (so it is bundled
    /// and transformed rather than left to Node).
    pub fn is_no_external(&self, spec: &str) -> bool {
        if self.no_external_all || self.webworker() {
            return true;
        }
        let pkg = package_name_of(spec);
        let candidates = [Some(spec.to_string()), pkg.clone()];
        for pat in &self.no_external {
            for c in candidates.iter().flatten() {
                if glob_matches(pat, c) || c.starts_with(&format!("{pat}/")) {
                    return true;
                }
            }
        }
        for src in &self.no_external_regex {
            if let Ok(re) = regex::Regex::new(src) {
                if candidates.iter().flatten().any(|c| re.is_match(c)) {
                    return true;
                }
            }
        }
        false
    }

    /// Vite's `createIsConfiguredAsExternal` decision for an SSR build: given
    /// the raw specifier, or a resolved path (`in_node_modules`), should the
    /// module stay external?
    pub fn is_external(&self, spec: &str, in_node_modules: bool) -> Option<bool> {
        if self.external_all {
            return Some(true);
        }
        let pkg = package_name_of(spec);
        if self
            .external
            .iter()
            .any(|e| Some(e) == pkg.as_ref() || e == spec)
        {
            return Some(true);
        }
        if self.is_no_external(spec) {
            return Some(false);
        }
        if in_node_modules {
            return Some(true);
        }
        None
    }
}

/// Whether the ssr environment is "runner-backed": its modules execute in a
/// plugin-driven runtime (e.g. the Cloudflare plugin's workerd
/// DevEnvironments), not in oj's own Node SSR runner. The extractor decides it
/// the way Vite does — the raw config, or a plugin's `config` hook return
/// merged Vite-style, declares `environments.<name>.dev.createEnvironment`
/// (the same declaration plugin-host.mjs's buildEnvironments gate reads) —
/// and publishes it as `ssr.runnerBacked` (see detectSsrRunnerBacked in
/// vite-extract.mjs).
///
/// Vite-shaped rule: conditions never cross runtimes. An environment's
/// `resolve.conditions` steer resolution only for code executing in that
/// environment's own runtime, so when ssr is runner-backed its list describes
/// workerd and every Node-executing consumer takes Vite's Node server
/// semantics (`node_server_conditions`) instead.
pub fn ssr_runner_backed(config: &OjConfig) -> bool {
    config
        .ssr
        .as_ref()
        .and_then(|s| s.get("runnerBacked"))
        .and_then(|v| v.as_bool())
        == Some(true)
}

#[cfg(test)]
mod ssr_externals_tests {
    use super::*;

    fn cfg(v: serde_json::Value) -> OjConfig {
        OjConfig {
            ssr: Some(v),
            ..Default::default()
        }
    }

    #[test]
    fn no_external_names_globs_regexes_and_true() {
        let r = ssr_externals(&cfg(serde_json::json!({
            "noExternal": ["lodash-es", "@acme/*", { "regex": "^@tanstack/" }],
            "external": ["sharp"],
            "target": "node"
        })));
        assert!(r.is_no_external("lodash-es"));
        assert!(r.is_no_external("lodash-es/debounce"));
        assert!(r.is_no_external("@acme/ui"));
        assert!(r.is_no_external("/app/node_modules/@acme/ui/dist/index.js"));
        assert!(r.is_no_external("@tanstack/react-query"));
        assert!(!r.is_no_external("react"));
        assert_eq!(r.is_external("sharp", true), Some(true), "external wins");
        assert_eq!(
            r.is_external("/app/node_modules/sharp/lib/index.js", true),
            Some(true)
        );
        assert_eq!(r.is_external("lodash-es", false), Some(false));
        assert_eq!(
            r.is_external("/app/node_modules/react/index.js", true),
            Some(true)
        );
        assert_eq!(
            r.is_external("./local", false),
            None,
            "undecided until resolved"
        );
        assert_eq!(r.target.as_deref(), Some("node"));

        let all = ssr_externals(&cfg(serde_json::json!({ "noExternal": true })));
        assert!(all.no_external_all);
        assert_eq!(
            all.is_external("/app/node_modules/react/index.js", true),
            Some(false)
        );
        let ext_all = ssr_externals(&cfg(
            serde_json::json!({ "noExternal": true, "external": true }),
        ));
        assert_eq!(ext_all.is_external("react", false), Some(true));
        let none = ssr_externals(&OjConfig::default());
        assert_eq!(
            none.is_external("/app/node_modules/react/index.js", true),
            Some(true)
        );
        assert_eq!(none.is_external("react", false), None);
    }

    #[test]
    fn package_names_from_specifiers_and_paths() {
        assert_eq!(package_name_of("react").as_deref(), Some("react"));
        assert_eq!(
            package_name_of("react/jsx-runtime").as_deref(),
            Some("react")
        );
        assert_eq!(
            package_name_of("@scope/pkg/sub?x").as_deref(),
            Some("@scope/pkg")
        );
        assert_eq!(
            package_name_of("/a/node_modules/x/node_modules/@s/p/i.js").as_deref(),
            Some("@s/p")
        );
        assert_eq!(package_name_of(""), None);
        assert_eq!(package_name_of("@scope"), None);
    }
}

#[cfg(test)]
mod ssr_option_tests {
    use super::*;

    use crate::build::{build_ssr_entry, ssr_manifest_name};

    fn cfg(json: &str) -> OjConfig {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn package_names_from_specifiers_and_paths() {
        assert_eq!(SsrExternals::package_of("react"), "react");
        assert_eq!(SsrExternals::package_of("react-dom/server"), "react-dom");
        assert_eq!(SsrExternals::package_of("@scope/pkg/sub/x"), "@scope/pkg");
        assert_eq!(SsrExternals::package_of("@scope/pkg"), "@scope/pkg");
        assert_eq!(
            SsrExternals::package_of_path(
                "/app/node_modules/.pnpm/x/node_modules/@scope/pkg/dist/i.js"
            ),
            Some("@scope/pkg")
        );
        assert_eq!(SsrExternals::package_of_path("/app/src/x.js"), None);
    }

    #[test]
    fn ssr_externals_follow_vite_precedence() {
        let e = ssr_externals(&cfg("{}"));
        assert!(e.is_external_pkg("react"), "deps are external by default");
        let e = ssr_externals(&cfg(
            r#"{"ssr":{"noExternal":["ui-kit"],"external":["react"]}}"#,
        ));
        assert!(!e.is_external_pkg("ui-kit"));
        assert!(e.is_external_pkg("react"));
        assert!(e.is_external_pkg("lodash"));
        let e = ssr_externals(&cfg(r#"{"ssr":{"noExternal":true,"external":["react"]}}"#));
        assert!(
            !e.is_external_pkg("lodash"),
            "noExternal: true bundles everything"
        );
        assert!(e.is_external_pkg("react"), "except explicit externals");
        let e = ssr_externals(&cfg(r#"{"ssr":{"noExternal":"single"}}"#));
        assert!(!e.is_external_pkg("single"));
        let e = ssr_externals(&cfg(r#"{"ssr":{"target":"webworker"}}"#));
        assert!(e.webworker() && !e.is_external_pkg("anything"));
    }

    #[test]
    fn build_ssr_true_takes_the_rollup_input() {
        assert_eq!(build_ssr_entry(&cfg("{}")), Ok(None));
        assert_eq!(
            build_ssr_entry(&cfg(r#"{"build":{"ssr":"src/s.ts"}}"#)),
            Ok(Some("src/s.ts".into()))
        );
        assert_eq!(
            build_ssr_entry(&cfg(
                r#"{"build":{"ssr":true,"rollupOptions":{"input":"src/entry-server.ts"}}}"#
            )),
            Ok(Some("src/entry-server.ts".into()))
        );
        assert!(build_ssr_entry(&cfg(r#"{"build":{"ssr":true}}"#)).is_err());
        assert_eq!(
            build_ssr_entry(&cfg(r#"{"build":{"ssr":false}}"#)),
            Ok(None)
        );
    }

    #[test]
    fn ssr_manifest_name_resolves() {
        assert_eq!(ssr_manifest_name(&cfg("{}")), None);
        assert_eq!(
            ssr_manifest_name(&cfg(r#"{"build":{"ssrManifest":true}}"#)),
            Some(".vite/ssr-manifest.json".into())
        );
        assert_eq!(
            ssr_manifest_name(&cfg(r#"{"build":{"ssrManifest":"m.json"}}"#)),
            Some("m.json".into())
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Conditions never cross runtimes: `ssr.runnerBacked` (published by the
    // extractor from a structural signal — the raw config's
    // `environments.ssr.dev.createEnvironment` or the Cloudflare dev plugin in
    // the plugin list) tells every Node-executing consumer to take Vite's Node
    // server semantics instead of the runner environment's own list.
    #[test]
    fn ssr_runner_backed_reads_the_extractor_flag() {
        let from = |json: &str| -> OjConfig { serde_json::from_str(json).unwrap() };
        assert!(crate::ssr_runner_backed(&from(
            r#"{ "ssr": { "runnerBacked": true } }"#
        )));
        assert!(!crate::ssr_runner_backed(&from(
            r#"{ "ssr": { "runnerBacked": false } }"#
        )));
        assert!(!crate::ssr_runner_backed(&from(
            r#"{ "ssr": { "noExternal": true } }"#
        )));
        assert!(!crate::ssr_runner_backed(&OjConfig::default()));
    }
}
