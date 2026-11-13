// SPDX-License-Identifier: MIT

//! An environment a plugin builds itself (Nitro's `nitro`), bundled when the
//! plugin's buildApp hook asks for it (`builder.build(environment)`), from
//! its resolved config alone, as Vite's buildEnvironment does: the input,
//! output options, resolve conditions and defines the environment declares,
//! its own plugin set in the shared host (`build.rolldownOptions.plugins`
//! included, as `vite:rollup-options-plugins` adds them), and oj's asset and
//! source conventions, which Vite applies to every environment. Only a
//! server environment gets here, so Vite's server defaults fill the gaps.

use std::sync::Arc;

use oj_server::plugins::PluginHost;
use oj_server::rolldown_runtime::on_rolldown;
use rolldown::{BundlerBuilder, BundlerOptions, InputItem, OutputFormat, Platform};
use rolldown_common::{
    AddonOutputOption, ChunkFilenamesOutputOption, InjectImport, PreserveEntrySignatures,
    RawMinifyOptions, RollupPreRenderedChunk,
};
use serde_json::Value;

use super::assets::{Emitter, Styles};
use super::errors::BundleError;
use super::facts::fn_handle;
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
    let v = host
        .host
        .environment_build_facts()
        .await
        .map_err(|e| anyhow::anyhow!("the {name} environment: {e}"))?;
    let input = inputs(&v["input"]);
    if input.is_empty() {
        anyhow::bail!("the {name} environment has no build.rolldownOptions.input");
    }
    let output = &v["output"];
    let text = |v: &Value| v.as_str().map(str::to_string);
    let strings = |v: &Value| -> Option<Vec<String>> {
        v.as_array().map(|a| a.iter().filter_map(text).collect())
    };
    let dir = start.root.join(
        text(&output["dir"])
            .or_else(|| text(&v["outDir"]))
            .unwrap_or_else(|| "dist".into()),
    );
    let format = match output["format"].as_str() {
        Some("cjs" | "commonjs") => OutputFormat::Cjs,
        Some("iife") => OutputFormat::Iife,
        Some("umd") => OutputFormat::Umd,
        _ => OutputFormat::Esm,
    };
    let platform = match v["platform"].as_str() {
        Some("browser") => Platform::Browser,
        Some("neutral") => Platform::Neutral,
        _ => Platform::Node,
    };
    let conditions = oj_config::reported_conditions(
        strings(&v["conditions"]).unwrap_or_else(|| {
            ["module", "node", "development|production"]
                .map(String::from)
                .to_vec()
        }),
        !start.is_production(),
    );
    let main_fields = strings(&v["mainFields"])
        .filter(|m| !m.is_empty())
        .unwrap_or_else(oj_resolver::default_server_main_fields);
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

    let emit = Arc::new(EmitState::new(start.root.clone()));
    let mut paths_conditions = conditions.clone();
    paths_conditions.push("import".into());
    let plugins = super::prod::server_plugins(
        start,
        Some(&host),
        styles,
        emitter,
        &emit,
        None,
        Some(paths_conditions),
    );
    let mut defines = start.server_defines(name, &host.env_delta, &host.defines);
    defines.extend(oj_server::plugins::define_exprs(&v["define"]));
    let mut transform = start.transform_options();
    if let Some(target) = text(&v["target"]).filter(|t| t != "modules") {
        transform.target = Some(rolldown_common::Either::Left(target));
    }
    let policy = super::chunks::chunk_policy(&host.facts, &host.host, false)?;
    let mut bundler = BundlerBuilder::default()
        .with_plugins(plugins)
        .with_options(BundlerOptions {
            input: Some(input),
            cwd: Some(start.root.clone()),
            platform: Some(platform),
            transform: Some(transform),
            define: Some(defines.into_iter().collect()),
            tsconfig: oj_server::tsconfig_file(&start.config)
                .map(rolldown_common::TsConfig::Manual),
            inject: injects(&v["inject"]),
            resolve: Some(start.resolve_options(&[], Some(main_fields), Some(conditions))),
            external: super::prod::client_external(&host.facts, Some(&host.host), None),
            dir: Some(dir.to_string_lossy().into_owned()),
            format: Some(format),
            minify: Some(minify),
            sourcemap,
            entry_filenames: Some(file_names(
                &host.host,
                &output["entryFileNames"],
                "[name].mjs",
            )),
            chunk_filenames: Some(file_names(
                &host.host,
                &output["chunkFileNames"],
                "assets/[name]-[hash].mjs",
            )),
            asset_filenames: Some(
                text(&output["assetFileNames"])
                    .unwrap_or_else(|| "assets/[name]-[hash][extname]".into())
                    .into(),
            ),
            minify_internal_exports: output["minifyInternalExports"].as_bool(),
            banner: text(&output["banner"]).map(|s| AddonOutputOption::String(Some(s))),
            footer: text(&output["footer"]).map(|s| AddonOutputOption::String(Some(s))),
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
    let output = on_rolldown(&start.rolldown, bundler.write())
        .await
        .map_err(|errs| anyhow::Error::new(BundleError::from_diagnostics(&errs.into_vec())))?;
    on_rolldown(&start.rolldown, bundler.close())
        .await
        .map_err(|errs| anyhow::Error::new(BundleError::from_diagnostics(&errs.into_vec())))?;
    let output = super::prod::checked(output)?;
    super::prod::write_emitted(&host.host, &dir).await?;
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
fn file_names(host: &Arc<PluginHost>, v: &Value, default: &str) -> ChunkFilenamesOutputOption {
    let Some(handle) = fn_handle(v) else {
        return ChunkFilenamesOutputOption::String(v.as_str().unwrap_or(default).to_string());
    };
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
