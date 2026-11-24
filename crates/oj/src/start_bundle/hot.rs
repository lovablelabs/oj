// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Hot updates for the dev client bundle, as Vite's bundled dev serves them
//! (server/bundledDev.ts): one rolldown `DevEngine` for the session instead
//! of a build per edit. Its watcher turns an edit into a patch per page (the
//! changed modules alone) within milliseconds; the dev server pushes each
//! patch to its page, whose runtime (hmr-runtime.js) re-runs the modules that
//! accept it. A rebundle request still gets the full current output, for
//! reloads and the bundle cache.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rolldown_common::{HmrUpdate, Output, WatcherFileWatcherOptions};
use rolldown_dev::{BundlerConfig, DevEngine, DevOptions, DevWatchOptions, ErrorStage};
use rolldown_plugin::__inner::SharedPluginable;
use rolldown_utils::pattern_filter::StringOrRegex;

use super::assets::DevStyleState;
use super::code::GlobScope;
use super::errors::{unresolved_import, unresolved_import_in, BundleError};
use super::inputs::{LoadedInputs, Notes};
use crate::start_hot::{HotUpdates, Patch};

/// What the engine's watcher made of a changed file since the last rebundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    Patch,
    Noop,
    Reload,
    /// The engine rebuilt without an update for any page.
    Unseen,
    /// The file was deleted behind an earlier rebundle: the engine's update
    /// over it failed, and that rebundle asked for the full build, which
    /// read the files without it. Stands only while the file stays gone.
    Gone,
}

/// The app's declared externals a client bundle may import as written.
pub(crate) type Declared = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// An output of the engine (or the error of its build) and the load notes
/// taken with it.
type NotedOutput = (Result<Vec<Output>, BundleError>, Notes);

#[derive(Default)]
struct Events {
    seen: Mutex<HashMap<String, Seen>>,
    /// The engine's last failed update (a syntax error), for the rebundle
    /// that reports it.
    error: Mutex<Option<BundleError>>,
    /// The engine's last output, with the load notes as they were when it
    /// was produced (the engine loads again after it, for the next patch).
    output: Mutex<Option<NotedOutput>>,
    outputs: AtomicU64,
    /// The engine's tasks that ended: its builds and its updates.
    tasks: AtomicU64,
    changed: tokio::sync::Notify,
}

/// One engine session and the state its plugins write into.
pub(crate) struct HotSession {
    pub engine: Arc<DevEngine>,
    /// The style epoch the session compiled its sheets under.
    pub epoch: u64,
    /// The plugin host's engine the session's hooks run on
    /// (`PluginHost::engine_generation`), 0 without a host.
    pub host_engine: u64,
    pub css_urls: Arc<Mutex<super::assets::LinkedSheets>>,
    pub glob_scopes: Arc<Mutex<Vec<GlobScope>>>,
    pub styles: Arc<DevStyleState>,
    events: Arc<Events>,
}

/// The session's engine, reachable from the pages' socket handlers.
#[derive(Clone, Default)]
pub(crate) struct EngineSlot(Arc<Mutex<Option<Arc<DevEngine>>>>);

impl EngineSlot {
    pub fn get(&self) -> Option<Arc<DevEngine>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set(&self, engine: Option<Arc<DevEngine>>) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = engine;
    }
}

pub(crate) struct SessionParts {
    pub options: rolldown::BundlerOptions,
    pub plugins: Vec<SharedPluginable>,
    pub css_urls: Arc<Mutex<super::assets::LinkedSheets>>,
    pub glob_scopes: Arc<Mutex<Vec<GlobScope>>>,
    pub styles: Arc<DevStyleState>,
    /// The bundler's load notes, which each output takes a copy of.
    pub loaded: Arc<LoadedInputs>,
}

impl HotSession {
    /// Starts an engine on `parts` (its initial full build included).
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        mut parts: SessionParts,
        root: &Path,
        rolldown: &tokio::runtime::Handle,
        runtime: String,
        watch: Option<&serde_json::Value>,
        epoch: u64,
        hot: Arc<HotUpdates>,
        declared: Declared,
        slot: &EngineSlot,
        clients: impl Fn() -> Vec<String>,
    ) -> Result<HotSession, BundleError> {
        let experimental = parts
            .options
            .experimental
            .get_or_insert_with(Default::default);
        experimental.dev_mode = Some(rolldown_common::DevModeOptions {
            lazy: Some(false),
            implement: Some(runtime),
            // For `ResendModules`, the one plugin of the bundle with the hook.
            hot_update: Some(true),
            ..Default::default()
        });
        // Vite turns this off for its bundled dev too (vitejs/vite#21843).
        parts.options.optimization = Some(rolldown_common::OptimizationOption {
            inline_const: Some(rolldown_common::InlineConstOption::Bool(false)),
            ..Default::default()
        });
        // First: an `import()` resolves to its proxy before any other plugin
        // names the target.
        parts.plugins.insert(
            0,
            rolldown_plugin::__inner::Pluginable::new_shared(super::lazy::LazyImports::default()),
        );
        let events = Arc::new(Events::default());
        let on_updates = {
            let events = Arc::clone(&events);
            let root = root.to_path_buf();
            let declared = Arc::clone(&declared);
            Arc::new(
                move |result: rolldown_error::BuildResult<(
                    Vec<rolldown_common::ClientHmrUpdate>,
                    Vec<String>,
                )>| {
                    on_hmr_updates(&events, &root, &hot, &declared, result);
                },
            )
        };
        let on_output = {
            let events = Arc::clone(&events);
            let loaded = Arc::clone(&parts.loaded);
            let declared = Arc::clone(&declared);
            Arc::new(
                move |result: rolldown_error::BuildResult<rolldown_dev::BundleOutput>| {
                    let recorded = match result {
                        Ok(out) => match unresolved_import_in(&out.warnings)
                            .or_else(|| unresolved_in_output(&out.assets, &declared))
                        {
                            Some(err) => Err(err),
                            None => Ok(out.assets),
                        },
                        Err(errs) => Err(BundleError::from_diagnostics(&errs.into_vec())),
                    };
                    // Inside the engine's build task, before a queued update
                    // can load a module the build read.
                    events.tasks.fetch_add(1, Ordering::SeqCst);
                    record_output(&events, recorded, loaded.snapshot());
                },
            )
        };
        let engine = DevEngine::new(
            BundlerConfig::new(parts.options, parts.plugins),
            DevOptions {
                on_hmr_updates: Some(on_updates),
                on_output: Some(on_output),
                watch: Some(watch_options(watch)),
                ..Default::default()
            },
        )
        .map_err(|errs| BundleError::from_diagnostics(&errs.into_vec()))?;
        let engine = Arc::new(engine);
        // The initial full build; its result lands in `on_output`. A page
        // registers after it, as a session's evaluated modules start from it.
        // `run` spawns the engine's coordinator, whose builds start rolldown's
        // tasks: on the rolldown runtime.
        let _ = oj_server::rolldown_runtime::on_rolldown(rolldown, engine.run()).await;
        // Published before the pages are read: a page connecting from here on
        // registers itself, one connected before is in the live set
        // (registering twice keeps the first session). The pages open now
        // did not load this engine's output, which is still to be written.
        slot.set(Some(Arc::clone(&engine)));
        for id in clients() {
            engine.register_client_without_output(id).await;
        }
        Ok(HotSession {
            engine,
            epoch,
            host_engine: 0,
            css_urls: parts.css_urls,
            glob_scopes: parts.glob_scopes,
            styles: parts.styles,
            events,
        })
    }

    pub async fn close(self, slot: &EngineSlot) {
        slot.set(None);
        let _ = self.engine.close().await;
    }

    /// Whether a rebundle's edits all went out as hot updates. The engine
    /// watches the same files the dev server does, usually sooner: its
    /// modules, and the scan bases of the globs they expand, so a file
    /// created or deleted under one is an edit it reports too (the glob's
    /// importer re-expands: `StartCodePlugin`'s transform registers each
    /// base as the importer's watch file). An input it
    /// has no module for and no glob over (a file the bundle only declared)
    /// rebuilds in full.
    pub async fn settle(&self, changed: &[PathBuf], has_pages: bool) -> Result<bool, BundleError> {
        let globs: Vec<oj_compiler::glob::GlobMatcher> = self
            .glob_scopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|g| g.call.matcher())
            .collect();
        let under_a_glob = |p: &str| globs.iter().any(|g| g.is_match(&p.replace('\\', "/")));
        // A sheet the bundle only names by its url (`./app.css?url`, as a
        // route's `links` do): the module is that url, which an edit to the
        // sheet does not change, and the page links the sheet itself. Vite
        // answers such an edit with a `css-update` for the linked sheet and
        // no reload; here the pages re-link their sheets, and the bundle has
        // nothing to rebuild.
        let linked_only = |p: &str| {
            crate::start_dev::is_style_input(Path::new(p))
                && self.engine.get_module_info(p).is_none()
                && self.engine.get_module_info(&format!("{p}?url")).is_some()
        };
        let all: Vec<String> = changed
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let linked = all.iter().filter(|p| linked_only(p)).count();
        if linked > 0 && linked == all.len() {
            // With no page open nothing re-links: the next document does.
            return Ok(true);
        }
        let changed: Vec<PathBuf> = changed
            .iter()
            .filter(|p| !linked_only(&p.to_string_lossy()))
            .cloned()
            .collect();
        let changed = changed.as_slice();
        let known: Vec<String> = changed
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .filter(|p| self.engine.get_module_info(p).is_some() || under_a_glob(p))
            .collect();
        // The engine's watcher reports an edit within milliseconds, unless it
        // is at work: an edit that lands during a build waits for it, and
        // giving up on it then would turn a hot update into a reload.
        let ready = || {
            self.events.error.lock().unwrap().is_some() || {
                let seen = self.events.seen.lock().unwrap();
                // A file noted as gone that is back is a new file, which the
                // engine has yet to report.
                known.iter().all(|p| match seen.get(p) {
                    Some(Seen::Gone) => !Path::new(p).exists(),
                    Some(_) => true,
                    None => false,
                })
            }
        };
        let busy = || async {
            let done = self.events.tasks.load(Ordering::SeqCst);
            let _ = self.engine.wait_for_ongoing_bundle().await;
            self.events.tasks.load(Ordering::SeqCst) != done
        };
        let watched = reported(
            ready,
            busy,
            Duration::from_millis(1000),
            Duration::from_secs(60),
        )
        .await;
        // A failed hot update over a deleted file is no verdict on the app:
        // the engine re-fetches every module that depends on a changed file,
        // a deleted file's own `?raw` module among them, which no longer
        // loads although nothing imports it any more once its glob's importer
        // re-expanded. The full build that follows (`latest_output`) starts
        // over from the entries, as Vite's bundled dev rebuilds after a
        // failed update when a page needs the output; its error, if it has
        // one, is the one to report. The engine watches on its own, so the
        // deletion may belong to a batch after this one (a file deleted while
        // this rebundle was on its way): the failure then names a module
        // whose file is gone, with every file of `changed` in place. A
        // failure with every changed file still on disk and its own module's
        // too (a syntax error mid-edit) is the edit's own, reported as it
        // is: the overlay shows it without a full build first.
        if let Some(err) = self.events.error.lock().unwrap().take() {
            let mut seen = self.events.seen.lock().unwrap();
            for p in &known {
                seen.remove(p);
            }
            let gone = deleted_file_behind(&err);
            if changed.iter().any(|p| !p.exists()) || gone.is_some() {
                // Asked for here, not left to `latest_output` reading the
                // engine's state: a second watcher event for the deleted
                // file starts another update, and the state reads as a
                // build in progress, not as the failed update, while it
                // runs. The output before the deletion would stand as the
                // current one.
                let _ = self.engine.trigger_full_build();
                // The deletion is in a batch still to come, and the engine
                // will say nothing more of the file (its failed update
                // recorded nothing): that batch's rebundle would wait out
                // the deadline above and build in full a second time. The
                // build asked for here covers it.
                if let Some(gone) = gone.filter(|gone| changed.iter().all(|p| p != Path::new(gone)))
                {
                    seen.insert(gone, Seen::Gone);
                }
                return Ok(false);
            }
            return Err(err);
        }
        let kinds: Vec<Option<Seen>> = {
            let mut seen = self.events.seen.lock().unwrap();
            known.iter().map(|p| seen.remove(p)).collect()
        };
        let unknown = changed.len() > known.len();
        if unknown || !watched {
            let _ = self.engine.trigger_full_build();
        }
        Ok(has_pages
            && !known.is_empty()
            && watched
            && !unknown
            && kinds
                .iter()
                .all(|k| matches!(k, Some(Seen::Patch | Seen::Noop))))
    }

    /// The engine's current output: what a rebundle writes for reloads and
    /// the bundle cache. Waits out a build the engine still owes; after a
    /// failed update, forces the full build that recovers it (Vite does when
    /// a page needs the output).
    pub async fn latest_output(&self) -> Result<(Vec<Output>, Notes), BundleError> {
        // Every build the engine owes, not only the one running: a change
        // that arrived during a build waits for it and starts as it ends,
        // and the build it waited for says the output is current.
        for _ in 0..8 {
            let done = self.events.tasks.load(Ordering::SeqCst);
            let _ = self.engine.wait_for_ongoing_bundle().await;
            if self.events.tasks.load(Ordering::SeqCst) == done {
                break;
            }
        }
        // Counted from here: an output of a build waited for above is not
        // the one the build forced below reports, and taking it for that
        // one would read the engine's output before the forced build's
        // lands.
        let before = self.events.outputs.load(Ordering::SeqCst);
        let state = self
            .engine
            .get_bundle_state()
            .await
            .map_err(|errs| BundleError::from_diagnostics(&errs.into_vec()))?;
        if state.last_build_errored && matches!(state.last_error_stage, Some(ErrorStage::Hmr)) {
            let _ = self.engine.trigger_full_build();
        }
        let has_output = self.events.output.lock().unwrap().is_some();
        if state.has_stale_output || state.last_build_errored || !has_output {
            let _ = self.engine.ensure_latest_bundle_output().await;
            // The output callback lands right after the build it reports;
            // none at all means the engine had nothing newer.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            loop {
                // Registered before the check: an output landing between the
                // check and the wait still wakes it.
                let notified = self.events.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.events.outputs.load(Ordering::SeqCst) != before {
                    break;
                }
                if tokio::time::timeout_at(deadline, notified).await.is_err() {
                    break;
                }
            }
        }
        match self.events.output.lock().unwrap().clone() {
            Some((result, notes)) => result.map(|output| (output, notes)),
            None => Err(BundleError::from_message(
                "the client bundle produced no output",
            )),
        }
    }
}

/// Waits for `ready`, a `window` at a time: when one ends, `busy` waits out
/// what the engine is doing and says whether it was doing anything, which
/// earns another window, up to `cap`.
async fn reported<B, F>(ready: impl Fn() -> bool, busy: B, window: Duration, cap: Duration) -> bool
where
    B: Fn() -> F,
    F: std::future::Future<Output = bool>,
{
    let end = tokio::time::Instant::now() + cap;
    loop {
        let deadline = tokio::time::Instant::now() + window;
        while tokio::time::Instant::now() < deadline {
            if ready() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        if tokio::time::Instant::now() >= end || !busy().await {
            return ready();
        }
    }
}

/// The file of the module a failed update is over, when it is no longer on
/// disk (`err.id`: the module that failed to load, without its query and
/// fragment, as the plugin host splits an id). An id that is no absolute
/// path (a plugin's `\0` virtual module) is no file. An absolute id with no
/// file behind it at all (a path-like virtual id such as `/@x`) reads as a
/// deleted file: its failure is reported all the same, by the full build
/// that follows, at the cost of that build.
fn deleted_file_behind(err: &BundleError) -> Option<String> {
    let id = err.id.as_deref()?;
    let file = id.split(['?', '#']).next().unwrap_or(id);
    (Path::new(file).is_absolute() && !Path::new(file).exists()).then(|| file.to_string())
}

/// An output of the engine, or the error of its build. An output that
/// built says the sources build as they are: a failed update recorded
/// before it (one the rebundle that took it in already answered with this
/// build, and that a second update over the same file failed again) is no
/// longer the app's state, and the next rebundle would otherwise take it for
/// a fresh failure and build in full where a patch does.
fn record_output(events: &Events, recorded: Result<Vec<Output>, BundleError>, notes: Notes) {
    if recorded.is_ok() {
        *events.error.lock().unwrap() = None;
    }
    *events.output.lock().unwrap() = Some((recorded, notes));
    events.outputs.fetch_add(1, Ordering::SeqCst);
    events.changed.notify_waiters();
}

fn on_hmr_updates(
    events: &Events,
    root: &Path,
    hot: &HotUpdates,
    declared: &Declared,
    result: rolldown_error::BuildResult<(Vec<rolldown_common::ClientHmrUpdate>, Vec<String>)>,
) {
    events.tasks.fetch_add(1, Ordering::SeqCst);
    let (updates, changed) = match result {
        Ok(ok) => ok,
        Err(errs) => {
            // A failed update reaches the page through the rebundle that follows.
            *events.error.lock().unwrap() = Some(BundleError::from_diagnostics(&errs.into_vec()));
            // Of files a page asked for, no rebundle follows: the page gives
            // up waiting and reloads, and the files are no longer asked for.
            hot.resend_failed();
            return;
        }
    };
    *events.error.lock().unwrap() = None;
    // The editor's gate holds these files from here, before any patch for
    // them is handed to the pages and, as the engine's build task still has
    // the bundler, before a page's `import()` can be compiled against them.
    // The dev server's own watcher reports the same files later (it waits
    // for a quiet moment), which left a patch made meanwhile unheld.
    // Not the files a page asked to be sent again: nothing wrote them, the
    // editor flushes nothing for them, and no rebundle waits to hear of them.
    let changed = hot.edits(changed);
    hot.hold(&changed);
    let kind = if updates
        .iter()
        .any(|u| matches!(u.update, HmrUpdate::FullReload { .. }))
    {
        Seen::Reload
    } else if updates
        .iter()
        .any(|u| matches!(u.update, HmrUpdate::Patch(_)))
    {
        Seen::Patch
    } else {
        Seen::Noop
    };
    {
        let mut seen = events.seen.lock().unwrap();
        for file in changed {
            seen.insert(
                file,
                if updates.is_empty() {
                    Seen::Unseen
                } else {
                    kind
                },
            );
        }
    }
    // Every page's patch for this edit is one change to the pages, which
    // ends by naming the clients it covered: a page the engine left out (it
    // registered mid-build) learns it is behind.
    let change = updates
        .iter()
        .any(|u| matches!(u.update, HmrUpdate::Patch(_)))
        .then(|| hot.begin_change());
    let mut covered = Vec::with_capacity(updates.len());
    for u in updates {
        let HmrUpdate::Patch(patch) = u.update else {
            // Nothing to patch (or a reload, which the reload count carries).
            covered.push(u.client_id);
            continue;
        };
        // An import that never resolved fails the rebundle (the overlay),
        // never the page that would run the patch. The page is still
        // covered: the overlay is what it gets for this change, not a reload.
        covered.push(u.client_id.clone());
        let importer = root.join(patch.changed_ids.first().map(String::as_str).unwrap_or(""));
        if let Some(err) = unresolved_in_patch(&patch.code, &importer.to_string_lossy(), declared) {
            *events.error.lock().unwrap() = Some(err);
            continue;
        }
        hot.accept(&Patch {
            client: u.client_id,
            filename: patch.filename,
            seq: patch.seq,
            changed_ids: patch.changed_ids,
            code: patch.code,
        });
    }
    if let Some(change) = change {
        hot.end_change(change, &covered);
    }
}

/// Whether `spec` names no module of the output: not an output file, not a
/// URL, not one of the app's own externals.
fn unresolved_spec(spec: &str, files: &HashSet<&str>, declared: &Declared) -> bool {
    static SCHEME: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)^(?:[a-z][a-z0-9+.-]*:|//)").unwrap());
    !files.contains(spec) && !SCHEME.is_match(spec) && !declared(spec)
}

/// The import an engine's output keeps external although nothing resolved
/// it (the dev engine reports no logs for it): any import of a client output
/// that is not another output file, a URL or a declared external. Its
/// importer is the module whose rendered code carries it.
fn unresolved_in_output(output: &[Output], declared: &Declared) -> Option<BundleError> {
    let files: HashSet<&str> = output
        .iter()
        .map(|o| match o {
            Output::Chunk(c) => c.filename.as_str(),
            Output::Asset(a) => a.filename.as_str(),
        })
        .collect();
    for o in output {
        let Output::Chunk(chunk) = o else { continue };
        let Some(spec) = chunk
            .imports
            .iter()
            .chain(chunk.dynamic_imports.iter())
            .map(|s| s.as_str())
            .find(|s| unresolved_spec(s, &files, declared))
        else {
            continue;
        };
        let quoted = [format!("\"{spec}\""), format!("'{spec}'")];
        let importer = chunk
            .modules
            .keys
            .iter()
            .zip(chunk.modules.values.iter())
            .find(|(_, m)| {
                m.code()
                    .is_some_and(|c| quoted.iter().any(|q| c.contains(q.as_str())))
            })
            .map(|(k, _)| k.as_str().to_string())
            .unwrap_or_else(|| chunk.filename.to_string());
        return Some(unresolved_import(spec, &importer));
    }
    None
}

/// The same in a hot patch: the engine renders an external import as a
/// plain `import` / `export from` / `import()` of its specifier (the modules
/// it bundles go through the runtime), so one in the patch is an import that
/// never resolved. Read from the parsed module record, never the text.
fn unresolved_in_patch(code: &str, importer: &str, declared: &Declared) -> Option<BundleError> {
    use oxc_allocator::Allocator;
    let allocator = Allocator::default();
    let parsed = oxc_parser::Parser::new(&allocator, code, oxc_span::SourceType::mjs()).parse();
    if parsed.fatal_error {
        return None;
    }
    let record = &parsed.module_record;
    let mut specs: Vec<String> = record
        .requested_modules
        .keys()
        .map(|k| k.to_string())
        .collect();
    for d in &record.dynamic_imports {
        let lit = &code[d.module_request.start as usize..d.module_request.end as usize];
        let quoted = lit.len() >= 2
            && ((lit.starts_with('"') && lit.ends_with('"'))
                || (lit.starts_with('\'') && lit.ends_with('\'')))
            && !lit[1..lit.len() - 1].contains(['"', '\'', '\\']);
        if quoted {
            specs.push(lit[1..lit.len() - 1].to_string());
        }
    }
    let files = HashSet::new();
    specs
        .iter()
        .find(|s| unresolved_spec(s, &files, declared))
        .map(|spec| unresolved_import(spec, importer.split('?').next().unwrap_or(importer)))
}

/// The engine's default `debounceDelay` (rolldown_dev_common's
/// `normalize_dev_options`), passed along so the tick below is sized against
/// the delay in effect.
const ENGINE_DEBOUNCE_DELAY_MS: u64 = 10;

/// The engine's watcher options from the app's `server.watch`, as Vite's
/// bundled dev hands them over (convertToDevWatchOptions): polling apps poll
/// here too, and `null` turns watching off. The engine never writes files:
/// the dev server serves its outputs.
fn watch_options(watch: Option<&serde_json::Value>) -> DevWatchOptions {
    let Some(w) = watch else {
        return DevWatchOptions {
            enabled: Some(false),
            skip_write: true,
            ..Default::default()
        };
    };
    let patterns = |k: &str| -> Option<Vec<StringOrRegex>> {
        let v = w.get(k)?;
        let items: Vec<&serde_json::Value> = match v {
            serde_json::Value::Array(a) => a.iter().collect(),
            other => vec![other],
        };
        Some(
            items
                .into_iter()
                .filter_map(|x| match x {
                    serde_json::Value::String(s) => Some(StringOrRegex::String(s.clone())),
                    other => super::facts::js_regex(other).map(StringOrRegex::Regex),
                })
                .collect(),
        )
    };
    let debounce_delay = w
        .get("debounceDelay")
        .or_else(|| w.get("debounceDuration"))
        .and_then(|v| v.as_u64())
        .unwrap_or(ENGINE_DEBOUNCE_DELAY_MS);
    let watcher = WatcherFileWatcherOptions {
        use_polling: w.get("usePolling").and_then(|v| v.as_bool()),
        poll_interval: w
            .get("pollInterval")
            .or_else(|| w.get("interval"))
            .and_then(|v| v.as_u64()),
        compare_contents_for_polling: w.get("compareContentsForPolling").and_then(|v| v.as_bool()),
        use_debounce: w.get("useDebounce").and_then(|v| v.as_bool()),
        debounce_delay: Some(debounce_delay),
        // The engine's debouncer thread sleeps while no event is queued
        // (oj's fork of the debouncer, crates/oj_notify_debouncer_full) and
        // wakes once a tick while one is, to look for an event old enough to
        // report; its default tick is a quarter of the delay. The delay
        // itself as the tick wakes it once per edit instead of four times,
        // and bounds the report at two delays from the save where it was
        // one and a quarter. An app's own tick is kept.
        debounce_tick_rate: Some(
            w.get("debounceTickRate")
                .and_then(|v| v.as_u64())
                .unwrap_or(debounce_delay),
        ),
    };
    DevWatchOptions {
        skip_write: true,
        watcher: Some(watcher),
        include: patterns("include"),
        exclude: patterns("exclude"),
        ..Default::default()
    }
}

/// The stylesheets a hot update imported for the first time, written into
/// css-urls.json (the server render links them from now on). Under Vite a
/// CSS import is a module injecting its own sheet; here it is a link the dev
/// server adds to the open pages.
pub(crate) fn link_new_styles(cache: &Path, css_urls: &[String]) -> std::io::Result<Vec<String>> {
    let file = cache.join("css-urls.json");
    let linked: Vec<String> = std::fs::read_to_string(&file)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let added: Vec<String> = css_urls
        .iter()
        .filter(|h| !linked.contains(h))
        .cloned()
        .collect();
    if !added.is_empty() {
        // After the links the full bundle wrote in its execution order, as
        // the dev client appends a sheet a new import injects.
        let all: Vec<&String> = linked.iter().chain(&added).collect();
        let tmp = cache.join("css-urls.json.tmp");
        std::fs::write(&tmp, serde_json::to_string(&all)?)?;
        std::fs::rename(&tmp, &file)?;
    }
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none_declared() -> Declared {
        Arc::new(|_| false)
    }

    /// What the plugin host's transform does for every module of the app's
    /// plugins: the file behind a queried id (`b.md?raw`) is the module's
    /// watch file, so the engine loads the module again when the file moves.
    #[derive(Debug)]
    struct WatchesTheFileBehindAQuery;

    impl rolldown_plugin::Plugin for WatchesTheFileBehindAQuery {
        fn name(&self) -> std::borrow::Cow<'static, str> {
            std::borrow::Cow::Borrowed("test:watch-queried")
        }

        fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
            rolldown_plugin::HookUsage::Transform
        }

        async fn transform(
            &self,
            ctx: rolldown_plugin::SharedTransformPluginContext,
            args: &rolldown_plugin::HookTransformArgs<'_>,
        ) -> rolldown_plugin::HookTransformReturn {
            if let Some((file, _)) = args.id.split_once('?') {
                ctx.add_watch_file(file);
            }
            Ok(None)
        }
    }

    #[test]
    fn a_failed_update_is_over_a_deleted_file_by_its_modules_file() {
        let dir = std::env::temp_dir().join(format!("oj-start-gone-id-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("x.svg");
        std::fs::write(&file, "<svg/>").unwrap();
        let over = |id: String| {
            deleted_file_behind(&BundleError {
                id: Some(id),
                ..Default::default()
            })
        };
        let path = file.to_string_lossy().into_owned();
        // The file is there: the failure is the module's own.
        assert_eq!(over(path.clone()), None);
        assert_eq!(over(format!("{path}?raw")), None);
        assert_eq!(over(format!("{path}#frag")), None);
        assert_eq!(over(format!("{path}?url#frag")), None);
        std::fs::remove_file(&file).unwrap();
        assert_eq!(over(path.clone()), Some(path.clone()));
        assert_eq!(over(format!("{path}?raw")), Some(path.clone()));
        assert_eq!(over(format!("{path}#frag")), Some(path.clone()));
        assert_eq!(over(format!("{path}?url#frag")), Some(path.clone()));
        // No file was ever behind these.
        assert_eq!(over("\0virtual:x".into()), None);
        assert_eq!(over("virtual:x".into()), None);
        assert_eq!(deleted_file_behind(&BundleError::default()), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A failed update recorded behind the rebundle that answered it (a
    /// second update over the same deleted file failing again) does not
    /// outlive the build that succeeded: the next rebundle would take it
    /// for a fresh failure. A build that failed leaves it.
    #[test]
    fn an_output_that_built_clears_the_failed_update_before_it() {
        let events = Events::default();
        let failed = || {
            Some(BundleError::from_message(
                "Could not load /app/src/b.md?raw",
            ))
        };
        *events.error.lock().unwrap() = failed();
        record_output(
            &events,
            Err(BundleError::from_message("the build failed")),
            Notes::default(),
        );
        assert_eq!(*events.error.lock().unwrap(), failed());
        record_output(&events, Ok(Vec::new()), Notes::default());
        assert_eq!(*events.error.lock().unwrap(), None);
        assert_eq!(events.outputs.load(Ordering::SeqCst), 2);
    }

    /// An app under a fresh directory and a session over its `src/entry.js`.
    async fn session_over(label: &str, files: &[(&str, &str)]) -> (PathBuf, HotSession) {
        use super::super::assets::{AssetMode, StartAssetsPlugin, Styles};
        use super::super::code::{CodeTarget, StartCodePlugin};
        use rolldown_plugin::__inner::Pluginable;

        let dir = std::env::temp_dir().join(format!("oj-start-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        for (name, body) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        let start = super::super::dev::tests::start_at(&dir);
        oj_server::write_start_assets(&start.cache).unwrap();
        let runtime = std::fs::read_to_string(start.cache.join("hmr-runtime.js")).unwrap();
        let css_urls = Arc::new(Mutex::new(super::super::assets::LinkedSheets::default()));
        let glob_scopes = Arc::new(Mutex::new(Vec::new()));
        let styles = Arc::new(DevStyleState::default());
        let parts = SessionParts {
            options: rolldown::BundlerOptions {
                input: Some(vec![rolldown::InputItem {
                    name: Some("entry".into()),
                    import: dir.join("src/entry.js").to_string_lossy().into_owned(),
                }]),
                cwd: Some(dir.clone()),
                platform: Some(rolldown::Platform::Browser),
                format: Some(rolldown::OutputFormat::Esm),
                ..Default::default()
            },
            plugins: vec![
                Pluginable::new_shared(WatchesTheFileBehindAQuery),
                Pluginable::new_shared(
                    StartCodePlugin::new(&dir, CodeTarget::Client)
                        .recording_globs(Arc::clone(&glob_scopes)),
                ),
                Pluginable::new_shared(StartAssetsPlugin::new(
                    &dir,
                    Arc::new(Styles::new(&start)),
                    AssetMode::Dev {
                        css_urls: Arc::clone(&css_urls),
                        state: Arc::clone(&styles),
                        url_base: String::new(),
                    },
                )),
            ],
            css_urls,
            glob_scopes,
            styles,
            loaded: Arc::new(LoadedInputs::new(&dir)),
        };
        let session = HotSession::start(
            parts,
            &dir,
            &start.rolldown,
            runtime,
            Some(&serde_json::Value::Null),
            0,
            Arc::new(HotUpdates::new("/")),
            none_declared(),
            &EngineSlot::default(),
            || vec!["page".to_string()],
        )
        .await
        .unwrap();
        (dir, session)
    }

    fn until(what: &str, done: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// `file` is written, and the engine's own watcher has taken the write
    /// in: where a rebundle for it stands when the dev server's batch for
    /// the write is still on its way.
    async fn written_and_seen(session: &HotSession, file: &Path, body: &str) {
        std::fs::write(file, body).unwrap();
        let name = file.to_string_lossy().into_owned();
        until("the engine never saw the write", || {
            session.events.seen.lock().unwrap().contains_key(&name)
        });
        // The engine's task is over, and its coordinator has taken the
        // task's end in (it answers in order): the files the task loaded
        // are watched before the next change is made.
        let _ = session.engine.wait_for_ongoing_bundle().await;
        let _ = session.engine.get_bundle_state().await;
    }

    /// `file` is deleted, and the engine's update over it has failed.
    fn deleted_and_failed(session: &HotSession, file: &Path) {
        std::fs::remove_file(file).unwrap();
        until(
            "the engine's update over the deleted file never failed",
            || session.events.error.lock().unwrap().is_some(),
        );
    }

    fn code(output: &[Output]) -> String {
        output
            .iter()
            .filter_map(|o| match o {
                Output::Chunk(c) => Some(c.code.as_str()),
                Output::Asset(_) => None,
            })
            .collect()
    }

    /// The engine watches on its own, ahead of the dev server's batches. A
    /// glob member deleted while the rebundle for an earlier batch is on its
    /// way fails the engine's update (it loads the deleted member's module
    /// again), and that rebundle finds the failure with every file of its
    /// own batch in place. The failure is over a file that is gone: the
    /// rebundle takes the full build, where the glob's importer expands to
    /// the members there are, and reports no failure of the app.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_member_deleted_behind_a_rebundle_does_not_fail_it() {
        let (dir, session) = session_over(
            "glob-member-race",
            &[
                (
                    "src/entry.js",
                    "const notes = import.meta.glob(\"./notes/*.md\", { query: \"?raw\", eager: true, import: \"default\" });\nconsole.log(notes);\n",
                ),
                ("src/notes/a.md", "note-a\n"),
                ("src/notes/b.md", "note-b\n"),
            ],
        )
        .await;
        let created = dir.join("src/notes/c.md");
        written_and_seen(&session, &created, "note-c\n").await;
        deleted_and_failed(&session, &dir.join("src/notes/b.md"));

        // The rebundle for the created member alone.
        let settled = session.settle(std::slice::from_ref(&created), true).await;
        assert_eq!(
            settled.as_ref().map_err(|e| e.message.as_str()),
            Ok(&false),
            "the rebundle takes the full build"
        );
        let (output, _) = session.latest_output().await.unwrap();
        let built = code(&output);
        assert!(
            built.contains("note-c") && !built.contains("note-b"),
            "{built}"
        );

        // The rebundle for the deletion's own batch, which comes after: the
        // build above read the files without the deleted member, so it has
        // nothing to wait for and nothing to build.
        let builds = session.events.outputs.load(Ordering::SeqCst);
        let settled = session.settle(&[dir.join("src/notes/b.md")], true).await;
        assert_eq!(settled.map_err(|e| e.message), Ok(false));
        let (output, _) = session.latest_output().await.unwrap();
        assert_eq!(
            session.events.outputs.load(Ordering::SeqCst),
            builds,
            "the deletion's batch built again"
        );
        assert!(!code(&output).contains("note-b"));
        session.close(&EngineSlot::default()).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file deleted behind a rebundle and written again before its own
    /// batch is rebundled is a new file: that rebundle waits for what the
    /// engine makes of it, and its output holds the new text.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_member_deleted_behind_a_rebundle_and_written_again_is_rebundled() {
        let (dir, session) = session_over(
            "glob-member-back",
            &[
                (
                    "src/entry.js",
                    "const notes = import.meta.glob(\"./notes/*.md\", { query: \"?raw\", eager: true, import: \"default\" });\nconsole.log(notes);\n",
                ),
                ("src/notes/a.md", "note-a\n"),
                ("src/notes/b.md", "note-b\n"),
            ],
        )
        .await;
        let created = dir.join("src/notes/c.md");
        let deleted = dir.join("src/notes/b.md");
        written_and_seen(&session, &created, "note-c\n").await;
        deleted_and_failed(&session, &deleted);
        let settled = session.settle(std::slice::from_ref(&created), true).await;
        assert_eq!(settled.map_err(|e| e.message), Ok(false));
        session.latest_output().await.unwrap();
        // The coordinator has taken the build's end in: its files are watched.
        let _ = session.engine.get_bundle_state().await;

        std::fs::write(&deleted, "note-again\n").unwrap();
        session
            .settle(std::slice::from_ref(&deleted), true)
            .await
            .unwrap();
        let (output, _) = session.latest_output().await.unwrap();
        let code = code(&output);
        assert!(code.contains("note-again"), "{code}");
        session.close(&EngineSlot::default()).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same deletion of a file a module still imports is the app's own
    /// failure: the full build the rebundle takes reports it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_deleted_behind_a_rebundle_that_is_still_imported_fails_it() {
        let (dir, session) = session_over(
            "import-deleted-race",
            &[
                (
                    "src/entry.js",
                    "import note from \"./notes/b.md?raw\";\nimport { other } from \"./other.js\";\nconsole.log(note, other);\n",
                ),
                ("src/notes/b.md", "note-b\n"),
                ("src/other.js", "export const other = 1;\n"),
            ],
        )
        .await;
        let edited = dir.join("src/other.js");
        written_and_seen(&session, &edited, "export const other = 2;\n").await;
        deleted_and_failed(&session, &dir.join("src/notes/b.md"));

        // The failed update is not the verdict: the full build is, and it
        // names the import that no longer loads.
        let settled = session.settle(std::slice::from_ref(&edited), true).await;
        assert_eq!(
            settled.map_err(|e| e.message),
            Ok(false),
            "the rebundle takes the full build"
        );
        let built = session
            .latest_output()
            .await
            .map(|(output, _)| code(&output));
        let failure = built.expect_err("the full build fails on the import of the deleted file");
        assert!(failure.message.contains("b.md?raw"), "{failure}");
        session.close(&EngineSlot::default()).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_patch_importing_what_never_resolved_is_an_error() {
        let declared = none_declared();
        let ok =
            "import { a } from \"./chunk.js\";\n__rolldown_runtime__.registerModule(\"x\", {});";
        assert!(
            unresolved_in_patch(ok, "/app/src/a.tsx", &declared).is_some(),
            "a relative file is no output of a patch"
        );
        let bare = "import { thing } from \"no-such-dep-typo\";\nexport { x } from 'https://cdn.test/x.js';";
        let err = unresolved_in_patch(bare, "/app/src/a.tsx?x", &declared).unwrap();
        assert!(
            err.message.contains("\"no-such-dep-typo\""),
            "{}",
            err.message
        );
        assert_eq!(err.id.as_deref(), Some("/app/src/a.tsx"));
        let text_only = "const s = \"import x from 'nope'\"; const d = import(name);";
        assert!(unresolved_in_patch(text_only, "/app/src/a.tsx", &declared).is_none());
        let ext: Declared = Arc::new(|s| s == "cdn-only-dep");
        assert!(unresolved_in_patch("import \"cdn-only-dep\";", "/app/a.ts", &ext).is_none());
    }

    // The engine's watcher reports an edit before the dev server's own does
    // (which waits for a quiet moment before it tells the gate): the gate
    // holds the files from the engine's report, so the patch made with it
    // waits for the flush instead of reaching the pages unheld.
    #[test]
    fn a_patch_is_held_from_the_engines_report_of_its_files() {
        use std::sync::atomic::AtomicBool;
        let hot = Arc::new(HotUpdates::new(""));
        let held = Arc::new(AtomicBool::new(false));
        let told: Arc<Mutex<Vec<String>>> = Arc::default();
        hot.set_gate({
            let held = Arc::clone(&held);
            Arc::new(move || held.load(Ordering::SeqCst))
        });
        hot.set_holder({
            let (held, told) = (Arc::clone(&held), Arc::clone(&told));
            Arc::new(move |files| {
                told.lock().unwrap().extend(files.iter().cloned());
                held.store(true, Ordering::SeqCst);
            })
        });
        let mut pages = hot.subscribe();
        let update = rolldown_common::ClientHmrUpdate {
            client_id: "c1".into(),
            update: HmrUpdate::Patch(rolldown_common::HmrPatch {
                code: "__rolldown_runtime__.registerFactory(\"src/a.tsx\", () => {});".into(),
                filename: "hmr_patch_0.js".into(),
                sourcemap: None,
                sourcemap_filename: None,
                changed_ids: vec!["src/a.tsx".into()],
                seq: 1,
                carried: Vec::new(),
            }),
        };
        on_hmr_updates(
            &Events::default(),
            Path::new("/app"),
            &hot,
            &none_declared(),
            Ok((vec![update], vec!["/app/src/a.tsx".into()])),
        );
        assert_eq!(*told.lock().unwrap(), ["/app/src/a.tsx"]);
        assert!(
            pages.try_recv().is_err(),
            "the patch reached the pages while the gate holds its file"
        );
        assert!(hot.has_parked());
        hot.release();
        let frame = pages.try_recv().expect("the flush sends the patch");
        assert_eq!(frame.client.as_deref(), Some("c1"));
    }

    // An edit made while the engine is rebuilding is reported after that
    // build: waiting one window for it would give it up (and reload the
    // pages) on a machine where the build outlasts the window.
    #[tokio::test(start_paused = true)]
    async fn an_edit_reported_after_a_running_build_is_waited_for() {
        let started = tokio::time::Instant::now();
        let ready = || started.elapsed() >= Duration::from_millis(2500);
        let window = Duration::from_millis(1000);
        let cap = Duration::from_secs(60);
        // The engine at work until the report: the wait goes on.
        let busy = || async { started.elapsed() < Duration::from_millis(2500) };
        assert!(reported(ready, busy, window, cap).await);
        // The engine idle: one window, as before.
        let started = tokio::time::Instant::now();
        let ready = || started.elapsed() >= Duration::from_millis(2500);
        assert!(!reported(ready, || async { false }, window, cap).await);
        assert!(started.elapsed() < Duration::from_millis(1100));
        // Never reported: bounded.
        let started = tokio::time::Instant::now();
        assert!(!reported(|| false, || async { true }, window, Duration::from_secs(5)).await);
        assert!(started.elapsed() <= Duration::from_secs(6));
    }

    #[test]
    fn watch_options_follow_server_watch() {
        let off = watch_options(None);
        assert_eq!(off.enabled, Some(false));
        let polling = watch_options(Some(
            &serde_json::json!({ "usePolling": true, "interval": 300, "debounceDuration": 50 }),
        ));
        let watcher = polling.watcher.as_ref().unwrap();
        assert_eq!(watcher.use_polling, Some(true));
        assert_eq!(watcher.poll_interval, Some(300));
        assert_eq!(watcher.debounce_delay, Some(50));
        // The tick follows the app's delay, so the debouncer's thread wakes
        // once a delay, not four times.
        assert_eq!(watcher.debounce_tick_rate, Some(50));
        assert!(polling.skip_write);
        assert_eq!(polling.enabled, None);
    }

    // The delay oj passes for an app that says nothing is the one the engine
    // would pick itself, so a rolldown bump that changes its default fails
    // here instead of leaving Start apps on a stale copy.
    #[test]
    fn engine_debounce_delay_is_the_engines_default() {
        let normalized = rolldown_dev_common::types::normalize_dev_options(
            rolldown_dev_common::types::DevOptions::default(),
        );
        assert_eq!(normalized.debounce_delay, ENGINE_DEBOUNCE_DELAY_MS);
    }

    // An app that says nothing about the watcher gets the engine's default
    // delay with the tick equal to it; one that sets its own tick keeps it.
    #[test]
    fn debounce_tick_defaults_to_the_delay() {
        let plain = watch_options(Some(&serde_json::json!({})));
        let watcher = plain.watcher.as_ref().unwrap();
        assert_eq!(watcher.debounce_delay, Some(ENGINE_DEBOUNCE_DELAY_MS));
        assert_eq!(watcher.debounce_tick_rate, Some(ENGINE_DEBOUNCE_DELAY_MS));
        let own = watch_options(Some(&serde_json::json!({ "debounceTickRate": 2 })));
        assert_eq!(own.watcher.as_ref().unwrap().debounce_tick_rate, Some(2));
    }

    #[test]
    fn new_styles_are_linked_once() {
        let dir = std::env::temp_dir().join(format!("oj-link-styles-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("css-urls.json"), "[\"/a.css\"]").unwrap();
        let added = link_new_styles(&dir, &["/a.css".into(), "/b.css".into()]).unwrap();
        assert_eq!(added, ["/b.css"]);
        assert_eq!(
            std::fs::read_to_string(dir.join("css-urls.json")).unwrap(),
            "[\"/a.css\",\"/b.css\"]"
        );
        assert!(link_new_styles(&dir, &["/a.css".into(), "/b.css".into()])
            .unwrap()
            .is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The full bundle wrote its links in execution order; a hot update's
    // list is in load order, and only adds to them.
    #[test]
    fn a_new_style_keeps_the_bundles_link_order() {
        let dir = std::env::temp_dir().join(format!("oj-link-order-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("css-urls.json"), "[\"/b.css\",\"/a.css\"]").unwrap();
        let added =
            link_new_styles(&dir, &["/a.css".into(), "/b.css".into(), "/c.css".into()]).unwrap();
        assert_eq!(added, ["/c.css"]);
        assert_eq!(
            std::fs::read_to_string(dir.join("css-urls.json")).unwrap(),
            "[\"/b.css\",\"/a.css\",\"/c.css\"]"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
