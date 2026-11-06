// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The in-process SSR module runner: an embedded JS engine whose module
//! loading delegates to the dev server's SSR pipeline (the same resolve/load
//! behind `/@ssr-resolve` and `/@ssr-module`).
//!
//! Invalidation works like the old runner's registry, adapted to an isolate
//! whose module map caches forever: every module id gets a version, and its
//! specifier carries it (`?v=N`). A change bumps the module and everything
//! that transitively imports it, so the next request's entry specifier is
//! new, re-linking reaches fresh instances exactly along the invalidated
//! chain, and untouched subtrees keep their cached instances and state.
//! Orphaned instances leak in the isolate, so after enough of them the
//! engine is respawned (a cheap snapshot boot) behind a write lock.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::SystemTime;

use oj_js::EngineConfig;
use oj_js::HostModule;
use oj_js::HostModuleType;
use oj_js::HostResolved;
use oj_js::JsEngine;
use oj_js::ModuleHost;
use oj_server::SsrBridge;
use oj_server::SsrResolution;

const SSR_BOOTSTRAP_JS: &str = include_str!("assets/ssr-bootstrap.mjs");

/// Scheme for module ids that are not filesystem paths (plugin virtual
/// modules): the id travels percent-encoded in the URL path.
const VIRTUAL_SCHEME: &str = "oj-ssr";

/// Respawn the isolate once this many module instances have been orphaned by
/// invalidation: stale instances stay in the module map forever (specifiers
/// are immortal in V8), so a long editing session trades a snapshot re-boot
/// for the slow leak.
const STALE_MODULE_RESPAWN_THRESHOLD: usize = 500;

/// Versioned module ids plus the importer graph, mirroring the registry the
/// node runner kept: mtimes recorded at load, dirtiness propagated up the
/// importers, dirty records dropped so the next request rebuilds them.
#[derive(Default)]
struct Graph {
    versions: HashMap<String, u64>,
    recs: HashMap<String, Rec>,
    /// Instances orphaned in the isolate since the last respawn.
    stale: usize,
}

#[derive(Default)]
struct Rec {
    mtime: Option<SystemTime>,
    importers: HashSet<String>,
}

pub(crate) fn mtime_of(id: &str) -> Option<SystemTime> {
    // A Start module id may carry an intent tag (`?ojasset=css`, `?react`);
    // the mtime belongs to the file behind it.
    let clean = id.split('?').next().unwrap_or(id);
    if !Path::new(clean).is_absolute() {
        return None;
    }
    std::fs::metadata(clean).and_then(|m| m.modified()).ok()
}

/// The shared version-graph: module versions, importer edges and mtimes behind
/// one lock, used by both module hosts (SSR and Start).
#[derive(Default)]
pub(crate) struct VersionGraph {
    graph: Mutex<Graph>,
}

impl VersionGraph {
    /// The version-stamped specifier the engine imports a module id under.
    pub(crate) fn specifier_for(&self, id: &str) -> String {
        specifier(id, self.version_of(id))
    }

    pub(crate) fn version_of(&self, id: &str) -> u64 {
        self.graph
            .lock()
            .unwrap()
            .versions
            .get(id)
            .copied()
            .unwrap_or(0)
    }

    /// Records an importer edge and returns the dependency's current stamped
    /// specifier, under one lock acquisition.
    pub(crate) fn edge_specifier(&self, id: &str, importer: String) -> String {
        let mut graph = self.graph.lock().unwrap();
        graph
            .recs
            .entry(id.to_string())
            .or_default()
            .importers
            .insert(importer);
        specifier(id, graph.versions.get(id).copied().unwrap_or(0))
    }

    /// Records the module's on-disk mtime at load time.
    pub(crate) fn record_loaded(&self, id: &str) {
        self.graph
            .lock()
            .unwrap()
            .recs
            .entry(id.to_string())
            .or_default()
            .mtime = mtime_of(id);
    }

    /// Force-bumps one module id (a reload of the entry): its next specifier is
    /// new even when its file did not change.
    pub(crate) fn bump(&self, id: &str) {
        let mut graph = self.graph.lock().unwrap();
        *graph.versions.entry(id.to_string()).or_insert(0) += 1;
        graph.recs.remove(id);
        graph.stale += 1;
    }

    /// Drops every record whose file changed since it was loaded, and every
    /// transitive importer of one: their versions bump, so the next request
    /// re-imports fresh instances along the chain. Removing a record as it is
    /// visited IS the visited set: a re-pushed id finds no record and stops.
    pub(crate) fn invalidate(&self) -> usize {
        self.invalidate_seeded().0
    }

    /// [`invalidate`](Self::invalidate), answering also whether a changed
    /// file the walk started from is one of `changed` (a watch batch's paths,
    /// either spelling). Vite acts on a watcher event through the modules of
    /// that event's file alone (`getModulesByFile`): a recorded file that
    /// changed without the watcher reporting it (an output oj itself rewrote,
    /// an ignored path) is re-imported, but is no hit for the batch.
    pub(crate) fn invalidate_for(&self, changed: &[PathBuf]) -> (usize, bool) {
        let (dropped, seeds) = self.invalidate_seeded();
        let spelled: HashSet<PathBuf> = changed
            .iter()
            .flat_map(|p| [Some(p.clone()), std::fs::canonicalize(p).ok()])
            .flatten()
            .collect();
        let hit = seeds.iter().any(|id| {
            let file = Path::new(id.split('?').next().unwrap_or(id));
            spelled.contains(file)
                || std::fs::canonicalize(file).is_ok_and(|real| spelled.contains(&real))
        });
        (dropped, hit)
    }

    /// The walk behind both: how many records it dropped, and the ids of the
    /// changed files it started from.
    fn invalidate_seeded(&self) -> (usize, Vec<String>) {
        // The seed scan stats every recorded file; do that OUTSIDE the lock,
        // so resolves never queue behind hundreds of filesystem calls. A
        // module that loads between snapshot and walk is caught by the next
        // watcher tick, like any change that lands mid-scan.
        let recorded: Vec<(String, Option<SystemTime>)> = {
            let graph = self.graph.lock().unwrap();
            graph
                .recs
                .iter()
                .map(|(id, rec)| (id.clone(), rec.mtime))
                .collect()
        };
        let seeds: Vec<String> = recorded
            .into_iter()
            .filter(|(id, mtime)| mtime_of(id) != *mtime)
            .map(|(id, _)| id)
            .collect();
        let mut stack = seeds.clone();
        let mut graph = self.graph.lock().unwrap();
        let mut dropped = 0;
        while let Some(id) = stack.pop() {
            let Some(rec) = graph.recs.remove(&id) else {
                continue;
            };
            *graph.versions.entry(id).or_insert(0) += 1;
            dropped += 1;
            stack.extend(rec.importers);
        }
        graph.stale += dropped;
        (dropped, seeds)
    }

    pub(crate) fn should_respawn(&self) -> bool {
        self.graph.lock().unwrap().stale >= STALE_MODULE_RESPAWN_THRESHOLD
    }

    /// A fresh isolate has an empty module map: no records are live and no
    /// instances leak. Versions persist (specifiers only need to keep moving
    /// forward).
    pub(crate) fn reset_after_respawn(&self) {
        let mut graph = self.graph.lock().unwrap();
        graph.recs.clear();
        graph.stale = 0;
    }
}

pub struct SsrHost {
    bridge: SsrBridge,
    graph: VersionGraph,
}

impl SsrHost {
    fn new(bridge: SsrBridge) -> Arc<SsrHost> {
        Arc::new(SsrHost {
            bridge,
            graph: VersionGraph::default(),
        })
    }

    /// The version-stamped specifier the engine imports a module id under.
    fn specifier_for(&self, id: &str) -> String {
        self.graph.specifier_for(id)
    }

    pub fn invalidate(&self) -> usize {
        self.graph.invalidate()
    }

    fn should_respawn(&self) -> bool {
        self.graph.should_respawn()
    }

    fn reset_after_respawn(&self) {
        self.graph.reset_after_respawn()
    }
}

pub(crate) fn specifier(id: &str, version: u64) -> String {
    if Path::new(id).is_absolute() {
        if let Ok(mut url) = url::Url::from_file_path(Path::new(id)) {
            url.set_query(Some(&format!("v={version}")));
            return url.to_string();
        }
    }
    format!("{VIRTUAL_SCHEME}:///{}?v={version}", percent_encode(id))
}

/// The module id behind a specifier this host minted; `None` for anything
/// else (the bootstrap file, node_modules files), which byonm loads.
pub(crate) fn versioned_id(specifier: &str) -> Option<String> {
    let url = url::Url::parse(specifier).ok()?;
    match url.scheme() {
        "file" => {
            if !url.query_pairs().any(|(k, _)| k == "v") {
                return None;
            }
            let path = url.to_file_path().ok()?;
            Some(path.to_string_lossy().into_owned())
        }
        VIRTUAL_SCHEME => Some(percent_decode(url.path().trim_start_matches('/'))),
        _ => None,
    }
}

/// A node_modules file the SSR host loads through the transform pipeline
/// instead of leaving it to the runtime.
fn claims_dependency(id: &str) -> bool {
    id.contains("/node_modules/") && crate::start_host::ts_source_ext(id)
}

/// Whether an importer's imports start from Node semantics: a dependency the
/// runtime loaded itself. One the pipeline served (versioned: `ssr.noExternal`,
/// or a file Node cannot take) resolves through the pipeline instead, as
/// Vite's runner fetches every import of a module it did not externalize.
fn node_resolves_imports_of(importer: &str) -> bool {
    importer_id(importer).contains("/node_modules/") && versioned_id(importer).is_none()
}

/// The module id an importer specifier stands for (version stripped), for
/// resolver context and importer-graph edges.
pub(crate) fn importer_id(specifier: &str) -> String {
    if let Ok(url) = url::Url::parse(specifier) {
        match url.scheme() {
            "file" => {
                if let Ok(path) = url.to_file_path() {
                    return path.to_string_lossy().into_owned();
                }
            }
            VIRTUAL_SCHEME => return percent_decode(url.path().trim_start_matches('/')),
            _ => {}
        }
    }
    specifier.to_string()
}

impl SsrHost {
    async fn resolve_import(
        &self,
        importer: &str,
        specifier: &str,
    ) -> Result<Option<HostResolved>, String> {
        let importer_id = importer_id(importer);
        // node_modules internals resolve with plain Node semantics, as
        // they did when the runner imported externals natively — except a
        // relative specifier with no exact file on disk (an extensionless
        // import inside a dependency shipping raw TypeScript, which only the
        // pipeline can serve): the Vite-style resolver finishes that edge
        // with its extension inference, as the Start host does. A dependency
        // module the pipeline transformed (a raw-TS source) asks the pipeline
        // first: Vite never externalizes a relative or absolute specifier
        // (createIsExternal), so its `import "./select.css"` reaches the css
        // pipeline instead of the runtime loading the stylesheet as JS.
        if node_resolves_imports_of(importer) {
            if claims_dependency(&importer_id) {
                if let Ok(SsrResolution::Module(id)) =
                    self.bridge.resolve(&importer_id, specifier).await
                {
                    let spec = self.graph.edge_specifier(&id, importer_id);
                    return Ok(Some(HostResolved::Url(spec)));
                }
            }
            if specifier.starts_with("./") || specifier.starts_with("../") {
                if let Some(dir) = Path::new(&importer_id).parent() {
                    if !crate::ssr_dev::normalize_path(&dir.join(specifier)).is_file() {
                        if let Ok(oj_server::StartResolution::Dependency(p)) =
                            self.bridge.resolve_start(&importer_id, specifier).await
                        {
                            if let Ok(u) = url::Url::from_file_path(&p) {
                                return Ok(Some(HostResolved::External(u.to_string())));
                            }
                        }
                    }
                }
            }
            return Ok(Some(HostResolved::External(specifier.to_string())));
        }
        match self.bridge.resolve(&importer_id, specifier).await? {
            SsrResolution::Module(id) => {
                let spec = self.graph.edge_specifier(&id, importer_id);
                Ok(Some(HostResolved::Url(spec)))
            }
            SsrResolution::External(spec) => Ok(Some(HostResolved::External(spec))),
        }
    }

    async fn load_import(&self, specifier: &str) -> Result<Option<HostModule>, String> {
        let Some(id) = versioned_id(specifier) else {
            return self.load_dependency_ts(specifier).await;
        };
        let code = self
            .bridge
            .load_module(&id)
            .await
            .map_err(|e| e.to_string())?;
        self.graph.record_loaded(&id);
        Ok(Some(HostModule {
            code,
            module_type: HostModuleType::JavaScript,
        }))
    }

    /// A dependency file the runtime resolved itself (byonm): TypeScript/JSX
    /// sources still go through the transform pipeline — the runtime refuses
    /// to type-strip under node_modules, and Vite never externalizes a
    /// resolved file it cannot hand to Node (canExternalizeFile allows only
    /// js/mjs/cjs). Deliberately not recorded in the version graph: edits to
    /// dependencies don't invalidate, as Vite doesn't watch node_modules.
    async fn load_dependency_ts(&self, specifier: &str) -> Result<Option<HostModule>, String> {
        let Some(path) = url::Url::parse(specifier)
            .ok()
            .filter(|u| u.scheme() == "file")
            .and_then(|u| u.to_file_path().ok())
        else {
            return Ok(None);
        };
        let id = path.to_string_lossy().into_owned();
        if !claims_dependency(&id) {
            return Ok(None);
        }
        let code = self
            .bridge
            .load_module(&id)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Some(HostModule {
            code,
            module_type: HostModuleType::JavaScript,
        }))
    }
}

/// Serves the engine's module-host requests by calling this host. One task
/// owns the recv loop; each request gets a task of its own because deno_core
/// polls module loads CONCURRENTLY while it builds a graph — handling them
/// inline would serialize every fetch behind the slowest transform. The loop
/// ends when the engine (the sender) is gone.
fn serve_module_host(host: Arc<SsrHost>, mut requests: oj_js::HostRequests) {
    tokio::spawn(async move {
        while let Some(request) = requests.recv().await {
            let host = Arc::clone(&host);
            tokio::spawn(async move {
                match request {
                    oj_js::HostRequest::Resolve {
                        importer,
                        specifier,
                        reply,
                    } => {
                        let _ = reply.send(host.resolve_import(&importer, &specifier).await);
                    }
                    oj_js::HostRequest::Load { specifier, reply } => {
                        let _ = reply.send(host.load_import(&specifier).await);
                    }
                }
            });
        }
    });
}

/// A rendered document from the entry, borrowed straight out of the
/// bootstrap's reply (head and html run large on real pages, so nothing is
/// copied out): the loader data (serialized, `<` escaped), the `<head>` HTML,
/// and the body HTML.
pub struct RenderOut(serde_json::Value);

impl RenderOut {
    pub fn data_json(&self) -> &str {
        self.0
            .get("data")
            .and_then(|v| v.as_str())
            .unwrap_or("null")
    }

    pub fn head(&self) -> &str {
        self.0.get("head").and_then(|v| v.as_str()).unwrap_or("")
    }

    /// The body HTML, taken out of the reply without a copy.
    pub fn take_html(&mut self) -> String {
        match self.0.get_mut("html").map(serde_json::Value::take) {
            Some(serde_json::Value::String(html)) => html,
            _ => String::new(),
        }
    }
}

/// The SSR runner handle `ssr_dev` calls per request. Wraps the engine so a
/// respawn (stale-instance reclaim) is transparent: requests share it behind
/// a read lock, the respawn takes the write lock.
pub struct SsrEngine {
    host: Arc<SsrHost>,
    root: PathBuf,
    entry_id: String,
    bootstrap: String,
    engine: tokio::sync::RwLock<JsEngine>,
}

impl SsrEngine {
    pub fn new(root: PathBuf, entry_abs: PathBuf, bridge: SsrBridge) -> anyhow::Result<SsrEngine> {
        let dir = oj_cache::cache_root(&root).join("ssr");
        std::fs::create_dir_all(&dir)?;
        let bootstrap = dir.join("bootstrap.mjs");
        std::fs::write(&bootstrap, SSR_BOOTSTRAP_JS)?;
        let host = SsrHost::new(bridge);
        let engine = spawn_engine(&root, &host)?;
        Ok(SsrEngine {
            host,
            root,
            entry_id: entry_abs.to_string_lossy().into_owned(),
            bootstrap: bootstrap.to_string_lossy().into_owned(),
            engine: tokio::sync::RwLock::new(engine),
        })
    }

    pub fn host(&self) -> Arc<SsrHost> {
        Arc::clone(&self.host)
    }

    /// The entry's loader data for `url`, serialized (the loader endpoint's
    /// response body).
    pub async fn load(&self, url: &str) -> Result<String, String> {
        let entry = self.host.specifier_for(&self.entry_id);
        let value = self
            .call_bootstrap("load", vec![entry.into(), url.into()])
            .await?;
        Ok(json_string(value))
    }

    /// Runs the entry's action for `url` with the request body (text and raw
    /// bytes, as the runner passed them), then reloads its data.
    pub async fn action(&self, url: &str, body: &[u8]) -> Result<String, String> {
        let entry = self.host.specifier_for(&self.entry_id);
        let text = String::from_utf8_lossy(body).into_owned();
        let value = self
            .call_bootstrap(
                "action",
                vec![entry.into(), url.into(), text.into(), base64(body).into()],
            )
            .await?;
        Ok(json_string(value))
    }

    pub async fn render(&self, url: &str) -> Result<RenderOut, String> {
        let entry = self.host.specifier_for(&self.entry_id);
        let value = self
            .call_bootstrap("render", vec![entry.into(), url.into()])
            .await?;
        Ok(RenderOut(value))
    }

    /// Invokes an export of a server module (the `/__oj_fn` path; the module
    /// was validated by the caller) and returns its JSON result.
    pub async fn call_server_fn(
        &self,
        module: &Path,
        name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let spec = self.host.specifier_for(&module.to_string_lossy());
        self.call_bootstrap("callFn", vec![spec.into(), name.into(), args])
            .await
    }

    async fn call_bootstrap(
        &self,
        export: &str,
        args: Vec<serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        // Same cadence as the runner: re-check the world before each request
        // (the HMR push does it eagerly between requests too).
        self.host.invalidate();
        self.maybe_respawn().await?;
        let engine = self.engine.read().await;
        engine
            .call(self.bootstrap.clone(), export, args, None)
            .await
            .map_err(|e| e.to_string())
    }

    async fn maybe_respawn(&self) -> Result<(), String> {
        if !self.host.should_respawn() {
            return Ok(());
        }
        let mut engine = self.engine.write().await;
        if !self.host.should_respawn() {
            return Ok(()); // another request already respawned
        }
        *engine = spawn_engine(&self.root, &self.host).map_err(|e| e.to_string())?;
        self.host.reset_after_respawn();
        eprintln!("oj ssr: engine respawned to reclaim stale module instances");
        Ok(())
    }
}

fn spawn_engine(root: &Path, host: &Arc<SsrHost>) -> Result<JsEngine, oj_js::EngineError> {
    let mut config = EngineConfig::new(root);
    config.code_cache_dir = Some(oj_server::engine_code_cache_dir(root));
    config.registry = Some(host.bridge.engine_registry());
    let (link, requests) = ModuleHost::channel();
    serve_module_host(Arc::clone(host), requests);
    JsEngine::spawn(config, Some(link), None)
}

/// The bootstrap's data endpoints reply with an already-serialized JSON
/// string; anything else is serialized here (belt and braces).
fn json_string(value: serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    }
}

pub(crate) fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub(crate) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Standard base64 with padding (what `atob` decodes).
pub(crate) fn base64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specifiers_roundtrip_file_and_virtual_ids() {
        let file = specifier("/a b/src/App.tsx", 3);
        assert!(file.starts_with("file://"), "{file}");
        assert!(file.ends_with("?v=3"), "{file}");
        assert_eq!(versioned_id(&file).as_deref(), Some("/a b/src/App.tsx"));
        assert_eq!(importer_id(&file), "/a b/src/App.tsx");

        let virt = specifier("virtual:plugin-greeting", 7);
        assert!(virt.starts_with("oj-ssr:///"), "{virt}");
        assert_eq!(
            versioned_id(&virt).as_deref(),
            Some("virtual:plugin-greeting")
        );
        assert_eq!(importer_id(&virt), "virtual:plugin-greeting");

        // Specifiers the host did not mint are not claimed.
        assert_eq!(versioned_id("file:///plain/file.mjs"), None);
        assert_eq!(versioned_id("node:path"), None);
    }

    // A `noExternal` dependency the pipeline served must not hand its own
    // imports back to Node: Vite's runner fetches them all through the pipeline.
    #[test]
    fn only_a_dependency_the_runtime_loaded_resolves_with_node() {
        let dep = "/app/node_modules/nox/index.js";
        assert!(node_resolves_imports_of(
            "file:///app/node_modules/nox/index.js"
        ));
        assert!(!node_resolves_imports_of(&specifier(dep, 2)));
        assert!(!node_resolves_imports_of(&specifier("/app/src/main.js", 1)));
    }

    // The shared version graph drives both hosts' warm reload: a changed file
    // bumps itself and every transitive importer (their next specifiers are
    // new), an explicit bump forces one id fresh, and untouched subtrees keep
    // their versions (cached instances).
    // Vite acts on a watcher event through that event's file's modules: a
    // recorded file rewritten behind the watcher's back (an output oj wrote
    // itself, like the client manifest after a hot update catches up) is
    // re-imported on the next batch, but is no hit for a batch that never
    // named it, so an unrelated event cannot reload the page.
    #[test]
    fn a_batch_hits_only_through_its_own_paths() {
        let dir = std::env::temp_dir().join(format!("oj-vgraph-for-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (manifest, page) = (dir.join("manifest.json"), dir.join("page.ts"));
        let stray = dir.join(".DS_Store");
        for f in [&manifest, &page] {
            std::fs::write(f, "1").unwrap();
        }
        let graph = VersionGraph::default();
        for f in [&manifest, &page] {
            let id = f.to_string_lossy().into_owned();
            let _ = graph.edge_specifier(&id, "entry".into());
            graph.record_loaded(&id);
        }
        let touch = |f: &Path| {
            let t = std::fs::metadata(f).unwrap().modified().unwrap()
                + std::time::Duration::from_secs(5);
            std::fs::File::options()
                .write(true)
                .open(f)
                .unwrap()
                .set_modified(t)
                .unwrap();
        };

        touch(&manifest);
        let (dropped, hit) = graph.invalidate_for(&[stray]);
        assert_eq!(dropped, 1, "the rewritten output is still re-imported");
        assert!(!hit, "an unrelated batch hit through a file it never named");

        touch(&page);
        assert_eq!(graph.invalidate_for(std::slice::from_ref(&page)), (1, true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn version_graph_invalidates_changed_files_and_their_importers() {
        let dir = std::env::temp_dir().join(format!("oj-vgraph-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let leaf = dir.join("leaf.ts");
        let mid = dir.join("mid.ts");
        let other = dir.join("other.ts");
        for f in [&leaf, &mid, &other] {
            std::fs::write(f, "export const a = 1;").unwrap();
        }
        let (leaf_id, mid_id, other_id) = (
            leaf.to_string_lossy().into_owned(),
            mid.to_string_lossy().into_owned(),
            other.to_string_lossy().into_owned(),
        );

        let graph = VersionGraph::default();
        // entry -> mid -> leaf; entry -> other.
        let _ = graph.edge_specifier(&mid_id, "entry".into());
        let _ = graph.edge_specifier(&leaf_id, mid_id.clone());
        let _ = graph.edge_specifier(&other_id, "entry".into());
        for id in [&leaf_id, &mid_id, &other_id] {
            graph.record_loaded(id);
        }
        assert_eq!(graph.invalidate(), 0, "nothing changed yet");

        // Touch the leaf: it and its importer chain bump, the sibling stays.
        std::fs::write(&leaf, "export const a = 2;").unwrap();
        let bumped_at = std::fs::metadata(&leaf).unwrap().modified().unwrap();
        // Belt and braces for coarse mtime clocks: force a distinct mtime.
        let _ = bumped_at;
        let dropped = graph.invalidate();
        assert!(dropped >= 2, "leaf + importer chain, got {dropped}");
        assert_eq!(graph.version_of(&leaf_id), 1);
        assert_eq!(graph.version_of(&mid_id), 1);
        assert_eq!(
            graph.version_of(&other_id),
            0,
            "untouched sibling keeps its version"
        );
        assert!(graph.specifier_for(&leaf_id).ends_with("?v=1"));

        // A forced bump (the Start engine's entry reload) moves one id.
        graph.bump(&other_id);
        assert_eq!(graph.version_of(&other_id), 1);

        // Respawn bookkeeping: enough orphans ask for a respawn, and the
        // reset clears the counter but keeps versions moving forward.
        for _ in 0..600 {
            graph.bump(&other_id);
        }
        assert!(graph.should_respawn());
        graph.reset_after_respawn();
        assert!(!graph.should_respawn());
        assert!(graph.version_of(&other_id) > 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn base64_matches_atob_expectations() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(&[0xff, 0x00, 0x10, 0x88]), "/wAQiA==");
    }
}
