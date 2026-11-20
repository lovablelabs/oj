//! commonJS canonical file names, theme-file suffix checks, import resolution.
// parity: babel-plugin src/utils/state-manager.js:566-707 + file-based-identifier.js

use std::borrow::Cow;
use std::cell::RefCell;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::sync::{
    Arc, LazyLock, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard,
};
use std::time::{Duration, SystemTime};

use crate::fxhash::FxHashMap;
use crate::options::{AliasMap, ModuleResolutionType, ResolvedOptions};
use crate::timings::{self, Stage};

// parity: state-manager.js EXTENSIONS — probe order is observable via hashes.
pub const EXTENSIONS: [&str; 6] = [".js", ".ts", ".tsx", ".jsx", ".mjs", ".cjs"];

pub const THEME_FILE_EXTENSION: &str = ".stylex";

const ROOT_PLACEHOLDER: &str = "/ROOT/";

/// The two `filePathResolver` arguments beyond the specifier and the importer.
/// `rewriteAliases` passes `root_dir: None` — upstream omits the argument there.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResolveConfig<'a> {
    pub aliases: Option<&'a AliasMap>,
    pub root_dir: Option<&'a Path>,
}

impl<'a> ResolveConfig<'a> {
    pub fn of(options: &'a ResolvedOptions) -> Self {
        Self {
            aliases: options.aliases.as_ref(),
            root_dir: options
                .unstable_module_resolution
                .as_ref()
                .and_then(|m| m.root_dir.as_deref()),
        }
    }

    /// `rewriteAliases`' call shape: aliases, never a rootDir.
    pub fn aliases_only(options: &'a ResolvedOptions) -> Self {
        Self {
            aliases: options.aliases.as_ref(),
            root_dir: None,
        }
    }
}

/// Filesystem seam: tests confine walks to a fixture root; oj wraps its resolver.
pub trait FsProvider: Sync {
    /// Nearest package.json walking up from dirname(from) — a package.json in
    /// `from` itself is skipped. Unparseable package.json stops the walk (None).
    fn nearest_package(&self, from: &Path) -> Option<(String, PathBuf)>;
    fn resolve_import(
        &self,
        specifier: &str,
        importer: &Path,
        config: ResolveConfig<'_>,
    ) -> Option<PathBuf>;
    fn exists(&self, p: &Path) -> bool;
    /// `fs.existsSync`: unlike [`FsProvider::exists`] a directory counts, which
    /// is what the `/ROOT/` and absolute alias branches probe with.
    fn exists_any(&self, p: &Path) -> bool {
        p.exists()
    }
    /// blake3 hex of the file's bytes; `None` when unreadable. Cache dep
    /// recording/replay goes through this so one-shot drivers may memoize.
    fn hash_file(&self, p: &Path) -> Option<String> {
        std::fs::read(p)
            .ok()
            .map(|bytes| blake3::hash(&bytes).to_hex().to_string())
    }
    /// Canonical form of a compile root (unreadable roots pass through); cache
    /// recording/replay anchors relative entry paths to it.
    fn canonicalize_root(&self, root: &Path) -> PathBuf {
        std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
    }
}

/// What import resolution asks of the filesystem.
trait Probe {
    /// `moduleResolve`'s file check: a directory is no file.
    fn is_file(&self, p: &Path) -> bool;
    /// `fs.existsSync`: a directory counts.
    fn exists_any(&self, p: &Path) -> bool;
    fn is_dir(&self, p: &Path) -> bool;
    /// The path with its symlinks resolved; one that cannot be passes through.
    fn realpath(&self, p: PathBuf) -> PathBuf;
    /// The package.json in `dir`.
    fn package_json(&self, dir: &Path) -> PackageRead;

    // parity: getPackageScopeConfig — nearest package.json above `from`, never
    // crossing a node_modules boundary; a malformed read throws mid-walk.
    fn package_scope(&self, from: &Path) -> ScopeConfig {
        let mut dir = from.parent();
        while let Some(d) = dir {
            if let Some(scope) = scope_at(self, d) {
                return scope;
            }
            dir = d.parent();
        }
        ScopeConfig::NotFound
    }

    /// packageResolve's walk: the first `node_modules/<name>` directory in
    /// `dir` or above it.
    fn package_dir(&self, name: &str, mut dir: Option<&Path>) -> Option<PathBuf> {
        while let Some(d) = dir {
            if let Some(found) = package_at(self, name, d) {
                return Some(found);
            }
            dir = d.parent();
        }
        None
    }
}

/// Where [`Probe::package_scope`]'s walk ends in `dir`, if it does.
fn scope_at<P: Probe + ?Sized>(fs: &P, dir: &Path) -> Option<ScopeConfig> {
    if dir.file_name().is_some_and(|f| f == "node_modules") {
        return Some(ScopeConfig::NotFound);
    }
    match fs.package_json(dir) {
        PackageRead::Parsed(pkg) => Some(ScopeConfig::Found(dir.to_path_buf(), pkg)),
        PackageRead::Malformed => Some(ScopeConfig::Malformed),
        PackageRead::Missing => None,
    }
}

fn package_at<P: Probe + ?Sized>(fs: &P, name: &str, dir: &Path) -> Option<PathBuf> {
    let pkg_dir = dir.join("node_modules").join(name);
    fs.is_dir(&pkg_dir).then_some(pkg_dir)
}

/// [`Probe`] straight off the disk, behind a provider's own existence checks.
struct Direct<'a>(&'a dyn FsProvider);

impl Probe for Direct<'_> {
    fn is_file(&self, p: &Path) -> bool {
        self.0.exists(p)
    }

    fn exists_any(&self, p: &Path) -> bool {
        self.0.exists_any(p)
    }

    fn is_dir(&self, p: &Path) -> bool {
        p.is_dir()
    }

    fn realpath(&self, p: PathBuf) -> PathBuf {
        std::fs::canonicalize(&p).unwrap_or(p)
    }

    fn package_json(&self, dir: &Path) -> PackageRead {
        Manifest::read(&dir.join("package.json")).for_resolution()
    }
}

pub struct StdFs;

impl FsProvider for StdFs {
    fn nearest_package(&self, from: &Path) -> Option<(String, PathBuf)> {
        let _t = timings::start(Stage::Fs);
        nearest_package_walk(from, None)
    }

    fn resolve_import(
        &self,
        specifier: &str,
        importer: &Path,
        config: ResolveConfig<'_>,
    ) -> Option<PathBuf> {
        let _t = timings::start(Stage::Fs);
        resolve_import_with(&Direct(self), specifier, importer, config)
    }

    fn exists(&self, p: &Path) -> bool {
        p.is_file()
    }
}

/// Memo over [`StdFs`]. `snapshot()` freezes the filesystem for the process:
/// every path is statted, read and resolved once. `live()` keeps its answers
/// too, each with the stats it rests on, and repeats those stats before it
/// gives the answer again: once per path and [`MemoFs::session`]. It keeps
/// no more than [`LIVE_CACHE_CAP`] of them in any one map.
pub struct MemoFs {
    /// Live: how long a timestamp must have stood to vouch for what it stamps.
    settle: Option<Duration>,
    /// How many answers a map holds before it is emptied to make room; a
    /// snapshot, whose answers must not be worked out twice, has no such end.
    cap: usize,
    /// The walks up the tree, by every directory one has started in or
    /// passed: to the nearest package, to the importer's package scope, and
    /// (the key ending in a NUL and the package's name) to a package under
    /// `node_modules`.
    nearest: PathMap<Checked<Option<(String, PathBuf)>>>,
    scopes: PathMap<Checked<ScopeConfig>>,
    packages: PathMap<Checked<Option<PathBuf>>>,
    manifests: PathMap<(Node, Arc<Manifest>)>,
    /// Relative imports, by the path they name: see [`relative_target`].
    relative: PathMap<Checked<Option<PathBuf>>>,
    /// Every other import, by the importer's directory.
    resolve: RwLock<ResolveMemos>,
    /// Snapshot: the one stat a path gets.
    stats: PathMap<Stamp>,
    /// `None`: the path has no realpath.
    real: PathMap<Checked<Option<PathBuf>>>,
    hashes: PathMap<(Option<Node>, Option<String>)>,
}

// Keyed by the path's raw bytes: `Path` equality folds spellings (`a//b`,
// `a/b/`) whose walks return differently spelled directories.
type PathMap<V> = RwLock<FxHashMap<Box<[u8]>, V>>;

/// How many answers each map of a live fs holds. The fs of a dev server lives
/// as long as the server does and is asked about paths without end: files
/// come, go and are renamed, and every half-typed import is a specifier of its
/// own. A map that is full is emptied and fills again with what is still being
/// asked for. A pass over an app leaves a map an answer or two for each
/// directory that has modules in it, so only an app of thousands of such
/// directories fills one by itself; its later passes then work out again what
/// the map had no room to keep.
const LIVE_CACHE_CAP: usize = 1 << 14;

/// How long a live fs waits before it trusts a timestamp: a change made within
/// the timestamp's granularity of an earlier one leaves it where it was (the
/// window oj_cache's digest memo keeps, for the same reason).
const SETTLE: Duration = Duration::from_secs(2);

/// Whether a directory's stamp speaks for its entries. POSIX has every create,
/// unlink and rename mark the directory's mtime and ctime; elsewhere each
/// entry is statted for itself.
const DIRS_PIN_ENTRIES: bool = cfg!(unix);

/// What one `stat` (symlinks followed) saw at a path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stamp {
    /// Nothing there, or nothing above it (`ENOENT`, `ENOTDIR`).
    Missing,
    /// Any other failure: no permission, a symlink loop.
    Unreadable,
    Node(Node),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    File,
    Dir,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Node {
    kind: Kind,
    len: u64,
    modified: Option<SystemTime>,
    /// The status-change time (the modification time where there is none):
    /// it moves when a rewrite or a swapped file keeps size and mtime.
    changed: Option<SystemTime>,
    device: u64,
    inode: u64,
}

impl Stamp {
    fn of(p: &Path) -> Self {
        match std::fs::metadata(p) {
            Ok(meta) => Stamp::Node(Node::of(&meta)),
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
                Stamp::Missing
            }
            Err(_) => Stamp::Unreadable,
        }
    }

    fn kind(self) -> Option<Kind> {
        match self {
            Stamp::Node(node) => Some(node.kind),
            Stamp::Missing | Stamp::Unreadable => None,
        }
    }
}

/// A [`Stamp`], and whether the directory holding the path decides it.
#[derive(Clone, Copy)]
struct Sight {
    stamp: Stamp,
    /// The path's last name is no symlink: it is the entry its directory
    /// lists, or the directory lists none by that name.
    plain: bool,
}

impl Sight {
    /// One `lstat`, with a `stat` behind it only for a symlink.
    fn of(p: &Path) -> Self {
        // `lstat` follows a last name spelled `link/` or `link/.`.
        let names_entry = p
            .file_name()
            .is_some_and(|name| path_key(p).ends_with(name.as_encoded_bytes()));
        match std::fs::symlink_metadata(p) {
            Ok(meta) if meta.file_type().is_symlink() => Self {
                stamp: Stamp::of(p),
                plain: false,
            },
            Ok(meta) => Self {
                stamp: Stamp::Node(Node::of(&meta)),
                plain: names_entry,
            },
            Err(e) => match e.kind() {
                ErrorKind::NotFound => Self {
                    stamp: Stamp::Missing,
                    plain: names_entry,
                },
                ErrorKind::NotADirectory => Self {
                    stamp: Stamp::Missing,
                    plain: false,
                },
                _ => Self {
                    stamp: Stamp::Unreadable,
                    plain: false,
                },
            },
        }
    }
}

impl Node {
    fn of(meta: &std::fs::Metadata) -> Self {
        let modified = meta.modified().ok();
        #[cfg(unix)]
        let (changed, device, inode) = {
            use std::os::unix::fs::MetadataExt;
            let changed = u64::try_from(meta.ctime())
                .ok()
                .zip(u32::try_from(meta.ctime_nsec()).ok())
                .and_then(|(secs, nanos)| {
                    SystemTime::UNIX_EPOCH.checked_add(Duration::new(secs, nanos))
                });
            (changed, meta.dev(), meta.ino())
        };
        #[cfg(not(unix))]
        let (changed, device, inode) = (modified, 0, 0);
        Self {
            kind: if meta.is_file() {
                Kind::File
            } else if meta.is_dir() {
                Kind::Dir
            } else {
                Kind::Other
            },
            len: meta.len(),
            modified,
            changed,
            device,
            inode,
        }
    }

    /// When the node last took a timestamp; `None` when it reports none.
    fn stamped_at(&self) -> Option<SystemTime> {
        Some(self.modified?.max(self.changed?))
    }
}

/// A package.json read once for both of its readers: the walk to the nearest
/// package wants the name it hashes, Node's resolution the fields it routes by.
enum Manifest {
    Unreadable,
    /// Not JSON.
    Invalid,
    Parsed {
        hash_name: String,
        /// `None` for JSON `null`, which resolution takes as malformed.
        package: Option<Arc<PackageJson>>,
    },
}

impl Manifest {
    fn read(path: &Path) -> Self {
        let Ok(raw) = std::fs::read_to_string(path) else {
            return Manifest::Unreadable;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
            return Manifest::Invalid;
        };
        Manifest::Parsed {
            hash_name: js_name_string(json.get("name")),
            package: (!json.is_null()).then(|| Arc::new(PackageJson::of(&json))),
        }
    }

    /// `None` when the walk to the nearest package stops with no answer.
    fn hash_name(&self) -> Option<&str> {
        match self {
            Manifest::Parsed { hash_name, .. } => Some(hash_name),
            Manifest::Unreadable | Manifest::Invalid => None,
        }
    }

    fn for_resolution(&self) -> PackageRead {
        match self {
            Manifest::Unreadable => PackageRead::Missing,
            Manifest::Parsed {
                package: Some(package),
                ..
            } => PackageRead::Parsed(Arc::clone(package)),
            Manifest::Invalid | Manifest::Parsed { package: None, .. } => PackageRead::Malformed,
        }
    }
}

/// One observation a kept answer rests on, as a live fs repeats it.
enum Dep {
    /// `stat` of the path still reports the stamp.
    Stat(Box<Path>, Stamp),
    /// The path's realpath (`None`: it has none) is still this one.
    Real(Box<Path>, Option<Box<Path>>),
}

impl Dep {
    /// Whether both observe the same thing of the same spelling of a path.
    fn repeats(&self, other: &Dep) -> bool {
        match (self, other) {
            (Dep::Stat(a, _), Dep::Stat(b, _)) | (Dep::Real(a, _), Dep::Real(b, _)) => {
                path_key(a) == path_key(b)
            }
            _ => false,
        }
    }
}

#[derive(Default)]
struct Deps {
    list: Vec<Dep>,
    /// What the kept answers this one was worked out from rest on.
    inherited: Vec<Arc<Deps>>,
    /// Read from a file whose stamp had not settled: no stat can vouch for
    /// the answer, so it is not kept.
    volatile: bool,
    /// When the directories that had just changed will have settled. Until
    /// then the answer rests on each path it probed under them; after, it is
    /// worked out once more, to rest on those that did settle.
    resettle: Option<SystemTime>,
}

/// How a kept answer stands with a look at the disk.
enum Standing {
    Holds,
    /// Something it rests on is no longer so.
    Moved,
    /// It holds, and is due the one more working out of [`Deps::resettle`].
    Due,
}

impl Deps {
    fn push(&mut self, dep: Dep) {
        if !self.list.iter().any(|known| known.repeats(&dep)) {
            self.list.push(dep);
        }
    }

    fn wait_for(&mut self, dir: &Node, look: &Look) {
        if let Some(at) = dir.stamped_at().and_then(|at| at.checked_add(look.settle)) {
            self.wait_until(at);
        }
    }

    fn wait_until(&mut self, at: SystemTime) {
        self.resettle = Some(self.resettle.map_or(at, |known| known.max(at)));
    }

    /// Takes on what an answer this one was worked out from rests on.
    fn absorb(&mut self, other: &Arc<Deps>) {
        let rests = !(other.list.is_empty() && other.inherited.is_empty());
        if rests && !self.inherited.iter().any(|known| Arc::ptr_eq(known, other)) {
            self.inherited.push(Arc::clone(other));
        }
        self.volatile |= other.volatile;
        if let Some(at) = other.resettle {
            self.wait_until(at);
        }
    }
}

/// An answer and what it rests on (nothing, in a snapshot).
struct Checked<T> {
    value: T,
    deps: Arc<Deps>,
}

/// What every answer of a snapshot rests on.
static NOTHING: LazyLock<Arc<Deps>> = LazyLock::new(Arc::default);

struct ResolveMemo {
    aliases: Option<AliasMap>,
    root_dir: Option<PathBuf>,
    by_specifier: FxHashMap<String, Checked<Option<PathBuf>>>,
}

impl ResolveMemo {
    fn matches(&self, config: ResolveConfig<'_>) -> bool {
        self.aliases.as_ref() == config.aliases
            && self.root_dir.as_deref().map(Path::as_os_str) == config.root_dir.map(Path::as_os_str)
    }
}

#[derive(Default)]
struct ResolveMemos {
    by_dir: FxHashMap<Box<[u8]>, Vec<ResolveMemo>>,
    /// The specifiers kept, over every directory and config: what the cap of
    /// a live fs counts.
    kept: usize,
}

impl ResolveMemos {
    fn get(&self, dir: &[u8], config: ResolveConfig<'_>) -> Option<&ResolveMemo> {
        self.by_dir.get(dir)?.iter().find(|m| m.matches(config))
    }

    fn clear(&mut self) {
        self.by_dir.clear();
        self.kept = 0;
    }

    /// Keeps `answer`, in the place of the one kept before if there is one.
    fn keep(
        &mut self,
        dir: &[u8],
        config: ResolveConfig<'_>,
        specifier: &str,
        answer: Checked<Option<PathBuf>>,
    ) {
        let memos = self.by_dir.entry(dir.into()).or_default();
        let memo = match memos.iter().position(|m| m.matches(config)) {
            Some(i) => &mut memos[i],
            None => {
                memos.push(ResolveMemo {
                    aliases: config.aliases.cloned(),
                    root_dir: config.root_dir.map(Path::to_path_buf),
                    by_specifier: FxHashMap::default(),
                });
                memos.last_mut().expect("just pushed")
            }
        };
        if memo
            .by_specifier
            .insert(specifier.to_string(), answer)
            .is_none()
        {
            self.kept += 1;
        }
    }

    /// Drops the answer kept, and with the last one its config and directory.
    fn forget(&mut self, dir: &[u8], config: ResolveConfig<'_>, specifier: &str) {
        let Some(memos) = self.by_dir.get_mut(dir) else {
            return;
        };
        let Some(i) = memos.iter().position(|m| m.matches(config)) else {
            return;
        };
        if memos[i].by_specifier.remove(specifier).is_none() {
            return;
        }
        self.kept -= 1;
        if memos[i].by_specifier.is_empty() {
            memos.swap_remove(i);
            if memos.is_empty() {
                self.by_dir.remove(dir);
            }
        }
    }
}

fn path_key(p: &Path) -> &[u8] {
    p.as_os_str().as_encoded_bytes()
}

/// The directory holding `p`; none for a root or a bare name.
fn parent_dir(p: &Path) -> Option<&Path> {
    p.parent().filter(|dir| !dir.as_os_str().is_empty())
}

fn read_lock<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

fn write_lock<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

static SHARED: LazyLock<MemoFs> = LazyLock::new(MemoFs::live);

impl MemoFs {
    pub fn snapshot() -> Self {
        Self::settling(None)
    }

    pub fn live() -> Self {
        Self::settling(Some(SETTLE))
    }

    /// Process-wide live instance: an embedder swaps `&StdFs` for
    /// `MemoFs::shared()` at its call sites and nothing else changes.
    pub fn shared() -> &'static MemoFs {
        &SHARED
    }

    fn settling(settle: Option<Duration>) -> Self {
        Self {
            settle,
            cap: if settle.is_some() {
                LIVE_CACHE_CAP
            } else {
                usize::MAX
            },
            nearest: RwLock::default(),
            scopes: RwLock::default(),
            packages: RwLock::default(),
            manifests: RwLock::default(),
            relative: RwLock::default(),
            resolve: RwLock::default(),
            stats: RwLock::default(),
            real: RwLock::default(),
            hashes: RwLock::default(),
        }
    }

    /// One compile's view of the filesystem. On a live fs every path is
    /// statted at most once through it, so the compile reads one state of the
    /// disk and the answers it shares with other compiles are checked once;
    /// a call made on the `MemoFs` itself is a session of its own. A snapshot
    /// is the same through any session.
    pub fn session(&self) -> MemoSession<'_> {
        MemoSession {
            fs: self,
            look: self.look(),
        }
    }

    pub fn invalidate_all(&self) {
        write_lock(&self.nearest).clear();
        write_lock(&self.scopes).clear();
        write_lock(&self.packages).clear();
        write_lock(&self.manifests).clear();
        write_lock(&self.relative).clear();
        write_lock(&self.resolve).clear();
        write_lock(&self.stats).clear();
        write_lock(&self.real).clear();
        write_lock(&self.hashes).clear();
    }

    fn look(&self) -> Option<Look> {
        self.settle.map(|settle| Look {
            started: SystemTime::now(),
            settle,
            seen: Mutex::new(Seen {
                // Room for the directories above a module and its imports.
                stats: FxHashMap::with_capacity_and_hasher(32, Default::default()),
                real: FxHashMap::default(),
            }),
        })
    }

    fn memo_path<T: Clone>(&self, map: &PathMap<T>, p: &Path, compute: impl FnOnce() -> T) -> T {
        let key = path_key(p);
        if let Some(hit) = read_lock(map).get(key) {
            return hit.clone();
        }
        let value = compute();
        write_lock(map).insert(key.into(), value.clone());
        value
    }

    /// Keeps `value` under `key`; a map with no room for one key more is
    /// emptied first. What was dropped is worked out again when next asked
    /// for, as it would have been had it never been kept.
    fn keep<V>(&self, map: &mut FxHashMap<Box<[u8]>, V>, key: &[u8], value: V) {
        if map.len() >= self.cap && !map.contains_key(key) {
            map.clear();
        }
        map.insert(key.into(), value);
    }

    /// A call on the fs itself: a view over a look of its own.
    fn alone<T>(&self, call: impl FnOnce(View<'_>) -> T) -> T {
        let look = self.look();
        call(View {
            fs: self,
            look: look.as_ref(),
        })
    }
}

/// See [`MemoFs::session`].
pub struct MemoSession<'a> {
    fs: &'a MemoFs,
    look: Option<Look>,
}

/// One pass over a live filesystem: what it has statted stays as it saw it.
struct Look {
    /// Taken before the first stat: a stamp is settled against this moment,
    /// which no change the look could have missed comes before.
    started: SystemTime,
    settle: Duration,
    seen: Mutex<Seen>,
}

#[derive(Default)]
struct Seen {
    stats: FxHashMap<Box<[u8]>, Sight>,
    real: FxHashMap<Box<[u8]>, Option<PathBuf>>,
}

impl Look {
    /// Whether any later change to the node is sure to move its stamp.
    fn settled(&self, node: &Node) -> bool {
        node.stamped_at()
            .and_then(|at| at.checked_add(self.settle))
            .is_some_and(|at| at <= self.started)
    }
}

/// The filesystem as one reader has it: frozen for the process in a snapshot,
/// for one [`Look`] when live.
#[derive(Clone, Copy)]
struct View<'a> {
    fs: &'a MemoFs,
    look: Option<&'a Look>,
}

impl View<'_> {
    fn stat(&self, p: &Path) -> Stamp {
        self.sight(p).stamp
    }

    fn sight(&self, p: &Path) -> Sight {
        let Some(look) = self.look else {
            // A snapshot rests on nothing: the stamp is all it wants.
            return Sight {
                stamp: self.fs.memo_path(&self.fs.stats, p, || Stamp::of(p)),
                plain: false,
            };
        };
        let key = path_key(p);
        let mut seen = lock(&look.seen);
        if let Some(hit) = seen.stats.get(key) {
            return *hit;
        }
        let sight = Sight::of(p);
        seen.stats.insert(key.into(), sight);
        sight
    }

    /// `realpath(p)` as this look finds it; a snapshot keeps it in
    /// [`MemoFs::real`] alone.
    fn real(&self, p: &Path) -> Option<PathBuf> {
        let canonicalize = || std::fs::canonicalize(p).ok();
        let Some(look) = self.look else {
            return canonicalize();
        };
        let key = path_key(p);
        let mut seen = lock(&look.seen);
        if let Some(hit) = seen.real.get(key) {
            return hit.clone();
        }
        let real = canonicalize();
        seen.real.insert(key.into(), real.clone());
        real
    }

    /// The parse of the package.json at `path`, a file stamped `node`.
    fn manifest(&self, path: &Path, node: Node) -> Arc<Manifest> {
        let key = path_key(path);
        if let Some((seen, manifest)) = read_lock(&self.fs.manifests).get(key)
            && (self.look.is_none() || *seen == node)
        {
            return Arc::clone(manifest);
        }
        let manifest = Arc::new(Manifest::read(path));
        // The stat came before the read, so the stamp is no newer than the
        // bytes; once settled, an edit after either moves it.
        if self.look.is_none_or(|look| look.settled(&node)) {
            let mut manifests = write_lock(&self.fs.manifests);
            self.fs
                .keep(&mut manifests, key, (node, Arc::clone(&manifest)));
        }
        manifest
    }

    fn standing(&self, deps: &Deps) -> Standing {
        let Some(look) = self.look else {
            return Standing::Holds;
        };
        if !self.holds(deps) {
            Standing::Moved
        } else if deps.resettle.is_some_and(|at| at <= look.started) {
            Standing::Due
        } else {
            Standing::Holds
        }
    }

    /// Whether a live fs finds everything `deps` lists and inherits as it was.
    fn holds(&self, deps: &Deps) -> bool {
        let repeats = |dep: &Dep| match dep {
            Dep::Stat(path, stamp) => self.stat(path) == *stamp,
            Dep::Real(path, real) => self.real(path).as_deref() == real.as_deref(),
        };
        deps.list.iter().all(repeats) && deps.inherited.iter().all(|deps| self.holds(deps))
    }

    /// The answer `map` keeps under `key` if it stands, else `work`'s, kept
    /// in its place; `rests_on` is shown what the answer returned rests on.
    fn kept<T: Clone>(
        &self,
        map: &PathMap<Checked<T>>,
        key: &[u8],
        work: impl FnOnce(&Tracker<'_>) -> T,
        rests_on: impl FnOnce(&Arc<Deps>),
    ) -> T {
        let mut standing = Standing::Moved;
        if let Some(hit) = read_lock(map).get(key) {
            standing = self.standing(&hit.deps);
            if let Standing::Holds = standing {
                rests_on(&hit.deps);
                return hit.value.clone();
            }
        }
        let tracker = Tracker::new(*self);
        let value = work(&tracker);
        let deps = tracker.finish(&standing);
        rests_on(&deps);
        let mut map = write_lock(map);
        if deps.volatile {
            map.remove(key);
        } else {
            let kept = Checked {
                value: value.clone(),
                deps,
            };
            self.fs.keep(&mut map, key, kept);
        }
        value
    }

    /// A walk up from `dir` to the first directory `at` has an answer in
    /// (`top`, past the last one), kept for every directory on the way: the
    /// next walk to come through any of them ends there. `salt` tells apart
    /// the walks that share a map.
    fn climb<T: Clone>(
        &self,
        map: &PathMap<Checked<T>>,
        dir: &Path,
        salt: &[u8],
        at: &dyn Fn(&Tracker<'_>, &Path) -> Option<T>,
        top: &T,
        rests_on: &dyn Fn(&Arc<Deps>),
    ) -> T {
        let from_here = |tracker: &Tracker<'_>| match (at(tracker, dir), dir.parent()) {
            (Some(answer), _) => answer,
            (None, Some(parent)) => {
                let rests_on = |deps: &Arc<Deps>| tracker.deps.borrow_mut().absorb(deps);
                self.climb(map, parent, salt, at, top, &rests_on)
            }
            (None, None) => top.clone(),
        };
        if salt.is_empty() {
            return self.kept(map, path_key(dir), from_here, rests_on);
        }
        let key = [path_key(dir), b"\0", salt].concat();
        self.kept(map, &key, from_here, rests_on)
    }

    // parity: state-manager.js getPackageNameAndPath, from the directory the
    // recursion first looks in.
    fn nearest_package(&self, from: &Path) -> Option<(String, PathBuf)> {
        let _t = timings::start(Stage::Fs);
        let at = |tracker: &Tracker<'_>, folder: &Path| {
            let manifest = tracker.manifest_at(&folder.join("package.json"))?;
            // A manifest that cannot be read ends the walk with no package.
            let name = manifest.hash_name().map(str::to_string);
            Some(name.map(|name| (name, folder.to_path_buf())))
        };
        self.climb(&self.fs.nearest, from.parent()?, b"", &at, &None, &|_| {})
    }

    fn resolve_import(
        &self,
        specifier: &str,
        importer: &Path,
        config: ResolveConfig<'_>,
    ) -> Option<PathBuf> {
        let _t = timings::start(Stage::Fs);
        let tracker = Tracker::new(*self);
        let Some(dir) = importer.parent() else {
            return resolve_import_with(&tracker, specifier, importer, config);
        };
        if let Some(target) = relative_target(dir, specifier) {
            let resolve =
                |tracker: &Tracker<'_>| resolve_import_with(tracker, specifier, importer, config);
            return self.kept(&self.fs.relative, &target, resolve, |_| {});
        }
        let key = path_key(dir);
        let mut standing = Standing::Moved;
        if let Some(memo) = read_lock(&self.fs.resolve).get(key, config)
            && let Some(hit) = memo.by_specifier.get(specifier)
        {
            standing = self.standing(&hit.deps);
            if let Standing::Holds = standing {
                return hit.value.clone();
            }
        }
        let resolved = resolve_import_with(&tracker, specifier, importer, config);
        let deps = tracker.finish(&standing);
        let mut memos = write_lock(&self.fs.resolve);
        if deps.volatile {
            memos.forget(key, config, specifier);
            return resolved;
        }
        let held = memos
            .get(key, config)
            .is_some_and(|memo| memo.by_specifier.contains_key(specifier));
        if memos.kept >= self.fs.cap && !held {
            memos.clear();
        }
        let kept = Checked {
            value: resolved.clone(),
            deps,
        };
        memos.keep(key, config, specifier, kept);
        resolved
    }

    fn exists(&self, p: &Path) -> bool {
        self.stat(p).kind() == Some(Kind::File)
    }

    fn exists_any(&self, p: &Path) -> bool {
        self.stat(p).kind().is_some()
    }

    fn hash_file(&self, p: &Path) -> Option<String> {
        let Some(look) = self.look else {
            let hash = || (None, StdFs.hash_file(p));
            return self.fs.memo_path(&self.fs.hashes, p, hash).1;
        };
        let node = match self.stat(p) {
            Stamp::Node(node) if node.kind == Kind::File => node,
            _ => return StdFs.hash_file(p),
        };
        let key = path_key(p);
        if let Some((seen, hash)) = read_lock(&self.fs.hashes).get(key)
            && *seen == Some(node)
        {
            return hash.clone();
        }
        let hash = StdFs.hash_file(p);
        if look.settled(&node) {
            let mut hashes = write_lock(&self.fs.hashes);
            self.fs.keep(&mut hashes, key, (Some(node), hash.clone()));
        }
        hash
    }

    fn canonicalize_root(&self, root: &Path) -> PathBuf {
        Tracker::new(*self).realpath(root.to_path_buf())
    }
}

impl FsProvider for MemoFs {
    fn nearest_package(&self, from: &Path) -> Option<(String, PathBuf)> {
        self.alone(|view| view.nearest_package(from))
    }

    fn resolve_import(
        &self,
        specifier: &str,
        importer: &Path,
        config: ResolveConfig<'_>,
    ) -> Option<PathBuf> {
        self.alone(|view| view.resolve_import(specifier, importer, config))
    }

    fn exists(&self, p: &Path) -> bool {
        self.alone(|view| view.exists(p))
    }

    fn exists_any(&self, p: &Path) -> bool {
        self.alone(|view| view.exists_any(p))
    }

    fn hash_file(&self, p: &Path) -> Option<String> {
        self.alone(|view| view.hash_file(p))
    }

    fn canonicalize_root(&self, root: &Path) -> PathBuf {
        self.alone(|view| view.canonicalize_root(root))
    }
}

impl MemoSession<'_> {
    fn view(&self) -> View<'_> {
        View {
            fs: self.fs,
            look: self.look.as_ref(),
        }
    }
}

impl FsProvider for MemoSession<'_> {
    fn nearest_package(&self, from: &Path) -> Option<(String, PathBuf)> {
        self.view().nearest_package(from)
    }

    fn resolve_import(
        &self,
        specifier: &str,
        importer: &Path,
        config: ResolveConfig<'_>,
    ) -> Option<PathBuf> {
        self.view().resolve_import(specifier, importer, config)
    }

    fn exists(&self, p: &Path) -> bool {
        self.view().exists(p)
    }

    fn exists_any(&self, p: &Path) -> bool {
        self.view().exists_any(p)
    }

    fn hash_file(&self, p: &Path) -> Option<String> {
        self.view().hash_file(p)
    }

    fn canonicalize_root(&self, root: &Path) -> PathBuf {
        self.view().canonicalize_root(root)
    }
}

/// A [`View`] that notes, on a live fs, what each thing it is asked rests on.
struct Tracker<'a> {
    view: View<'a>,
    deps: RefCell<Deps>,
}

impl<'a> Tracker<'a> {
    fn new(view: View<'a>) -> Self {
        Self {
            view,
            deps: RefCell::default(),
        }
    }

    /// What the answer worked out rests on. One that was [`Standing::Due`]
    /// its second working out is not due a third.
    fn finish(self, was: &Standing) -> Arc<Deps> {
        if self.view.look.is_none() {
            return Arc::clone(&NOTHING);
        }
        let mut deps = self.deps.into_inner();
        if let Standing::Due = was {
            deps.resettle = None;
        }
        Arc::new(deps)
    }

    /// `stat(p)`, noting what keeps the path's kind (or its absence) so.
    fn entry(&self, p: &Path) -> Stamp {
        let stamp = self.view.stat(p);
        self.note_entry(p, stamp);
        stamp
    }

    /// The parse of the package.json at `path`; `None` when no file is there.
    fn manifest_at(&self, path: &Path) -> Option<Arc<Manifest>> {
        match self.view.stat(path) {
            Stamp::Node(node) if node.kind == Kind::File => {
                if let Some(look) = self.view.look {
                    let mut deps = self.deps.borrow_mut();
                    deps.push(Dep::Stat(path.into(), Stamp::Node(node)));
                    deps.volatile |= !look.settled(&node);
                }
                Some(self.view.manifest(path, node))
            }
            stamp => {
                self.note_entry(path, stamp);
                None
            }
        }
    }

    /// Notes what keeps `stat(p)` at `stamp` as far as the kind of thing at
    /// `p` goes: the listing of the directory it is (or is not) an entry of,
    /// when that directory's stamp can be trusted, else the path's own stat.
    ///
    /// The directory is statted through every symlink its path crosses, so
    /// the stamp noted is that of the directory the name is really listed
    /// in: it moves when an entry comes or goes there, though the symlink and
    /// the directory holding it stay as they were, and a symlink pointed
    /// elsewhere leads to another inode.
    fn note_entry(&self, p: &Path, stamp: Stamp) {
        let Some(look) = self.view.look else {
            return;
        };
        // Missing along with the directories above it: the topmost missing
        // one stands for them all.
        let mut path = p;
        if stamp == Stamp::Missing {
            while let Some(parent) = parent_dir(path)
                && self.view.stat(parent) == Stamp::Missing
            {
                path = parent;
            }
        }
        let mut deps = self.deps.borrow_mut();
        if DIRS_PIN_ENTRIES
            && stamp != Stamp::Unreadable
            && let Some(parent) = parent_dir(path)
            && let Stamp::Node(dir) = self.view.stat(parent)
            && dir.kind == Kind::Dir
        {
            if !look.settled(&dir) {
                deps.wait_for(&dir, look);
            } else if self.view.sight(path).plain {
                deps.push(Dep::Stat(parent.into(), Stamp::Node(dir)));
                return;
            }
        }
        deps.push(Dep::Stat(path.into(), stamp));
    }

    /// Notes what keeps `p`'s realpath at `real`.
    fn note_real(&self, p: &Path, real: Option<&Path>) {
        let Some(look) = self.view.look else {
            return;
        };
        let mut deps = self.deps.borrow_mut();
        let own = real.is_some_and(|real| real.as_os_str() == p.as_os_str());
        if !(DIRS_PIN_ENTRIES && own) {
            deps.push(Dep::Real(p.into(), real.map(Into::into)));
            return;
        }
        // A path that is its own realpath crosses no symlink: each directory
        // above lists the next name as the real entry it is, and goes on
        // doing so until its stamp moves. Under a directory that cannot say,
        // what is left of the path is realpathed again.
        let mut path = p;
        while let Some(dir) = parent_dir(path) {
            match self.view.stat(dir) {
                Stamp::Node(node) if node.kind == Kind::Dir && look.settled(&node) => {
                    deps.push(Dep::Stat(dir.into(), Stamp::Node(node)));
                }
                stamp => {
                    if let Stamp::Node(node) = stamp {
                        deps.wait_for(&node, look);
                    }
                    deps.push(Dep::Real(path.into(), Some(path.into())));
                    return;
                }
            }
            path = dir;
        }
    }
}

impl Probe for Tracker<'_> {
    fn is_file(&self, p: &Path) -> bool {
        self.entry(p).kind() == Some(Kind::File)
    }

    fn exists_any(&self, p: &Path) -> bool {
        self.entry(p).kind().is_some()
    }

    fn is_dir(&self, p: &Path) -> bool {
        self.entry(p).kind() == Some(Kind::Dir)
    }

    fn realpath(&self, p: PathBuf) -> PathBuf {
        let view = self.view;
        let work = |tracker: &Tracker<'_>| {
            let real = view.real(&p);
            tracker.note_real(&p, real.as_deref());
            real
        };
        let rests_on = |deps: &Arc<Deps>| self.deps.borrow_mut().absorb(deps);
        let real = view.kept(&view.fs.real, path_key(&p), work, rests_on);
        real.unwrap_or(p)
    }

    fn package_json(&self, dir: &Path) -> PackageRead {
        let path = dir.join("package.json");
        if self.view.stat(&path).kind() == Some(Kind::Other) {
            // A pipe or a device reads as whatever it yields this time.
            self.deps.borrow_mut().volatile = true;
            return Manifest::read(&path).for_resolution();
        }
        match self.manifest_at(&path) {
            Some(manifest) => manifest.for_resolution(),
            None => PackageRead::Missing,
        }
    }

    fn package_scope(&self, from: &Path) -> ScopeConfig {
        let Some(dir) = from.parent() else {
            return ScopeConfig::NotFound;
        };
        let scopes = &self.view.fs.scopes;
        let rests_on = |deps: &Arc<Deps>| self.deps.borrow_mut().absorb(deps);
        let at = |level: &Tracker<'_>, dir: &Path| scope_at(level, dir);
        self.view
            .climb(scopes, dir, b"", &at, &ScopeConfig::NotFound, &rests_on)
    }

    fn package_dir(&self, name: &str, dir: Option<&Path>) -> Option<PathBuf> {
        let packages = &self.view.fs.packages;
        let rests_on = |deps: &Arc<Deps>| self.deps.borrow_mut().absorb(deps);
        let at = |level: &Tracker<'_>, dir: &Path| package_at(level, name, dir).map(Some);
        self.view
            .climb(packages, dir?, name.as_bytes(), &at, &None, &rests_on)
    }
}

/// The path a relative specifier names, when that path is all its resolution
/// turns on: every candidate `possible_file_paths` makes of the specifier then
/// joins to this path with its last name's extension swapped, wherever the
/// importer is and whatever the aliases are (a dot-leading specifier never
/// reaches them). `None` leaves the specifier to be kept by importer directory.
fn relative_target(dir: &Path, specifier: &str) -> Option<Vec<u8>> {
    let last = specifier.rsplit('/').next()?;
    // Escapes, backslashes, queries and fragments are rewritten on the way to
    // the filesystem, and a last segment that is no name moves the join.
    let plain = is_relative_specifier(specifier)
        && !specifier.contains(['%', '\\', '?', '#'])
        && !matches!(last, "" | "." | "..");
    // `normalize_path(dir)` must be `dir` itself.
    if !(plain && dir.is_absolute() && is_lexically_normal(dir)) {
        return None;
    }
    // `join_url_segments` over such a pair, without a `PathBuf` to re-parse
    // at every segment.
    let mut target = path_key(dir).to_vec();
    for segment in specifier.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                let parent = target.iter().rposition(|&b| b == b'/').unwrap_or(0);
                target.truncate(parent.max(1));
            }
            name => {
                if target.last() != Some(&b'/') {
                    target.push(b'/');
                }
                target.extend_from_slice(name.as_bytes());
            }
        }
    }
    Some(target)
}

/// `StdFs` whose package.json walk stops at `root`; pin fixtures use it so the
/// walk never escapes into the repository's own package.json files.
pub struct BoundedFs {
    pub root: PathBuf,
}

impl FsProvider for BoundedFs {
    fn nearest_package(&self, from: &Path) -> Option<(String, PathBuf)> {
        nearest_package_walk(from, Some(&self.root))
    }

    fn resolve_import(
        &self,
        specifier: &str,
        importer: &Path,
        config: ResolveConfig<'_>,
    ) -> Option<PathBuf> {
        resolve_import_with(&Direct(self), specifier, importer, config)
    }

    fn exists(&self, p: &Path) -> bool {
        p.is_file()
    }
}

// parity: state-manager.js getPackageNameAndPath (dirname-first recursion).
fn nearest_package_walk(from: &Path, stop_at: Option<&Path>) -> Option<(String, PathBuf)> {
    let mut folder = from.parent()?;
    loop {
        if stop_at.is_some_and(|stop| !folder.starts_with(stop)) {
            return None;
        }
        let candidate = folder.join("package.json");
        if candidate.is_file() {
            return Manifest::read(&candidate)
                .hash_name()
                .map(|name| (name.to_string(), folder.to_path_buf()));
        }
        if folder == Path::new("/") || folder.as_os_str().is_empty() {
            return None;
        }
        folder = folder.parent()?;
    }
}

// parity: JS template coercion of packageJson.name (missing name → "undefined").
fn js_name_string(name: Option<&serde_json::Value>) -> String {
    match name {
        None => "undefined".to_string(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Null) => "null".to_string(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        Some(serde_json::Value::Number(n)) => {
            crate::jsrt::js_number_to_string(n.as_f64().unwrap_or(f64::NAN))
        }
        Some(_) => "[object Object]".to_string(),
    }
}

// parity: state-manager.js getPossibleFilePaths — raw path first, then each
// extension appended to the path with any known code extension stripped.
pub fn possible_file_paths(file_path: &str) -> Vec<String> {
    file_path_candidates(file_path)
        .map(Cow::into_owned)
        .collect()
}

/// [`possible_file_paths`], each made as it is asked for: most resolutions
/// end within the first few.
fn file_path_candidates(file_path: &str) -> impl Iterator<Item = Cow<'_, str>> {
    let stripped = EXTENSIONS
        .iter()
        .find(|ext| file_path.ends_with(**ext))
        .map_or(file_path, |ext| &file_path[..file_path.len() - ext.len()]);
    let extended = EXTENSIONS
        .iter()
        .map(move |ext| Cow::Owned([stripped, ext].concat()));
    std::iter::once(Cow::Borrowed(file_path)).chain(extended)
}

// parity: state-manager.js possibleAliasedPaths — the raw specifier first, then
// every matching key's values in declaration order, each value array in order.
fn possible_aliased_paths(import_path: &str, aliases: Option<&AliasMap>) -> Vec<String> {
    let mut result = vec![import_path.to_string()];
    let Some(aliases) = aliases.filter(|a| !a.is_empty()) else {
        return result;
    };
    for (alias, values) in aliases {
        match alias.split_once('*') {
            // Only the first two split parts are read: a third `*` is ignored.
            Some((before, rest)) => {
                let after = rest.split('*').next().unwrap_or("");
                if !import_path.starts_with(before) || !import_path.ends_with(after) {
                    continue;
                }
                let end = import_path.len() - after.len();
                let capture = &import_path[before.len().min(end)..end];
                // Every `*` in the value takes the same capture.
                result.extend(values.iter().map(|v| v.replace('*', capture)));
            }
            None if alias == import_path => result.extend(values.iter().cloned()),
            None => {}
        }
    }
    result
}

fn resolve_import_with(
    fs: &dyn Probe,
    specifier: &str,
    importer: &Path,
    config: ResolveConfig<'_>,
) -> Option<PathBuf> {
    for candidate in file_path_candidates(specifier) {
        // A dot-leading specifier short-circuits before aliases are consulted.
        if candidate.starts_with('.') {
            if let Some(p) = module_resolve(fs, &candidate, importer) {
                return Some(p);
            }
            continue;
        }
        for possible in possible_aliased_paths(&candidate, config.aliases) {
            // Turbopack's placeholder, honored only when a rootDir is set.
            if let Some(rest) = possible.strip_prefix(ROOT_PLACEHOLDER)
                && let Some(root) = config.root_dir
            {
                let joined = node_path_join(root, rest);
                if let Some(p) = first_existing(fs, &joined) {
                    return Some(p);
                }
                continue;
            }
            if Path::new(&possible).is_absolute() {
                if let Some(p) = first_existing(fs, &possible) {
                    return Some(p);
                }
                continue;
            }
            if let Some(p) = module_resolve(fs, &possible, importer) {
                return Some(p);
            }
        }
    }
    None
}

/// One `moduleResolve` attempt. It realpaths its answer, which the existsSync
/// branches never do, so two spellings of one file can hash differently.
fn module_resolve(fs: &dyn Probe, specifier: &str, importer: &Path) -> Option<PathBuf> {
    let realpath = |p: PathBuf| fs.realpath(p);
    if is_relative_specifier(specifier) {
        let dir = importer.parent()?;
        let joined = join_url_segments(
            &normalize_path(dir),
            &url_path(strip_query_fragment(specifier)),
        );
        return finalize_file(fs, joined).map(realpath);
    }
    // Any other dot-leading form is an invalid package name — always an error.
    if specifier.starts_with('.') {
        return None;
    }
    if specifier.starts_with('#') {
        return package_imports_resolve(fs, specifier, importer).map(realpath);
    }
    package_resolve(fs, specifier, importer).ok().map(realpath)
}

/// Node `path.join` over two POSIX segments: concatenate, then normalize —
/// an absolute second segment appends rather than replacing the base.
fn node_path_join(base: &Path, rest: &str) -> String {
    let base = base.to_string_lossy();
    let joined = format!("{}/{rest}", base.trim_end_matches('/'));
    normalize_path(Path::new(&joined))
        .to_string_lossy()
        .into_owned()
}

/// The `fs.existsSync` sweep both non-moduleResolve branches share: the literal
/// path first, then each extension; a directory is a valid hit.
fn first_existing(fs: &dyn Probe, path: &str) -> Option<PathBuf> {
    file_path_candidates(path)
        .map(|candidate| PathBuf::from(candidate.into_owned()))
        .find(|p| fs.exists_any(p))
}

// Node ESM package resolution as executed by import-meta-resolve 4.2.1 (the
// oracle's resolver); every throw collapses to None per specifier candidate.

const CONDITIONS: [&str; 2] = ["node", "import"];

// parity: moduleResolve shouldBeTreatedAsRelativeOrAbsolutePath.
fn is_relative_specifier(s: &str) -> bool {
    s == "." || s == ".." || s.starts_with("./") || s.starts_with("../")
}

// URL joins turn `\` into `/` before file paths are resolved.
fn url_path(s: &str) -> String {
    s.replace('\\', "/")
}

// parity: URL parsing — the first `?` or `#` ends the pathname; query and
// fragment never reach the filesystem lookup.
fn strip_query_fragment(s: &str) -> &str {
    &s[..s.find(['?', '#']).unwrap_or(s.len())]
}

/// URL-reference join: empty and single-dot segments drop, double-dot pops
/// (clamped at the root). Percent-encoded dot variants count (WHATWG URL).
fn join_url_segments(base: &Path, rel: &str) -> PathBuf {
    let mut out = base.to_path_buf();
    for seg in rel.split('/') {
        if seg.is_empty() || matches_encoded(seg, ".") {
            continue;
        }
        if matches_encoded(seg, "..") {
            out.pop();
        } else {
            out.push(seg);
        }
    }
    out
}

// parity: finalizeResolution's encoded-separator rejection (`%2f`/`%5c`, /i).
fn has_encoded_separator(p: &Path) -> bool {
    let s = p.to_string_lossy();
    s.as_bytes().windows(3).any(|w| {
        w[0] == b'%'
            && ((w[1] == b'2' && (w[2] | 0x20) == b'f') || (w[1] == b'5' && (w[2] | 0x20) == b'c'))
    })
}

/// fileURLToPath's percent-decode of the pathname; `None` mirrors the
/// URIError on a malformed escape (decode-to-invalid-UTF-8 included).
fn percent_decode_path(p: &Path) -> Option<PathBuf> {
    let s = p.to_str()?;
    if !s.contains('%') {
        return Some(p.to_path_buf());
    }
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut bytes = Vec::with_capacity(s.len());
    let mut rest = s.as_bytes();
    while let Some((&b, tail)) = rest.split_first() {
        if b == b'%' {
            let [hi, lo, tail @ ..] = tail else {
                return None;
            };
            bytes.push(hex(*hi)? * 16 + hex(*lo)?);
            rest = tail;
        } else {
            bytes.push(b);
            rest = tail;
        }
    }
    Some(PathBuf::from(String::from_utf8(bytes).ok()?))
}

fn finalize_file(fs: &dyn Probe, p: PathBuf) -> Option<PathBuf> {
    if has_encoded_separator(&p) {
        return None;
    }
    let decoded = percent_decode_path(&p)?;
    fs.is_file(&decoded).then_some(decoded)
}

/// One expected char matches its literal (ASCII case-insensitive) or `%XX`
/// with hex decoding to either case, mirroring the upstream regex /i flag.
fn matches_encoded(segment: &str, word: &str) -> bool {
    let mut rest = segment.as_bytes();
    for &want in word.as_bytes() {
        let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
        rest = match rest {
            [b'%', hi, lo, tail @ ..]
                if hex(*hi)
                    .zip(hex(*lo))
                    .is_some_and(|(a, b)| (a * 16 + b).eq_ignore_ascii_case(&want)) =>
            {
                tail
            }
            [b, tail @ ..] if *b != b'%' && b.eq_ignore_ascii_case(&want) => tail,
            _ => return false,
        };
    }
    rest.is_empty()
}

// parity: deprecatedInvalidSegmentRegEx — a `.`/`..`/`node_modules` segment
// (encoded variants included) anywhere; these throw, empty segments only warn.
fn has_deprecated_segment(text: &str) -> bool {
    text.split(['/', '\\']).any(|seg| {
        matches_encoded(seg, ".")
            || matches_encoded(seg, "..")
            || matches_encoded(seg, "node_modules")
    })
}

// parity: isArrayIndex — canonical JS number round-trip within [0, 2^32-1).
fn is_js_array_index(key: &str) -> bool {
    key.parse::<f64>().is_ok_and(|n| {
        crate::jsrt::js_number_to_string(n) == key && (0.0..4_294_967_295.0).contains(&n)
    })
}

// parity: the `new URL(target)` probe — an ASCII-alpha scheme then `:`.
fn parses_as_url(s: &str) -> bool {
    let Some(colon) = s.find(':') else {
        return false;
    };
    let mut chars = s[..colon].chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

struct PackageJson {
    name: Option<String>,
    main: Option<String>,
    exports: Option<serde_json::Value>,
    imports: Option<serde_json::Value>,
}

/// `Malformed` (unparseable JSON, or JSON `null`) aborts the candidate —
/// unlike `Missing`, which downstream treats as an empty config.
enum PackageRead {
    Missing,
    Malformed,
    Parsed(Arc<PackageJson>),
}

impl PackageJson {
    fn of(json: &serde_json::Value) -> Self {
        let string_field = |key: &str| json.get(key).and_then(|v| v.as_str()).map(str::to_string);
        let value_field = |key: &str| json.get(key).filter(|v| !v.is_null()).cloned();
        Self {
            name: string_field("name"),
            main: string_field("main"),
            exports: value_field("exports"),
            imports: value_field("imports"),
        }
    }
}

fn parse_package_name(specifier: &str) -> Option<(&str, String)> {
    let mut separator = specifier.find('/');
    if specifier.starts_with('@') {
        let scope_sep = separator?;
        separator = specifier[scope_sep + 1..]
            .find('/')
            .map(|i| i + scope_sep + 1);
    }
    let name = separator.map_or(specifier, |i| &specifier[..i]);
    if name.is_empty() || name.starts_with('.') || name.contains('%') || name.contains('\\') {
        return None;
    }
    let subpath = format!(".{}", separator.map_or("", |i| &specifier[i..]));
    Some((name, subpath))
}

#[derive(Clone)]
enum ScopeConfig {
    Found(PathBuf, Arc<PackageJson>),
    NotFound,
    Malformed,
}

/// `Invalid` mirrors thrown ERR_INVALID_PACKAGE_TARGET (target arrays skip
/// past it); `Fatal` mirrors every other throw (they abort the candidate).
#[derive(Clone, Copy)]
enum ResolveErrKind {
    Invalid,
    Fatal,
}

fn package_resolve(
    fs: &dyn Probe,
    specifier: &str,
    importer: &Path,
) -> Result<PathBuf, ResolveErrKind> {
    let (name, subpath) = parse_package_name(specifier).ok_or(ResolveErrKind::Fatal)?;
    // ResolveSelf: the importer's own scope wins when named + exporting.
    match fs.package_scope(importer) {
        ScopeConfig::Malformed => return Err(ResolveErrKind::Fatal),
        ScopeConfig::Found(scope_dir, pkg) => {
            if pkg.name.as_deref() == Some(name)
                && let Some(exports) = &pkg.exports
            {
                return package_exports_resolve(fs, &scope_dir, &subpath, exports);
            }
        }
        ScopeConfig::NotFound => {}
    }
    let Some(pkg_dir) = fs.package_dir(name, importer.parent()) else {
        return Err(ResolveErrKind::Fatal);
    };
    let pkg = match fs.package_json(&pkg_dir) {
        PackageRead::Malformed => return Err(ResolveErrKind::Fatal),
        PackageRead::Parsed(pkg) => Some(pkg),
        PackageRead::Missing => None,
    };
    if let Some(exports) = pkg.as_ref().and_then(|p| p.exports.as_ref()) {
        return package_exports_resolve(fs, &pkg_dir, &subpath, exports);
    }
    if subpath == "." {
        return legacy_main_resolve(fs, &pkg_dir, pkg.and_then(|p| p.main.clone()))
            .ok_or(ResolveErrKind::Fatal);
    }
    let file = join_url_segments(&pkg_dir, &url_path(strip_query_fragment(&subpath[2..])));
    finalize_file(fs, file).ok_or(ResolveErrKind::Fatal)
}

// parity: legacyMainResolve — main, main+ext, main/index.*, then index.*.
fn legacy_main_resolve(fs: &dyn Probe, pkg_dir: &Path, main: Option<String>) -> Option<PathBuf> {
    let mut guesses: Vec<String> = Vec::new();
    if let Some(main) = &main {
        guesses.push(main.clone());
        for ext in [".js", ".json", ".node"] {
            guesses.push(format!("{main}{ext}"));
        }
        for ext in [".js", ".json", ".node"] {
            guesses.push(format!("{main}/index{ext}"));
        }
    }
    for ext in [".js", ".json", ".node"] {
        guesses.push(format!("index{ext}"));
    }
    guesses
        .iter()
        .map(|g| join_url_segments(pkg_dir, &url_path(strip_query_fragment(g))))
        .find_map(|p| finalize_file(fs, p))
}

/// resolvePackageTarget result algebra: `Null` mirrors JS null — target
/// arrays keep going, condition objects and top-level lookups stop on it.
enum TargetOutcome {
    Resolved(PathBuf),
    Null,
    Invalid,
    Fatal,
}

fn finish_exports_target(
    fs: &dyn Probe,
    outcome: TargetOutcome,
) -> Result<PathBuf, ResolveErrKind> {
    match outcome {
        // Existence is checked once at the end, never inside target arrays.
        TargetOutcome::Resolved(p) => finalize_file(fs, p).ok_or(ResolveErrKind::Fatal),
        TargetOutcome::Null | TargetOutcome::Fatal => Err(ResolveErrKind::Fatal),
        TargetOutcome::Invalid => Err(ResolveErrKind::Invalid),
    }
}

fn package_exports_resolve(
    fs: &dyn Probe,
    pkg_dir: &Path,
    subpath: &str,
    exports: &serde_json::Value,
) -> Result<PathBuf, ResolveErrKind> {
    // parity: isConditionalExportsMainSugar — string/array, or an object with
    // no dot-keys, stands for { ".": exports }; mixed keys are invalid.
    let sugar_map;
    let map: &serde_json::Map<String, serde_json::Value> = match exports {
        serde_json::Value::Object(map) => {
            let dotted = map.keys().filter(|k| k.starts_with('.')).count();
            if dotted == map.len() {
                map
            } else if dotted == 0 {
                sugar_map = serde_json::Map::from_iter([(".".to_string(), exports.clone())]);
                &sugar_map
            } else {
                return Err(ResolveErrKind::Fatal);
            }
        }
        serde_json::Value::String(_) | serde_json::Value::Array(_) => {
            sugar_map = serde_json::Map::from_iter([(".".to_string(), exports.clone())]);
            &sugar_map
        }
        // A boolean/number root is neither sugar nor a map: hasOwnProperty
        // and getOwnPropertyNames both miss — always PATH_NOT_EXPORTED.
        _ => return Err(ResolveErrKind::Fatal),
    };
    if let Some(target) = map.get(subpath)
        && !subpath.contains('*')
        && !subpath.ends_with('/')
    {
        let outcome = resolve_package_target(fs, pkg_dir, target, "", false, false);
        return finish_exports_target(fs, outcome);
    }
    let subpath_units: Vec<u16> = subpath.encode_utf16().collect();
    let mut best_match: Option<(&str, String)> = None;
    for key in map.keys() {
        let key_units: Vec<u16> = key.encode_utf16().collect();
        let Some(star) = key_units.iter().position(|&u| u == u16::from(b'*')) else {
            continue;
        };
        if key_units[star + 1..].contains(&u16::from(b'*')) {
            continue;
        }
        let (prefix, trailer) = (&key_units[..star], &key_units[star + 1..]);
        if subpath_units.starts_with(prefix)
            && subpath_units.len() >= key_units.len()
            && subpath_units.ends_with(trailer)
            && best_match
                .as_ref()
                .is_none_or(|(best, _)| pattern_key_compare(best, key) == 1)
            && let Ok(matched) =
                String::from_utf16(&subpath_units[star..subpath_units.len() - trailer.len()])
        {
            best_match = Some((key, matched));
        }
    }
    let (key, matched) = best_match.ok_or(ResolveErrKind::Fatal)?;
    let outcome = resolve_package_target(fs, pkg_dir, &map[key], &matched, true, false);
    finish_exports_target(fs, outcome)
}

// parity: patternKeyCompare — -1 when `a` sorts first (wins), 1 when `b`
// does; positions and lengths count UTF-16 units like JS string indexing.
fn pattern_key_compare(a: &str, b: &str) -> i32 {
    let a: Vec<u16> = a.encode_utf16().collect();
    let b: Vec<u16> = b.encode_utf16().collect();
    let a_star = a.iter().position(|&u| u == u16::from(b'*'));
    let b_star = b.iter().position(|&u| u == u16::from(b'*'));
    let base_a = a_star.map_or(a.len(), |i| i + 1);
    let base_b = b_star.map_or(b.len(), |i| i + 1);
    if base_a > base_b {
        return -1;
    }
    if base_b > base_a {
        return 1;
    }
    if a_star.is_none() {
        return 1;
    }
    if b_star.is_none() {
        return -1;
    }
    if a.len() > b.len() {
        return -1;
    }
    if b.len() > a.len() {
        return 1;
    }
    0
}

fn resolve_package_target(
    fs: &dyn Probe,
    pkg_dir: &Path,
    target: &serde_json::Value,
    matched: &str,
    pattern: bool,
    internal: bool,
) -> TargetOutcome {
    match target {
        serde_json::Value::String(target) => {
            resolve_target_string(fs, pkg_dir, target, matched, pattern, internal)
        }
        serde_json::Value::Array(items) => {
            if items.is_empty() {
                return TargetOutcome::Null;
            }
            let mut last_invalid = false;
            for item in items {
                match resolve_package_target(fs, pkg_dir, item, matched, pattern, internal) {
                    TargetOutcome::Null => last_invalid = false,
                    TargetOutcome::Invalid => last_invalid = true,
                    other => return other,
                }
            }
            if last_invalid {
                TargetOutcome::Invalid
            } else {
                TargetOutcome::Null
            }
        }
        serde_json::Value::Object(map) => {
            if map.keys().any(|k| is_js_array_index(k)) {
                return TargetOutcome::Fatal;
            }
            for (key, value) in map {
                if key == "default" || CONDITIONS.contains(&key.as_str()) {
                    return resolve_package_target(fs, pkg_dir, value, matched, pattern, internal);
                }
            }
            TargetOutcome::Null
        }
        serde_json::Value::Null => TargetOutcome::Null,
        _ => TargetOutcome::Invalid,
    }
}

fn resolve_target_string(
    fs: &dyn Probe,
    pkg_dir: &Path,
    target: &str,
    matched: &str,
    pattern: bool,
    internal: bool,
) -> TargetOutcome {
    if !matched.is_empty() && !pattern && !target.ends_with('/') {
        return TargetOutcome::Invalid;
    }
    if !target.starts_with("./") {
        // Internal (#imports) targets may re-enter bare package resolution,
        // but a target that parses as a URL is invalid outright.
        if internal && !target.starts_with("../") && !target.starts_with('/') {
            if parses_as_url(target) {
                return TargetOutcome::Invalid;
            }
            let expanded = if pattern {
                target.replace('*', matched)
            } else {
                format!("{target}{matched}")
            };
            return match package_resolve(fs, &expanded, &pkg_dir.join("package.json")) {
                Ok(p) => TargetOutcome::Resolved(p),
                Err(ResolveErrKind::Invalid) => TargetOutcome::Invalid,
                Err(ResolveErrKind::Fatal) => TargetOutcome::Fatal,
            };
        }
        return TargetOutcome::Invalid;
    }
    if has_deprecated_segment(&target[2..]) {
        return TargetOutcome::Invalid;
    }
    let resolved = join_url_segments(pkg_dir, &url_path(strip_query_fragment(&target[2..])));
    // Containment: checked before `*` substitution, like upstream.
    if !resolved.starts_with(normalize_path(pkg_dir)) {
        return TargetOutcome::Invalid;
    }
    if matched.is_empty() {
        return TargetOutcome::Resolved(resolved);
    }
    if has_deprecated_segment(matched) {
        // parity: throwInvalidSubpath — not an invalid-target, arrays abort.
        return TargetOutcome::Fatal;
    }
    let path = if pattern {
        // parity: `*` substitutes into the href, so a `?`/`#` inside the
        // matched subpath truncates the pathname there.
        let replaced = resolved.to_string_lossy().replace('*', &url_path(matched));
        normalize_path(Path::new(strip_query_fragment(&replaced)))
    } else {
        join_url_segments(&resolved, &url_path(strip_query_fragment(matched)))
    };
    TargetOutcome::Resolved(path)
}

// parity: PACKAGE_IMPORTS_RESOLVE over the importer scope's "imports" map.
fn package_imports_resolve(fs: &dyn Probe, specifier: &str, importer: &Path) -> Option<PathBuf> {
    if specifier == "#" || specifier.starts_with("#/") || specifier.ends_with('/') {
        return None;
    }
    let ScopeConfig::Found(scope_dir, pkg) = fs.package_scope(importer) else {
        return None;
    };
    let Some(serde_json::Value::Object(map)) = &pkg.imports else {
        return None;
    };
    if let Some(target) = map.get(specifier)
        && !specifier.contains('*')
    {
        return match resolve_package_target(fs, &scope_dir, target, "", false, true) {
            TargetOutcome::Resolved(p) => finalize_file(fs, p),
            _ => None,
        };
    }
    let specifier_units: Vec<u16> = specifier.encode_utf16().collect();
    let mut best_match: Option<(&str, String)> = None;
    for key in map.keys() {
        let key_units: Vec<u16> = key.encode_utf16().collect();
        let Some(star) = key_units.iter().position(|&u| u == u16::from(b'*')) else {
            continue;
        };
        if key_units[star + 1..].contains(&u16::from(b'*')) {
            continue;
        }
        // Oracle quirk (import-meta-resolve 4.2.1): the startsWith prefix is
        // the key minus its LAST UTF-16 unit, so only star-final keys match.
        let (prefix, trailer) = (&key_units[..key_units.len() - 1], &key_units[star + 1..]);
        if specifier_units.starts_with(prefix)
            && specifier_units.len() >= key_units.len()
            && specifier_units.ends_with(trailer)
            && best_match
                .as_ref()
                .is_none_or(|(best, _)| pattern_key_compare(best, key) == 1)
            && let Ok(matched) =
                String::from_utf16(&specifier_units[star..specifier_units.len() - trailer.len()])
        {
            best_match = Some((key, matched));
        }
    }
    let (key, matched) = best_match?;
    match resolve_package_target(fs, &scope_dir, &map[key].clone(), &matched, true, true) {
        TargetOutcome::Resolved(p) => finalize_file(fs, p),
        _ => None,
    }
}

// parity: state-manager.js matchesFileSuffix — bare suffix or suffix + code ext.
pub fn matches_file_suffix(suffix: &str, filename: &str) -> bool {
    if filename.ends_with(suffix) {
        return true;
    }
    EXTENSIONS.iter().any(|ext| {
        filename
            .strip_suffix(ext)
            .is_some_and(|stem| stem.ends_with(suffix))
    })
}

// `.transformed` is a literal upstream, never derived from the option.
pub fn is_theme_specifier(specifier: &str, theme_extension: &str) -> bool {
    matches_file_suffix(theme_extension, specifier)
        || matches_file_suffix(&format!("{theme_extension}.const"), specifier)
        || matches_file_suffix(".transformed", specifier)
}

// parity: state-manager.js getCanonicalFilePath.
pub fn canonical_file_path(fs: &dyn FsProvider, file: &Path, root_dir: Option<&Path>) -> String {
    if let Some((name, dir)) = fs.nearest_package(file) {
        return format!("{name}:{}", path_relative(&dir, file));
    }
    if let Some(root) = root_dir {
        return path_relative(root, file);
    }
    let basename = file
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("_unknown_path_:{basename}")
}

/// `importPathResolver`: `Some(canonical theme name)` mirrors
/// `['themeNameRef', …]`, `None` mirrors `false`.
pub fn import_path_resolver(
    fs: &dyn FsProvider,
    specifier: &str,
    source_file: Option<&Path>,
    options: &ResolvedOptions,
) -> Option<String> {
    let source_file = source_file?;
    let module_resolution = options.unstable_module_resolution.as_ref()?;
    if !is_theme_specifier(specifier, &module_resolution.theme_file_extension) {
        return None;
    }
    match module_resolution.kind {
        // Haste never touches the filesystem: the specifier is the module name.
        ModuleResolutionType::Haste => Some(add_file_extension(specifier, source_file)),
        ModuleResolutionType::CommonJs => {
            let resolved = fs.resolve_import(specifier, source_file, ResolveConfig::of(options))?;
            Some(canonical_file_path(
                fs,
                &resolved,
                module_resolution.root_dir.as_deref(),
            ))
        }
    }
}

/// `rewriteAliases`' new import source, or `None` to leave it alone. parity:
/// index.js Program.exit hardcodes `.stylex` and passes no rootDir.
pub fn rewritten_import_source(
    fs: &dyn FsProvider,
    source: &str,
    filename: &Path,
    options: &ResolvedOptions,
) -> Option<String> {
    if !matches_file_suffix(THEME_FILE_EXTENSION, source) {
        return None;
    }
    let resolved = fs.resolve_import(source, filename, ResolveConfig::aliases_only(options))?;
    let relative = get_relative_path(filename, &resolved);
    let stripped = EXTENSIONS
        .iter()
        .find(|ext| relative.ends_with(**ext))
        .map_or(relative.as_str(), |ext| {
            &relative[..relative.len() - ext.len()]
        });
    Some(stripped.to_string())
}

// parity: state-manager.js getRelativePath — posix, always `./`-prefixed.
pub fn get_relative_path(from: &Path, to: &Path) -> String {
    let dir = from.parent().unwrap_or(Path::new(""));
    let relative = path_relative(dir, to);
    if relative.starts_with('.') {
        relative
    } else {
        format!("./{relative}")
    }
}

// parity: state-manager.js addFileExtension — the extension is taken from the
// importing file, never validated, because haste cannot resolve the real one.
pub fn add_file_extension(imported: &str, source_file: &Path) -> String {
    if EXTENSIONS.iter().any(|ext| imported.ends_with(ext)) {
        return imported.to_string();
    }
    format!("{imported}{}", node_extname(&source_file.to_string_lossy()))
}

// parity: Node path.basename — trailing separators are ignored.
pub fn node_basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(idx) => &trimmed[idx + 1..],
        None => trimmed,
    }
}

// parity: Node path.extname's scan-from-the-end state machine, including its
// empty answers for a dot-only basename (".stylex", "..").
pub fn node_extname(path: &str) -> &str {
    let bytes = path.as_bytes();
    let (mut start_dot, mut start_part, mut end) = (usize::MAX, 0usize, usize::MAX);
    let mut matched_slash = true;
    let mut pre_dot_state = 0i32;
    for i in (0..bytes.len()).rev() {
        let code = bytes[i];
        if code == b'/' {
            if !matched_slash {
                start_part = i + 1;
                break;
            }
            continue;
        }
        if end == usize::MAX {
            matched_slash = false;
            end = i + 1;
        }
        if code == b'.' {
            if start_dot == usize::MAX {
                start_dot = i;
            } else if pre_dot_state != 1 {
                pre_dot_state = 1;
            }
        } else if start_dot != usize::MAX {
            pre_dot_state = -1;
        }
    }
    if start_dot == usize::MAX
        || end == usize::MAX
        || pre_dot_state == 0
        || (pre_dot_state == 1 && start_dot == end - 1 && start_dot == start_part + 1)
    {
        return "";
    }
    &path[start_dot..end]
}

// parity: shared/utils/file-based-identifier.js genFileBasedIdentifier.
pub fn gen_file_based_identifier(file_name: &str, export_name: &str, key: Option<&str>) -> String {
    match key {
        Some(key) => format!("{file_name}//{export_name}.{key}"),
        None => format!("{file_name}//{export_name}"),
    }
}

/// Node `path.relative` over absolute POSIX-style paths.
pub fn path_relative(from: &Path, to: &Path) -> String {
    let from = normalized(from);
    let to = normalized(to);
    // A file under the directory, neither path wanting normalizing: the
    // common case, and its answer is what is left of the file's path.
    if let Some(below) = path_key(&to).strip_prefix(path_key(&from))
        && let Some(below) = below.strip_prefix(b"/")
        && !below.is_empty()
        && matches!((&from, &to), (Cow::Borrowed(_), Cow::Borrowed(_)))
    {
        return String::from_utf8_lossy(below).into_owned();
    }
    let from_parts: Vec<&std::ffi::OsStr> = from
        .components()
        .filter_map(|c| match c {
            Component::Normal(p) => Some(p),
            _ => None,
        })
        .collect();
    let to_parts: Vec<&std::ffi::OsStr> = to
        .components()
        .filter_map(|c| match c {
            Component::Normal(p) => Some(p),
            _ => None,
        })
        .collect();
    let common = from_parts
        .iter()
        .zip(&to_parts)
        .take_while(|(a, b)| **a == **b)
        .count();
    let mut segments: Vec<String> = vec!["..".to_string(); from_parts.len() - common];
    segments.extend(
        to_parts[common..]
            .iter()
            .map(|p| p.to_string_lossy().into_owned()),
    );
    segments.join("/")
}

/// Lexical `.`/`..` folding (no symlink resolution, matching Node URL joins).
pub fn normalize_path(path: &Path) -> PathBuf {
    normalized(path).into_owned()
}

fn normalized(path: &Path) -> Cow<'_, Path> {
    if is_lexically_normal(path) {
        return Cow::Borrowed(path);
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    Cow::Owned(out)
}

/// Whether [`normalized`] would rebuild the path byte for byte: `/`-separated
/// names with no empty, `.` or `..` segment among them and no slash after the
/// last. Never so where a path can carry a prefix or another separator.
fn is_lexically_normal(path: &Path) -> bool {
    if !cfg!(unix) {
        return false;
    }
    let bytes = path_key(path);
    let names = bytes.strip_prefix(b"/").unwrap_or(bytes);
    bytes == b"/"
        || names
            .split(|&b| b == b'/')
            .all(|name| !matches!(name, b"" | b"." | b".."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_order_is_raw_then_each_extension() {
        assert_eq!(
            possible_file_paths("./a.stylex"),
            vec![
                "./a.stylex",
                "./a.stylex.js",
                "./a.stylex.ts",
                "./a.stylex.tsx",
                "./a.stylex.jsx",
                "./a.stylex.mjs",
                "./a.stylex.cjs",
            ]
        );
        // A known code extension is stripped before probing, raw stays first.
        assert_eq!(
            possible_file_paths("./a.stylex.js")[..3],
            ["./a.stylex.js", "./a.stylex.js", "./a.stylex.ts"].map(String::from)
        );
        assert_eq!(possible_file_paths("./a.stylex.cjs")[0], "./a.stylex.cjs");
        assert_eq!(possible_file_paths("./a.stylex.cjs")[1], "./a.stylex.js");
    }

    fn expansions(specifier: &str, aliases: &[(&str, &[&str])]) -> Vec<String> {
        let map: AliasMap = aliases
            .iter()
            .map(|(k, v)| {
                (
                    (*k).to_string(),
                    v.iter().map(|s| (*s).to_string()).collect(),
                )
            })
            .collect();
        possible_aliased_paths(specifier, Some(&map))
    }

    #[test]
    fn alias_expansion_order_and_capture() {
        // The raw specifier is always candidate #0; matches follow in
        // declaration order, each value array in its own order.
        assert_eq!(
            expansions(
                "@lib/c.stylex",
                &[("@lib/*", &["/a/*", "/b/*"]), ("@x", &["/z"])]
            ),
            ["@lib/c.stylex", "/a/c.stylex", "/b/c.stylex"]
        );
        // A star-free key is exact equality, not a prefix.
        assert_eq!(
            expansions("@lib/c.stylex", &[("@lib", &["/a"])]),
            ["@lib/c.stylex"]
        );
        // Suffix keys, a bare star, and the ignored third star.
        assert_eq!(
            expansions("~c.stylex", &[("~*.stylex", &["/a/*.stylex.ts"])])[1],
            "/a/c.stylex.ts"
        );
        assert_eq!(
            expansions("@x/c.stylex", &[("*", &["/a/*"])])[1],
            "/a/@x/c.stylex"
        );
        assert_eq!(
            expansions("@a/c.stylex", &[("@a/*.stylex*", &["/a/*.stylex.ts"])])[1],
            "/a/c.stylex.ts"
        );
        // Every star in the value takes the same capture; overlapping
        // before/after (JS slice with start > end) captures the empty string.
        assert_eq!(
            expansions("@d/d.stylex", &[("@d/*", &["/l/*/*"])])[1],
            "/l/d.stylex/d.stylex"
        );
        assert_eq!(
            expansions(
                "@lib/c.stylex",
                &[("@lib/c.stylex*c.stylex", &["/alt/*c.stylex"])]
            )[1],
            "/alt/c.stylex"
        );
        // An empty map short-circuits to the raw specifier alone.
        assert_eq!(expansions("@lib/c.stylex", &[]), ["@lib/c.stylex"]);
        assert_eq!(
            possible_aliased_paths("@lib/c.stylex", None),
            ["@lib/c.stylex"]
        );
    }

    #[test]
    fn relative_paths_for_rewritten_sources() {
        let rel = |from: &str, to: &str| get_relative_path(Path::new(from), Path::new(to));
        assert_eq!(
            rel("/r/src/input.ts", "/r/lib/c.stylex.ts"),
            "../lib/c.stylex.ts"
        );
        // A same-directory target still gains the `./` prefix.
        assert_eq!(
            rel("/r/src/input.ts", "/r/src/c.stylex.ts"),
            "./c.stylex.ts"
        );
        assert_eq!(rel("/r/src/input.ts", "/r/lib"), "../lib");
    }

    #[test]
    fn root_placeholder_joins_like_node() {
        assert_eq!(node_path_join(Path::new("/r"), "lib/c.ts"), "/r/lib/c.ts");
        // An absolute rest appends instead of replacing the base.
        assert_eq!(node_path_join(Path::new("/r"), "/lib/c.ts"), "/r/lib/c.ts");
        assert_eq!(
            node_path_join(Path::new("/r/"), "lib/deep/../c.ts"),
            "/r/lib/c.ts"
        );
    }

    #[test]
    fn suffix_matching() {
        assert!(matches_file_suffix(".stylex", "foo.stylex"));
        assert!(matches_file_suffix(".stylex", "foo.stylex.ts"));
        assert!(matches_file_suffix(".stylex", "a/b/foo.stylex.mjs"));
        assert!(!matches_file_suffix(".stylex", "foo.stylex.const.ts"));
        assert!(!matches_file_suffix(".stylex", "foostylex.ts"));
        assert!(matches_file_suffix(".stylex.const", "foo.stylex.const.ts"));
        assert!(is_theme_specifier("./x.stylex", THEME_FILE_EXTENSION));
        assert!(is_theme_specifier(
            "./x.stylex.const.js",
            THEME_FILE_EXTENSION
        ));
        assert!(is_theme_specifier(
            "./x.transformed.js",
            THEME_FILE_EXTENSION
        ));
        assert!(!is_theme_specifier("./helpers", THEME_FILE_EXTENSION));
        assert!(!is_theme_specifier("./helpers.ts", THEME_FILE_EXTENSION));
    }

    #[test]
    fn suffix_matching_under_a_configured_extension() {
        assert!(matches_file_suffix("cssvars", "src/defaultcssvars.js"));
        assert!(matches_file_suffix("", "src/whatever.ts"));
        assert!(matches_file_suffix("/vars", "src/theme/vars.ts"));
        assert!(!matches_file_suffix(".css", "src/tokens.css.mts"));
        assert!(!matches_file_suffix(".css", "src/tokens.CSS.ts"));
        assert!(!matches_file_suffix(".css", "src/tokensXcssY.ts"));
        assert!(is_theme_specifier("./tokens.css", ".css"));
        assert!(is_theme_specifier("./tokens.css.const", ".css"));
        // `.transformed` stays allowed, the default extension stops resolving.
        assert!(is_theme_specifier("./tokens.transformed", ".css"));
        assert!(!is_theme_specifier("./tokens.stylex", ".css"));
    }

    #[test]
    fn file_based_identifier() {
        assert_eq!(
            gen_file_based_identifier("pkg:src/t.stylex.ts", "colors", None),
            "pkg:src/t.stylex.ts//colors"
        );
        assert_eq!(
            gen_file_based_identifier("pkg:t.stylex.ts", "colors", Some("accent")),
            "pkg:t.stylex.ts//colors.accent"
        );
        assert_eq!(
            gen_file_based_identifier("f", "e", Some("button.primary")),
            "f//e.button.primary"
        );
    }

    fn node_fixture() -> PathBuf {
        static ONCE: std::sync::Once = std::sync::Once::new();
        let root = std::env::temp_dir().join("stylex-rs-node-resolution-fixture");
        ONCE.call_once(|| {
            let write = |rel: &str, content: &str| {
                let path = root.join(rel);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, content).unwrap();
            };
            write(
                "package.json",
                r##"{"name":"self-pkg","exports":{"./self.stylex":"./src/self.stylex.js"},"imports":{"#tok/*":"./src/hashed/*.js","#hash/*.stylex":"./src/hashed/*.stylex.js"}}"##,
            );
            write("src/self.stylex.js", "");
            write("src/hashed/deep.stylex.js", "");
            write(
                "node_modules/design-tokens/package.json",
                r#"{"name":"design-tokens","exports":{"./theme.stylex":"./out/theme.stylex.js","./pat/*.stylex":"./out/pat-*.stylex.js","./cond.stylex":{"browser":"./out/browser.js","node":"./out/node.js","default":"./out/default.js"},"./blocked.stylex":null}}"#,
            );
            write("node_modules/design-tokens/out/theme.stylex.js", "");
            write("node_modules/design-tokens/out/pat-deep.stylex.js", "");
            write("node_modules/design-tokens/out/node.js", "");
            write("node_modules/design-tokens/out/default.js", "");
            write("node_modules/plain-tokens/package.json", r#"{"name":"plain-tokens"}"#);
            write("node_modules/plain-tokens/lib/theme.stylex.js", "");
            write(
                "node_modules/tokens.stylex/package.json",
                r#"{"name":"tokens.stylex","main":"lib/entry"}"#,
            );
            write("node_modules/tokens.stylex/lib/entry.js", "");
        });
        // resolve_import realpaths results; macOS tempdirs live under a
        // /var → /private/var symlink, so compare against the real root.
        std::fs::canonicalize(&root).unwrap_or(root)
    }

    #[test]
    fn node_package_resolution() {
        let root = node_fixture();
        let importer = root.join("src/input.ts");
        let resolve = |spec: &str| {
            StdFs
                .resolve_import(spec, &importer, ResolveConfig::default())
                .map(|p| path_relative(&root, &p))
        };
        // exports: exact, pattern, conditions (node beats default), blocked.
        assert_eq!(
            resolve("design-tokens/theme.stylex").as_deref(),
            Some("node_modules/design-tokens/out/theme.stylex.js")
        );
        assert_eq!(
            resolve("design-tokens/pat/deep.stylex").as_deref(),
            Some("node_modules/design-tokens/out/pat-deep.stylex.js")
        );
        assert_eq!(
            resolve("design-tokens/cond.stylex").as_deref(),
            Some("node_modules/design-tokens/out/node.js")
        );
        assert_eq!(resolve("design-tokens/blocked.stylex"), None);
        assert_eq!(resolve("design-tokens/missing.stylex"), None);
        // no exports: URL-join subpath; bare root: legacy main resolution.
        assert_eq!(
            resolve("plain-tokens/lib/theme.stylex").as_deref(),
            Some("node_modules/plain-tokens/lib/theme.stylex.js")
        );
        assert_eq!(
            resolve("tokens.stylex").as_deref(),
            Some("node_modules/tokens.stylex/lib/entry.js")
        );
        // self-reference through the scope package's own exports.
        assert_eq!(
            resolve("self-pkg/self.stylex").as_deref(),
            Some("src/self.stylex.js")
        );
        // imports maps: star-final keys match; the oracle's non-final-star
        // quirk (prefix = key minus last char) keeps #hash/*.stylex dead.
        assert_eq!(
            resolve("#tok/deep.stylex").as_deref(),
            Some("src/hashed/deep.stylex.js")
        );
        assert_eq!(resolve("#hash/deep.stylex"), None);
    }

    /// Every expectation here was executed against import-meta-resolve 4.2.1
    /// (scratchpad imr-r4-probe.mjs, 2026-08-28) — codex r4 findings 2 + 11.
    fn hostile_fixture() -> PathBuf {
        static ONCE: std::sync::Once = std::sync::Once::new();
        let root = std::env::temp_dir().join("stylex-rs-node-resolution-hostile");
        ONCE.call_once(|| {
            let write = |rel: &str, content: &str| {
                let path = root.join(rel);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, content).unwrap();
            };
            write(
                "package.json",
                r##"{"name":"hostile-root","imports":{"#tok/*":"./src/hashed/*.js","#url":"https://example.com/x.js","#bare":"sealed/tokens/x.stylex","#barestar/*":"sealed/tokens/*.stylex","#arr/*":[null,"./src/hashed/*.js"],"#dead/*.stylex":"./src/hashed/*.stylex.js"}}"##,
            );
            write("src/input.ts", "");
            write("src/hashed/deep.stylex.js", "");
            write("src/hashed/x.js", "");
            write("src/esc.js", "");
            write("src/esc%2fx.stylex.js", "");
            write("src/.hidden.stylex.js", "");
            write(
                "node_modules/sealed/package.json",
                r#"{"name":"sealed","exports":{
                    "./tokens/*.stylex":"./public/*.stylex.js",
                    "./arrnull.stylex":[null,"./ok.stylex.js"],
                    "./arrbad.stylex":["bad-no-dot-slash","./ok.stylex.js"],
                    "./arrempty.stylex":[],
                    "./arrnested.stylex":[[],"./ok.stylex.js"],
                    "./arrmiss.stylex":["./gone.stylex.js","./ok.stylex.js"],
                    "./multistar/*.stylex":"./out/*-*.stylex.js",
                    "./nested.stylex":{"node":{"unknown":"./inner.stylex.js"},"default":"./default.stylex.js"},
                    "./nesteddef.stylex":{"node":{"unknown":"./inner.stylex.js","default":"./innerdef.stylex.js"},"default":"./default.stylex.js"},
                    "./num.stylex":{"0":"./zero.stylex.js","default":"./default.stylex.js"},
                    "./numarr.stylex":[{"0":"./zero.stylex.js"},"./ok.stylex.js"],
                    "./badtarget.stylex":"./../escape.stylex.js",
                    "./nmtarget.stylex":"./node_modules/inner.stylex.js",
                    "./dslash.stylex":".//ok.stylex.js",
                    "./dotseg.stylex":"./././ok.stylex.js",
                    "./urltarget.stylex":"file:///etc/passwd",
                    "./numtarget.stylex":[42,"./ok.stylex.js"]
                }}"#,
            );
            for f in [
                "public/x.stylex.js",
                "public/a%2fb.stylex.js",
                "public/a\\b.stylex.js",
                "public/a/b.stylex.js",
                "ok.stylex.js",
                "out/deep-deep.stylex.js",
                "default.stylex.js",
                "inner.stylex.js",
                "innerdef.stylex.js",
                "zero.stylex.js",
                "private.stylex.js",
                "node_modules/inner.stylex.js",
            ] {
                write(&format!("node_modules/sealed/{f}"), "");
            }
            write("node_modules/escape.stylex.js", "");
            write("node_modules/badjson/package.json", "{ nope");
            write("node_modules/badjson/lib/x.stylex.js", "");
            write("node_modules/badjson/index.js", "");
            write("node_modules/plain/lib/x.stylex.js", "");
            write("node_modules/plain/index.js", "");
            write("scoped/package.json", "{ also nope");
            write("scoped/input.ts", "");
        });
        std::fs::canonicalize(&root).unwrap_or(root)
    }

    #[test]
    fn exports_invalid_segments_reject() {
        let root = hostile_fixture();
        let importer = root.join("src/input.ts");
        let resolve = |spec: &str| {
            StdFs
                .resolve_import(spec, &importer, ResolveConfig::default())
                .map(|p| path_relative(&root, &p))
        };
        assert_eq!(
            resolve("sealed/tokens/x.stylex").as_deref(),
            Some("node_modules/sealed/public/x.stylex.js")
        );
        // `..`/`.`/node_modules subpath segments reject even when the
        // normalized file exists (finding 2's private-file escape).
        assert_eq!(resolve("sealed/tokens/../private.stylex"), None);
        assert_eq!(resolve("sealed/tokens/%2e%2e/private.stylex"), None);
        assert_eq!(resolve("sealed/tokens/%2E%2E/private.stylex"), None);
        assert_eq!(resolve("sealed/tokens/./x.stylex"), None);
        assert_eq!(resolve("sealed/tokens/node_modules/x.stylex"), None);
        assert_eq!(resolve("sealed/tokens/NoDe_MoDuLeS/x.stylex"), None);
        assert_eq!(resolve("sealed/tokens/%6eode_modules/x.stylex"), None);
        assert_eq!(resolve("sealed/tokens/..\\private.stylex"), None);
        // Encoded separators reject at finalize; the literal files exist.
        assert_eq!(resolve("sealed/tokens/a%2fb.stylex"), None);
        assert_eq!(resolve("sealed/tokens/a%5Cb.stylex"), None);
        assert_eq!(resolve("./esc%2fx.stylex"), None);
        // A raw backslash is a URL path separator, not a filename char.
        assert_eq!(
            resolve("sealed/tokens/a\\b.stylex").as_deref(),
            Some("node_modules/sealed/public/a/b.stylex.js")
        );
        // Target-side invalid segments reject; empty segments only warn.
        assert_eq!(resolve("sealed/badtarget.stylex"), None);
        assert_eq!(resolve("sealed/nmtarget.stylex"), None);
        assert_eq!(resolve("sealed/dotseg.stylex"), None);
        assert_eq!(
            resolve("sealed/dslash.stylex").as_deref(),
            Some("node_modules/sealed/ok.stylex.js")
        );
        assert_eq!(resolve("sealed/urltarget.stylex"), None);
        // No-exports packages take the raw URL join — `..` allowed there.
        assert_eq!(
            resolve("plain/lib/x.stylex").as_deref(),
            Some("node_modules/plain/lib/x.stylex.js")
        );
        assert_eq!(
            resolve("plain/lib/../lib/x.stylex").as_deref(),
            Some("node_modules/plain/lib/x.stylex.js")
        );
        assert_eq!(
            resolve("plain/lib/../../escape.stylex").as_deref(),
            Some("node_modules/escape.stylex.js")
        );
        // Dot-leading non-relative specifiers are invalid package names.
        assert_eq!(resolve(".hidden.stylex"), None);
    }

    #[test]
    fn exports_result_algebra() {
        let root = hostile_fixture();
        let importer = root.join("src/input.ts");
        let resolve = |spec: &str| {
            StdFs
                .resolve_import(spec, &importer, ResolveConfig::default())
                .map(|p| path_relative(&root, &p))
        };
        // Arrays: null and invalid targets fall through; fatal errors and
        // missing files do not; an exhausted/empty array is not exported.
        assert_eq!(
            resolve("sealed/arrnull.stylex").as_deref(),
            Some("node_modules/sealed/ok.stylex.js")
        );
        assert_eq!(
            resolve("sealed/arrbad.stylex").as_deref(),
            Some("node_modules/sealed/ok.stylex.js")
        );
        assert_eq!(
            resolve("sealed/arrnested.stylex").as_deref(),
            Some("node_modules/sealed/ok.stylex.js")
        );
        assert_eq!(
            resolve("sealed/numtarget.stylex").as_deref(),
            Some("node_modules/sealed/ok.stylex.js")
        );
        assert_eq!(resolve("sealed/arrempty.stylex"), None);
        assert_eq!(resolve("sealed/arrmiss.stylex"), None);
        // Multi-star targets replace every star with the matched subpath.
        assert_eq!(
            resolve("sealed/multistar/deep.stylex").as_deref(),
            Some("node_modules/sealed/out/deep-deep.stylex.js")
        );
        // A matched condition whose object has no active inner key yields
        // null and STOPS — the outer default is never consulted.
        assert_eq!(resolve("sealed/nested.stylex"), None);
        assert_eq!(
            resolve("sealed/nesteddef.stylex").as_deref(),
            Some("node_modules/sealed/innerdef.stylex.js")
        );
        // Numeric condition keys are invalid-package-config, even alongside
        // a default, and abort target arrays instead of falling through.
        assert_eq!(resolve("sealed/num.stylex"), None);
        assert_eq!(resolve("sealed/numarr.stylex"), None);
        // Malformed package.json is fatal, not absent-with-legacy-fallback.
        assert_eq!(resolve("badjson/lib/x.stylex"), None);
        assert_eq!(resolve("badjson"), None);
        // Missing package.json (dir exists) still gets legacy resolution.
        assert_eq!(
            resolve("plain").as_deref(),
            Some("node_modules/plain/index.js")
        );
    }

    #[test]
    fn imports_algebra_and_malformed_scope() {
        let root = hostile_fixture();
        let importer = root.join("src/input.ts");
        let resolve = |spec: &str| {
            StdFs
                .resolve_import(spec, &importer, ResolveConfig::default())
                .map(|p| path_relative(&root, &p))
        };
        assert_eq!(
            resolve("#tok/deep.stylex").as_deref(),
            Some("src/hashed/deep.stylex.js")
        );
        // Subpath validation applies to imports patterns too.
        assert_eq!(resolve("#tok/../esc"), None);
        assert_eq!(resolve("#tok/x/"), None);
        // Internal targets that parse as URLs are invalid.
        assert_eq!(resolve("#url"), None);
        // Bare re-entry through another package's exports, exact + pattern.
        assert_eq!(
            resolve("#bare").as_deref(),
            Some("node_modules/sealed/public/x.stylex.js")
        );
        assert_eq!(
            resolve("#barestar/x").as_deref(),
            Some("node_modules/sealed/public/x.stylex.js")
        );
        // Null-in-array fallback holds for imports maps as well.
        assert_eq!(resolve("#arr/x").as_deref(), Some("src/hashed/x.js"));
        // Non-star-final keys stay dead (prefix = key minus last char).
        assert_eq!(resolve("#dead/deep.stylex"), None);
        // A malformed scope package.json is fatal for every bare/# import.
        let scoped = root.join("scoped/input.ts");
        assert_eq!(
            StdFs.resolve_import("sealed/tokens/x.stylex", &scoped, ResolveConfig::default()),
            None
        );
        assert_eq!(
            StdFs.resolve_import("#tok/deep.stylex", &scoped, ResolveConfig::default()),
            None
        );
    }

    /// Every expectation executed against import-meta-resolve 4.2.1
    /// (scratchpad imr-r5-probe.mjs, 2026-08-28) — codex r5 findings 3-5.
    fn r5_fixture() -> PathBuf {
        static ONCE: std::sync::Once = std::sync::Once::new();
        let root = std::env::temp_dir().join("stylex-rs-node-resolution-r5");
        ONCE.call_once(|| {
            let write = |rel: &str, content: &str| {
                let path = root.join(rel);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, content).unwrap();
            };
            write(
                "package.json",
                r##"{"name":"r5-root","imports":{"#x*é":"./src/star/*.stylex.js","#y*":"./src/star/*.stylex.js","#q*":"./src/star/*.stylex.js?mode=theme","#é*":"./src/star/*.stylex.js","#bool.stylex":["bad-bool","./src/fallback.stylex.js"],"#num.stylex":["bad-num","./src/fallback.stylex.js"]}}"##,
            );
            for f in [
                "src/input.ts",
                "src/star/foo.stylex.stylex.js",
                "src/star/*foo.stylex.stylex.js",
                "src/star/-theme.stylex.js",
                "src/fallback.stylex.js",
                "src/x%20.stylex.js",
                "src/x .stylex.js",
                "src/only-enc%21.stylex.js",
                "src/pct é.stylex.js",
                "src/q.stylex",
            ] {
                write(f, "");
            }
            write(
                "node_modules/bad-bool/package.json",
                r#"{"name":"bad-bool","exports":true}"#,
            );
            write("node_modules/bad-bool/index.js", "");
            write(
                "node_modules/bad-num/package.json",
                r#"{"name":"bad-num","exports":42}"#,
            );
            write("node_modules/bad-num/index.js", "");
            write(
                "node_modules/qexp/package.json",
                r##"{"name":"qexp","exports":{
                    "./q.stylex":"./out/q.stylex.js?mode=x",
                    "./frag.stylex":"./out/q.stylex.js#frag",
                    "./qstar/*.stylex":"./out/*.stylex.js?v=*",
                    "./enc/*.stylex":"./out/*.stylex.js",
                    "./malformed.stylex":"./out/q%2g.stylex.js",
                    "./encdot/*.stylex":"./out/%2e%2e/*.stylex.js"
                }}"##,
            );
            for f in [
                "out/q.stylex.js",
                "out/deep.stylex.js",
                "out/sp ace.stylex.js",
                "out/q%2g.stylex.js",
                "escape.stylex.js",
            ] {
                write(&format!("node_modules/qexp/{f}"), "");
            }
        });
        std::fs::canonicalize(&root).unwrap_or(root)
    }

    #[test]
    fn r5_unicode_imports_keys() {
        let root = r5_fixture();
        let importer = root.join("src/input.ts");
        let resolve = |spec: &str| {
            StdFs
                .resolve_import(spec, &importer, ResolveConfig::default())
                .map(|p| path_relative(&root, &p))
        };
        // The é key's quirk prefix is "#x*" (slice(0,-1) in UTF-16 units);
        // this specifier byte-panicked before the fix.
        assert_eq!(resolve("#xfoo.stylex"), None);
        // A literal-star specifier does satisfy the quirk prefix; matched
        // starts at the star's UNIT index, keeping the star itself.
        assert_eq!(
            resolve("#x*foo.stylexé").as_deref(),
            Some("src/star/*foo.stylex.stylex.js")
        );
        assert_eq!(
            resolve("#yfoo.stylex").as_deref(),
            Some("src/star/foo.stylex.stylex.js")
        );
        // Multibyte prefix: matched must slice at unit index 2, not byte 3.
        assert_eq!(
            resolve("#éfoo.stylex").as_deref(),
            Some("src/star/foo.stylex.stylex.js")
        );
    }

    #[test]
    fn r5_file_url_semantics() {
        let root = r5_fixture();
        let importer = root.join("src/input.ts");
        let resolve = |spec: &str| {
            StdFs
                .resolve_import(spec, &importer, ResolveConfig::default())
                .map(|p| path_relative(&root, &p))
        };
        // Percent-decoding at finalization: the literal-percent file loses.
        assert_eq!(
            resolve("./x%20.stylex").as_deref(),
            Some("src/x .stylex.js")
        );
        assert_eq!(resolve("./x .stylex").as_deref(), Some("src/x .stylex.js"));
        assert_eq!(resolve("./only-enc%21.stylex"), None);
        assert_eq!(
            resolve("./pct%20é.stylex").as_deref(),
            Some("src/pct é.stylex.js")
        );
        // Query/fragment end the pathname (extensions append after them, so
        // only the raw extensionless candidate can hit).
        assert_eq!(
            resolve("./q.stylex?mode=theme").as_deref(),
            Some("src/q.stylex")
        );
        assert_eq!(resolve("./q.stylex#frag").as_deref(), Some("src/q.stylex"));
        // Malformed escapes reject (fileURLToPath URIError).
        assert_eq!(resolve("./q%2g.stylex"), None);
        assert_eq!(resolve("./x%2G.stylex"), None);
        // Encoded dot-dot pops in relative URL joins.
        assert_eq!(
            resolve("./star/%2e%2e/q.stylex").as_deref(),
            Some("src/q.stylex")
        );
        // Targets: query/fragment stripped for the fs lookup.
        assert_eq!(
            resolve("qexp/q.stylex").as_deref(),
            Some("node_modules/qexp/out/q.stylex.js")
        );
        assert_eq!(
            resolve("qexp/frag.stylex").as_deref(),
            Some("node_modules/qexp/out/q.stylex.js")
        );
        assert_eq!(
            resolve("qexp/qstar/deep.stylex").as_deref(),
            Some("node_modules/qexp/out/deep.stylex.js")
        );
        assert_eq!(
            resolve("qexp/enc/sp%20ace.stylex").as_deref(),
            Some("node_modules/qexp/out/sp ace.stylex.js")
        );
        assert_eq!(
            resolve("#q-theme").as_deref(),
            Some("src/star/-theme.stylex.js")
        );
        // A query inside the subpath is part of the exports-key match.
        assert_eq!(resolve("qexp/enc/deep.stylex?x"), None);
        assert_eq!(resolve("qexp/malformed.stylex"), None);
        // Encoded dot-dot in a target is invalid-segment, never a pop.
        assert_eq!(resolve("qexp/encdot/deep.stylex"), None);
    }

    #[test]
    fn r5_primitive_exports_roots_are_fatal() {
        let root = r5_fixture();
        let importer = root.join("src/input.ts");
        let resolve = |spec: &str| {
            StdFs
                .resolve_import(spec, &importer, ResolveConfig::default())
                .map(|p| path_relative(&root, &p))
        };
        // exports:true / exports:42 → PATH_NOT_EXPORTED, aborting the target
        // array — the fallback entry must never resolve.
        assert_eq!(resolve("#bool.stylex"), None);
        assert_eq!(resolve("#num.stylex"), None);
        assert_eq!(resolve("bad-bool"), None);
        assert_eq!(resolve("bad-bool/sub.stylex"), None);
    }

    #[test]
    fn segment_and_key_helpers() {
        assert!(has_deprecated_segment(".."));
        assert!(has_deprecated_segment("a/../b"));
        assert!(has_deprecated_segment("a/%2e%2E/b"));
        assert!(has_deprecated_segment("%2e."));
        assert!(has_deprecated_segment("a\\..\\b"));
        assert!(has_deprecated_segment("x/node_modules/y"));
        assert!(has_deprecated_segment("x/NODE_MODULES/y"));
        assert!(has_deprecated_segment("x/%6eode_modul%65s/y"));
        assert!(!has_deprecated_segment("a//b"));
        assert!(!has_deprecated_segment("a/.b/b"));
        assert!(!has_deprecated_segment("a/..b/b"));
        assert!(!has_deprecated_segment("a/node_modulesx/b"));
        assert!(!has_deprecated_segment("a/%2f/b"));
        assert!(is_js_array_index("0"));
        assert!(is_js_array_index("42"));
        assert!(is_js_array_index("1.5"));
        assert!(!is_js_array_index("01"));
        assert!(!is_js_array_index("-1"));
        assert!(!is_js_array_index("0x10"));
        assert!(!is_js_array_index(" 1"));
        assert!(!is_js_array_index("4294967295"));
        assert!(is_js_array_index("4294967294"));
        assert!(!is_js_array_index("default"));
        assert!(!is_js_array_index("NaN"));
        assert!(!is_js_array_index("Infinity"));
        assert!(parses_as_url("https://example.com/x.js"));
        assert!(parses_as_url("file:///etc/passwd"));
        assert!(parses_as_url("a:"));
        assert!(!parses_as_url("1a:b"));
        assert!(!parses_as_url("a/b:c"));
        assert!(!parses_as_url("no-colon"));
    }

    #[test]
    fn relative_paths() {
        let rel = |a: &str, b: &str| path_relative(Path::new(a), Path::new(b));
        assert_eq!(rel("/a/b", "/a/b/c/d.ts"), "c/d.ts");
        assert_eq!(rel("/a/b", "/a/x/d.ts"), "../x/d.ts");
        assert_eq!(rel("/a/b/c", "/a"), "../..");
        assert_eq!(
            normalize_path(Path::new("/a/b/../c/./d.ts")),
            PathBuf::from("/a/c/d.ts")
        );
        // A name that only starts like the directory's is no file under it.
        assert_eq!(rel("/a/b", "/a/bc/d.ts"), "../bc/d.ts");
        assert_eq!(rel("/a/b", "/a/b"), "");
        assert_eq!(rel("/", "/a/d.ts"), "a/d.ts");
        // Spellings that normalize to a directory above the file.
        assert_eq!(rel("/a/b/", "/a/b/c/d.ts"), "c/d.ts");
        assert_eq!(rel("/a/./b", "/a/b/c/d.ts"), "c/d.ts");
        assert_eq!(rel("/a/b", "/a/b//c/../d.ts"), "d.ts");
        assert_eq!(rel("a/b", "a/b/c/d.ts"), "c/d.ts");
        for path in ["/", "/a", "/a/b.c/d", "a/b", "a", "..a/.b/c.."] {
            assert_eq!(normalize_path(Path::new(path)), PathBuf::from(path));
        }
        for (path, normal) in [
            ("/a/", "/a"),
            ("/a//b", "/a/b"),
            ("./a", "a"),
            ("a/..", ""),
            ("../a", "../a"),
            ("", ""),
        ] {
            assert_eq!(normalize_path(Path::new(path)), PathBuf::from(normal));
        }
    }

    /// Fresh per test (tests mutate it): root manifest, a nested `pkg`
    /// manifest, and files two and three levels below `pkg`.
    fn memo_fixture(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("stylex-rs-memofs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let write = |rel: &str, content: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        };
        write("package.json", r#"{"name":"root-pkg"}"#);
        write("pkg/package.json", r#"{"name":"nested-pkg"}"#);
        write("pkg/src/a/one.ts", "");
        write("pkg/src/b/two.ts", "");
        write("pkg/src/b/tokens.stylex.ts", "");
        write("other/deep/three.ts", "");
        std::fs::canonicalize(&root).unwrap_or(root)
    }

    fn os_eq(a: Option<(String, PathBuf)>, b: Option<(String, PathBuf)>) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some((n1, d1)), Some((n2, d2))) => n1 == n2 && d1.as_os_str() == d2.as_os_str(),
            _ => false,
        }
    }

    #[test]
    fn memo_snapshot_matches_std_and_fills_every_visited_level() {
        let root = memo_fixture("snapshot");
        let memo = MemoFs::snapshot();
        for rel in [
            "pkg/src/a/one.ts",
            "pkg/src/b/two.ts",
            "other/deep/three.ts",
            "pkg/src/a/one.ts",
        ] {
            let file = root.join(rel);
            assert!(os_eq(
                memo.nearest_package(&file),
                StdFs.nearest_package(&file)
            ));
        }
        let filled = read_lock(&memo.nearest);
        for dir in ["pkg/src/a", "pkg/src", "pkg", "other/deep", "other"] {
            assert!(
                filled.contains_key(path_key(&root.join(dir))),
                "{dir} not filled"
            );
        }
        // Seven directories were visited (pkg/src/a, pkg/src/b, pkg/src, pkg,
        // other/deep, other, root), each probed exactly once; the two
        // manifests among them were read.
        let under_root = |keys: Vec<&Box<[u8]>>| {
            keys.iter()
                .filter(|k| k.starts_with(path_key(&root)))
                .count()
        };
        assert_eq!(under_root(read_lock(&memo.stats).keys().collect()), 7);
        let manifests = read_lock(&memo.manifests);
        assert!(manifests.contains_key(path_key(&root.join("pkg/package.json"))));
        assert!(manifests.contains_key(path_key(&root.join("package.json"))));
        assert_eq!(under_root(manifests.keys().collect()), 2);
    }

    #[test]
    fn memo_snapshot_freezes_while_live_revalidates_edits_and_new_manifests() {
        let root = memo_fixture("live");
        let snapshot = MemoFs::snapshot();
        let live = MemoFs::live();
        let file = root.join("pkg/src/a/one.ts");
        let before = Some(("nested-pkg".to_string(), root.join("pkg")));
        assert!(os_eq(snapshot.nearest_package(&file), before.clone()));
        assert!(os_eq(live.nearest_package(&file), before.clone()));

        // Edit: a different length guarantees a stamp change even inside one
        // mtime tick.
        std::fs::write(
            root.join("pkg/package.json"),
            r#"{"name":"nested-pkg-renamed"}"#,
        )
        .unwrap();
        assert!(os_eq(snapshot.nearest_package(&file), before.clone()));
        assert!(os_eq(
            live.nearest_package(&file),
            Some(("nested-pkg-renamed".to_string(), root.join("pkg")))
        ));

        // A new manifest at an intermediate level is seen live, not by snapshot.
        std::fs::write(root.join("pkg/src/package.json"), r#"{"name":"src-pkg"}"#).unwrap();
        assert!(os_eq(snapshot.nearest_package(&file), before));
        assert!(os_eq(
            live.nearest_package(&file),
            Some(("src-pkg".to_string(), root.join("pkg/src")))
        ));
        assert!(os_eq(
            live.nearest_package(&file),
            StdFs.nearest_package(&file)
        ));

        // A broken manifest ends the live walk with no answer, like StdFs.
        std::fs::write(root.join("pkg/src/package.json"), "{").unwrap();
        assert!(live.nearest_package(&file).is_none());
        assert!(StdFs.nearest_package(&file).is_none());

        snapshot.invalidate_all();
        assert!(snapshot.nearest_package(&file).is_none());
    }

    #[test]
    fn memo_keys_by_spelling_so_found_dirs_keep_the_callers_spelling() {
        let root = memo_fixture("spelling");
        let memo = MemoFs::snapshot();
        let plain = root.join("pkg/src/a/one.ts");
        let doubled = PathBuf::from(format!("{}//pkg/src/a/one.ts", root.display()));
        assert_eq!(Path::new(&plain), Path::new(&doubled));
        assert!(os_eq(
            memo.nearest_package(&plain),
            StdFs.nearest_package(&plain)
        ));
        assert!(os_eq(
            memo.nearest_package(&doubled),
            StdFs.nearest_package(&doubled)
        ));
        assert_ne!(
            memo.nearest_package(&plain).unwrap().1.as_os_str(),
            memo.nearest_package(&doubled).unwrap().1.as_os_str()
        );
    }

    #[test]
    fn memo_snapshot_resolve_is_keyed_by_importer_dir_and_config() {
        let root = memo_fixture("resolve");
        let memo = MemoFs::snapshot();
        let a = root.join("pkg/src/b/two.ts");
        let b = root.join("pkg/src/b/other.ts");
        let with_root = ResolveConfig {
            aliases: None,
            root_dir: Some(&root),
        };
        let specifier = "/ROOT/pkg/src/b/tokens.stylex";
        let expected = StdFs.resolve_import(specifier, &a, with_root);
        assert_eq!(expected, Some(root.join("pkg/src/b/tokens.stylex.ts")));
        assert_eq!(memo.resolve_import(specifier, &a, with_root), expected);
        assert_eq!(memo.resolve_import(specifier, &b, with_root), expected);
        // No rootDir: the placeholder is a path like any other.
        assert_eq!(
            memo.resolve_import(specifier, &b, ResolveConfig::default()),
            None
        );
        let memos = read_lock(&memo.resolve);
        let by_dir = &memos.by_dir[path_key(&root.join("pkg/src/b"))];
        assert_eq!(by_dir.len(), 2, "one memo per distinct config");
        assert!(by_dir.iter().all(|m| m.by_specifier.len() == 1));
        assert_eq!(memos.by_dir.len(), 1, "one importer directory");
        assert_eq!(memos.kept, 2);
        assert!(read_lock(&memo.relative).is_empty());
    }

    #[test]
    fn memo_keeps_a_relative_import_by_the_path_it_names() {
        let root = memo_fixture("relative");
        let memo = MemoFs::snapshot();
        let tokens = Some(root.join("pkg/src/b/tokens.stylex.ts"));
        let resolve = |fs: &dyn FsProvider, specifier: &str, importer: &str| {
            fs.resolve_import(specifier, &root.join(importer), ResolveConfig::default())
        };
        // One file from four directories, under one name.
        for (specifier, importer) in [
            ("./tokens.stylex", "pkg/src/b/two.ts"),
            ("../b/tokens.stylex", "pkg/src/a/one.ts"),
            ("../../pkg/src/b/tokens.stylex", "other/deep/three.ts"),
            ("./src/b/tokens.stylex", "pkg/index.ts"),
            ("./src/a/.././b//tokens.stylex", "pkg/index.ts"),
        ] {
            assert_eq!(resolve(&memo, specifier, importer), tokens, "{specifier}");
            assert_eq!(resolve(&StdFs, specifier, importer), tokens, "{specifier}");
        }
        assert_eq!(read_lock(&memo.relative).len(), 1);
        assert!(read_lock(&memo.resolve).by_dir.is_empty());

        // Each a name of its own: the extension it ends in decides which
        // candidates follow it.
        let named = [
            "./tokens.stylex.ts",
            "./tokens.stylex.js",
            "./tokens",
            "./two",
            "./two.ts",
            "./.ts",
            "../b",
            "./missing/tokens.stylex",
        ];
        for specifier in named {
            assert_eq!(
                resolve(&memo, specifier, "pkg/src/b/two.ts"),
                resolve(&StdFs, specifier, "pkg/src/b/two.ts"),
                "{specifier}"
            );
        }
        assert_eq!(read_lock(&memo.relative).len(), 1 + named.len());
        assert!(read_lock(&memo.resolve).by_dir.is_empty());

        // Spellings that are rewritten before they reach the filesystem, or
        // whose last segment is no name, stay with the importer's directory.
        let rewritten = [
            "./tokens.stylex?raw",
            "./tokens.stylex#top",
            "./tokens%2Estylex",
            ".\\tokens.stylex",
            "./%2e/tokens.stylex",
            "./tokens.stylex/",
            "./tokens.stylex/..",
            "..",
            ".",
        ];
        for specifier in rewritten {
            assert_eq!(
                resolve(&memo, specifier, "pkg/src/b/two.ts"),
                resolve(&StdFs, specifier, "pkg/src/b/two.ts"),
                "{specifier}"
            );
        }
        assert_eq!(read_lock(&memo.relative).len(), 1 + named.len());
        let memos = read_lock(&memo.resolve);
        let by_dir = &memos.by_dir[path_key(&root.join("pkg/src/b"))];
        assert_eq!(by_dir[0].by_specifier.len(), rewritten.len());
        assert_eq!(memos.kept, rewritten.len());

        // So does every specifier of an importer whose own path wants
        // normalizing first.
        drop(memos);
        let dotted = format!("{}/pkg/src/a/../b/two.ts", root.display());
        assert_eq!(
            memo.resolve_import(
                "./tokens.stylex",
                Path::new(&dotted),
                ResolveConfig::default()
            ),
            tokens
        );
        assert_eq!(read_lock(&memo.relative).len(), 1 + named.len());
        assert_eq!(read_lock(&memo.resolve).by_dir.len(), 2);
    }

    /// The live tests' settle window: a fraction of the real one, still
    /// several timestamp ticks wide on any filesystem they run on.
    const TEST_SETTLE: Duration = Duration::from_millis(40);

    fn come_to_rest() {
        std::thread::sleep(TEST_SETTLE * 3);
    }

    fn put(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// Fresh per test: an app with a theme directory, two components three
    /// levels down, and a dependency that routes through an exports map.
    fn live_fixture(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("stylex-rs-livefs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        put(&root, "package.json", r#"{"name":"app"}"#);
        put(&root, "src/theme/tokens.stylex.ts", "");
        put(&root, "src/theme/alt/tokens.stylex.ts", "");
        put(&root, "src/ui/button/Button.tsx", "");
        put(&root, "src/ui/card/Card.tsx", "");
        put(
            &root,
            "node_modules/dep/package.json",
            r#"{"name":"dep","exports":{"./*":"./lib/*.js"}}"#,
        );
        put(&root, "node_modules/dep/lib/tokens.stylex.js", "");
        put(&root, "node_modules/dep/alt/tokens.stylex.js", "");
        std::fs::canonicalize(&root).unwrap_or(root)
    }

    fn theme_alias(root: &Path) -> AliasMap {
        vec![(
            "@theme/*".to_string(),
            vec![format!("{}/src/theme/*", root.display())],
        )]
    }

    /// What the fixture's components ask while they compile.
    fn compile_answers(fs: &dyn FsProvider, root: &Path) -> Vec<String> {
        let aliases = theme_alias(root);
        let aliased = ResolveConfig {
            aliases: Some(&aliases),
            root_dir: Some(root),
        };
        let mut answers = Vec::new();
        for rel in ["src/ui/button/Button.tsx", "src/ui/card/Card.tsx"] {
            let file = root.join(rel);
            answers.push(format!("{:?}", fs.nearest_package(&file)));
            for specifier in [
                "../../theme/tokens.stylex",
                "./missing/deep/thing",
                "dep/tokens.stylex",
            ] {
                let resolved = fs.resolve_import(specifier, &file, ResolveConfig::default());
                answers.push(format!("{specifier} -> {resolved:?}"));
            }
            for specifier in ["@theme/tokens.stylex", "@theme/alt", "/ROOT/src/theme/alt"] {
                let resolved = fs.resolve_import(specifier, &file, aliased);
                answers.push(format!("{specifier} -> {resolved:?}"));
            }
        }
        answers
    }

    /// Those, and what a provider is asked beside a compile.
    fn answers(fs: &dyn FsProvider, root: &Path) -> Vec<String> {
        let mut answers = compile_answers(fs, root);
        let manifest = root.join("package.json");
        let tokens = root.join("src/theme/tokens.stylex.ts");
        answers.push(format!("{:?}", fs.hash_file(&manifest)));
        answers.push(format!("{:?}", fs.hash_file(&tokens)));
        answers.push(format!("{:?}", fs.exists(&tokens)));
        answers.push(format!("{:?}", fs.exists_any(&root.join("src/theme"))));
        answers.push(format!(
            "{:?}",
            fs.canonicalize_root(&root.join("src/theme"))
        ));
        // The theme directory, spelled with the slash that has a stat follow
        // a symlink by that name.
        let aliases = theme_alias(root);
        let aliased = ResolveConfig {
            aliases: Some(&aliases),
            root_dir: None,
        };
        let button = root.join("src/ui/button/Button.tsx");
        answers.push(format!(
            "{:?}",
            fs.resolve_import("@theme/", &button, aliased)
        ));
        answers
    }

    /// The live fs says what the disk says right after a change, once the
    /// change has settled, and again from what it then kept.
    #[track_caller]
    fn assert_follows_disk(live: &MemoFs, root: &Path, what: &str) -> Vec<String> {
        let disk = answers(&StdFs, root);
        assert_eq!(answers(&live.session(), root), disk, "{what}: just changed");
        assert_eq!(answers(live, root), disk, "{what}: call by call");
        come_to_rest();
        assert_eq!(answers(&live.session(), root), disk, "{what}: settled");
        assert_eq!(answers(&live.session(), root), disk, "{what}: kept");
        disk
    }

    /// Runs each change in turn against a live fs that has answers kept from
    /// before it; every change must move an answer.
    #[track_caller]
    fn assert_follows_changes(root: &Path, changes: &[(&str, &dyn Fn())]) {
        let live = MemoFs::settling(Some(TEST_SETTLE));
        assert_fs_follows_changes(&live, root, changes);
    }

    #[track_caller]
    fn assert_fs_follows_changes(live: &MemoFs, root: &Path, changes: &[(&str, &dyn Fn())]) {
        let mut last = assert_follows_disk(live, root, "untouched");
        for (what, change) in changes {
            change();
            let now = assert_follows_disk(live, root, what);
            assert_ne!(now, last, "{what}: moves an answer");
            last = now;
        }
    }

    /// How many answers each map of `fs` holds.
    fn kept_counts(fs: &MemoFs) -> [(&'static str, usize); 8] {
        let resolve = read_lock(&fs.resolve);
        let specifiers = resolve.by_dir.values().flatten();
        assert_eq!(
            resolve.kept,
            specifiers
                .map(|memo| memo.by_specifier.len())
                .sum::<usize>()
        );
        [
            ("nearest", read_lock(&fs.nearest).len()),
            ("scopes", read_lock(&fs.scopes).len()),
            ("packages", read_lock(&fs.packages).len()),
            ("manifests", read_lock(&fs.manifests).len()),
            ("relative", read_lock(&fs.relative).len()),
            ("resolve", resolve.kept),
            ("real", read_lock(&fs.real).len()),
            ("hashes", read_lock(&fs.hashes).len()),
        ]
    }

    #[test]
    fn memo_live_empties_a_full_map_and_goes_on_answering() {
        assert_eq!(MemoFs::live().cap, LIVE_CACHE_CAP);
        assert_eq!(MemoFs::snapshot().cap, usize::MAX);

        for cap in [1, 2] {
            let root = live_fixture(&format!("capped-{cap}"));
            let roomy = MemoFs::settling(Some(TEST_SETTLE));
            let capped = MemoFs {
                cap,
                ..MemoFs::settling(Some(TEST_SETTLE))
            };
            come_to_rest();
            // Twice: an answer that waited on the temp dir above the fixture
            // is kept for good the second time.
            for _ in 0..2 {
                let disk = answers(&StdFs, &root);
                assert_eq!(answers(&roomy.session(), &root), disk);
                assert_eq!(answers(&capped.session(), &root), disk);
                come_to_rest();
            }
            // The fixture asks every map for more than one answer (all but
            // `manifests` and `hashes` for more than two), so each has been
            // emptied on the way.
            let unbounded = kept_counts(&roomy);
            for ((map, held), (_, wanted)) in kept_counts(&capped).into_iter().zip(unbounded) {
                assert!(wanted > 1, "{map}: {wanted} answers to keep");
                assert!((1..=cap).contains(&held), "{map}: {held} kept under {cap}");
            }

            let remove = |rel: &str| std::fs::remove_file(root.join(rel)).unwrap();
            assert_fs_follows_changes(
                &capped,
                &root,
                &[
                    ("an earlier extension appears", &|| {
                        put(&root, "src/theme/tokens.stylex.js", "");
                    }),
                    ("a nearer manifest appears", &|| {
                        put(&root, "src/ui/package.json", r#"{"name":"ui"}"#);
                    }),
                    ("a package reroutes its exports", &|| {
                        put(
                            &root,
                            "node_modules/dep/package.json",
                            r#"{"name":"dep","exports":{"./*":"./alt/*.js"}}"#,
                        );
                    }),
                    ("the resolved files go", &|| {
                        remove("src/theme/tokens.stylex.js");
                        remove("src/theme/tokens.stylex.ts");
                    }),
                ],
            );
            for (map, held) in kept_counts(&capped) {
                assert!(held <= cap, "{map}: {held} kept under {cap}");
            }
        }
    }

    #[test]
    fn memo_live_follows_files_directories_and_manifests() {
        let root = live_fixture("follows");
        let remove = |rel: &str| std::fs::remove_file(root.join(rel)).unwrap();
        let remove_dir = |rel: &str| std::fs::remove_dir_all(root.join(rel)).unwrap();
        assert_follows_changes(
            &root,
            &[
                ("an earlier extension appears", &|| {
                    put(&root, "src/theme/tokens.stylex.js", "");
                }),
                ("and goes", &|| remove("src/theme/tokens.stylex.js")),
                ("the resolved file goes", &|| {
                    remove("src/theme/tokens.stylex.ts");
                }),
                ("and is back with other bytes", &|| {
                    put(&root, "src/theme/tokens.stylex.ts", "export {};");
                }),
                ("a file becomes a directory", &|| {
                    remove("src/theme/tokens.stylex.ts");
                    put(&root, "src/theme/tokens.stylex.ts/index.ts", "");
                }),
                ("and a file again", &|| {
                    remove_dir("src/theme/tokens.stylex.ts");
                    put(&root, "src/theme/tokens.stylex.ts", "");
                }),
                ("missing directories fill in", &|| {
                    put(&root, "src/ui/button/missing/deep/thing.ts", "");
                }),
                ("an aliased directory goes", &|| remove_dir("src/theme/alt")),
                ("a manifest is renamed at the same length", &|| {
                    put(&root, "package.json", r#"{"name":"apq"}"#);
                }),
                ("a nearer manifest appears", &|| {
                    put(&root, "src/ui/package.json", r#"{"name":"ui"}"#);
                }),
                ("breaks", &|| put(&root, "src/ui/package.json", "{")),
                ("and goes", &|| remove("src/ui/package.json")),
                ("a package reroutes its exports", &|| {
                    put(
                        &root,
                        "node_modules/dep/package.json",
                        r#"{"name":"dep","exports":{"./*":"./alt/*.js"}}"#,
                    );
                }),
                ("a nearer copy of the package appears", &|| {
                    put(
                        &root,
                        "src/ui/node_modules/dep/package.json",
                        r#"{"name":"dep","exports":{"./*":"./*.js"}}"#,
                    );
                    put(&root, "src/ui/node_modules/dep/tokens.stylex.js", "");
                }),
                ("loses the file it routes to", &|| {
                    remove("src/ui/node_modules/dep/tokens.stylex.js");
                }),
                ("and goes", &|| remove_dir("src/ui/node_modules")),
                ("the tree above the components goes", &|| remove_dir("src")),
            ],
        );
    }

    #[cfg(unix)]
    #[test]
    fn memo_live_follows_symlinks_and_what_they_point_at() {
        use std::os::unix::fs::symlink;
        let root = live_fixture("symlinks");
        put(&root, "src/theme/other/tokens.stylex.ts", "");
        put(&root, "themes/two/tokens.stylex.ts", "");
        let dep = |exports: &str| {
            put(
                &root,
                "store/dep/package.json",
                &format!(r#"{{"name":"dep","exports":{{"./*":"{exports}"}}}}"#),
            );
        };
        dep("./*.js");
        put(&root, "store/dep/tokens.stylex.js", "");
        put(&root, "store/dep/alt/tokens.stylex.js", "");
        let remove = |rel: &str| std::fs::remove_file(root.join(rel)).unwrap();
        let link = |target: &str, rel: &str| symlink(target, root.join(rel)).unwrap();
        // Where a step leaves the symlink be, the directory holding it does
        // not change either.
        assert_follows_changes(
            &root,
            &[
                ("a file is swapped for a symlink", &|| {
                    remove("src/theme/tokens.stylex.ts");
                    link("alt/tokens.stylex.ts", "src/theme/tokens.stylex.ts");
                }),
                ("its target goes", &|| {
                    remove("src/theme/alt/tokens.stylex.ts");
                }),
                ("it is retargeted", &|| {
                    remove("src/theme/tokens.stylex.ts");
                    link("other/tokens.stylex.ts", "src/theme/tokens.stylex.ts");
                }),
                ("a directory is swapped for a symlink", &|| {
                    std::fs::rename(root.join("src/theme"), root.join("themes/one")).unwrap();
                    link("../themes/one", "src/theme");
                }),
                ("it is retargeted", &|| {
                    remove("src/theme");
                    link("../themes/two", "src/theme");
                }),
                ("its target goes", &|| {
                    std::fs::remove_dir_all(root.join("themes/two")).unwrap();
                }),
                ("and is back", &|| {
                    put(&root, "themes/two/tokens.stylex.ts", "");
                }),
                ("a package is swapped for a symlink", &|| {
                    std::fs::remove_dir_all(root.join("node_modules/dep")).unwrap();
                    link("../store/dep", "node_modules/dep");
                }),
                ("the manifest behind it reroutes", &|| dep("./alt/*.js")),
                ("the package behind it goes", &|| {
                    std::fs::rename(root.join("store/dep"), root.join("store/gone")).unwrap();
                }),
            ],
        );
    }

    /// A miss beyond a symlinked directory rests on the directory the symlink
    /// leads to, not on the one that holds the symlink: what appears or
    /// changes kind there is seen by the next look, with every stamp long
    /// settled and neither the symlink nor its holder touched.
    #[cfg(unix)]
    #[test]
    fn memo_live_sees_into_a_symlinked_directory_at_once() {
        use std::os::unix::fs::symlink;
        let root = live_fixture("symlinked-dirs");
        // A workspace package and a scoped dependency, each behind a symlink.
        put(&root, "packages/theme/src/colors.stylex.ts", "");
        put(
            &root,
            "store/pkg/package.json",
            r#"{"name":"@scope/pkg","exports":{"./*":"./lib/*.js"}}"#,
        );
        put(&root, "store/pkg/lib/colors.stylex.js", "");
        std::fs::create_dir_all(root.join("node_modules/@scope")).unwrap();
        symlink("../packages/theme", root.join("src/linked")).unwrap();
        symlink("../../store/pkg", root.join("node_modules/@scope/pkg")).unwrap();

        let aliases: AliasMap = vec![(
            "@linked/*".to_string(),
            vec![format!("{}/src/linked/src/*", root.display())],
        )];
        let ask = |fs: &dyn FsProvider| -> Vec<String> {
            let button = root.join("src/ui/button/Button.tsx");
            let aliased = ResolveConfig {
                aliases: Some(&aliases),
                root_dir: Some(&root),
            };
            let mut answers = Vec::new();
            for specifier in [
                "../../linked/src/tokens.stylex",
                "../../linked/deep/tokens.stylex",
                "@scope/pkg/tokens.stylex",
            ] {
                let resolved = fs.resolve_import(specifier, &button, ResolveConfig::default());
                answers.push(format!("{specifier} -> {resolved:?}"));
            }
            for specifier in ["@linked/tokens.stylex", "/ROOT/src/linked/deep"] {
                let resolved = fs.resolve_import(specifier, &button, aliased);
                answers.push(format!("{specifier} -> {resolved:?}"));
            }
            let inside = root.join("src/linked/src/colors.stylex.ts");
            answers.push(format!("{:?}", fs.nearest_package(&inside)));
            answers
        };
        // What moves when a symlink is retargeted or its holder relisted.
        let links = || {
            ["src", "node_modules/@scope"]
                .map(|holder| Stamp::of(&root.join(holder)))
                .into_iter()
                .chain(["src/linked", "node_modules/@scope/pkg"].map(|link| {
                    Stamp::Node(Node::of(&root.join(link).symlink_metadata().unwrap()))
                }))
                .collect::<Vec<_>>()
        };

        let live = MemoFs::settling(Some(TEST_SETTLE));
        come_to_rest();
        let mut last = ask(&live.session());
        assert_eq!(last, ask(&StdFs));
        assert!(last[..5].iter().all(|answer| answer.ends_with("None")));
        // Answers that waited on the temp dir above the fixture are worked
        // out a second time, and no more.
        come_to_rest();
        assert_eq!(ask(&live.session()), last);

        // At rest the miss is answered from the listing of the directory
        // behind the symlink, the missing file itself left unstatted.
        let session = live.session();
        assert_eq!(ask(&session), last);
        {
            let seen = lock(&session.look.as_ref().unwrap().seen);
            let through_link = seen.stats[path_key(&root.join("src/linked/src"))];
            assert_eq!(
                through_link.stamp,
                Stamp::of(&root.join("packages/theme/src"))
            );
            assert!(
                !seen
                    .stats
                    .contains_key(path_key(&root.join("src/linked/src/tokens.stylex.ts")))
            );
        }

        let untouched = links();
        let remove = |rel: &str| std::fs::remove_file(root.join(rel)).unwrap();
        let changes: [(&str, &dyn Fn()); 7] = [
            ("a file appears behind the symlink", &|| {
                put(&root, "packages/theme/src/tokens.stylex.ts", "");
            }),
            ("a directory fills in behind it", &|| {
                put(&root, "packages/theme/deep/tokens.stylex.ts", "");
            }),
            ("a file appears in the linked package", &|| {
                put(&root, "store/pkg/lib/tokens.stylex.js", "");
            }),
            ("a manifest appears behind the symlink", &|| {
                put(&root, "packages/theme/package.json", r#"{"name":"theme"}"#);
            }),
            ("the file becomes a directory", &|| {
                remove("packages/theme/src/tokens.stylex.ts");
                put(&root, "packages/theme/src/tokens.stylex.ts/index.ts", "");
            }),
            ("the linked package loses its file", &|| {
                remove("store/pkg/lib/tokens.stylex.js");
            }),
            ("the directory behind the symlink goes", &|| {
                std::fs::remove_dir_all(root.join("packages/theme/deep")).unwrap();
            }),
        ];
        for (what, change) in changes {
            change();
            let disk = ask(&StdFs);
            assert_ne!(disk, last, "{what}: moves an answer");
            assert_eq!(ask(&live.session()), disk, "{what}: just changed");
            assert_eq!(ask(&live), disk, "{what}: call by call");
            come_to_rest();
            assert_eq!(ask(&live.session()), disk, "{what}: settled");
            assert_eq!(ask(&live.session()), disk, "{what}: kept");
            assert_eq!(links(), untouched, "{what}: leaves the symlinks be");
            last = disk;
        }
    }

    /// Bare and `#` specifiers from importers at every depth: the walks a
    /// memo keeps by directory end where the walks made afresh do, whichever
    /// importer comes through first.
    #[test]
    fn memo_walks_end_where_std_walks_end() {
        let root = live_fixture("walks");
        put(
            &root,
            "package.json",
            r##"{"name":"app","exports":{"./*":"./src/*.ts"},"imports":{"#theme/*":"./src/theme/*.ts"}}"##,
        );
        put(
            &root,
            "src/ui/package.json",
            r#"{"name":"ui","exports":{"./*":"./*.tsx"}}"#,
        );
        put(
            &root,
            "src/ui/node_modules/dep/package.json",
            r#"{"name":"dep","exports":{"./*":"./near/*.js"}}"#,
        );
        put(&root, "src/ui/node_modules/dep/near/tokens.stylex.js", "");
        put(&root, "src/ui/card/node_modules/only-card/index.js", "");
        let importers = [
            "src/ui/button/Button.tsx",
            "src/ui/card/Card.tsx",
            "src/ui/index.ts",
            "src/main.ts",
            "main.ts",
            "node_modules/dep/lib/inner.js",
            "src/ui/node_modules/dep/near/inner.js",
        ];
        let specifiers = [
            "dep/tokens.stylex",
            "app/theme/tokens.stylex",
            "#theme/tokens.stylex",
            "ui/button/Button",
            "only-card",
            "nowhere/tokens.stylex",
            "@scope/nowhere",
        ];
        let mut questions = Vec::new();
        for importer in importers {
            for specifier in specifiers {
                questions.push((root.join(importer), specifier));
            }
        }
        let ask = |fs: &dyn FsProvider, questions: &[(PathBuf, &str)]| -> Vec<Option<PathBuf>> {
            questions
                .iter()
                .map(|(importer, specifier)| {
                    fs.resolve_import(specifier, importer, ResolveConfig::default())
                })
                .collect()
        };
        let expected = ask(&StdFs, &questions);
        let mut found: Vec<&PathBuf> = expected.iter().flatten().collect();
        found.sort();
        found.dedup();
        assert_eq!(
            found,
            [
                &root.join("node_modules/dep/lib/tokens.stylex.js"),
                &root.join("src/theme/tokens.stylex.ts"),
                &root.join("src/ui/button/Button.tsx"),
                &root.join("src/ui/card/node_modules/only-card/index.js"),
                &root.join("src/ui/node_modules/dep/near/tokens.stylex.js"),
            ]
        );

        assert_eq!(ask(&MemoFs::snapshot(), &questions), expected);
        let backwards: Vec<_> = questions.iter().rev().cloned().collect();
        let mut answers = ask(&MemoFs::snapshot(), &backwards);
        answers.reverse();
        assert_eq!(answers, expected);

        let live = MemoFs::settling(Some(TEST_SETTLE));
        assert_eq!(ask(&live.session(), &questions), expected);
        come_to_rest();
        let mut answers = ask(&live.session(), &backwards);
        answers.reverse();
        assert_eq!(answers, expected);
        assert_eq!(ask(&live.session(), &questions), expected);
    }

    #[test]
    fn memo_session_reads_one_state_of_the_disk() {
        let root = live_fixture("session");
        let live = MemoFs::settling(Some(TEST_SETTLE));
        let button = root.join("src/ui/button/Button.tsx");
        let card = root.join("src/ui/card/Card.tsx");
        let resolve = |fs: &dyn FsProvider, importer: &Path| {
            fs.resolve_import(
                "../../theme/tokens.stylex",
                importer,
                ResolveConfig::default(),
            )
        };
        let ts = Some(root.join("src/theme/tokens.stylex.ts"));
        let js = Some(root.join("src/theme/tokens.stylex.js"));

        let session = live.session();
        assert_eq!(resolve(&session, &button), ts);
        put(&root, "src/theme/tokens.stylex.js", "");
        // Kept, and worked out anew from another directory: the session has
        // looked, and saw no `.js`.
        assert_eq!(resolve(&session, &button), ts);
        assert_eq!(resolve(&session, &card), ts);
        assert_eq!(resolve(&live.session(), &card), js);
        assert_eq!(resolve(&live, &button), js);

        // A snapshot's sessions are the snapshot.
        let frozen = MemoFs::snapshot();
        assert_eq!(resolve(&frozen.session(), &button), js);
        std::fs::remove_file(root.join("src/theme/tokens.stylex.js")).unwrap();
        assert_eq!(resolve(&frozen.session(), &card), js);
        assert_eq!(resolve(&frozen, &card), js);
    }

    #[test]
    fn memo_live_trusts_a_stamp_only_once_it_has_stood_the_settle_window() {
        let at = |millis: u64| SystemTime::UNIX_EPOCH + Duration::from_millis(millis);
        let look = |started: u64| Look {
            started: at(started),
            settle: Duration::from_millis(100),
            seen: Mutex::default(),
        };
        let node = |modified: u64, changed: u64| Node {
            kind: Kind::Dir,
            len: 0,
            modified: Some(at(modified)),
            changed: Some(at(changed)),
            device: 0,
            inode: 0,
        };
        assert!(look(1100).settled(&node(1000, 1000)));
        assert!(!look(1099).settled(&node(1000, 1000)));
        // The later of the two times counts: a restored mtime hides nothing.
        assert!(!look(1100).settled(&node(500, 1001)));
        assert!(!look(1100).settled(&node(1001, 500)));
        // A stamp from the look's future, as a skewed clock hands out.
        assert!(!look(1100).settled(&node(5000, 5000)));
        let unstamped = Node {
            modified: None,
            ..node(0, 0)
        };
        assert!(!look(1100).settled(&unstamped));
    }

    #[cfg(unix)]
    #[test]
    fn memo_live_at_rest_checks_directories_and_the_manifests_it_read() {
        let root = live_fixture("rest");
        let live = MemoFs::settling(Some(TEST_SETTLE));
        come_to_rest();
        let first = compile_answers(&live.session(), &root);
        assert_eq!(first, compile_answers(&StdFs, &root));
        // Answers that waited on the temp dir above the fixture are worked
        // out a second time, and no more.
        come_to_rest();
        assert_eq!(compile_answers(&live.session(), &root), first);

        let session = live.session();
        assert_eq!(compile_answers(&session, &root), first);
        let seen = lock(&session.look.as_ref().unwrap().seen);
        let below_root = |key: &[u8]| {
            let path = Path::new(std::str::from_utf8(key).unwrap()).to_path_buf();
            (path != root && path.starts_with(&root)).then_some(path)
        };
        let mut manifests = Vec::new();
        for (key, sight) in &seen.stats {
            let Some(path) = below_root(key) else {
                continue;
            };
            match sight.stamp.kind() {
                Some(Kind::Dir) => {}
                Some(Kind::File) => manifests.push(path),
                _ => panic!("{} was statted: {:?}", path.display(), sight.stamp),
            }
        }
        manifests.sort();
        assert_eq!(
            manifests,
            [
                root.join("node_modules/dep/package.json"),
                root.join("package.json")
            ]
        );
        // Symlinks are looked for no deeper than the fixture's own name, in a
        // temp dir that may never come to rest.
        assert!(!seen.real.keys().any(|key| below_root(key).is_some()));
    }
}
