//! A filter rejects symbols that cannot occur in the merged-symbol map before probing its hash table.

use rustc_hash::FxHashMap;
use tsrs_ast::Symbol;
use tsrs_core::P;

const FILTER_WORDS: usize = 1024;

#[derive(Clone)]
pub struct MergedSymbols {
    map: FxHashMap<P<Symbol>, P<Symbol>>,
    filter: Box<[u64; FILTER_WORDS]>,
}

impl Default for MergedSymbols {
    fn default() -> Self {
        Self { map: FxHashMap::default(), filter: Box::new([0; FILTER_WORDS]) }
    }
}

impl MergedSymbols {
    #[inline]
    fn filter_position(symbol: P<Symbol>) -> (usize, u64) {
        let key = tsrs_core::ptr::key_to_usize(symbol.key());
        let hash = key ^ (key >> 16);
        ((hash >> 6) & (FILTER_WORDS - 1), 1 << (hash & 63))
    }

    #[inline]
    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "preserves the map's borrowed-key lookup API; inlined at callers"
    )]
    pub(crate) fn get(&self, symbol: &P<Symbol>) -> Option<&P<Symbol>> {
        let (word, bit) = Self::filter_position(*symbol);
        if self.filter[word] & bit == 0 {
            return None;
        }
        self.map.get(symbol)
    }

    pub(crate) fn insert(&mut self, source: P<Symbol>, target: P<Symbol>) {
        let (word, bit) = Self::filter_position(source);
        self.filter[word] |= bit;
        self.map.insert(source, target);
    }
}
