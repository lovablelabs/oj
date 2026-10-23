// SPDX-License-Identifier: MIT

//! An environment a plugin builds itself (Nitro's `nitro`), bundled when the
//! plugin's buildApp hook asks for it (`builder.build(environment)`), from
//! its resolved config alone, as Vite's buildEnvironment does: the input,
//! output options, resolve conditions and defines the environment declares,
//! its own plugin set in the shared host (`build.rolldownOptions.plugins`
//! included, as `vite:rollup-options-plugins` adds them), and oj's asset and
//! source conventions, which Vite applies to every environment.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use oj_server::plugins::PluginHost;
use rolldown::{BundlerBuilder, BundlerOptions, InputItem, OutputFormat, Platform};
use rolldown_common::{
    AddonOutputOption, ChunkFilenamesOutputOption, InjectImport, PreserveEntrySignatures,
    RawMinifyOptions, RollupPreRenderedChunk,
};
use rolldown_plugin::__inner::{Pluginable, SharedPluginable};
use serde_json::Value;

use super::alias::AppAliasPlugin;
use super::assets::{AssetMode, Emitter, StartAssetsPlugin, Styles};
use super::code::{CodeTarget, StartCodePlugin};
use super::errors::BundleError;
use super::facts::fn_handle;
use super::host::StartHostPlugin;
use super::hosts::BundleHost;
use super::StartConfig;
use crate::build::EmitState;

/// Builds environment `name` on the shared host and answers what Vite's
/// `builder.build` resolves to: the output (`{ output: [...] }`), each chunk
/// with its entry facts, as Nitro reads them to find its server entry.
pub(crate) async fn build(
    start: &StartConfig,
    host: &BundleHost,
    name: &str,
    styles: &Arc<Styles>,
    emitter: &Arc<Emitter>,
) -> anyhow::Result<Value> {
    let host = host.switch_environment(name).await?;
    let facts = host
        .host
        .environment_build_facts()
        .await
        .map_err(|e| anyhow::anyhow!("the {name} environment: {e}"))?;
    let options = EnvironmentOptions::from_json(start, name, &facts)?;
    let emit = Arc::new(EmitState::new(start.root.clone()));

    let mut plugins: Vec<SharedPluginable> = Vec::new();
    if !host.facts.alias.is_empty() {
        plugins.push(Pluginable::new_shared(AppAliasPlugin {
            entries: host.facts.alias.clone(),
        }));
    }
    plugins.push(Pluginable::new_shared(
        StartHostPlugin::new(
            Arc::clone(&host.host),
            host.ctx.clone(),
            &host.facts,
            start.command,
            &start.root,
            Arc::clone(&emit),
        )
        .with_module_parsed(start.jsx_config()),
    ));
    plugins.push(Pluginable::new_shared(StartCodePlugin::new(
        &start.root,
        CodeTarget::Server,
    )));
    plugins.push(Pluginable::new_shared(StartAssetsPlugin::new(
        &start.root,
        Arc::clone(styles),
        AssetMode::Build {
            emitter: Arc::clone(emitter),
            server: true,
        },
    )));

    let mut defines = start.server_defines(name, &host.env_delta, &host.defines);
    defines.extend(options.defines);
    let mut transform = start.transform_options();
    if let Some(target) = options.target {
        transform.target = Some(rolldown_common::Either::Left(target));
    }
    let policy = super::chunks::chunk_policy(&host.facts, &host.host, false)?;
    let mut bundler = BundlerBuilder::default()
        .with_plugins(plugins)
        .with_options(BundlerOptions {
            input: Some(options.input),
            cwd: Some(start.root.clone()),
            platform: Some(options.platform),
            transform: Some(transform),
            define: Some(defines.into_iter().collect()),
            inject: options.inject,
            resolve: Some(start.resolve_options(
                &[],
                Some(options.main_fields),
                Some(options.conditions),
            )),
            external: super::prod::client_external(&host.facts, Some(&host.host), None),
            dir: Some(options.dir.to_string_lossy().into_owned()),
            format: Some(options.format),
            minify: Some(options.minify),
            sourcemap: options.sourcemap,
            entry_filenames: Some(file_names(&host.host, options.entry_file_names)),
            chunk_filenames: Some(file_names(&host.host, options.chunk_file_names)),
            asset_filenames: Some(options.asset_file_names.into()),
            minify_internal_exports: options.minify_internal_exports,
            banner: options.banner.map(|s| AddonOutputOption::String(Some(s))),
            footer: options.footer.map(|s| AddonOutputOption::String(Some(s))),
            code_splitting: policy.code_splitting,
            preserve_entry_signatures: Some(match host.facts.preserve_entry_signatures {
                Some(_) => policy.preserve_entry_signatures,
                // Vite's server default.
                None => PreserveEntrySignatures::AllowExtension,
            }),
            ..Default::default()
        })
        .build()
        .map_err(|errs| anyhow::Error::new(BundleError::from_diagnostics(&errs.into_vec())))?;
    let output = bundler
        .write()
        .await
        .map_err(|errs| anyhow::Error::new(BundleError::from_diagnostics(&errs.into_vec())))?;
    bundler
        .close()
        .await
        .map_err(|errs| anyhow::Error::new(BundleError::from_diagnostics(&errs.into_vec())))?;
    let output = super::prod::checked(output)?;
    super::prod::write_emitted(&host.host, &options.dir).await?;
    host.host
        .close_bundle()
        .await
        .map_err(|e| anyhow::anyhow!("plugin closeBundle failed:\n{e}"))?;
    Ok(output_json(&output))
}

/// What a bundle's output is to the plugin that asked for it.
fn output_json(output: &rolldown::BundleOutput) -> Value {
    let items: Vec<Value> = output
        .assets
        .iter()
        .map(|o| match o {
            rolldown_common::Output::Chunk(c) => serde_json::json!({
                "type": "chunk",
                "fileName": c.filename.as_str(),
                "name": c.name.as_str(),
                "isEntry": c.is_entry,
                "isDynamicEntry": c.is_dynamic_entry,
                "facadeModuleId": c.facade_module_id.as_ref().map(|m| m.as_str()),
                "moduleIds": c.module_ids.iter().map(|m| m.as_str()).collect::<Vec<_>>(),
                "exports": c.exports.iter().map(|e| e.as_str()).collect::<Vec<_>>(),
            }),
            rolldown_common::Output::Asset(a) => serde_json::json!({
                "type": "asset",
                "fileName": a.filename.as_str(),
            }),
        })
        .collect();
    serde_json::json!({ "output": items })
}

/// An output file name option: a pattern, or a config function the host
/// calls with the pre-rendered chunk (Nitro names its chunks by route).
enum FileNames {
    Pattern(String),
    Fn(u64),
}

fn file_names(host: &Arc<PluginHost>, names: FileNames) -> ChunkFilenamesOutputOption {
    match names {
        FileNames::Pattern(p) => ChunkFilenamesOutputOption::String(p),
        FileNames::Fn(handle) => {
            let host = Arc::clone(host);
            ChunkFilenamesOutputOption::Fn(Arc::new(move |chunk: &RollupPreRenderedChunk| {
                let host = Arc::clone(&host);
                let chunk = serde_json::json!({
                    "type": "chunk",
                    "name": chunk.name.as_str(),
                    "isEntry": chunk.is_entry,
                    "isDynamicEntry": chunk.is_dynamic_entry,
                    "facadeModuleId": chunk.facade_module_id.as_ref().map(|m| m.as_str()),
                    "moduleIds": chunk.module_ids.iter().map(|m| m.as_str()).collect::<Vec<_>>(),
                    "exports": chunk.exports.iter().map(|e| e.as_str()).collect::<Vec<_>>(),
                });
                Box::pin(async move {
                    match host
                        .call_start_fn(handle, &serde_json::json!([chunk]))
                        .await
                        .map_err(|e| anyhow::anyhow!("output file names: {e}"))?
                    {
                        Value::String(name) => Ok(name),
                        other => anyhow::bail!("output file names: expected a string, got {other}"),
                    }
                })
            }))
        }
    }
}

/// The bundle options an environment's resolved config describes, with
/// Vite's build defaults for its consumer.
struct EnvironmentOptions {
    input: Vec<InputItem>,
    dir: PathBuf,
    platform: Platform,
    format: OutputFormat,
    conditions: Vec<String>,
    main_fields: Vec<String>,
    defines: Vec<(String, String)>,
    inject: Option<Vec<InjectImport>>,
    target: Option<String>,
    minify: RawMinifyOptions,
    sourcemap: Option<rolldown_common::SourceMapType>,
    entry_file_names: FileNames,
    chunk_file_names: FileNames,
    asset_file_names: String,
    minify_internal_exports: Option<bool>,
    banner: Option<String>,
    footer: Option<String>,
}

impl EnvironmentOptions {
    fn from_json(start: &StartConfig, name: &str, v: &Value) -> anyhow::Result<Self> {
        let text = |v: &Value| v.as_str().map(str::to_string);
        let strings = |v: &Value| -> Option<Vec<String>> {
            v.as_array().map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
        };
        let server = v["consumer"].as_str() != Some("client");
        let input = inputs(&v["input"]);
        if input.is_empty() {
            anyhow::bail!("the {name} environment has no build.rolldownOptions.input");
        }
        let output = &v["output"];
        let dir = text(&output["dir"])
            .or_else(|| text(&v["outDir"]))
            .unwrap_or_else(|| "dist".into());
        let dir = absolute(&start.root, &dir);
        let format = match output["format"].as_str() {
            Some("cjs" | "commonjs") => OutputFormat::Cjs,
            Some("iife") => OutputFormat::Iife,
            Some("umd") => OutputFormat::Umd,
            _ => OutputFormat::Esm,
        };
        let platform = match v["platform"].as_str() {
            Some("browser") => Platform::Browser,
            Some("neutral") => Platform::Neutral,
            Some("node") => Platform::Node,
            _ if server => Platform::Node,
            _ => Platform::Browser,
        };
        // Vite's `development|production` condition is the mode's.
        let mode = if start.is_production() {
            "production"
        } else {
            "development"
        };
        let mut conditions: Vec<String> = strings(&v["conditions"])
            .unwrap_or_else(|| vec!["module".into(), "node".into(), mode.into()])
            .into_iter()
            .map(|c| {
                if c == "development|production" {
                    mode.into()
                } else {
                    c
                }
            })
            .collect();
        if !conditions.iter().any(|c| c == "import") {
            conditions.push("import".into());
        }
        let main_fields = strings(&v["mainFields"])
            .filter(|m| !m.is_empty())
            .unwrap_or_else(oj_resolver::default_server_main_fields);
        // Vite's define: a string is the expression, anything else its JSON.
        let defines = v["define"]
            .as_object()
            .map(|d| {
                d.iter()
                    .filter(|(_, v)| !v.is_null())
                    .map(|(k, v)| {
                        let expr = v.as_str().map_or_else(|| v.to_string(), str::to_string);
                        (k.clone(), expr)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let minify = match &v["minify"] {
            Value::Bool(false) | Value::Null => RawMinifyOptions::DeadCodeEliminationOnly,
            Value::String(s) if s == "false" => RawMinifyOptions::DeadCodeEliminationOnly,
            _ => RawMinifyOptions::Bool(true),
        };
        let sourcemap = match &v["sourcemap"] {
            Value::Bool(true) => Some(rolldown_common::SourceMapType::File),
            Value::String(s) if s == "inline" => Some(rolldown_common::SourceMapType::Inline),
            Value::String(s) if s == "hidden" => Some(rolldown_common::SourceMapType::Hidden),
            _ => None,
        };
        let names = |v: &Value, default: String| match (v.as_str(), fn_handle(v)) {
            (Some(p), _) => FileNames::Pattern(p.to_string()),
            (None, Some(handle)) => FileNames::Fn(handle),
            _ => FileNames::Pattern(default),
        };
        let ext =
            if matches!(format, OutputFormat::Esm) && server && !package_type_module(&start.root) {
                "mjs"
            } else {
                "js"
            };
        Ok(Self {
            input,
            dir,
            platform,
            format,
            conditions,
            main_fields,
            defines,
            inject: injects(&v["inject"]),
            target: text(&v["target"]).filter(|t| t != "modules"),
            minify,
            sourcemap,
            entry_file_names: names(
                &output["entryFileNames"],
                if server {
                    format!("[name].{ext}")
                } else {
                    format!("assets/[name]-[hash].{ext}")
                },
            ),
            chunk_file_names: names(
                &output["chunkFileNames"],
                format!("assets/[name]-[hash].{ext}"),
            ),
            asset_file_names: text(&output["assetFileNames"])
                .unwrap_or_else(|| "assets/[name]-[hash][extname]".into()),
            minify_internal_exports: output["minifyInternalExports"].as_bool(),
            banner: text(&output["banner"]),
            footer: text(&output["footer"]),
        })
    }
}

/// `input` as Rollup takes it: one entry, a list, or named entries.
fn inputs(v: &Value) -> Vec<InputItem> {
    let item = |name: Option<String>, import: &str| InputItem {
        name,
        import: import.to_string(),
    };
    match v {
        Value::String(s) => vec![item(None, s)],
        Value::Array(a) => a
            .iter()
            .filter_map(Value::as_str)
            .map(|s| item(None, s))
            .collect(),
        Value::Object(o) => o
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| item(Some(k.clone()), s)))
            .collect(),
        _ => Vec::new(),
    }
}

/// `transform.inject` as @rollup/plugin-inject reads it: a module's default
/// (`"mod"`), a named export (`["mod", "name"]`) or its namespace
/// (`["mod", "*"]`), under each global name.
fn injects(v: &Value) -> Option<Vec<InjectImport>> {
    let entries = v.as_object()?;
    let out: Vec<InjectImport> = entries
        .iter()
        .filter_map(|(alias, spec)| match spec {
            Value::String(from) => Some(InjectImport::default(alias.clone(), from.clone())),
            Value::Array(a) => {
                let from = a.first()?.as_str()?.to_string();
                match a.get(1).and_then(Value::as_str) {
                    Some("*") => Some(InjectImport::namespace(alias.clone(), from)),
                    Some(imported) => Some(InjectImport::named(
                        imported.to_string(),
                        Some(alias.clone()),
                        from,
                    )),
                    None => Some(InjectImport::default(alias.clone(), from)),
                }
            }
            _ => None,
        })
        .collect();
    (!out.is_empty()).then_some(out)
}

fn absolute(root: &Path, p: &str) -> PathBuf {
    let p = Path::new(p);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

/// Vite names an ESM server bundle `.js` in a `"type": "module"` package and
/// `.mjs` otherwise.
fn package_type_module(root: &Path) -> bool {
    std::fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .is_some_and(|p| p["type"].as_str() == Some("module"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_take_every_rollup_shape() {
        assert_eq!(inputs(&serde_json::json!("a.mjs"))[0].import, "a.mjs");
        assert_eq!(inputs(&serde_json::json!(["a", "b"])).len(), 2);
        let named = inputs(&serde_json::json!({ "index": "/x/entry.mjs" }));
        assert_eq!(named[0].name.as_deref(), Some("index"));
        assert!(inputs(&Value::Null).is_empty());
    }

    #[test]
    fn injects_read_plugin_inject_entries() {
        let out = injects(&serde_json::json!({
            "process": "node:process",
            "Buffer": ["node:buffer", "Buffer"],
            "ns": ["mod", "*"],
        }))
        .unwrap();
        assert_eq!(out.len(), 3);
        assert!(out.iter().any(|i| matches!(i,
            InjectImport::Named { imported, alias: Some(a), from } if imported == "default" && a == "process" && from == "node:process")));
        assert!(out.iter().any(|i| matches!(i,
            InjectImport::Named { imported, alias: Some(a), .. } if imported == "Buffer" && a == "Buffer")));
        assert!(out.iter().any(|i| matches!(i, InjectImport::Namespace { alias, from } if alias == "ns" && from == "mod")));
        assert!(injects(&serde_json::json!({})).is_none());
    }
}
