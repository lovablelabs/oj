// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `oj build` for a TanStack Start app: the client bundle, the manifest that
//! points the server render at it, the server bundle (or, with
//! @cloudflare/vite-plugin, the Worker environment's bundle), the Node and
//! edge entrypoints around it, the public directory, and prerendered routes.
//!
//! Both bundles are fully self-contained (the framework's `#`-import seam
//! only resolves when the framework itself is bundled) and code-split (the
//! framework's server module uses top-level await, which one file cannot
//! always carry). They share one asset emitter, so the server render and the
//! browser agree on every `/assets/<hash>` URL.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rolldown::{BundlerBuilder, BundlerOptions, InputItem, OutputFormat, Platform, SourceMapType};
use rolldown_common::{AddonOutputOption, IsExternal};
use rolldown_plugin::__inner::{Pluginable, SharedPluginable};

use super::alias::AppAliasPlugin;
use super::assets::{AssetMode, Emitter, StartAssetsPlugin, Styles, WorkerBundler};
use super::cloudflare::{self, CloudflareWorkerPlugin};
use super::code::{CodeTarget, StartCodePlugin};
use super::errors::{unresolved_import_in, BundleError};
use super::facts::{fn_handle, js_regex, Facts};
use super::host::StartHostPlugin;
use super::hosts::{self, BundleHost};
use super::shims::NodeBuiltinShims;
use super::StartConfig;
use crate::build::EmitState;

/// The build options Vite applies to a Start app's bundles.
pub(crate) struct BuildOutput {
    /// `--out` / `build.outDir`.
    pub out_dir: PathBuf,
    pub sourcemap: oj_config::Sourcemap,
    pub minify: bool,
    /// `build.prerender` routes.
    pub prerender: Vec<String>,
}

fn sourcemap(s: oj_config::Sourcemap) -> Option<SourceMapType> {
    match s {
        oj_config::Sourcemap::Off => None,
        oj_config::Sourcemap::File => Some(SourceMapType::File),
        oj_config::Sourcemap::Inline => Some(SourceMapType::Inline),
        oj_config::Sourcemap::Hidden => Some(SourceMapType::Hidden),
    }
}

fn input(name: &str, path: &Path) -> InputItem {
    InputItem {
        name: Some(name.to_string()),
        import: path.to_string_lossy().into_owned(),
    }
}

/// The app's `external` for a client bundle (strings, RegExps, a function),
/// as Vite passes `build.rolldownOptions.external` to the build. `kept`, when
/// given, collects the specifiers the function answered external for.
pub(crate) fn client_external(
    facts: &Facts,
    host: Option<&Arc<oj_server::plugins::PluginHost>>,
    kept: Option<Arc<std::sync::Mutex<std::collections::HashSet<String>>>>,
) -> Option<IsExternal> {
    let spec = facts.external.clone()?;
    let items: Vec<serde_json::Value> = match spec {
        serde_json::Value::Array(items) => items,
        one => vec![one],
    };
    let host = host.cloned();
    let matchers: Vec<External> = items
        .iter()
        .filter_map(|v| match v {
            serde_json::Value::String(s) => Some(External::Name(s.clone())),
            other => match fn_handle(other) {
                Some(handle) => Some(External::Fn(handle)),
                None => js_regex(other).map(External::Regex),
            },
        })
        .collect();
    if matchers.is_empty() {
        return None;
    }
    let matchers = Arc::new(matchers);
    Some(IsExternal::Fn(Some(Arc::new(
        move |spec: &str, importer: Option<&str>, is_resolved: bool| {
            let matchers = Arc::clone(&matchers);
            let host = host.clone();
            let kept = kept.clone();
            let spec = spec.to_string();
            let importer = importer.map(str::to_string);
            Box::pin(async move {
                for m in matchers.iter() {
                    let hit = match m {
                        External::Name(name) => *name == spec,
                        External::Regex(re) => re.matches(&spec),
                        External::Fn(handle) => match &host {
                            Some(host) => {
                                host.call_start_fn(
                                    *handle,
                                    &serde_json::json!([spec, importer, is_resolved]),
                                )
                                .await
                                .map_err(|e| {
                                    anyhow::anyhow!("build.rolldownOptions.external: {e}")
                                })?
                                .as_bool()
                                .unwrap_or(false)
                                    && {
                                        if let Some(kept) = &kept {
                                            kept.lock()
                                                .unwrap_or_else(|p| p.into_inner())
                                                .insert(spec.clone());
                                        }
                                        true
                                    }
                            }
                            None => false,
                        },
                    };
                    if hit {
                        return Ok(true);
                    }
                }
                Ok(false)
            })
        },
    ))))
}

enum External {
    Name(String),
    Regex(rolldown_utils::js_regex::HybridRegex),
    Fn(u64),
}

/// Vite's `ssr.external` for the Start server bundle. The bundle takes its
/// dependencies in by default (dist/ is self-contained and worker-ready), so
/// `noExternal` has nothing left to do; a listed name (or every installed
/// package under `external: true`) stays a bare import resolved from
/// node_modules at run time, as Vite's ssrExternal leaves it out of the
/// server build. Only an unresolved bare specifier qualifies.
fn ssr_external(start: &StartConfig) -> Option<IsExternal> {
    let externals = oj_config::ssr_externals(&start.config);
    if externals.external.is_empty() && !externals.external_all {
        return None;
    }
    let names: Arc<std::collections::HashSet<String>> =
        Arc::new(externals.external.iter().cloned().collect());
    let all = externals.external_all;
    let node_modules = start.root.join("node_modules");
    Some(IsExternal::Fn(Some(Arc::new(
        move |spec: &str, _importer: Option<&str>, is_resolved: bool| {
            let bare = !spec.starts_with('.')
                && !spec.starts_with('/')
                && !spec.starts_with('\0')
                && !spec.contains(':');
            let ext = !is_resolved && bare && {
                let pkg = oj_config::SsrExternals::package_of(spec);
                !pkg.is_empty()
                    && (names.contains(pkg)
                        || (all && node_modules.join(pkg).join("package.json").is_file()))
            };
            Box::pin(async move { Ok(ext) })
        },
    ))))
}

/// The plugins every client-side bundle (the app's client, a worker entry)
/// runs, in Vite's order: the alias, the app's plugins, oj's source
/// rewrites, the asset conventions, the Node built-in stubs.
fn client_plugins(
    start: &StartConfig,
    host: Option<&BundleHost>,
    target: CodeTarget,
    styles: &Arc<Styles>,
    emitter: &Arc<Emitter>,
    emit: &Arc<EmitState>,
) -> Vec<SharedPluginable> {
    let mut plugins: Vec<SharedPluginable> = Vec::new();
    if let Some(plugin) = &start.stylex {
        plugins.push(crate::stylex_plugin::transform(plugin));
    }
    if let Some(h) = host {
        if !h.facts.alias.is_empty() {
            plugins.push(Pluginable::new_shared(AppAliasPlugin {
                entries: h.facts.alias.clone(),
            }));
        }
        plugins.push(Pluginable::new_shared(
            StartHostPlugin::new(
                Arc::clone(&h.host),
                h.ctx.clone(),
                &h.facts,
                start.command,
                &start.root,
                Arc::clone(emit),
            )
            .with_module_parsed(start.jsx_config()),
        ));
    }
    plugins.push(Pluginable::new_shared(
        StartCodePlugin::new(&start.root, target)
            .with_alias(host.map(|h| h.facts.alias.clone()).unwrap_or_default()),
    ));
    plugins.push(Pluginable::new_shared(StartAssetsPlugin::new(
        &start.root,
        Arc::clone(styles),
        AssetMode::Build {
            emitter: Arc::clone(emitter),
            server: false,
        },
    )));
    plugins.push(Pluginable::new_shared(NodeBuiltinShims {
        production: true,
    }));
    plugins
}

/// Turns a finished build into its outputs, failing on an import nothing
/// resolved as Vite does.
fn checked(output: rolldown::BundleOutput) -> anyhow::Result<rolldown::BundleOutput> {
    if let Some(err) = unresolved_import_in(&output.warnings) {
        return Err(anyhow::Error::new(err));
    }
    for warning in &output.warnings {
        let text = warning.to_diagnostic().to_string();
        // Vite's onRollupLog drops these: a `"use client"` / `"use server"`
        // directive means nothing to a client bundle.
        let directive = matches!(
            warning.kind(),
            rolldown_error::EventKind::ModuleLevelDirective
        ) && (text.contains("\"use client\"") || text.contains("\"use server\""));
        if directive || format!("{warning:?}").contains("SOURCEMAP_BROKEN") {
            continue;
        }
        eprintln!("oj build warning: {text}");
    }
    Ok(output)
}

/// The files the plugins emitted (`this.emitFile` assets), written into `dir`.
async fn write_emitted(host: &oj_server::plugins::PluginHost, dir: &Path) -> anyhow::Result<()> {
    let files = host
        .emitted_files()
        .await
        .map_err(|e| anyhow::anyhow!("plugin emitFile collection failed: {e}"))?;
    for file in files {
        let dest = dir.join(&file.file_name);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, file.source.as_bytes())?;
    }
    Ok(())
}

pub(crate) async fn build(
    start: StartConfig,
    config_file: Option<PathBuf>,
    out: BuildOutput,
) -> anyhow::Result<()> {
    let start = Arc::new(start);
    let dist = out.out_dir.clone();
    let client_dir = dist.join("client");
    match std::fs::remove_dir_all(&dist) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(anyhow::anyhow!("could not empty {}: {e}", dist.display())),
    }
    std::fs::create_dir_all(&client_dir)?;

    // TanStack Start's config hook sets Vite's `builder.sharedPlugins`, so
    // every environment builds on the same plugin instances: one host, moved
    // to the server environment once the client is written.
    let client_host = hosts::spawn(&start, config_file.as_deref(), "client").await?;
    // With @cloudflare/vite-plugin the server build is the Worker environment
    // the plugin declared (named from the wrangler config, or its
    // `viteEnvironment.name`); the plugin's hooks must see that environment.
    let cf_env = client_host
        .as_ref()
        .and_then(|h| h.facts.cloudflare.clone());
    let server_env_name = cf_env
        .as_ref()
        .map_or("ssr", |e| e.name.as_str())
        .to_string();
    let server_entry = start
        .server_entry()
        .unwrap_or_else(|| start.cache.join("server-entry.tsx"));
    let server_out_dir = match &cf_env {
        Some(env) => cloudflare::worker_out_dir(env, &start.root, &dist),
        None => dist.clone(),
    };

    let styles = Arc::new(Styles::new(&start));
    let emitter = Arc::new(Emitter::new(&client_dir, &start.base, Arc::clone(&styles)));
    let emit = Arc::new(EmitState::new(start.root.clone()));
    let empty_facts = Facts::default();
    let client_facts = client_host.as_ref().map_or(&empty_facts, |h| &h.facts);
    let client_defines = start.client_defines(
        client_host.as_ref().map_or(&[][..], |h| &h.env_delta),
        client_host.as_ref().map_or(&[][..], |h| &h.defines),
    );
    let client_aliases = start.client_aliases();
    let resolve_client = start.resolve_options(
        &client_aliases,
        start.client_main_fields(),
        Some(start.client_conditions()),
    );
    let minify = crate::build::rolldown_minify(out.minify);

    // `?worker` / `?worker&url` in a build: Vite's bundleWorkerEntry bundles
    // the worker entry on its own (the client's transform and define) with
    // `worker.plugins` alone, a fresh set per bundle with their whole
    // lifecycle (generate, no write): what their generateBundle emits ships
    // with the client. The bundle takes `worker.rolldownOptions` (never the
    // app's build options) and `worker.format`, iife by default: a classic
    // script whose `import.meta.url` is its own URL (vite:worker-post). The
    // entry is emitted where it is imported, its chunks beside it.
    let worker_options = client_facts.worker.clone();
    {
        let start = Arc::clone(&start);
        let styles = Arc::clone(&styles);
        let weak_emitter = Arc::downgrade(&emitter);
        let config_file = config_file.clone();
        let client_dir = client_dir.clone();
        let mut defines = client_defines.clone();
        if !worker_options.is_module() {
            defines.push(("import.meta.url".into(), "self.location.href".into()));
            defines.push(("import.meta".into(), "{ url: self.location.href }".into()));
        }
        let resolve = resolve_client.clone();
        let minify = minify.clone();
        let sourcemap_kind = out.sourcemap;
        let worker = worker_options.clone();
        let bundler: WorkerBundler = Arc::new(move |file: PathBuf| {
            let start = Arc::clone(&start);
            let styles = Arc::clone(&styles);
            let emitter = weak_emitter.upgrade();
            let config_file = config_file.clone();
            let client_dir = client_dir.clone();
            let defines = defines.clone();
            let resolve = resolve.clone();
            let minify = minify.clone();
            let worker = worker.clone();
            Box::pin(async move {
                let emitter = emitter.ok_or("the build is over")?;
                let host = hosts::spawn_worker(&start, config_file.as_deref(), &[&file])
                    .await
                    .map_err(|e| e.to_string())?;
                let result = bundle_worker(
                    &start,
                    host.as_ref(),
                    &file,
                    &styles,
                    &emitter,
                    &client_dir,
                    &worker,
                    WorkerBundleOptions {
                        defines,
                        resolve,
                        minify,
                        sourcemap: sourcemap_kind,
                    },
                )
                .await;
                if let Some(h) = &host {
                    let closed = h.host.close_bundle().await;
                    h.host.shutdown();
                    closed.map_err(|e| format!("plugin closeBundle failed:\n{e}"))?;
                }
                result
            })
        });
        emitter.set_worker_bundler(bundler, worker_options.is_module());
    }

    let build_all = async {
        // The client.
        let policy = match &client_host {
            Some(h) => Some(super::chunks::chunk_policy(&h.facts, &h.host, false)?),
            None => None,
        };
        let mut client = BundlerBuilder::default()
            .with_plugins(client_plugins(
                &start,
                client_host.as_ref(),
                CodeTarget::Client,
                &styles,
                &emitter,
                &emit,
            ))
            .with_options(BundlerOptions {
                input: Some(vec![input("client", &start.cache.join("client-entry.tsx"))]),
                cwd: Some(start.root.clone()),
                platform: Some(Platform::Browser),
                transform: Some(start.transform_options()),
                define: Some(client_defines.iter().cloned().collect()),
                resolve: Some(resolve_client.clone()),
                external: client_external(
                    client_facts,
                    client_host.as_ref().map(|h| &h.host),
                    None,
                ),
                dir: Some(client_dir.to_string_lossy().into_owned()),
                format: Some(OutputFormat::Esm),
                minify: Some(minify.clone()),
                sourcemap: sourcemap(out.sourcemap),
                entry_filenames: Some("assets/[name]-[hash].js".to_string().into()),
                chunk_filenames: Some("assets/[name]-[hash].js".to_string().into()),
                asset_filenames: Some("assets/[name]-[hash][extname]".to_string().into()),
                banner: Some(AddonOutputOption::String(Some(start.client_banner()))),
                code_splitting: policy.as_ref().and_then(|p| p.code_splitting.clone()),
                preserve_entry_signatures: Some(
                    policy
                        .as_ref()
                        .map_or(rolldown_common::PreserveEntrySignatures::False, |p| {
                            p.preserve_entry_signatures
                        }),
                ),
                experimental: policy.as_ref().and_then(|p| p.chunk_optimization).map(|o| {
                    rolldown_common::ExperimentalOptions {
                        chunk_optimization: Some(o),
                        ..Default::default()
                    }
                }),
                ..Default::default()
            })
            .build()
            .map_err(|errs| anyhow::Error::new(BundleError::from_diagnostics(&errs.into_vec())))?;
        let output = client
            .write()
            .await
            .map_err(|errs| bundle_failed(&emitter, errs.into_vec()))?;
        client
            .close()
            .await
            .map_err(|errs| anyhow::Error::new(BundleError::from_diagnostics(&errs.into_vec())))?;
        let output = checked(output)?;
        // Vite's buildEnvironment closes an environment's bundle as soon as
        // its output is written, before the next environment builds.
        if let Some(h) = &client_host {
            write_emitted(&h.host, &client_dir).await?;
            h.host
                .close_bundle()
                .await
                .map_err(|e| anyhow::anyhow!("plugin closeBundle failed:\n{e}"))?;
        }
        let entry = output
            .assets
            .iter()
            .find_map(|o| match o {
                rolldown_common::Output::Chunk(c) if c.is_entry => Some(c.filename.to_string()),
                _ => None,
            })
            .ok_or_else(|| anyhow::anyhow!("the client bundle has no entry chunk"))?;
        let client_url = format!("{}{entry}", start.base);

        // The server bundle bakes this manifest in (its alias maps the manifest
        // virtual to the same file), so it serves the shape every framework
        // version reads: 1.168+ the per-route `scripts`/`css`, 1.167 and earlier
        // the top-level `clientEntry` and css from per-route `assets`.
        // The StyleX plugin's sheet, linked after the client's own stylesheets:
        // the plugin appends it to the first one, which is where it lands in
        // the cascade, but that sheet is hashed and referenced by the time
        // every module's rules are known.
        if let Some(plugin) = &start.stylex {
            if let Some(sheet) = crate::stylex_plugin::stylesheet(plugin, &start.root)? {
                emitter
                    .generated_css("stylex.css", &sheet)
                    .map_err(|e| anyhow::anyhow!(e))?;
            }
        }
        let css = emitter.css_urls();
        let root_route = serde_json::json!({
            "preloads": [client_url],
            "css": css,
            "scripts": [{ "attrs": { "type": "module", "async": true, "src": client_url } }],
            "assets": css.iter().map(|href| serde_json::json!({ "tag": "link", "attrs": { "rel": "stylesheet", "href": href } })).collect::<Vec<_>>(),
        });
        std::fs::write(
            start.cache.join("manifest.ts"),
            format!(
                "export const tsrStartManifest = () => ({});\n",
                serde_json::json!({ "routes": { "__root__": root_route }, "clientEntry": client_url })
            ),
        )?;

        // The server, on the client's plugin instances.
        let server_host = match &client_host {
            Some(h) => Some(h.switch_environment(&server_env_name).await?),
            None => None,
        };
        let server_defines = start.server_defines(
            &server_env_name,
            server_host.as_ref().map_or(&[][..], |h| &h.env_delta),
            server_host.as_ref().map_or(&[][..], |h| &h.defines),
        );
        let server_plugins = || -> Vec<SharedPluginable> {
            let mut plugins: Vec<SharedPluginable> = Vec::new();
            if let Some(plugin) = &start.stylex {
                plugins.push(crate::stylex_plugin::transform(plugin));
            }
            if let Some(facts) = server_host
                .as_ref()
                .map(|h| &h.facts)
                .filter(|f| !f.alias.is_empty())
            {
                plugins.push(Pluginable::new_shared(AppAliasPlugin {
                    entries: facts.alias.clone(),
                }));
            }
            if let Some(h) = &server_host {
                plugins.push(Pluginable::new_shared(
                    StartHostPlugin::new(
                        Arc::clone(&h.host),
                        h.ctx.clone(),
                        &h.facts,
                        start.command,
                        &start.root,
                        Arc::clone(&emit),
                    )
                    .with_module_parsed(start.jsx_config()),
                ));
            }
            plugins.push(Pluginable::new_shared(
                StartCodePlugin::new(&start.root, CodeTarget::Server).with_alias(
                    server_host
                        .as_ref()
                        .map(|h| h.facts.alias.clone())
                        .unwrap_or_default(),
                ),
            ));
            plugins.push(Pluginable::new_shared(StartAssetsPlugin::new(
                &start.root,
                Arc::clone(&styles),
                AssetMode::Build {
                    emitter: Arc::clone(&emitter),
                    server: true,
                },
            )));
            plugins
        };
        let mut server_aliases = start.server_aliases();
        let worker_dir = match &cf_env {
            Some(env) => {
                let dir = server_out_dir.clone();
                server_aliases.push((
                    "@cloudflare/vite-plugin/server".into(),
                    start.cache.join("cf-server-worker.mjs"),
                ));
                let mut plugins: Vec<SharedPluginable> = Vec::new();
                if let Some(h) = &server_host {
                    plugins.push(Pluginable::new_shared(CloudflareWorkerPlugin::new(
                        Arc::clone(&h.host),
                        env.clone(),
                        server_entry.clone(),
                    )));
                }
                plugins.extend(server_plugins());
                plugins.extend(cloudflare::builtin_plugins(env));
                let mut conditions = env.conditions.clone();
                conditions.push("import".into());
                conditions.push(
                    if start.is_production() {
                        "production"
                    } else {
                        "development"
                    }
                    .into(),
                );
                let mut transform = start.transform_options();
                if let Some(target) = &env.target {
                    transform.target = Some(rolldown_common::Either::Left(target.clone()));
                }
                let mut worker = BundlerBuilder::default()
                    .with_plugins(plugins)
                    .with_options(BundlerOptions {
                        input: Some(vec![InputItem {
                            name: Some("index".into()),
                            import: cloudflare::WORKER_ENTRY.into(),
                        }]),
                        cwd: Some(start.root.clone()),
                        platform: Some(Platform::Neutral),
                        transform: Some(transform),
                        define: Some(server_defines.iter().cloned().collect()),
                        resolve: Some(
                            start.resolve_options(
                                &server_aliases,
                                Some(
                                    env.main_fields
                                        .clone()
                                        .unwrap_or_else(oj_resolver::default_server_main_fields),
                                ),
                                Some(conditions),
                            ),
                        ),
                        dir: Some(dir.to_string_lossy().into_owned()),
                        format: Some(OutputFormat::Esm),
                        minify: Some(minify.clone()),
                        sourcemap: sourcemap(out.sourcemap),
                        entry_filenames: Some("[name].js".to_string().into()),
                        chunk_filenames: Some("assets/[name]-[hash].js".to_string().into()),
                        asset_filenames: Some("assets/[name]-[hash][extname]".to_string().into()),
                        ..Default::default()
                    })
                    .build()
                    .map_err(|errs| {
                        anyhow::Error::new(BundleError::from_diagnostics(&errs.into_vec()))
                    })?;
                let output = worker
                    .write()
                    .await
                    .map_err(|errs| bundle_failed(&emitter, errs.into_vec()))?;
                worker.close().await.map_err(|errs| {
                    anyhow::Error::new(BundleError::from_diagnostics(&errs.into_vec()))
                })?;
                checked(output)?;
                Some(dir)
            }
            None => {
                server_aliases.push((
                    "@cloudflare/vite-plugin/server".into(),
                    start.cache.join("cf-server.mjs"),
                ));
                let mut server = BundlerBuilder::default()
                    .with_plugins(server_plugins())
                    .with_options(BundlerOptions {
                        input: Some(vec![input("server-bundle", &server_entry)]),
                        cwd: Some(start.root.clone()),
                        platform: Some(Platform::Node),
                        external: ssr_external(&start),
                        transform: Some(start.transform_options()),
                        define: Some(server_defines.iter().cloned().collect()),
                        resolve: Some(start.resolve_options(
                            &server_aliases,
                            Some(start.server_main_fields()),
                            None,
                        )),
                        dir: Some(dist.to_string_lossy().into_owned()),
                        format: Some(OutputFormat::Esm),
                        minify: Some(minify.clone()),
                        sourcemap: sourcemap(out.sourcemap),
                        entry_filenames: Some("[name].mjs".to_string().into()),
                        chunk_filenames: Some("chunks/[name]-[hash].mjs".to_string().into()),
                        banner: Some(AddonOutputOption::String(Some(
                            "import { createRequire as ___cr } from 'node:module'; const require = ___cr(import.meta.url || 'file:///worker.js');".into(),
                        ))),
                        ..Default::default()
                    })
                    .build()
                    .map_err(|errs| anyhow::Error::new(BundleError::from_diagnostics(&errs.into_vec())))?;
                let output = server
                    .write()
                    .await
                    .map_err(|errs| bundle_failed(&emitter, errs.into_vec()))?;
                server.close().await.map_err(|errs| {
                    anyhow::Error::new(BundleError::from_diagnostics(&errs.into_vec()))
                })?;
                checked(output)?;
                std::fs::write(dist.join("server.mjs"), node_server(&start.base))?;
                std::fs::write(dist.join("worker.mjs"), WORKER_MJS)?;
                std::fs::copy(
                    start.cache.join("cf-server.mjs"),
                    dist.join("cf-server.mjs"),
                )?;
                std::fs::write(dist.join("cf-loader.mjs"), CF_LOADER_MJS)?;
                None
            }
        };
        if let Some(h) = &server_host {
            write_emitted(&h.host, &server_out_dir).await?;
            h.host
                .close_bundle()
                .await
                .map_err(|e| anyhow::anyhow!("plugin closeBundle failed:\n{e}"))?;
        }

        if let Some(public_dir) = oj_config::public_dir(&start.config, &start.root) {
            copy_dir_over(&public_dir, &client_dir)?;
        }

        if !out.prerender.is_empty() {
            if worker_dir.is_some() {
                // The Worker bundle imports the runtime's own modules; it does not run under Node.
                eprintln!(
                    "{}: prerender skipped: the Cloudflare Worker bundle only runs in workerd",
                    oj_server::oj_brand()
                );
            } else {
                prerender(&start, &dist, &client_dir, &out.prerender).await?;
                eprintln!(
                    "{}: prerendered {} route(s)",
                    oj_server::oj_brand(),
                    out.prerender.len()
                );
            }
        }

        eprintln!(
            "{}: built dist (client {client_url})",
            oj_server::oj_brand()
        );
        if let (Some(dir), Some(env)) = (&worker_dir, &cf_env) {
            eprintln!(
                "{}: cloudflare worker \"{}\" -> {}/index.js (deploy: wrangler deploy)",
                oj_server::oj_brand(),
                env.name,
                dir.strip_prefix(&start.root).unwrap_or(dir).display()
            );
        }
        Ok::<(), anyhow::Error>(())
    };

    let Some(host) = &client_host else {
        return build_all.await;
    };
    // Vite's `vite build` is builder.buildApp: the plugins' buildApp hooks,
    // with oj's environment builds standing in for TanStack Start's config
    // builder, run once when a hook (or the builder's own turn) asks.
    let built = serde_json::json!({
        "client": {
            "outDir": client_dir.display().to_string(),
            "input": start.cache.join("client-entry.tsx").display().to_string(),
        },
        server_env_name.clone(): {
            "outDir": server_out_dir.display().to_string(),
            "input": if cf_env.is_some() {
                cloudflare::WORKER_ENTRY.to_string()
            } else {
                server_entry.display().to_string()
            },
        },
    })
    .to_string();
    let mut requests = host.ctx.build_requests();
    let app = host.host.build_app(&built);
    tokio::pin!(app);
    tokio::pin!(build_all);
    let mut result: Option<anyhow::Result<()>> = None;
    let app_result = loop {
        tokio::select! {
            r = &mut app => break r,
            Some(rpc) = requests.recv() => {
                if result.is_none() {
                    result = Some((&mut build_all).await);
                }
                let reply = match &result {
                    Some(Err(e)) => Err(format!("{e:#}")),
                    _ => Ok(serde_json::Value::Null),
                };
                host.host.rpc_reply(rpc.id, reply);
            }
        }
    };
    match result {
        Some(Err(e)) => Err(e),
        Some(Ok(())) => app_result.map_err(|e| anyhow::anyhow!("plugin buildApp failed:\n{e}")),
        None => {
            app_result.map_err(|e| anyhow::anyhow!("plugin buildApp failed:\n{e}"))?;
            build_all.await
        }
    }
}

/// A failed bundle's error: a worker bundle's own when one failed under it
/// (the parent bundle reports it as its own failed load of the worker).
fn bundle_failed(emitter: &Emitter, errs: Vec<rolldown_error::BuildDiagnostic>) -> anyhow::Error {
    match emitter.worker_error() {
        Some(e) => anyhow::anyhow!("{e}"),
        None => anyhow::Error::new(BundleError::from_diagnostics(&errs)),
    }
}

/// What a worker bundle takes from the client build.
struct WorkerBundleOptions {
    defines: Vec<(String, String)>,
    resolve: rolldown_common::ResolveOptions,
    minify: rolldown_common::RawMinifyOptions,
    sourcemap: oj_config::Sourcemap,
}

/// One worker entry's bundle (Vite's bundleWorkerEntry) on its own host's
/// `worker.plugins`: generated, its plugins' generateBundle and emitted files
/// applied, every output but the entry written under the client.
#[allow(clippy::too_many_arguments)]
async fn bundle_worker(
    start: &StartConfig,
    host: Option<&BundleHost>,
    file: &Path,
    styles: &Arc<Styles>,
    emitter: &Arc<Emitter>,
    client_dir: &Path,
    worker: &super::facts::WorkerOptions,
    options: WorkerBundleOptions,
) -> Result<super::assets::WorkerBundle, String> {
    let emit = Arc::new(EmitState::new(start.root.clone()));
    let plugins = client_plugins(
        start,
        host,
        CodeTarget::Worker {
            entry: file.to_path_buf(),
        },
        styles,
        emitter,
        &emit,
    );
    // The worker's own `rolldownOptions.external` (its function a handle on
    // the worker's host).
    let external = host.and_then(|h| {
        let facts = Facts {
            external: h.facts.worker.external.clone(),
            ..Facts::default()
        };
        client_external(&facts, Some(&h.host), None)
    });
    let name = |given: &Option<String>, default: &str| {
        given.clone().unwrap_or_else(|| default.to_string())
    };
    let format = if worker.is_module() {
        OutputFormat::Esm
    } else {
        OutputFormat::Iife
    };
    let mut bundler = BundlerBuilder::default()
        .with_plugins(plugins)
        .with_options(BundlerOptions {
            input: Some(vec![input("worker", file)]),
            cwd: Some(start.root.clone()),
            platform: Some(Platform::Browser),
            transform: Some(start.transform_options()),
            define: Some(options.defines.into_iter().collect()),
            resolve: Some(options.resolve),
            external,
            format: Some(format),
            minify: Some(options.minify),
            sourcemap: sourcemap(options.sourcemap),
            entry_filenames: Some(name(&worker.entry_file_names, "assets/[name]-[hash].js").into()),
            chunk_filenames: Some(name(&worker.chunk_file_names, "assets/[name]-[hash].js").into()),
            asset_filenames: Some(
                name(&worker.asset_file_names, "assets/[name]-[hash][extname]").into(),
            ),
            preserve_entry_signatures: Some(rolldown_common::PreserveEntrySignatures::False),
            ..Default::default()
        })
        .build()
        .map_err(|errs| BundleError::from_diagnostics(&errs.into_vec()).to_string())?;
    let output = bundler
        .generate()
        .await
        .map_err(|errs| BundleError::from_diagnostics(&errs.into_vec()).to_string())?;
    let _ = bundler.close().await;
    let output = checked(output).map_err(|e| e.to_string())?;
    if let Some(h) = host {
        write_emitted(&h.host, client_dir)
            .await
            .map_err(|e| e.to_string())?;
    }
    let write = |file_name: &str, bytes: &[u8]| -> Result<(), String> {
        let dest = client_dir.join(file_name);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&dest, bytes).map_err(|e| e.to_string())
    };
    let mut entry = None;
    for o in &output.assets {
        match o {
            rolldown_common::Output::Chunk(c) => {
                if let Some(map) = &c.map {
                    if !matches!(options.sourcemap, oj_config::Sourcemap::Inline) {
                        write(
                            &format!("{}.map", c.filename),
                            map.to_json_string().as_bytes(),
                        )?;
                    }
                }
                if c.is_entry {
                    entry = Some(super::assets::WorkerBundle {
                        code: c.code.clone(),
                        file_name: c.filename.to_string(),
                    });
                } else {
                    write(&c.filename, c.code.as_bytes())?;
                }
            }
            rolldown_common::Output::Asset(a) => write(&a.filename, a.source.as_bytes())?,
        }
    }
    entry.ok_or_else(|| format!("the worker bundle of {} has no entry chunk", file.display()))
}

/// `public/` copied over the client output (a file there wins, as Vite's
/// copyPublicDir writes after the bundle).
fn copy_dir_over(src: &Path, dest: &Path) -> anyhow::Result<()> {
    if !src.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if from.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_dir_over(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Renders each route with the built server bundle and writes the HTML as
/// `<route>/index.html` under the client output, served first by server.mjs.
async fn prerender(
    start: &StartConfig,
    dist: &Path,
    client_dir: &Path,
    routes: &[String],
) -> anyhow::Result<()> {
    let scripts = crate::start_host::ScriptEngine::new(&start.root)?;
    let mut env = vec![
        (
            "OJ_APP_ROOT".to_string(),
            start.root.to_string_lossy().into_owned(),
        ),
        (
            "OJ_SERVER_BUNDLE".to_string(),
            dist.join("server-bundle.mjs")
                .to_string_lossy()
                .into_owned(),
        ),
        (
            "OJ_CLIENT_DIR".to_string(),
            client_dir.to_string_lossy().into_owned(),
        ),
        (
            "OJ_PRERENDER_ROUTES".to_string(),
            serde_json::to_string(routes)?,
        ),
        ("NODE_ENV".to_string(), start.node_env().to_string()),
    ];
    env.extend(super::dotenv_process_vars(oj_env::get(), &start.app_env));
    scripts
        .run_async(&start.cache.join("prerender.mjs"), &env, "prerender")
        .await
}

/// `node dist/server.mjs`: a Node HTTP server around the bundle's handler.
/// It registers cf-loader.mjs first, a resolve hook that maps the framework's
/// runtime `import("@cloudflare/vite-plugin/server")` (which escapes the
/// build-time alias) to the bundled cf-server.mjs shim. A GET tries the exact
/// static file, then a prerendered `<path>/index.html`, before the handler;
/// `.../index.html` folds to `.../` so the router matches the directory route.
fn node_server(base: &str) -> String {
    NODE_SERVER_MJS.replace(
        "__OJ_BASE__",
        &serde_json::Value::String(base.to_string()).to_string(),
    )
}

const NODE_SERVER_MJS: &str = r#"import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { join, extname, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { register } from "node:module";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
register("./cf-loader.mjs", import.meta.url);
const handler = (await import("./server-bundle.mjs")).default;
const HERE = dirname(fileURLToPath(import.meta.url));
const CLIENT = join(HERE, "client");
const BASE = __OJ_BASE__;
const PORT = process.env.PORT || 3000;
const MIME = { ".html":"text/html; charset=utf-8", ".js":"text/javascript", ".css":"text/css", ".json":"application/json", ".wasm":"application/wasm", ".ico":"image/x-icon", ".png":"image/png", ".svg":"image/svg+xml", ".webp":"image/webp", ".avif":"image/avif", ".gif":"image/gif", ".jpg":"image/jpeg", ".jpeg":"image/jpeg", ".woff2":"font/woff2", ".woff":"font/woff", ".ttf":"font/ttf", ".otf":"font/otf", ".txt":"text/plain; charset=utf-8", ".xml":"application/xml", ".webmanifest":"application/manifest+json" };
// Request bodies stream into the handler and responses stream out (Start
// streams its dehydrated data into the HTML; binary bodies stay bytes), and
// every set-cookie header survives, as in the dev runner.
createServer(async (req, res) => {
  const url = new URL(req.url, "http://" + (req.headers.host || "localhost"));
  if (req.method === "GET") {
    const rel = (BASE !== "/" && url.pathname.startsWith(BASE) ? url.pathname.slice(BASE.length) : url.pathname).replace(/^\/+/, "");
    if (!rel.startsWith("_serverFn/")) {
      const candidates = [];
      if (rel) candidates.push(rel);
      candidates.push((rel ? rel.replace(/\/+$/, "") + "/" : "") + "index.html");
      for (const c of candidates) {
        try { const bytes = await readFile(join(CLIENT, c)); res.writeHead(200, { "content-type": MIME[extname(c)] || "application/octet-stream" }); res.end(bytes); return; } catch {}
      }
    }
  }
  const headers = new Headers();
  for (const [k, v] of Object.entries(req.headers)) {
    if (v == null) continue;
    if (Array.isArray(v)) for (const x of v) headers.append(k, x); else headers.set(k, v);
  }
  const init = { method: req.method, headers };
  if (req.method !== "GET" && req.method !== "HEAD") { init.body = Readable.toWeb(req); init.duplex = "half"; }
  if (url.pathname.endsWith("/index.html")) url.pathname = url.pathname.slice(0, -10);
  try {
    const response = await handler.fetch(new Request(url.href, init));
    const out = {};
    response.headers.forEach((v, k) => { if (k !== "set-cookie" && k !== "content-length") out[k] = v; });
    const cookies = response.headers.getSetCookie?.() ?? [];
    if (cookies.length) out["set-cookie"] = cookies;
    res.writeHead(response.status, out);
    if (response.body) await pipeline(Readable.fromWeb(response.body), res); else res.end();
  } catch (e) {
    if (!res.headersSent) res.writeHead(500, { "content-type": "text/plain; charset=utf-8" });
    res.end(String((e && e.stack) || e));
  }
}).listen(PORT, () => console.log("oj tanstack server on http://localhost:" + PORT));
"#;

/// The edge Web `fetch` handler (a Cloudflare Worker gets its bindings from
/// `cloudflare:workers`, so it needs neither the loader nor the shim).
const WORKER_MJS: &str = r#"import handler from "./server-bundle.mjs";
export default { async fetch(request, env, ctx) { return handler.fetch(request); } };
"#;

const CF_LOADER_MJS: &str = r#"export async function resolve(spec, ctx, next) {
  if (spec === "@cloudflare/vite-plugin/server")
    return { url: new URL("./cf-server.mjs", import.meta.url).href, shortCircuit: true };
  return next(spec, ctx);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_node_server_bakes_the_base_in_as_a_string() {
        let server = node_server("/app/");
        assert!(server.contains("const BASE = \"/app/\";"), "{server}");
        assert!(server.contains("register(\"./cf-loader.mjs\", import.meta.url);"));
        assert!(!server.contains("__OJ_BASE__"));
    }
}
