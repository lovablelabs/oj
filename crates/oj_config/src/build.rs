use crate::schema::*;

/// A `build.*` option, if the config has a `build` block.
fn build_opt<'a, T>(
    config: &'a OjConfig,
    f: impl FnOnce(&'a BuildConfig) -> Option<T>,
) -> Option<T> {
    config.build.as_ref().and_then(f)
}

/// A `true`/`false`/minifier-name flag: any string but `"false"` means on.
fn flag_on(v: &BoolOrString) -> bool {
    match v {
        BoolOrString::Bool(b) => *b,
        BoolOrString::Str(s) => s != "false",
    }
}

/// A manifest option: `true` (or `"true"`) writes `default`, a string names the
/// file, unset or `false` writes none.
fn manifest_name(v: Option<&BoolOrString>, default: &str) -> Option<String> {
    match v? {
        BoolOrString::Bool(false) => None,
        BoolOrString::Bool(true) => Some(default.to_string()),
        BoolOrString::Str(s) => match s.as_str() {
            "false" => None,
            "true" => Some(default.to_string()),
            _ => Some(s.clone()),
        },
    }
}

/// `build.sourcemap` resolved: Vite's `true` -> separate `.map` files, `"inline"`,
/// `"hidden"` (maps written, no `sourceMappingURL` comment), default off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sourcemap {
    Off,
    File,
    Inline,
    Hidden,
}

pub fn build_sourcemap(config: &OjConfig) -> Sourcemap {
    match build_opt(config, |b| b.sourcemap.as_ref()) {
        None | Some(BoolOrString::Bool(false)) => Sourcemap::Off,
        Some(BoolOrString::Bool(true)) => Sourcemap::File,
        Some(BoolOrString::Str(s)) => match s.as_str() {
            "inline" => Sourcemap::Inline,
            "hidden" => Sourcemap::Hidden,
            "false" => Sourcemap::Off,
            _ => Sourcemap::File,
        },
    }
}

/// `build.minify`: Vite's default is on; a minifier name (`"oxc"`, `"esbuild"`,
/// `"terser"`) selects the tool in Vite and just means "on" here.
pub fn build_minify(config: &OjConfig) -> bool {
    build_opt(config, |b| b.minify.as_ref()).is_none_or(flag_on)
}

/// `build.assetsDir`, normalized to a `/`-separated outDir-relative directory
/// with no surrounding slashes (Vite's default is `assets`; an empty string
/// puts hashed files at the outDir root).
pub fn build_assets_dir(config: &OjConfig) -> String {
    let raw = build_opt(config, |b| b.assets_dir.as_deref()).unwrap_or("assets");
    raw.replace('\\', "/")
        .trim_start_matches("./")
        .trim_matches('/')
        .to_string()
}

/// `tail` under `build.assetsDir`, as Vite spells its default output patterns
/// (`path.posix.join(assetsDir, "[name]-[hash].js")`).
pub fn assets_dir_path(assets_dir: &str, tail: &str) -> String {
    if assets_dir.is_empty() {
        tail.to_string()
    } else {
        format!("{assets_dir}/{tail}")
    }
}

/// `build.manifest`: the manifest file name to write, if any (Vite writes none
/// by default; `true` means `.vite/manifest.json`).
pub fn build_manifest_name(config: &OjConfig) -> Option<String> {
    manifest_name(
        build_opt(config, |b| b.manifest.as_ref()),
        ".vite/manifest.json",
    )
}

/// `build.reportCompressedSize` (Vite default true).
pub fn build_report_compressed_size(config: &OjConfig) -> bool {
    build_opt(config, |b| b.report_compressed_size).unwrap_or(true)
}

/// `build.chunkSizeWarningLimit` in kB (Vite default 500).
pub fn build_chunk_size_warning_limit(config: &OjConfig) -> f64 {
    build_opt(config, |b| b.chunk_size_warning_limit).unwrap_or(500.0)
}

/// Vite 8's `'baseline-widely-available'` default target (constants.ts).
pub const BASELINE_WIDELY_AVAILABLE: &[&str] = &[
    "chrome111",
    "edge111",
    "firefox114",
    "safari16.4",
    "ios16.4",
];

/// Vite's legacy `'modules'` target.
pub const MODULES_TARGET: &[&str] = &["es2020", "edge88", "firefox78", "chrome87", "safari14"];

/// Expands Vite's named target presets to their browser lists.
fn expand_targets(raw: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for t in raw {
        match t.as_str() {
            "baseline-widely-available" => {
                out.extend(BASELINE_WIDELY_AVAILABLE.iter().map(|s| s.to_string()))
            }
            "modules" => out.extend(MODULES_TARGET.iter().map(|s| s.to_string())),
            _ => out.push(t),
        }
    }
    out
}

/// `build.target` as the engine list oxc lowers to, presets expanded; unset
/// means Vite's default baseline.
pub fn build_targets(config: &OjConfig) -> Vec<String> {
    let raw = build_opt(config, |b| b.target.as_ref()).map_or_else(
        || vec!["baseline-widely-available".into()],
        StringOrList::to_vec,
    );
    expand_targets(raw)
}

/// `build.cssTarget` as an engine list for CSS lowering, presets expanded;
/// unset follows `build.target` (Vite).
pub fn build_css_targets(config: &OjConfig) -> Vec<String> {
    match build_opt(config, |b| b.css_target.as_ref()) {
        Some(raw) => expand_targets(raw.to_vec()),
        None => build_targets(config),
    }
}

/// `build.cssMinify` (Vite build.ts): unset follows `build.minify` for the
/// client and is on for a server (SSR) build; a minifier name means "on".
pub fn build_css_minify(config: &OjConfig, server: bool) -> bool {
    match build_opt(config, |b| b.css_minify.as_ref()) {
        None => server || build_minify(config),
        Some(v) => flag_on(v),
    }
}

fn module_preload(config: &OjConfig) -> Option<&serde_json::Value> {
    build_opt(config, |b| b.module_preload.as_ref())
}

/// Whether pages get `<link rel="modulepreload">` for their entry chunks' static
/// imports: on unless `build.modulePreload` is `false` (Vite's html plugin).
pub fn module_preload_links(config: &OjConfig) -> bool {
    !matches!(module_preload(config), Some(serde_json::Value::Bool(false)))
}

/// Whether page entries get Vite's modulepreload polyfill: on unless
/// `build.modulePreload` is `false` or `{ polyfill: false }`.
pub fn module_preload_polyfill(config: &OjConfig) -> bool {
    match module_preload(config) {
        Some(serde_json::Value::Bool(false)) => false,
        Some(serde_json::Value::Object(o)) => {
            o.get("polyfill").and_then(|v| v.as_bool()) != Some(false)
        }
        _ => true,
    }
}

/// The first entry of a `rollupOptions.input` string, array or object.
fn first_input(input: Option<&serde_json::Value>) -> Option<String> {
    let first = match input? {
        serde_json::Value::String(s) => return Some(s.clone()),
        serde_json::Value::Array(a) => a.first(),
        serde_json::Value::Object(o) => o.values().next(),
        _ => None,
    };
    first.and_then(|v| v.as_str()).map(str::to_string)
}

/// The SSR entry `oj build` uses when none is given on the command line:
/// `build.ssr` as a path, or with `build.ssr: true` the `rollupOptions.input`
/// entry (Vite's contract).
pub fn build_ssr_entry(config: &OjConfig) -> Result<Option<String>, String> {
    match build_opt(config, |b| b.ssr.as_ref()) {
        None | Some(BoolOrString::Bool(false)) => Ok(None),
        Some(BoolOrString::Str(s)) => Ok(Some(s.clone())),
        Some(BoolOrString::Bool(true)) => {
            first_input(rolldown_options(config).and_then(|ro| ro.get("input")))
                .map(Some)
                .ok_or_else(|| {
                    "build.ssr: true needs the SSR entry in build.rollupOptions.input".to_string()
                })
        }
    }
}

/// `build.ssrManifest`: the manifest file name to write, if any.
pub fn ssr_manifest_name(config: &OjConfig) -> Option<String> {
    manifest_name(
        build_opt(config, |b| b.ssr_manifest.as_ref()),
        ".vite/ssr-manifest.json",
    )
}

/// `build.rolldownOptions`, else `build.rollupOptions`.
pub fn rolldown_options(config: &OjConfig) -> Option<&serde_json::Value> {
    build_opt(config, |b| {
        b.rolldown_options.as_ref().or(b.rollup_options.as_ref())
    })
}

/// `build.rolldownOptions.input` (else rollupOptions), as written: the dep
/// scan's entries when `optimizeDeps.entries` is unset, before the html-glob
/// fallback (Vite's computeEntries order).
pub fn build_inputs(config: &OjConfig) -> Vec<String> {
    let Some(input) = rolldown_options(config).and_then(|v| v.get("input")) else {
        return Vec::new();
    };
    match input {
        serde_json::Value::String(s) => vec![s.clone()],
        serde_json::Value::Array(a) => a
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        serde_json::Value::Object(o) => o
            .values()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

/// `build.outDir`, defaulted: the dep scan's html glob skips it.
pub fn build_out_dir(config: &OjConfig) -> String {
    build_opt(config, |b| b.out_dir.as_ref())
        .cloned()
        .unwrap_or_else(|| "dist".to_string())
}

/// `environments.<env_name>.build.<field>` as a bool.
pub fn environment_build_bool(config: &OjConfig, env_name: &str, field: &str) -> Option<bool> {
    config
        .environments
        .as_ref()
        .and_then(|e| e.get(env_name))
        .and_then(|e| e.get("build"))
        .and_then(|b| b.get(field))
        .and_then(|v| v.as_bool())
}

#[cfg(test)]
mod build_option_defaults_tests {
    use super::*;

    fn cfg(json: &str) -> OjConfig {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn sourcemap_accepts_bool_and_vite_strings() {
        assert_eq!(build_sourcemap(&cfg("{}")), Sourcemap::Off);
        assert_eq!(
            build_sourcemap(&cfg(r#"{"build":{"sourcemap":true}}"#)),
            Sourcemap::File
        );
        assert_eq!(
            build_sourcemap(&cfg(r#"{"build":{"sourcemap":false}}"#)),
            Sourcemap::Off
        );
        assert_eq!(
            build_sourcemap(&cfg(r#"{"build":{"sourcemap":"inline"}}"#)),
            Sourcemap::Inline
        );
        assert_eq!(
            build_sourcemap(&cfg(r#"{"build":{"sourcemap":"hidden"}}"#)),
            Sourcemap::Hidden
        );
    }

    #[test]
    fn minify_accepts_bool_and_minifier_names() {
        assert!(build_minify(&cfg("{}")));
        assert!(!build_minify(&cfg(r#"{"build":{"minify":false}}"#)));
        assert!(build_minify(&cfg(r#"{"build":{"minify":"terser"}}"#)));
        assert!(build_minify(&cfg(
            r#"{"build":{"minify":"esbuild","terserOptions":{"compress":{}}}}"#
        )));
    }

    #[test]
    fn target_expands_vite_presets_and_accepts_arrays() {
        assert_eq!(build_targets(&cfg("{}")), BASELINE_WIDELY_AVAILABLE);
        assert_eq!(
            build_targets(&cfg(r#"{"build":{"target":"es2015"}}"#)),
            vec!["es2015"]
        );
        assert_eq!(
            build_targets(&cfg(r#"{"build":{"target":"modules"}}"#)),
            MODULES_TARGET
        );
        assert_eq!(
            build_targets(&cfg(r#"{"build":{"target":["es2020","safari14"]}}"#)),
            vec!["es2020", "safari14"]
        );
    }

    #[test]
    fn module_preload_polyfill_defaults_on() {
        assert!(module_preload_polyfill(&cfg("{}")));
        assert!(!module_preload_polyfill(&cfg(
            r#"{"build":{"modulePreload":false}}"#
        )));
        assert!(!module_preload_polyfill(&cfg(
            r#"{"build":{"modulePreload":{"polyfill":false}}}"#
        )));
        assert!(module_preload_polyfill(&cfg(
            r#"{"build":{"modulePreload":{"polyfill":true}}}"#
        )));
        assert!(module_preload_links(&cfg("{}")));
        assert!(!module_preload_links(&cfg(
            r#"{"build":{"modulePreload":false}}"#
        )));
        assert!(
            module_preload_links(&cfg(r#"{"build":{"modulePreload":{"polyfill":false}}}"#)),
            "polyfill off still links"
        );
    }

    #[test]
    fn build_inputs_takes_every_rollup_input_shape() {
        assert_eq!(
            build_inputs(&cfg(
                r#"{"build":{"rollupOptions":{"input":"src/main.ts"}}}"#
            )),
            vec!["src/main.ts"]
        );
        assert_eq!(
            build_inputs(&cfg(
                r#"{"build":{"rolldownOptions":{"input":["a.ts","b.ts"]}}}"#
            )),
            vec!["a.ts", "b.ts"]
        );
        assert_eq!(
            build_inputs(&cfg(
                r#"{"build":{"rollupOptions":{"input":{"main":"m.ts","admin":"ad.ts"}}}}"#
            )),
            vec!["m.ts", "ad.ts"]
        );
        assert!(build_inputs(&cfg("{}")).is_empty());
    }

    #[test]
    fn build_out_dir_defaults_to_dist() {
        assert_eq!(build_out_dir(&cfg("{}")), "dist");
        assert_eq!(build_out_dir(&cfg(r#"{"build":{"outDir":"out"}}"#)), "out");
    }

    #[test]
    fn empty_out_dir_parses() {
        assert_eq!(
            cfg(r#"{"build":{"emptyOutDir":false}}"#)
                .build
                .unwrap()
                .empty_out_dir,
            Some(false)
        );
        assert_eq!(cfg("{}").build.and_then(|b| b.empty_out_dir), None);
    }
}

#[cfg(test)]
mod build_manifest_css_minify_tests {
    use super::*;

    #[test]
    fn manifest_css_minify_and_assets_dir_follow_vite_defaults() {
        let cfg = OjConfig::default();
        assert_eq!(
            build_manifest_name(&cfg),
            None,
            "Vite writes no manifest by default"
        );
        assert!(
            build_css_minify(&cfg, false),
            "cssMinify defaults to minify (on)"
        );
        assert_eq!(build_assets_dir(&cfg), "assets");
        assert!(build_report_compressed_size(&cfg));
        assert_eq!(build_chunk_size_warning_limit(&cfg), 500.0);

        let cfg: OjConfig = serde_json::from_str(
            r#"{"build":{"manifest":true,"minify":false,"assetsDir":"/static/","chunkSizeWarningLimit":1000,"reportCompressedSize":false}}"#,
        )
        .unwrap();
        assert_eq!(
            build_manifest_name(&cfg).as_deref(),
            Some(".vite/manifest.json")
        );
        assert!(
            !build_css_minify(&cfg, false),
            "cssMinify unset follows minify: false"
        );
        assert_eq!(build_assets_dir(&cfg), "static");
        assert_eq!(build_chunk_size_warning_limit(&cfg), 1000.0);
        assert!(!build_report_compressed_size(&cfg));

        let cfg: OjConfig = serde_json::from_str(
            r#"{"build":{"manifest":"meta/m.json","minify":false,"cssMinify":"lightningcss","ssrManifest":"true"}}"#,
        )
        .unwrap();
        assert_eq!(build_manifest_name(&cfg).as_deref(), Some("meta/m.json"));
        assert!(
            build_css_minify(&cfg, false),
            "an explicit cssMinify is independent of minify"
        );
        assert_eq!(
            ssr_manifest_name(&cfg).as_deref(),
            Some(".vite/ssr-manifest.json")
        );
    }
}
