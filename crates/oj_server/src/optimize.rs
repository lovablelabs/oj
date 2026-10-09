// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::watch;

const OPTIMIZE_JS: &str = include_str!("assets/optimize-deps.mjs");

/// The `optimizeDeps` knobs that apply to DIRECTLY-SERVED deps (ones the
/// optimizer did not pre-bundle): `exclude` routes a dep through plugin
/// hooks like Vite, and `needsInterop` forces the CJS interop rewrite even
/// when static analysis reads the dep as ESM.
pub struct OptimizeView {
    pub exclude: std::collections::HashSet<String>,
    pub needs_interop: std::collections::HashSet<String>,
}

impl OptimizeView {
    pub fn new(exclude: Vec<String>, needs_interop: Vec<String>) -> Self {
        OptimizeView {
            exclude: exclude.into_iter().collect(),
            needs_interop: needs_interop.into_iter().collect(),
        }
    }

    fn names(&self, set: &std::collections::HashSet<String>, path: &std::path::Path) -> bool {
        package_name(path).is_some_and(|name| set.contains(&name))
    }

    pub fn is_excluded(&self, path: &std::path::Path) -> bool {
        self.names(&self.exclude, path)
    }

    pub fn needs_forced_interop(&self, path: &std::path::Path) -> bool {
        self.names(&self.needs_interop, path)
    }
}

/// The npm package name a node_modules path belongs to (`@scope/name` or `name`).
pub(crate) fn package_name(entry: &std::path::Path) -> Option<String> {
    let comps: Vec<&std::ffi::OsStr> = entry.components().map(|c| c.as_os_str()).collect();
    let idx = comps.iter().rposition(|c| *c == "node_modules")?;
    let first = comps.get(idx + 1)?.to_str()?;
    if first.starts_with('@') {
        Some(format!("{first}/{}", comps.get(idx + 2)?.to_str()?))
    } else {
        Some(first.to_string())
    }
}

#[derive(Clone)]
pub struct DepMeta {
    pub file: String,
    pub needs_interop: bool,
    /// Importer rewrite target `/@oj-deps/<file>?v=<version>`; the version query
    /// is what lets the response be served immutable (Vite's ensureVersionQuery).
    pub url: String,
    /// Content hash of the bundled entry (Vite's fileHash): a re-optimization
    /// whose previously served entries all hash the same commits without a
    /// reload. Empty for a provisional (registered, not yet bundled) entry.
    pub file_hash: String,
}

/// The `?v=` a meta's URL carries.
fn url_version(url: &str) -> &str {
    url.rsplit_once("?v=").map(|(_, v)| v).unwrap_or("")
}

/// The sidecar's entry naming for a bare dep (its `optimize()` mirrors this):
/// the future bundle file is known at registration, like Vite's
/// `getOptimizedDepPath`, so the import rewrites to it before any bundling.
fn flatten_dep_file(dep: &str) -> String {
    let name: String = dep
        .trim_start_matches('@')
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{name}.mjs")
}

pub type DepMap = HashMap<String, DepMeta>;

pub struct OptimizedDeps {
    rx: watch::Receiver<Option<Arc<DepMap>>>,
    dir: PathBuf,
    /// Short prebundle hash: changes with the lockfile or optimizer config so
    /// stale immutable entries never share a URL. Empty when disabled.
    version: String,
    rerun: Option<Arc<RerunState>>,
}

/// Commit callback: the newly optimized deps, empty when a batch failed.
type OnCommit = Box<dyn Fn(&[String]) + Send + Sync>;

/// Vite's discovered-deps machinery: a bare import the serve-time rewrite
/// finds missing from the pre-bundle registers here (`registerMissingImport`)
/// and is rewritten to its FUTURE optimized URL at once; the dep route holds
/// requests for a pending bundle until the debounced rerun commits. A commit
/// that changed previously served entries (by `fileHash`) bumps the browser
/// version everywhere and reloads; one that did not keeps every served URL
/// valid and reloads nothing, exactly Vite's needsReload split.
struct RerunState {
    /// Base prebundle hash, the seed of every browser version.
    hash: String,
    tx: watch::Sender<Option<Arc<DepMap>>>,
    /// Every serve-time-discovered dep with its provisional meta (the version
    /// its URLs were first served under); persists across reruns.
    discovered: std::sync::Mutex<std::collections::BTreeMap<String, DepMeta>>,
    /// Discovered deps whose rerun has not committed yet: the dep route
    /// blocks on these (Vite awaits the processing promise).
    pending: std::sync::Mutex<std::collections::HashSet<String>>,
    wake: tokio::sync::mpsc::UnboundedSender<()>,
    browser_version: std::sync::RwLock<String>,
    /// Deps a rerun could not bundle (or whose rerun failed): registration
    /// declines them, so their importers keep the per-file path instead of a
    /// phantom optimized URL. Vite instead resets `discovered` and retries on
    /// the next request, which can loop a persistently failing dep.
    failed: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Set when the initial pre-bundle could not run at all (no rolldown via
    /// vite, vendored, or installed): registration declines everything, so
    /// every dep keeps the per-file path a bundler-less app always had,
    /// instead of phantom optimized URLs and a failed rerun per batch.
    dead: std::sync::atomic::AtomicBool,
    exclude: Vec<String>,
    discovery: bool,
    on_commit: std::sync::OnceLock<OnCommit>,
}

/// What the dep route should do once `await_dep` settles.
pub enum DepServe {
    Ready,
    /// The request's `?v=` predates the committed pre-bundle (Vite's
    /// ERR_OUTDATED_OPTIMIZED_DEP): answer 504, the page is mid-reload.
    Outdated,
}

/// A discovery's URL version: the base key plus the (sorted) discovered set,
/// so chunks that change can never be served under a URL a page already holds
/// (Vite's getDiscoveredBrowserHash).
fn browser_hash<'a>(hash: &str, discovered: impl Iterator<Item = &'a String>) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(hash.as_bytes());
    for dep in discovered {
        hasher.update(b"\0");
        hasher.update(dep.as_bytes());
    }
    hasher.finalize().to_hex()[..8].to_string()
}

/// Vite's `moduleListContains`: an exclude entry names the dep or a parent path.
fn module_list_contains(list: &[String], dep: &str) -> bool {
    list.iter()
        .any(|m| m == dep || dep.starts_with(&format!("{m}/")))
}

/// `/@oj-deps/<file>?v=<version>` (Vite: `<cacheDir>/deps/<file>?v=<browserHash>`).
pub fn dep_url(file: &str, version: &str) -> String {
    if version.is_empty() {
        format!("/@oj-deps/{file}")
    } else {
        format!("/@oj-deps/{file}?v={version}")
    }
}

/// Rolldown links a chunk to an entry chunk as `./<entry>.mjs`, which the
/// browser resolves without the `?v=` the app's imports of that entry carry:
/// two URLs, so the entry evaluates twice. Each such import moves to the
/// entry's own URL, as Vite's resolve plugin gives a relative import of an
/// optimized file that file's browserHash. `None` when nothing links.
///
/// The answer stays valid under a `?v=` URL's immutable caching: a file can
/// only name entries of its own bundle (`[name].mjs`; chunks carry a hash),
/// a no-reload commit keeps every committed entry's version and bytes, and a
/// reload commit moves every URL, the referencing file's included.
fn linked_entry_imports(map: &DepMap, name: &str, code: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(code).ok()?;
    let entries: HashMap<&str, &str> = map
        .values()
        .filter(|meta| !url_version(&meta.url).is_empty())
        .filter_map(|meta| Some((meta.file.as_str(), meta.url.strip_prefix("/@oj-deps/")?)))
        .collect();
    if !mentions_entry(text, &entries) {
        return None;
    }
    oj_compiler::rewrite_specifiers(text, Path::new(name), |spec| {
        entries
            .get(spec.strip_prefix("./")?)
            .map(|url| format!("./{url}"))
    })
}

/// The parse-free gate before linking: whether a quoted `./<entry>` appears
/// at all, which most pre-bundle files never do.
fn mentions_entry(text: &str, entries: &HashMap<&str, &str>) -> bool {
    let bytes = text.as_bytes();
    text.match_indices("./").any(|(at, _)| {
        let Some(&quote @ (b'"' | b'\'')) = at.checked_sub(1).and_then(|q| bytes.get(q)) else {
            return false;
        };
        text[at + 2..]
            .split(quote as char)
            .next()
            .is_some_and(|file| entries.contains_key(file))
    })
}

impl OptimizedDeps {
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub async fn ready(&self) -> Arc<DepMap> {
        let mut rx = self.rx.clone();
        loop {
            if let Some(map) = rx.borrow().clone() {
                return map;
            }
            if rx.changed().await.is_err() {
                return Arc::new(DepMap::new());
            }
        }
    }

    pub fn disabled() -> Self {
        let (_tx, rx) = watch::channel(Some(Arc::new(DepMap::new())));
        OptimizedDeps {
            rx,
            dir: PathBuf::new(),
            version: String::new(),
            rerun: None,
        }
    }

    /// The version the served URLs (and compile keys) carry right now: the
    /// base hash until a rerun commits, then the discovered-set hash.
    pub fn browser_version(&self) -> String {
        match &self.rerun {
            Some(r) => r.browser_version.read().unwrap().clone(),
            None => self.version.clone(),
        }
    }

    /// After a rerun commits: `&[String]` is the newly optimized deps. The
    /// caller invalidates its module caches and reloads the page here.
    pub fn set_on_commit(&self, f: OnCommit) {
        if let Some(r) = &self.rerun {
            let _ = r.on_commit.set(f);
        }
    }

    /// Cheap pre-gate for the serve-time rewrite: whether `dep` could register
    /// at all, checked before the resolve and CJS-sniff work a registration
    /// needs.
    pub fn may_register(&self, dep: &str) -> bool {
        self.rerun.as_ref().is_some_and(|r| {
            r.discovery
                && !r.dead.load(std::sync::atomic::Ordering::Acquire)
                && !module_list_contains(&r.exclude, dep)
                && !r.failed.lock().unwrap().contains(dep)
        })
    }

    /// The live map's entry for `dep`, including a provisional one registered
    /// mid-compile, which a per-compile snapshot cannot see.
    pub fn meta_now(&self, dep: &str) -> Option<DepMeta> {
        self.rx.borrow().as_ref().and_then(|m| m.get(dep).cloned())
    }

    /// Vite's registerMissingImport, from the serve-time rewrite: a bare
    /// import that resolved into node_modules but is not pre-bundled. The
    /// returned meta points at the FUTURE bundle (file name and version are
    /// known now, Vite's getOptimizedDepPath), so the import rewrites to the
    /// optimized URL immediately and the dep route holds its requests until
    /// the debounced rerun commits. `needs_interop` is read off the resolved
    /// entry's source (Vite's extractExportsData); a bundle that disagrees
    /// later forces the reload path.
    pub fn register_missing(&self, dep: &str, needs_interop: bool) -> Option<DepMeta> {
        let r = self.rerun.as_ref()?;
        if !self.may_register(dep) {
            return None;
        }
        if let Some(meta) = self.meta_now(dep) {
            return Some(meta);
        }
        let meta = {
            let mut discovered = r.discovered.lock().unwrap();
            if let Some(meta) = discovered.get(dep) {
                return Some(meta.clone());
            }
            let file = flatten_dep_file(dep);
            discovered.insert(
                dep.to_string(),
                DepMeta {
                    file: file.clone(),
                    needs_interop,
                    url: String::new(),
                    file_hash: String::new(),
                },
            );
            // Hashing the set it joins (sorted, so registration order is
            // moot): Vite's getDiscoveredBrowserHash over known + missing.
            let version = browser_hash(&r.hash, discovered.keys());
            let meta = DepMeta {
                url: dep_url(&file, &version),
                file,
                needs_interop,
                file_hash: String::new(),
            };
            discovered.insert(dep.to_string(), meta.clone());
            meta
        };
        r.pending.lock().unwrap().insert(dep.to_string());
        r.tx.send_modify(|cur| {
            if let Some(map) = cur {
                Arc::make_mut(map).insert(dep.to_string(), meta.clone());
            }
        });
        let _ = r.wake.send(());
        Some(meta)
    }

    /// A served pre-bundle file, its relative imports of entries pointed at
    /// the entries' current URLs ([`linked_entry_imports`]).
    pub fn link_entries(&self, name: &str, code: Vec<u8>) -> Vec<u8> {
        let map = self.rx.borrow().clone();
        map.and_then(|map| linked_entry_imports(&map, name, &code))
            .map(String::into_bytes)
            .unwrap_or(code)
    }

    /// Route gate for `/@oj-deps/<file>`: a pending dep's request waits for
    /// its rerun to commit (Vite awaits the dep's processing promise), and a
    /// `?v=` the committed entry no longer carries is Vite's outdated-request
    /// 504, which a page mid-reload may still send.
    pub async fn await_dep(&self, file: &str, req_version: Option<&str>) -> DepServe {
        let deadline = tokio::time::Instant::now() + optimizer_timeout();
        let mut rx = self.rx.clone();
        loop {
            enum St {
                Wait,
                Ready,
                Outdated,
            }
            let st = {
                let cur = rx.borrow_and_update();
                match cur
                    .as_ref()
                    .and_then(|m| m.iter().find(|(_, meta)| meta.file == file))
                {
                    // Not in the map: an older kept bundle file, served as-is.
                    None => St::Ready,
                    Some((dep, meta)) => {
                        let pending = self
                            .rerun
                            .as_ref()
                            .is_some_and(|r| r.pending.lock().unwrap().contains(dep));
                        if pending {
                            St::Wait
                        } else if req_version.is_some_and(|v| v != url_version(&meta.url)) {
                            St::Outdated
                        } else {
                            St::Ready
                        }
                    }
                }
            };
            match st {
                St::Ready => return DepServe::Ready,
                St::Outdated => return DepServe::Outdated,
                St::Wait => {
                    tokio::select! {
                        changed = rx.changed() => {
                            if changed.is_err() {
                                return DepServe::Ready;
                            }
                        }
                        _ = tokio::time::sleep_until(deadline) => return DepServe::Ready,
                    }
                }
            }
        }
    }

    /// `host`: the app's plugin host, which runs the scan when there is one.
    pub fn prepare(
        root: &Path,
        version: &str,
        input: OptimizeInput,
        host: Option<Arc<crate::plugins::PluginHost>>,
    ) -> Self {
        let dir = oj_cache::cache_root(root).join("deps");
        let hash = lockfile_hash(root, version, &input);
        let short = hash[..8].to_string();
        let (tx, rx) = watch::channel(None);

        // optimizeDeps.force: ignore any cached pre-bundle and always rebuild.
        let cached = (!input.force).then(|| load_manifest(&dir, &hash)).flatten();
        // A cached manifest restores the previous session's discovered deps
        // (already committed in its map) and the version they were served under.
        let (initial_discovered, initial_version) = match &cached {
            Some(m) => {
                let seeded = m
                    .discovered
                    .iter()
                    .filter_map(|d| m.map.get(d).map(|meta| (d.clone(), meta.clone())))
                    .collect();
                (seeded, m.browser_hash.clone())
            }
            None => (std::collections::BTreeMap::new(), short.clone()),
        };
        let (wake, mut wake_rx) = tokio::sync::mpsc::unbounded_channel();
        let rerun = Arc::new(RerunState {
            hash: hash.clone(),
            tx,
            discovered: std::sync::Mutex::new(initial_discovered),
            pending: std::sync::Mutex::new(std::collections::HashSet::new()),
            wake,
            browser_version: std::sync::RwLock::new(initial_version),
            failed: std::sync::Mutex::new(std::collections::HashSet::new()),
            dead: std::sync::atomic::AtomicBool::new(false),
            exclude: input.exclude.clone(),
            discovery: effective_auto_discover(input.no_discovery),
            on_commit: std::sync::OnceLock::new(),
        });
        {
            let rerun = Arc::clone(&rerun);
            let root = root.to_path_buf();
            let dir = dir.clone();
            let cached_map = cached.map(|m| m.map);
            tokio::spawn(async move {
                match cached_map {
                    Some(map) => {
                        let _ = rerun.tx.send(Some(Arc::new(map)));
                    }
                    None => {
                        let map = match run_optimizer(
                            &root,
                            &dir,
                            &input,
                            host.as_deref(),
                            &Default::default(),
                            &hash[..8],
                            false,
                        )
                        .await
                        {
                            Some(map) => {
                                // A failed run writes NO manifest: an empty
                                // one would warm-boot the next session into
                                // the same dead optimizer with discovery on.
                                write_manifest(&dir, &hash, &hash[..8], [].iter(), &map);
                                map
                            }
                            None => {
                                // No pre-bundle at all (no rolldown via vite,
                                // vendored, or installed, or the run failed):
                                // discovery off, deps serve per-file as a
                                // bundler-less app always did.
                                rerun.dead.store(true, std::sync::atomic::Ordering::Release);
                                eprintln!(
                                    "oj: dep pre-bundling unavailable; serving dependencies per-file"
                                );
                                DepMap::new()
                            }
                        };
                        let _ = rerun.tx.send(Some(Arc::new(map)));
                    }
                }
                // Vite's debouncedProcessing: every registration slides the
                // window, the batch runs after a full quiet one, and a wake
                // landing during a rerun queues the next (one rerun in
                // flight ever, Vite's enqueuedRerun).
                while wake_rx.recv().await.is_some() {
                    debounce_wakes(&mut wake_rx, DEBOUNCE).await;
                    rerun_once(&rerun, &root, &dir, &hash, &input, host.as_deref()).await;
                }
            });
        }
        OptimizedDeps {
            rx,
            dir,
            version: short,
            rerun: Some(rerun),
        }
    }
}

/// Vite's debounceMs.
const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(100);

/// Vite's sliding debounce (debouncedProcessing clears and re-arms its timer
/// on every registration): returns once a full `window` passes with no wake,
/// having consumed every wake that arrived meanwhile, so a burst of
/// discoveries settles into one rerun.
async fn debounce_wakes(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<()>,
    window: std::time::Duration,
) {
    loop {
        match tokio::time::timeout(window, rx.recv()).await {
            // A wake inside the window slides it.
            Ok(Some(())) => continue,
            // Channel closed (the OptimizedDeps dropped): nothing to wait for.
            Ok(None) => return,
            // A quiet window: the batch is settled.
            Err(_) => return,
        }
    }
}

/// One debounced re-optimization: bundle known + discovered, then Vite's
/// commit split. If no previously served entry changed (`fileHash`) and no
/// provisional interop guess was contradicted, every served URL stays valid
/// and nothing reloads; otherwise all URLs move to the batch version and the
/// commit callback invalidates caches and reloads the page.
async fn rerun_once(
    rerun: &Arc<RerunState>,
    root: &Path,
    dir: &Path,
    hash: &str,
    input: &OptimizeInput,
    host: Option<&crate::plugins::PluginHost>,
) {
    let provisional = rerun.discovered.lock().unwrap().clone();
    let pending: Vec<String> = rerun.pending.lock().unwrap().iter().cloned().collect();
    if pending.is_empty() {
        return;
    }
    let names: std::collections::BTreeSet<String> = provisional.keys().cloned().collect();
    let candidate = browser_hash(hash, names.iter());
    let Some(new_map) = run_optimizer(root, dir, input, host, &names, &candidate, true).await
    else {
        // Park the batch as failed (importers recompile onto the per-file
        // path) instead of Vite's reset-and-retry, which can reload-loop a
        // persistently failing dep.
        eprintln!("oj: re-optimizing newly discovered dependencies failed; serving them per-file");
        fail_pending(rerun, &pending);
        return;
    };
    // A pending dep the bundle skipped (a linked package, a macro entry) must
    // not keep its phantom URL either.
    let skipped: Vec<String> = pending
        .iter()
        .filter(|d| !new_map.contains_key(*d))
        .cloned()
        .collect();
    if !skipped.is_empty() {
        eprintln!(
            "oj: {} could not be pre-bundled; serving per-file",
            skipped.join(", ")
        );
        fail_pending(rerun, &skipped);
    }
    // Deps registered while the bundle ran: their wakes are queued, the next
    // rerun bundles the union. The commit below carries their provisional
    // entries over so their URLs keep blocking at the dep route.
    let late: std::collections::HashSet<String> = {
        let p = rerun.pending.lock().unwrap();
        p.iter()
            .filter(|d| !pending.contains(*d))
            .cloned()
            .collect()
    };
    let old = rerun.tx.borrow().clone();
    let old_map = old.as_deref();
    // Vite's needsReload: a previously served entry whose bundle changed, a
    // dep that vanished, or a provisional interop guess the bundle disagrees
    // with (Vite's needsInteropMismatch). A still-pending (mid-rerun) dep is
    // not a vanished one; its own batch decides.
    let mut needs_reload = false;
    if let Some(old_map) = old_map {
        for (dep, old_meta) in old_map {
            if pending.contains(dep) || late.contains(dep) {
                continue;
            }
            match new_map.get(dep) {
                Some(n) if !old_meta.file_hash.is_empty() && n.file_hash == old_meta.file_hash => {}
                _ => {
                    needs_reload = true;
                    break;
                }
            }
        }
    }
    let newly: Vec<String> = pending
        .iter()
        .filter(|d| new_map.contains_key(*d))
        .cloned()
        .collect();
    for dep in &newly {
        if provisional.get(dep).map(|p| p.needs_interop)
            != new_map.get(dep).map(|n| n.needs_interop)
        {
            needs_reload = true;
        }
    }
    // Vite's "delaying reload as new dependencies have been found": a reload
    // commit while discoveries are still arriving would reload once per wave;
    // drop this result and let the queued wake rerun with the union, so one
    // reload lands at the end. Everything stays pending, the dep route keeps
    // holding.
    if needs_reload && !late.is_empty() {
        let _ = rerun.wake.send(());
        return;
    }
    {
        let mut p = rerun.pending.lock().unwrap();
        for dep in &pending {
            p.remove(dep);
        }
    }
    let final_map: DepMap = if needs_reload {
        // Every URL moves to the batch version, this batch's own deps
        // included (parse stamped it already): exactly Vite's commitProcessing
        // on needsReload, where each entry carries the new browserHash, an
        // in-flight request still holding a provisional version is an
        // outdated 504, and the full reload retires every older URL.
        new_map
    } else {
        // Served URLs stay valid: committed deps keep the version they were
        // served under, pending ones their provisional registration version.
        new_map
            .into_iter()
            .map(|(dep, mut meta)| {
                let keep = old_map
                    .and_then(|m| m.get(&dep))
                    .map(|o| url_version(&o.url).to_string())
                    .unwrap_or_else(|| candidate.clone());
                meta.url = dep_url(&meta.file, &keep);
                (dep, meta)
            })
            .collect()
    };
    let bv = if needs_reload {
        candidate.clone()
    } else {
        rerun.browser_version.read().unwrap().clone()
    };
    // The manifest holds only bundled entries (a provisional one points at a
    // file that does not exist yet, which would void the warm boot); the
    // committed map additionally carries every still-pending provisional
    // entry, read under the watch lock so a registration can never interleave
    // between snapshot and commit.
    write_manifest(dir, hash, &bv, names.iter(), &final_map);
    *rerun.browser_version.write().unwrap() = bv;
    rerun.tx.send_modify(|cur| {
        let prev = cur.take();
        let still = rerun.pending.lock().unwrap();
        *cur = Some(Arc::new(merge_pending(final_map, prev.as_deref(), &still)));
    });
    if needs_reload {
        if let Some(cb) = rerun.on_commit.get() {
            cb(&newly);
        }
    } else if !newly.is_empty() {
        println!("oj: new dependencies optimized: {}", newly.join(", "));
    }
}

/// The map a rerun commits: the bundle's output plus the provisional entry of
/// every still-pending dep (one registered while the bundle ran), carried
/// over from the live map so its URL keeps blocking at the dep route until
/// its own batch commits.
fn merge_pending(
    mut built: DepMap,
    prev: Option<&DepMap>,
    still_pending: &std::collections::HashSet<String>,
) -> DepMap {
    if let Some(prev) = prev {
        for dep in still_pending {
            if !built.contains_key(dep) {
                if let Some(meta) = prev.get(dep) {
                    built.insert(dep.clone(), meta.clone());
                }
            }
        }
    }
    built
}

/// A batch (or part of one) that could not bundle: drop the provisional
/// entries and decline future registrations, then reload so their importers
/// recompile onto the per-file path.
fn fail_pending(rerun: &Arc<RerunState>, deps: &[String]) {
    {
        let mut discovered = rerun.discovered.lock().unwrap();
        let mut pending = rerun.pending.lock().unwrap();
        let mut failed = rerun.failed.lock().unwrap();
        for dep in deps {
            discovered.remove(dep);
            pending.remove(dep);
            failed.insert(dep.clone());
        }
    }
    rerun.tx.send_modify(|cur| {
        if let Some(map) = cur {
            let m = Arc::make_mut(map);
            for dep in deps {
                m.remove(dep);
            }
        }
    });
    if let Some(cb) = rerun.on_commit.get() {
        cb(&[]);
    }
}

/// Vite's default: the optimizer crawls the app for deps unless
/// `optimizeDeps.noDiscovery` is set, which leaves only `include`.
fn effective_auto_discover(no_discovery: Option<bool>) -> bool {
    !no_discovery.unwrap_or(false)
}

/// The rolldown vendored next to the binary, for apps whose own Vite brings
/// no bundler; `None` when this build vendors none or the vendor is unusable.
fn vendored_rolldown() -> Option<(&'static str, &'static str)> {
    match oj_cache::start_bundle::vendored_rolldown() {
        oj_cache::start_bundle::VendoredRolldown::Resolved { path, version } => {
            Some((path.as_str(), version.as_str()))
        }
        _ => None,
    }
}

#[derive(Default, Clone)]
pub struct OptimizeInput {
    pub no_discovery: Option<bool>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub entries: Vec<String>,
    /// `build.rolldownOptions.input`: the scan's entries when
    /// `optimizeDeps.entries` is unset, before the html glob (Vite's
    /// computeEntries order).
    pub build_inputs: Vec<String>,
    /// `build.outDir`: the scan's html glob skips it.
    pub build_out_dir: String,
    pub dedupe: Vec<String>,
    pub alias: Vec<(String, String)>,
    /// `optimizeDeps.force`: bypass the cached pre-bundle and rebuild.
    pub force: bool,
    /// `optimizeDeps.esbuildOptions`/`rolldownOptions`: forwarded to the sidecar.
    pub bundler_options: Option<serde_json::Value>,
    /// `optimizeDeps.rolldownOptions` as written; Vite spreads it into the
    /// scan and the bundle.
    pub rolldown_options: Option<serde_json::Value>,
    /// The Rust resolver's settings, so the pre-bundle resolves every dep to the
    /// same file the dev server serves (Vite uses one resolver for both).
    pub conditions: Vec<String>,
    pub main_fields: Vec<String>,
    pub extensions: Vec<String>,
    pub preserve_symlinks: bool,
    /// Vite's `--mode` (getConfigHash folds `define: NODE_ENV || mode`): a dep
    /// prebundled for `development` is not the `production` one.
    pub mode: String,
    /// Vite's `process.env.NODE_ENV || mode`, defined into every dep bundle.
    pub node_env: String,
    /// The app's plugin names (Vite's getConfigHash `plugins`): the scan
    /// resolves through them, so a plugin change can change the dep set.
    pub plugin_names: Vec<String>,
    /// `optimizeDeps.needsInterop`: force `needsInterop: true` whatever the
    /// bundle's export shape (Vite's needsInterop()).
    pub needs_interop: Vec<String>,
}

/// Prebundle mainFields: the resolved config's list with Vite's `pkg.main`
/// fallback appended last, since the sidecar bundler walks the list verbatim.
pub fn optimizer_main_fields(config: &oj_config::OjConfig) -> Vec<String> {
    oj_resolver::with_main_fallback(
        oj_config::resolve_main_fields(config).unwrap_or_else(oj_resolver::default_main_fields),
    )
}

/// Vite's lockfileFormats: each lockfile paired with the patch-package
/// directory whose mtime must also invalidate the prebundle (checkPatchesDir).
const LOCKFILES: &[(&str, Option<&str>)] = &[
    ("node_modules/.pnpm/lock.yaml", None),
    ("node_modules/.package-lock.json", Some("patches")),
    ("node_modules/.yarn-state.yml", None),
    ("bun.lock", Some("patches")),
    (".rush/temp/shrinkwrap-deps.json", None),
    ("aube-lock.yaml", None),
    ("nub.lock", Some("patches")),
    (".pnp.cjs", Some(".yarn/patches")),
    (".pnp.js", Some(".yarn/patches")),
    ("node_modules/.yarn-integrity", Some("patches")),
    ("bun.lockb", Some("patches")),
    // Top-level lockfiles oj has always keyed on (Vite reads the installed
    // node_modules mirrors above instead; hashing both is a superset).
    ("package-lock.json", Some("patches")),
    ("yarn.lock", Some(".yarn/patches")),
    ("pnpm-lock.yaml", None),
    ("deno.lock", None),
];

/// Fold every lockfile in the nearest ancestor directory that has one (Vite's
/// lookupFile), plus its patch-package dir mtime, into `hasher`.
fn hash_lockfiles(root: &Path, hasher: &mut blake3::Hasher) {
    let mut dir = Some(root);
    while let Some(d) = dir {
        let mut found = false;
        for (name, patches) in LOCKFILES {
            if let Ok(bytes) = std::fs::read(d.join(name)) {
                found = true;
                hasher.update(name.as_bytes());
                hasher.update(&bytes);
                if let Some(patches) = patches {
                    if let Ok(meta) = std::fs::metadata(d.join(patches)) {
                        if meta.is_dir() {
                            if let Ok(mtime) = meta.modified() {
                                hasher.update(b"\0p");
                                hasher.update(format!("{mtime:?}").as_bytes());
                            }
                        }
                    }
                }
            }
        }
        if found {
            return;
        }
        dir = d.parent();
    }
}

/// Folds each entry of each list into `hasher`, prefixed by its list's tag.
fn hash_tagged_lists(hasher: &mut blake3::Hasher, lists: &[(&[u8], &Vec<String>)]) {
    for (tag, list) in lists {
        for entry in *list {
            hasher.update(tag);
            hasher.update(entry.as_bytes());
        }
    }
}

fn lockfile_hash(root: &Path, version: &str, input: &OptimizeInput) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(version.as_bytes());
    hash_lockfiles(root, &mut hasher);
    if let Ok(bytes) = std::fs::read(root.join("package.json")) {
        hasher.update(b"package.json");
        hasher.update(&bytes);
    }
    hasher.update(b"\0mode=");
    hasher.update(input.mode.as_bytes());
    hasher.update(b"\0node_env=");
    hasher.update(input.node_env.as_bytes());
    // Fold the optimizer config into the key so include/exclude/entries/dedupe/alias
    // changes invalidate a stale prebundle.
    hash_tagged_lists(
        &mut hasher,
        &[
            (b"\0i", &input.include),
            (b"\0x", &input.exclude),
            (b"\0e", &input.entries),
            (b"\0b", &input.build_inputs),
            (b"\0d", &input.dedupe),
            (b"\0n", &input.needs_interop),
            (b"\0p", &input.plugin_names),
        ],
    );
    hasher.update(b"\0bo=");
    hasher.update(input.build_out_dir.as_bytes());
    for (find, replacement) in &input.alias {
        hasher.update(b"\0a");
        hasher.update(find.as_bytes());
        hasher.update(b"=");
        hasher.update(replacement.as_bytes());
    }
    hasher.update(
        format!(
            "\0discovery:{}",
            effective_auto_discover(input.no_discovery)
        )
        .as_bytes(),
    );
    // The vendor's version can change without an oj version bump only in
    // development, so it keys the prebundle; its path must not (relocating
    // the binary would invalidate every app's cache for nothing).
    if let Some((_, version)) = vendored_rolldown() {
        hasher.update(format!("\0vendor:{version}").as_bytes());
    }
    if let Some(opts) = &input.bundler_options {
        hasher.update(b"\0o");
        hasher.update(opts.to_string().as_bytes());
    }
    hash_tagged_lists(
        &mut hasher,
        &[
            (b"\0c", &input.conditions),
            (b"\0m", &input.main_fields),
            (b"\0t", &input.extensions),
        ],
    );
    hasher.update(&[b'\0', b's', input.preserve_symlinks as u8]);
    hasher.finalize().to_hex().to_string()
}

fn parse_metadata(v: &serde_json::Value, version: &str) -> Option<DepMap> {
    let obj = v.as_object()?;
    let version = version.get(..8).unwrap_or(version);
    let mut map = DepMap::new();
    for (dep, meta) in obj {
        let file = meta.get("file")?.as_str()?.to_string();
        let needs_interop = meta
            .get("needsInterop")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        // Per-dep version override: after a no-reload commit, deps keep the
        // version they were first served under instead of the batch's.
        let v = meta.get("v").and_then(|v| v.as_str()).unwrap_or(version);
        let file_hash = meta
            .get("fileHash")
            .and_then(|h| h.as_str())
            .unwrap_or_default()
            .to_string();
        map.insert(
            dep.clone(),
            DepMeta {
                url: dep_url(&file, v),
                file,
                needs_interop,
                file_hash,
            },
        );
    }
    Some(map)
}

/// The warm-boot manifest, written from the committed map so per-dep versions
/// and file hashes survive a restart (Vite persists its _metadata the same way).
fn write_manifest<'a>(
    dir: &Path,
    hash: &str,
    browser_version: &str,
    discovered: impl Iterator<Item = &'a String>,
    map: &DepMap,
) {
    let metadata: serde_json::Map<String, serde_json::Value> = map
        .iter()
        .map(|(dep, m)| {
            (
                dep.clone(),
                serde_json::json!({
                    "file": m.file,
                    "needsInterop": m.needs_interop,
                    "fileHash": m.file_hash,
                    "v": url_version(&m.url),
                }),
            )
        })
        .collect();
    let manifest = serde_json::json!({
        "hash": hash,
        "browserHash": browser_version,
        "discovered": discovered.collect::<Vec<_>>(),
        "metadata": metadata,
    });
    let _ = std::fs::write(dir.join("manifest.json"), manifest.to_string());
}

struct Manifest {
    map: DepMap,
    /// Deps discovered at serve time in a previous session (their bundles are
    /// already in `map`); the next rerun must keep carrying them.
    discovered: std::collections::BTreeSet<String>,
    /// The version the persisted URLs were written with: the base short hash
    /// until a rerun happened, then the discovered-set hash.
    browser_hash: String,
}

fn load_manifest(dir: &Path, hash: &str) -> Option<Manifest> {
    let raw = std::fs::read_to_string(dir.join("manifest.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    if v.get("hash")?.as_str()? != hash {
        return None;
    }
    let browser_hash = v
        .get("browserHash")
        .and_then(|b| b.as_str())
        .unwrap_or(hash.get(..8).unwrap_or(hash))
        .to_string();
    let map = parse_metadata(v.get("metadata")?, &browser_hash)?;
    for m in map.values() {
        if !dir.join(&m.file).exists() {
            return None;
        }
    }
    let discovered = v
        .get("discovered")
        .and_then(|d| d.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Some(Manifest {
        map,
        discovered,
        browser_hash,
    })
}

/// Deadline for the dep pre-bundle (a wedged bundler must not stall it
/// forever): 120 s default, raised via `OJ_OPTIMIZE_TIMEOUT=<seconds>`.
pub(crate) fn optimizer_timeout() -> std::time::Duration {
    optimizer_timeout_from(oj_env::get().knobs.optimize_timeout.as_deref())
}

fn optimizer_timeout_from(raw: Option<&str>) -> std::time::Duration {
    let secs = raw
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(120);
    std::time::Duration::from_secs(secs)
}

/// `discovered`: serve-time-found deps bundled alongside the configured set
/// (as implicit includes, which never warn). `browser_version` is the `?v=`
/// the emitted URLs carry, and `keep_existing` leaves the previous bundle's
/// files in place so a page that predates the rerun keeps loading until the
/// reload lands.
#[allow(clippy::too_many_arguments)]
async fn run_optimizer(
    root: &Path,
    dir: &Path,
    input: &OptimizeInput,
    host: Option<&crate::plugins::PluginHost>,
    discovered: &std::collections::BTreeSet<String>,
    browser_version: &str,
    keep_existing: bool,
) -> Option<DepMap> {
    let cache = oj_cache::cache_root(root);
    std::fs::create_dir_all(&cache).ok()?;
    // Atomic rename: an engine could import the script while a concurrent oj
    // process rewrites it.
    crate::plugins::ensure_asset(&cache, "optimize-deps.mjs", OPTIMIZE_JS).ok()?;
    let script = cache.join("optimize-deps.mjs");
    // react/jsx-dev-runtime is always prebundled (oj injects the dev JSX
    // runtime), but as oj's own implicit include: unlike the user's list it
    // must not warn when the app has no React.
    let alias: Vec<[&str; 2]> = input
        .alias
        .iter()
        .map(|(f, r)| [f.as_str(), r.as_str()])
        .collect();
    let auto_discover = effective_auto_discover(input.no_discovery);
    // Config as JSON engine-call argument, metadata as the return value: argv
    // has an OS length limit and stdout broke when a dep printed on require.
    let mut cfg = serde_json::json!({
        "root": root.to_string_lossy(),
        "outDir": dir.to_string_lossy(),
        "entries": input.entries,
        "buildInputs": input.build_inputs,
        "buildOutDir": input.build_out_dir,
        "include": input.include,
        "implicitInclude": std::iter::once("react/jsx-dev-runtime".to_string())
            .chain(discovered.iter().cloned())
            .collect::<Vec<_>>(),
        "keepExisting": keep_existing,
        "exclude": input.exclude,
        "dedupe": input.dedupe,
        "alias": alias,
        "needsInterop": input.needs_interop,
        "autoDiscover": auto_discover,
        "nodeEnv": input.node_env,
        "esbuildOptions": input.bundler_options,
        "rolldownOptions": input.rolldown_options,
        "vendoredRolldown": vendored_rolldown().map(|(path, _)| path),
        "resolve": {
            "conditions": input.conditions,
            "mainFields": input.main_fields,
            "extensions": input.extensions,
            "preserveSymlinks": input.preserve_symlinks,
        },
    });
    // Not gated on discovery: with noDiscovery the scan still resolves the
    // include list, and Vite's container resolves manual includes through
    // plugins either way.
    if let Some(host) = host {
        cfg["scanned"] = scan_through_plugins(host, &cfg).await;
    }
    let timeout = optimizer_timeout();
    let job_root = root.to_path_buf();
    let job_script = script.clone();
    // spawn_blocking: the engine job blocks its thread for the whole
    // pre-bundle (see run_engine_job), which must not park a runtime worker.
    let result = tokio::task::spawn_blocking(move || {
        crate::plugins::run_engine_job(&job_root, &job_script, "optimize", cfg, timeout)
    })
    .await
    .ok()?;
    let v = match result {
        Ok(v) => v,
        Err(oj_js::EngineError::Deadline) => {
            eprintln!(
                "oj: the dep pre-bundle did not finish within {}s and was stopped (raise OJ_OPTIMIZE_TIMEOUT for slower machines); deps are served unbundled",
                timeout.as_secs()
            );
            return None;
        }
        Err(e) => {
            eprintln!("oj: optimizer failed: {e}");
            return None;
        }
    };
    let metadata = v.get("metadata")?;
    parse_metadata(metadata, browser_version)
}

/// The scan run in the plugin host (see `optimizeScan` in plugin-host.mjs).
/// Null keeps the scan in the optimizer job: a host that failed, which is
/// logged and costs only plugin-aware resolution.
async fn scan_through_plugins(
    host: &crate::plugins::PluginHost,
    cfg: &serde_json::Value,
) -> serde_json::Value {
    match host.optimize_scan(&cfg.to_string()).await {
        Ok(Some(json)) => serde_json::from_str(&json).unwrap_or(serde_json::Value::Null),
        Ok(None) => serde_json::Value::Null,
        Err(e) => {
            eprintln!("oj: dependency scan through plugins failed ({e}); scanning without them");
            serde_json::Value::Null
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(
        include: &[&str],
        exclude: &[&str],
        entries: &[&str],
        dedupe: &[&str],
    ) -> OptimizeInput {
        OptimizeInput {
            include: include.iter().map(|s| s.to_string()).collect(),
            exclude: exclude.iter().map(|s| s.to_string()).collect(),
            entries: entries.iter().map(|s| s.to_string()).collect(),
            dedupe: dedupe.iter().map(|s| s.to_string()).collect(),
            alias: Vec::new(),
            ..Default::default()
        }
    }

    // Vite's pkg.main fallback: "main" is appended last so a dep that resolves
    // in the dev server never fails the prebundle under a Vite-shaped list.
    #[test]
    fn optimizer_main_fields_append_vites_main_fallback() {
        let from = |json: &str| -> oj_config::OjConfig { serde_json::from_str(json).unwrap() };
        assert_eq!(
            optimizer_main_fields(&from(
                r#"{ "resolve": { "mainFields": ["browser", "module", "jsnext:main", "jsnext"] } }"#
            )),
            ["browser", "module", "jsnext:main", "jsnext", "main"].map(String::from)
        );
        // A configured "main" is never duplicated or outranked.
        assert_eq!(
            optimizer_main_fields(&from(
                r#"{ "resolve": { "mainFields": ["main", "module"] } }"#
            )),
            ["main", "module"].map(String::from)
        );
        // No user list: the dev resolver's defaults (already main-terminated).
        assert_eq!(
            optimizer_main_fields(&from("{}")),
            oj_resolver::default_main_fields()
        );
    }

    fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, contents) in files {
            std::fs::write(dir.path().join(name), contents).unwrap();
        }
        dir
    }

    #[test]
    fn every_optimizer_list_is_distinguishable_in_the_key() {
        let dir = project(&[("package.json", r#"{"name":"app"}"#)]);
        let root = dir.path();
        let key = |i: &OptimizeInput| lockfile_hash(root, "0.0.1", i);

        // The same package in a different list must not reuse a prebundle: an
        // excluded dep is not a prebundled one.
        let base = key(&input(&["react"], &[], &[], &[]));
        assert_ne!(
            base,
            key(&input(&[], &["react"], &[], &[])),
            "include vs exclude"
        );
        assert_ne!(
            base,
            key(&input(&[], &[], &["react"], &[])),
            "include vs entries"
        );
        assert_ne!(
            base,
            key(&input(&[], &[], &[], &["react"])),
            "include vs dedupe"
        );
        let forced = OptimizeInput {
            include: vec!["react".into()],
            needs_interop: vec!["react".into()],
            ..Default::default()
        };
        assert_ne!(base, key(&forced), "needsInterop is part of the key");
        // A plugin added or removed can change what the scan resolves (Vite's
        // getConfigHash keys on the plugin names).
        let with_plugin = OptimizeInput {
            include: vec!["react".into()],
            plugin_names: vec!["app-icons".into()],
            ..Default::default()
        };
        assert_ne!(base, key(&with_plugin), "plugin names are part of the key");
        // ...and neither may a list boundary that merely moves an item across it.
        assert_ne!(
            key(&input(&["a"], &["b"], &[], &[])),
            key(&input(&["a", "b"], &[], &[], &[])),
            "moving an entry across the include/exclude boundary"
        );
        assert_ne!(
            key(&input(&[], &[], &["a", "b"], &[])),
            key(&input(&[], &[], &["a"], &["b"])),
            "moving an entry across the entries/dedupe boundary"
        );
    }

    #[test]
    fn the_key_covers_the_lockfiles_the_version_and_the_aliases() {
        let dir = project(&[
            (
                "package.json",
                r#"{"name":"app","dependencies":{"react":"18"}}"#,
            ),
            ("package-lock.json", r#"{"lockfileVersion":3}"#),
        ]);
        let root = dir.path();
        let empty = OptimizeInput::default();
        let base = lockfile_hash(root, "0.0.1", &empty);

        assert_eq!(base, lockfile_hash(root, "0.0.1", &empty), "deterministic");
        assert_ne!(base, lockfile_hash(root, "0.0.2", &empty), "tool version");

        std::fs::write(root.join("package-lock.json"), r#"{"lockfileVersion":4}"#).unwrap();
        let after_lock = lockfile_hash(root, "0.0.1", &empty);
        assert_ne!(base, after_lock, "a lockfile change must invalidate");

        let dev = OptimizeInput {
            mode: "development".into(),
            ..OptimizeInput::default()
        };
        let prod = OptimizeInput {
            mode: "production".into(),
            ..OptimizeInput::default()
        };
        assert_ne!(
            lockfile_hash(root, "0.0.1", &dev),
            lockfile_hash(root, "0.0.1", &prod),
            "mode"
        );
        let prod_env = OptimizeInput {
            node_env: "production".into(),
            ..OptimizeInput::default()
        };
        assert_ne!(
            lockfile_hash(root, "0.0.1", &empty),
            lockfile_hash(root, "0.0.1", &prod_env),
            "NODE_ENV"
        );
        let with_build_input = OptimizeInput {
            build_inputs: vec!["src/main.ts".into()],
            ..OptimizeInput::default()
        };
        assert_ne!(
            lockfile_hash(root, "0.0.1", &empty),
            lockfile_hash(root, "0.0.1", &with_build_input),
            "build inputs feed the scan entries"
        );
        let with_out_dir = OptimizeInput {
            build_out_dir: "build".into(),
            ..OptimizeInput::default()
        };
        assert_ne!(
            lockfile_hash(root, "0.0.1", &empty),
            lockfile_hash(root, "0.0.1", &with_out_dir),
            "build outDir shapes the html glob"
        );

        let aliased = OptimizeInput {
            alias: vec![("~".into(), "./src".into())],
            ..OptimizeInput::default()
        };
        assert_ne!(after_lock, lockfile_hash(root, "0.0.1", &aliased), "alias");
        let swapped = OptimizeInput {
            alias: vec![("./src".into(), "~".into())],
            ..OptimizeInput::default()
        };
        assert_ne!(
            lockfile_hash(root, "0.0.1", &aliased),
            lockfile_hash(root, "0.0.1", &swapped),
            "an alias is directional"
        );
    }

    #[test]
    fn every_vite_lockfile_format_and_the_patches_dir_key_the_prebundle() {
        let dir = project(&[("package.json", r#"{"name":"app"}"#)]);
        let root = dir.path();
        let empty = OptimizeInput::default();
        let key = || lockfile_hash(root, "v", &empty);
        let bare = key();

        // Text bun.lock and deno.lock (audit: only bun.lockb was keyed).
        std::fs::write(root.join("bun.lock"), "{}").unwrap();
        let bun = key();
        assert_ne!(bare, bun, "bun.lock");
        std::fs::write(root.join("bun.lock"), "{\"a\":1}").unwrap();
        assert_ne!(bun, key(), "bun.lock content");
        std::fs::remove_file(root.join("bun.lock")).unwrap();
        std::fs::write(root.join("deno.lock"), "{}").unwrap();
        assert_ne!(bare, key(), "deno.lock");
        std::fs::remove_file(root.join("deno.lock")).unwrap();

        // The installed mirror Vite reads (node_modules/.package-lock.json).
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        std::fs::write(root.join("node_modules/.package-lock.json"), "{}").unwrap();
        let installed = key();
        assert_ne!(bare, installed, "node_modules/.package-lock.json");

        // patch-package: a `patches/` directory next to an npm lockfile is part
        // of the key (Vite's checkPatchesDir), so re-patching a dep re-bundles.
        std::fs::create_dir_all(root.join("patches")).unwrap();
        let patched = key();
        assert_ne!(installed, patched, "patches dir");
        assert_eq!(patched, key(), "deterministic while nothing changes");
    }

    #[test]
    fn the_lockfile_is_looked_up_in_ancestor_directories() {
        // A workspace package's prebundle keys on the monorepo lockfile above it
        // (Vite's lookupFile walks up from root).
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("packages/app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("package.json"), r#"{"name":"app"}"#).unwrap();
        let empty = OptimizeInput::default();
        let before = lockfile_hash(&app, "v", &empty);
        std::fs::write(dir.path().join("pnpm-lock.yaml"), "lockfileVersion: 9").unwrap();
        let after = lockfile_hash(&app, "v", &empty);
        assert_ne!(before, after, "ancestor lockfile is keyed");
        std::fs::write(dir.path().join("pnpm-lock.yaml"), "lockfileVersion: 10").unwrap();
        assert_ne!(
            after,
            lockfile_hash(&app, "v", &empty),
            "and its content matters"
        );
    }

    #[test]
    fn a_manifest_is_only_reused_when_its_hash_and_its_files_are_there() {
        let dir = tempfile::tempdir().unwrap();
        let deps = dir.path().join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        std::fs::write(deps.join("react.js"), "export default 1;").unwrap();
        std::fs::write(
            deps.join("manifest.json"),
            r#"{"hash":"abc","metadata":{"react":{"file":"react.js","needsInterop":false}}}"#,
        )
        .unwrap();

        let m = load_manifest(&deps, "abc").expect("matching hash loads");
        assert_eq!(m.map["react"].file, "react.js");
        assert_eq!(m.map["react"].url, "/@oj-deps/react.js?v=abc");
        assert!(!m.map["react"].needs_interop);
        assert!(
            m.discovered.is_empty() && m.browser_hash == "abc",
            "no rerun persisted"
        );

        // A rerun's manifest restores the discovered set and the bumped
        // version its URLs were written with.
        std::fs::write(
            deps.join("manifest.json"),
            r#"{"hash":"abc","browserHash":"feedf00d","discovered":["latedep"],"metadata":{"react":{"file":"react.js","needsInterop":false}}}"#,
        )
        .unwrap();
        let rerun = load_manifest(&deps, "abc").expect("rerun manifest loads");
        assert_eq!(rerun.browser_hash, "feedf00d");
        assert_eq!(rerun.map["react"].url, "/@oj-deps/react.js?v=feedf00d");
        assert_eq!(
            rerun.discovered.iter().cloned().collect::<Vec<_>>(),
            ["latedep"]
        );
        std::fs::write(
            deps.join("manifest.json"),
            r#"{"hash":"abc","metadata":{"react":{"file":"react.js","needsInterop":false}}}"#,
        )
        .unwrap();

        assert!(load_manifest(&deps, "different").is_none(), "stale hash");

        // A manifest whose prebundle is gone is not a warm cache.
        std::fs::remove_file(deps.join("react.js")).unwrap();
        assert!(load_manifest(&deps, "abc").is_none(), "missing dep file");
    }

    #[test]
    fn a_malformed_manifest_is_a_miss_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let deps = dir.path().join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        for contents in [
            "",
            "{",
            "null",
            "[]",
            r#"{"metadata":{}}"#,
            r#"{"hash":"abc"}"#,
            r#"{"hash":123,"metadata":{}}"#,
            r#"{"hash":"abc","metadata":[]}"#,
            r#"{"hash":"abc","metadata":{"react":{}}}"#,
            r#"{"hash":"abc","metadata":{"react":{"file":42}}}"#,
        ] {
            std::fs::write(deps.join("manifest.json"), contents).unwrap();
            assert!(
                load_manifest(&deps, "abc").is_none(),
                "accepted {contents:?}"
            );
        }
        // An empty metadata object is a legitimate warm cache with no deps.
        std::fs::write(
            deps.join("manifest.json"),
            r#"{"hash":"abc","metadata":{}}"#,
        )
        .unwrap();
        assert!(load_manifest(&deps, "abc")
            .expect("empty is valid")
            .map
            .is_empty());
    }

    fn test_deps(discovery: bool, exclude: Vec<String>) -> OptimizedDeps {
        let (tx, rx) = watch::channel(Some(Arc::new(DepMap::new())));
        let (wake, _wake_rx) = tokio::sync::mpsc::unbounded_channel();
        let rerun = Arc::new(RerunState {
            hash: "testhash".into(),
            tx,
            discovered: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            pending: std::sync::Mutex::new(std::collections::HashSet::new()),
            wake,
            browser_version: std::sync::RwLock::new("00000000".into()),
            failed: std::sync::Mutex::new(std::collections::HashSet::new()),
            dead: std::sync::atomic::AtomicBool::new(false),
            exclude,
            discovery,
            on_commit: std::sync::OnceLock::new(),
        });
        OptimizedDeps {
            rx,
            dir: PathBuf::new(),
            version: "00000000".into(),
            rerun: Some(rerun),
        }
    }

    #[test]
    fn registration_declines_excluded_failed_dead_and_no_discovery() {
        let deps = test_deps(true, vec!["left-out".into()]);
        assert!(deps.may_register("lodash"));
        assert!(!deps.may_register("left-out"));
        assert!(
            !deps.may_register("left-out/sub"),
            "exclude covers subpaths"
        );
        let r = deps.rerun.as_ref().unwrap();
        r.failed.lock().unwrap().insert("broken".into());
        assert!(deps.register_missing("broken", false).is_none());
        // A dead optimizer (no rolldown found, the initial run failed): no
        // registration, no phantom URL, every dep keeps the per-file path.
        r.dead.store(true, std::sync::atomic::Ordering::Release);
        assert!(!deps.may_register("lodash"));
        assert!(deps.register_missing("lodash", false).is_none());
        assert!(deps.meta_now("lodash").is_none());
        let off = test_deps(false, vec![]);
        assert!(!off.may_register("lodash"), "noDiscovery declines all");
    }

    #[test]
    fn registration_rewrites_to_the_future_url() {
        let deps = test_deps(true, vec![]);
        let meta = deps.register_missing("@scope/pkg", true).unwrap();
        assert_eq!(meta.file, "scope_pkg.mjs");
        assert!(meta.url.starts_with("/@oj-deps/scope_pkg.mjs?v="));
        assert!(meta.needs_interop);
        let live = deps.meta_now("@scope/pkg").expect("live map sees it");
        assert_eq!(live.url, meta.url);
        let again = deps.register_missing("@scope/pkg", true).unwrap();
        assert_eq!(again.url, meta.url, "re-registration is idempotent");
    }

    #[tokio::test(start_paused = true)]
    async fn debounce_slides_per_wake_and_drains_the_burst() {
        let window = std::time::Duration::from_millis(100);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(()).unwrap();
        rx.recv().await; // the loop's outer recv consumed the first wake
        let start = tokio::time::Instant::now();
        let waiter = tokio::spawn(async move {
            debounce_wakes(&mut rx, window).await;
            (start.elapsed(), rx)
        });
        // Two registrations inside successive windows: each slides it.
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        tx.send(()).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        tx.send(()).unwrap();
        let (elapsed, mut rx) = waiter.await.unwrap();
        assert_eq!(
            elapsed,
            std::time::Duration::from_millis(220),
            "60 + 60, then one full quiet window"
        );
        assert!(
            rx.try_recv().is_err(),
            "every wake of the burst joined the one batch"
        );
    }

    #[test]
    fn merge_pending_carries_mid_rerun_registrations() {
        let meta = |file: &str| DepMeta {
            file: file.into(),
            needs_interop: false,
            url: dep_url(file, "aaaaaaaa"),
            file_hash: String::new(),
        };
        let mut prev = DepMap::new();
        prev.insert("early".into(), meta("early.mjs"));
        prev.insert("late".into(), meta("late.mjs"));
        let mut built = DepMap::new();
        built.insert(
            "early".into(),
            DepMeta {
                file_hash: "bb".into(),
                ..meta("early.mjs")
            },
        );
        let still: std::collections::HashSet<String> = ["late".to_string()].into_iter().collect();
        let out = merge_pending(built.clone(), Some(&prev), &still);
        assert_eq!(
            out["late"].url, prev["late"].url,
            "a dep registered mid-rerun keeps its provisional entry"
        );
        assert_eq!(out["early"].file_hash, "bb", "bundled entries win");
        assert!(
            !merge_pending(built, None, &still).contains_key("late"),
            "nothing to carry before the first commit"
        );
    }

    #[test]
    fn browser_hash_tracks_the_discovered_set() {
        let set = |deps: &[&str]| -> std::collections::BTreeSet<String> {
            deps.iter().map(|d| d.to_string()).collect()
        };
        let base = browser_hash("hash", set(&[]).iter());
        let one = browser_hash("hash", set(&["lodash"]).iter());
        let two = browser_hash("hash", set(&["lodash", "dayjs"]).iter());
        assert_eq!(base.len(), 8);
        assert_ne!(base, one, "a discovered dep bumps the version");
        assert_ne!(one, two);
        // The set is sorted: registration order cannot change the hash.
        assert_eq!(two, browser_hash("hash", set(&["dayjs", "lodash"]).iter()));
    }

    #[test]
    fn manifest_round_trips_per_dep_versions_and_file_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let deps = dir.path().join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        std::fs::write(deps.join("react.js"), "export default 1;").unwrap();
        std::fs::write(deps.join("latedep.mjs"), "export default 2;").unwrap();
        let mut map = DepMap::new();
        map.insert(
            "react".into(),
            DepMeta {
                file: "react.js".into(),
                needs_interop: false,
                url: dep_url("react.js", "11111111"),
                file_hash: "aaaa".into(),
            },
        );
        map.insert(
            "latedep".into(),
            DepMeta {
                file: "latedep.mjs".into(),
                needs_interop: true,
                url: dep_url("latedep.mjs", "22222222"),
                file_hash: "bbbb".into(),
            },
        );
        let discovered = ["latedep".to_string()];
        write_manifest(&deps, "abc", "11111111", discovered.iter(), &map);
        let m = load_manifest(&deps, "abc").expect("round trip");
        assert_eq!(m.browser_hash, "11111111");
        assert_eq!(m.map["react"].url, "/@oj-deps/react.js?v=11111111");
        assert_eq!(m.map["latedep"].url, "/@oj-deps/latedep.mjs?v=22222222");
        assert_eq!(m.map["latedep"].file_hash, "bbbb");
        assert!(m.map["latedep"].needs_interop);
        assert_eq!(
            m.discovered.iter().cloned().collect::<Vec<_>>(),
            ["latedep"]
        );
    }

    #[test]
    fn exclude_list_matches_deps_and_their_subpaths() {
        let list = vec!["lodash".to_string(), "@scope/pkg".to_string()];
        assert!(module_list_contains(&list, "lodash"));
        assert!(module_list_contains(&list, "lodash/debounce"));
        assert!(module_list_contains(&list, "@scope/pkg/sub"));
        assert!(!module_list_contains(&list, "lodash-es"));
    }

    #[test]
    fn needs_interop_defaults_to_true_when_the_optimizer_does_not_say() {
        // Interop is the safe default: assuming an ESM dep needs none would
        // break `import x from "cjs-dep"` at runtime.
        let v: serde_json::Value = serde_json::from_str(r#"{"dep":{"file":"dep.js"}}"#).unwrap();
        let map = parse_metadata(&v, "0123456789abcdef").unwrap();
        assert!(map["dep"].needs_interop);
    }

    #[test]
    fn dep_urls_carry_the_short_prebundle_version() {
        // Vite stamps `?v=<browserHash>` (8 hex chars) on every optimized dep URL
        // so the immutable response can never outlive the prebundle it came from.
        let v: serde_json::Value =
            serde_json::from_str(r#"{"react":{"file":"react.js","needsInterop":false}}"#).unwrap();
        let map = parse_metadata(&v, "0123456789abcdef0123").unwrap();
        assert_eq!(map["react"].url, "/@oj-deps/react.js?v=01234567");
        assert_eq!(map["react"].file, "react.js");
        assert_eq!(
            dep_url("x.js", ""),
            "/@oj-deps/x.js",
            "no version, no query"
        );
        // A different prebundle hash is a different URL.
        let other = parse_metadata(&v, "fedcba9876543210").unwrap();
        assert_ne!(other["react"].url, map["react"].url);
    }

    #[test]
    fn chunk_imports_of_an_entry_carry_that_entrys_own_url() {
        let meta = |file: &str, version: &str| DepMeta {
            file: file.into(),
            needs_interop: false,
            url: dep_url(file, version),
            file_hash: String::new(),
        };
        let map: DepMap = [
            (
                "@tiptap/pm/gapcursor",
                meta("tiptap_pm_gapcursor.mjs", "aaaa1111"),
            ),
            ("late", meta("late.mjs", "bbbb2222")),
            ("react", meta("react__oj_named.mjs", "aaaa1111")),
        ]
        .into_iter()
        .map(|(dep, meta)| (dep.to_string(), meta))
        .collect();
        let chunk = concat!(
            "import { a } from \"./dist-B4TcxAGx.mjs\";\n",
            "import { gapCursor } from \"./tiptap_pm_gapcursor.mjs\";\n",
            "const late = () => import(\"./late.mjs\");\n",
            "export { a, gapCursor, late };\n",
        );
        assert_eq!(
            linked_entry_imports(&map, "dist-C0ffee00.mjs", chunk.as_bytes()).as_deref(),
            Some(concat!(
                "import { a } from \"./dist-B4TcxAGx.mjs\";\n",
                "import { gapCursor } from \"./tiptap_pm_gapcursor.mjs?v=aaaa1111\";\n",
                "const late = () => import(\"./late.mjs?v=bbbb2222\");\n",
                "export { a, gapCursor, late };\n",
            )),
            "each entry gets its own version; chunks stay unversioned"
        );
        let facade = "import __m from \"./react.mjs\";\nexport default __m;\n";
        assert!(
            linked_entry_imports(&map, "react__oj_named.mjs", facade.as_bytes()).is_none(),
            "the bundle behind a named-export facade is not an entry"
        );
        let unversioned: DepMap = [(
            "@tiptap/pm/gapcursor".to_string(),
            meta("tiptap_pm_gapcursor.mjs", ""),
        )]
        .into_iter()
        .collect();
        assert!(
            linked_entry_imports(&unversioned, "dist-C0ffee00.mjs", chunk.as_bytes()).is_none(),
            "an unversioned entry URL is the relative one already"
        );
    }

    #[test]
    fn optimizer_timeout_defaults_and_reads_env_seconds() {
        assert_eq!(optimizer_timeout_from(None).as_secs(), 120);
        assert_eq!(optimizer_timeout_from(Some("300")).as_secs(), 300);
        assert_eq!(optimizer_timeout_from(Some(" 30 ")).as_secs(), 30);
        // Garbage and zero fall back to the default rather than disabling the bound.
        assert_eq!(optimizer_timeout_from(Some("junk")).as_secs(), 120);
        assert_eq!(optimizer_timeout_from(Some("0")).as_secs(), 120);
    }

    #[tokio::test]
    async fn a_disabled_optimizer_is_ready_immediately_and_empty() {
        let deps = OptimizedDeps::disabled();
        assert!(deps.ready().await.is_empty());
        assert_eq!(deps.dir(), Path::new(""));
    }

    // The engine-run pre-bundle itself lives in tests/optimize_prebundle.rs:
    // rolldown is a napi addon, and only integration-test binaries get the
    // exported-symbols link flag from build.rs.

    #[test]
    fn discovery_is_on_unless_no_discovery_is_set() {
        assert!(effective_auto_discover(None));
        assert!(effective_auto_discover(Some(false)));
        assert!(!effective_auto_discover(Some(true)));
        let dir = project(&[("package.json", r#"{"name":"app"}"#)]);
        let key = |no_discovery| {
            lockfile_hash(
                dir.path(),
                "v",
                &OptimizeInput {
                    no_discovery,
                    ..Default::default()
                },
            )
        };
        assert_eq!(
            key(None),
            key(Some(false)),
            "unset is the discovering default"
        );
        assert_ne!(
            key(None),
            key(Some(true)),
            "noDiscovery is its own prebundle"
        );
    }

    #[test]
    fn resolve_settings_change_the_prebundle_hash() {
        let dir = std::env::temp_dir().join(format!("oj-opt-hash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let base = OptimizeInput {
            include: vec!["dep".into()],
            conditions: vec!["browser".into(), "import".into()],
            ..Default::default()
        };
        let mut with_dev = base.clone();
        with_dev.conditions.push("development".into());
        let mut fields = base.clone();
        fields.main_fields = vec!["main".into()];
        let mut links = base.clone();
        links.preserve_symlinks = true;
        let h = |i: &OptimizeInput| lockfile_hash(&dir, "v", i);
        assert_ne!(h(&base), h(&with_dev));
        assert_ne!(h(&base), h(&fields));
        assert_ne!(h(&base), h(&links));
        assert_eq!(h(&base), h(&base.clone()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
