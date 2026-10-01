use super::*;

// Rough fixed cost of one cache entry beyond its module payload: the MemoryEntry
// struct, the HashMap bucket, and the String headers for the url + key.
pub(crate) const MEMORY_ENTRY_OVERHEAD: usize = 160;

pub(crate) struct MemoryEntry {
    key: String,
    module: Arc<CachedModule>,
    bytes: usize,
    seq: u64,
}

pub(crate) struct MemoryCache {
    map: HashMap<String, MemoryEntry>,
    total: usize,
    budget: usize,
    seq: u64,
}

impl MemoryCache {
    pub(crate) fn new(budget: usize) -> Self {
        MemoryCache {
            map: HashMap::new(),
            total: 0,
            budget,
            seq: 0,
        }
    }

    fn get(&mut self, url: &str, key: &str) -> Option<Arc<CachedModule>> {
        self.seq += 1;
        let seq = self.seq;
        let entry = self.map.get_mut(url)?;
        if entry.key != key {
            return None;
        }
        entry.seq = seq;
        Some(Arc::clone(&entry.module))
    }

    pub(crate) fn remove(&mut self, url: &str) {
        if let Some(old) = self.map.remove(url) {
            self.total -= old.bytes;
        }
    }

    pub(crate) fn clear(&mut self) {
        self.map.clear();
        self.total = 0;
    }

    /// (entries, accounted bytes, code bytes, map bytes): the footprint
    /// split behind /@oj/debug/mem.
    pub(crate) fn stats(&self) -> (usize, usize, usize, usize) {
        let mut code = 0;
        let mut map = 0;
        for e in self.map.values() {
            code += e.module.code.len();
            map += e.module.map_json.as_ref().map_or(0, String::len);
        }
        (self.map.len(), self.total, code, map)
    }

    fn put(&mut self, url: &str, key: &str, module: &Arc<CachedModule>) {
        let bytes = module_weight(module) + url.len() + key.len() + MEMORY_ENTRY_OVERHEAD;
        self.seq += 1;
        let seq = self.seq;
        let entry = MemoryEntry {
            key: key.to_string(),
            module: Arc::clone(module),
            bytes,
            seq,
        };
        if let Some(old) = self.map.insert(url.to_string(), entry) {
            self.total -= old.bytes;
        }
        self.total += bytes;
        self.evict();
    }

    // Evict in one sorted pass down to a low-water mark (90% of budget) so the
    // hot get()/put() paths stay cheap and eviction runs rarely, not per-put.
    fn evict(&mut self) {
        if self.total <= self.budget {
            return;
        }
        let low = self.budget - self.budget / 10;
        let mut order: Vec<(u64, String)> = self
            .map
            .iter()
            .map(|(url, e)| (e.seq, url.clone()))
            .collect();
        order.sort_unstable_by_key(|(seq, _)| *seq);
        for (_, url) in order {
            if self.total <= low || self.map.len() <= 1 {
                break;
            }
            if let Some(e) = self.map.remove(&url) {
                self.total -= e.bytes;
            }
        }
    }
}

pub(crate) fn module_weight(module: &CachedModule) -> usize {
    fn strs(v: &[String]) -> usize {
        v.iter()
            .map(|s| s.len() + std::mem::size_of::<String>())
            .sum::<usize>()
    }
    fn pairs(v: &[(String, String)]) -> usize {
        v.iter()
            .map(|(a, b)| a.len() + b.len() + 2 * std::mem::size_of::<String>())
            .sum::<usize>()
    }
    module.code.len()
        + module.map_json.as_ref().map_or(0, String::len)
        + module.kind.len()
        + strs(&module.imports)
        + strs(&module.fs_allow)
        + strs(&module.watch_files)
        + pairs(&module.require_map)
        + pairs(&module.css_exports)
        + module
            .import_bindings
            .iter()
            .map(|(s, names)| s.len() + std::mem::size_of::<String>() + strs(names))
            .sum::<usize>()
        + module.hot.as_ref().map_or(0, |h| {
            strs(&h.deps) + h.accepted_exports.as_deref().map_or(0, strs)
        })
}

pub(crate) fn memory_cache_budget() -> usize {
    if let Some(mb) = std::env::var("OJ_MEMORY_CACHE_MB")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        return if mb == 0 {
            usize::MAX
        } else {
            mb.saturating_mul(1024 * 1024)
        };
    }
    // No explicit budget: scale to the container memory limit (density) if one is
    // visible, else a sane fixed default for a dev machine.
    let ceil = 256 * 1024 * 1024;
    let floor = 32 * 1024 * 1024;
    match detect_memory_limit() {
        Some(limit) => (limit / 8).clamp(floor, ceil),
        None => 128 * 1024 * 1024,
    }
}

// The cgroup memory ceiling (v2 then v1); None off-container or when unlimited.
pub(crate) fn detect_memory_limit() -> Option<usize> {
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/memory.max") {
        let t = s.trim();
        if t != "max" {
            if let Ok(v) = t.parse::<u64>() {
                return Some(v as usize);
            }
        }
    }
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes") {
        if let Ok(v) = s.trim().parse::<u64>() {
            if v < (1u64 << 62) {
                return Some(v as usize);
            }
        }
    }
    None
}

pub(crate) fn memory_get(state: &ServerState, url: &str, key: &str) -> Option<Arc<CachedModule>> {
    state.memory.lock().unwrap().get(url, key)
}

pub(crate) fn memory_put(state: &ServerState, url: &str, key: &str, module: &Arc<CachedModule>) {
    state.memory.lock().unwrap().put(url, key, module);
}
