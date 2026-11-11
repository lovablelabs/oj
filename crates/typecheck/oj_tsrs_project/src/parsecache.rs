use std::sync::{Arc, Mutex};

use rustc_hash::FxHashMap;
use tsrs_core::arena::Region;

use tsrs_ast::{SourceFile, SourceFileParseOptions};
use tsrs_compiler::DuplicateSourceFile;
use tsrs_contentmapper::SourceFiles;
use tsrs_core::{Locale, P, ScriptKind, ensure_script_kind_from_file_name};

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

// ContentMappedParseCacheKey identifies the complete output bundle for one mapped input. Hash folds the
// original content, mapper transform identity, and diagnostic locale together.
// parsecache.go:39
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct ContentMappedParseCacheKey {
    // Go embeds ast.SourceFileParseOptions.
    pub source_file_parse_options: SourceFileParseOptions,
    pub hash: Hash128,
}

// parsecache.go:44 (Go writes each Uint128 as its Hi then its Lo word, little-endian; a u128's little-endian bytes
// are the Lo word's, then the Hi word's.)
pub(crate) fn content_mapped_parse_cache_key(
    options: SourceFileParseOptions,
    raw_hash: Hash128,
    transform_identity: Hash128,
    diagnostic_locale: &Locale,
) -> ContentMappedParseCacheKey {
    let locale = diagnostic_locale.string();
    let mut buf = Vec::with_capacity(32 + locale.len());
    for hash in [raw_hash, transform_identity] {
        let bytes = hash.to_le_bytes();
        buf.extend_from_slice(&bytes[8..16]);
        buf.extend_from_slice(&bytes[0..8]);
    }
    buf.extend_from_slice(locale.as_bytes());
    ContentMappedParseCacheKey {
        source_file_parse_options: options,
        hash: xxhash_rust::xxh3::xxh3_128(&buf),
    }
}

// parsecache.go:55
// parseCacheKeyForFile reconstructs the ordinary parse-cache key for a source file held by a program.
pub fn parse_cache_key_for_file(file: P<SourceFile>) -> ParseCacheKey {
    new_parse_cache_key(file.parse_options().clone(), file.hash.get(), file.script_kind())
}

// parsecache.go:59
pub fn content_mapped_parse_cache_key_for_file(file: P<SourceFile>) -> ContentMappedParseCacheKey {
    ContentMappedParseCacheKey {
        source_file_parse_options: file.content_mapper_parse_options(),
        hash: file.hash.get(),
    }
}

// parsecache.go:64
// parseCacheKeyForDuplicate reconstructs an ordinary parse-cache key for a deduplicated source file.
pub(crate) fn parse_cache_key_for_duplicate(file: &DuplicateSourceFile) -> ParseCacheKey {
    new_parse_cache_key(file.parse_options.clone(), file.hash, file.script_kind)
}

// parsecache.go:68
pub fn content_mapped_parse_cache_key_for_duplicate(
    file: &DuplicateSourceFile,
) -> ContentMappedParseCacheKey {
    ContentMappedParseCacheKey {
        source_file_parse_options: file.content_mapper_parse_options.clone(),
        hash: file.hash,
    }
}

pub type ParseCache = RefCountCache<ParseCacheKey, P<SourceFile>, Arc<dyn FileHandle>>;

/// Tracks the parse-cache references acquired by one snapshot clone, in both the parse cache and the
/// content-mapped parse cache.
///
/// `compilerHost::get_source_file` and `get_content_mapped_source_files` acquire them for the programs it builds,
/// with program-update refs and derefs in `Project::create_program`. They are handed to the new snapshot, which
/// releases them in `Snapshot::dispose`.
///
/// If the clone unwinds before a snapshot exists (a panic while building
/// a program, e.g. the module resolver's `Unexpected moduleResolution`), nothing else would release them: the
/// clone rolls them back with this record so the files' regions are freed.
#[derive(Default)]
pub struct ParseCacheJournal {
    parsed: Mutex<FxHashMap<ParseCacheKey, i64>>,
    content_mapped: Mutex<FxHashMap<ContentMappedParseCacheKey, i64>>,
}

fn journal_add<K: std::hash::Hash + Eq>(held: &Mutex<FxHashMap<K, i64>>, key: K, delta: i64) {
    *held.lock().unwrap_or_else(std::sync::PoisonError::into_inner).entry(key).or_default() +=
        delta;
}

impl ParseCacheJournal {
    pub(crate) fn acquire(
        &self,
        cache: &ParseCache,
        key: ParseCacheKey,
        fh: Arc<dyn FileHandle>,
    ) -> P<SourceFile> {
        let file = cache.acquire(&key, fh);
        journal_add(&self.parsed, key, 1);
        file
    }

    pub(crate) fn ref_(&self, cache: &ParseCache, key: ParseCacheKey) {
        cache.ref_(&key);
        journal_add(&self.parsed, key, 1);
    }

    pub(crate) fn deref(&self, cache: &ParseCache, key: &ParseCacheKey) {
        cache.deref(key);
        journal_add(&self.parsed, key.clone(), -1);
    }

    pub(crate) fn acquire_content_mapped(
        &self,
        cache: &ContentMappedParseCache,
        key: &ContentMappedParseCacheKey,
        text_len: usize,
        produce: impl FnOnce() -> Result<SourceFiles, tsrs_contentmapper::Error>,
    ) -> Result<SourceFiles, tsrs_contentmapper::Error> {
        let files = cache.acquire_or_error(key, text_len, produce)?;
        journal_add(&self.content_mapped, key.clone(), 1);
        Ok(files)
    }

    pub(crate) fn ref_content_mapped(
        &self,
        cache: &ContentMappedParseCache,
        key: ContentMappedParseCacheKey,
    ) {
        cache.ref_(&key);
        journal_add(&self.content_mapped, key, 1);
    }

    pub(crate) fn deref_content_mapped(
        &self,
        cache: &ContentMappedParseCache,
        key: &ContentMappedParseCacheKey,
    ) {
        cache.deref(key);
        journal_add(&self.content_mapped, key.clone(), -1);
    }

    /// Releases every reference this clone still holds (its programs never reached a snapshot).
    pub(crate) fn roll_back(&self, cache: &ParseCache, content_mapped: &ContentMappedParseCache) {
        let held = std::mem::take(
            &mut *self.parsed.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        #[expect(
            clippy::iter_over_hash_type,
            reason = "refcount decrements on distinct keys commute (no Go counterpart: Rust-only journal)"
        )]
        for (key, count) in held {
            for _ in 0..count.max(0) {
                cache.deref(&key);
            }
        }
        let held = std::mem::take(
            &mut *self.content_mapped.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        #[expect(
            clippy::iter_over_hash_type,
            reason = "refcount decrements on distinct keys commute (no Go counterpart: Rust-only journal)"
        )]
        for (key, count) in held {
            for _ in 0..count.max(0) {
                content_mapped.deref(&key);
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

// parsecache.go:84 (Go embeds the *RefCountCache; the methods below delegate to it.)
//
// Memory (as for the parse cache above): each transformed bundle gets its own region, which the entry owns until
// its final Deref and every program containing one of the bundle's files owns too.
pub struct ContentMappedParseCache {
    // One reference owns the canonical file and all supplemental files as a bundle. Callers ref and
    // deref the canonical file only.
    cache: RefCountCache<ContentMappedParseCacheKey, SourceFiles, ()>,
    // The region of each cached bundle, by the address of its canonical file.
    regions: Arc<Mutex<FxHashMap<usize, Region>>>,
}

// parsecache.go:90
pub fn new_content_mapped_parse_cache(options: RefCountCacheOptions) -> ContentMappedParseCache {
    let regions: Arc<Mutex<FxHashMap<usize, Region>>> = Arc::default();
    let mut cache = new_ref_count_cache(options, |_: &ContentMappedParseCacheKey, (): ()| {
        panic!("content-mapped source files must be produced with AcquireOrError")
    });
    let evict_regions = Arc::clone(&regions);
    cache.on_evict = Some(Box::new(move |files: &SourceFiles| {
        if let Some(canonical) = files.canonical {
            let region = evict_regions.lock().unwrap().remove(&canonical.addr());
            drop(region);
        }
    }));
    let revive_regions = Arc::clone(&regions);
    cache.on_revive = Some(Box::new(move |files: &SourceFiles| {
        // A live program still holds the region (it is the one re-referencing the bundle).
        if let Some(canonical) = files.canonical
            && let Some(region) = Region::containing(canonical.addr())
        {
            revive_regions.lock().unwrap().insert(canonical.addr(), region);
        }
    }));
    ContentMappedParseCache { cache, regions }
}

impl ContentMappedParseCache {
    // refcountcache.go:86 AcquireOrError. `produce` runs in the bundle's new region, sized for `text_len` bytes of
    // original text (not in Go: memory regions); a failed produce frees it.
    pub fn acquire_or_error<E>(
        &self,
        identity: &ContentMappedParseCacheKey,
        text_len: usize,
        produce: impl FnOnce() -> Result<SourceFiles, E>,
    ) -> Result<SourceFiles, E> {
        self.cache.acquire_or_error(identity, || {
            let region = Region::new(file_region_first_chunk(text_len));
            let files = {
                let _scope = region.enter();
                produce()?
            };
            region.trim();
            if let Some(canonical) = files.canonical {
                self.regions.lock().unwrap().insert(canonical.addr(), region);
            }
            Ok(files)
        })
    }

    // refcountcache.go:55
    pub fn has(&self, identity: &ContentMappedParseCacheKey) -> bool {
        self.cache.has(identity)
    }

    // refcountcache.go:105
    pub(crate) fn ref_(&self, identity: &ContentMappedParseCacheKey) {
        self.cache.ref_(identity);
    }

    // refcountcache.go:125
    pub fn deref(&self, identity: &ContentMappedParseCacheKey) {
        self.cache.deref(identity);
    }

    #[expect(clippy::len_without_is_empty, reason = "Public for the integration tests.")]
    pub fn len(&self) -> usize {
        self.cache.len()
    }
}
