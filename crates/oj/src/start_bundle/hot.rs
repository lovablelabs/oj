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
use rolldown_common::{HmrUpdate, Output};
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
    pub css_urls: Arc<Mutex<Vec<String>>>,
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
    pub css_urls: Arc<Mutex<Vec<String>>>,
    pub glob_scopes: Arc<Mutex<Vec<GlobCall>>>,
    pub styles: Arc<DevStyleState>,
}

impl HotSession {
    /// Starts an engine on `parts` (its initial full build included).
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        mut parts: SessionParts,
        root: &Path,
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
        let _ = engine.run().await;
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
    /// glob's scan base) rebuilds in full.
    pub async fn settle(&self, changed: &[PathBuf], has_pages: bool) -> Result<bool, BundleError> {
        let known: Vec<String> = changed
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .filter(|p| self.engine.get_module_info(p).is_some())
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
        if let Some(err) = self.events.error.lock().unwrap().take() {
            let mut seen = self.events.seen.lock().unwrap();
            for p in &known {
                seen.remove(p);
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
            disable_watcher: Some(true),
            skip_write: Some(true),
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
    DevWatchOptions {
        skip_write: Some(true),
        use_polling: w.get("usePolling").and_then(|v| v.as_bool()),
        poll_interval: w
            .get("pollInterval")
            .or_else(|| w.get("interval"))
            .and_then(|v| v.as_u64()),
        use_debounce: w.get("useDebounce").and_then(|v| v.as_bool()),
        debounce_duration: w.get("debounceDuration").and_then(|v| v.as_u64()),
        debounce_tick_rate: w.get("debounceTickRate").and_then(|v| v.as_u64()),
        compare_contents_for_polling: w.get("compareContentsForPolling").and_then(|v| v.as_bool()),
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
        let tmp = cache.join("css-urls.json.tmp");
        std::fs::write(&tmp, serde_json::to_string(css_urls)?)?;
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
        assert_eq!(off.disable_watcher, Some(true));
        let polling = watch_options(Some(
            &serde_json::json!({ "usePolling": true, "interval": 300 }),
        ));
        assert_eq!(polling.use_polling, Some(true));
        assert_eq!(polling.poll_interval, Some(300));
        assert_eq!(polling.skip_write, Some(true));
        assert_eq!(polling.disable_watcher, None);
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
}
