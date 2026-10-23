//! commonJS canonical file names, theme-file suffix checks, import resolution.
// parity: babel-plugin src/utils/state-manager.js:566-707 + file-based-identifier.js

use std::path::{Component, Path, PathBuf};
use std::sync::{LazyLock, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::SystemTime;

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
        resolve_import_with(self, specifier, importer, config)
    }

    fn exists(&self, p: &Path) -> bool {
        p.is_file()
    }
}

/// Directory-keyed memo over [`StdFs`]: `snapshot()` freezes the fs for the process;
/// `live()` re-stats package.json per level and re-parses only on an (mtime, size) change.
pub struct MemoFs {
    live: bool,
    nearest: PathMap<Option<(String, PathBuf)>>,
    manifests: PathMap<Option<Manifest>>,
    resolve: PathMap<Vec<ResolveMemo>>,
    exists: PathMap<bool>,
    exists_any: PathMap<bool>,
    hashes: PathMap<(Option<Stamp>, Option<String>)>,
    canon: PathMap<PathBuf>,
}

// Keyed by the path's raw bytes: `Path` equality folds spellings (`a//b`,
// `a/b/`) whose walks return differently spelled directories.
type PathMap<V> = RwLock<FxHashMap<Box<[u8]>, V>>;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl Stamp {
    fn of(meta: &std::fs::Metadata) -> Self {
        Self {
            modified: meta.modified().ok(),
            len: meta.len(),
        }
    }
}

#[derive(Clone)]
enum PackageProbe {
    Name(String),
    Broken,
}

#[derive(Clone)]
struct Manifest {
    stamp: Stamp,
    probe: PackageProbe,
}

struct ResolveMemo {
    aliases: Option<AliasMap>,
    root_dir: Option<PathBuf>,
    by_specifier: FxHashMap<String, Option<PathBuf>>,
}

impl ResolveMemo {
    fn matches(&self, config: ResolveConfig<'_>) -> bool {
        self.aliases.as_ref() == config.aliases
            && self.root_dir.as_deref().map(Path::as_os_str) == config.root_dir.map(Path::as_os_str)
    }
}

fn path_key(p: &Path) -> &[u8] {
    p.as_os_str().as_encoded_bytes()
}

fn read_lock<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

fn write_lock<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

static SHARED: LazyLock<MemoFs> = LazyLock::new(MemoFs::live);

impl MemoFs {
    pub fn snapshot() -> Self {
        Self::with_mode(false)
    }

    pub fn live() -> Self {
        Self::with_mode(true)
    }

    /// Process-wide live instance: an embedder swaps `&StdFs` for
    /// `MemoFs::shared()` at its call sites and nothing else changes.
    pub fn shared() -> &'static MemoFs {
        &SHARED
    }

    fn with_mode(live: bool) -> Self {
        Self {
            live,
            nearest: RwLock::default(),
            manifests: RwLock::default(),
            resolve: RwLock::default(),
            exists: RwLock::default(),
            exists_any: RwLock::default(),
            hashes: RwLock::default(),
            canon: RwLock::default(),
        }
    }

    pub fn invalidate_all(&self) {
        write_lock(&self.nearest).clear();
        write_lock(&self.manifests).clear();
        write_lock(&self.resolve).clear();
        write_lock(&self.exists).clear();
        write_lock(&self.exists_any).clear();
        write_lock(&self.hashes).clear();
        write_lock(&self.canon).clear();
    }

    /// `None` mirrors `!candidate.is_file()`; the walk keeps climbing.
    fn probe_manifest(&self, candidate: &Path) -> Option<PackageProbe> {
        let key = path_key(candidate);
        if !self.live
            && let Some(hit) = read_lock(&self.manifests).get(key)
        {
            return hit.as_ref().map(|m| m.probe.clone());
        }
        let stamp = match std::fs::metadata(candidate) {
            Ok(meta) if meta.is_file() => Stamp::of(&meta),
            _ => {
                if !self.live {
                    write_lock(&self.manifests).insert(key.into(), None);
                }
                return None;
            }
        };
        if self.live
            && let Some(Some(hit)) = read_lock(&self.manifests).get(key)
            && hit.stamp == stamp
        {
            return Some(hit.probe.clone());
        }
        let probe = read_manifest(candidate);
        write_lock(&self.manifests).insert(
            key.into(),
            Some(Manifest {
                stamp,
                probe: probe.clone(),
            }),
        );
        Some(probe)
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
}

impl FsProvider for MemoFs {
    fn nearest_package(&self, from: &Path) -> Option<(String, PathBuf)> {
        let _t = timings::start(Stage::Fs);
        let mut folder = from.parent()?;
        let mut visited: Vec<&Path> = Vec::new();
        let answer = loop {
            if !self.live {
                if let Some(hit) = read_lock(&self.nearest).get(path_key(folder)) {
                    break hit.clone();
                }
                visited.push(folder);
            }
            match self.probe_manifest(&folder.join("package.json")) {
                Some(PackageProbe::Name(name)) => break Some((name, folder.to_path_buf())),
                Some(PackageProbe::Broken) => break None,
                None => {}
            }
            if folder == Path::new("/") || folder.as_os_str().is_empty() {
                break None;
            }
            match folder.parent() {
                Some(parent) => folder = parent,
                None => break None,
            }
        };
        if !visited.is_empty() {
            let mut nearest = write_lock(&self.nearest);
            for dir in visited {
                nearest.insert(path_key(dir).into(), answer.clone());
            }
        }
        answer
    }

    fn resolve_import(
        &self,
        specifier: &str,
        importer: &Path,
        config: ResolveConfig<'_>,
    ) -> Option<PathBuf> {
        let _t = timings::start(Stage::Fs);
        let dir = match importer.parent() {
            Some(dir) if !self.live => dir,
            _ => return resolve_import_with(self, specifier, importer, config),
        };
        let key = path_key(dir);
        if let Some(memos) = read_lock(&self.resolve).get(key)
            && let Some(memo) = memos.iter().find(|m| m.matches(config))
            && let Some(hit) = memo.by_specifier.get(specifier)
        {
            return hit.clone();
        }
        let resolved = resolve_import_with(self, specifier, importer, config);
        let mut map = write_lock(&self.resolve);
        let memos = map.entry(key.into()).or_default();
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
        memo.by_specifier
            .insert(specifier.to_string(), resolved.clone());
        resolved
    }

    fn exists(&self, p: &Path) -> bool {
        if self.live {
            return p.is_file();
        }
        self.memo_path(&self.exists, p, || p.is_file())
    }

    fn exists_any(&self, p: &Path) -> bool {
        if self.live {
            return p.exists();
        }
        self.memo_path(&self.exists_any, p, || p.exists())
    }

    fn hash_file(&self, p: &Path) -> Option<String> {
        let stamp = if self.live {
            match std::fs::metadata(p) {
                Ok(meta) if meta.is_file() => Some(Stamp::of(&meta)),
                _ => return StdFs.hash_file(p),
            }
        } else {
            None
        };
        let key = path_key(p);
        if let Some((seen, hash)) = read_lock(&self.hashes).get(key)
            && (stamp.is_none() || *seen == stamp)
        {
            return hash.clone();
        }
        let hash = StdFs.hash_file(p);
        write_lock(&self.hashes).insert(key.into(), (stamp, hash.clone()));
        hash
    }

    fn canonicalize_root(&self, root: &Path) -> PathBuf {
        if self.live {
            return StdFs.canonicalize_root(root);
        }
        self.memo_path(&self.canon, root, || StdFs.canonicalize_root(root))
    }
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
        resolve_import_with(self, specifier, importer, config)
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
            return match read_manifest(&candidate) {
                PackageProbe::Name(name) => Some((name, folder.to_path_buf())),
                PackageProbe::Broken => None,
            };
        }
        if folder == Path::new("/") || folder.as_os_str().is_empty() {
            return None;
        }
        folder = folder.parent()?;
    }
}

fn read_manifest(candidate: &Path) -> PackageProbe {
    let Ok(raw) = std::fs::read_to_string(candidate) else {
        return PackageProbe::Broken;
    };
    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(json) => PackageProbe::Name(js_name_string(json.get("name"))),
        Err(_) => PackageProbe::Broken,
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
    let stripped = EXTENSIONS
        .iter()
        .find(|ext| file_path.ends_with(**ext))
        .map_or(file_path, |ext| &file_path[..file_path.len() - ext.len()]);
    let mut out = vec![file_path.to_string()];
    out.extend(EXTENSIONS.iter().map(|ext| format!("{stripped}{ext}")));
    out
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
    fs: &dyn FsProvider,
    specifier: &str,
    importer: &Path,
    config: ResolveConfig<'_>,
) -> Option<PathBuf> {
    for candidate in possible_file_paths(specifier) {
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
fn module_resolve(fs: &dyn FsProvider, specifier: &str, importer: &Path) -> Option<PathBuf> {
    let realpath = |p: PathBuf| std::fs::canonicalize(&p).unwrap_or(p);
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
fn first_existing(fs: &dyn FsProvider, path: &str) -> Option<PathBuf> {
    possible_file_paths(path)
        .into_iter()
        .map(PathBuf::from)
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

fn finalize_file(fs: &dyn FsProvider, p: PathBuf) -> Option<PathBuf> {
    if has_encoded_separator(&p) {
        return None;
    }
    let decoded = percent_decode_path(&p)?;
    fs.exists(&decoded).then_some(decoded)
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
    Parsed(PackageJson),
}

fn read_package_json(dir: &Path) -> PackageRead {
    let Ok(raw) = std::fs::read_to_string(dir.join("package.json")) else {
        return PackageRead::Missing;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return PackageRead::Malformed;
    };
    if json.is_null() {
        return PackageRead::Malformed;
    }
    let string_field = |key: &str| json.get(key).and_then(|v| v.as_str()).map(str::to_string);
    let value_field = |key: &str| json.get(key).filter(|v| !v.is_null()).cloned();
    PackageRead::Parsed(PackageJson {
        name: string_field("name"),
        main: string_field("main"),
        exports: value_field("exports"),
        imports: value_field("imports"),
    })
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

enum ScopeConfig {
    Found(PathBuf, Box<PackageJson>),
    NotFound,
    Malformed,
}

// parity: getPackageScopeConfig — nearest package.json above `from`, never
// crossing a node_modules boundary; a malformed read throws mid-walk.
fn package_scope_config(from: &Path) -> ScopeConfig {
    let Some(mut dir) = from.parent() else {
        return ScopeConfig::NotFound;
    };
    loop {
        if dir.file_name().is_some_and(|f| f == "node_modules") {
            return ScopeConfig::NotFound;
        }
        match read_package_json(dir) {
            PackageRead::Parsed(pkg) => {
                return ScopeConfig::Found(dir.to_path_buf(), Box::new(pkg));
            }
            PackageRead::Malformed => return ScopeConfig::Malformed,
            PackageRead::Missing => {}
        }
        let Some(parent) = dir.parent() else {
            return ScopeConfig::NotFound;
        };
        dir = parent;
    }
}

/// `Invalid` mirrors thrown ERR_INVALID_PACKAGE_TARGET (target arrays skip
/// past it); `Fatal` mirrors every other throw (they abort the candidate).
#[derive(Clone, Copy)]
enum ResolveErrKind {
    Invalid,
    Fatal,
}

fn package_resolve(
    fs: &dyn FsProvider,
    specifier: &str,
    importer: &Path,
) -> Result<PathBuf, ResolveErrKind> {
    let (name, subpath) = parse_package_name(specifier).ok_or(ResolveErrKind::Fatal)?;
    // ResolveSelf: the importer's own scope wins when named + exporting.
    match package_scope_config(importer) {
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
    let mut dir = importer.parent();
    while let Some(d) = dir {
        let pkg_dir = d.join("node_modules").join(name);
        if pkg_dir.is_dir() {
            let pkg = match read_package_json(&pkg_dir) {
                PackageRead::Malformed => return Err(ResolveErrKind::Fatal),
                PackageRead::Parsed(pkg) => Some(pkg),
                PackageRead::Missing => None,
            };
            if let Some(exports) = pkg.as_ref().and_then(|p| p.exports.as_ref()) {
                return package_exports_resolve(fs, &pkg_dir, &subpath, exports);
            }
            if subpath == "." {
                return legacy_main_resolve(fs, &pkg_dir, pkg.and_then(|p| p.main))
                    .ok_or(ResolveErrKind::Fatal);
            }
            let file = join_url_segments(&pkg_dir, &url_path(strip_query_fragment(&subpath[2..])));
            return finalize_file(fs, file).ok_or(ResolveErrKind::Fatal);
        }
        dir = d.parent();
    }
    Err(ResolveErrKind::Fatal)
}

// parity: legacyMainResolve — main, main+ext, main/index.*, then index.*.
fn legacy_main_resolve(
    fs: &dyn FsProvider,
    pkg_dir: &Path,
    main: Option<String>,
) -> Option<PathBuf> {
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
    fs: &dyn FsProvider,
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
    fs: &dyn FsProvider,
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
    fs: &dyn FsProvider,
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
    fs: &dyn FsProvider,
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
fn package_imports_resolve(
    fs: &dyn FsProvider,
    specifier: &str,
    importer: &Path,
) -> Option<PathBuf> {
    if specifier == "#" || specifier.starts_with("#/") || specifier.ends_with('/') {
        return None;
    }
    let ScopeConfig::Found(scope_dir, pkg) = package_scope_config(importer) else {
        return None;
    };
    let Some(serde_json::Value::Object(map)) = pkg.imports else {
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
    EXTENSIONS
        .iter()
        .any(|ext| filename.ends_with(&format!("{suffix}{ext}")))
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
    let from = normalize_path(from);
    let to = normalize_path(to);
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
    out
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
        // other/deep, other, root), each probed exactly once.
        let manifests = read_lock(&memo.manifests);
        assert!(manifests.contains_key(path_key(&root.join("pkg/package.json"))));
        assert!(manifests.contains_key(path_key(&root.join("package.json"))));
        assert_eq!(
            manifests
                .keys()
                .filter(|k| k.starts_with(path_key(&root)))
                .count(),
            7
        );
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
        let expected = StdFs.resolve_import("./tokens.stylex", &a, with_root);
        assert!(expected.is_some());
        assert_eq!(
            memo.resolve_import("./tokens.stylex", &a, with_root),
            expected
        );
        assert_eq!(
            memo.resolve_import("./tokens.stylex", &b, with_root),
            expected
        );
        assert_eq!(
            memo.resolve_import("./tokens.stylex", &b, ResolveConfig::default()),
            expected
        );
        let memos = read_lock(&memo.resolve);
        let by_dir = &memos[path_key(&root.join("pkg/src/b"))];
        assert_eq!(by_dir.len(), 2, "one memo per distinct config");
        assert!(by_dir.iter().all(|m| m.by_specifier.len() == 1));
        assert_eq!(memos.len(), 1, "one importer directory");
    }
}
