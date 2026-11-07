use std::path::PathBuf;

use serde_json::Value;

use crate::errors::{ErrorCode, StylexError};
use crate::eval::value::{EvalValue, JsObjectMap};
use crate::module_resolution::THEME_FILE_EXTENSION;

/// Raw `@stylexjs/babel-plugin` options as they appear in conformance jobs.
/// Fields hold unvalidated JSON; all typing happens in [`CompilerOptions::resolve`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompilerOptions {
    pub dev: Option<Value>,
    pub test: Option<Value>,
    pub debug: Option<Value>,
    pub class_name_prefix: Option<Value>,
    pub import_sources: Option<Value>,
    pub runtime_injection: Option<Value>,
    pub style_resolution: Option<Value>,
    pub property_validation_mode: Option<Value>,
    pub unstable_module_resolution: Option<Value>,
    pub treeshake_compensation: Option<Value>,
    pub sx_prop_name: Option<Value>,
    pub enable_debug_class_names: Option<Value>,
    pub enable_debug_data_prop: Option<Value>,
    pub enable_dev_class_names: Option<Value>,
    pub enable_font_size_px_to_rem: Option<Value>,
    pub enable_inlined_conditional_merge: Option<Value>,
    pub enable_media_query_order: Option<Value>,
    pub enable_minified_keys: Option<Value>,
    pub enable_legacy_value_flipping: Option<Value>,
    pub enable_logical_styles_polyfill: Option<Value>,
    pub enable_ltr_rtl_comments: Option<Value>,
    pub aliases: Option<Value>,
    pub rewrite_aliases: Option<Value>,
    pub debug_file_path: Option<Value>,
    pub env: Option<Value>,
    pub include: Option<Value>,
    pub exclude: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StyleResolution {
    PropertySpecificity,
    ApplicationOrder,
    LegacyExpandShorthands,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PropertyValidationMode {
    Silent,
    Warn,
    Throw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ModuleResolutionType {
    CommonJs,
    Haste,
}

/// `custom` / `experimental_crossFileParsing` stay unsupported; `root_dir` is
/// always `None` under haste (see below).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleResolution {
    pub kind: ModuleResolutionType,
    pub root_dir: Option<PathBuf>,
    /// `themeFileExtension ?? '.stylex'`; gates hashing only, never hashed.
    pub theme_file_extension: String,
}

impl ModuleResolution {
    /// Upstream derives the consts suffix, it is never configured separately.
    pub fn consts_file_extension(&self) -> String {
        format!("{}.const", self.theme_file_extension)
    }
}

/// `aliases` in declaration order: the candidate list is consumed in order and
/// the first target that resolves on disk wins, so a map would lose the answer.
pub type AliasMap = Vec<(String, Vec<String>)>;
/// One resolved `importSources` entry. `Aliased` names the single named export
/// that carries the whole stylex namespace for that source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportSource {
    Plain(String),
    Aliased { from: String, as_name: String },
}

impl ImportSource {
    pub fn from_specifier(&self) -> &str {
        match self {
            ImportSource::Plain(s) => s,
            ImportSource::Aliased { from, .. } => from,
        }
    }
}

/// Resolved `runtimeInjection`: the module the inject call is imported from,
/// plus the named export to import (`None` = default import).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeInjection {
    pub from: String,
    pub as_name: Option<String>,
}

const DEFAULT_INJECT_PATH: &str = "@stylexjs/stylex/lib/stylex-inject";

// parity: babel-plugin src/utils/state-manager.js setOptions +
// src/shared/utils/default-options.js (0.19.0 ??-chains).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedOptions {
    pub class_name_prefix: String,
    pub dev: bool,
    pub debug: bool,
    pub test: bool,
    pub enable_debug_class_names: bool,
    pub enable_debug_data_prop: bool,
    pub enable_dev_class_names: bool,
    pub enable_font_size_px_to_rem: bool,
    pub enable_inlined_conditional_merge: bool,
    pub enable_media_query_order: bool,
    pub enable_minified_keys: bool,
    pub enable_legacy_value_flipping: bool,
    pub enable_logical_styles_polyfill: bool,
    pub enable_ltr_rtl_comments: bool,
    pub sx_prop_name: Option<String>,
    /// `Object.freeze({...options.env})`: always an object, injected into the
    /// evaluator as `stylex.env` / the `env` named import.
    pub env: EvalValue,
    pub import_sources: Vec<ImportSource>,
    pub runtime_injection: Option<RuntimeInjection>,
    pub style_resolution: StyleResolution,
    pub property_validation_mode: PropertyValidationMode,
    pub treeshake_compensation: bool,
    pub unstable_module_resolution: Option<ModuleResolution>,
    pub aliases: Option<AliasMap>,
    pub rewrite_aliases: bool,
    /// Canonical `{:?}` capture of every other field, computed once at
    /// resolve: cache/memo keys read it instead of reformatting per call.
    pub cache_repr: String,
}

impl ResolvedOptions {
    /// parity: `state.importSources.includes(source)` — the `.map(from)` list.
    pub fn is_import_source(&self, source: &str) -> bool {
        self.import_sources
            .iter()
            .any(|entry| entry.from_specifier() == source)
    }

    /// parity: `state.importAs(source)` — first object entry whose `from`
    /// matches; string entries are invisible to it.
    pub fn import_as(&self, source: &str) -> Option<&str> {
        self.import_sources.iter().find_map(|entry| match entry {
            ImportSource::Aliased { from, as_name } if from == source => Some(as_name.as_str()),
            _ => None,
        })
    }

    /// parity: `state.importSources[i]` after the `.map(from)` projection.
    pub fn import_source_at(&self, index: usize) -> Option<&str> {
        self.import_sources
            .get(index)
            .map(ImportSource::from_specifier)
    }
}

impl Default for ResolvedOptions {
    fn default() -> Self {
        CompilerOptions::default()
            .resolve()
            .expect("empty options always resolve")
    }
}

pub fn resolve_options(raw: &CompilerOptions) -> Result<ResolvedOptions, StylexError> {
    raw.resolve()
}

impl CompilerOptions {
    pub fn from_json(value: &Value) -> Result<Self, StylexError> {
        let mut opts = Self::default();
        let map = match value {
            Value::Null => return Ok(opts),
            Value::Object(map) => map,
            other => {
                return Err(StylexError::new(
                    ErrorCode::InvalidOptionValue,
                    format!(
                        "Expected the @stylexjs/babel-plugin options to be an object, but got `{}`.",
                        json_text(other)
                    ),
                ));
            }
        };
        for (key, v) in map {
            let slot = match key.as_str() {
                "dev" => &mut opts.dev,
                "test" => &mut opts.test,
                "debug" => &mut opts.debug,
                "classNamePrefix" => &mut opts.class_name_prefix,
                "importSources" => &mut opts.import_sources,
                "runtimeInjection" => &mut opts.runtime_injection,
                "styleResolution" => &mut opts.style_resolution,
                "propertyValidationMode" => &mut opts.property_validation_mode,
                "unstable_moduleResolution" => &mut opts.unstable_module_resolution,
                "treeshakeCompensation" => &mut opts.treeshake_compensation,
                "sxPropName" => &mut opts.sx_prop_name,
                "enableDebugClassNames" => &mut opts.enable_debug_class_names,
                "enableDebugDataProp" => &mut opts.enable_debug_data_prop,
                "enableDevClassNames" => &mut opts.enable_dev_class_names,
                "enableFontSizePxToRem" => &mut opts.enable_font_size_px_to_rem,
                "enableInlinedConditionalMerge" => &mut opts.enable_inlined_conditional_merge,
                "enableMediaQueryOrder" => &mut opts.enable_media_query_order,
                "enableMinifiedKeys" => &mut opts.enable_minified_keys,
                "enableLegacyValueFlipping" => &mut opts.enable_legacy_value_flipping,
                "enableLogicalStylesPolyfill" => &mut opts.enable_logical_styles_polyfill,
                "enableLTRRTLComments" => &mut opts.enable_ltr_rtl_comments,
                "aliases" => &mut opts.aliases,
                "rewriteAliases" => &mut opts.rewrite_aliases,
                "debugFilePath" => &mut opts.debug_file_path,
                "env" => &mut opts.env,
                "include" => &mut opts.include,
                "exclude" => &mut opts.exclude,
                // Inert upstream: state-manager.js hardcodes definedStylexCSSVariables to `{}`,
                // and useLayers is a processStylexRules config key the plugin never reads.
                "definedStylexCSSVariables" | "useLayers" => continue,
                _ => return Err(StylexError::unknown_option(key)),
            };
            *slot = Some(v.clone());
        }
        Ok(opts)
    }

    // Divergence from upstream: type-invalid values hard-error (InvalidOptionValue)
    // instead of logAndDefault's log-and-fall-back, except runtimeInjection below.
    pub fn resolve(&self) -> Result<ResolvedOptions, StylexError> {
        let dev = bool_opt("options.dev", &self.dev)?.unwrap_or(false);
        let debug = bool_opt("options.debug", &self.debug)?.unwrap_or(dev);
        let enable_debug_class_names = bool_opt(
            "options.enableDebugClassNames",
            &self.enable_debug_class_names,
        )?
        .unwrap_or(false);
        let enable_debug_data_prop =
            bool_opt("options.enableDebugDataProp", &self.enable_debug_data_prop)?.unwrap_or(debug);
        let enable_dev_class_names =
            bool_opt("options.enableDevClassNames", &self.enable_dev_class_names)?.unwrap_or(dev);

        let enable_font_size_px_to_rem = bool_opt(
            "options.enableFontSizePxToRem",
            &self.enable_font_size_px_to_rem,
        )?
        .unwrap_or(false);

        let enable_inlined_conditional_merge = bool_opt(
            "options.enableInlinedConditionalMerge",
            &self.enable_inlined_conditional_merge,
        )?
        .unwrap_or(true);
        let enable_minified_keys =
            bool_opt("options.enableMinifiedKeys", &self.enable_minified_keys)?.unwrap_or(true);
        let enable_media_query_order = bool_opt(
            "options.enableMediaQueryOrder",
            &self.enable_media_query_order,
        )?
        .unwrap_or(true);

        let enable_legacy_value_flipping = bool_opt(
            "options.enableLegacyValueFlipping",
            &self.enable_legacy_value_flipping,
        )?
        .unwrap_or(false);
        let enable_logical_styles_polyfill = bool_opt(
            "options.enableLogicalStylesPolyfill",
            &self.enable_logical_styles_polyfill,
        )?
        .unwrap_or(false);
        let enable_ltr_rtl_comments = bool_opt(
            "options.enableLTRRTLComments",
            &self.enable_ltr_rtl_comments,
        )?
        .unwrap_or(false);
        if enable_ltr_rtl_comments {
            return Err(StylexError::unsupported_option(
                "enableLTRRTLComments: true",
            ));
        }

        let sx_prop_name = match get(&self.sx_prop_name) {
            None => Some("sx".to_string()),
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Bool(false)) => None,
            Some(other) => {
                return Err(invalid_value(
                    "options.sxPropName",
                    other,
                    "a string or the literal false",
                ));
            }
        };

        let test = bool_opt("options.test", &self.test)?.unwrap_or(false);

        // The one logAndDefault shape reproduced here: a type-invalid value
        // (null and `{from}` included) falls back to off instead of throwing.
        let runtime_injection = match get(&self.runtime_injection) {
            Some(Value::Bool(true)) => Some(RuntimeInjection {
                from: DEFAULT_INJECT_PATH.to_string(),
                as_name: None,
            }),
            Some(Value::String(s)) => Some(RuntimeInjection {
                from: s.clone(),
                as_name: None,
            }),
            Some(Value::Object(map)) => match (map.get("from"), map.get("as")) {
                (Some(Value::String(from)), Some(Value::String(as_name))) => {
                    Some(RuntimeInjection {
                        from: from.clone(),
                        as_name: Some(as_name.clone()),
                    })
                }
                _ => None,
            },
            _ => None,
        };

        let class_name_prefix = match get(&self.class_name_prefix) {
            None => "x".to_string(),
            Some(Value::String(s)) => s.clone(),
            Some(other) => return Err(invalid_value("options.classNamePrefix", other, "a string")),
        };

        let mut import_sources = vec![
            ImportSource::Plain("@stylexjs/stylex".to_string()),
            ImportSource::Plain("stylex".to_string()),
        ];
        match get(&self.import_sources) {
            None => {}
            Some(Value::Array(items)) => {
                for item in items {
                    match item {
                        Value::String(s) => import_sources.push(ImportSource::Plain(s.clone())),
                        // Upstream's z.object strips keys outside the shape.
                        Value::Object(map) => match (map.get("from"), map.get("as")) {
                            (Some(Value::String(from)), Some(Value::String(as_name))) => {
                                import_sources.push(ImportSource::Aliased {
                                    from: from.clone(),
                                    as_name: as_name.clone(),
                                });
                            }
                            _ => {
                                return Err(invalid_value(
                                    "options.importSources[]",
                                    item,
                                    "a string or an object with string `from` and `as`",
                                ));
                            }
                        },
                        other => {
                            return Err(invalid_value(
                                "options.importSources[]",
                                other,
                                "a string or an object with string `from` and `as`",
                            ));
                        }
                    }
                }
            }
            Some(other) => return Err(invalid_value("options.importSources", other, "an array")),
        }

        let style_resolution = match get(&self.style_resolution) {
            None => StyleResolution::PropertySpecificity,
            Some(v) => match v.as_str() {
                Some("property-specificity") => StyleResolution::PropertySpecificity,
                Some("application-order") => StyleResolution::ApplicationOrder,
                Some("legacy-expand-shorthands") => StyleResolution::LegacyExpandShorthands,
                _ => {
                    return Err(invalid_value(
                        "options.styleResolution",
                        v,
                        "one of \"application-order\" | \"property-specificity\" | \"legacy-expand-shorthands\"",
                    ));
                }
            },
        };

        let property_validation_mode = match get(&self.property_validation_mode) {
            None => PropertyValidationMode::Silent,
            Some(v) => match v.as_str() {
                Some("silent") => PropertyValidationMode::Silent,
                Some("throw") => PropertyValidationMode::Throw,
                Some("warn") => PropertyValidationMode::Warn,
                _ => {
                    return Err(invalid_value(
                        "options.propertyValidationMode",
                        v,
                        "one of \"throw\" | \"warn\" | \"silent\"",
                    ));
                }
            },
        };

        let unstable_module_resolution =
            resolve_module_resolution(&self.unstable_module_resolution)?;

        let treeshake_compensation = bool_opt(
            "options.treeshakeCompensation",
            &self.treeshake_compensation,
        )?
        .unwrap_or(false);

        let env = match get(&self.env) {
            None => EvalValue::Obj(JsObjectMap::new().into()),
            Some(value @ Value::Object(_)) => EvalValue::from_json(value),
            Some(other) => return Err(invalid_value("options.env", other, "an object")),
        };
        let aliases = resolve_aliases(get(&self.aliases));
        // parity: `typeof options.rewriteAliases === 'boolean' ? … : false` —
        // a non-boolean reads as off with no diagnostic.
        let rewrite_aliases = get(&self.rewrite_aliases) == Some(&Value::Bool(true));
        if get(&self.debug_file_path).is_some() {
            return Err(StylexError::unsupported_option("debugFilePath"));
        }
        if get(&self.include).is_some() {
            return Err(StylexError::unsupported_option("include"));
        }
        if get(&self.exclude).is_some() {
            return Err(StylexError::unsupported_option("exclude"));
        }

        let mut resolved = ResolvedOptions {
            class_name_prefix,
            dev,
            debug,
            test,
            enable_debug_class_names,
            enable_debug_data_prop,
            enable_dev_class_names,
            enable_font_size_px_to_rem,
            enable_inlined_conditional_merge,
            enable_media_query_order,
            enable_minified_keys,
            enable_legacy_value_flipping,
            enable_logical_styles_polyfill,
            enable_ltr_rtl_comments,
            sx_prop_name,
            env,
            import_sources,
            runtime_injection,
            style_resolution,
            property_validation_mode,
            treeshake_compensation,
            unstable_module_resolution,
            aliases,
            rewrite_aliases,
            cache_repr: String::new(),
        };
        // The empty-field capture keeps the repr a pure function of the inputs.
        resolved.cache_repr = format!("{resolved:?}");
        Ok(resolved)
    }
}

/// The second reproduced `logAndDefault` (see `runtime_injection` above): one
/// malformed entry discards the whole map, valid siblings included.
fn resolve_aliases(raw: Option<&Value>) -> Option<AliasMap> {
    let raw = raw?;
    match parse_alias_map(raw) {
        Some(map) => Some(map),
        None => {
            eprintln!(
                "[fru] Expected (options.aliases) to be a map of string to string or \
                 string[], but got `{}`. Ignoring every alias.",
                json_text(raw)
            );
            None
        }
    }
}

fn parse_alias_map(raw: &Value) -> Option<AliasMap> {
    let entries: Vec<(String, &Value)> = match raw {
        // parity: z.objectOf only checks `typeof value === 'object'`, then
        // `for (const key in value)` — an array validates with index keys.
        Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), v)).collect(),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v))
            .collect(),
        _ => return None,
    };
    entries
        .into_iter()
        .map(|(key, value)| {
            let values = match value {
                Value::String(s) => vec![s.clone()],
                Value::Array(items) => items
                    .iter()
                    .map(|item| item.as_str().map(str::to_string))
                    .collect::<Option<Vec<String>>>()?,
                _ => return None,
            };
            Some((key, values))
        })
        .collect()
}

// parity: validate.js logAndDefault — upstream reports the bad value and falls
// back to the default rather than failing the build.
fn warn_and_default<T>(name: &str, value: &Value, expected: &str, default: T) -> T {
    eprintln!("[fru] Expected ({name}) to be {expected}, but got `{value}`; using the default",);
    default
}

fn resolve_module_resolution(raw: &Option<Value>) -> Result<Option<ModuleResolution>, StylexError> {
    let map = match get(raw) {
        None => return Ok(None),
        Some(Value::Object(map)) => map,
        Some(other) => {
            return Ok(warn_and_default(
                "options.unstable_moduleResolution",
                other,
                "an object",
                None,
            ));
        }
    };
    let ty = match map.get("type") {
        Some(Value::String(s)) => s.as_str(),
        _ => {
            return Ok(warn_and_default(
                "options.unstable_moduleResolution.type",
                map.get("type").unwrap_or(&Value::Null),
                "a string",
                None,
            ));
        }
    };
    let kind = match ty {
        "commonJS" => ModuleResolutionType::CommonJs,
        "haste" => ModuleResolutionType::Haste,
        "custom" | "experimental_crossFileParsing" => {
            return Err(StylexError::unsupported_option(&format!(
                "unstable_moduleResolution.type: \"{ty}\""
            )));
        }
        _ => {
            return Ok(warn_and_default(
                "options.unstable_moduleResolution.type",
                map.get("type").unwrap(),
                "one of \"commonJS\" | \"haste\" | \"custom\" | \"experimental_crossFileParsing\"",
                None,
            ));
        }
    };
    // parity: validate.js's object combinator iterates the SHAPE's keys and
    // builds a fresh result, so anything else in the map is simply dropped.
    let theme_file_extension = match map.get("themeFileExtension") {
        None | Some(Value::Null) => THEME_FILE_EXTENSION.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => {
            return Ok(warn_and_default(
                "options.unstable_moduleResolution.themeFileExtension",
                other,
                "a string",
                None,
            ));
        }
    };
    // Haste's validator shape has no rootDir, so whatever is passed is dropped
    // before any type check ever sees it.
    if kind == ModuleResolutionType::Haste {
        return Ok(Some(ModuleResolution {
            kind,
            root_dir: None,
            theme_file_extension,
        }));
    }
    let root_dir = match map.get("rootDir") {
        Some(Value::String(s)) => Some(PathBuf::from(s)),
        None | Some(Value::Null) => None,
        Some(other) => {
            return Err(invalid_value(
                "options.unstable_moduleResolution.rootDir",
                other,
                "a string",
            ));
        }
    };
    Ok(Some(ModuleResolution {
        kind,
        root_dir,
        theme_file_extension,
    }))
}

// Upstream `??` semantics: an explicit null reads as absent.
fn get(v: &Option<Value>) -> Option<&Value> {
    match v {
        Some(Value::Null) | None => None,
        Some(x) => Some(x),
    }
}

fn bool_opt(name: &str, v: &Option<Value>) -> Result<Option<bool>, StylexError> {
    match get(v) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err(invalid_value(name, other, "a boolean")),
    }
}

fn invalid_value(name: &str, got: &Value, expected: &str) -> StylexError {
    StylexError::new(
        ErrorCode::InvalidOptionValue,
        format!(
            "Expected ({name}) to be {expected}, but got `{}`.",
            json_text(got)
        ),
    )
}

fn json_text(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "<unserializable>".to_string())
}
