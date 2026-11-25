// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The app's chunk policy for a Start client bundle. Vite applies no chunk
//! grouping of its own: `build.rolldownOptions` passes through to rolldown in
//! the build and in bundled dev alike, so the grouping an app configures
//! (vendor chunks, i18n buckets) is the app's to make and oj's to honor:
//! `output.codeSplitting` (rolldown's current name; `advancedChunks` is the
//! earlier spelling), `experimental.chunkOptimization`, and
//! `preserveEntrySignatures` (false by default, as Vite's client app build
//! sets it). The client environment's options win over the top-level ones.
//!
//! The config is live in the plugin host, so a group's `name` and `test`
//! functions run there, in batches, as rolldown calls them under Vite:
//! `name` with `getModuleInfo` over the chunking pass's module graph, `test`
//! with the id alone.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use oj_server::plugins::{ChunkPass, PluginHost};
use rolldown_common::{
    ChunkFilenamesOutputOption, ChunkOptimizationOption, ChunkOptimizationOptions, ChunkingContext,
    CodeSplittingMode, ManualCodeSplittingOptions, MatchGroup, MatchGroupName, MatchGroupTest,
    ModuleTag, PreserveEntrySignatures, RollupPreRenderedChunk,
};
use serde_json::Value;

use super::facts::{fn_handle, js_regex, Facts};

pub(crate) struct ChunkPolicy {
    pub code_splitting: Option<CodeSplittingMode>,
    pub chunk_optimization: Option<ChunkOptimizationOption>,
    pub preserve_entry_signatures: PreserveEntrySignatures,
}

/// The policy `facts` describe. `dev`: a group whose function throws, or
/// whose host cannot run it, steps aside with a warning. That is oj's
/// leniency, not Vite's, whose bundled dev hands the functions to rolldown
/// as its build does (only its unbundled dev never calls them): a function
/// written for the build may read state only a build-only plugin produces,
/// and a dev server should outlive it. A build stays loud, as `vite build`
/// is.
pub(crate) fn chunk_policy(
    facts: &Facts,
    host: &Arc<PluginHost>,
    dev: bool,
) -> anyhow::Result<ChunkPolicy> {
    let code_splitting = match &facts.code_splitting {
        None => None,
        Some(Value::Bool(b)) => Some(CodeSplittingMode::Bool(*b)),
        Some(v @ Value::Object(_)) => Some(CodeSplittingMode::Advanced(manual(v, host, dev)?)),
        Some(other) => anyhow::bail!("output.codeSplitting: unsupported value {other}"),
    };
    let chunk_optimization = facts.chunk_optimization.as_ref().and_then(|v| match v {
        Value::Bool(b) => Some(ChunkOptimizationOption::Bool(*b)),
        Value::Object(o) => {
            let flag = |k: &str| o.get(k).and_then(Value::as_bool).unwrap_or(true);
            Some(ChunkOptimizationOption::Options(ChunkOptimizationOptions {
                merge_common_chunks: flag("mergeCommonChunks"),
                avoid_redundant_chunk_loads: flag("avoidRedundantChunkLoads"),
            }))
        }
        _ => None,
    });
    let preserve_entry_signatures = match &facts.preserve_entry_signatures {
        Some(Value::String(s)) if s == "strict" => PreserveEntrySignatures::Strict,
        Some(Value::String(s)) if s == "allow-extension" => PreserveEntrySignatures::AllowExtension,
        Some(Value::String(s)) if s == "exports-only" => PreserveEntrySignatures::ExportsOnly,
        _ => PreserveEntrySignatures::False,
    };
    Ok(ChunkPolicy {
        code_splitting,
        chunk_optimization,
        preserve_entry_signatures,
    })
}

fn manual(
    v: &Value,
    host: &Arc<PluginHost>,
    dev: bool,
) -> anyhow::Result<ManualCodeSplittingOptions> {
    let f64_of = |o: &Value, k: &str| o.get(k).and_then(Value::as_f64);
    let u32_of = |o: &Value, k: &str| o.get(k).and_then(Value::as_u64).map(|n| n as u32);
    let mut groups = Vec::new();
    for (index, g) in v
        .get("groups")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        if !g.is_object() {
            continue;
        }
        let label = match g.get("name") {
            Some(Value::String(s)) => s.clone(),
            _ => format!("#{index}"),
        };
        let guard = Arc::new(Guard::new(label.clone(), dev, host.engine_generation()));
        let name = match g.get("name") {
            Some(Value::String(s)) => MatchGroupName::Static(s.clone()),
            Some(f) if fn_handle(f).is_some() => MatchGroupName::Dynamic(name_fn(
                Arc::clone(host),
                fn_handle(f).unwrap(),
                Arc::clone(&guard),
            )),
            other => anyhow::bail!("codeSplitting group {label}: unsupported name {other:?}"),
        };
        let test = match g.get("test") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(MatchGroupTest::Regex(
                rolldown_utils::js_regex::HybridRegex::new(s)
                    .map_err(|e| anyhow::anyhow!("codeSplitting group {label}: test {s:?}: {e}"))?,
            )),
            Some(f) if fn_handle(f).is_some() => Some(MatchGroupTest::Function(test_fn(
                Arc::clone(host),
                fn_handle(f).unwrap(),
                guard,
            ))),
            Some(r) => Some(MatchGroupTest::Regex(js_regex(r).ok_or_else(|| {
                anyhow::anyhow!("codeSplitting group {label}: unsupported test {r}")
            })?)),
        };
        groups.push(MatchGroup {
            name,
            test,
            priority: u32_of(g, "priority"),
            min_size: f64_of(g, "minSize"),
            max_size: f64_of(g, "maxSize"),
            min_share_count: u32_of(g, "minShareCount"),
            min_module_size: f64_of(g, "minModuleSize"),
            max_module_size: f64_of(g, "maxModuleSize"),
            entries_aware: g.get("entriesAware").and_then(Value::as_bool),
            entries_aware_merge_threshold: f64_of(g, "entriesAwareMergeThreshold"),
            tags: group_tags(g),
            include_dependencies_recursively: g
                .get("includeDependenciesRecursively")
                .and_then(Value::as_bool),
        });
    }
    Ok(ManualCodeSplittingOptions {
        groups: (!groups.is_empty()).then_some(groups),
        min_share_count: u32_of(v, "minShareCount"),
        min_size: f64_of(v, "minSize"),
        max_size: f64_of(v, "maxSize"),
        min_module_size: f64_of(v, "minModuleSize"),
        max_module_size: f64_of(v, "maxModuleSize"),
        include_dependencies_recursively: v
            .get("includeDependenciesRecursively")
            .and_then(Value::as_bool),
        ..Default::default()
    })
}

/// A group's `tags` (`["$initial"]`: only modules the entries reach by static
/// imports), as rolldown's own options reader takes them. Without them a
/// group with no `test` captures every module.
pub(crate) fn group_tags(g: &Value) -> Option<Vec<ModuleTag>> {
    let tags = g.get("tags")?.as_array()?;
    Some(
        tags.iter()
            .filter_map(Value::as_str)
            .map(|tag| ModuleTag::from(tag.to_string()))
            .collect(),
    )
}

/// `output.entryFileNames` / `output.chunkFileNames` for a client build:
/// the app's pattern, or its function called in the plugin host with the
/// chunk as rolldown describes it before rendering (Rollup's
/// `PreRenderedChunk`), else `default`, the bundle's own pattern.
pub(crate) fn file_names(
    option: &'static str,
    value: Option<&Value>,
    host: Option<&Arc<PluginHost>>,
    default: &str,
) -> ChunkFilenamesOutputOption {
    match (value, host) {
        (Some(Value::String(pattern)), _) => pattern.clone().into(),
        (Some(f), Some(host)) if fn_handle(f).is_some() => {
            let handle = fn_handle(f).unwrap();
            let host = Arc::clone(host);
            ChunkFilenamesOutputOption::Fn(Arc::new(move |chunk: &RollupPreRenderedChunk| {
                let host = Arc::clone(&host);
                let chunk = serde_json::json!([{
                    "type": "chunk",
                    "name": chunk.name.as_str(),
                    "isEntry": chunk.is_entry,
                    "isDynamicEntry": chunk.is_dynamic_entry,
                    "facadeModuleId": chunk.facade_module_id.as_ref().map(|id| id.as_str()),
                    "moduleIds": chunk.module_ids.iter().map(|id| id.as_str()).collect::<Vec<_>>(),
                    "exports": chunk.exports.iter().map(|e| e.as_str()).collect::<Vec<_>>(),
                }]);
                Box::pin(async move {
                    let pattern = host
                        .call_start_fn(handle, &chunk)
                        .await
                        .map_err(|e| anyhow::anyhow!("output.{option}: {e}"))?;
                    pattern.as_str().map(str::to_string).ok_or_else(|| {
                        anyhow::anyhow!(
                            "output.{option} returned {pattern}, not a file name pattern"
                        )
                    })
                })
            }))
        }
        _ => default.to_string().into(),
    }
}

/// A group's dev tolerance: the group steps aside, with one warning for a
/// function that throws (it throws the same way every pass) and one for
/// each call the host could not run. rolldown calls a group's `test` once a
/// chunking pass and its `name` once after it, for the modules the test
/// let through, so that is a warning for each pass whose bundle lacks the
/// group, whichever of the two failed.
struct Guard {
    label: String,
    dev: bool,
    /// The host's engine when the bundle's policy was made, before any of
    /// the bundle's hooks ran: a `name` called on another one is of a
    /// bundle whose hooks' word on the modules is gone.
    engine: u64,
    warned: Mutex<bool>,
    /// The calls the host could not run.
    host_failures: std::sync::atomic::AtomicU64,
}

impl Guard {
    fn new(label: String, dev: bool, engine: u64) -> Self {
        Self {
            label,
            dev,
            engine,
            warned: Mutex::new(false),
            host_failures: Default::default(),
        }
    }

    /// The function's results, with dev throws turned into misses.
    fn results(&self, out: Vec<Value>) -> Vec<Option<Value>> {
        out.into_iter()
            .map(|r| match r.get("$threw").and_then(Value::as_str) {
                Some(message) => {
                    self.warn_once(&format!(
                        "threw under dev and steps aside (it may expect a build-only plugin, which did not run): {message}"
                    ));
                    None
                }
                None => Some(r),
            })
            .collect()
    }

    /// A call the host could not run at all (`error`, the host's): under
    /// dev the group steps aside for it, a miss per id, with a warning; a
    /// build fails, as rolldown fails `vite build` when a group's function
    /// cannot be called.
    fn step_aside(
        &self,
        what: &str,
        error: String,
        ids: usize,
    ) -> anyhow::Result<Vec<Option<Value>>> {
        if !self.dev {
            anyhow::bail!("codeSplitting group {} {what}: {error}", self.label);
        }
        self.host_failures
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        eprintln!(
            "oj: codeSplitting group {} {what} could not run under dev and steps aside for this bundle: {error}",
            self.label
        );
        Ok(vec![None; ids])
    }

    fn warn_once(&self, why: &str) {
        let mut warned = self.warned.lock().unwrap();
        if !*warned {
            *warned = true;
            eprintln!("oj: codeSplitting group {} {why}", self.label);
        }
    }
}

/// The module infos a chunking pass has sent to the host. A pass is its
/// [`ChunkingContext`]; holding it keeps a later pass from reusing its
/// address and reading as the same pass.
#[derive(Default)]
struct Sent {
    pass: Option<ChunkingContext>,
    generation: u64,
    ids: HashSet<String>,
    /// The host holds nothing of the pass: its next batch starts it over.
    fresh: bool,
}

/// A `name` batch's share of its pass.
#[derive(Debug, PartialEq)]
struct Batch {
    /// The pass.
    generation: u64,
    /// The module infos the host has not been sent this pass, as JSON.
    infos: Option<String>,
    /// The batch starts the pass over for the host: what it carries is all
    /// the host holds of the pass from then on. Said outright, since a
    /// batch may carry infos and still not be the pass's first (a part of
    /// the graph the earlier batches did not reach).
    fresh: bool,
}

static SENT: std::sync::LazyLock<Mutex<Sent>> = std::sync::LazyLock::new(Default::default);

/// The pass's generation and the infos the host has not seen this pass, as
/// Rollup's ModuleInfo (code left out): those of `ids` and of every module
/// reachable from them through imports and importers, so `getModuleInfo`
/// answers a walk of the graph, as it does under Vite, and not only for the
/// ids a function was called with.
fn infos_to_send(ids: &[String], ctx: &ChunkingContext) -> Batch {
    let mut sent = SENT.lock().unwrap();
    if !sent
        .pass
        .as_ref()
        .is_some_and(|pass| std::ptr::eq(&**pass, &**ctx))
    {
        *sent = Sent {
            pass: Some(ctx.clone()),
            generation: sent.generation + 1,
            ids: HashSet::new(),
            fresh: true,
        };
    }
    let fresh = std::mem::take(&mut sent.fresh);
    let list = |set: &rolldown_utils::indexmap::FxIndexSet<rolldown_common::ModuleId>| {
        set.iter()
            .map(|m| m.as_str().to_string())
            .collect::<Vec<_>>()
    };
    let mut infos = Vec::new();
    let mut queue: Vec<String> = ids.to_vec();
    while let Some(id) = queue.pop() {
        if !sent.ids.insert(id.clone()) {
            continue;
        }
        let Some(info) = ctx.get_module_info(&id) else {
            continue;
        };
        for linked in [
            &info.importers,
            &info.dynamic_importers,
            &info.imported_ids,
            &info.dynamically_imported_ids,
        ] {
            queue.extend(
                linked
                    .iter()
                    .map(|m| m.as_str())
                    .filter(|m| !sent.ids.contains(*m))
                    .map(str::to_string),
            );
        }
        infos.push(serde_json::json!({
            "id": info.id.as_str(),
            "isEntry": info.is_entry,
            "importers": list(&info.importers),
            "dynamicImporters": list(&info.dynamic_importers),
            "importedIds": list(&info.imported_ids),
            "dynamicallyImportedIds": list(&info.dynamically_imported_ids),
            "exports": info.exports.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
            "code": null,
        }));
    }
    Batch {
        generation: sent.generation,
        infos: (!infos.is_empty()).then(|| Value::Array(infos).to_string()),
        fresh,
    }
}

/// The host no longer holds what pass `generation` sent it (its engine was
/// respawned, or a batch carrying infos failed on the way): the pass's next
/// `name` batch starts it over.
fn forget_sent(generation: u64) {
    let mut sent = SENT.lock().unwrap();
    if sent.generation == generation {
        sent.ids.clear();
        sent.fresh = true;
    }
}

type NameFn = dyn Fn(
        Vec<String>,
        &ChunkingContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<Vec<Option<String>>>> + Send + 'static>,
    > + Send
    + Sync;
/// rolldown calls a group's `test` with the ids alone, as Rollup's `test(id)`.
type TestFn = dyn Fn(
        Vec<String>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<Vec<bool>>> + Send + 'static>,
    > + Send
    + Sync;

/// What a `name` batch is to its pass, for the engine running now: a batch
/// that starts the pass over (`fresh`) on an engine other than the one the
/// bundle's hooks ran on (`then`) starts it with their word on the modules
/// gone.
fn chunk_pass(fresh: bool, then: u64, now: u64) -> ChunkPass {
    match (fresh, then == now) {
        (false, _) => ChunkPass::Continue,
        (true, true) => ChunkPass::Start,
        (true, false) => ChunkPass::StartHooksLost,
    }
}

/// Whether a `name` batch is sent again from its pass's start: the engine
/// refused it (`stale`), or was respawned while the call was out (`before`
/// and `after`, the engine's generation around it).
fn starts_over(stale: bool, before: u64, after: u64) -> bool {
    stale || before != after
}

/// How often a `name` batch is sent again from its pass's start.
const RESENDS: usize = 3;

/// Sends a `name` batch (`send`, with what the batch is to its pass on the
/// engine running then), and again from the pass's start (`restart`) while
/// the engine refuses it or is respawned under the call: the answer then
/// came from an engine that knows nothing of the bundle and was not told
/// so. `bundle_engine` is the engine the bundle's hooks ran on, `engine`
/// the one running.
async fn send_name_batch<Send, Sent>(
    batch: Batch,
    bundle_engine: u64,
    engine: impl Fn() -> u64,
    restart: impl Fn() -> Batch,
    send: Send,
) -> Result<Vec<Value>, String>
where
    Send: Fn(Batch, ChunkPass) -> Sent,
    Sent: std::future::Future<Output = Result<Vec<Value>, String>>,
{
    let mut batch = batch;
    let mut resent = 0;
    loop {
        let before = engine();
        let pass = chunk_pass(batch.fresh, bundle_engine, before);
        let answer = send(batch, pass).await;
        let stale = answer
            .as_ref()
            .is_err_and(|e| e.contains(oj_server::plugins::STALE_CHUNK_INFOS));
        if !starts_over(stale, before, engine()) {
            return answer;
        }
        if resent == RESENDS {
            // Not an answer to go by: no engine stayed up for the pass.
            return Err(format!(
                "the plugin host was respawned under the chunking pass {} times",
                RESENDS + 1
            ));
        }
        resent += 1;
        batch = restart();
    }
}

fn name_fn(host: Arc<PluginHost>, handle: u64, guard: Arc<Guard>) -> Arc<NameFn> {
    Arc::new(move |ids: Vec<String>, ctx: &ChunkingContext| {
        let batch = infos_to_send(&ids, ctx);
        let host = Arc::clone(&host);
        let guard = Arc::clone(&guard);
        let ctx = ctx.clone();
        Box::pin(async move {
            let generation = batch.generation;
            let answer = send_name_batch(
                batch,
                guard.engine,
                || host.engine_generation(),
                || {
                    forget_sent(generation);
                    infos_to_send(&ids, &ctx)
                },
                |batch: Batch, pass| {
                    let (host, ids, dev) = (Arc::clone(&host), ids.clone(), guard.dev);
                    async move {
                        host.call_start_fn_batch(
                            handle,
                            &ids,
                            batch.generation,
                            batch.infos.as_deref(),
                            dev,
                            true,
                            pass,
                        )
                        .await
                    }
                },
            )
            .await;
            let out = match answer {
                Ok(out) => guard.results(out),
                Err(e) => {
                    // Whatever this batch carried may not have arrived.
                    forget_sent(generation);
                    guard.step_aside("name", e, ids.len())?
                }
            };
            Ok(out
                .into_iter()
                .map(|r| r.and_then(|v| v.as_str().map(str::to_string)))
                .collect())
        })
    })
}

fn test_fn(host: Arc<PluginHost>, handle: u64, guard: Arc<Guard>) -> Arc<TestFn> {
    Arc::new(move |ids: Vec<String>| {
        let host = Arc::clone(&host);
        let guard = Arc::clone(&guard);
        Box::pin(async move {
            let out = match host
                .call_start_fn_batch(handle, &ids, 0, None, guard.dev, false, ChunkPass::Continue)
                .await
            {
                Ok(out) => guard.results(out),
                Err(e) => guard.step_aside("test", e, ids.len())?,
            };
            Ok(out
                .into_iter()
                .map(|r| r.is_some_and(|v| truthy(&v)))
                .collect())
        })
    })
}

/// JavaScript truthiness of a JSON value.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_groups_tags_reach_rolldown() {
        let group =
            serde_json::json!({ "name": "app-boot", "tags": ["$initial"], "minShareCount": 1 });
        assert_eq!(group_tags(&group), Some(vec![ModuleTag::Initial]));
        assert_eq!(group_tags(&serde_json::json!({ "name": "vendor" })), None);
    }

    #[test]
    fn javascript_truthiness() {
        for (v, want) in [
            (json!(null), false),
            (json!(false), false),
            (json!(0), false),
            (json!(""), false),
            (json!(true), true),
            (json!(1), true),
            (json!("x"), true),
            (json!({}), true),
            (json!([]), true),
        ] {
            assert_eq!(truthy(&v), want, "{v}");
        }
    }

    fn pass(edges: &[(&str, &str)], lone: &[&str]) -> ChunkingContext {
        use rolldown_common::{ModuleId, ModuleInfo};
        let ids: Vec<&str> = edges
            .iter()
            .flat_map(|(a, b)| [*a, *b])
            .chain(lone.iter().copied())
            .collect();
        let map = rolldown_utils::dashmap::FxDashMap::default();
        for id in ids {
            let mut info = ModuleInfo {
                code: None,
                id: ModuleId::new(id),
                is_entry: false,
                importers: Default::default(),
                dynamic_importers: Default::default(),
                imported_ids: Default::default(),
                dynamically_imported_ids: Default::default(),
                exports: Vec::new(),
                input_format: Default::default(),
            };
            for (importer, imported) in edges {
                if *imported == id {
                    info.importers.insert(ModuleId::new(*importer));
                }
                if *importer == id {
                    info.imported_ids.insert(ModuleId::new(*imported));
                }
            }
            map.insert(arcstr::ArcStr::from(id), Arc::new(info));
        }
        ChunkingContext::new(Arc::new(map))
    }

    fn sent_ids(json: Option<String>) -> Vec<String> {
        let mut ids: Vec<String> = json
            .map(|j| serde_json::from_str::<Vec<Value>>(&j).unwrap())
            .unwrap_or_default()
            .iter()
            .map(|i| i["id"].as_str().unwrap().to_string())
            .collect();
        ids.sort();
        ids
    }

    // What each `name` batch of a pass carries. The first sends what a
    // walk from its ids reaches and starts the pass for the host; later
    // ones send only what the host lacks, which may be a part of the graph
    // no earlier batch reached, and do not start it over. A host that lost
    // the pass (a respawned engine, a batch that failed) is sent it from the
    // start by the next batch, which says so whether or not it has an info
    // to carry.
    #[test]
    fn a_pass_sends_its_module_infos_once_and_starts_over_for_a_host_that_lost_them() {
        // Two parts with no edge between them, as two entries make.
        let first = pass(&[("/entry", "/route"), ("/route", "/dep")], &["/lone"]);
        let batch = infos_to_send(&["/route".into()], &first);
        let generation = batch.generation;
        assert!(batch.fresh, "the pass's first batch");
        assert_eq!(sent_ids(batch.infos), ["/dep", "/entry", "/route"]);
        // The other part: infos to carry, and still not the pass's start.
        let batch = infos_to_send(&["/entry".into(), "/lone".into()], &first);
        assert_eq!(batch.generation, generation);
        assert!(!batch.fresh, "a later batch, though it carries infos");
        assert_eq!(sent_ids(batch.infos), ["/lone"]);
        assert_eq!(
            infos_to_send(&["/lone".into()], &first),
            Batch {
                generation,
                infos: None,
                fresh: false
            }
        );

        // A new pass is a new generation and sends its own graph.
        let second = pass(&[("/entry", "/route")], &["/lone"]);
        let batch = infos_to_send(&["/entry".into()], &second);
        let next = batch.generation;
        assert_ne!(next, generation);
        assert!(batch.fresh);
        assert_eq!(sent_ids(batch.infos), ["/entry", "/route"]);

        // Another pass's generation forgets nothing of this one.
        forget_sent(generation);
        assert_eq!(
            infos_to_send(&["/route".into()], &second),
            Batch {
                generation: next,
                infos: None,
                fresh: false
            }
        );
        // The host lost the pass: the next batch starts it over, under the
        // same generation, with the walk from its own ids. The part it does
        // not reach follows with the batch that does, not as a start.
        forget_sent(next);
        let batch = infos_to_send(&["/lone".into()], &second);
        assert_eq!(batch.generation, next);
        assert!(batch.fresh);
        assert_eq!(sent_ids(batch.infos), ["/lone"]);
        let batch = infos_to_send(&["/route".into()], &second);
        assert!(!batch.fresh);
        assert_eq!(sent_ids(batch.infos), ["/entry", "/route"]);

        // A start with no info to carry is a start all the same.
        forget_sent(next);
        assert_eq!(
            infos_to_send(&["/not-a-module".into()], &second),
            Batch {
                generation: next,
                infos: None,
                fresh: true
            }
        );
        assert!(!infos_to_send(&["/not-a-module".into()], &second).fresh);
        let third = pass(&[("/entry", "/route")], &[]);
        let batch = infos_to_send(&["/not-a-module".into()], &third);
        assert!(batch.fresh && batch.infos.is_none() && batch.generation != next);
    }

    #[test]
    fn dev_throws_step_aside_once() {
        let guard = Guard::new("vendor".into(), true, 0);
        let out = guard.results(vec![
            json!("a"),
            json!({ "$threw": "boom" }),
            json!({ "$threw": "boom" }),
        ]);
        assert_eq!(out, vec![Some(json!("a")), None, None]);
        assert!(*guard.warned.lock().unwrap());
    }

    // A batch that starts its pass over does so with the hooks' word on
    // the modules gone when the engine is no longer the one the bundle's
    // hooks ran on; a later batch of a pass only continues it, and is
    // refused by an engine that holds nothing of it.
    #[test]
    fn a_pass_started_on_another_engine_has_lost_what_the_hooks_said() {
        assert_eq!(chunk_pass(true, 3, 3), ChunkPass::Start);
        assert_eq!(chunk_pass(true, 3, 4), ChunkPass::StartHooksLost);
        assert_eq!(chunk_pass(true, 0, 2), ChunkPass::StartHooksLost);
        assert_eq!(chunk_pass(false, 3, 3), ChunkPass::Continue);
        assert_eq!(chunk_pass(false, 3, 4), ChunkPass::Continue);
        assert_eq!(Guard::new("vendor".into(), true, 7).engine, 7);
    }

    // A batch goes out again from its pass's start when the engine refused
    // it, and when the engine was respawned under the call: the answer then
    // came from an engine told nothing of the bundle. Sent again, the batch
    // starts the pass with the hooks' word gone, since the engine is no
    // longer the bundle's.
    #[test]
    fn a_batch_whose_call_respawned_the_engine_starts_the_pass_over() {
        assert!(!starts_over(false, 4, 4));
        assert!(starts_over(true, 4, 4), "refused by the engine");
        assert!(starts_over(false, 4, 5), "respawned under the call");
        assert!(starts_over(true, 4, 5));
        // The bundle's engine was 4: the batch sent again says what is lost.
        assert_eq!(chunk_pass(true, 4, 5), ChunkPass::StartHooksLost);
    }

    /// A plugin host for [`send_name_batch`]: `script` is what each call
    /// does in turn (answer, refuse as stale, or be respawned under the
    /// call and answer from the new engine). Records what each call was
    /// sent.
    struct FakeHost {
        engine: std::sync::atomic::AtomicU64,
        script: Mutex<std::collections::VecDeque<&'static str>>,
        sent: Mutex<Vec<(u64, ChunkPass, bool)>>,
        restarts: std::sync::atomic::AtomicU64,
    }

    impl FakeHost {
        fn new(engine: u64, script: &[&'static str]) -> Self {
            Self {
                engine: engine.into(),
                script: Mutex::new(script.iter().copied().collect()),
                sent: Mutex::new(Vec::new()),
                restarts: 0.into(),
            }
        }

        async fn run(&self, first: Batch, bundle_engine: u64) -> Result<Vec<Value>, String> {
            use std::sync::atomic::Ordering::SeqCst;
            send_name_batch(
                first,
                bundle_engine,
                || self.engine.load(SeqCst),
                || {
                    self.restarts.fetch_add(1, SeqCst);
                    Batch {
                        generation: 9,
                        infos: Some("[]".into()),
                        fresh: true,
                    }
                },
                |batch: Batch, pass| async move {
                    self.sent.lock().unwrap().push((
                        self.engine.load(SeqCst),
                        pass,
                        batch.infos.is_some(),
                    ));
                    match self
                        .script
                        .lock()
                        .unwrap()
                        .pop_front()
                        .expect("a scripted call")
                    {
                        "answer" => Ok(vec![json!(format!("engine-{}", self.engine.load(SeqCst)))]),
                        "stale" => Err(format!("Error: {}", oj_server::plugins::STALE_CHUNK_INFOS)),
                        "respawn" => {
                            let now = self.engine.fetch_add(1, SeqCst) + 1;
                            Ok(vec![json!(format!("untold-engine-{now}"))])
                        }
                        "fail" => Err("plugin host exited".into()),
                        other => panic!("{other}"),
                    }
                },
            )
            .await
        }

        fn sent(&self) -> Vec<(u64, ChunkPass, bool)> {
            self.sent.lock().unwrap().clone()
        }
    }

    fn later_batch() -> Batch {
        Batch {
            generation: 9,
            infos: None,
            fresh: false,
        }
    }

    fn first_batch() -> Batch {
        Batch {
            generation: 9,
            infos: Some("[]".into()),
            fresh: true,
        }
    }

    // A `name` batch as it goes to the host: once when the engine answers
    // it; again from the pass's start when the engine refuses it, or was
    // respawned under the call (the answer of an engine told nothing is not
    // kept), each time as what the batch is on the engine then running; and
    // not for ever.
    #[tokio::test]
    async fn a_name_batch_is_sent_again_until_an_engine_that_was_told_answers() {
        use ChunkPass::{Continue, Start, StartHooksLost};
        // The bundle's engine answers: one call, as the batch stands.
        let host = FakeHost::new(4, &["answer"]);
        assert_eq!(
            host.run(first_batch(), 4).await,
            Ok(vec![json!("engine-4")])
        );
        assert_eq!(host.sent(), [(4, Start, true)]);
        let host = FakeHost::new(4, &["answer"]);
        assert_eq!(
            host.run(later_batch(), 4).await,
            Ok(vec![json!("engine-4")])
        );
        assert_eq!(host.sent(), [(4, Continue, false)]);

        // Respawned before the batch (by another hook): refused, and the
        // pass starts over there with the hooks' word gone.
        let host = FakeHost::new(5, &["stale", "answer"]);
        assert_eq!(
            host.run(later_batch(), 4).await,
            Ok(vec![json!("engine-5")])
        );
        assert_eq!(
            host.sent(),
            [(5, Continue, false), (5, StartHooksLost, true)]
        );
        assert_eq!(host.restarts.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Respawned under the call: the first answer is dropped though it
        // is one, for it came from an engine told the pass was whole.
        let host = FakeHost::new(4, &["respawn", "answer"]);
        assert_eq!(
            host.run(first_batch(), 4).await,
            Ok(vec![json!("engine-5")])
        );
        assert_eq!(host.sent(), [(4, Start, true), (5, StartHooksLost, true)]);

        // And under the call sent again.
        let host = FakeHost::new(4, &["respawn", "respawn", "answer"]);
        assert_eq!(
            host.run(later_batch(), 4).await,
            Ok(vec![json!("engine-6")])
        );
        assert_eq!(
            host.sent(),
            [
                (4, Continue, false),
                (5, StartHooksLost, true),
                (6, StartHooksLost, true)
            ]
        );

        // A host that failed otherwise is not asked again.
        let host = FakeHost::new(4, &["fail"]);
        assert_eq!(
            host.run(first_batch(), 4).await,
            Err("plugin host exited".into())
        );
        assert_eq!(host.sent().len(), 1);

        // An engine that never stays up: an error, not the last answer.
        let host = FakeHost::new(4, &["respawn"; 8]);
        let gave_up = host.run(first_batch(), 4).await;
        assert!(
            gave_up
                .as_ref()
                .is_err_and(|e| e.contains("respawned under the chunking pass")),
            "{gave_up:?}"
        );
        assert_eq!(host.sent().len(), RESENDS + 1);
    }

    fn host_failures(guard: &Guard) -> u64 {
        guard
            .host_failures
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    #[test]
    fn a_host_that_cannot_run_the_function_steps_aside_under_dev_and_fails_a_build() {
        let dev = Guard::new("#0".into(), true, 0);
        let out = dev
            .step_aside("name", "oj: no config function #0".into(), 2)
            .expect("dev steps aside");
        assert_eq!(out, vec![None, None]);
        assert_eq!(host_failures(&dev), 1);
        let build = Guard::new("#0".into(), false, 0);
        let err = build
            .step_aside("name", "oj: no config function #0".into(), 2)
            .expect_err("a build stays loud");
        assert_eq!(
            err.to_string(),
            "codeSplitting group #0 name: oj: no config function #0"
        );
    }

    // Stepping aside holds for the call that failed and no longer: the next
    // one the host answers is used as it comes. Each call the host could
    // not run is warned of, a `test` as a `name`, pass after pass (a `test`
    // that keeps failing keeps its `name` from ever being called, and each
    // of those bundles lacks the group), apart from the one warning a
    // throwing function gets.
    #[test]
    fn a_group_that_stepped_aside_answers_again_and_warns_of_every_call_the_host_missed() {
        let guard = Guard::new("vendor".into(), true, 0);
        assert_eq!(
            guard.results(vec![json!("a"), json!(null)]),
            vec![Some(json!("a")), Some(json!(null))],
            "answers pass through, a null one included"
        );
        assert!(!*guard.warned.lock().unwrap());
        assert_eq!(host_failures(&guard), 0);

        assert_eq!(
            guard
                .step_aside("test", "plugin host unresponsive".into(), 3)
                .unwrap(),
            vec![None, None, None],
            "a miss per id"
        );
        assert_eq!(host_failures(&guard), 1);
        assert!(!*guard.warned.lock().unwrap(), "not the throw's warning");

        assert_eq!(
            guard.results(vec![json!("b"), json!({ "$threw": "boom" }), json!(true)]),
            vec![Some(json!("b")), None, Some(json!(true))],
            "the call after is used as it comes"
        );
        assert!(*guard.warned.lock().unwrap());
        assert_eq!(host_failures(&guard), 1);

        // The same function failing pass after pass, and the other one.
        for pass in 2..=4 {
            guard
                .step_aside("test", "plugin host exited".into(), 1)
                .unwrap();
            assert_eq!(host_failures(&guard), pass);
        }
        assert_eq!(
            guard.step_aside("name", "again".into(), 0).unwrap(),
            Vec::<Option<Value>>::new()
        );
        assert_eq!(host_failures(&guard), 5);
    }

    // A build names the group and which of its functions failed, for a
    // group with a static name as for one without.
    #[test]
    fn a_build_names_the_group_and_the_function_that_could_not_run() {
        for (label, what, want) in [
            (
                "vendor",
                "test",
                "codeSplitting group vendor test: host gone",
            ),
            ("#3", "name", "codeSplitting group #3 name: host gone"),
        ] {
            let build = Guard::new(label.into(), false, 0);
            let err = build.step_aside(what, "host gone".into(), 1).unwrap_err();
            assert_eq!(err.to_string(), want);
            assert_eq!(host_failures(&build), 0, "a build fails, it does not warn");
        }
    }
}
