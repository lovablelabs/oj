//! Local content-addressed compile cache (docs/design-cache.md): entries pair
//! the result with an fs-observation log, replayed to validate every hit.

mod codec;

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::api::{
    FileContext, SourceCompileResult, might_contain_stylex, transform_source_with_dep_log_in,
};
use crate::errors::StylexError;
use crate::module_resolution::{FsProvider, ResolveConfig, path_relative};
use crate::options::ResolvedOptions;
use crate::timings::{self, Stage};

use codec::{Corrupt, DepRecord, PathRef, Reader};

// v2: options key material moved to the precomputed ResolvedOptions::cache_repr.
// v3: entries are binary (`codec`; v2 was JSON), and the key hashes its
// run-constant fields first.
pub const CACHE_SCHEMA_VERSION: u32 = 3;

/// Bump on any behavior-affecting compiler change (the STYLEX_PASS_VERSION
/// pattern): the crate version alone does not move per commit.
pub const STYLEX_PASS_VERSION: &str = "0.19.2-rs.2";

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
    let mut hasher = options_prefix(options, fingerprint);
    update_field(&mut hasher, filename_key.as_bytes());
    update_field(&mut hasher, source.as_bytes());
    CacheKey(*hasher.finalize().as_bytes())
}

// Length-prefixed, so bytes cannot shift across a field boundary unnoticed.
fn update_field(hasher: &mut blake3::Hasher, field: &[u8]) {
    hasher.update(&(field.len() as u64).to_le_bytes());
    hasher.update(field);
}

/// The key's leading fields, the ones every file of a run shares.
fn options_prefix(options: &ResolvedOptions, fingerprint: &str) -> blake3::Hasher {
    let mut hasher = blake3::Hasher::new();
    update_field(&mut hasher, fingerprint.as_bytes());
    // The precomputed Debug capture keys every ResolvedOptions field, present
    // and future — no per-field listing to drift out of sync with options.rs.
    update_field(&mut hasher, options.cache_repr.as_bytes());
    hasher
}

pub fn filename_key(cwd: &Path, filename: &Path) -> String {
    path_relative(cwd, filename)
}

thread_local! {
    // The option set this thread keyed last, with the hasher that absorbed it.
    static KEY_PREFIX: RefCell<Option<(String, blake3::Hasher)>> = const { RefCell::new(None) };
}

/// [`cache_key`] of a compile: the same bytes hashed, without building the
/// filename key or rehashing the options the previous file had.
fn compile_key(ctx: &FileContext<'_>, options: &ResolvedOptions) -> CacheKey {
    let mut hasher = KEY_PREFIX.with(|memo| {
        let mut memo = memo.borrow_mut();
        match &*memo {
            Some((repr, hasher)) if *repr == options.cache_repr => hasher.clone(),
            _ => {
                let hasher = options_prefix(options, compiler_fingerprint());
                *memo = Some((options.cache_repr.clone(), hasher.clone()));
                hasher
            }
        }
    });
    update_filename_key(&mut hasher, ctx.cwd, ctx.filename);
    update_field(&mut hasher, ctx.source_text.as_bytes());
    CacheKey(*hasher.finalize().as_bytes())
}

/// Hashes [`filename_key`]'s answer as a field, streaming it when the file
/// sits plainly below `cwd`.
fn update_filename_key(hasher: &mut blake3::Hasher, cwd: &Path, filename: &Path) {
    let Some(below) = plain_suffix(cwd, filename) else {
        update_field(hasher, filename_key(cwd, filename).as_bytes());
        return;
    };
    let parts = || below.components().map(|part| part.as_os_str());
    let len = parts()
        .map(|part| part.len() + 1)
        .sum::<usize>()
        .saturating_sub(1);
    hasher.update(&(len as u64).to_le_bytes());
    for (i, part) in parts().enumerate() {
        if i > 0 {
            hasher.update(b"/");
        }
        hasher.update(part.as_encoded_bytes());
    }
}

/// `filename` below `cwd` with nothing for `path_relative` to fold or to
/// re-encode: its answer is then this remainder's components joined by `/`.
fn plain_suffix<'a>(cwd: &Path, filename: &'a Path) -> Option<&'a Path> {
    let below = filename.strip_prefix(cwd).ok()?;
    let folds = |part: Component<'_>| matches!(part, Component::CurDir | Component::ParentDir);
    if cwd.components().any(folds) {
        return None;
    }
    below
        .components()
        .all(|part| matches!(part, Component::Normal(name) if name.to_str().is_some()))
        .then_some(below)
}

// ------------------------------------------------------------ entry writing

/// A dep-log path in its stored spelling (see [`PathRef`]).
struct StoredPath<'a> {
    relative: bool,
    text: Cow<'a, str>,
}

impl<'a> StoredPath<'a> {
    fn of(log: &DepLog, path: &'a Path) -> Self {
        for root in [&log.root, &log.raw_root] {
            if let Ok(below) = path.strip_prefix(root) {
                let text = below.to_string_lossy();
                let text = if text.contains('\\') {
                    Cow::Owned(text.replace('\\', "/"))
                } else {
                    text
                };
                return StoredPath {
                    relative: true,
                    text,
                };
            }
        }
        StoredPath {
            relative: false,
            text: path.to_string_lossy(),
        }
    }

    fn as_ref(&self) -> PathRef<'_> {
        if self.relative {
            PathRef::Rel(&self.text)
        } else {
            PathRef::Abs(&self.text)
        }
    }
}

fn put_event(buf: &mut Vec<u8>, log: &DepLog, event: &DepEvent) {
    match event {
        DepEvent::NearestPackage {
            from,
            found,
            package_json_hash,
        } => {
            let from = StoredPath::of(log, from);
            let found = found
                .as_ref()
                .map(|(name, dir)| (name.as_str(), StoredPath::of(log, dir)));
            codec::put_dep(
                buf,
                &DepRecord::NearestPackage {
                    from: from.as_ref(),
                    found: found.as_ref().map(|(name, dir)| (*name, dir.as_ref())),
                    hash: package_json_hash.as_deref(),
                },
            );
        }
        DepEvent::ResolveImport {
            specifier,
            importer,
            resolved,
        } => {
            let importer = StoredPath::of(log, importer);
            let resolved = resolved.as_ref().map(|path| StoredPath::of(log, path));
            codec::put_dep(
                buf,
                &DepRecord::ResolveImport {
                    specifier,
                    importer: importer.as_ref(),
                    resolved: resolved.as_ref().map(StoredPath::as_ref),
                },
            );
        }
        DepEvent::Exists { path, found } => codec::put_dep(
            buf,
            &DepRecord::Exists {
                path: StoredPath::of(log, path).as_ref(),
                found: *found,
            },
        ),
        DepEvent::ExistsAny { path, found } => codec::put_dep(
            buf,
            &DepRecord::ExistsAny {
                path: StoredPath::of(log, path).as_ref(),
                found: *found,
            },
        ),
    }
}

/// The whole entry into `buf`; `false` when it is too large to store.
fn encode_entry(buf: &mut Vec<u8>, result: &SourceCompileResult, deps: &DepLog) -> bool {
    codec::begin_entry(
        buf,
        compiler_fingerprint(),
        &deps.root.to_string_lossy(),
        result.modified,
        result.map.is_some(),
    );
    codec::put_varint(buf, deps.events.len() as u64);
    for event in &deps.events {
        put_event(buf, deps, event);
    }
    codec::put_rules(buf, &result.rules);
    codec::put_output(buf, &result.code, result.map.as_deref());
    codec::put_objects(buf, &result.create_objects);
    codec::finish_entry(buf)
}

// ---------------------------------------------------------- hit validation

/// Rebuilds stored paths against the root a hit is validated from.
struct Anchor<'a> {
    current_root: &'a Path,
    same_root: bool,
}

impl Anchor<'_> {
    /// The stored path into `out`; `false` on an absolute path under a
    /// different root: such entries are root-pinned because relative
    /// derivations cannot reconstruct them.
    fn resolve(&self, stored: PathRef<'_>, out: &mut PathBuf) -> bool {
        out.clear();
        match stored {
            PathRef::Rel(below) => {
                out.push(self.current_root);
                out.push(below);
                true
            }
            PathRef::Abs(path) if self.same_root => {
                out.push(path);
                true
            }
            PathRef::Abs(_) => false,
        }
    }
}

/// Replays the dep log straight off the entry bytes; the first observation
/// that no longer holds ends it.
fn deps_hold(
    reader: &mut Reader<'_>,
    recorded_root: &str,
    current_root: &Path,
    fs: &dyn FsProvider,
    config: ResolveConfig<'_>,
) -> Result<bool, Corrupt> {
    let count = reader.dep_count()?;
    if count == 0 {
        return Ok(true);
    }
    let canonical = fs.canonicalize_root(current_root);
    let anchor = Anchor {
        same_root: canonical == Path::new(recorded_root),
        current_root: &canonical,
    };
    // Every event's paths are rebuilt in these two buffers.
    let (mut first, mut second) = (PathBuf::new(), PathBuf::new());
    for _ in 0..count {
        if !dep_holds(&reader.dep()?, &anchor, fs, config, &mut first, &mut second) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn dep_holds(
    dep: &DepRecord<'_>,
    anchor: &Anchor<'_>,
    fs: &dyn FsProvider,
    config: ResolveConfig<'_>,
    first: &mut PathBuf,
    second: &mut PathBuf,
) -> bool {
    match *dep {
        DepRecord::NearestPackage { from, found, hash } => {
            if !anchor.resolve(from, first) {
                return false;
            }
            let expected = match found {
                None => None,
                Some((name, dir)) => {
                    if !anchor.resolve(dir, second) {
                        return false;
                    }
                    Some(name)
                }
            };
            match (fs.nearest_package(first), expected) {
                (None, None) => hash.is_none(),
                (Some((name, dir)), Some(expected)) if name == expected && dir == *second => {
                    second.push("package.json");
                    fs.hash_file(second).as_deref() == hash
                }
                _ => false,
            }
        }
        DepRecord::ResolveImport {
            specifier,
            importer,
            resolved,
        } => {
            if !anchor.resolve(importer, first) {
                return false;
            }
            let expected = match resolved {
                None => None,
                Some(path) => {
                    if !anchor.resolve(path, second) {
                        return false;
                    }
                    Some(second.as_path())
                }
            };
            fs.resolve_import(specifier, first, config).as_deref() == expected
        }
        DepRecord::Exists { path, found } => {
            anchor.resolve(path, first) && fs.exists(first) == found
        }
        DepRecord::ExistsAny { path, found } => {
            anchor.resolve(path, first) && fs.exists_any(first) == found
        }
    }
}

/// How much of a hit comes back; a miss always returns the whole fresh result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HitParts {
    /// Every field.
    Full,
    /// `code`, `map`, `rules` and `modified`; `create_objects` stays empty.
    Output,
    /// `code`, `rules` and `modified`: the map stays `None` too, undecoded.
    Code,
    /// `rules` and `modified` only: `code` stays empty too.
    Rules,
}

/// `Ok(None)` is a clean miss (another compiler's entry, or a dep that moved).
fn decode_hit(
    entry: &[u8],
    current_root: &Path,
    fs: &dyn FsProvider,
    config: ResolveConfig<'_>,
    parts: HitParts,
) -> Result<Option<SourceCompileResult>, Corrupt> {
    let (mut reader, header) = {
        let _t = timings::start(Stage::CacheParse);
        let mut reader = Reader::open(entry)?;
        let header = reader.header()?;
        (reader, header)
    };
    if header.fingerprint != compiler_fingerprint() {
        return Ok(None);
    }
    {
        let _t = timings::start(Stage::CacheReplay);
        if !deps_hold(&mut reader, header.root, current_root, fs, config)? {
            return Ok(None);
        }
    }
    let _t = timings::start(Stage::CacheDecode);
    let mut result = SourceCompileResult {
        code: String::new(),
        map: None,
        rules: reader.rules()?,
        modified: header.modified,
        create_objects: Vec::new(),
    };
    if parts == HitParts::Rules {
        return Ok(Some(result));
    }
    result.code = reader.code()?;
    if parts == HitParts::Code {
        return Ok(Some(result));
    }
    if header.has_map {
        result.map = Some(reader.code()?);
    }
    if parts == HitParts::Output {
        return Ok(Some(result));
    }
    result.create_objects = reader.objects()?;
    if !reader.is_empty() {
        return Err(Corrupt);
    }
    Ok(Some(result))
}

// ------------------------------------------------------------------- store

const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// One read takes an entry up to this size whole; a larger one takes a second.
const READ_CHUNK: usize = 64 << 10;

/// A thread's scratch buffer that grew past this is dropped after the call
/// rather than pinned at its high-water mark.
const SCRATCH_KEEP: usize = 1 << 20;

thread_local! {
    // Entries are read into and encoded in per-thread buffers: a hit allocates
    // only what it returns. Taken out for the call, so a nested one still works.
    static READ_BUF: Cell<Vec<u8>> = const { Cell::new(Vec::new()) };
    static WRITE_BUF: Cell<Vec<u8>> = const { Cell::new(Vec::new()) };
}

fn with_scratch<T>(
    slot: &'static std::thread::LocalKey<Cell<Vec<u8>>>,
    run: impl FnOnce(&mut Vec<u8>) -> T,
) -> T {
    let mut buf = slot.take();
    let out = run(&mut buf);
    if buf.capacity() <= SCRATCH_KEEP {
        slot.set(buf);
    }
    out
}

/// Reads the entry at `path` into the front of `buf` and returns its length.
/// The length the entry declares ends the read, so the usual hit costs one
/// `read` with no probe for end-of-file; a file that disagrees with its own
/// prelude comes back as read and fails to decode.
fn read_entry(path: &Path, buf: &mut Vec<u8>) -> std::io::Result<usize> {
    let mut file = std::fs::File::open(path)?;
    if buf.len() < READ_CHUNK {
        buf.resize(READ_CHUNK, 0);
    }
    let mut filled = 0;
    loop {
        let read = match file.read(&mut buf[filled..]) {
            Ok(read) => read,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        filled += read;
        let Some(total) = codec::declared_len(&buf[..filled]) else {
            if read == 0 || filled >= codec::PRELUDE_LEN {
                return Ok(filled);
            }
            continue;
        };
        if total <= filled {
            return Ok(filled);
        }
        // The length is the file's own claim: the buffer grows to it only once
        // the file is seen to be that long.
        if file.metadata()?.len() != total as u64 {
            return Ok(filled);
        }
        if buf.len() < total {
            buf.resize(total, 0);
        }
        file.read_exact(&mut buf[filled..total])?;
        return Ok(total);
    }
}

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
        self.dir.join(blake3::Hash::from(key.0).to_hex().as_str())
    }

    /// A hit only when the entry decodes, matches the compiler fingerprint,
    /// and every logged fs observation replays identically against `fs`.
    pub fn get_validated(
        &self,
        key: &CacheKey,
        current_root: &Path,
        fs: &dyn FsProvider,
        config: ResolveConfig<'_>,
    ) -> Option<SourceCompileResult> {
        self.get_validated_parts(key, current_root, fs, config, HitParts::Full)
    }

    /// [`CacheStore::get_validated`] returning only `parts` of the entry: the
    /// rest is neither validated nor allocated.
    pub fn get_validated_parts(
        &self,
        key: &CacheKey,
        current_root: &Path,
        fs: &dyn FsProvider,
        config: ResolveConfig<'_>,
        parts: HitParts,
    ) -> Option<SourceCompileResult> {
        let path = self.entry_path(key);
        with_scratch(&READ_BUF, |buf| {
            let len = {
                let _t = timings::start(Stage::CacheRead);
                read_entry(&path, buf).ok()?
            };
            match decode_hit(&buf[..len], current_root, fs, config, parts) {
                Ok(hit) => hit,
                Err(Corrupt) => {
                    let _ = std::fs::remove_file(&path);
                    None
                }
            }
        })
    }

    pub fn put(
        &self,
        key: &CacheKey,
        result: &SourceCompileResult,
        deps: &DepLog,
    ) -> std::io::Result<()> {
        let written = with_scratch(&WRITE_BUF, |buf| {
            if !encode_entry(buf, result, deps) {
                return Err(std::io::Error::other("cache entry larger than 4 GiB"));
            }
            static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
            let tmp = self.dir.join(format!(
                ".tmp-{}-{}",
                std::process::id(),
                TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::write(&tmp, &buf[..])?;
            std::fs::rename(&tmp, self.entry_path(key))?;
            Ok(buf.len() as u64)
        })?;
        let since_gc = self.written_since_gc.fetch_add(written, Ordering::Relaxed) + written;
        if since_gc > self.max_bytes / 8 {
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

/// Byte-exact comparison: `create_objects` through its lossless entry encoding.
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
        let mut bytes = Vec::new();
        codec::put_objects(&mut bytes, &result.create_objects);
        bytes
    };
    if encode(cached) != encode(fresh) {
        return Err(mismatch(
            "create_objects",
            &format!("{:?}", cached.create_objects),
            &format!("{:?}", fresh.create_objects),
        ));
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
    compile_through_cache_parts_in(allocator, store, ctx, options, fs, verify, HitParts::Full)
}

/// [`compile_through_cache_in`] for a caller that reads only `parts` of a hit
/// (see [`HitParts`]); `verify` still compares the whole entry.
pub fn compile_through_cache_parts_in(
    allocator: &oxc_allocator::Allocator,
    store: &CacheStore,
    ctx: &FileContext<'_>,
    options: &ResolvedOptions,
    fs: &dyn FsProvider,
    verify: bool,
    parts: HitParts,
) -> Result<(Option<SourceCompileResult>, CacheStatus), StylexError> {
    if !might_contain_stylex(ctx.source_text, options) {
        return Ok((None, CacheStatus::Gated));
    }
    let key = compile_key(ctx, options);
    let parts = if verify { HitParts::Full } else { parts };
    // The key hashes every ResolvedOptions field, so an entry under it was
    // recorded with this resolver config; replaying under it is sound.
    let hit = store.get_validated_parts(&key, ctx.cwd, fs, ResolveConfig::of(options), parts);
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
    use std::sync::Arc;

    use super::*;
    use crate::eval::value::{EvalValue, JsObjectMap};
    use crate::options::CompilerOptions;
    use crate::rules::StylexRule;

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

    fn sample_object() -> JsObjectMap {
        let mut inner = JsObjectMap::new();
        inner.insert("width", EvalValue::Str("1px".into()));
        inner.set_css_type("<length>".to_string());
        let mut obj = JsObjectMap::new();
        obj.insert("2", EvalValue::Num(-0.0));
        obj.insert("b", EvalValue::Num(f64::from_bits(0x7ff8_0000_0000_beef)));
        obj.insert("0", EvalValue::Num(f64::NEG_INFINITY));
        obj.insert("a", EvalValue::Undefined);
        obj.insert("t", EvalValue::Bool(true));
        obj.insert(
            "arr",
            EvalValue::Arr(vec![EvalValue::Null, EvalValue::Obj(inner.into())]),
        );
        obj
    }

    #[test]
    fn value_encoding_roundtrip_is_lossless() {
        let value = EvalValue::Obj(sample_object().into());
        let mut bytes = Vec::new();
        codec::put_value(&mut bytes, &value);
        let mut reader = Reader::raw(&bytes);
        let decoded = reader.value(0).expect("decodes");
        assert!(reader.is_empty());
        let mut again = Vec::new();
        codec::put_value(&mut again, &decoded);
        assert!(bytes == again, "round-trip drifted");
        let EvalValue::Obj(map) = decoded else {
            panic!("expected object");
        };
        assert_eq!(
            map.keys().collect::<Vec<_>>(),
            vec!["0", "2", "b", "a", "t", "arr"]
        );
        // The NaN payload and the zero's sign survive: the bits are stored.
        let bits = |key: &str| match map.get(key) {
            Some(EvalValue::Num(n)) => n.to_bits(),
            other => panic!("{key}: {other:?}"),
        };
        assert_eq!(bits("b"), 0x7ff8_0000_0000_beef);
        assert_eq!(bits("2"), (-0.0f64).to_bits());
        let Some(EvalValue::Arr(items)) = map.get("arr") else {
            panic!("expected array");
        };
        let EvalValue::Obj(inner) = &items[1] else {
            panic!("expected nested object");
        };
        assert_eq!(inner.css_type(), Some("<length>"));
    }

    fn sample_result() -> SourceCompileResult {
        let rule = |class_name: &str, priority: f64| StylexRule {
            class_name: class_name.into(),
            ltr: format!(".{class_name}{{color:\"red\"}}\n").into(),
            rtl: None,
            const_key: None,
            const_val: None,
            priority,
        };
        SourceCompileResult {
            code: "const s = {a: \"x1\"};\n// \u{1F600} \\ done\n".to_string(),
            map: Some("{\"version\":3}".to_string()),
            rules: vec![
                rule("x1", 3000.0),
                StylexRule {
                    rtl: Some(".x2{margin-right:0}".into()),
                    ..rule("x2", 3000.5)
                },
                StylexRule {
                    const_key: Some("xk".into()),
                    const_val: Some(Box::new(serde_json::json!({
                        "z": [1, -2, 1.5, 1e300, u64::MAX, i64::MIN, null, true, "s"],
                        "a": { "__jsnum": "NaN" },
                    }))),
                    ..rule("xk", 0.0)
                },
            ],
            modified: true,
            create_objects: vec![
                (Some("styles".to_string()), Arc::new(sample_object())),
                (None, Arc::new(JsObjectMap::new())),
            ],
        }
    }

    fn sample_log() -> DepLog {
        DepLog {
            root: PathBuf::from("/work/app"),
            raw_root: PathBuf::from("/alias/app"),
            events: vec![
                DepEvent::NearestPackage {
                    from: PathBuf::from("/work/app/src/input.ts"),
                    found: Some(("app".to_string(), PathBuf::from("/work/app"))),
                    package_json_hash: Some("ab".repeat(32)),
                },
                DepEvent::NearestPackage {
                    from: PathBuf::from("/elsewhere/x.ts"),
                    found: None,
                    package_json_hash: None,
                },
                DepEvent::ResolveImport {
                    specifier: "./tokens.stylex".to_string(),
                    importer: PathBuf::from("/alias/app/src/input.ts"),
                    resolved: Some(PathBuf::from("/work/app/src/tokens.stylex.ts")),
                },
                DepEvent::ResolveImport {
                    specifier: "missing".to_string(),
                    importer: PathBuf::from("/work/app/src/input.ts"),
                    resolved: None,
                },
                DepEvent::Exists {
                    path: PathBuf::from("/work/app/src/tokens.stylex.js"),
                    found: false,
                },
                DepEvent::ExistsAny {
                    path: PathBuf::from("/outside/lib"),
                    found: true,
                },
            ],
        }
    }

    /// Answers every probe the way [`sample_log`] recorded it from `/work/app`.
    struct SampleFs;
    impl FsProvider for SampleFs {
        fn nearest_package(&self, from: &Path) -> Option<(String, PathBuf)> {
            (from == Path::new("/work/app/src/input.ts"))
                .then(|| ("app".to_string(), PathBuf::from("/work/app")))
        }
        fn resolve_import(
            &self,
            specifier: &str,
            _: &Path,
            _: ResolveConfig<'_>,
        ) -> Option<PathBuf> {
            (specifier == "./tokens.stylex")
                .then(|| PathBuf::from("/work/app/src/tokens.stylex.ts"))
        }
        fn exists(&self, _: &Path) -> bool {
            false
        }
        fn exists_any(&self, path: &Path) -> bool {
            path == Path::new("/outside/lib")
        }
        fn hash_file(&self, path: &Path) -> Option<String> {
            (path == Path::new("/work/app/package.json")).then(|| "ab".repeat(32))
        }
        fn canonicalize_root(&self, root: &Path) -> PathBuf {
            root.to_path_buf()
        }
    }

    fn sample_entry() -> Vec<u8> {
        let mut entry = Vec::new();
        assert!(encode_entry(&mut entry, &sample_result(), &sample_log()));
        entry
    }

    fn decode(entry: &[u8], parts: HitParts) -> Result<Option<SourceCompileResult>, Corrupt> {
        decode_hit(
            entry,
            Path::new("/work/app"),
            &SampleFs,
            ResolveConfig::default(),
            parts,
        )
    }

    #[test]
    fn entry_roundtrip_is_lossless() {
        let entry = sample_entry();
        let hit = decode(&entry, HitParts::Full)
            .expect("decodes")
            .expect("deps replay");
        compare_results(&hit, &sample_result()).expect("identical");
        assert_eq!(hit.rules, sample_result().rules);
        assert_eq!(hit.create_objects[0].0.as_deref(), Some("styles"));

        // The stored paths: in-root ones relative to either root spelling,
        // the rest absolute.
        let mut reader = Reader::open(&entry).expect("prelude");
        let header = reader.header().expect("header");
        assert_eq!(header.root, "/work/app");
        let count = reader.dep_count().expect("count");
        let deps: Vec<DepRecord<'_>> = (0..count).map(|_| reader.dep().expect("dep")).collect();
        assert_eq!(
            deps[0],
            DepRecord::NearestPackage {
                from: PathRef::Rel("src/input.ts"),
                found: Some(("app", PathRef::Rel(""))),
                hash: Some(&"ab".repeat(32)),
            }
        );
        assert_eq!(
            deps[1],
            DepRecord::NearestPackage {
                from: PathRef::Abs("/elsewhere/x.ts"),
                found: None,
                hash: None,
            }
        );
        assert_eq!(
            deps[2],
            DepRecord::ResolveImport {
                specifier: "./tokens.stylex",
                importer: PathRef::Rel("src/input.ts"),
                resolved: Some(PathRef::Rel("src/tokens.stylex.ts")),
            }
        );
        assert_eq!(
            deps[5],
            DepRecord::ExistsAny {
                path: PathRef::Abs("/outside/lib"),
                found: true,
            }
        );
    }

    #[test]
    fn partial_hits_return_only_what_was_asked() {
        let entry = sample_entry();
        let full = sample_result();
        let rules = decode(&entry, HitParts::Rules).unwrap().unwrap();
        assert_eq!(rules.rules, full.rules);
        assert!(rules.modified);
        assert!(rules.code.is_empty() && rules.map.is_none() && rules.create_objects.is_empty());
        let code = decode(&entry, HitParts::Code).unwrap().unwrap();
        assert_eq!(code.code, full.code);
        assert_eq!(code.rules, full.rules);
        assert!(code.modified);
        assert!(full.map.is_some() && code.map.is_none() && code.create_objects.is_empty());
        let output = decode(&entry, HitParts::Output).unwrap().unwrap();
        assert_eq!(output.code, full.code);
        assert_eq!(output.map, full.map);
        assert_eq!(output.rules, full.rules);
        assert!(output.create_objects.is_empty());
    }

    #[test]
    fn a_moved_dep_or_a_foreign_root_misses_cleanly() {
        let entry = sample_entry();
        // Root-relative deps follow the root; the absolute ones pin the entry.
        let moved = decode_hit(
            &entry,
            Path::new("/work/other"),
            &SampleFs,
            ResolveConfig::default(),
            HitParts::Full,
        );
        assert!(matches!(moved, Ok(None)), "foreign root must miss");

        struct Moved;
        impl FsProvider for Moved {
            fn nearest_package(&self, from: &Path) -> Option<(String, PathBuf)> {
                SampleFs.nearest_package(from)
            }
            fn resolve_import(&self, _: &str, _: &Path, _: ResolveConfig<'_>) -> Option<PathBuf> {
                Some(PathBuf::from("/work/app/src/tokens.stylex.tsx"))
            }
            fn exists(&self, _: &Path) -> bool {
                false
            }
            fn hash_file(&self, path: &Path) -> Option<String> {
                SampleFs.hash_file(path)
            }
            fn canonicalize_root(&self, root: &Path) -> PathBuf {
                root.to_path_buf()
            }
        }
        let moved = decode_hit(
            &entry,
            Path::new("/work/app"),
            &Moved,
            ResolveConfig::default(),
            HitParts::Full,
        );
        assert!(matches!(moved, Ok(None)), "re-resolved import must miss");
    }

    #[test]
    fn truncated_and_damaged_entries_are_corrupt_never_a_panic() {
        let entry = sample_entry();
        for cut in 0..entry.len() {
            // As the file would be after a short write…
            assert!(decode(&entry[..cut], HitParts::Full).is_err(), "cut {cut}");
            // …and with a prelude that vouches for the shorter length.
            let mut short = entry[..cut].to_vec();
            if cut >= codec::PRELUDE_LEN {
                short[4..8].copy_from_slice(&(cut as u32).to_le_bytes());
                assert!(
                    decode(&short, HitParts::Full).is_err(),
                    "resealed cut {cut}"
                );
                // A partial read stops before the damage or reports it.
                let _ = decode(&short, HitParts::Rules);
                let _ = decode(&short, HitParts::Code);
                let _ = decode(&short, HitParts::Output);
            }
        }
        let mut long = entry.clone();
        long.push(0);
        assert!(decode(&long, HitParts::Full).is_err(), "trailing byte");
        let resealed = (long.len() as u32).to_le_bytes();
        long[4..8].copy_from_slice(&resealed);
        assert!(
            decode(&long, HitParts::Full).is_err(),
            "resealed trailing byte"
        );

        // Any byte changed to any value decodes to something or to Corrupt.
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..20_000 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let at = (state >> 33) as usize % entry.len();
            let mut damaged = entry.clone();
            damaged[at] = (state >> 24) as u8;
            let _ = decode(&damaged, HitParts::Full);
        }
    }

    #[test]
    fn store_roundtrips_large_entries_and_drops_damaged_ones() {
        let dir = std::env::temp_dir().join(format!("stylex-cache-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = CacheStore::new(&dir).expect("store");
        let opts = options(serde_json::json!({}));
        let lookup = |key: &CacheKey, parts| {
            store.get_validated_parts(
                key,
                Path::new("/work/app"),
                &SampleFs,
                ResolveConfig::default(),
                parts,
            )
        };

        // Past the read chunk: the entry arrives in two reads.
        let big = SourceCompileResult {
            code: "const filler = 1;\n".repeat(READ_CHUNK / 8),
            ..sample_result()
        };
        let key = cache_key("big", "input.ts", &opts);
        store.put(&key, &big, &sample_log()).expect("put");
        for _ in 0..2 {
            let hit = lookup(&key, HitParts::Full).expect("large entry hits");
            compare_results(&hit, &big).expect("identical");
        }
        // A small entry read into the buffer the large one grew.
        let small_key = cache_key("small", "input.ts", &opts);
        store
            .put(&small_key, &sample_result(), &sample_log())
            .expect("put");
        let hit = lookup(&small_key, HitParts::Full).expect("small entry hits");
        compare_results(&hit, &sample_result()).expect("identical");
        assert!(lookup(&small_key, HitParts::Rules).is_some());

        // Damage is a miss that removes the file: garbage, a cut-off write, a
        // v2 JSON entry, an empty file.
        let path = store.entry_path(&small_key);
        let good = std::fs::read(&path).expect("entry bytes");
        for damaged in [
            b"not an entry".to_vec(),
            good[..good.len() - 1].to_vec(),
            br#"{"schema":2,"fingerprint":"x","root":"/","deps":[]}"#.to_vec(),
            Vec::new(),
        ] {
            std::fs::write(&path, &damaged).expect("write");
            assert!(lookup(&small_key, HitParts::Full).is_none());
            assert!(!path.exists(), "damaged entry must be removed");
        }
        // Another compiler's entry is left alone: its own key never names it.
        let mut foreign = Vec::new();
        codec::begin_entry(&mut foreign, "other-compiler", "/work/app", false, false);
        assert!(codec::finish_entry(&mut foreign));
        std::fs::write(&path, &foreign).expect("write");
        assert!(lookup(&small_key, HitParts::Full).is_none());
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compile_key_matches_the_public_key_for_every_path_shape() {
        let opts = options(serde_json::json!({ "dev": true }));
        let other = options(serde_json::json!({}));
        for (cwd, filename) in [
            ("/work/app", "/work/app/src/input.ts"),
            ("/work/app/", "/work/app/src//deep/./input.ts"),
            ("/work/app", "/work/app"),
            ("/work/app", "/work/app/src/../input.ts"),
            ("/work/app", "/work/other/input.ts"),
            ("/work/../work/app", "/work/../work/app/input.ts"),
            ("./app", "./app/input.ts"),
            ("app", "app/src/input.ts"),
            ("", "src/input.ts"),
            ("", "/abs/input.ts"),
            ("/", "/input.ts"),
        ] {
            // Alternating option sets also turns the per-thread prefix over.
            for options in [&opts, &other, &opts] {
                let ctx = FileContext {
                    filename: Path::new(filename),
                    source_text: "const a = 1;",
                    cwd: Path::new(cwd),
                };
                assert_eq!(
                    compile_key(&ctx, options),
                    cache_key(
                        ctx.source_text,
                        &filename_key(ctx.cwd, ctx.filename),
                        options
                    ),
                    "{cwd:?} {filename:?}"
                );
            }
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
