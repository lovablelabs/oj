// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `optimizeDeps.rolldownOptions` as rolldown's Rust options. Vite spreads
//! the object into both the scan and the pre-bundle (and `output` into the
//! bundle's write); the legacy `optimizeDeps.esbuildOptions` keys Vite 8
//! still converts sit underneath it. Values a JSON config cannot carry (a
//! function, a regex) never reach here; a key oj does not map is reported
//! once and left out, never silently dropped.

use rolldown_common::{
    AddonOutputOption, BundlerOptions, Either, IsExternal, JsxOptions, LegalComments, ModuleType,
    Platform, RawMinifyOptions, ResolveOptions, TreeshakeOptions,
};
use rolldown_utils::indexmap::FxIndexMap;
use rolldown_utils::pattern_filter::StringOrRegex;
use rustc_hash::FxHashMap;
use serde_json::{Map, Value};

use crate::optimize::OptimizeInput;

/// Vite's default optimizer target (ESBUILD_BASELINE_WIDELY_AVAILABLE_TARGET).
const BASELINE_TARGET: &[&str] = &[
    "chrome111",
    "edge111",
    "firefox114",
    "safari16.4",
    "ios16.4",
];

/// The conditions rolldown's resolver adds itself per import kind; Vite's
/// esbuild-era contract kept them out of the configured list.
const IMPLICIT_CONDITIONS: &[&str] = &["import", "require", "default"];

pub(crate) struct DepOptions {
    platform: Platform,
    resolve: ResolveOptions,
    define: FxIndexMap<String, String>,
    target: Either<String, Vec<String>>,
    jsx: Option<Either<String, JsxOptions>>,
    module_types: FxHashMap<String, ModuleType>,
    external: Vec<String>,
    treeshake: Option<bool>,
    keep_names: Option<bool>,
    output: Output,
}

#[derive(Default)]
struct Output {
    banner: Option<String>,
    footer: Option<String>,
    minify: Option<bool>,
    legal_comments: Option<LegalComments>,
    keep_names: Option<bool>,
    top_level_var: Option<bool>,
}

fn strings(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::String(s) => Some(vec![s.clone()]),
        Value::Array(a) => a.iter().map(|s| s.as_str().map(str::to_string)).collect(),
        _ => None,
    }
}

/// Once per key per process: every optimizer run reads the same config.
fn unsupported(path: &str) {
    static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if !seen.iter().any(|p| p == path) {
        seen.push(path.to_string());
        eprintln!(
            "oj: optimizeDeps.{path} is not supported by oj's dependency pre-bundle; ignored"
        );
    }
}

/// `resolve.alias` in either of rolldown's shapes: `{ find: replacement }`
/// (a string, a list of fallbacks, or `false`).
fn alias(v: &Value) -> Option<Vec<(String, Vec<Option<String>>)>> {
    let map = v.as_object()?;
    Some(
        map.iter()
            .map(|(find, to)| {
                let targets = match to {
                    Value::Bool(false) => vec![None],
                    Value::Array(a) => a.iter().map(|t| t.as_str().map(str::to_string)).collect(),
                    other => vec![other.as_str().map(str::to_string)],
                };
                (find.clone(), targets)
            })
            .collect(),
    )
}

fn jsx(v: &Value) -> Option<Either<String, JsxOptions>> {
    match v {
        Value::String(s) => Some(Either::Left(s.clone())),
        Value::Object(o) => {
            let s = |k: &str| o.get(k).and_then(|v| v.as_str()).map(str::to_string);
            Some(Either::Right(JsxOptions {
                runtime: s("runtime"),
                development: o.get("development").and_then(Value::as_bool),
                import_source: s("importSource"),
                pragma: s("pragma"),
                pragma_frag: s("pragmaFrag"),
                ..Default::default()
            }))
        }
        _ => None,
    }
}

impl DepOptions {
    pub fn new(input: &OptimizeInput) -> DepOptions {
        let esbuild = input
            .bundler_options
            .as_ref()
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let rolldown = input
            .rolldown_options
            .as_ref()
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let es = |k: &str| esbuild.get(k).filter(|v| !v.is_null());

        let conditions: Vec<String> = es("conditions").and_then(strings).unwrap_or_else(|| {
            if input.conditions.is_empty() {
                ["browser", "module", "development"]
                    .map(str::to_string)
                    .to_vec()
            } else {
                input.conditions.clone()
            }
        });
        let mut resolve = ResolveOptions {
            main_fields: Some(es("mainFields").and_then(strings).unwrap_or_else(|| {
                if input.main_fields.is_empty() {
                    ["browser", "module", "main"].map(str::to_string).to_vec()
                } else {
                    input.main_fields.clone()
                }
            })),
            condition_names: Some(
                conditions
                    .into_iter()
                    .filter(|c| !IMPLICIT_CONDITIONS.contains(&c.as_str()))
                    .collect(),
            ),
            extensions: es("resolveExtensions")
                .and_then(strings)
                .or_else(|| (!input.extensions.is_empty()).then(|| input.extensions.clone())),
            symlinks: input.preserve_symlinks.then_some(false),
            alias: es("alias").and_then(alias),
            ..Default::default()
        };

        let mut define = FxIndexMap::default();
        define.insert(
            "process.env.NODE_ENV".to_string(),
            Value::String(input.node_env.clone()).to_string(),
        );
        if let Some(map) = es("define").and_then(Value::as_object) {
            for (k, v) in map {
                if let Some(v) = v.as_str() {
                    define.insert(k.clone(), v.to_string());
                }
            }
        }
        let mut options = DepOptions {
            platform: es("platform")
                .and_then(Value::as_str)
                .and_then(|p| Platform::try_from(p).ok())
                .unwrap_or(Platform::Browser),
            resolve: ResolveOptions::default(),
            define,
            target: es("target")
                .and_then(strings)
                .map(Either::Right)
                .unwrap_or_else(|| {
                    Either::Right(BASELINE_TARGET.iter().map(|t| t.to_string()).collect())
                }),
            jsx: None,
            module_types: FxHashMap::default(),
            external: es("external").and_then(strings).unwrap_or_default(),
            treeshake: None,
            keep_names: None,
            output: Output::default(),
        };
        for (key, value) in &rolldown {
            match key.as_str() {
                // The scan and the bundle set their own input; the bridged
                // plugins reach the scan through the plugin host.
                "input" | "plugins" => {}
                "platform" => match value.as_str().and_then(|p| Platform::try_from(p).ok()) {
                    Some(p) => options.platform = p,
                    None => unsupported("rolldownOptions.platform"),
                },
                "resolve" => apply_resolve(&mut resolve, value),
                "transform" => options.apply_transform(value),
                "moduleTypes" => {
                    for (ext, ty) in value.as_object().into_iter().flatten() {
                        match ty.as_str().map(ModuleType::from_known_str) {
                            Some(Ok(ty)) => {
                                options.module_types.insert(ext.clone(), ty);
                            }
                            _ => unsupported(&format!("rolldownOptions.moduleTypes[{ext:?}]")),
                        }
                    }
                }
                "external" => match strings(value) {
                    Some(list) => options.external = list,
                    None => unsupported("rolldownOptions.external (only names)"),
                },
                "treeshake" => match value.as_bool() {
                    Some(b) => options.treeshake = Some(b),
                    None => unsupported("rolldownOptions.treeshake (only a boolean)"),
                },
                "keepNames" => options.keep_names = value.as_bool(),
                "output" => options.output = Output::from(value),
                other => unsupported(&format!("rolldownOptions.{other}")),
            }
        }
        options.resolve = resolve;
        options
    }

    fn apply_transform(&mut self, value: &Value) {
        for (key, v) in value.as_object().into_iter().flatten() {
            match key.as_str() {
                "define" => {
                    for (k, d) in v.as_object().into_iter().flatten() {
                        if let Some(d) = d.as_str() {
                            self.define.insert(k.clone(), d.to_string());
                        }
                    }
                }
                "target" => match strings(v) {
                    Some(t) => self.target = Either::Right(t),
                    None => unsupported("rolldownOptions.transform.target"),
                },
                "jsx" => self.jsx = jsx(v),
                other => unsupported(&format!("rolldownOptions.transform.{other}")),
            }
        }
    }

    /// The options a scan (`scan`) or a pre-bundle run starts from. The scan
    /// parses the app's own modules, so its JSX defaults to development; it
    /// never emits code, so it takes no define or target.
    pub fn bundler_options(&self, scan: bool) -> BundlerOptions {
        let jsx = self.jsx.clone().or_else(|| {
            scan.then(|| {
                Either::Right(JsxOptions {
                    runtime: Some("automatic".into()),
                    development: Some(true),
                    ..Default::default()
                })
            })
        });
        BundlerOptions {
            platform: Some(self.platform),
            resolve: Some(self.resolve.clone()),
            define: (!scan).then(|| self.define.clone()),
            transform: Some(rolldown_common::BundlerTransformOptions {
                target: (!scan).then(|| self.target.clone()),
                jsx,
                ..Default::default()
            }),
            module_types: Some(self.module_types.clone()),
            external: (!self.external.is_empty()).then(|| {
                IsExternal::StringOrRegex(
                    self.external
                        .iter()
                        .cloned()
                        .map(StringOrRegex::String)
                        .collect(),
                )
            }),
            treeshake: self
                .treeshake
                .map(TreeshakeOptions::Boolean)
                .unwrap_or_default(),
            // rolldown's own spelling is the output option; the top-level
            // key is accepted too.
            keep_names: self.output.keep_names.or(self.keep_names),
            banner: self
                .output
                .banner
                .clone()
                .map(|b| AddonOutputOption::String(Some(b))),
            footer: self
                .output
                .footer
                .clone()
                .map(|f| AddonOutputOption::String(Some(f))),
            minify: self.output.minify.map(RawMinifyOptions::Bool),
            legal_comments: self.output.legal_comments,
            // Vite's optimizer default (`output.topLevelVar ??= true`): a binding
            // read before its declaration is `undefined`, as under esbuild.
            top_level_var: (!scan).then(|| self.output.top_level_var.unwrap_or(true)),
            ..Default::default()
        }
    }
}

fn apply_resolve(resolve: &mut ResolveOptions, value: &Value) {
    let list = strings;
    let nested =
        |v: &Value| -> Option<Vec<Vec<String>>> { v.as_array()?.iter().map(strings).collect() };
    for (key, v) in value.as_object().into_iter().flatten() {
        let ok = match key.as_str() {
            "alias" => alias(v).map(|a| resolve.alias = Some(a)).is_some(),
            "aliasFields" => nested(v).map(|f| resolve.alias_fields = Some(f)).is_some(),
            "conditionNames" => list(v).map(|c| resolve.condition_names = Some(c)).is_some(),
            "exportsFields" => nested(v)
                .map(|f| resolve.exports_fields = Some(f))
                .is_some(),
            "extensions" => list(v).map(|e| resolve.extensions = Some(e)).is_some(),
            "mainFields" => list(v).map(|m| resolve.main_fields = Some(m)).is_some(),
            "mainFiles" => list(v).map(|m| resolve.main_files = Some(m)).is_some(),
            "modules" => list(v).map(|m| resolve.modules = Some(m)).is_some(),
            "symlinks" => v.as_bool().map(|s| resolve.symlinks = Some(s)).is_some(),
            _ => false,
        };
        if !ok {
            unsupported(&format!("rolldownOptions.resolve.{key}"));
        }
    }
}

impl From<&Value> for Output {
    fn from(value: &Value) -> Output {
        let mut out = Output::default();
        let empty = Map::new();
        for (key, v) in value.as_object().unwrap_or(&empty) {
            let ok = match key.as_str() {
                // The run owns where and how its files are written.
                "format" | "dir" | "entryFileNames" | "chunkFileNames" | "sourcemap" => true,
                "banner" => v.as_str().map(|b| out.banner = Some(b.into())).is_some(),
                "footer" => v.as_str().map(|f| out.footer = Some(f.into())).is_some(),
                "minify" => v.as_bool().map(|m| out.minify = Some(m)).is_some(),
                "keepNames" => v.as_bool().map(|k| out.keep_names = Some(k)).is_some(),
                "topLevelVar" => v.as_bool().map(|t| out.top_level_var = Some(t)).is_some(),
                "legalComments" => match v.as_str() {
                    Some("none") => {
                        out.legal_comments = Some(LegalComments::None);
                        true
                    }
                    Some("inline") => {
                        out.legal_comments = Some(LegalComments::Inline);
                        true
                    }
                    _ => false,
                },
                _ => false,
            };
            if !ok {
                unsupported(&format!("rolldownOptions.output.{key}"));
            }
        }
        out
    }
}
