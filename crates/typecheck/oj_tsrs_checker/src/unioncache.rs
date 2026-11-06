//! The union front cache (`TSRS_UNION_CACHE`, on by default, off under Go-compatible history; docs/DEBUGGING.md,
//! notes/perf-union-inference.md).
//!
//! Most `getUnionType` calls build a union of a few types that was built before. Without the cache every such call
//! flattens, sorts (`compareTypes` is structural), reduces literals, builds and hashes the type-list key and looks it up
//! in `unionTypes` (or `unionOfUnionTypes`). The cache is a direct-mapped table (2^10 slots of 40 bytes per checker)
//! in front of all of that. The key is the input handles in input order, the reduction mode and, with an alias, the
//! alias symbol and type arguments (what `getAliasKey` hashes), at most `MAX_WORDS` words; calls with an origin, or
//! with a longer key, go the normal way.
//!
//! Exactness: an entry is stored only from a call that
//! - created no type, or only the union it returns (`type_count` grew by one and the result has the last id): a fresh
//!   origin for named unions, or anything a relation or a resolution made, keeps the call out of the cache, so the
//!   cache never changes the number or the ids of the types created;
//! - instantiated nothing (`instantiation_count` and `total_instantiation_count` unchanged), so the TS2589 budget sees
//!   the same counts with or without the cache;
//! - did not take one of the two reductions whose answer reads state that can still change (string literals matched by
//!   template literals: relations and inference; constrained type variables: base constraints, which read circularity
//!   state), counted by `impure`;
//! - and did not return `errorType`: TS2590 "too complex to represent" reports at `currentNode` and caches nothing,
//!   so every such call must run again (an `any` input that is an error type returns it too and is not stored either).
//!
//! Such a call is a function of its inputs and of caches that only grow (`unionTypes`, `unionOfUnionTypes`,
//! `subtypeReductionCache`): the same call made again without the cache takes the same path through the same entries,
//! finds the union this call interned, and returns the same object, creating and instantiating nothing. So a hit
//! returns exactly what the uncached call would, including the non-union answers (one distinct member, `never`,
//! `any`/`unknown` absorption) and subtype reduction (whose second call is a `subtypeReductionCache` hit). Types are
//! never freed while their checker lives, so a handle in a key is never reused for another type.
//!
//! `TSRS_UNION_CACHE=shadow` computes the uncached answer at every hit as well and panics if it is a different object.
//! `TSRS_UNION_CACHE=0` turns the cache off, `=1` forces it on under Go-compatible history too.
//! `TSRS_UNION_CACHE_STATS=1` prints the totals (lookups, hits, stores, why misses were not stored, calls that bypass
//! the cache) on stderr at exit; `TSRS_UNION_CACHE_BITS=<n>` sets the table size (default 10; larger tables cost RSS,
//! notes/perf-union-inference.md).

use crate::*;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use tsrs_core::ptr::PKey;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnionCacheMode {
    Off,
    On,
    Shadow,
}

fn env_mode() -> Option<UnionCacheMode> {
    static MODE: OnceLock<Option<UnionCacheMode>> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("TSRS_UNION_CACHE").as_deref() {
        Ok("0" | "off") => Some(UnionCacheMode::Off),
        Ok("1" | "on") => Some(UnionCacheMode::On),
        Ok("shadow") => Some(UnionCacheMode::Shadow),
        _ => None,
    })
}

/// The mode of the indexed-access memo (`TSRS_IA_MEMO`, `checker_13.rs`
/// `get_indexed_access_type_by_union_memoized`): on unless turned off or under Go-compatible history, like
/// the union front cache.
pub fn indexed_access_memo_mode() -> UnionCacheMode {
    static MODE: OnceLock<UnionCacheMode> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("TSRS_IA_MEMO").as_deref() {
        Ok("0" | "off") => UnionCacheMode::Off,
        Ok("1" | "on") => UnionCacheMode::On,
        Ok("shadow") => UnionCacheMode::Shadow,
        _ if tsrs_core::compat::go_compatible_history() => UnionCacheMode::Off,
        _ => UnionCacheMode::On,
    })
}

/// The mode for a checker created now: the environment's; by default on, except under Go-compatible history.
pub fn union_cache_mode() -> UnionCacheMode {
    match env_mode() {
        Some(mode) => mode,
        None if tsrs_core::compat::go_compatible_history() => UnionCacheMode::Off,
        None => UnionCacheMode::On,
    }
}

fn stats_on() -> bool {
    static STATS: OnceLock<bool> = OnceLock::new();
    *STATS.get_or_init(|| {
        std::env::var("TSRS_UNION_CACHE_STATS").is_ok_and(|v| !v.is_empty() && v != "0")
    })
}

static LOOKUPS: AtomicU64 = AtomicU64::new(0);
static HITS: AtomicU64 = AtomicU64::new(0);
static STORES: AtomicU64 = AtomicU64::new(0);
static SHADOW_CHECKS: AtomicU64 = AtomicU64::new(0);
static SKIP_CREATED: AtomicU64 = AtomicU64::new(0);
static SKIP_INSTANTIATED: AtomicU64 = AtomicU64::new(0);
static SKIP_IMPURE: AtomicU64 = AtomicU64::new(0);
static SKIP_ERROR: AtomicU64 = AtomicU64::new(0);

fn bits() -> u32 {
    static BITS: OnceLock<u32> = OnceLock::new();
    *BITS.get_or_init(|| {
        std::env::var("TSRS_UNION_CACHE_BITS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|b| (4..=24).contains(b))
            .unwrap_or(10)
    })
}
/// Key words: the inputs, then for an alias its symbol and type arguments.
pub const MAX_WORDS: usize = 8;

#[derive(Clone, Copy, Default)]
struct Slot {
    /// Input handles in input order, then the alias's symbol and type arguments; unused words are 0 (never a handle).
    key: [PKey; MAX_WORDS],
    /// 0: empty; else 1 | reduction << 1 | input count << 3 | has alias << 7.
    meta: u16,
    result: Option<P<Type>>,
}

pub struct UnionFrontCache {
    pub(crate) mode: UnionCacheMode,
    /// Calls that took a reduction whose answer can change (see the module comment); stores are skipped across them.
    pub(crate) impure: u32,
    slots: Vec<Slot>,
    /// 64 - log2 of the table size.
    shift: u32,
    /// Count lookups, hits, stores and shadow checks into the process totals (stats or shadow mode).
    stats: bool,
    /// Calls whose key is longer than `MAX_WORDS` (`LongKeys`).
    long: LongKeys,
}

/// The table for keys longer than `MAX_WORDS`: same key words, same store rule, but an exact hash table instead of a
/// direct-mapped one. Large unions (an indexed access over a union of hundreds of keys, say) are built again from the
/// same inputs far more often than small ones on application code: on the 38k-file codebase 94% of the calls whose
/// flattened list has eight or more ascending runs repeat an earlier call of the same checker, and merging those runs
/// is most of what remains of `compareTypes`.
#[derive(Default)]
struct LongKeys {
    table: hashbrown::HashTable<u32>,
    entries: Vec<LongEntry>,
    /// The key words of every entry, back to back.
    words: Vec<PKey>,
    /// The key of the call being looked up.
    probe: Vec<PKey>,
}

struct LongEntry {
    hash: u64,
    start: u32,
    len: u32,
    meta: u16,
    result: P<Type>,
}

impl UnionFrontCache {
    pub(crate) fn new() -> Self {
        let mode = union_cache_mode();
        UnionFrontCache {
            mode,
            impure: 0,
            slots: Vec::new(),
            shift: 64 - bits(),
            stats: mode != UnionCacheMode::Off && (stats_on() || mode == UnionCacheMode::Shadow),
            long: LongKeys::default(),
        }
    }
}

static BYPASS_ORIGIN: AtomicU64 = AtomicU64::new(0);
static BYPASS_SIZE: AtomicU64 = AtomicU64::new(0);

/// Stats mode: a 2+ input call with an origin (`origin`) or a key longer than `MAX_WORDS`.
#[inline]
pub fn count_bypass_origin(cache: &UnionFrontCache) {
    count(cache.stats, &BYPASS_ORIGIN);
}

#[inline]
fn count_bypass(cache: &UnionFrontCache) {
    count(cache.stats, &BYPASS_SIZE);
}

fn total(counter: &AtomicU64) -> u64 {
    // Relaxed: read at exit, after the checker threads that counted were joined.
    counter.load(Ordering::Relaxed)
}

#[inline]
fn count(on: bool, counter: &AtomicU64) {
    if on {
        // Relaxed: a statistic, read once at exit after the checker threads are joined.
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// The key of a call, or None when it does not fit in `MAX_WORDS`.
#[inline(always)]
#[expect(
    clippy::inline_always,
    reason = "on the getUnionType path; out of line the key is built in memory and copied"
)]
fn make_key(
    types: &[P<Type>],
    reduction: UnionReduction,
    alias: AliasArg<'_>,
    shift: u32,
) -> Option<([PKey; MAX_WORDS], u16, usize)> {
    let (symbol, type_arguments): (Option<P<Symbol>>, &[P<Type>]) = match alias {
        AliasArg::None => (None, &[]),
        AliasArg::Some(alias) => (alias.symbol.get(), alias.type_arguments.get()),
        AliasArg::Pending(pending) => (pending.symbol, pending.type_arguments.as_slice()),
    };
    let has_alias = !alias.is_none();
    if types.len() + usize::from(has_alias) + type_arguments.len() > MAX_WORDS {
        return None;
    }
    let mut key = [0 as PKey; MAX_WORDS];
    let mut h: u64 = reduction as u64;
    let mut n = 0;
    let mut push = |k: PKey| {
        key[n] = k;
        n += 1;
        h = (h ^ k as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    };
    for t in types {
        push(t.key());
    }
    if has_alias {
        push(symbol?.key());
        for t in type_arguments {
            push(t.key());
        }
    }
    h ^= h >> 29;
    let meta =
        1 | ((reduction as u16) << 1) | ((types.len() as u16) << 3) | (u16::from(has_alias) << 7);
    Some((key, meta, (h >> shift) as usize))
}

/// The key words of a call that does not fit `make_key` (inputs, then the alias's symbol and type arguments), its meta
/// and its hash, or None for an alias without a symbol (not cached, as in `make_key`).
fn make_long_key(
    words: &mut Vec<PKey>,
    types: &[P<Type>],
    reduction: UnionReduction,
    alias: AliasArg<'_>,
) -> Option<(u16, u64)> {
    let (symbol, type_arguments): (Option<P<Symbol>>, &[P<Type>]) = match alias {
        AliasArg::None => (None, &[]),
        AliasArg::Some(alias) => (alias.symbol.get(), alias.type_arguments.get()),
        AliasArg::Pending(pending) => (pending.symbol, pending.type_arguments.as_slice()),
    };
    let has_alias = !alias.is_none();
    words.clear();
    words.extend(types.iter().map(|t| t.key()));
    if has_alias {
        words.push(symbol?.key());
        words.extend(type_arguments.iter().map(|t| t.key()));
    }
    let mut h: u64 = reduction as u64 | ((types.len() as u64) << 2);
    h = words.iter().fold(h, |mut h, &k| {
        h = (h ^ k as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);

        h
    });
    h ^= h >> 29;
    Some((reduction as u16 | (u16::from(has_alias) << 2), h))
}

impl Checker {
    /// `get_union_type_front_cached` for keys longer than `MAX_WORDS`.
    fn get_union_type_long_cached(
        &mut self,
        types: &[P<Type>],
        union_reduction: UnionReduction,
        alias: AliasArg<'_>,
    ) -> P<Type> {
        let mut probe = std::mem::take(&mut self.union_front_cache.long.probe);
        let Some((meta, hash)) = make_long_key(&mut probe, types, union_reduction, alias) else {
            self.union_front_cache.long.probe = probe;
            count_bypass(&self.union_front_cache);
            return self.get_union_type_ex_uncached(types, union_reduction, alias, None);
        };
        let cache = &mut self.union_front_cache;
        count(cache.stats, &LOOKUPS);
        let found = {
            let long = &cache.long;
            long.table
                .find(hash, |&i| {
                    let e = &long.entries[i as usize];
                    e.hash == hash
                        && e.meta == meta
                        && long.words[e.start as usize..(e.start + e.len) as usize] == probe[..]
                })
                .map(|&i| long.entries[i as usize].result)
        };
        if let Some(cached) = found {
            count(cache.stats, &HITS);
            cache.long.probe = probe;
            if cache.mode == UnionCacheMode::Shadow {
                count(cache.stats, &SHADOW_CHECKS);
                let fresh = self.get_union_type_ex_uncached(types, union_reduction, alias, None);
                assert!(
                    fresh == cached,
                    "TSRS_UNION_CACHE=shadow: union of {} types ({union_reduction:?}, alias {}): cached type {}, fresh type {}",
                    types.len(),
                    !alias.is_none(),
                    cached.id.0,
                    fresh.id.0
                );
            }
            return cached;
        }
        let type_count = self.type_count;
        let instantiation_count = self.instantiation_count;
        let total_instantiation_count = self.total_instantiation_count;
        let impure = self.union_front_cache.impure;
        let result = self.get_union_type_ex_uncached(types, union_reduction, alias, None);
        let created = type_count != self.type_count
            && !(self.type_count == type_count + 1 && result.id.0 == self.type_count);
        let instantiated = instantiation_count != self.instantiation_count
            || total_instantiation_count != self.total_instantiation_count;
        let impure = impure != self.union_front_cache.impure;
        let error = result == self.error_type;
        let cache = &mut self.union_front_cache;
        if !(created || instantiated || impure || error) {
            count(cache.stats, &STORES);
            let long = &mut cache.long;
            let index = long.entries.len() as u32;
            long.entries.push(LongEntry {
                hash,
                start: long.words.len() as u32,
                len: probe.len() as u32,
                meta,
                result,
            });
            long.words.extend_from_slice(&probe);
            let entries = &long.entries;
            long.table.insert_unique(hash, index, |&i| entries[i as usize].hash);
        } else if cache.stats {
            count(created, &SKIP_CREATED);
            count(instantiated, &SKIP_INSTANTIATED);
            count(impure, &SKIP_IMPURE);
            count(error, &SKIP_ERROR);
        }
        cache.long.probe = probe;
        result
    }

    /// The front of `getUnionType` for calls with two or more inputs and no origin.
    pub(crate) fn get_union_type_front_cached(
        &mut self,
        types: &[P<Type>],
        union_reduction: UnionReduction,
        alias: AliasArg<'_>,
    ) -> P<Type> {
        let Some((key, meta, index)) =
            make_key(types, union_reduction, alias, self.union_front_cache.shift)
        else {
            return self.get_union_type_long_cached(types, union_reduction, alias);
        };
        let cache = &mut self.union_front_cache;
        if cache.slots.is_empty() {
            cache.slots = vec![Slot::default(); 1 << (64 - cache.shift)];
        }
        count(cache.stats, &LOOKUPS);
        let slot = cache.slots[index];
        if slot.meta == meta && slot.key == key {
            count(cache.stats, &HITS);
            let cached = slot.result.unwrap();
            if cache.mode == UnionCacheMode::Shadow {
                count(cache.stats, &SHADOW_CHECKS);
                let fresh = self.get_union_type_ex_uncached(types, union_reduction, alias, None);
                assert!(
                    fresh == cached,
                    "TSRS_UNION_CACHE=shadow: union of [{}] ({union_reduction:?}, alias {}): cached type {} (flags {:?}), fresh type {} (flags {:?})",
                    types.iter().map(|t| t.id.0.to_string()).collect::<Vec<_>>().join(", "),
                    !alias.is_none(),
                    cached.id.0,
                    cached.flags(),
                    fresh.id.0,
                    fresh.flags()
                );
            }
            return cached;
        }
        let type_count = self.type_count;
        let instantiation_count = self.instantiation_count;
        let total_instantiation_count = self.total_instantiation_count;
        let impure = self.union_front_cache.impure;
        let result = self.get_union_type_ex_uncached(types, union_reduction, alias, None);
        // Nothing created, or only the result (the union this call interned, which the next call finds).
        let created = type_count != self.type_count
            && !(self.type_count == type_count + 1 && result.id.0 == self.type_count);
        let instantiated = instantiation_count != self.instantiation_count
            || total_instantiation_count != self.total_instantiation_count;
        let impure = impure != self.union_front_cache.impure;
        let error = result == self.error_type;
        let cache = &mut self.union_front_cache;
        if !(created || instantiated || impure || error) {
            count(cache.stats, &STORES);
            cache.slots[index] = Slot { key, meta, result: Some(result) };
        } else if cache.stats {
            count(created, &SKIP_CREATED);
            count(instantiated, &SKIP_INSTANTIATED);
            count(impure, &SKIP_IMPURE);
            count(error, &SKIP_ERROR);
        }
        result
    }

    /// Prints the process totals when `TSRS_UNION_CACHE_STATS` is set or in shadow mode (call at exit).
    pub fn union_cache_finish() {
        if !stats_on() && env_mode() != Some(UnionCacheMode::Shadow) {
            return;
        }
        let lookups = total(&LOOKUPS);
        let hits = total(&HITS);
        eprintln!(
            "tsrs: union front cache: {lookups} lookups, {hits} hits ({:.1}%), {} stores, {} shadow checks; misses not stored: {} created a type, {} instantiated, {} impure reductions, {} error type; bypassed: {} with an origin, {} keys longer than {MAX_WORDS} words",
            if lookups == 0 { 0.0 } else { 100.0 * hits as f64 / lookups as f64 },
            total(&STORES),
            total(&SHADOW_CHECKS),
            total(&SKIP_CREATED),
            total(&SKIP_INSTANTIATED),
            total(&SKIP_IMPURE),
            total(&SKIP_ERROR),
            total(&BYPASS_ORIGIN),
            total(&BYPASS_SIZE)
        );
    }
}
