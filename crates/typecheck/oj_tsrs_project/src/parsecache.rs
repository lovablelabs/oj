use std::sync::{Arc, Mutex};

use rustc_hash::FxHashMap;
use tsrs_core::arena::Region;

use tsrs_ast::{SourceFile, SourceFileParseOptions};
use tsrs_compiler::DuplicateSourceFile;
use tsrs_core::{P, ScriptKind, ensure_script_kind_from_file_name};

use crate::overlayfs::{FileHandle, Hash128};
use crate::refcountcache::{RefCountCache, RefCountCacheOptions, new_ref_count_cache};

#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct ParseCacheKey {
    // Go embeds ast.SourceFileParseOptions.
    source_file_parse_options: SourceFileParseOptions,
    script_kind: ScriptKind,
    hash: Hash128,
}

// parsecache.go:22
pub fn new_parse_cache_key(
    options: SourceFileParseOptions,
    hash: Hash128,
    mut script_kind: ScriptKind,
) -> ParseCacheKey {
    if script_kind == ScriptKind::Unknown {
        script_kind = ensure_script_kind_from_file_name(&options.file_name);
    }
    ParseCacheKey { source_file_parse_options: options, hash, script_kind }
}

// parsecache.go:55
// parseCacheKeyForFile reconstructs the ordinary parse-cache key for a source file held by a program.
pub fn parse_cache_key_for_file(file: P<SourceFile>) -> ParseCacheKey {
    new_parse_cache_key(file.parse_options().clone(), file.hash.get(), file.script_kind())
}

// parsecache.go:64
// parseCacheKeyForDuplicate reconstructs an ordinary parse-cache key for a deduplicated source file.
pub(crate) fn parse_cache_key_for_duplicate(file: &DuplicateSourceFile) -> ParseCacheKey {
    new_parse_cache_key(file.parse_options.clone(), file.hash, file.script_kind)
}

pub type ParseCache = RefCountCache<ParseCacheKey, P<SourceFile>, Arc<dyn FileHandle>>;

/// Tracks the parse-cache references acquired by one snapshot clone.
///
/// `compilerHost::get_source_file` acquires them for the programs it builds, with program-update refs and derefs
/// in `Project::create_program`. They are handed to the new snapshot, which releases them in `Snapshot::dispose`.
///
/// If the clone unwinds before a snapshot exists (a panic while building
/// a program, e.g. the module resolver's `Unexpected moduleResolution`), nothing else would release them: the
/// clone rolls them back with this record so the files' regions are freed.
#[derive(Default)]
pub struct ParseCacheJournal(Mutex<FxHashMap<ParseCacheKey, i64>>);

impl ParseCacheJournal {
    pub(crate) fn acquire(
        &self,
        cache: &ParseCache,
        key: ParseCacheKey,
        fh: Arc<dyn FileHandle>,
    ) -> P<SourceFile> {
        let file = cache.acquire(&key, fh);
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(key)
            .or_default() += 1;
        file
    }

    pub(crate) fn ref_(&self, cache: &ParseCache, key: ParseCacheKey) {
        cache.ref_(&key);
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(key)
            .or_default() += 1;
    }

    pub(crate) fn deref(&self, cache: &ParseCache, key: &ParseCacheKey) {
        cache.deref(key);
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(key.clone())
            .or_default() -= 1;
    }

    /// Releases every reference this clone still holds (its programs never reached a snapshot).
    pub(crate) fn roll_back(&self, cache: &ParseCache) {
        let held =
            std::mem::take(&mut *self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        #[expect(
            clippy::iter_over_hash_type,
            reason = "refcount decrements on distinct keys commute (no Go counterpart: Rust-only journal)"
        )]
        for (key, count) in held {
            for _ in 0..count.max(0) {
                cache.deref(&key);
            }
        }
    }
}

// parsecache.go:74
//
// Memory (docs/LSP.md "Memory plan for a long-lived server"): each parsed file version gets its own region; parse
// and bind (and later, through `arena::enter_owner`, the file's lazily filled data) allocate there. The cache entry
// owns the region until its final Deref, and every program that contains the file owns it too (`programOwner`), so
// it is freed once neither the cache nor any live program refers to the file, as Go's GC would.
pub fn new_parse_cache(options: RefCountCacheOptions) -> ParseCache {
    let regions: Arc<Mutex<FxHashMap<usize, Region>>> = Arc::default();
    let parse_regions = Arc::clone(&regions);
    let mut cache =
        new_ref_count_cache(options, move |key: &ParseCacheKey, fh: Arc<dyn FileHandle>| {
            let text = fh.content();
            let region = Region::new(file_region_first_chunk(text.len()));
            let file = {
                let _scope = region.enter();
                let file = tsrs_parser::parse_source_file(
                    key.source_file_parse_options.clone(),
                    text,
                    key.script_kind,
                );
                file.hash.set(fh.hash());
                tsrs_binder::bind_source_file(file);
                file
            };
            region.trim();
            let text_index = file.text_index.get();
            region.on_free(Box::new(move || {
                tsrs_ast::unregister_source_text(text_index);
            }));
            parse_regions.lock().unwrap().insert(file.addr(), region);
            file
        });
    let evict_regions = Arc::clone(&regions);
    cache.on_evict = Some(Box::new(move |file: &P<SourceFile>| {
        let region = evict_regions.lock().unwrap().remove(&file.addr());
        drop(region);
    }));
    cache.on_revive = Some(Box::new(move |file: &P<SourceFile>| {
        // A live program still holds the region (it is the one re-referencing the file).
        if let Some(region) = Region::containing(file.addr()) {
            regions.lock().unwrap().insert(file.addr(), region);
        }
    }));
    cache
}

// The text is copied into the region; text, AST and binder data take about 8 times the text (measured on the
// private monorepo: 7.8x on average). Regions grow by a quarter when this is exceeded, and the region is trimmed to
// what it used after binding.
fn file_region_first_chunk(text_len: usize) -> usize {
    text_len.saturating_mul(10).saturating_add(4 << 10).min(64 << 20)
}
