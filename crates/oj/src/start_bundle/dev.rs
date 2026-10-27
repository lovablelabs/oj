// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The dev server's client bundle, in process on oj's rolldown. One plugin
//! host serves the whole session, as Vite keeps one plugin container per
//! server (buildStart once, plugin state across rebundles); a fresh host per
//! rebundle would rerun buildStart, and a plugin writing a generated input
//! from it would feed its own write back as the next rebundle forever.
//!
//! A bundle leaves its outputs in the Start cache directory, the contract the
//! dev server serves and gates from: `client-chunks/` with
//! `client-chunks.json`, `client-entry.modules`, `closure.json` (every file
//! the bundle read, the rebundle gate's input set and the bundle cache's
//! key), `glob-scopes.json`, `manifest.ts`, `css-urls.json`, and the
//! `client-error.json` / `client-hmr.json` records of how the run went.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use oj_compiler::glob::GlobCall;
use oj_server::rolldown_runtime::on_rolldown;
use rolldown::{BundlerBuilder, BundlerOptions, InputItem, OutputFormat, Platform};
use rolldown_common::AddonOutputOption;
use rolldown_plugin::__inner::{Pluginable, SharedPluginable};

use super::alias::AppAliasPlugin;
use super::assets::{AssetMode, DevStyleState, StartAssetsPlugin, Styles};
use super::code::{CodeTarget, StartCodePlugin};
use super::errors::{unresolved_import_in, BundleError};
use super::host::StartHostPlugin;
use super::hosts::{self, BundleHost};
use super::hot::{EngineSlot, HotSession, SessionParts};
use super::refresh::{RefreshPreamble, RefreshTransform, RefreshWrapper};
use super::shims::NodeBuiltinShims;
use super::StartConfig;
use crate::build::EmitState;

pub(crate) const CLIENT_ERROR_FILE: &str = "client-error.json";
pub(crate) const CLIENT_HMR_FILE: &str = "client-hmr.json";
/// The path the dev server serves the client entry chunk at, under `base`.
const CLIENT_ENTRY_PATH: &str = "/@oj-start/client-entry.js";

pub(crate) struct DevBundler {
    start: Arc<StartConfig>,
    config_file: Option<PathBuf>,
    host: tokio::sync::OnceCell<Option<BundleHost>>,
    /// The dev server's plugin host, once it is up (`Some(None)`: it runs
    /// none), whose client environment the bundle runs in. Without one (the
    /// tests) the bundle spawns a host of its own.
    dev_host:
        Option<tokio::sync::watch::Receiver<Option<Option<Arc<oj_server::plugins::PluginHost>>>>>,
    styles: Arc<Styles>,
    /// The hot-update engine, while one runs; one bundle at a time holds it
    /// (rebundles and output refreshes share the cache directory).
    session: tokio::sync::Mutex<Option<HotSession>>,
    engine: EngineSlot,
    /// The bundler's hot updates on their way to the pages.
    pub(crate) hot: Arc<crate::start_hot::HotUpdates>,
    /// Whether pages may take hot updates: not while the editor's HMR gate
    /// holds updates, where every edit reloads.
    hot_enabled: std::sync::atomic::AtomicBool,
    /// Bumped by every batch that touched a style input.
    style_epoch: std::sync::atomic::AtomicU64,
    /// Pages that announced their runtime id.
    clients: Mutex<HashSet<String>>,
    /// Specifiers the app's `external` function kept external, so the dev
    /// bundle's unresolved-import check takes the function's own answers.
    fn_externals: Arc<Mutex<HashSet<String>>>,
    /// The module ids of the last bundle written, whose StyleX rules a cached
    /// copy of it carries.
    bundle_modules: Mutex<Vec<String>>,
}

impl DevBundler {
    pub(crate) fn new(start: StartConfig, config_file: Option<PathBuf>) -> DevBundler {
        let styles = Arc::new(Styles::new(&start));
        let hot = Arc::new(crate::start_hot::HotUpdates::new(&start.dev_url_base()));
        // The server-side manifest (manifest-dev.ts) names the client entry
        // from here, under the dev `base`.
        let entry = format!("{}{CLIENT_ENTRY_PATH}", start.dev_url_base());
        let _ = std::fs::create_dir_all(&start.cache);
        let _ = std::fs::write(
            start.cache.join("client-entry.json"),
            serde_json::Value::String(entry).to_string(),
        );
        DevBundler {
            start: Arc::new(start),
            config_file,
            host: tokio::sync::OnceCell::new(),
            dev_host: None,
            styles,
            session: tokio::sync::Mutex::new(None),
            engine: EngineSlot::default(),
            hot,
            hot_enabled: std::sync::atomic::AtomicBool::new(!oj_env::get().knobs.hmr_gate),
            style_epoch: std::sync::atomic::AtomicU64::new(0),
            clients: Mutex::new(HashSet::new()),
            fn_externals: Arc::default(),
            bundle_modules: Mutex::new(Vec::new()),
        }
    }

    /// Runs the bundle's plugins on the dev server's host (see `dev_host`).
    pub(crate) fn on_dev_host(
        mut self,
        dev_host: tokio::sync::watch::Receiver<Option<Option<Arc<oj_server::plugins::PluginHost>>>>,
    ) -> DevBundler {
        self.dev_host = Some(dev_host);
        self
    }

    #[cfg(test)]
    pub(crate) fn for_tests(root: &Path) -> DevBundler {
        DevBundler::new(tests::start_at(root), None)
    }

    /// The module ids of the last bundle written.
    pub(crate) fn bundle_modules(&self) -> Vec<String> {
        self.bundle_modules.lock().unwrap().clone()
    }

    /// The app's StyleX plugin, whose rules a cached bundle must carry.
    pub(crate) fn stylex(&self) -> Option<&std::sync::Arc<oj_stylex::plugin::StylexPlugin>> {
        self.start.stylex.as_ref()
    }

    /// Vite's `base`, normalized (`/`, `/x/` or `./`).
    pub(crate) fn base(&self) -> &str {
        &self.start.base
    }

    pub(crate) fn root(&self) -> &Path {
        &self.start.root
    }

    pub(crate) fn set_hot_enabled(&self, enabled: bool) {
        self.hot_enabled
            .store(enabled, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn hot_enabled(&self) -> bool {
        self.hot_enabled.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(crate) fn style_epoch(&self) -> u64 {
        self.style_epoch.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// A watch batch's changed paths: any style input moves the style epoch.
    pub(crate) fn note_changes(&self, paths: &[PathBuf]) {
        // Any edit can change a sheet (Tailwind reads the sources).
        self.styles.dev_sheets_stale();
        if paths.iter().any(|p| crate::start_dev::is_style_input(p)) {
            self.style_epoch
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// A page announced its runtime id: the engine addresses its patches.
    pub(crate) fn page_connected(&self, id: &str) {
        self.clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.to_string());
        if let Some(engine) = self.engine.get() {
            let id = id.to_string();
            tokio::spawn(async move { engine.register_client(id).await });
        }
    }

    pub(crate) fn page_gone(&self, id: &str) {
        self.clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
        if let Some(engine) = self.engine.get() {
            let id = id.to_string();
            tokio::spawn(async move { engine.remove_client(&id).await });
        }
    }

    /// A page ran a patch (Vite's payloadDeliveredAck).
    pub(crate) fn patch_delivered(&self, filename: &str) {
        if let Some(engine) = self.engine.get() {
            let filename = filename.to_string();
            tokio::spawn(async move { engine.notify_payload_delivered(&filename).await });
        }
    }

    fn pages(&self) -> Vec<String> {
        self.clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    /// The sheet the current bundle compiled for `path` through the client
    /// plugins (see [`Styles::dev_sheet`]).
    pub(crate) fn linked_sheet(&self, path: &Path) -> Option<String> {
        self.styles.dev_sheet(path)
    }

    /// The same for a plugin-served sheet whose `load` returns `loaded`.
    pub(crate) fn linked_served_sheet(&self, id: &Path, loaded: &str) -> Option<String> {
        self.styles.dev_served_sheet(id, loaded)
    }

    /// The client environment's plugin host, spawned here when no bundle ran
    /// yet (one restored from cache): a page's `<link>` still goes through
    /// the client plugins.
    pub(crate) async fn plugin_host(&self) -> Option<Arc<oj_server::plugins::PluginHost>> {
        self.host()
            .await
            .ok()
            .flatten()
            .map(|h| Arc::clone(&h.host))
    }

    /// The session's plugin container, opened by the first bundle.
    async fn host(&self) -> anyhow::Result<Option<&BundleHost>> {
        self.host
            .get_or_try_init(|| async {
                let Some(dev_host) = &self.dev_host else {
                    return hosts::spawn(&self.start, self.config_file.as_deref(), "client").await;
                };
                let mut dev_host = dev_host.clone();
                // A server build that failed before its host came up drops the sender.
                let dev = match dev_host.wait_for(Option::is_some).await {
                    Ok(dev) => dev.clone().flatten(),
                    Err(_) => None,
                };
                match dev {
                    Some(dev) => hosts::open(&dev, "client").await.map(Some),
                    None => Ok(None),
                }
            })
            .await
            .map(Option::as_ref)
    }

    /// One client bundle, from a blocking context (the dev server's bundle
    /// workers run on blocking threads). `changed`: the batch's paths;
    /// `refresh`: the refresher bringing the full output current. Ok(true)
    /// when every change already reached the pages as a hot update.
    pub(crate) fn bundle(&self, changed: &[PathBuf], refresh: bool) -> anyhow::Result<bool> {
        tokio::runtime::Handle::current().block_on(self.bundle_async(changed, refresh))
    }

    pub(crate) async fn bundle_async(
        &self,
        changed: &[PathBuf],
        refresh: bool,
    ) -> anyhow::Result<bool> {
        let mut session = self.session.lock().await;
        self.styles.dev_sheets_stale();
        let cache = &self.start.cache;
        // Only this run's failure may read as an app build error.
        let _ = std::fs::remove_file(cache.join(CLIENT_ERROR_FILE));
        let _ = std::fs::remove_file(cache.join(CLIENT_HMR_FILE));
        let outcome = match self.host().await {
            Ok(host) => self.run(&mut session, host, changed, refresh).await,
            Err(e) => Err(BundleError::from_message(format!("{e:#}"))),
        };
        match outcome {
            Ok(Run::Hot { css }) => {
                write_json(
                    &cache.join(CLIENT_HMR_FILE),
                    &serde_json::json!({ "handled": true, "css": css }),
                )?;
                Ok(true)
            }
            Ok(Run::Output { files }) => {
                write_json(
                    &cache.join(CLIENT_HMR_FILE),
                    &serde_json::json!({ "handled": false }),
                )?;
                eprintln!("{}: client bundled ({files} files)", oj_server::oj_brand());
                Ok(false)
            }
            Err(err) => {
                let record = serde_json::to_string(&err.payload())?;
                std::fs::write(cache.join(CLIENT_ERROR_FILE), record)?;
                Err(anyhow::anyhow!("client entry bundling failed: {err}"))
            }
        }
    }

    async fn run(
        &self,
        session: &mut Option<HotSession>,
        host: Option<&BundleHost>,
        changed: &[PathBuf],
        refresh: bool,
    ) -> Result<Run, BundleError> {
        let start = &self.start;
        // Vite's bundled dev runs only with HMR on: never for a production
        // client (`NODE_ENV=production oj dev`) or `server.hmr: false`, nor
        // under the editor's HMR gate (every edit reloads there, so pages
        // never take a patch).
        let hot =
            self.hot_enabled() && !start.is_production() && host.is_none_or(|h| h.facts.server_hmr);
        let epoch = self.style_epoch();
        // A style input moved: CSS modules and preprocessor sheets the
        // engine compiled predate the epoch, and a partial is no module of
        // its graph at all. A fresh engine compiles them anew. One that
        // compiled no sheet keeps running.
        let stale = session.as_ref().is_some_and(|s| {
            !hot || (s.epoch != epoch
                && s.styles
                    .compiled_sheets
                    .load(std::sync::atomic::Ordering::SeqCst))
        });
        if stale {
            if let Some(old) = session.take() {
                old.close(&self.engine).await;
            }
        }
        if !hot {
            eprintln!("{}: client bundling", oj_server::oj_brand());
            return self.build(host).await.map(|files| Run::Output { files });
        }
        if let Some(live) = session.as_mut() {
            live.epoch = epoch;
            if !refresh {
                let pages = !self.pages().is_empty();
                if live.settle(changed, pages).await? {
                    let css = live.css_urls.lock().unwrap().urls();
                    let css = super::hot::link_new_styles(&start.cache, &css)
                        .map_err(|e| BundleError::from_message(format!("css-urls.json: {e}")))?;
                    return Ok(Run::Hot { css });
                }
            }
        } else {
            eprintln!("{}: client bundling", oj_server::oj_brand());
            let parts = self.parts(host, true)?;
            let runtime = std::fs::read_to_string(start.cache.join("hmr-runtime.js"))
                .map_err(|e| BundleError::from_message(format!("hmr-runtime.js: {e}")))?;
            let declared = declared_externals(host, &self.fn_externals);
            let live = HotSession::start(
                parts,
                &start.root,
                &start.rolldown,
                runtime,
                host.map_or(Some(&serde_json::Value::Null), |h| {
                    h.facts.server_watch.as_ref()
                }),
                epoch,
                Arc::clone(&self.hot),
                declared,
                &self.engine,
                || self.pages(),
            )
            .await?;
            *session = Some(live);
        }
        let live = session.as_ref().expect("a live session");
        let output = live.latest_output().await?;
        let module_ids: Vec<String> = live
            .engine
            .get_module_ids()
            .iter()
            .map(|id| id.to_string())
            .collect();
        *self.bundle_modules.lock().unwrap() = module_ids.clone();
        let watch_files = match host {
            Some(h) => h.host.watch_files().await.unwrap_or_default(),
            None => Vec::new(),
        };
        let css_urls = live.css_urls.lock().unwrap().in_bundle_order(&output);
        let scopes = live.glob_scopes.lock().unwrap().clone();
        write_outputs(
            start,
            &output,
            module_ids
                .iter()
                .chain(watch_files.iter())
                .map(String::as_str),
            &css_urls,
            &scopes,
            true,
        )
        .map_err(|e| BundleError::from_message(format!("writing the client bundle: {e}")))?;
        Ok(Run::Output {
            files: output.len(),
        })
    }

    /// The client bundle's plugins and options: `hot` adds React Fast
    /// Refresh, as @vitejs/plugin-react does for Vite's bundled dev (only for
    /// an app that configures the plugin).
    fn parts(&self, host: Option<&BundleHost>, hot: bool) -> Result<SessionParts, BundleError> {
        let start = &self.start;
        let emit = Arc::new(EmitState::new(start.root.clone()));
        let css_urls = Arc::new(Mutex::new(super::assets::LinkedSheets::default()));
        let glob_scopes = Arc::new(Mutex::new(Vec::new()));
        let styles = Arc::new(DevStyleState::default());
        let refresh = hot && host.is_some_and(|h| h.facts.react_refresh);
        let entry = start.cache.join("client-entry.tsx");
        let mut plugins: Vec<SharedPluginable> = Vec::new();
        if let Some(plugin) = &start.stylex {
            plugins.push(crate::stylex_plugin::transform(plugin));
        }
        if refresh {
            plugins.push(Pluginable::new_shared(RefreshPreamble {
                runtime: start.cache.join("react-refresh-runtime.js"),
                entry: entry.clone(),
            }));
        }
        if let Some(h) = host {
            if !h.facts.alias.is_empty() {
                plugins.push(Pluginable::new_shared(AppAliasPlugin {
                    entries: h.facts.alias.clone(),
                }));
            }
            plugins.push(Pluginable::new_shared(StartHostPlugin::new(
                Arc::clone(&h.host),
                h.ctx.clone(),
                &h.facts,
                start.command,
                &start.root,
                Arc::clone(&emit),
                start.jsx_config(),
            )));
        }
        plugins.push(Pluginable::new_shared(
            StartCodePlugin::new(&start.root, CodeTarget::Client)
                .with_alias(host.map(|h| h.facts.alias.clone()).unwrap_or_default())
                .recording_globs(Arc::clone(&glob_scopes)),
        ));
        plugins.push(Pluginable::new_shared(
            StartAssetsPlugin::new(
                &start.root,
                Arc::clone(&self.styles),
                AssetMode::Dev {
                    css_urls: Arc::clone(&css_urls),
                    state: Arc::clone(&styles),
                    url_base: start.dev_url_base(),
                },
            )
            .with_plugins(host.map(|h| Arc::clone(&h.host))),
        ));
        plugins.push(Pluginable::new_shared(NodeBuiltinShims {
            production: false,
        }));
        if refresh {
            let jsx = oj_config::jsx_settings(&start.config);
            let jsx = oj_compiler::JsxConfig {
                runtime: jsx.runtime,
                import_source: jsx.import_source,
                pragma: jsx.pragma,
                pragma_frag: jsx.pragma_frag,
            };
            plugins.push(Pluginable::new_shared(RefreshWrapper::new(
                jsx.import_source.as_deref().unwrap_or("react"),
            )));
            // After every other transform, as Vite's oxc plugin sits after the
            // app's plugins (rolldown runs transform hooks in plugin order,
            // and the wrapper reads the refresh registrations this emits).
            plugins.insert(
                plugins.len() - 1,
                Pluginable::new_shared(RefreshTransform { jsx }),
            );
        }

        let policy = match host {
            Some(h) => Some(
                super::chunks::chunk_policy(&h.facts, &h.host, true)
                    .map_err(|e| BundleError::from_message(format!("{e:#}")))?,
            ),
            None => None,
        };
        let defines = start.client_defines(
            host.map_or(&[][..], |h| &h.env_delta),
            host.map_or(&[][..], |h| &h.defines),
        );
        let options = BundlerOptions {
            input: Some(vec![InputItem {
                name: Some("client-entry".into()),
                import: entry.to_string_lossy().into_owned(),
            }]),
            cwd: Some(start.root.clone()),
            platform: Some(Platform::Browser),
            transform: Some(start.transform_options()),
            define: Some(defines.into_iter().collect()),
            resolve: Some(start.resolve_options(
                &start.client_aliases(),
                start.client_main_fields(),
                Some(start.client_conditions()),
            )),
            external: host.and_then(|h| {
                super::prod::client_external(
                    &h.facts,
                    Some(&h.host),
                    Some(Arc::clone(&self.fn_externals)),
                )
            }),
            format: Some(OutputFormat::Esm),
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
        };
        Ok(SessionParts {
            options,
            plugins,
            css_urls,
            glob_scopes,
            styles,
        })
    }

    /// A full client build, its outputs written; how many files it made.
    async fn build(&self, host: Option<&BundleHost>) -> Result<usize, BundleError> {
        let start = &self.start;
        let parts = self.parts(host, false)?;
        let mut bundler = BundlerBuilder::default()
            .with_plugins(parts.plugins)
            .with_options(parts.options)
            .build()
            .map_err(|errs| BundleError::from_diagnostics(&errs.into_vec()))?;
        let (generated, module_ids) = on_rolldown(&start.rolldown, async {
            let generated = bundler.generate().await;
            let module_ids: Vec<String> = bundler
                .module_infos()
                .iter()
                .map(|e| e.key().as_str().to_string())
                .chain(bundler.watch_files().iter().map(|f| f.to_string()))
                .collect();
            let _ = bundler.close().await;
            (generated, module_ids)
        })
        .await;
        let output = generated.map_err(|errs| BundleError::from_diagnostics(&errs.into_vec()))?;
        if let Some(err) = unresolved_import_in(&output.warnings) {
            return Err(err);
        }
        let watch_files = match host {
            Some(h) => h.host.watch_files().await.unwrap_or_default(),
            None => Vec::new(),
        };
        *self.bundle_modules.lock().unwrap() = module_ids.clone();
        let css_urls = parts
            .css_urls
            .lock()
            .unwrap()
            .in_bundle_order(&output.assets);
        let scopes = parts.glob_scopes.lock().unwrap().clone();
        write_outputs(
            start,
            &output.assets,
            module_ids
                .iter()
                .chain(watch_files.iter())
                .map(String::as_str),
            &css_urls,
            &scopes,
            false,
        )
        .map_err(|e| BundleError::from_message(format!("writing the client bundle: {e}")))?;
        Ok(output.assets.len())
    }
}

/// How a rebundle went: every change out as hot updates (`css`: the sheets
/// they newly linked), or a full output written.
enum Run {
    Hot { css: Vec<String> },
    Output { files: usize },
}

/// The app's client externals as a predicate over specifiers: strings,
/// RegExps, and the specifiers a function external answered true for when
/// the bundle resolved them (`kept`).
fn declared_externals(
    host: Option<&BundleHost>,
    kept: &Arc<Mutex<HashSet<String>>>,
) -> super::hot::Declared {
    let items: Vec<serde_json::Value> = match host.and_then(|h| h.facts.external.clone()) {
        Some(serde_json::Value::Array(items)) => items,
        Some(one) => vec![one],
        None => Vec::new(),
    };
    let names: Vec<String> = items
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    let regexes: Vec<_> = items.iter().filter_map(super::facts::js_regex).collect();
    let kept = Arc::clone(kept);
    Arc::new(move |spec: &str| {
        names.iter().any(|n| n == spec)
            || regexes.iter().any(|r| r.matches(spec))
            || kept
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains(spec)
    })
}

/// Writes a bundle's outputs into the Start cache directory (the dev
/// server's contract, see the module docs).
pub(crate) fn write_outputs<'a>(
    start: &StartConfig,
    outputs: &[rolldown_common::Output],
    module_ids: impl Iterator<Item = &'a str>,
    css_urls: &[String],
    scopes: &[GlobCall],
    hot: bool,
) -> std::io::Result<()> {
    use rolldown_common::Output;
    let cache = &start.cache;
    let chunks_dir = cache.join("client-chunks");
    match std::fs::remove_dir_all(&chunks_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    std::fs::create_dir_all(&chunks_dir)?;
    let mut files = Vec::with_capacity(outputs.len());
    let mut modules = 0usize;
    let mut entry = None;
    let mut closure: Vec<String> = vec![start.router_entry().to_string_lossy().into_owned()];
    for out in outputs {
        let (name, bytes): (&str, &[u8]) = match out {
            Output::Chunk(c) => {
                modules += c.modules.keys.len();
                closure.extend(c.modules.keys.iter().map(|k| k.as_str().to_string()));
                if c.is_entry && entry.is_none() {
                    entry = Some(c.filename.to_string());
                }
                (c.filename.as_str(), c.code.as_bytes())
            }
            Output::Asset(a) => (a.filename.as_str(), a.source.as_bytes()),
        };
        let dest = chunks_dir.join(name);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, bytes)?;
        files.push(serde_json::json!({ "name": name, "size": bytes.len() }));
    }
    let entry = entry.or_else(|| {
        outputs.first().map(|o| match o {
            Output::Chunk(c) => c.filename.to_string(),
            Output::Asset(a) => a.filename.to_string(),
        })
    });
    write_json(
        &cache.join("client-chunks.json"),
        &serde_json::json!({ "entry": entry, "files": files }),
    )?;
    // Module count for the editor's update-progress narration.
    std::fs::write(cache.join("client-entry.modules"), modules.to_string())?;

    closure.extend(module_ids.map(str::to_string));
    write_json(
        &cache.join("closure.json"),
        &serde_json::json!(closure_files(start, closure)),
    )?;
    let mut scopes: Vec<serde_json::Value> = scopes
        .iter()
        .map(|s| {
            serde_json::json!({
                "patterns": s.patterns,
                "exhaustive": s.exhaustive,
                "caseSensitive": s.case_sensitive,
            })
        })
        .collect();
    scopes.sort_by_key(|v| v.to_string());
    scopes.dedup();
    write_json(
        &cache.join("glob-scopes.json"),
        &serde_json::Value::Array(scopes),
    )?;

    let manifest = dev_manifest(&start.dev_url_base(), css_urls, hot);
    // Only a changed manifest is written: the client bundle imports it, so a
    // watching engine takes every write as an edit.
    let path = cache.join("manifest.ts");
    if std::fs::read_to_string(&path).ok().as_deref() != Some(manifest.as_str()) {
        std::fs::write(&path, manifest)?;
    }
    let tmp = cache.join("css-urls.json.tmp");
    std::fs::write(&tmp, serde_json::to_string(css_urls)?)?;
    std::fs::rename(&tmp, cache.join("css-urls.json"))?;
    Ok(())
}

/// The client-side Start manifest of the dev bundle: the entry at the dev
/// server's URL and the stylesheets the bundle links. Under a hot engine it
/// accepts its own update (the pages read it once, at hydration).
pub(crate) fn dev_manifest(url_base: &str, css_urls: &[String], hot: bool) -> String {
    let entry = format!("{url_base}{CLIENT_ENTRY_PATH}");
    let manifest = serde_json::json!({
        "routes": {
            "__root__": {
                "preloads": [entry],
                "css": css_urls,
                "scripts": [{ "attrs": { "type": "module", "async": true, "src": entry } }],
            },
        },
    });
    let mut out = format!("export const tsrStartManifest = () => ({manifest});\n");
    if hot {
        out.push_str("if (import.meta.hot) import.meta.hot.accept();\n");
    }
    out
}

/// The bundle's input files, as the rebundle gate matches watcher paths
/// against them: an oj module id unwrapped to the file it wraps, queries
/// dropped, virtual ids, oj's own cache files and the generated route tree
/// left out, existing files only, sorted.
pub(crate) fn closure_files(start: &StartConfig, ids: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = ids
        .into_iter()
        .map(|id| {
            let id = if id.starts_with("\0oj-") {
                id.split_once(':')
                    .map_or(String::new(), |(_, rest)| rest.to_string())
            } else {
                id
            };
            id.split('?').next().unwrap_or("").to_string()
        })
        .filter(|p| !p.is_empty() && !p.starts_with('\0'))
        .map(|p| {
            let path = Path::new(&p);
            if path.is_absolute() {
                p
            } else {
                start.root.join(path).to_string_lossy().into_owned()
            }
        })
        .filter(|p| {
            let path = Path::new(p);
            !path.starts_with(&start.cache)
                && !path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    n.starts_with("routeTree.gen.")
                        && matches!(n.rsplit('.').next(), Some("ts" | "tsx" | "js" | "jsx"))
                })
                && path.is_file()
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

fn write_json(path: &Path, value: &serde_json::Value) -> std::io::Result<()> {
    std::fs::write(path, value.to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn start_at(root: &Path) -> StartConfig {
        StartConfig {
            root: root.to_path_buf(),
            cache: root.join(".oj-cache/v1/start"),
            command: super::super::Command::Serve,
            mode: "development".into(),
            config: Default::default(),
            app_env: oj_env::AppEnv::resolve(
                &oj_env::Env::from_vars::<&str, &str>([]),
                root,
                "development",
                vec!["VITE_".into()],
                "development",
            ),
            base: "/".into(),
            stylex: None,
            rolldown: test_runtime(),
        }
    }

    /// The test's own runtime; a synchronous test (which never bundles)
    /// gets one kept for the test process.
    fn test_runtime() -> tokio::runtime::Handle {
        static IDLE: std::sync::LazyLock<tokio::runtime::Runtime> =
            std::sync::LazyLock::new(|| tokio::runtime::Runtime::new().unwrap());
        tokio::runtime::Handle::try_current().unwrap_or_else(|_| IDLE.handle().clone())
    }

    // A function `external` declares only what it answered external for: a
    // typo'd bare import it never claimed still fails the dev bundle.
    #[test]
    fn a_function_external_declares_only_what_it_kept() {
        let kept: Arc<Mutex<HashSet<String>>> = Arc::default();
        let declared = declared_externals(None, &kept);
        assert!(!declared("cdn:lib"));
        kept.lock().unwrap().insert("cdn:lib".into());
        assert!(declared("cdn:lib"));
        assert!(!declared("reactt"), "a specifier the function never kept");
    }

    #[test]
    fn the_closure_holds_the_files_the_bundle_read() {
        let dir = std::env::temp_dir().join(format!("oj-start-closure-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join(".oj-cache/v1/start")).unwrap();
        for f in [
            "src/a.tsx",
            "src/b.css",
            "src/routeTree.gen.ts",
            ".oj-cache/v1/start/manifest.ts",
        ] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        let start = start_at(&dir);
        let ids = vec![
            dir.join("src/a.tsx?tsr-split=x")
                .to_string_lossy()
                .into_owned(),
            format!("\0oj-style:{}", dir.join("src/b.css").display()),
            "\0oj-node-shim:fs".into(),
            "virtual:routes".into(),
            "src/a.tsx".into(),
            dir.join("src/routeTree.gen.ts")
                .to_string_lossy()
                .into_owned(),
            dir.join(".oj-cache/v1/start/manifest.ts")
                .to_string_lossy()
                .into_owned(),
            dir.join("src/gone.ts").to_string_lossy().into_owned(),
        ];
        let files = closure_files(&start, ids);
        assert_eq!(
            files,
            vec![
                dir.join("src/a.tsx").to_string_lossy().into_owned(),
                dir.join("src/b.css").to_string_lossy().into_owned(),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_dev_manifest_names_the_entry_and_its_sheets() {
        let plain = dev_manifest("", &["/@oj-start/fs/app/a.css".into()], false);
        assert!(
            plain.starts_with("export const tsrStartManifest = () => ({\"routes\""),
            "{plain}"
        );
        assert!(plain.contains("\"/@oj-start/client-entry.js\""));
        assert!(plain.contains("\"css\":[\"/@oj-start/fs/app/a.css\"]"));
        // Under a dev `base`, the entry is named there, as Vite's urls are.
        let based = dev_manifest("/app", &[], false);
        assert!(
            based.contains("\"/app/@oj-start/client-entry.js\""),
            "{based}"
        );
        assert!(!plain.contains("import.meta.hot"));
        assert!(dev_manifest("", &[], true)
            .ends_with("if (import.meta.hot) import.meta.hot.accept();\n"));
    }
}
