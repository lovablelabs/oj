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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use oj_server::rolldown_runtime::on_rolldown;
use rolldown::{BundlerBuilder, BundlerOptions, InputItem, OutputFormat, Platform};
use rolldown_common::AddonOutputOption;
use rolldown_plugin::__inner::{Pluginable, SharedPluginable};

use super::alias::AppAliasPlugin;
use super::assets::{AssetMode, DevStyleState, StartAssetsPlugin, Styles};
use super::code::{CodeTarget, GlobScope, StartCodePlugin};
use super::errors::{unresolved_import_in, BundleError};
use super::host::StartHostPlugin;
use super::hosts::{self, BundleHost};
use super::hot::{EngineSlot, HotSession, SessionParts};
use super::inputs::{BuiltInputs, LoadedInputs, Notes, RecordLoadedInputs};
use super::refresh::{RefreshPreamble, RefreshTransform, RefreshWrapper};
use super::shims::NodeBuiltinShims;
use super::StartConfig;
use crate::build::EmitState;

pub(crate) const CLIENT_ERROR_FILE: &str = "client-error.json";
pub(crate) const CLIENT_HMR_FILE: &str = "client-hmr.json";
/// The chunk each module of the client bundle is in: what answers a page's
/// `import()` while no engine runs (see [`DevBundler::lazy_chunk`]).
const CLIENT_MODULES_FILE: &str = "client-modules.json";
/// The path the dev server serves the client entry chunk at, under `base`.
const CLIENT_ENTRY_PATH: &str = "/@oj-start/client-entry.js";

/// What answers a page's `import()`.
pub(crate) enum LazyAnswer {
    /// The module, compiled by the engine for the page.
    Compiled(String),
    /// A module that imports the restored chunk holding it.
    Restored(String),
    /// No module of a hot bundle goes by that id.
    Unknown,
    /// The engine did not compile the module: for an error that may be the
    /// module's own or another's (an edit that does not compile, whatever
    /// module it is in, fails every compile), or because no engine runs.
    Unavailable(BundleError),
    /// The module cannot be answered, and waiting changes nothing.
    Failed(BundleError),
}

pub(crate) struct DevBundler {
    start: Arc<StartConfig>,
    config_file: Option<PathBuf>,
    host: tokio::sync::OnceCell<Option<BundleHost>>,
    /// The dev server's plugin host, once it is up (`Some(None)`: it runs
    /// none), whose client environment the bundle runs in. Without one (the
    /// tests) the bundle spawns a host of its own.
    dev_host:
        Option<tokio::sync::watch::Receiver<Option<Option<Arc<oj_server::plugins::PluginHost>>>>>,
    /// Set while the app's routes have made no tree (see
    /// [`super::RouteTreeRefusal`]).
    route_tree_refusal: Option<super::RouteTreeRefusal>,
    styles: Arc<Styles>,
    /// The hot-update engine, while one runs; one bundle at a time holds it
    /// (rebundles and output refreshes share the cache directory).
    session: tokio::sync::Mutex<Option<HotSession>>,
    engine: EngineSlot,
    /// The bundler's hot updates on their way to the pages.
    pub(crate) hot: Arc<crate::start_hot::HotUpdates>,
    /// Whether pages may take hot updates: not behind the editor's
    /// full-reload HMR gate, where every edit reloads. A granular gate stays
    /// hot from the boot bundle on, so its first edit already patches.
    hot_enabled: std::sync::atomic::AtomicBool,
    /// The dev server's plugin host never came up (its server build failed
    /// first): the bundle ran without the app's plugins, and is not cached.
    host_failed: std::sync::atomic::AtomicBool,
    /// Bumped by every batch that touched a style input.
    style_epoch: std::sync::atomic::AtomicU64,
    /// Pages that announced their runtime id, with their open sockets.
    clients: Mutex<PageSockets>,
    /// Whether an engine ever ran in this dev server: until one does, the
    /// bundle the pages run is the one on disk (built or restored at boot).
    engine_started: std::sync::atomic::AtomicBool,
    /// Whether an engine started that pages are still to be reloaded for,
    /// and by whom (see [`DevBundler::rebundle_reloads_for_engine`]).
    engine_reload: std::sync::atomic::AtomicU8,
    /// The running engine's tag, which the runtime id of every page of its
    /// bundles starts with (see [`DevBundler::runs_engine_output`]).
    engine_tag: Mutex<String>,
    /// The modules of the bundle a boot restored from the cache, by chunk
    /// (see [`DevBundler::lazy_chunk`]); read when first asked.
    restored_modules: Mutex<Option<HashMap<String, String>>>,
    /// Specifiers the app's `external` function kept external, so the dev
    /// bundle's unresolved-import check takes the function's own answers.
    fn_externals: Arc<Mutex<HashSet<String>>>,
    /// The module ids of the last bundle written, whose StyleX rules a cached
    /// copy of it carries.
    bundle_modules: Mutex<Vec<String>>,
    /// Each module's file as the bundler loaded it, as loading goes on.
    loaded: Arc<LoadedInputs>,
    /// The files of the last bundle written, as they were when its output
    /// was produced.
    built: Mutex<BuiltInputs>,
}

/// Whether the client bundle hot-updates under the editor's HMR gate as the
/// config and the knobs resolve it (`oj_server`'s `hmr_gate_of`): only a
/// full-reload gate turns hot updates off. Decided here, before the boot
/// bundle, so the bundle it builds and the cache key it uses are the
/// session's own; the dev server applies the same resolution once its gate
/// exists (`start_dev::hot_under_gate`).
fn hot_under_config(config: &oj_config::OjConfig) -> bool {
    let knobs = &oj_env::get().knobs;
    let gate = config.server.as_ref().and_then(|s| s.hmr_gate) == Some(true) || knobs.hmr_gate;
    !(gate && knobs.hmr_full_reload)
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
        let hot_enabled = hot_under_config(&start.config);
        let loaded = Arc::new(LoadedInputs::new(&start.root));
        DevBundler {
            start: Arc::new(start),
            config_file,
            host: tokio::sync::OnceCell::new(),
            dev_host: None,
            route_tree_refusal: None,
            styles,
            session: tokio::sync::Mutex::new(None),
            engine: EngineSlot::default(),
            hot,
            hot_enabled: std::sync::atomic::AtomicBool::new(hot_enabled),
            host_failed: std::sync::atomic::AtomicBool::new(false),
            style_epoch: std::sync::atomic::AtomicU64::new(0),
            clients: Mutex::new(PageSockets::default()),
            engine_started: std::sync::atomic::AtomicBool::new(false),
            engine_reload: std::sync::atomic::AtomicU8::new(ENGINE_RELOAD_NONE),
            engine_tag: Mutex::default(),
            restored_modules: Mutex::default(),
            fn_externals: Arc::default(),
            bundle_modules: Mutex::new(Vec::new()),
            loaded,
            built: Mutex::default(),
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

    /// Fails its bundles with what `refusal` holds, while it holds anything.
    pub(crate) fn refusing_on(mut self, refusal: super::RouteTreeRefusal) -> DevBundler {
        self.route_tree_refusal = Some(refusal);
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

    /// The files the client bundle depends on that closure.json does not
    /// list, which the bundle store keys a generation by: the trees its
    /// routes come from (the client's own, generate.mjs's pruned tree in the
    /// cache, and the generated tree it stands in for and falls back to; the
    /// dev server regenerates and rebundles on a route change itself), and
    /// the config file with every file it imported (Vite's
    /// `configFileDependencies`, on which Vite restarts), which the store's
    /// epoch cannot name.
    pub(crate) fn cache_key_files(&self) -> Vec<PathBuf> {
        let mut config: Vec<PathBuf> = oj_server::plugins::config_dependencies().to_vec();
        config.extend(self.config_file.clone());
        // A cached extraction lists the config itself and sorted, a fresh one
        // only what it imported, in import order: the same set keys the same.
        config.sort();
        config.dedup();
        let mut files = vec![
            self.start.cache.join(super::CLIENT_ROUTE_TREE),
            self.start.route_tree(),
        ];
        files.extend(config);
        files
    }

    /// The defines the client bundle inlines, which the bundle store keys a
    /// generation by too: `import.meta.env` (the mode, NODE_ENV, `BASE_URL`
    /// and the `envPrefix` vars of the `.env` files in `envDir` and of the
    /// process, Vite's loadEnv), the config's `define` and the client
    /// environment's own. These are values of the evaluated config, so a
    /// define the config computes (from an env var, from a file it reads)
    /// keys the bundle by what it came to. What plugin `config` hooks add is
    /// known only once the plugin host is open, after the restore.
    pub(crate) fn cache_key_defines(&self) -> String {
        let defines = self.start.client_defines(&[], &[]);
        serde_json::to_string(&defines).unwrap_or_default()
    }

    pub(crate) fn set_hot_enabled(&self, enabled: bool) {
        self.hot_enabled
            .store(enabled, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn hot_enabled(&self) -> bool {
        self.hot_enabled.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Whether the client bundle is built for hot updates (Vite's bundled
    /// dev, with React Fast Refresh for an app that configures plugin-react):
    /// hot updates on and a development client. The bundle store keys a
    /// generation by this too: a bundle built for one mode must never be
    /// restored into the other (a page running a bundle without the refresh
    /// preamble would throw at the first patched component, and one built hot
    /// carries the engine's runtime a plain reload never drives). The run
    /// also asks the app's `server.hmr`, which the key carries through the
    /// config file it hashes.
    pub(crate) fn hot_bundle(&self) -> bool {
        self.hot_enabled() && !self.start.is_production()
    }

    /// Whether the last bundle may be cached: one built while the dev
    /// server's plugin host was down ran none of the app's plugins (and no
    /// Fast Refresh), so it must not be restored into a later, whole session
    /// under the key that session computes.
    pub(crate) fn cacheable(&self) -> bool {
        !self.host_failed.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Whether the files of the last bundle written are still as the
    /// bundler loaded them for it. One saved since (while the bundle was
    /// building, or after its output and before it is cached) is not what
    /// the bundle holds, so its contents now must not key that bundle in the
    /// cache.
    pub(crate) fn inputs_as_built(&self) -> bool {
        let built = self.built.lock().unwrap().clone();
        super::inputs::unchanged(&built)
    }

    /// The files of the bundle being served that were saved since it read
    /// them, or since its restore verified them.
    pub(crate) fn moved_inputs(&self) -> Vec<PathBuf> {
        let built = self.built.lock().unwrap().clone();
        super::inputs::moved(&built)
    }

    /// The files of the bundle being served, as its build or restore noted
    /// them.
    pub(crate) fn input_files(&self) -> std::collections::HashSet<PathBuf> {
        let built = self.built.lock().unwrap();
        built.iter().map(|(file, ..)| file.clone()).collect()
    }

    /// Takes a bundle restored from the store as the one being served, its
    /// files as the restore verified them (see `RestoreStats::inputs`).
    pub(crate) fn note_restored(&self, inputs: &[oj_cache::start_bundle::RestoredInput]) {
        *self.built.lock().unwrap() = super::inputs::restored_inputs(inputs);
    }

    /// Takes `notes`, the load notes as they were when `outputs` was
    /// produced, as what the bundle about to be written was built from: the
    /// modules of its chunks, which the bundle cache keys it by.
    fn note_built(&self, notes: &Notes, outputs: &[rolldown_common::Output]) {
        let modules = outputs.iter().filter_map(|out| match out {
            rolldown_common::Output::Chunk(c) => Some(c.modules.keys.iter()),
            rolldown_common::Output::Asset(_) => None,
        });
        let built = super::inputs::built_inputs(
            notes,
            modules.flatten().map(|id| id.as_str()),
            left_out_of_closure(&self.start.cache),
        );
        *self.built.lock().unwrap() = built;
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
    /// `another_run`: its document was served by another run of the dev
    /// server.
    ///
    /// `owed`: a reload is owed to it (see [`DevBundler::holds_engine_output`]).
    ///
    /// Whether the page is behind a hot update already: one made for it
    /// before its socket opened (see [`HotUpdates::connected`]).
    pub(crate) fn page_connected(&self, id: &str, another_run: bool, owed: bool) -> bool {
        let mut pages = self
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if another_run {
            pages.another_run.insert(id.to_string());
        }
        let first = pages.connect(id);
        if !first {
            return false;
        }
        let behind = self.hot.connected(id);
        // A page of another run reloads as it connects: nothing is made
        // for it.
        if let Some(engine) = self.engine.get().filter(|_| !another_run) {
            let holds = self.holds_engine_output(id, owed);
            let id = id.to_string();
            pages.then(async move { register(&engine, id, holds).await });
        }
        behind
    }

    /// The open pages this run of the dev server rendered.
    fn pages_of_this_run(&self) -> Vec<String> {
        let pages = self
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut ids = pages.ids();
        ids.retain(|id| !pages.another_run.contains(id));
        ids
    }

    /// A rebundle run ended, with fresh output written (`fresh`) or not.
    /// Whether the pages reload for an engine that started: the run's own,
    /// or one whose reload was left for the next fresh output.
    ///
    /// A page open across an engine's start runs the bundle of what came
    /// before: the one a boot restored from the cache, or the engine before
    /// a restart. From that engine it may hold modules (those a hot update
    /// or an `import()` gave it) under that engine's version stamps, and
    /// the page's runtime drops a module of the new engine whose stamp is
    /// lower, so a hot update of the new engine cannot be relied on to
    /// reach it. It reloads onto the new engine's output instead. Vite's
    /// client reloads the page when the dev server it lost is back
    /// (client.ts: "server connection lost. Polling for restart...", then
    /// `location.reload()`), which is what a restart of the server
    /// (server/index.ts `restartServer`) is to an open page.
    ///
    /// The reload is the starting run's to make, with its output: a
    /// rebundle's own (`fresh`), or the refresher's when the refresher
    /// started the engine (the plugin host's was replaced, and no edit is
    /// there to reload for). A rebundle that started one and wrote no
    /// output (it failed after the start) leaves it to whichever writes the
    /// next: the edit that fixes it may reach the pages as a hot update,
    /// which reloads nothing. The refresher never takes the reload of a
    /// rebundle still on its way, which would reload the pages twice.
    pub(crate) fn rebundle_reloads_for_engine(&self, fresh: bool) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        if fresh {
            return self.engine_reload.swap(ENGINE_RELOAD_NONE, SeqCst) != ENGINE_RELOAD_NONE;
        }
        let _ = self.engine_reload.compare_exchange(
            ENGINE_RELOAD_BY_RUN,
            ENGINE_RELOAD_BY_NEXT,
            SeqCst,
            SeqCst,
        );
        false
    }

    /// The refresher wrote fresh output. Whether the pages reload for an
    /// engine that started (see [`DevBundler::rebundle_reloads_for_engine`]).
    pub(crate) fn refresh_reloads_for_engine(&self) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        self.engine_reload
            .compare_exchange(ENGINE_RELOAD_BY_NEXT, ENGINE_RELOAD_NONE, SeqCst, SeqCst)
            .is_ok()
    }

    /// An engine started in a run of the refresher (`refresh`) or of a
    /// rebundle. Pages are owed a reload for it when any is open, and when
    /// it replaces an engine (`restart`): a page of that engine's bundle
    /// whose socket is down just now is one too, and reloads as it comes
    /// back.
    fn owe_engine_reload(&self, refresh: bool, restart: bool) {
        if restart || !self.pages_of_this_run().is_empty() {
            let by = if refresh {
                ENGINE_RELOAD_BY_NEXT
            } else {
                ENGINE_RELOAD_BY_RUN
            };
            self.engine_reload
                .store(by, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Whether the page `id` runs a bundle the running engine made (its
    /// runtime id starts with the engine's tag).
    fn runs_engine_output(&self, id: &str) -> bool {
        let tag = self
            .engine_tag
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !tag.is_empty()
            && id
                .strip_prefix(tag.as_str())
                .is_some_and(|rest| rest.starts_with('.'))
    }

    /// Whether the engine may take the page `id` to have run its newest
    /// output, which is what registering a page with it says: the engine
    /// then leaves out of every patch and every `import()` answer the
    /// modules that output evaluates as it loads (rolldown's
    /// `DevEngine::register_client`). That is true of a page that loaded
    /// the output. It is not of a page on a bundle from before the engine
    /// (the one a boot restored from the cache, or an earlier engine's),
    /// nor of one a reload is owed to (`owed`: its document is from before
    /// a rebuild, or it is behind a hot change, held by the editor's gate
    /// or missed): a module an edit made part of the entry since would be
    /// taken for one the page has, and an `import()` of a module that
    /// imports it would fail with `No factory registered for module`, for
    /// as long as the editor's gate holds the page's reload. Such a page is
    /// told to the engine as one that holds nothing of its output (see
    /// [`register`]): it is sent every module an answer or a patch needs
    /// that it was not sent before.
    /// Under Vite the page's `import()` is a request for the module, and
    /// each of its imports another, whatever the page loaded before.
    fn holds_engine_output(&self, id: &str, owed: bool) -> bool {
        !owed && self.runs_engine_output(id)
    }

    pub(crate) fn page_gone(&self, id: &str) {
        let mut pages = self
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !pages.gone(id) {
            return;
        }
        pages.another_run.remove(id);
        self.hot.forget(id);
        if let Some(engine) = self.engine.get() {
            let id = id.to_string();
            pages.then(async move { engine.remove_client(&id).await });
        }
    }

    /// The module behind an `import()` of an open page (see [`LazyAnswer`]).
    ///
    /// With the engine running, the module is compiled for the page as the
    /// bundle has it now: its factory and those of the static imports the
    /// page does not hold yet (rolldown's `DevEngine::compile_lazy_entry`).
    ///
    /// A boot that restored the bundle from the cache runs no engine until
    /// the first edit, and until then the restored bundle is the app as it
    /// is: the answer imports the restored chunk that holds the module, which
    /// registers it as it evaluates, at no cost to the boot. The first edit
    /// starts the engine and reloads the pages onto its bundle.
    ///
    /// Nothing here starts a bundle: a request cannot make the dev server
    /// build.
    ///
    /// `owed`: a reload is owed to the page (its document is from before a
    /// rebuild).
    pub(crate) async fn lazy_chunk(&self, proxy: &str, client: &str, owed: bool) -> LazyAnswer {
        // Only the id of an `import()`'s boundary: the engine would scan
        // any other module of the graph it is given as if it had changed.
        if !self.hot_bundle() || rolldown_common::lazy_proxy_target(proxy).is_none() {
            return LazyAnswer::Unknown;
        }
        let engine = match self.engine.get() {
            Some(engine) => engine,
            None => {
                let never_ran = !self
                    .engine_started
                    .load(std::sync::atomic::Ordering::SeqCst);
                if never_ran {
                    return match self.restored_chunk_of(proxy) {
                        // Beside this answer's own URL (`…/@oj-start/hmr/lazy`).
                        Ok(Some(chunk)) => LazyAnswer::Restored(format!(
                            "import {};\nexport {{}};\n",
                            serde_json::Value::String(format!("../{chunk}"))
                        )),
                        Ok(None) => LazyAnswer::Unknown,
                        // Nothing to wait for: no bundler runs.
                        Err(why) => LazyAnswer::Failed(BundleError::from_message(why)),
                    };
                }
                // Between two engines (a session that starts over): the next
                // one answers. A run in progress holds the session.
                drop(self.session.lock().await);
                match self.engine.get() {
                    Some(engine) => engine,
                    None => {
                        return LazyAnswer::Unavailable(BundleError::from_message(
                            "the client bundle is not built",
                        ))
                    }
                }
            }
        };
        // Not while a bundle is being made: a run in progress is waited
        // for, and one that starts waits for this compile.
        let _no_run = self.session.lock().await;
        // Nothing is compiled while the engine's last build failed. A
        // module that build could not load is loaded by the next thing
        // that scans the graph; if that were this compile, right after the
        // file is fixed and before the engine's watcher reports it, the
        // fixed module would join the graph with no update made for it, and
        // the engine's own update would then find nothing changed: a page
        // running the module would keep its code from before, told that
        // the build is fine again. The engine's update takes the fix, as
        // Vite's does (hmr.ts handleHMRUpdate), and imports wait for it.
        let errored = on_rolldown(&self.start.rolldown, engine.get_bundle_state())
            .await
            .map(|state| state.last_build_errored)
            .unwrap_or(false);
        if errored {
            return LazyAnswer::Unavailable(BundleError::from_message(
                "the client bundle does not build: a module of it has an error",
            ));
        }
        // Known to the engine before it compiles for it: what the page
        // already holds is left out of the answer. (The page's socket
        // registers it too; a request can come first.) A page that does not
        // run the engine's output is known as one that holds none of it
        // (see `holds_engine_output`).
        on_rolldown(
            &self.start.rolldown,
            register(
                &engine,
                client.to_string(),
                self.holds_engine_output(client, owed),
            ),
        )
        .await;
        let result = on_rolldown(
            &self.start.rolldown,
            engine.compile_lazy_entry(proxy.to_string(), client.to_string()),
        )
        .await;
        match result {
            // The page reports the chunk once its factories registered (as
            // for a patch); the export keeps it an ES module.
            Ok(out) => LazyAnswer::Compiled(format!(
                "{}\n;__rolldown_runtime__.payloadDelivered({});\n; export {{}}",
                out.code,
                serde_json::to_string(&out.filename).unwrap_or_default()
            )),
            Err(errs) => {
                let err = BundleError::from_diagnostics(&errs.into_vec());
                if err.message.contains("Lazy entry module not found") {
                    LazyAnswer::Unknown
                } else {
                    // The engine's error is not always the module's own:
                    // with a broken module anywhere in the graph it fails
                    // every compile with that module's error, and after a
                    // failed full build with "the last full build failed".
                    LazyAnswer::Unavailable(err)
                }
            }
        }
    }

    /// The client bundle's banner. A hot bundle's entry first imports the
    /// module that sets its engine's tag (see `start_hot::ENGINE_PATH`): an
    /// import in the entry alone, whose file name is fixed, so no chunk is
    /// renamed for it, and ahead of the entry's own imports, so it has run
    /// when the runtime makes the page's id.
    fn client_banner(&self, hot: bool) -> AddonOutputOption {
        let plain = self.start.client_banner();
        let tag = self
            .engine_tag
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if !hot || tag.is_empty() {
            return AddonOutputOption::String(Some(plain));
        }
        let entry = format!(
            "import {};{plain}",
            serde_json::Value::String(format!(
                "{}{}?t={tag}",
                self.start.dev_url_base(),
                crate::start_hot::ENGINE_PATH
            ))
        );
        AddonOutputOption::Fn(Arc::new(move |chunk| {
            let text = if chunk.is_entry {
                entry.clone()
            } else {
                plain.clone()
            };
            Box::pin(async move { Ok(Some(text)) })
        }))
    }

    /// A tag for the engine about to start, kept as the running engine's.
    fn new_engine_tag(&self) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let tag = format!("{nanos:x}");
        *self
            .engine_tag
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = tag.clone();
        tag
    }

    /// The chunk of the bundle on disk that holds the module `proxy` stands
    /// for (client-modules.json, written with the bundle and restored with
    /// it); `None` for an id that is no module of the bundle.
    fn restored_chunk_of(&self, proxy: &str) -> Result<Option<String>, String> {
        let Some(target) = rolldown_common::lazy_proxy_target(proxy) else {
            return Ok(None);
        };
        let mut modules = self
            .restored_modules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if modules.is_none() {
            let file = self.start.cache.join(CLIENT_MODULES_FILE);
            let read = std::fs::read_to_string(&file)
                .map_err(|e| e.to_string())
                .and_then(|text| serde_json::from_str(&text).map_err(|e| e.to_string()))
                .map_err(|e| {
                    format!("the client bundle's module map ({CLIENT_MODULES_FILE}): {e}")
                })?;
            *modules = Some(read);
        }
        Ok(modules.as_ref().and_then(|m| m.get(target).cloned()))
    }

    /// Whether the page `id` runs a bundle of another run of the dev server
    /// (a tab left open across a restart), as its socket said when it
    /// connected. Such a page is told to reload as it connects; this is for
    /// an `import()` it makes before it does.
    pub(crate) fn page_from_another_run(&self, id: &str) -> bool {
        self.clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .another_run
            .contains(id)
    }

    /// The engine forgets a page that asked for a module and has no socket
    /// open (a proxy that passes none, a page that left): it would render a
    /// patch for it at every edit. Queued with the pages' own comings and
    /// goings, so a socket that connects meanwhile registers after it.
    pub(crate) fn forget_unconnected(&self, id: &str) {
        let mut pages = self
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pages.is_open(id) {
            return;
        }
        self.hot.forget(id);
        if let Some(engine) = self.engine.get() {
            let id = id.to_string();
            pages.then(async move { engine.remove_client(&id).await });
        }
    }

    /// A page ran a patch (Vite's payloadDeliveredAck).
    pub(crate) fn patch_delivered(&self, filename: &str) {
        if let Some(engine) = self.engine.get() {
            let filename = filename.to_string();
            tokio::spawn(async move { engine.notify_payload_delivered(&filename).await });
        }
    }

    pub(crate) fn pages(&self) -> Vec<String> {
        self.clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ids()
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
                    Err(_) => {
                        self.host_failed
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                        None
                    }
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
        // Routes that have made no tree since this server started: the page
        // is shown what the generator said, in place of a client built on
        // the tree an earlier run left on disk. Start's client tree plugin
        // fails the tree's load the same way under Vite while its generator
        // has no crawl to build the client's tree from; once one has
        // succeeded it keeps serving that crawl's tree through a later
        // failure, as the client tree generate.mjs last wrote is kept here.
        let refused = self.route_tree_refusal.as_ref().and_then(|refusal| {
            refusal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        });
        let outcome = match refused {
            Some(said) => Err(BundleError::from_message(said)),
            None => match self.host().await {
                Ok(host) => self.run(&mut session, host, changed, refresh).await,
                Err(e) => Err(BundleError::from_message(format!("{e:#}"))),
            },
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
        // under the editor's full-reload HMR gate (every edit reloads there,
        // so pages never take a patch). A granular gate stays hot.
        let hot = self.hot_bundle() && host.is_none_or(|h| h.facts.server_hmr);
        let epoch = self.style_epoch();
        // A style input moved: CSS modules and preprocessor sheets the
        // engine compiled predate the epoch, and a partial is no module of
        // its graph at all. A fresh engine compiles them anew. One that
        // compiled no sheet keeps running.
        // The plugin host's engine was respawned: what its hooks said of the
        // session's modules (`meta`, which a chunk group may name by) went
        // with it, and the session rebuilds only what changes. A fresh one
        // runs every hook on the engine there is.
        // Of an engine that is up: a host found dead is respawned here, so
        // the session's hooks, and its chunk policy made below, count the
        // engine they run on and not the dead one before it.
        let host_engine = match host {
            Some(h) => h.host.live_engine_generation().await,
            None => 0,
        };
        let stale = session.as_ref().is_some_and(|s| {
            session_stale(
                hot,
                s.epoch != epoch
                    && s.styles
                        .compiled_sheets
                        .load(std::sync::atomic::Ordering::SeqCst),
                s.host_engine,
                host_engine,
            )
        });
        if stale {
            if let Some(old) = session.take() {
                old.close(&self.engine).await;
            }
        }
        if !hot {
            // A build runs no watchChange (the dev engine's does, before each
            // rebuild): Start's import protection forgets the batch here.
            if let Some(h) = host.filter(|_| !changed.is_empty()) {
                if let Some(ip) = h.host.import_protection() {
                    for file in changed {
                        ip.changed(
                            oj_server::import_protection::Side::Client,
                            &file.to_string_lossy(),
                        );
                    }
                }
                let files: Vec<String> = changed
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                if let Err(e) = h.host.import_protection_changed(&files).await {
                    eprintln!(
                        "{}: import protection did not take the change: {e}",
                        oj_server::oj_brand()
                    );
                }
            }
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
                    self.record_inputs(live, host).await?;
                    return Ok(Run::Hot { css });
                }
            }
        } else {
            eprintln!("{}: client bundling", oj_server::oj_brand());
            // Before the parts: the entry's banner names the tag.
            self.new_engine_tag();
            let parts = self.parts(host, true)?;
            let runtime = std::fs::read_to_string(start.cache.join("hmr-runtime.js"))
                .map_err(|e| BundleError::from_message(format!("hmr-runtime.js: {e}")))?
                .replace(
                    "__OJ_DEV_BASE__",
                    &serde_json::Value::String(start.dev_url_base()).to_string(),
                );
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
                // The pages open now run another bundle (this engine's
                // first output is still to be written): they are told to
                // the engine as holding none of its output, and reload onto
                // it (see `rebundle_reloads_for_engine`).
                || self.pages_of_this_run(),
            )
            .await?;
            let restart = self
                .engine_started
                .swap(true, std::sync::atomic::Ordering::SeqCst);
            self.owe_engine_reload(refresh, restart);
            let mut live = live;
            live.host_engine = host_engine;
            *session = Some(live);
        }
        let live = session.as_ref().expect("a live session");
        let (output, notes) = live.latest_output().await?;
        self.note_built(&notes, &output);
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

    /// Records the live engine's module graph as the bundle's input set
    /// (closure.json, glob-scopes.json) after a hot update. A patch can grow
    /// the graph: an import the edit added, a new route and the components
    /// it imports, a file a glob gained. Vite's module graph takes such a
    /// module the moment it is first transformed (server/moduleGraph.ts,
    /// `ensureEntryFromUrl`), so the next change to it propagates; the input
    /// set the rebundle gate consults has to follow the graph the same way,
    /// not wait for the next full output. Until it did, every later edit to
    /// a module gained by a patch was "outside every served graph".
    async fn record_inputs(
        &self,
        live: &HotSession,
        host: Option<&BundleHost>,
    ) -> Result<(), BundleError> {
        let module_ids: Vec<String> = live
            .engine
            .get_module_ids()
            .iter()
            .map(|id| id.to_string())
            .collect();
        let watch_files = match host {
            Some(h) => h.host.watch_files().await.unwrap_or_default(),
            None => Vec::new(),
        };
        let scopes = live.glob_scopes.lock().unwrap().clone();
        // A union with the set as written: a patch only ever adds to the
        // graph, and the engine's map is emptied while a full build runs
        // (rolldown clears it at the start of one), so a read during one
        // must not shrink the set. The next full output writes it afresh.
        let written: Vec<String> = std::fs::read_to_string(self.start.cache.join("closure.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        write_inputs(
            &self.start,
            written
                .iter()
                .chain(module_ids.iter())
                .chain(watch_files.iter())
                .map(String::as_str),
            &scopes,
        )
        .map_err(|e| BundleError::from_message(format!("writing the bundle's inputs: {e}")))?;
        *self.bundle_modules.lock().unwrap() = module_ids;
        Ok(())
    }

    /// The client bundle's plugins and options: `hot` adds React Fast
    /// Refresh, as @vitejs/plugin-react does for Vite's bundled dev (only for
    /// an app that configures the plugin): the runtime module, which installs
    /// the preamble as it evaluates and is the entry's first import, the
    /// refresh transform and the wrapper (see `refresh`).
    fn parts(&self, host: Option<&BundleHost>, hot: bool) -> Result<SessionParts, BundleError> {
        let start = &self.start;
        let emit = Arc::new(EmitState::new(start.root.clone()));
        let css_urls = Arc::new(Mutex::new(super::assets::LinkedSheets::default()));
        let glob_scopes = Arc::new(Mutex::new(Vec::new()));
        let styles = Arc::new(DevStyleState::default());
        let refresh = hot && host.is_some_and(|h| h.facts.react_refresh);
        let entry = start.cache.join("client-entry.tsx");
        // First, so each module's file is noted before anything reads it; a
        // bundle that starts over loads every module again.
        self.loaded.clear();
        let mut plugins: Vec<SharedPluginable> = vec![Pluginable::new_shared(RecordLoadedInputs(
            Arc::clone(&self.loaded),
        ))];
        if let Some(plugin) = &start.stylex {
            plugins.push(crate::stylex_plugin::transform(plugin));
        }
        if refresh {
            plugins.push(Pluginable::new_shared(RefreshPreamble {
                runtime: start.cache.join("react-refresh-runtime.js"),
                entry: entry.clone(),
            }));
        }
        // Start's client route tree (route-tree-client-plugin, a "pre" load).
        plugins.push(Pluginable::new_shared(
            super::route_tree::ClientRouteTree::new(start),
        ));
        if let Some(h) = host {
            if !h.facts.alias.is_empty() {
                plugins.push(Pluginable::new_shared(AppAliasPlugin {
                    entries: h.facts.alias.clone(),
                }));
            }

            if let Some(ip) = h.host.import_protection() {
                plugins.push(Pluginable::new_shared(
                    super::import_protection::ImportProtectionPlugin(ip),
                ));
            }
            plugins.push(Pluginable::new_shared(
                StartHostPlugin::new(
                    Arc::clone(&h.host),
                    h.ctx.clone(),
                    &h.facts,
                    start.command,
                    &start.root,
                    Arc::clone(&emit),
                    start.jsx_config(),
                )
                .compiling(
                    StartCodePlugin::new(&start.root, CodeTarget::Client)
                        .with_route_tree(start.route_tree())
                        .with_alias(h.facts.alias.clone())
                        .routes_split_in_host(true),
                ),
            ));
        }
        // After the app's plugins, where vite:resolve sits: tsconfig `paths`
        // as Vite's dev server applies them, which never falls through to
        // rolldown's own (oxc's order, ahead of packages).
        if matches!(
            oj_server::tsconfig_paths(&start.config),
            oj_resolver::TsconfigPaths::Plugin
                | oj_resolver::TsconfigPaths::PluginNative
                | oj_resolver::TsconfigPaths::Neither
        ) {
            plugins.push(Pluginable::new_shared(
                super::alias::TsconfigPathsPlugin::new(
                    oj_server::client_resolver(&start.root, &start.config),
                    false,
                ),
            ));
        }
        plugins.extend(crate::build::dedupe_plugin(
            &start.root,
            oj_config::environment_dedupe(&start.config, "client"),
        ));
        plugins.push(Pluginable::new_shared(
            StartCodePlugin::new(&start.root, CodeTarget::Client)
                .with_route_tree(start.route_tree())
                .with_alias(host.map(|h| h.facts.alias.clone()).unwrap_or_default())
                .compiled_in_host(host.is_some())
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
            tsconfig: oj_server::tsconfig_file(&start.config)
                .map(rolldown_common::TsConfig::Manual),
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
            banner: Some(self.client_banner(hot)),
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
            loaded: Arc::clone(&self.loaded),
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
        let (generated, module_ids, notes) = on_rolldown(&start.rolldown, async {
            let generated = bundler.generate().await;
            // Every module is loaded by now, and nothing loads after.
            let notes = self.loaded.snapshot();
            let module_ids: Vec<String> = bundler
                .module_infos()
                .iter()
                .map(|e| e.key().as_str().to_string())
                .chain(bundler.watch_files().iter().map(|f| f.to_string()))
                .collect();
            let _ = bundler.close().await;
            (generated, module_ids, notes)
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
        self.note_built(&notes, &output.assets);
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

/// No engine start is waiting for a reload.
const ENGINE_RELOAD_NONE: u8 = 0;
/// The rebundle run that started the engine reloads the pages at its end.
const ENGINE_RELOAD_BY_RUN: u8 = 1;
/// Whichever writes the next fresh output reloads them.
const ENGINE_RELOAD_BY_NEXT: u8 = 2;

/// Tells the engine of the page `id`. One that `holds` its newest output is
/// sent only what that output does not evaluate as it loads; any other is
/// taken to hold nothing, and the engine records what each answer and patch
/// then gives it (rolldown's delivery notification), so a second `import()`
/// is not sent the first one's modules again.
async fn register(engine: &rolldown_dev::DevEngine, id: String, holds: bool) {
    if holds {
        engine.register_client(id).await;
    } else {
        engine.register_client_without_output(id).await;
    }
}

/// Whether a hot session is done for: the bundle is no longer hot, a style
/// input moved under sheets the session compiled, or the plugin host's
/// engine is another than the one the session's hooks ran on.
fn session_stale(hot: bool, styles_moved: bool, engine_then: u64, engine_now: u64) -> bool {
    !hot || styles_moved || engine_then != engine_now
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
    scopes: &[GlobScope],
    hot: bool,
) -> std::io::Result<()> {
    use rolldown_common::Output;
    let cache = &start.cache;
    // The chunk files of the bundles before this one stay: a page open on an
    // older entry imports its chunks lazily by their old names, and the dev
    // server serves them from here for as long as it retains that bundle
    // (`start_chunks::ServedBundles`), as Vite's bundled dev keeps every
    // output of the session (server/bundledDev.ts `storeOutputFiles`: "don't
    // clear memoryFiles here"). Chunk names are content-hashed, so a name
    // this bundle shares with an earlier one is the same bytes; the entry's
    // fixed name is only ever served from the current bundle. The dev server
    // removes a bundle's files when it forgets the bundle, and what an
    // earlier run left when it boots.
    let chunks_dir = cache.join("client-chunks");
    std::fs::create_dir_all(&chunks_dir)?;
    let mut files = Vec::with_capacity(outputs.len());
    let mut modules = 0usize;
    let mut entry = None;
    let mut closure: Vec<String> = Vec::new();
    let mut chunk_of = serde_json::Map::new();
    for out in outputs {
        let (name, bytes): (&str, &[u8]) = match out {
            Output::Chunk(c) => {
                modules += c.modules.keys.len();
                for id in c.modules.keys.iter() {
                    chunk_of.insert(id.as_str().to_string(), c.filename.as_str().into());
                }
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
        write_chunk_file(&dest, bytes)?;
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
    write_json(&cache.join(CLIENT_MODULES_FILE), &chunk_of.into())?;
    // Module count for the editor's update-progress narration.
    std::fs::write(cache.join("client-entry.modules"), modules.to_string())?;

    closure.extend(module_ids.map(str::to_string));
    write_inputs(start, closure.iter().map(String::as_str), scopes)?;

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

/// Writes a chunk file by a temp name and a rename, never in place: the
/// name may already exist, as an earlier bundle's file the dev server still
/// serves (same bytes, since chunk names are content-hashed) or as the
/// entry's fixed name, and at a store-restored boot it may be a hard link of
/// the store's blob (`start_chunks::keep_chunk_files`). Truncating and
/// rewriting that file would show a reader a partial file for the length of
/// the write, and would write the new bytes into the blob behind the link; a
/// rename swaps the whole file in and leaves the old inode to its other
/// readers and names.
fn write_chunk_file(dest: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let Some(parent) = dest.parent() else {
        return std::fs::write(dest, bytes);
    };
    let file_name = dest.file_name().and_then(|n| n.to_str()).unwrap_or("chunk");
    let tmp = parent.join(format!(".tmp-{file_name}-{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, dest).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
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

/// Writes the bundle's input set, which the dev server's rebundle gate
/// consults: closure.json (the router entry, every module id named, see
/// [`closure_files`]) and glob-scopes.json (every `import.meta.glob` the
/// bundle expanded, with its importer, for the files created or deleted
/// under one). A full output and a hot update both write it, since both can
/// change the graph.
pub(crate) fn write_inputs<'a>(
    start: &StartConfig,
    ids: impl Iterator<Item = &'a str>,
    scopes: &[GlobScope],
) -> std::io::Result<()> {
    let cache = &start.cache;
    let mut closure: Vec<String> = vec![start.router_entry().to_string_lossy().into_owned()];
    closure.extend(ids.map(str::to_string));
    write_json(
        &cache.join("closure.json"),
        &serde_json::json!(closure_files(start, closure)),
    )?;
    let mut scopes: Vec<serde_json::Value> = scopes
        .iter()
        .map(|s| {
            serde_json::json!({
                "importer": s.importer,
                "patterns": s.call.patterns,
                "exhaustive": s.call.exhaustive,
                "caseSensitive": s.call.case_sensitive,
            })
        })
        .collect();
    scopes.sort_by_key(|v| v.to_string());
    scopes.dedup();
    write_json(
        &cache.join("glob-scopes.json"),
        &serde_json::Value::Array(scopes),
    )
}

/// The bundle's input files, as the rebundle gate matches watcher paths
/// against them: an oj module id unwrapped to the file it wraps, queries
/// dropped, virtual ids, oj's own cache files and the generated route tree
/// left out, existing files only, sorted.
pub(crate) fn closure_files(start: &StartConfig, ids: Vec<String>) -> Vec<String> {
    let left_out = left_out_of_closure(&start.cache);
    let mut out: Vec<String> = ids
        .iter()
        .filter_map(|id| module_file(&start.root, id))
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|p| {
            let path = Path::new(p);
            !left_out(path) && path.is_file()
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The file a module id names, under `root` when the id is relative: its
/// query aside, and a wrapped id of oj's own (`\0oj-<kind>:<path>`) by the
/// file it wraps. None for any other virtual id.
pub(crate) fn module_file(root: &Path, id: &str) -> Option<PathBuf> {
    let id = match id.strip_prefix("\0oj-") {
        Some(rest) => rest.split_once(':')?.1,
        None => id,
    };
    let file = id.split('?').next().unwrap_or("");
    if file.is_empty() || file.starts_with('\0') {
        return None;
    }
    let path = Path::new(file);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    })
}

/// What [`closure_files`] leaves out, which a restore's glob check has to
/// leave out too: oj's own cache files and the generated route tree. The
/// bundler names files by realpath, so a cache dir reached through a symlink
/// is matched under both spellings.
pub(crate) fn left_out_of_closure(cache: &Path) -> impl Fn(&Path) -> bool {
    let cache = cache.to_path_buf();
    let real_cache = std::fs::canonicalize(&cache).ok();
    move |path| {
        path.starts_with(&cache)
            || real_cache
                .as_deref()
                .is_some_and(|real| path.starts_with(real))
            || is_route_tree_gen(path)
    }
}

/// The generated route tree, `routeTree.gen.[jt]sx?`.
pub(crate) fn is_route_tree_gen(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("routeTree.gen."))
        .is_some_and(|ext| matches!(ext, "js" | "jsx" | "ts" | "tsx"))
}

fn write_json(path: &Path, value: &serde_json::Value) -> std::io::Result<()> {
    std::fs::write(path, value.to_string())
}

/// The open sockets of each page, by its runtime id. A page's id outlives
/// its socket: a page whose socket died without a close opens a new one
/// under the same id, and the dead one's server end only closes later (its
/// ping deadline, or TCP giving up). That close must not take the page off
/// the engine while its new socket is open, so a page leaves only with its
/// last socket. (Socket.IO and Phoenix name each connection instead, so a
/// dead one's close never touches its successor.)
///
/// The engine hears of pages in the order they come and go: a socket
/// closing just before the page's next one says hello takes it off, then
/// on again, and the two calls must not land the other way round (rolldown's
/// `register_client` waits on a lock first, so two spawned tasks could).
#[derive(Default)]
pub(crate) struct PageSockets {
    open: std::collections::HashMap<String, usize>,
    /// The open pages whose document another run of the dev server served.
    another_run: HashSet<String>,
    /// The last engine call queued; the next one runs after it.
    chain: Option<tokio::task::JoinHandle<()>>,
}

impl PageSockets {
    /// Runs `call` once the engine calls queued before it have run. Queued
    /// under the same lock as the count it follows, so the order is the
    /// pages' own.
    pub(crate) fn then(&mut self, call: impl std::future::Future<Output = ()> + Send + 'static) {
        let before = self.chain.take();
        self.chain = Some(tokio::spawn(async move {
            if let Some(before) = before {
                let _ = before.await;
            }
            call.await;
        }));
    }

    fn is_open(&self, id: &str) -> bool {
        self.open.contains_key(id)
    }

    /// Whether `id` was not connected before: the engine learns of it.
    pub(crate) fn connect(&mut self, id: &str) -> bool {
        let open = self.open.entry(id.to_string()).or_default();
        *open += 1;
        *open == 1
    }

    /// Whether that was `id`'s last socket: the engine forgets it.
    pub(crate) fn gone(&mut self, id: &str) -> bool {
        match self.open.get_mut(id) {
            Some(open) if *open > 1 => {
                *open -= 1;
                false
            }
            Some(_) => {
                self.open.remove(id);
                true
            }
            None => false,
        }
    }

    pub(crate) fn ids(&self) -> Vec<String> {
        self.open.keys().cloned().collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    // A hot session lives while the bundle is hot, its sheets stand and the
    // plugin host's engine is the one its hooks ran on. A respawned engine
    // knows none of what they said of the modules, and the session would
    // rebuild only what changes: it starts over, and the bundle after a
    // respawn is whole again.
    #[test]
    fn a_hot_session_ends_with_the_engine_its_hooks_ran_on() {
        assert!(!super::session_stale(true, false, 2, 2));
        assert!(super::session_stale(true, false, 2, 3), "respawned");
        assert!(
            super::session_stale(true, true, 2, 2),
            "a style input moved"
        );
        assert!(super::session_stale(false, false, 2, 2), "no longer hot");
        assert!(!super::session_stale(true, false, 0, 0), "no plugin host");
    }

    use super::*;

    /// Who reloads the pages for an engine that started: the run that
    /// started it, with its own output, and never another run in its place
    /// while it is still on its way.
    #[test]
    fn the_reload_for_an_engine_start_is_made_once_by_the_run_that_owes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let bundler = DevBundler::for_tests(tmp.path());
        // No engine started: nothing to reload for.
        assert!(!bundler.refresh_reloads_for_engine());
        assert!(!bundler.rebundle_reloads_for_engine(true));
        // A first engine with no page open owes none.
        bundler.owe_engine_reload(false, false);
        assert!(!bundler.rebundle_reloads_for_engine(true));

        // A rebundle restarts the engine: the refresher, finishing first,
        // leaves the reload to it, and the rebundle makes it once.
        bundler.owe_engine_reload(false, true);
        assert!(!bundler.refresh_reloads_for_engine());
        assert!(bundler.rebundle_reloads_for_engine(true));
        assert!(!bundler.rebundle_reloads_for_engine(true));
        assert!(!bundler.refresh_reloads_for_engine());

        // A rebundle that started one and wrote no output leaves it to the
        // next fresh output, the refresher's included.
        bundler.owe_engine_reload(false, true);
        assert!(!bundler.rebundle_reloads_for_engine(false));
        assert!(bundler.refresh_reloads_for_engine());
        assert!(!bundler.refresh_reloads_for_engine());
        bundler.owe_engine_reload(false, true);
        assert!(!bundler.rebundle_reloads_for_engine(false));
        assert!(bundler.rebundle_reloads_for_engine(true));

        // The refresher restarts the engine: its own reload, or a rebundle's
        // that writes fresh output before it.
        bundler.owe_engine_reload(true, true);
        assert!(bundler.refresh_reloads_for_engine());
        bundler.owe_engine_reload(true, true);
        assert!(!bundler.rebundle_reloads_for_engine(false));
        assert!(bundler.rebundle_reloads_for_engine(true));
        assert!(!bundler.refresh_reloads_for_engine());

        // A page open at the first engine's start is owed one too.
        bundler.page_connected("page", false, false);
        bundler.owe_engine_reload(false, false);
        assert!(bundler.rebundle_reloads_for_engine(true));
    }

    /// Engine calls run in the order they were queued, however long each
    /// takes: a slow first call (rolldown's register waits on a lock) is
    /// not overtaken by a quick second.
    #[tokio::test]
    async fn engine_calls_run_in_the_order_the_pages_came_and_went() {
        let mut pages = PageSockets::default();
        let order = Arc::new(Mutex::new(Vec::new()));
        for (name, wait) in [("register", 50), ("remove", 0), ("register again", 10)] {
            let order = Arc::clone(&order);
            pages.then(async move {
                tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                order.lock().unwrap().push(name);
            });
        }
        pages.chain.take().unwrap().await.unwrap();
        assert_eq!(
            *order.lock().unwrap(),
            ["register", "remove", "register again"]
        );
    }

    /// The page's new socket connects before its dead one's server end
    /// closes: the page stays on the engine through both.
    #[test]
    fn a_page_leaves_only_with_its_last_socket() {
        let mut pages = PageSockets::default();
        assert!(pages.connect("c1"), "the first socket announces the page");
        assert!(
            !pages.connect("c1"),
            "a second socket of the same page does not"
        );
        assert!(
            !pages.gone("c1"),
            "the dead socket's close leaves the page on"
        );
        assert_eq!(pages.ids(), vec!["c1".to_string()]);
        assert!(pages.gone("c1"), "its last socket takes it off");
        assert!(pages.ids().is_empty());
        assert!(!pages.gone("c1"), "an unknown page's close changes nothing");
        assert!(
            pages.connect("c1"),
            "a page back after leaving is new again"
        );
    }

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

    #[cfg(unix)]
    #[test]
    fn the_closure_leaves_out_a_symlinked_cache_by_its_realpath() {
        let tmp =
            std::env::temp_dir().join(format!("oj-start-closure-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let dir = std::fs::canonicalize(&tmp).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("out/start")).unwrap();
        std::fs::create_dir_all(dir.join(".oj-cache/v1")).unwrap();
        std::os::unix::fs::symlink(dir.join("out/start"), dir.join(".oj-cache/v1/start")).unwrap();
        std::fs::write(dir.join("src/a.tsx"), "").unwrap();
        std::fs::write(dir.join("out/start/manifest.ts"), "").unwrap();
        let start = start_at(&dir);
        let ids = vec![
            dir.join("src/a.tsx").to_string_lossy().into_owned(),
            dir.join("out/start/manifest.ts")
                .to_string_lossy()
                .into_owned(),
        ];
        assert_eq!(
            closure_files(&start, ids),
            vec![dir.join("src/a.tsx").to_string_lossy().into_owned()]
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

    /// A chunk file is replaced by a rename, not rewritten in place: a hard
    /// link of the old file (the store's blob, at a restored boot) keeps its
    /// bytes, a reader holding the old file keeps a whole one, and no temp
    /// file is left beside the chunks.
    #[test]
    fn a_chunk_file_is_swapped_in_whole_and_never_written_through_a_link() {
        let dir = std::env::temp_dir().join(format!("oj-chunk-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        let chunk = dir.join("assets/shared-AAAA.js");
        let blob = dir.join("blob");
        std::fs::write(&blob, b"old bytes").unwrap();
        std::fs::hard_link(&blob, &chunk).unwrap();
        let old = std::fs::File::open(&chunk).unwrap();

        write_chunk_file(&chunk, b"new bytes, longer").unwrap();

        assert_eq!(std::fs::read(&chunk).unwrap(), b"new bytes, longer");
        assert_eq!(
            std::fs::read(&blob).unwrap(),
            b"old bytes",
            "the link's other name kept its bytes"
        );
        let mut held = String::new();
        std::io::Read::read_to_string(&mut &old, &mut held).unwrap();
        assert_eq!(
            held, "old bytes",
            "a reader of the old file still sees all of it"
        );
        let leftovers: Vec<_> = std::fs::read_dir(dir.join("assets"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        // A fresh name is written the same way.
        write_chunk_file(&dir.join("assets/new-BBBB.js"), b"fresh").unwrap();
        assert_eq!(
            std::fs::read(dir.join("assets/new-BBBB.js")).unwrap(),
            b"fresh"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
