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

use oj_server::plugins::PluginHost;
use rolldown_common::{
    ChunkOptimizationOption, ChunkOptimizationOptions, ChunkingContext, CodeSplittingMode,
    ManualCodeSplittingOptions, MatchGroup, MatchGroupName, MatchGroupTest,
    PreserveEntrySignatures,
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
        let guard = Arc::new(Guard::new(label.clone(), dev));
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
            include_dependencies_recursively: g
                .get("includeDependenciesRecursively")
                .and_then(Value::as_bool),
            ..Default::default()
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

/// A group's dev tolerance: the group steps aside, with one warning for a
/// function that throws (it throws the same way every pass) and one per
/// chunking pass for a host that could not run it (each is an outage of its
/// own, and the bundle that pass wrote lacks the group).
struct Guard {
    label: String,
    dev: bool,
    warned: Mutex<bool>,
    /// The pass the last host failure was warned of.
    host_warned: Mutex<Option<u64>>,
}

impl Guard {
    fn new(label: String, dev: bool) -> Self {
        Self {
            label,
            dev,
            warned: Mutex::new(false),
            host_warned: Mutex::new(None),
        }
    }

    /// The function's results, with dev throws turned into misses.
    fn results(&self, out: Vec<Value>) -> Vec<Option<Value>> {
        out.into_iter()
            .map(|r| match r.get("$threw").and_then(Value::as_str) {
                Some(message) => {
                    self.warn_once(&format!(
                        "threw under dev and steps aside (a build-only plugin it expects did not run): {message}"
                    ));
                    None
                }
                None => Some(r),
            })
            .collect()
    }

    /// A batch the host could not run at all (`error`, the host's): under
    /// dev the group steps aside for the batch, a miss per id, with one
    /// warning for the chunking pass (`generation`); a build fails, as
    /// rolldown fails `vite build` when a group's function cannot be called.
    fn step_aside(
        &self,
        what: &str,
        error: String,
        ids: usize,
        generation: u64,
    ) -> anyhow::Result<Vec<Option<Value>>> {
        if !self.dev {
            anyhow::bail!("codeSplitting group {} {what}: {error}", self.label);
        }
        let mut warned = self.host_warned.lock().unwrap();
        if *warned != Some(generation) {
            *warned = Some(generation);
            eprintln!(
                "oj: codeSplitting group {} {what} could not run under dev and steps aside for this bundle: {error}",
                self.label
            );
        }
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
    /// The host holds nothing of the pass: its next batch carries infos, an
    /// empty list when there are none, which tells the host the pass is new
    /// to it and not one whose infos an earlier engine took.
    fresh: bool,
}

static SENT: std::sync::LazyLock<Mutex<Sent>> = std::sync::LazyLock::new(Default::default);

/// The pass's generation and the infos the host has not seen this pass, as
/// Rollup's ModuleInfo (code left out): those of `ids` and of every module
/// reachable from them through imports and importers, so `getModuleInfo`
/// answers a walk of the graph, as it does under Vite, and not only for the
/// ids a function was called with.
fn infos_to_send(ids: &[String], ctx: &ChunkingContext) -> (u64, Option<String>) {
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
    let json = (fresh || !infos.is_empty()).then(|| Value::Array(infos).to_string());
    (sent.generation, json)
}

/// The host no longer holds what pass `generation` sent it (its engine was
/// respawned, or a batch carrying infos failed on the way): the pass's next
/// `name` batch sends them again.
fn forget_sent(generation: u64) {
    let mut sent = SENT.lock().unwrap();
    if sent.generation == generation {
        sent.ids.clear();
        sent.fresh = true;
    }
}

/// The generation of the pass in flight. rolldown calls a group's `test`
/// without the pass's context, between `name` batches of the same pass, so
/// the call names that pass and the host keeps its module infos.
fn current_generation() -> u64 {
    SENT.lock().unwrap().generation
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

fn name_fn(host: Arc<PluginHost>, handle: u64, guard: Arc<Guard>) -> Arc<NameFn> {
    Arc::new(move |ids: Vec<String>, ctx: &ChunkingContext| {
        let (generation, infos) = infos_to_send(&ids, ctx);
        let host = Arc::clone(&host);
        let guard = Arc::clone(&guard);
        let ctx = ctx.clone();
        Box::pin(async move {
            let mut answer = host
                .call_start_fn_batch(handle, &ids, generation, infos.as_deref(), guard.dev, true)
                .await;
            // The engine was respawned since the pass sent its infos: once
            // more, with them.
            if answer
                .as_ref()
                .is_err_and(|e| e.contains(oj_server::plugins::STALE_CHUNK_INFOS))
            {
                forget_sent(generation);
                let (generation, infos) = infos_to_send(&ids, &ctx);
                answer = host
                    .call_start_fn_batch(
                        handle,
                        &ids,
                        generation,
                        infos.as_deref(),
                        guard.dev,
                        true,
                    )
                    .await;
            }
            let out = match answer {
                Ok(out) => guard.results(out),
                Err(e) => {
                    // Whatever this batch carried may not have arrived.
                    forget_sent(generation);
                    guard.step_aside("name", e, ids.len(), generation)?
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
        let generation = current_generation();
        let host = Arc::clone(&host);
        let guard = Arc::clone(&guard);
        Box::pin(async move {
            let out = match host
                .call_start_fn_batch(handle, &ids, generation, None, guard.dev, false)
                .await
            {
                Ok(out) => guard.results(out),
                Err(e) => guard.step_aside("test", e, ids.len(), generation)?,
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

    #[test]
    fn module_infos_cover_the_graph_once_per_pass_and_test_calls_keep_the_pass() {
        let first = pass(&[("/entry", "/route"), ("/route", "/dep")], &["/lone"]);
        // A name batch over one module sends what a walk from it reaches.
        let (generation, json) = infos_to_send(&["/route".into()], &first);
        assert_eq!(sent_ids(json), ["/dep", "/entry", "/route"]);
        // A test call between name batches names the same pass, so the host
        // keeps the infos (it resets them on a new generation).
        assert_eq!(current_generation(), generation);
        // Later batches of the pass send only what the host lacks.
        let (again, json) = infos_to_send(&["/entry".into(), "/lone".into()], &first);
        assert_eq!(again, generation);
        assert_eq!(sent_ids(json), ["/lone"]);
        // A new pass is a new generation and sends its own graph.
        let second = pass(&[("/entry", "/route")], &[]);
        let (next, json) = infos_to_send(&["/entry".into()], &second);
        assert_ne!(next, generation);
        assert_eq!(sent_ids(json), ["/entry", "/route"]);

        // The host lost what the pass sent (its engine was respawned): the
        // pass's next batch sends the walk again, under the same generation.
        let (_, json) = infos_to_send(&["/route".into()], &second);
        assert_eq!(json, None, "nothing the host lacks");
        forget_sent(generation);
        let (_, json) = infos_to_send(&["/route".into()], &second);
        assert_eq!(json, None, "another pass's generation forgets nothing");
        forget_sent(next);
        let (same, json) = infos_to_send(&["/route".into()], &second);
        assert_eq!(same, next);
        assert_eq!(sent_ids(json), ["/entry", "/route"]);
        // The first batch after says the pass is new to the host even with
        // no info to send, so the host does not take it for one it lost.
        forget_sent(next);
        let (_, json) = infos_to_send(&["/not-a-module".into()], &second);
        assert_eq!(json.as_deref(), Some("[]"));
        let (_, json) = infos_to_send(&["/not-a-module".into()], &second);
        assert_eq!(json, None, "and only the first");
        let third = pass(&[("/entry", "/route")], &[]);
        let (_, json) = infos_to_send(&["/not-a-module".into()], &third);
        assert_eq!(json.as_deref(), Some("[]"), "a new pass's first batch too");
    }

    #[test]
    fn dev_throws_step_aside_once() {
        let guard = Guard::new("vendor".into(), true);
        let out = guard.results(vec![
            json!("a"),
            json!({ "$threw": "boom" }),
            json!({ "$threw": "boom" }),
        ]);
        assert_eq!(out, vec![Some(json!("a")), None, None]);
        assert!(*guard.warned.lock().unwrap());
    }

    #[test]
    fn a_host_that_cannot_run_the_function_steps_aside_under_dev_and_fails_a_build() {
        let dev = Guard::new("#0".into(), true);
        let out = dev
            .step_aside("name", "oj: no config function #0".into(), 2, 1)
            .expect("dev steps aside");
        assert_eq!(out, vec![None, None]);
        assert_eq!(*dev.host_warned.lock().unwrap(), Some(1));
        let build = Guard::new("#0".into(), false);
        let err = build
            .step_aside("name", "oj: no config function #0".into(), 2, 1)
            .expect_err("a build stays loud");
        assert_eq!(
            err.to_string(),
            "codeSplitting group #0 name: oj: no config function #0"
        );
    }

    // Stepping aside holds for the batch that failed and no longer: the
    // next batch the host answers is used as it comes. A host that could
    // not run the function is warned of once per chunking pass, whichever
    // of the group's functions it was, apart from the one warning a
    // throwing function gets.
    #[test]
    fn a_group_that_stepped_aside_answers_again_and_warns_once_a_pass() {
        let guard = Guard::new("vendor".into(), true);
        assert_eq!(
            guard.results(vec![json!("a"), json!(null)]),
            vec![Some(json!("a")), Some(json!(null))],
            "answers pass through, a null one included"
        );
        assert!(!*guard.warned.lock().unwrap());
        assert_eq!(*guard.host_warned.lock().unwrap(), None);

        assert_eq!(
            guard
                .step_aside("test", "plugin host unresponsive".into(), 3, 7)
                .unwrap(),
            vec![None, None, None],
            "a miss per id"
        );
        assert_eq!(*guard.host_warned.lock().unwrap(), Some(7));
        assert!(!*guard.warned.lock().unwrap(), "not the throw's warning");
        assert_eq!(
            guard.step_aside("name", "again".into(), 0, 7).unwrap(),
            Vec::<Option<Value>>::new()
        );
        assert_eq!(*guard.host_warned.lock().unwrap(), Some(7), "the same pass");

        assert_eq!(
            guard.results(vec![json!("b"), json!({ "$threw": "boom" }), json!(true)]),
            vec![Some(json!("b")), None, Some(json!(true))],
            "the batch after is used as it comes"
        );
        assert!(*guard.warned.lock().unwrap());
        assert_eq!(*guard.host_warned.lock().unwrap(), Some(7));

        // The next pass's outage is warned of again, a throw or not before.
        guard
            .step_aside("name", "plugin host exited".into(), 1, 8)
            .unwrap();
        assert_eq!(*guard.host_warned.lock().unwrap(), Some(8));
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
            let build = Guard::new(label.into(), false);
            let err = build
                .step_aside(what, "host gone".into(), 1, 1)
                .unwrap_err();
            assert_eq!(err.to_string(), want);
            assert!(
                !*build.warned.lock().unwrap(),
                "a build fails, it does not warn"
            );
        }
    }
}
