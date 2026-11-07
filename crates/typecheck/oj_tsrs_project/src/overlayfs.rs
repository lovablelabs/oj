use std::sync::{Arc, OnceLock, RwLock};
use std::time::SystemTime;

use rustc_hash::FxHashMap;
use tsrs_core::tspath::{self, Path};
use tsrs_core::{ScriptKind, TextChange, compute_ecma_line_starts, get_script_kind_from_file_name};
use tsrs_ls::lsconv::{self, LSPLineMap, Script};
use tsrs_ls::sourcemap::{ECMALineInfo, create_ecma_line_info};
use tsrs_ls::spanmap::{self, SpanMap};
use tsrs_lsproto as lsproto;
use tsrs_vfs::{Entries, FS, FileInfo, FileMode};

use crate::filechange::{FileChange, FileChangeExpander, FileChangeKind, FileChangeSummary};
use crate::snapshotfs::FileHandleSource;

// Go `xxh3.Uint128`; the zero value is 0.
pub type Hash128 = u128;

// Go `xxh3.HashString128`.
pub(crate) fn hash_string_128(s: &str) -> Hash128 {
    xxhash_rust::xxh3::xxh3_128(s.as_bytes())
}

pub type ToPath = Arc<dyn Fn(&str) -> Path + Send + Sync>;

// Go `map[tspath.Path]*Overlay`, shared between snapshots.
pub type OverlayMap = Arc<FxHashMap<Path, Arc<Overlay>>>;

pub trait FileContent {
    fn content(&self) -> &str;
    fn hash(&self) -> Hash128;
}

// Go `FileHandle` interface values are `Arc<dyn FileHandle>`.
pub trait FileHandle: FileContent + Send + Sync {
    fn file_name(&self) -> &str;
    fn version(&self) -> i32;
    fn matches_disk_text(&self) -> bool;
    fn is_overlay(&self) -> bool;
    fn lsp_line_map(&self) -> Arc<LSPLineMap>;
    fn ecma_line_info(&self) -> Arc<ECMALineInfo>;
    fn kind(&self) -> ScriptKind;
}

#[derive(Clone, Default)]
pub struct FileBase {
    pub(crate) file_name: String,
    pub(crate) content: String,
    pub(crate) hash: Hash128,

    line_map: OnceLock<Arc<LSPLineMap>>,
    line_info: OnceLock<Arc<ECMALineInfo>>,
}

impl FileBase {
    pub(crate) fn new(file_name: String, content: String, hash: Hash128) -> FileBase {
        FileBase { file_name, content, hash, line_map: OnceLock::new(), line_info: OnceLock::new() }
    }

    // overlayfs.go:49
    fn file_name(&self) -> &str {
        &self.file_name
    }

    // overlayfs.go:61
    fn lsp_line_map(&self) -> Arc<LSPLineMap> {
        Arc::clone(self.line_map.get_or_init(|| lsconv::compute_lsp_line_starts(&self.content)))
    }

    // overlayfs.go:68
    fn ecma_line_info(&self) -> Arc<ECMALineInfo> {
        Arc::clone(self.line_info.get_or_init(|| {
            let line_starts = compute_ecma_line_starts(&self.content);
            create_ecma_line_info(self.content.clone(), line_starts)
        }))
    }
}

// A field-by-field copy (used when a shared value must be copied before mutation); Go's `Clone` is
// `Cloneable::clone_value`.
#[derive(Clone, Default)]
pub struct CachedFile {
    pub(crate) base: FileBase,
    pub(crate) needs_reload: bool,
    pub(crate) realpath_path: Path,
}

// overlayfs.go:82
pub(crate) fn new_cached_file(file_name: String, content: String) -> CachedFile {
    let hash = hash_string_128(&content);
    CachedFile {
        base: FileBase::new(file_name, content, hash),
        needs_reload: false,
        realpath_path: Path::default(),
    }
}

// overlayfs.go:90
pub fn new_cached_file_handle(file_name: &str, content: &str) -> Arc<dyn FileHandle> {
    Arc::new(new_cached_file(file_name.to_string(), content.to_string()))
}

impl FileContent for CachedFile {
    // overlayfs.go:57
    fn content(&self) -> &str {
        &self.base.content
    }

    // overlayfs.go:53
    fn hash(&self) -> Hash128 {
        self.base.hash
    }
}

impl FileHandle for CachedFile {
    fn file_name(&self) -> &str {
        self.base.file_name()
    }

    // overlayfs.go:96
    fn version(&self) -> i32 {
        0
    }

    // overlayfs.go:100
    fn matches_disk_text(&self) -> bool {
        !self.needs_reload
    }

    // overlayfs.go:104
    fn is_overlay(&self) -> bool {
        false
    }

    fn lsp_line_map(&self) -> Arc<LSPLineMap> {
        self.base.lsp_line_map()
    }

    fn ecma_line_info(&self) -> Arc<ECMALineInfo> {
        self.base.ecma_line_info()
    }

    // overlayfs.go:108
    fn kind(&self) -> ScriptKind {
        get_script_kind_from_file_name(&self.base.file_name)
    }
}

impl crate::dirty::Cloneable for CachedFile {
    // overlayfs.go:112
    fn clone_value(&self) -> CachedFile {
        CachedFile {
            realpath_path: self.realpath_path.clone(),
            base: FileBase::new(
                self.base.file_name.clone(),
                self.base.content.clone(),
                self.base.hash,
            ),
            needs_reload: false,
        }
    }
}

pub struct Overlay {
    pub(crate) base: FileBase,
    version: i32,
    kind: ScriptKind,
    matches_disk_text: bool,
}

// overlayfs.go:130
pub fn new_overlay(file_name: String, content: String, version: i32, kind: ScriptKind) -> Overlay {
    let hash = hash_string_128(&content);
    Overlay {
        base: FileBase::new(file_name, content, hash),
        version,
        kind,
        matches_disk_text: false,
    }
}

impl Overlay {
    // overlayfs.go:144
    fn text(&self) -> &str {
        &self.base.content
    }

    // overlayfs.go:148
    fn original_file_name(&self) -> &str {
        self.base.file_name()
    }

    // overlayfs.go:153
    // SpanMap and OriginalText satisfy lsconv.Script. An overlay holds the editor's raw text (for a
    // content-mapped file, that is the original foreign text, not the transformed output), so it never
    // carries a span map and its original text is its own text.
    #[expect(
        clippy::unused_self,
        reason = "Span mapping is an optional property of the overlay instance, matching the host interface."
    )]
    fn span_map(&self) -> Option<&SpanMap> {
        None
    }

    // overlayfs.go:155
    fn original_text(&self) -> &str {
        &self.base.content
    }

    // overlayfs.go:163
    // !!! optimization: incorporate mtime
    fn compute_matches_disk_text(&self, fs: &dyn FS) -> (bool, bool) {
        if tspath::is_dynamic_file_name(&self.base.file_name) {
            return (false, false);
        }
        let Some(disk_content) = fs.read_file(&self.base.file_name) else {
            return (false, false);
        };
        (hash_string_128(&disk_content) == self.base.hash, true)
    }
}

impl FileContent for Overlay {
    fn content(&self) -> &str {
        &self.base.content
    }

    fn hash(&self) -> Hash128 {
        self.base.hash
    }
}

impl FileHandle for Overlay {
    fn file_name(&self) -> &str {
        self.base.file_name()
    }

    // overlayfs.go:140
    fn version(&self) -> i32 {
        self.version
    }

    // overlayfs.go:158
    // MatchesDiskText may return false negatives, but never false positives.
    fn matches_disk_text(&self) -> bool {
        self.matches_disk_text
    }

    // overlayfs.go:174
    fn is_overlay(&self) -> bool {
        true
    }

    fn lsp_line_map(&self) -> Arc<LSPLineMap> {
        self.base.lsp_line_map()
    }

    fn ecma_line_info(&self) -> Arc<ECMALineInfo> {
        self.base.ecma_line_info()
    }

    // overlayfs.go:178
    fn kind(&self) -> ScriptKind {
        self.kind
    }
}

impl Script for Overlay {
    fn file_name(&self) -> &str {
        self.base.file_name()
    }
    fn original_file_name(&self) -> &str {
        Overlay::original_file_name(self)
    }
    fn text(&self) -> &str {
        Overlay::text(self)
    }
    fn span_map(&self) -> Option<&SpanMap> {
        Overlay::span_map(self)
    }
    fn original_text(&self) -> &str {
        Overlay::original_text(self)
    }
}

impl Script for &Overlay {
    fn file_name(&self) -> &str {
        self.base.file_name()
    }
    fn original_file_name(&self) -> &str {
        Overlay::original_file_name(self)
    }
    fn text(&self) -> &str {
        Overlay::text(self)
    }
    fn span_map(&self) -> Option<&SpanMap> {
        Overlay::span_map(self)
    }
    fn original_text(&self) -> &str {
        Overlay::original_text(self)
    }
}

// Go `vfs.FS` values that `layerOverlayFileSystem` and `overlayFS` inspect with type assertions
// (`fileSystem.(*overlayFS)`, `fs.host.(FileHandleSource)`): a plain file system, or one of this package's
// layered file systems. (`RebasableFileSystem` file systems come only from API requests: tsrs_api's
// `RequestFileSystem`.)
#[derive(Clone)]
pub enum FsRef {
    Host(Arc<dyn FS>),
    Layered(Arc<dyn LayeredFileSystem>),
}

impl FsRef {
    fn fs(&self) -> &dyn FS {
        match self {
            FsRef::Host(fs) => &**fs,
            FsRef::Layered(fs) => &**fs,
        }
    }

    fn file_handle_source(&self) -> Option<&dyn FileHandleSource> {
        match self {
            FsRef::Host(_) => None,
            FsRef::Layered(fs) => Some(&**fs),
        }
    }
}

pub struct OverlayFS {
    to_path: ToPath,
    host: FsRef,
    position_encoding: lsproto::PositionEncodingKind,

    mu: RwLock<OverlayFSState>,
}

struct OverlayFSState {
    overlays: OverlayMap,
    overlay_directories: FxHashMap<Path, FxHashMap<Path, String>>,
}

pub trait LayeredFileSystem: FS + FileHandleSource {
    fn overlays(&self) -> OverlayMap;

    // Go's `fs.(*overlayFS)` type assertion.
    fn as_overlay_fs(&self) -> Option<&OverlayFS> {
        None
    }

    // Go's `fs.(FileChangeExpander)` type assertion.
    fn as_file_change_expander(&self) -> Option<&dyn FileChangeExpander> {
        None
    }

    // Go's `fileSystem.(RebasableFileSystem)` type assertion (API request file systems).
    fn as_rebasable(&self) -> Option<&dyn RebasableFileSystem> {
        None
    }
}

// overlayfs.go:198 RebasableFileSystem: a layered file system (an API request file system) that can be moved
// onto a new base, so the snapshot's overlays sit *under* it (`layerOverlayFileSystem`).
pub trait RebasableFileSystem {
    fn base_file_system(&self) -> FsRef;
    fn with_base_file_system(&self, base: FsRef) -> Arc<dyn LayeredFileSystem>;
}

// overlayfs.go:204
pub fn new_overlay_fs(
    fs: FsRef,
    overlays: OverlayMap,
    position_encoding: lsproto::PositionEncodingKind,
    to_path: ToPath,
) -> OverlayFS {
    let overlay_directories = create_overlay_directories(&overlays);
    OverlayFS {
        host: fs,
        position_encoding,
        mu: RwLock::new(OverlayFSState { overlays, overlay_directories }),
        to_path,
    }
}

impl LayeredFileSystem for OverlayFS {
    // overlayfs.go:220
    fn overlays(&self) -> OverlayMap {
        Arc::clone(&self.mu.read().unwrap().overlays)
    }

    fn as_overlay_fs(&self) -> Option<&OverlayFS> {
        Some(self)
    }
}

// overlayfs.go:226
pub(crate) fn layer_overlay_file_system(
    file_system: &FsRef,
    overlays: OverlayMap,
    position_encoding: lsproto::PositionEncodingKind,
    to_path: ToPath,
) -> Arc<dyn LayeredFileSystem> {
    let mut base = file_system.clone();
    let mut layer: Option<&dyn RebasableFileSystem> = None;
    if let FsRef::Layered(layered) = &file_system
        && let Some(rebasable) = layered.as_rebasable()
    {
        layer = Some(rebasable);
        base = rebasable.base_file_system();
    }
    if let FsRef::Layered(layered) = &base
        && let Some(previous) = layered.as_overlay_fs()
    {
        let host = previous.host.clone();
        base = host;
    }
    let overlay: Arc<dyn LayeredFileSystem> =
        Arc::new(new_overlay_fs(base, overlays, position_encoding, to_path));
    match layer {
        None => overlay,
        Some(layer) => layer.with_base_file_system(FsRef::Layered(overlay)),
    }
}

impl FileHandleSource for OverlayFS {
    // overlayfs.go:243
    fn get_file(&self, file_name: &str) -> Option<Arc<dyn FileHandle>> {
        self.get_file_by_path(file_name, &(self.to_path)(file_name))
    }

    // overlayfs.go:247
    fn get_file_by_path(&self, file_name: &str, path: &Path) -> Option<Arc<dyn FileHandle>> {
        let (overlay, directory) = {
            let st = self.mu.read().unwrap();
            (st.overlays.get(path).cloned(), st.overlay_directories.contains_key(path))
        };
        if let Some(overlay) = overlay {
            return Some(overlay);
        }
        if directory {
            return None;
        }

        if let Some(source) = self.host.file_handle_source() {
            return source.get_file_by_path(file_name, path);
        }
        let content = self.host.fs().read_file(file_name)?;
        Some(Arc::new(new_cached_file(file_name.to_string(), content)))
    }
}

impl FS for OverlayFS {
    // overlayfs.go:269
    fn use_case_sensitive_file_names(&self) -> bool {
        self.host.fs().use_case_sensitive_file_names()
    }

    // overlayfs.go:271
    fn file_exists(&self, file_name: &str) -> bool {
        let (file, directory) = {
            let st = self.mu.read().unwrap();
            let path = (self.to_path)(file_name);
            (st.overlays.contains_key(&path), st.overlay_directories.contains_key(&path))
        };
        file || !directory && self.host.fs().file_exists(file_name)
    }

    // overlayfs.go:280
    fn read_file(&self, file_name: &str) -> Option<String> {
        self.get_file(file_name).map(|file| file.content().to_string())
    }

    // overlayfs.go:287
    fn write_file(&self, path: &str, data: &str) -> Result<(), String> {
        self.host.fs().write_file(path, data)
    }

    // overlayfs.go:289
    fn append_file(&self, path: &str, data: &str) -> Result<(), String> {
        self.host.fs().append_file(path, data)
    }

    // overlayfs.go:292
    fn remove(&self, path: &str) -> Result<(), String> {
        self.host.fs().remove(path)
    }

    // overlayfs.go:293
    fn chtimes(&self, path: &str, a_time: SystemTime, m_time: SystemTime) -> Result<(), String> {
        self.host.fs().chtimes(path, a_time, m_time)
    }

    // overlayfs.go:297
    fn directory_exists(&self, directory_name: &str) -> bool {
        let (file, directory) = {
            let st = self.mu.read().unwrap();
            let path = (self.to_path)(directory_name);
            (st.overlays.contains_key(&path), st.overlay_directories.contains_key(&path))
        };
        directory || !file && self.host.fs().directory_exists(directory_name)
    }

    // overlayfs.go:306
    fn get_accessible_entries(&self, directory_name: &str) -> Entries {
        let (file, directory, overlays) = {
            let st = self.mu.read().unwrap();
            let path = (self.to_path)(directory_name);
            (
                st.overlays.contains_key(&path),
                st.overlay_directories.get(&path).cloned(),
                Arc::clone(&st.overlays),
            )
        };
        if file {
            return Entries::default();
        }
        let host_entries = self.host.fs().get_accessible_entries(directory_name);
        let mut entries = Entries {
            files: host_entries.files.clone(),
            directories: host_entries.directories.clone(),
            symlinks: host_entries.symlinks,
        };
        let use_case_sensitive_file_names = self.use_case_sensitive_file_names();
        let equal_name = |left: &str, right: &str| {
            tspath::get_canonical_file_name(left, use_case_sensitive_file_names)
                == tspath::get_canonical_file_name(right, use_case_sensitive_file_names)
        };
        if let Some(directory) = directory {
            #[expect(
                clippy::iter_over_hash_type,
                reason = "Go ranges the overlay directory map too, so overlay-only children are appended in map order there as well"
            )]
            for (child_path, child_name) in &directory {
                entries.files.retain(|name| !equal_name(name, child_name));
                entries.directories.retain(|name| !equal_name(name, child_name));
                if let Some(symlinks) = &mut entries.symlinks {
                    symlinks.retain(|name| !equal_name(name, child_name));
                }
                if overlays.contains_key(child_path) {
                    entries.files.push(child_name.clone());
                } else {
                    entries.directories.push(child_name.clone());
                }
            }
        }
        entries
    }

    // overlayfs.go:345
    fn stat(&self, path: &str) -> Option<FileInfo> {
        let (overlay, directory) = {
            let st = self.mu.read().unwrap();
            let canonical_path = (self.to_path)(path);
            (
                st.overlays.get(&canonical_path).cloned(),
                st.overlay_directories.contains_key(&canonical_path),
            )
        };
        if let Some(overlay) = overlay {
            return Some(overlay_file_info(&overlay));
        }
        if directory {
            return Some(overlay_directory_info(&tspath::get_base_file_name(path)));
        }
        self.host.fs().stat(path)
    }

    // overlayfs.go:360
    fn realpath(&self, path: &str) -> String {
        self.host.fs().realpath(path)
    }
}

// overlayfs.go:362 (Go `overlayFileInfo`: name, size, mode 0o444, zero mtime)
fn overlay_file_info(overlay: &Overlay) -> FileInfo {
    FileInfo {
        name: tspath::get_base_file_name(overlay.base.file_name()),
        size: overlay.content().len() as i64,
        mode: FileMode::from_bits_retain(0o444),
        mod_time: None,
    }
}

// overlayfs.go:373 (Go `overlayDirectoryInfo`: mode ModeDir | 0o555)
fn overlay_directory_info(name: &str) -> FileInfo {
    FileInfo {
        name: name.to_string(),
        size: 0,
        mode: FileMode::Dir | FileMode::from_bits_retain(0o555),
        mod_time: None,
    }
}

// overlayfs.go:384
fn create_overlay_directories(
    overlays: &FxHashMap<Path, Arc<Overlay>>,
) -> FxHashMap<Path, FxHashMap<Path, String>> {
    let mut overlay_directories: FxHashMap<Path, FxHashMap<Path, String>> = FxHashMap::default();
    #[expect(
        clippy::iter_over_hash_type,
        reason = "builds nested maps keyed by path; Go ranges the map too"
    )]
    for (path, overlay) in overlays {
        let mut child_path = path.clone();
        let mut child = overlay.base.file_name().to_string();
        loop {
            let parent_path = child_path.get_directory_path();
            let parent = tspath::get_directory_path(&child);
            if child_path == parent_path {
                break;
            }
            overlay_directories
                .entry(parent_path.clone())
                .or_default()
                .insert(child_path.clone(), tspath::get_base_file_name(&child));
            child_path = parent_path;
            child = parent;
        }
    }
    overlay_directories
}

// Reduced collection of changes that occurred on a single file
#[derive(Default)]
struct FileEvents {
    open_change: Option<FileChange>,
    close_change: Option<FileChange>,
    watch_changed: bool,
    changes: Vec<FileChange>,
    saved: bool,
    created: bool,
    deleted: bool,
}

impl OverlayFS {
    // overlayfs.go:407
    pub fn process_changes(&self, changes: &[FileChange]) -> (FileChangeSummary, OverlayMap) {
        let mut st = self.mu.write().unwrap();

        let mut result = FileChangeSummary::default();
        let mut new_overlays: FxHashMap<Path, Arc<Overlay>> = (*st.overlays).clone();

        let mut file_event_map: FxHashMap<lsproto::DocumentUri, FileEvents> = FxHashMap::default();
        let mut order: Vec<lsproto::DocumentUri> = Vec::new();

        for change in changes {
            let uri = change.uri.clone();
            let events = if let Some(events) = file_event_map.get_mut(&uri) {
                assert!(events.open_change.is_none(), "should see no changes after open");
                events
            } else {
                order.push(uri.clone());
                file_event_map.entry(uri.clone()).or_default()
            };

            if !result.includes_watch_change_outside_node_modules
                && change.kind.is_watch_kind()
                && !uri.0.contains("/node_modules/")
            {
                result.includes_watch_change_outside_node_modules = true;
            }

            match change.kind {
                FileChangeKind::Open => {
                    if events.close_change.is_some() {
                        events.close_change = None;
                    }
                    events.open_change = Some(change.clone());
                    events.watch_changed = false;
                    events.changes = Vec::new();
                    events.saved = false;
                    events.created = false;
                    events.deleted = false;
                }
                FileChangeKind::Close => {
                    events.close_change = Some(change.clone());
                    events.changes = Vec::new();
                    events.saved = false;
                    events.watch_changed = false;
                }
                FileChangeKind::Change => {
                    // tsrs-only: Go asserts that no change follows a close in the batch, so a client that edits a
                    // document it closed (a protocol error) ended the session. The change is dropped, as is the change
                    // of a document that is not open (it has no overlay below).
                    if events.close_change.is_some() {
                        continue;
                    }
                    events.changes.push(change.clone());
                    events.saved = false;
                    events.watch_changed = false;
                }
                FileChangeKind::Save => {
                    events.saved = true;
                }
                FileChangeKind::WatchCreate => {
                    if events.deleted {
                        // Delete followed by create becomes a change
                        events.deleted = false;
                        events.watch_changed = true;
                    } else {
                        events.created = true;
                    }
                }
                FileChangeKind::WatchChange => {
                    if !events.created {
                        events.watch_changed = true;
                        events.saved = false;
                    }
                }
                FileChangeKind::WatchDelete => {
                    events.watch_changed = false;
                    events.saved = false;
                    // Delete after create cancels out
                    if events.created {
                        events.created = false;
                    } else {
                        events.deleted = true;
                    }
                }
            }
        }

        // Process deduplicated events per file (Go iterates its map in random order; this is first-seen order)
        for uri in &order {
            let events = &file_event_map[uri];
            let path = uri.path(self.host.fs().use_case_sensitive_file_names());
            let mut o = new_overlays.get(&path).cloned();

            if let Some(open_change) = &events.open_change {
                assert!(
                    !(!result.opened.0.is_empty() || !result.reopened.0.is_empty()),
                    "can only process one file open event at a time"
                );
                match &o {
                    Some(existing) if existing.content() != open_change.content => {
                        result.changed.add(uri.clone());
                    }
                    None => result.opened = uri.clone(),
                    Some(_) => result.reopened = uri.clone(),
                }
                let mut script_kind =
                    lsconv::language_kind_to_script_kind(open_change.language_kind);
                if script_kind == ScriptKind::Unknown {
                    script_kind = get_script_kind_from_file_name(&uri.file_name());
                }
                new_overlays.insert(
                    path,
                    Arc::new(new_overlay(
                        uri.file_name(),
                        open_change.content.clone(),
                        open_change.version,
                        script_kind,
                    )),
                );
                continue;
            }

            if events.close_change.is_some() && o.is_some() {
                result.closed.add(uri.clone());
                new_overlays.remove(&path);
                o = None;
            }

            if events.watch_changed {
                match &o {
                    None => result.changed.add(uri.clone()),
                    Some(existing) if !events.saved => {
                        let (matches_disk_text, _) =
                            existing.compute_matches_disk_text(self.host.fs());
                        if matches_disk_text != existing.matches_disk_text() {
                            let mut updated = new_overlay(
                                existing.base.file_name.clone(),
                                existing.content().to_string(),
                                existing.version(),
                                existing.kind,
                            );
                            updated.matches_disk_text = matches_disk_text;
                            let updated = Arc::new(updated);
                            new_overlays.insert(path.clone(), Arc::clone(&updated));
                            o = Some(updated);
                        }
                    }
                    Some(_) => {}
                }
            }

            if !events.changes.is_empty() && o.is_some() {
                result.changed.add(uri.clone());
                for change in &events.changes {
                    let mut current = o.clone().unwrap();
                    for text_change in &change.changes {
                        if let Some(partial_change) = &text_change.partial {
                            let line_map = current.lsp_line_map();
                            let converters = lsconv::new_converters(
                                self.position_encoding,
                                move |_file_name: &str| Some(Arc::clone(&line_map)),
                            );
                            let ranges = converters.from_lsp_range(
                                &*current,
                                partial_change.range,
                                spanmap::Feature::All,
                            );
                            assert!(
                                ranges.len() == 1,
                                "expected exactly one range for partial change"
                            );
                            let text_change = TextChange {
                                text_range: ranges[0].span,
                                new_text: partial_change.text.clone(),
                            };
                            let new_content = text_change.apply_to(&current.base.content);
                            let next = new_overlay(
                                current.base.file_name.clone(),
                                new_content,
                                change.version,
                                current.kind,
                            );
                            current = Arc::new(next);
                        } else if let Some(whole_change) = &text_change.whole_document {
                            current = Arc::new(new_overlay(
                                current.base.file_name.clone(),
                                whole_change.text.clone(),
                                change.version,
                                current.kind,
                            ));
                        }
                    }
                    // Go then sets version, hash and matchesDiskText on that overlay in place.
                    if !change.changes.is_empty() {
                        let mut next = new_overlay(
                            current.base.file_name.clone(),
                            current.base.content.clone(),
                            change.version,
                            current.kind,
                        );
                        next.base.hash = hash_string_128(&next.base.content);
                        next.matches_disk_text = false;
                        current = Arc::new(next);
                        new_overlays.insert(path.clone(), Arc::clone(&current));
                    }
                    o = Some(current);
                }
            }

            if events.saved {
                if let Some(existing) = &o {
                    let mut updated = new_overlay(
                        existing.base.file_name.clone(),
                        existing.content().to_string(),
                        existing.version(),
                        existing.kind,
                    );
                    updated.matches_disk_text = true;
                    let updated = Arc::new(updated);
                    new_overlays.insert(path.clone(), Arc::clone(&updated));
                    o = Some(updated);
                } else if !events.watch_changed {
                    // File was saved but never opened via didOpen; treat as a disk change.
                    result.changed.add(uri.clone());
                }
            }

            if events.created && o.is_none() {
                result.created.add(uri.clone());
            }

            if events.deleted && o.is_none() {
                result.deleted.add(uri.clone());
            }
        }

        let new_overlays: OverlayMap = Arc::new(new_overlays);
        st.overlay_directories = create_overlay_directories(&new_overlays);
        st.overlays = Arc::clone(&new_overlays);
        (result, new_overlays)
    }
}
