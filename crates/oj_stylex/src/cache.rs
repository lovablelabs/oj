//! Local content-addressed compile cache (docs/design-cache.md): entries pair
//! the result with an fs-observation log, replayed to validate every hit.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::api::{
    FileContext, SourceCompileResult, might_contain_stylex, transform_source_with_dep_log_in,
};
use crate::errors::StylexError;
use crate::eval::value::{EvalValue, JsObjectMap};
use crate::module_resolution::{FsProvider, ResolveConfig, path_relative};
use crate::options::ResolvedOptions;
use crate::rules::StylexRule;

// v2: options key material moved to the precomputed ResolvedOptions::cache_repr.
pub const CACHE_SCHEMA_VERSION: u32 = 2;

/// Bump on any behavior-affecting compiler change (the STYLEX_PASS_VERSION
/// pattern): the crate version alone does not move per commit.
pub const STYLEX_PASS_VERSION: &str = "0.19.0-rs.2";

pub fn compiler_fingerprint() -> &'static str {
    static FINGERPRINT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    FINGERPRINT.get_or_init(|| {
        format!(
            "{}+{STYLEX_PASS_VERSION}+schema{CACHE_SCHEMA_VERSION}",
            env!("CARGO_PKG_VERSION")
        )
    })
}

// ------------------------------------------------------------ dep recording

/// One fs observation made during a compile, with its full outcome.
#[derive(Clone, Debug, PartialEq)]
pub enum DepEvent {
    NearestPackage {
        from: PathBuf,
        found: Option<(String, PathBuf)>,
        /// blake3 of the found package.json bytes: any edit there is a miss.
        package_json_hash: Option<String>,
    },
    ResolveImport {
        specifier: String,
        importer: PathBuf,
        resolved: Option<PathBuf>,
    },
    Exists {
        path: PathBuf,
        found: bool,
    },
    /// The `fs.existsSync` probe of the alias branches (directories count).
    ExistsAny {
        path: PathBuf,
        found: bool,
    },
}

/// Every fs observation of one compile, with the root the paths anchor to.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DepLog {
    /// Canonicalized compile root (`ctx.cwd`); in-root paths serialize relative.
    pub root: PathBuf,
    /// The root as the caller spelled it (macOS tempdirs alias /var → /private/var).
    pub raw_root: PathBuf,
    pub events: Vec<DepEvent>,
}

/// [`FsProvider`] wrapper logging every call + outcome for cache validation.
pub struct RecordingFs<'a> {
    inner: &'a dyn FsProvider,
    root: PathBuf,
    raw_root: PathBuf,
    events: Mutex<Vec<DepEvent>>,
}

impl<'a> RecordingFs<'a> {
    pub fn new(inner: &'a dyn FsProvider, root: &Path) -> Self {
        let canonical = inner.canonicalize_root(root);
        Self {
            inner,
            root: canonical,
            raw_root: root.to_path_buf(),
            events: Mutex::new(Vec::new()),
        }
    }

    pub fn into_log(self) -> DepLog {
        DepLog {
            root: self.root,
            raw_root: self.raw_root,
            events: self
                .events
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        }
    }

    fn record(&self, event: DepEvent) {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !events.contains(&event) {
            events.push(event);
        }
    }
}

impl FsProvider for RecordingFs<'_> {
    fn nearest_package(&self, from: &Path) -> Option<(String, PathBuf)> {
        let found = self.inner.nearest_package(from);
        let package_json_hash = found
            .as_ref()
            .and_then(|(_, dir)| self.inner.hash_file(&dir.join("package.json")));
        self.record(DepEvent::NearestPackage {
            from: from.to_path_buf(),
            found: found.clone(),
            package_json_hash,
        });
        found
    }

    fn resolve_import(
        &self,
        specifier: &str,
        importer: &Path,
        config: ResolveConfig<'_>,
    ) -> Option<PathBuf> {
        let resolved = self.inner.resolve_import(specifier, importer, config);
        self.record(DepEvent::ResolveImport {
            specifier: specifier.to_string(),
            importer: importer.to_path_buf(),
            resolved: resolved.clone(),
        });
        resolved
    }

    fn exists(&self, p: &Path) -> bool {
        let found = self.inner.exists(p);
        self.record(DepEvent::Exists {
            path: p.to_path_buf(),
            found,
        });
        found
    }

    fn exists_any(&self, p: &Path) -> bool {
        let found = self.inner.exists_any(p);
        self.record(DepEvent::ExistsAny {
            path: p.to_path_buf(),
            found,
        });
        found
    }

    fn hash_file(&self, p: &Path) -> Option<String> {
        self.inner.hash_file(p)
    }
}

// --------------------------------------------------------------------- key

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CacheKey([u8; 32]);

impl CacheKey {
    pub fn hex(&self) -> String {
        blake3::Hash::from(self.0).to_hex().to_string()
    }
}

/// `filename_key` must be the compile path relative to `ctx.cwd` (see
/// [`filename_key`]) so entries are shareable across worktrees.
pub fn cache_key(source: &str, filename_key: &str, options: &ResolvedOptions) -> CacheKey {
    cache_key_with_fingerprint(source, filename_key, options, compiler_fingerprint())
}

pub fn cache_key_with_fingerprint(
    source: &str,
    filename_key: &str,
    options: &ResolvedOptions,
    fingerprint: &str,
) -> CacheKey {
    // The precomputed Debug capture keys every ResolvedOptions field, present
    // and future — no per-field listing to drift out of sync with options.rs.
    let mut hasher = blake3::Hasher::new();
    for field in [source, filename_key, &options.cache_repr, fingerprint] {
        hasher.update(&(field.len() as u64).to_le_bytes());
        hasher.update(field.as_bytes());
    }
    CacheKey(*hasher.finalize().as_bytes())
}

pub fn filename_key(cwd: &Path, filename: &Path) -> String {
    path_relative(cwd, filename)
}

// ------------------------------------------------------------- wire format

#[derive(Serialize, Deserialize)]
struct EntryWire {
    schema: u32,
    fingerprint: String,
    root: String,
    deps: Vec<DepEventWire>,
    result: ResultWire,
}

/// In-root paths serialize root-relative so a hit can revalidate from any
/// worktree; an absolute path pins the entry to its recorded root.
#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(tag = "t")]
enum PathWire {
    #[serde(rename = "r")]
    Rel { p: String },
    #[serde(rename = "a")]
    Abs { p: String },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "t")]
enum DepEventWire {
    #[serde(rename = "pkg")]
    NearestPackage {
        from: PathWire,
        found: Option<(String, PathWire)>,
        hash: Option<String>,
    },
    #[serde(rename = "imp")]
    ResolveImport {
        specifier: String,
        importer: PathWire,
        resolved: Option<PathWire>,
    },
    #[serde(rename = "ex")]
    Exists { path: PathWire, found: bool },
    #[serde(rename = "exa")]
    ExistsAny { path: PathWire, found: bool },
}

#[derive(Serialize, Deserialize)]
struct ResultWire {
    code: String,
    map: Option<String>,
    rules: Vec<StylexRule>,
    modified: bool,
    create_objects: Vec<(Option<String>, ObjWire)>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
struct ObjWire {
    entries: Vec<(String, ValueWire)>,
    css_type: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(tag = "t")]
enum ValueWire {
    Null,
    Undef,
    Bool {
        v: bool,
    },
    /// f64 bits — exact for NaN payloads, -0.0, and infinities.
    Num {
        bits: u64,
    },
    Str {
        v: String,
    },
    Arr {
        v: Vec<ValueWire>,
    },
    Obj {
        v: ObjWire,
    },
}

fn encode_value(value: &EvalValue) -> ValueWire {
    match value {
        EvalValue::Null => ValueWire::Null,
        EvalValue::Undefined => ValueWire::Undef,
        EvalValue::Bool(b) => ValueWire::Bool { v: *b },
        EvalValue::Num(n) => ValueWire::Num { bits: n.to_bits() },
        EvalValue::Str(s) => ValueWire::Str { v: s.clone() },
        EvalValue::Arr(items) => ValueWire::Arr {
            v: items.iter().map(encode_value).collect(),
        },
        EvalValue::Obj(map) => ValueWire::Obj {
            v: encode_object(map),
        },
    }
}

fn decode_value(wire: ValueWire) -> EvalValue {
    match wire {
        ValueWire::Null => EvalValue::Null,
        ValueWire::Undef => EvalValue::Undefined,
        ValueWire::Bool { v } => EvalValue::Bool(v),
        ValueWire::Num { bits } => EvalValue::Num(f64::from_bits(bits)),
        ValueWire::Str { v } => EvalValue::Str(v),
        ValueWire::Arr { v } => EvalValue::Arr(v.into_iter().map(decode_value).collect()),
        ValueWire::Obj { v } => EvalValue::Obj(Arc::new(decode_object(v))),
    }
}

fn encode_object(map: &JsObjectMap) -> ObjWire {
    ObjWire {
        entries: map
            .entries()
            .map(|(k, v)| (k.to_string(), encode_value(v)))
            .collect(),
        css_type: map.css_type().map(str::to_string),
    }
}

fn decode_object(wire: ObjWire) -> JsObjectMap {
    let mut map = JsObjectMap::new();
    for (k, v) in wire.entries {
        map.insert(k, decode_value(v));
    }
    if let Some(syntax) = wire.css_type {
        map.set_css_type(syntax);
    }
    map
}

fn encode_result(result: &SourceCompileResult) -> ResultWire {
    ResultWire {
        code: result.code.clone(),
        map: result.map.clone(),
        rules: result.rules.clone(),
        modified: result.modified,
        create_objects: result
            .create_objects
            .iter()
            .map(|(name, obj)| (name.clone(), encode_object(obj)))
            .collect(),
    }
}

fn decode_result(wire: ResultWire) -> SourceCompileResult {
    SourceCompileResult {
        code: wire.code,
        map: wire.map,
        rules: wire.rules,
        modified: wire.modified,
        create_objects: wire
            .create_objects
            .into_iter()
            .map(|(name, obj)| (name, Arc::new(decode_object(obj))))
            .collect(),
    }
}

fn to_wire_path(log: &DepLog, p: &Path) -> PathWire {
    for root in [&log.root, &log.raw_root] {
        if let Ok(rel) = p.strip_prefix(root) {
            return PathWire::Rel {
                p: rel.to_string_lossy().replace('\\', "/"),
            };
        }
    }
    PathWire::Abs {
        p: p.to_string_lossy().into_owned(),
    }
}

fn to_wire_event(log: &DepLog, event: &DepEvent) -> DepEventWire {
    match event {
        DepEvent::NearestPackage {
            from,
            found,
            package_json_hash,
        } => DepEventWire::NearestPackage {
            from: to_wire_path(log, from),
            found: found
                .as_ref()
                .map(|(name, dir)| (name.clone(), to_wire_path(log, dir))),
            hash: package_json_hash.clone(),
        },
        DepEvent::ResolveImport {
            specifier,
            importer,
            resolved,
        } => DepEventWire::ResolveImport {
            specifier: specifier.clone(),
            importer: to_wire_path(log, importer),
            resolved: resolved.as_ref().map(|p| to_wire_path(log, p)),
        },
        DepEvent::Exists { path, found } => DepEventWire::Exists {
            path: to_wire_path(log, path),
            found: *found,
        },
        DepEvent::ExistsAny { path, found } => DepEventWire::ExistsAny {
            path: to_wire_path(log, path),
            found: *found,
        },
    }
}

// ---------------------------------------------------------- hit validation

struct Anchor {
    current_root: PathBuf,
    same_root: bool,
}

/// `None` on an absolute path under a different root: such entries are
/// root-pinned because relative derivations cannot reconstruct them.
fn from_wire_path(anchor: &Anchor, wire: &PathWire) -> Option<PathBuf> {
    match wire {
        PathWire::Rel { p } => Some(anchor.current_root.join(p)),
        PathWire::Abs { p } if anchor.same_root => Some(PathBuf::from(p)),
        PathWire::Abs { .. } => None,
    }
}

fn deps_valid(
    entry: &EntryWire,
    current_root: &Path,
    fs: &dyn FsProvider,
    config: ResolveConfig<'_>,
) -> bool {
    let canonical = fs.canonicalize_root(current_root);
    let anchor = Anchor {
        same_root: canonical == Path::new(&entry.root),
        current_root: canonical,
    };
    entry
        .deps
        .iter()
        .all(|dep| dep_valid(dep, &anchor, fs, config))
}

fn dep_valid(
    dep: &DepEventWire,
    anchor: &Anchor,
    fs: &dyn FsProvider,
    config: ResolveConfig<'_>,
) -> bool {
    match dep {
        DepEventWire::NearestPackage { from, found, hash } => {
            let Some(from) = from_wire_path(anchor, from) else {
                return false;
            };
            let expected = match found {
                None => None,
                Some((name, dir)) => match from_wire_path(anchor, dir) {
                    Some(dir) => Some((name.clone(), dir)),
                    None => return false,
                },
            };
            if fs.nearest_package(&from) != expected {
                return false;
            }
            let recomputed = expected
                .as_ref()
                .and_then(|(_, dir)| fs.hash_file(&dir.join("package.json")));
            recomputed == *hash
        }
        DepEventWire::ResolveImport {
            specifier,
            importer,
            resolved,
        } => {
            let Some(importer) = from_wire_path(anchor, importer) else {
                return false;
            };
            let expected = match resolved {
                None => None,
                Some(p) => match from_wire_path(anchor, p) {
                    Some(p) => Some(p),
                    None => return false,
                },
            };
            fs.resolve_import(specifier, &importer, config) == expected
        }
        DepEventWire::Exists { path, found } => {
            let Some(path) = from_wire_path(anchor, path) else {
                return false;
            };
            fs.exists(&path) == *found
        }
        DepEventWire::ExistsAny { path, found } => {
            let Some(path) = from_wire_path(anchor, path) else {
                return false;
            };
            fs.exists_any(&path) == *found
        }
    }
}

// ------------------------------------------------------------------- store

const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// One file per key under `dir`; atomic tmp+rename writes, lock-free.
pub struct CacheStore {
    dir: PathBuf,
    max_bytes: u64,
    written_since_gc: AtomicU64,
}

impl CacheStore {
    pub fn new(dir: &Path) -> std::io::Result<Self> {
        Self::with_max_bytes(dir, DEFAULT_MAX_BYTES)
    }

    pub fn with_max_bytes(dir: &Path, max_bytes: u64) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            max_bytes,
            written_since_gc: AtomicU64::new(0),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn entry_path(&self, key: &CacheKey) -> PathBuf {
        self.dir.join(key.hex())
    }

    /// A hit only when the entry parses, matches the compiler fingerprint,
    /// and every logged fs observation replays identically against `fs`.
    pub fn get_validated(
        &self,
        key: &CacheKey,
        current_root: &Path,
        fs: &dyn FsProvider,
        config: ResolveConfig<'_>,
    ) -> Option<SourceCompileResult> {
        let path = self.entry_path(key);
        let bytes = {
            let _t = crate::timings::start(crate::timings::Stage::CacheRead);
            std::fs::read(&path).ok()?
        };
        let entry = {
            let _t = crate::timings::start(crate::timings::Stage::CacheParse);
            serde_json::from_slice::<EntryWire>(&bytes)
        };
        let Ok(entry) = entry else {
            let _ = std::fs::remove_file(&path);
            return None;
        };
        if entry.schema != CACHE_SCHEMA_VERSION || entry.fingerprint != compiler_fingerprint() {
            return None;
        }
        {
            let _t = crate::timings::start(crate::timings::Stage::CacheReplay);
            if !deps_valid(&entry, current_root, fs, config) {
                return None;
            }
        }
        let _t = crate::timings::start(crate::timings::Stage::CacheDecode);
        Some(decode_result(entry.result))
    }

    pub fn put(
        &self,
        key: &CacheKey,
        result: &SourceCompileResult,
        deps: &DepLog,
    ) -> std::io::Result<()> {
        let entry = EntryWire {
            schema: CACHE_SCHEMA_VERSION,
            fingerprint: compiler_fingerprint().to_string(),
            root: deps.root.to_string_lossy().into_owned(),
            deps: deps.events.iter().map(|e| to_wire_event(deps, e)).collect(),
            result: encode_result(result),
        };
        let bytes = serde_json::to_vec(&entry).map_err(std::io::Error::other)?;
        static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
        let tmp = self.dir.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, self.entry_path(key))?;
        let written = self
            .written_since_gc
            .fetch_add(bytes.len() as u64, Ordering::Relaxed)
            + bytes.len() as u64;
        if written > self.max_bytes / 8 {
            self.written_since_gc.store(0, Ordering::Relaxed);
            self.gc()?;
        }
        Ok(())
    }

    /// Deletes oldest entries beyond the byte cap (and stale tmp files);
    /// returns bytes freed. GC only causes misses, never wrong output.
    pub fn gc(&self) -> std::io::Result<u64> {
        let mut entries: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
        let mut total: u64 = 0;
        for dirent in std::fs::read_dir(&self.dir)? {
            let dirent = dirent?;
            let path = dirent.path();
            let Ok(meta) = dirent.metadata() else {
                continue;
            };
            let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            let name = dirent.file_name();
            let is_tmp = name.to_string_lossy().starts_with(".tmp-");
            if is_tmp {
                let stale = modified
                    .elapsed()
                    .is_ok_and(|age| age > std::time::Duration::from_secs(3600));
                if stale {
                    let _ = std::fs::remove_file(&path);
                }
                continue;
            }
            total += meta.len();
            entries.push((path, meta.len(), modified));
        }
        if total <= self.max_bytes {
            return Ok(0);
        }
        entries.sort_by(|a, b| (a.2, &a.0).cmp(&(b.2, &b.0)));
        let mut freed = 0;
        for (path, len, _) in entries {
            if total - freed <= self.max_bytes {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                freed += len;
            }
        }
        Ok(freed)
    }
}

// ------------------------------------------------------------- verify mode

#[derive(Clone, Debug, PartialEq)]
pub struct VerifyMismatch {
    pub field: &'static str,
    pub cached: String,
    pub fresh: String,
}

#[derive(Debug, PartialEq)]
pub enum CacheStatus {
    Hit,
    Miss,
    /// The pre-gate skipped the file; nothing to cache.
    Gated,
    /// Verify mode found a stale/corrupt hit; the fresh result was returned
    /// and the entry repaired.
    Poisoned(VerifyMismatch),
}

fn snippet(text: &str) -> String {
    let mut s: String = text.chars().take(160).collect();
    if s.len() < text.len() {
        s.push('…');
    }
    s
}

/// Byte-exact comparison via the lossless wire encoding of each field.
pub fn compare_results(
    cached: &SourceCompileResult,
    fresh: &SourceCompileResult,
) -> Result<(), VerifyMismatch> {
    let mismatch = |field, cached: &str, fresh: &str| VerifyMismatch {
        field,
        cached: snippet(cached),
        fresh: snippet(fresh),
    };
    if cached.code != fresh.code {
        return Err(mismatch("code", &cached.code, &fresh.code));
    }
    if cached.map != fresh.map {
        return Err(mismatch(
            "map",
            &format!("{:?}", cached.map),
            &format!("{:?}", fresh.map),
        ));
    }
    if cached.modified != fresh.modified {
        return Err(mismatch(
            "modified",
            &cached.modified.to_string(),
            &fresh.modified.to_string(),
        ));
    }
    if cached.rules != fresh.rules {
        return Err(mismatch(
            "rules",
            &format!("{:?}", cached.rules),
            &format!("{:?}", fresh.rules),
        ));
    }
    let encode = |result: &SourceCompileResult| {
        serde_json::to_string(
            &result
                .create_objects
                .iter()
                .map(|(name, obj)| (name.clone(), encode_object(obj)))
                .collect::<Vec<_>>(),
        )
        .unwrap_or_default()
    };
    let (cached_objs, fresh_objs) = (encode(cached), encode(fresh));
    if cached_objs != fresh_objs {
        return Err(mismatch("create_objects", &cached_objs, &fresh_objs));
    }
    Ok(())
}

/// Cache-fronted [`crate::api::transform_source`]. With `verify` it compiles
/// even on a hit, byte-compares, and repairs + reports a poisoned entry.
pub fn compile_through_cache(
    store: &CacheStore,
    ctx: &FileContext<'_>,
    options: &ResolvedOptions,
    fs: &dyn FsProvider,
    verify: bool,
) -> Result<(Option<SourceCompileResult>, CacheStatus), StylexError> {
    let allocator = oxc_allocator::Allocator::default();
    compile_through_cache_in(&allocator, store, ctx, options, fs, verify)
}

/// [`compile_through_cache`] compiling misses into the caller's arena (see
/// [`crate::api::transform_source_in`]); at most one compile lands per call.
pub fn compile_through_cache_in(
    allocator: &oxc_allocator::Allocator,
    store: &CacheStore,
    ctx: &FileContext<'_>,
    options: &ResolvedOptions,
    fs: &dyn FsProvider,
    verify: bool,
) -> Result<(Option<SourceCompileResult>, CacheStatus), StylexError> {
    if !might_contain_stylex(ctx.source_text, options) {
        return Ok((None, CacheStatus::Gated));
    }
    let key = cache_key(
        ctx.source_text,
        &filename_key(ctx.cwd, ctx.filename),
        options,
    );
    // The key hashes every ResolvedOptions field, so an entry under it was
    // recorded with this resolver config; replaying under it is sound.
    let hit = store.get_validated(&key, ctx.cwd, fs, ResolveConfig::of(options));
    if let Some(hit) = hit {
        if !verify {
            return Ok((Some(hit), CacheStatus::Hit));
        }
        let (fresh, log) = transform_source_with_dep_log_in(allocator, ctx, options, fs)?;
        // The gate is pure on (source, options): an entry under this key
        // proves the gate passed, so fresh is always Some here.
        let Some(fresh) = fresh else {
            return Ok((None, CacheStatus::Gated));
        };
        return Ok(match compare_results(&hit, &fresh) {
            Ok(()) => (Some(fresh), CacheStatus::Hit),
            Err(report) => {
                let _ = store.put(&key, &fresh, &log);
                (Some(fresh), CacheStatus::Poisoned(report))
            }
        });
    }
    let (fresh, log) = transform_source_with_dep_log_in(allocator, ctx, options, fs)?;
    let Some(fresh) = fresh else {
        return Ok((None, CacheStatus::Gated));
    };
    // A failed entry write must never fail the compile.
    let _ = store.put(&key, &fresh, &log);
    Ok((Some(fresh), CacheStatus::Miss))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::CompilerOptions;

    fn options(json: serde_json::Value) -> ResolvedOptions {
        CompilerOptions::from_json(&json)
            .expect("options parse")
            .resolve()
            .expect("options resolve")
    }

    #[test]
    fn key_is_sensitive_to_every_dimension() {
        let base_opts = options(serde_json::json!({}));
        let base = cache_key("src", "a/input.ts", &base_opts);
        assert_eq!(base, cache_key("src", "a/input.ts", &base_opts));
        assert_ne!(base, cache_key("src2", "a/input.ts", &base_opts));
        assert_ne!(base, cache_key("src", "a/other.ts", &base_opts));
        assert_ne!(
            base,
            cache_key(
                "src",
                "a/input.ts",
                &options(serde_json::json!({"dev": true}))
            )
        );
        assert_ne!(
            base,
            cache_key(
                "src",
                "a/input.ts",
                &options(serde_json::json!({"classNamePrefix": "y"}))
            )
        );
        assert_ne!(
            base,
            cache_key(
                "src",
                "a/input.ts",
                &options(serde_json::json!({"env": {"k": "red"}}))
            )
        );
        assert_ne!(
            cache_key(
                "src",
                "a/input.ts",
                &options(serde_json::json!({"env": {"k": "red"}}))
            ),
            cache_key(
                "src",
                "a/input.ts",
                &options(serde_json::json!({"env": {"k": "blue"}}))
            )
        );
        assert_ne!(
            base,
            cache_key_with_fingerprint("src", "a/input.ts", &base_opts, "other-version")
        );
        // Length-prefixed fields: shifting bytes across a boundary re-keys.
        assert_ne!(
            cache_key("ab", "c", &base_opts),
            cache_key("a", "bc", &base_opts)
        );
    }

    #[test]
    fn value_wire_roundtrip_is_lossless() {
        let mut inner = JsObjectMap::new();
        inner.insert("width", EvalValue::Str("1px".into()));
        inner.set_css_type("<length>".to_string());
        let mut obj = JsObjectMap::new();
        obj.insert("2", EvalValue::Num(-0.0));
        obj.insert("b", EvalValue::Num(f64::NAN));
        obj.insert("0", EvalValue::Num(f64::NEG_INFINITY));
        obj.insert("a", EvalValue::Undefined);
        obj.insert(
            "arr",
            EvalValue::Arr(vec![EvalValue::Null, EvalValue::Obj(inner.into())]),
        );
        let value = EvalValue::Obj(obj.into());
        let wire = encode_value(&value);
        let rewire = encode_value(&decode_value(wire.clone()));
        assert!(wire == rewire, "round-trip drifted");
        if let EvalValue::Obj(map) = decode_value(wire) {
            assert_eq!(
                map.keys().collect::<Vec<_>>(),
                vec!["0", "2", "b", "a", "arr"]
            );
        } else {
            panic!("expected object");
        }
    }

    #[test]
    fn puts_are_atomic_under_concurrent_writers() {
        let dir = std::env::temp_dir().join(format!("stylex-cache-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = CacheStore::new(&dir).expect("store");
        let opts = options(serde_json::json!({}));
        let key = cache_key("const a = 1;", "input.ts", &opts);
        let result = SourceCompileResult {
            code: "X".repeat(4096),
            map: None,
            rules: Vec::new(),
            modified: false,
            create_objects: Vec::new(),
        };
        let deps = DepLog::default();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..50 {
                        store.put(&key, &result, &deps).expect("put");
                        if let Some(read) = store.get_validated(
                            &key,
                            Path::new("/nonexistent"),
                            &NoFs,
                            ResolveConfig::default(),
                        ) {
                            assert_eq!(read.code, result.code, "torn read");
                        }
                    }
                });
            }
        });
        let read = store
            .get_validated(
                &key,
                Path::new("/nonexistent"),
                &NoFs,
                ResolveConfig::default(),
            )
            .expect("entry after writers");
        assert_eq!(read.code, result.code);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gc_deletes_oldest_beyond_cap() {
        let dir = std::env::temp_dir().join(format!("stylex-cache-gc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = CacheStore::with_max_bytes(&dir, 1).expect("store");
        let opts = options(serde_json::json!({}));
        let result = SourceCompileResult {
            code: "y".repeat(512),
            map: None,
            rules: Vec::new(),
            modified: false,
            create_objects: Vec::new(),
        };
        for i in 0..4 {
            let key = cache_key(&format!("src{i}"), "input.ts", &opts);
            store.put(&key, &result, &DepLog::default()).expect("put");
        }
        let remaining = std::fs::read_dir(&dir).expect("dir").count();
        assert!(remaining <= 1, "cap not enforced: {remaining} entries left");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Empty-log validation needs an FsProvider; no call should ever land.
    struct NoFs;
    impl FsProvider for NoFs {
        fn nearest_package(&self, _: &Path) -> Option<(String, PathBuf)> {
            unreachable!("empty dep log must not touch the fs")
        }
        fn resolve_import(&self, _: &str, _: &Path, _: ResolveConfig<'_>) -> Option<PathBuf> {
            unreachable!("empty dep log must not touch the fs")
        }
        fn exists(&self, _: &Path) -> bool {
            unreachable!("empty dep log must not touch the fs")
        }
    }
}
