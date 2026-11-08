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

use oj_compiler::glob::GlobCall;
use rolldown_common::{HmrUpdate, Output, WatcherFileWatcherOptions};
use rolldown_dev::{BundlerConfig, DevEngine, DevOptions, DevWatchOptions, ErrorStage};
use rolldown_plugin::__inner::SharedPluginable;
use rolldown_utils::pattern_filter::StringOrRegex;

use super::assets::DevStyleState;
use super::errors::{unresolved_import, unresolved_import_in, BundleError};
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

#[derive(Default)]
struct Events {
    seen: Mutex<HashMap<String, Seen>>,
    /// The engine's last failed update (a syntax error), for the rebundle
    /// that reports it.
    error: Mutex<Option<BundleError>>,
    output: Mutex<Option<Result<Vec<Output>, BundleError>>>,
    outputs: AtomicU64,
    changed: tokio::sync::Notify,
}

/// One engine session and the state its plugins write into.
pub(crate) struct HotSession {
    pub engine: Arc<DevEngine>,
    /// The style epoch the session compiled its sheets under.
    pub epoch: u64,
    pub css_urls: Arc<Mutex<super::assets::LinkedSheets>>,
    pub glob_scopes: Arc<Mutex<Vec<GlobCall>>>,
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
    pub glob_scopes: Arc<Mutex<Vec<GlobCall>>>,
    pub styles: Arc<DevStyleState>,
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
                    *events.output.lock().unwrap() = Some(recorded);
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
    /// watches the same files the dev server does, usually sooner; an input
    /// it has no module for (a file the bundle only declared, a create under a
    /// glob's scan base) rebuilds in full — except a style input, which is
    /// the CSS pipeline's (see `unknown_forces_build`): the batch still
    /// reloads, it just skips the rebuild.
    pub async fn settle(&self, changed: &[PathBuf], has_pages: bool) -> Result<bool, BundleError> {
        let debug = oj_env::get().knobs.debug_hot_settle;
        let t0 = tokio::time::Instant::now();
        let known: Vec<String> = changed
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .filter(|p| self.engine.get_module_info(p).is_some())
            .collect();
        // The spelling probe: a path the engine knows only under another
        // spelling (canonicalized, or the reverse) reads as unknown above and
        // forces a full build. Only computed under the debug flag: one stat
        // per unknown path per batch.
        let unknown: Vec<String> = if debug {
            changed
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .filter(|p| self.engine.get_module_info(p).is_none())
                .collect()
        } else {
            Vec::new()
        };
        let deadline = t0 + Duration::from_millis(1000);
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
        if let Some(err) = self.events.error.lock().unwrap().take() {
            let mut seen = self.events.seen.lock().unwrap();
            for p in &known {
                seen.remove(p);
            }
            if debug {
                eprintln!(
                    "{}: hot settle: changed={changed:?} known={known:?} has_pages={has_pages} waited={:?} error={err:?} => full build (engine error)",
                    oj_server::oj_brand(),
                    t0.elapsed(),
                );
            }
            return Err(err);
        }
        let kinds: Vec<Option<Seen>> = {
            let mut seen = self.events.seen.lock().unwrap();
            known.iter().map(|p| seen.remove(p)).collect()
        };
        let unknown = if debug {
            unknown
        } else {
            changed
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .filter(|p| self.engine.get_module_info(p).is_none())
                .collect()
        };
        // Style inputs the engine never knew are the CSS pipeline's, not its
        // module graph's (a plain sheet, a partial, a css config): they must
        // not force the engine's full build. The verdict below still treats
        // every unknown as not-hot, so the batch's reload applies the new
        // rules, exactly as before — only the rebuild is skipped.
        let build_unknown = unknown.iter().filter(|p| unknown_forces_build(p)).count();
        if build_unknown > 0 || !watched {
            let _ = self.engine.trigger_full_build();
        }
        let hot = settle_verdict(has_pages, known.len(), unknown.len(), watched, &kinds);
        if debug {
            // Alternate spellings for each unknown path, to tell an id
            // mismatch (engine knows the canonicalized path, or the
            // watcher/closure spelling differs) from a genuinely new module.
            let mut probes = Vec::new();
            for p in &unknown {
                let direct = self.engine.get_module_info(p).is_some();
                let canon = std::fs::canonicalize(p)
                    .ok()
                    .map(|c| c.to_string_lossy().into_owned());
                let canon_hit = canon
                    .as_ref()
                    .is_some_and(|c| self.engine.get_module_info(c).is_some());
                probes.push(format!(
                    "{p} direct={direct} canonical={canon:?} canonical_hit={canon_hit}"
                ));
            }
            let seen_keys: Vec<String> =
                { self.events.seen.lock().unwrap().keys().cloned().collect() };
            eprintln!(
                "{}: hot settle: changed={changed:?} known={known:?} unknown_probes={probes:?} build_unknown={build_unknown} has_pages={has_pages} watched={watched} waited={:?} kinds={kinds:?} leftover_seen={seen_keys:?} => {}",
                oj_server::oj_brand(),
                t0.elapsed(),
                if hot { "hot" } else { "full build" },
            );
        }
        Ok(hot)
    }

    /// The engine's current output: what a rebundle writes for reloads and
    /// the bundle cache. Waits out a build the engine still owes; after a
    /// failed update, forces the full build that recovers it (Vite does when
    /// a page needs the output).
    pub async fn latest_output(&self) -> Result<Vec<Output>, BundleError> {
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
            Some(result) => result,
            None => Err(BundleError::from_message(
                "the client bundle produced no output",
            )),
        }
    }
}

/// An unknown input that forces the engine's full build: everything but a
/// style input. Style files are owned by the CSS pipeline, not the JS module
/// graph — the engine never holds them as modules, so every css edit would
/// otherwise rebuild the client in full. Skipping the rebuild is safe
/// because the verdict still counts them unknown (not hot), so the batch
/// reloads and the pages fetch the recompiled sheets, exactly as before.
fn unknown_forces_build(path: &str) -> bool {
    !crate::start_dev::is_style_input(Path::new(path))
}

/// The settle verdict from one batch's inputs: hot only when every changed
/// path is a known engine module seen patched (or a no-op) in time, with a
/// page to take the patch. Unknown or unwatched inputs, engine errors,
/// reloads, and unseen rebuilds keep the full-build fallback (genuinely
/// unknown/unwatched changes still rebuild in full).
fn settle_verdict(
    has_pages: bool,
    known: usize,
    unknown: usize,
    watched: bool,
    kinds: &[Option<Seen>],
) -> bool {
    has_pages
        && known > 0
        && watched
        && unknown == 0
        && kinds
            .iter()
            .all(|k| matches!(k, Some(Seen::Patch | Seen::Noop)))
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
    if oj_env::get().knobs.debug_hot_settle {
        eprintln!(
            "{}: hot engine update: changed={changed:?} kind={kind:?} updates={}",
            oj_server::oj_brand(),
            updates.len(),
        );
    }
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
    for u in updates {
        let HmrUpdate::Patch(patch) = u.update else {
            continue;
        };
        // An import that never resolved fails the rebundle (the overlay),
        // never the page that would run the patch.
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

    /// The settle truth table: only a fully-known, fully-watched, patched
    /// (or no-op) batch with a page to take it goes hot. Everything else —
    /// unknown or unwatched inputs, no pages, reloads, unseen rebuilds —
    /// keeps the full-build fallback.
    #[test]
    fn settle_verdict_keeps_the_full_build_fallback() {
        use Seen::*;
        let hot = |kinds: &[Option<Seen>]| settle_verdict(true, 1, 0, true, kinds);
        assert!(hot(&[Some(Patch)]), "a patched batch with a page goes hot");
        assert!(
            hot(&[Some(Noop)]),
            "a no-op batch (identical content) goes hot"
        );
        assert!(
            hot(&[Some(Patch), Some(Noop)]),
            "a mixed patched/no-op batch goes hot"
        );
        assert!(
            !hot(&[Some(Patch), Some(Reload)]),
            "a batch the engine reloads for rebuilds in full"
        );
        assert!(
            !hot(&[Some(Unseen)]),
            "an engine rebuild with no update for any page rebuilds in full"
        );
        assert!(
            !hot(&[None]),
            "an input the engine never reported rebuilds in full"
        );
        assert!(
            !settle_verdict(true, 1, 1, true, &[Some(Patch)]),
            "one unknown input rebuilds the whole batch in full"
        );
        assert!(
            !settle_verdict(true, 1, 0, false, &[Some(Patch)]),
            "an input the watcher missed rebuilds in full"
        );
        assert!(
            !settle_verdict(false, 1, 0, true, &[Some(Patch)]),
            "with no page to take the patch (a curl-only bench) the output rebuilds in full"
        );
        assert!(
            !settle_verdict(true, 0, 0, true, &[]),
            "an empty batch is not a hot update"
        );
    }

    /// Style inputs skip the engine's rebuild trigger, but never go hot:
    /// the batch still reloads, which is what applies the new rules.
    #[test]
    fn style_inputs_skip_the_engine_build_but_keep_the_reload() {
        assert!(
            !unknown_forces_build("/app/src/globals.css"),
            "a sheet is the CSS pipeline's, not the module graph's"
        );
        assert!(
            !unknown_forces_build("/app/src/_vars.scss"),
            "a partial feeds sheets, not modules"
        );
        assert!(
            !unknown_forces_build("/app/tailwind.config.js"),
            "the tailwind config feeds sheets, not modules"
        );
        assert!(
            !unknown_forces_build("/app/postcss.config.mjs"),
            "the postcss config feeds sheets, not modules"
        );
        assert!(
            unknown_forces_build("/app/src/app.tsx"),
            "a JS module the engine never knew still rebuilds in full"
        );
        assert!(
            unknown_forces_build("/app/README.md"),
            "any other unknown input still rebuilds in full"
        );
        assert!(
            !settle_verdict(true, 0, 1, true, &[]),
            "a style-only unknown batch still reloads, so the pages fetch the recompiled sheets"
        );
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
