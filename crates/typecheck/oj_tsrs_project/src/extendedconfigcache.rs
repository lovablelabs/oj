use std::sync::Arc;

use tsrs_core::P;
use tsrs_core::tspath::Path;
use tsrs_tsoptions::{self as tsoptions, ParseConfigHost};

use crate::overlayfs::Hash128;
use crate::ownercache::{LoadArgs, OwnerCache, new_owner_cache};
use crate::snapshotfs::FileSource;

pub struct ExtendedConfigParseArgs<'a> {
    pub(crate) file_name: String,
    pub(crate) content: String,
    pub(crate) fs: Arc<dyn FileSource>,
    pub(crate) resolution_stack: Vec<Path>,
    // Borrowed by the one load that uses them; the parse retains neither (issue #646).
    pub(crate) host: &'a dyn ParseConfigHost,
    pub(crate) cache: &'a dyn tsoptions::ExtendedConfigCache,
}

// The loader argument family of `ExtendedConfigCache` (`ownercache::LoadArgs`): each load borrows its own hosts.
pub struct ExtendedConfigLoadArgs;

impl LoadArgs for ExtendedConfigLoadArgs {
    type Args<'a> = ExtendedConfigParseArgs<'a>;
}

pub struct ExtendedConfigCacheEntry {
    // Go embeds *tsoptions.ExtendedConfigCacheEntry.
    pub(crate) extended_config_cache_entry: P<tsoptions::ExtendedConfigCacheEntry>,
    hash: Hash128,
}

pub type ExtendedConfigCache =
    OwnerCache<Path, Arc<ExtendedConfigCacheEntry>, ExtendedConfigLoadArgs>;

// extendedconfigcache.go:25
pub(crate) fn new_extended_config_cache() -> ExtendedConfigCache {
    new_owner_cache(
        |path: &Path, args: &ExtendedConfigParseArgs<'_>| {
            let extended_config_cache_entry = tsoptions::parse_extended_config(
                &args.file_name,
                path.clone(),
                &args.resolution_stack,
                args.host,
                Some(args.cache),
            );
            let hash = hash(&extended_config_cache_entry, args);
            Arc::new(ExtendedConfigCacheEntry { extended_config_cache_entry, hash })
        },
        Some(Box::new(
            |_path: &Path,
             entry: &Arc<ExtendedConfigCacheEntry>,
             args: &ExtendedConfigParseArgs<'_>| {
                entry.hash == 0 || entry.hash != hash(&entry.extended_config_cache_entry, args)
            },
        )),
    )
}

// extendedconfigcache.go:40
fn hash(
    entry: &tsoptions::ExtendedConfigCacheEntry,
    args: &ExtendedConfigParseArgs<'_>,
) -> Hash128 {
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    hasher.update(args.content.as_bytes());
    for file_name in entry.extended_file_names() {
        let Some(fh) = args.fs.get_file(&file_name) else {
            return 0;
        };
        hasher.update(fh.content().as_bytes());
    }
    hasher.digest128()
}
