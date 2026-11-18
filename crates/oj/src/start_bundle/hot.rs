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
    changed: tokio::sync::Notify,
}

/// The chunks hot updates left behind: ones the pages may still name whose
/// modules were edited since.
///
/// A hot update re-runs a changed module on the pages that run it. On a
/// page that never loaded it the patch has nothing to re-run, and the page's
/// first import of the module is not the runtime's: its entry imports the
/// module's chunk by the name it had before the edit, which the dev server
/// keeps serving for pages held behind a reload (see
/// `start_chunks::ServedBundles`). Served to a hot page, that chunk is the
/// module as it was before the edit, for as long as the page stays open.
/// Vite's dev server serves a module at its current code whenever it is
/// first imported (plugins/importAnalysis.ts rewrites `import()` to the
/// module's url, server/transformRequest.ts transforms it on request). So
/// the chunks holding a hot-updated module are noted here once an output
/// renames them, and a page that asks for one is told it is gone and
/// reloads onto the current output (`start_dev::serve_client_chunk`).
#[derive(Default)]
pub(crate) struct LeftBehind {
    /// The files hot updates changed since the last output.
    changed: HashSet<String>,
    /// The chunks of the last output each file's modules are in.
    holders: HashMap<String, Vec<String>>,
    /// The chunks left behind, by name.
    names: HashSet<String>,
}

impl LeftBehind {
    /// A batch of edits went to the pages as hot updates.
    pub fn hot_updated(&mut self, changed: &[PathBuf]) {
        self.changed
            .extend(changed.iter().map(|p| p.to_string_lossy().into_owned()));
    }

    /// A new output, as `(chunk name, its module ids)`: the chunks of the
    /// output before it that hold a file hot-updated since are left behind,
    /// unless this output still has them under the same name (a save that
    /// changed nothing renames nothing, and an edit taken back brings the
    /// old name back). A module is counted under its file, whatever query
    /// it carries (a route's split-off component is `route.tsx?tsr-split=...`).
    pub fn output<'a>(&mut self, chunks: impl Iterator<Item = (&'a str, Vec<&'a str>)>) {
        let before = std::mem::take(&mut self.holders);
        let mut current = HashSet::new();
        for (name, modules) in chunks {
            current.insert(name);
            for id in modules {
                let file = id.split('?').next().unwrap_or(id);
                let held = self.holders.entry(file.to_string()).or_default();
                if !held.iter().any(|n| n == name) {
                    held.push(name.to_string());
                }
            }
        }
        for file in self.changed.drain() {
            self.names
                .extend(before.get(&file).into_iter().flatten().cloned());
        }
        self.names.retain(|name| !current.contains(name.as_str()));
    }

    pub fn has(&self, name: &str) -> bool {
        self.names.contains(name)
    }
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
            ..Default::default()
        });
        // Vite turns this off for its bundled dev too (vitejs/vite#21843).
        parts.options.optimization = Some(rolldown_common::OptimizationOption {
            inline_const: Some(rolldown_common::InlineConstOption::Bool(false)),
            ..Default::default()
        });
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
                    *events.output.lock().unwrap() = Some((recorded, loaded.snapshot()));
                    events.outputs.fetch_add(1, Ordering::SeqCst);
                    events.changed.notify_waiters();
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
        // (registering twice keeps the first session).
        slot.set(Some(Arc::clone(&engine)));
        for id in clients() {
            engine.register_client(id).await;
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
        let known: Vec<String> = changed
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .filter(|p| self.engine.get_module_info(p).is_some() || under_a_glob(p))
            .collect();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(1000);
        let watched = loop {
            let ready = self.events.error.lock().unwrap().is_some() || {
                let seen = self.events.seen.lock().unwrap();
                known.iter().all(|p| seen.contains_key(p))
            };
            if ready {
                break true;
            }
            if tokio::time::Instant::now() >= deadline {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        // A failed hot update over a deleted file is no verdict on the app:
        // the engine re-fetches every module that depends on a changed file,
        // a deleted file's own `?raw` module among them, which no longer
        // loads although nothing imports it any more once its glob's importer
        // re-expanded. The full build that follows (`latest_output`) starts
        // over from the entries, as Vite's bundled dev rebuilds after a
        // failed update when a page needs the output; its error, if it has
        // one, is the one to report. A failure with every changed file still
        // on disk (a syntax error mid-edit) is the edit's own, reported as
        // it is: the overlay shows it without a full build first.
        if let Some(err) = self.events.error.lock().unwrap().take() {
            let mut seen = self.events.seen.lock().unwrap();
            for p in &known {
                seen.remove(p);
            }
            if changed.iter().any(|p| !p.exists()) {
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
        let before = self.events.outputs.load(Ordering::SeqCst);
        let _ = self.engine.wait_for_ongoing_bundle().await;
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

fn on_hmr_updates(
    events: &Events,
    root: &Path,
    hot: &HotUpdates,
    declared: &Declared,
    result: rolldown_error::BuildResult<(Vec<rolldown_common::ClientHmrUpdate>, Vec<String>)>,
) {
    let (updates, changed) = match result {
        Ok(ok) => ok,
        Err(errs) => {
            // A failed update reaches the page through the rebundle that follows.
            *events.error.lock().unwrap() = Some(BundleError::from_diagnostics(&errs.into_vec()));
            return;
        }
    };
    *events.error.lock().unwrap() = None;
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
    let watcher = WatcherFileWatcherOptions {
        use_polling: w.get("usePolling").and_then(|v| v.as_bool()),
        poll_interval: w
            .get("pollInterval")
            .or_else(|| w.get("interval"))
            .and_then(|v| v.as_u64()),
        compare_contents_for_polling: w.get("compareContentsForPolling").and_then(|v| v.as_bool()),
        use_debounce: w.get("useDebounce").and_then(|v| v.as_bool()),
        debounce_delay: w
            .get("debounceDelay")
            .or_else(|| w.get("debounceDuration"))
            .and_then(|v| v.as_u64()),
        debounce_tick_rate: w.get("debounceTickRate").and_then(|v| v.as_u64()),
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

    #[test]
    fn a_chunk_holding_a_hot_updated_module_is_left_behind_once_renamed() {
        let mut left = LeftBehind::default();
        let entry = ("client-entry.js", vec!["/app/src/main.tsx"]);
        let about = |name: &'static str| (name, vec!["/app/src/about.tsx?tsr-split=component"]);
        let note = |name: &'static str| (name, vec!["/app/src/note.ts", "/app/src/words.ts"]);
        left.output([entry.clone(), about("about-1.js"), note("note-1.js")].into_iter());
        assert!(!left.has("note-1.js"));

        // A save that changed nothing: the output names the chunk as before.
        left.hot_updated(&[PathBuf::from("/app/src/note.ts")]);
        left.output([entry.clone(), about("about-1.js"), note("note-1.js")].into_iter());
        assert!(!left.has("note-1.js"), "the chunk was not renamed");

        // An edit: the chunk it renamed is left behind, the others are not.
        left.hot_updated(&[PathBuf::from("/app/src/note.ts")]);
        left.output([entry.clone(), about("about-1.js"), note("note-2.js")].into_iter());
        assert!(left.has("note-1.js"));
        assert!(!left.has("note-2.js") && !left.has("about-1.js") && !left.has("client-entry.js"));

        // A module is its file's, whatever query it carries; the entry keeps
        // its name, so it is never left behind.
        left.hot_updated(&[
            PathBuf::from("/app/src/about.tsx"),
            PathBuf::from("/app/src/main.tsx"),
        ]);
        left.output([entry.clone(), about("about-2.js"), note("note-2.js")].into_iter());
        assert!(left.has("about-1.js") && left.has("note-1.js"));
        assert!(!left.has("client-entry.js") && !left.has("note-2.js"));

        // A chunk renamed with no hot update to its modules (a rebuild the
        // pages reload for) stays served to the pages held behind it.
        left.output([entry.clone(), about("about-3.js"), note("note-3.js")].into_iter());
        assert!(!left.has("about-2.js") && !left.has("note-2.js"));

        // An edit taken back: the first name is the current chunk again.
        left.hot_updated(&[PathBuf::from("/app/src/note.ts")]);
        left.output([entry, about("about-3.js"), note("note-1.js")].into_iter());
        assert!(!left.has("note-1.js"), "the current output has it");
        assert!(left.has("note-3.js"));
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
        assert!(polling.skip_write);
        assert_eq!(polling.enabled, None);
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
