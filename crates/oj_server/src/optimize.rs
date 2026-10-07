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

pub struct DepMeta {
    pub file: String,
    pub needs_interop: bool,
    /// Importer rewrite target `/@oj-deps/<file>?v=<version>`; the version query
    /// is what lets the response be served immutable (Vite's ensureVersionQuery).
    pub url: String,
}

pub type DepMap = HashMap<String, DepMeta>;

pub struct OptimizedDeps {
    rx: watch::Receiver<Option<Arc<DepMap>>>,
    dir: PathBuf,
    /// Short prebundle hash (Vite's browserHash): changes with the lockfile or
    /// optimizer config so stale immutable entries never share a URL. Empty when disabled.
    version: String,
}

/// `/@oj-deps/<file>?v=<version>` (Vite: `<cacheDir>/deps/<file>?v=<browserHash>`).
pub fn dep_url(file: &str, version: &str) -> String {
    if version.is_empty() {
        format!("/@oj-deps/{file}")
    } else {
        format!("/@oj-deps/{file}?v={version}")
    }
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
        if let Some(map) = cached {
            let _ = tx.send(Some(Arc::new(map)));
        } else {
            let root = root.to_path_buf();
            let dir_task = dir.clone();
            tokio::spawn(async move {
                let map = run_optimizer(&root, &dir_task, &hash, &input, host.as_deref())
                    .await
                    .unwrap_or_default();
                let _ = tx.send(Some(Arc::new(map)));
            });
        }
        OptimizedDeps {
            rx,
            dir,
            version: short,
        }
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
            (b"\0d", &input.dedupe),
            (b"\0n", &input.needs_interop),
            (b"\0p", &input.plugin_names),
        ],
    );
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

fn parse_metadata(v: &serde_json::Value, hash: &str) -> Option<DepMap> {
    let obj = v.as_object()?;
    let version = hash.get(..8).unwrap_or(hash);
    let mut map = DepMap::new();
    for (dep, meta) in obj {
        let file = meta.get("file")?.as_str()?.to_string();
        let needs_interop = meta
            .get("needsInterop")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        map.insert(
            dep.clone(),
            DepMeta {
                url: dep_url(&file, version),
                file,
                needs_interop,
            },
        );
    }
    Some(map)
}

fn load_manifest(dir: &Path, hash: &str) -> Option<DepMap> {
    let raw = std::fs::read_to_string(dir.join("manifest.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    if v.get("hash")?.as_str()? != hash {
        return None;
    }
    let map = parse_metadata(v.get("metadata")?, hash)?;
    for m in map.values() {
        if !dir.join(&m.file).exists() {
            return None;
        }
    }
    Some(map)
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

async fn run_optimizer(
    root: &Path,
    dir: &Path,
    hash: &str,
    input: &OptimizeInput,
    host: Option<&crate::plugins::PluginHost>,
) -> Option<DepMap> {
    let cache = oj_cache::cache_root(root);
    std::fs::create_dir_all(&cache).ok()?;
    // Atomic rename: an engine could import the script while a concurrent oj
    // process rewrites it.
    crate::plugins::ensure_asset(&cache, "optimize-deps.mjs", OPTIMIZE_JS).ok()?;
    let script = cache.join("optimize-deps.mjs");
    // react/jsx-dev-runtime is always prebundled (oj injects the dev JSX runtime);
    // merge it with any user optimizeDeps.include.
    let mut include = vec!["react/jsx-dev-runtime".to_string()];
    for dep in &input.include {
        if !include.contains(dep) {
            include.push(dep.clone());
        }
    }
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
        "include": include,
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
    let map = parse_metadata(metadata, hash)?;
    let manifest = serde_json::json!({ "hash": hash, "metadata": metadata });
    let _ = std::fs::write(dir.join("manifest.json"), manifest.to_string());
    Some(map)
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

        let map = load_manifest(&deps, "abc").expect("matching hash loads");
        assert_eq!(map["react"].file, "react.js");
        assert_eq!(map["react"].url, "/@oj-deps/react.js?v=abc");
        assert!(!map["react"].needs_interop);

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
            .is_empty());
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
