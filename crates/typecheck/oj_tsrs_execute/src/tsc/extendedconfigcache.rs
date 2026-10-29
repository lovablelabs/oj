use std::sync::Mutex;

use rustc_hash::FxHashMap;
use tsrs_core::P;
use tsrs_core::tspath::Path;
use tsrs_tsoptions::{self as tsoptions, ExtendedConfigCacheEntry, ParseConfigHost};

// extendedConfigCache is a minimal implementation of tsoptions.ExtendedConfigCache.
// It stores cached entries permanently.
#[derive(Default)]
pub struct ExtendedConfigCache {
    m: Mutex<FxHashMap<Path, P<ExtendedConfigCacheEntry>>>,
}

impl tsoptions::ExtendedConfigCache for ExtendedConfigCache {
    fn get_extended_config(
        &self,
        file_name: &str,
        path: &Path,
        resolution_stack: &[Path],
        host: &dyn ParseConfigHost,
    ) -> P<ExtendedConfigCacheEntry> {
        if let Some(entry) = self.m.lock().unwrap().get(path) {
            return *entry;
        }
        // Parsing may recurse into this cache for nested `extends`, so it runs without the lock held.
        let entry = tsoptions::parse_extended_config(
            file_name,
            path.clone(),
            resolution_stack,
            host,
            Some(self),
        );
        *self.m.lock().unwrap().entry(path.clone()).or_insert(entry)
    }
}
