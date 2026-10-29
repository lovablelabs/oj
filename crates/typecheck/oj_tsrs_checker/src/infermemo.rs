//! The inference walk memo (`TSRS_INFER_MEMO`, on by default, off under Go-compatible history; docs/DEBUGGING.md,
//! notes/perf-heavy-files-infer-memo.md).
//!
//! `inferTypes` walks a source type against a target type and adds candidates to the inference infos of the type
//! parameters it reaches. Some walks are long and are made again and again with the same inputs. Inferring from a
//! contextual type that is a union of N object types to a return type `Extract<Union, { type: K }>` (a union of N
//! deferred conditional types) takes about 4·N² `inferFromTypes` steps, and a generic call checked M times against
//! the same contextual type repeats it 2·M times (the return type, then the return mapper's context); the infos start
//! empty each time, so every repeat ends in the same candidates. Go has no cache for it.
//!
//! The memo maps the inputs of a top-level walk to its outcome. A walk starts from a fresh `InferenceState` (empty
//! `visited`, no propagation type, not bivariant), so its inputs are the source, the target, the priority, the
//! contravariance, and for each inference info (at most `MAX_INFOS`) what the walk reads from it: the type parameter,
//! the candidate and contravariant candidate lists, the priority, `topLevel`, `isFixed` and the implied arity. Its
//! outcome is what it writes: each info's two lists, priority and `topLevel`, and whether it cleared the infos'
//! cached inferred types (`clearCachedInferences`; the walk never reads them). A walk is stored only when
//! - it created no type, symbol or signature, instantiated nothing and left the instantiation counters where they
//!   were, added no diagnostic and took no impure union reduction (`union_front_cache.impure`): it read only values
//!   that were already computed, and computed nothing new (a mapper that reads the infos' inferred types runs only
//!   inside an instantiation, so such a walk is never stored);
//! - it checked no expression if it started with the instantiation count at 0 (`checkExpression` resets the count,
//!   and a reset from 0 to 0 cannot be seen in it);
//! - it did not start inside an instantiation: there it could take instantiations from the active mappers' caches
//!   without counting them, and the same walk made later outside it would count them (the flow memo's frames treat
//!   this the same way). A hit inside an instantiation is fine: the stored walk made no instantiation that was not
//!   already cached for good, so neither does the walk it stands for;
//! - it read no transient state: the flow memo's taint frame (flowmemo.rs) around it saw no in-progress or circular
//!   resolution older than the walk, no `resolvingSignature` and no `flowTypeCache` entry;
//! - the language service is not blocking inference from some nodes (`skipDirectInferenceNodes` is empty);
//! - it took at least `MIN_STEPS` `inferFromTypes` steps (shorter walks cost about as much as the memo), and an
//!   earlier walk to the same target did too: the first long walk to a target only marks the target
//!   (`long_targets`), and only walks to marked targets are keyed, looked up and measured, so the many short walks
//!   pay one set lookup.
//!
//! Such a walk read only finished values (caches that only grow, types that never change) besides its inputs, so the
//! same inputs walked again take the same path and write the same outcome. A hit writes the outcome without the walk,
//! and replays what the walk reported to the enclosing flow memo frame (the height of the flow sub-walks inside it,
//! their flags, plus `FLAG_COUNT_RESET` when it did flow work: that flag depends on the count at the time and only
//! restricts the flow memo), so the flow memo sees what it would have seen or more. Types are never freed while their checker lives, so a
//! handle in a key or an outcome never names another type.
//!
//! One kind of creation is not stable: lazy members (notes/mem-lazy.md) instantiate a declared member symbol when a
//! path first needs it, and a relation's path can depend on whether the other side's members are resolved yet, so a
//! walk made later may instantiate member symbols the stored walk did not need (any later caller would instantiate
//! them as well; when they are created is already history-dependent under lazy members, and with
//! `TSRS_LAZY_MEMBERS=0` the difference does not exist).
//!
//! `TSRS_INFER_MEMO=shadow` also walks every hit (from the same starting state) and panics unless that walk creates
//! nothing but such member symbols, has no other effects and ends in the stored outcome. `TSRS_INFER_MEMO=0` turns the memo off, `=1` forces it on under
//! Go-compatible history too. `TSRS_INFER_MEMO_STATS=1` prints the totals on stderr at exit.
//! `TSRS_INFER_MEMO_MIN_STEPS=<n>` sets the shortest walk that is stored (default `MIN_STEPS`).

use crate::*;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InferMemoMode {
    Off,
    On,
    Shadow,
}

fn env_mode() -> Option<InferMemoMode> {
    static MODE: OnceLock<Option<InferMemoMode>> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("TSRS_INFER_MEMO").as_deref() {
        Ok("0" | "off") => Some(InferMemoMode::Off),
        Ok("1" | "on") => Some(InferMemoMode::On),
        Ok("shadow") => Some(InferMemoMode::Shadow),
        _ => None,
    })
}

/// The mode for a checker created now: the environment's; by default on, except under Go-compatible history.
fn infer_memo_mode() -> InferMemoMode {
    match env_mode() {
        Some(mode) => mode,
        None if tsrs_core::compat::go_compatible_history() => InferMemoMode::Off,
        None => InferMemoMode::On,
    }
}

fn stats_on() -> bool {
    static STATS: OnceLock<bool> = OnceLock::new();
    *STATS.get_or_init(|| {
        std::env::var("TSRS_INFER_MEMO_STATS").is_ok_and(|v| !v.is_empty() && v != "0")
    })
}

/// Calls with more inference infos than this are not memoized.
const MAX_INFOS: usize = 8;
/// Walks with fewer `inferFromTypes` steps are not stored (`TSRS_INFER_MEMO_MIN_STEPS` overrides it); 16 measured best
/// of 16, 32 and 64 on every bench project (notes/perf-heavy-files-infer-memo.md).
const MIN_STEPS: u32 = 16;
fn min_steps() -> u32 {
    static V: OnceLock<u32> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("TSRS_INFER_MEMO_MIN_STEPS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(MIN_STEPS)
    })
}

static LOOKUPS: AtomicU64 = AtomicU64::new(0);
static HITS: AtomicU64 = AtomicU64::new(0);
static STORES: AtomicU64 = AtomicU64::new(0);
static SKIPPED_STEPS: AtomicU64 = AtomicU64::new(0);
static NOT_STORED_EFFECTS: AtomicU64 = AtomicU64::new(0);
static NOT_STORED_CREATED: AtomicU64 = AtomicU64::new(0);
static NOT_STORED_INSTANTIATED: AtomicU64 = AtomicU64::new(0);
static NOT_STORED_TAINT: AtomicU64 = AtomicU64::new(0);

/// What a walk wrote to one inference info.
#[derive(PartialEq, Eq, Debug)]
struct InfoOutcome {
    candidates: Box<[P<Type>]>,
    contra_candidates: Box<[P<Type>]>,
    priority: InferencePriority,
    top_level: bool,
}

/// The outcome of a stored walk and what it reported to the enclosing flow memo frame.
struct Entry {
    infos: Box<[InfoOutcome]>,
    cleared: bool,
    height: u16,
    flags: u8,
    steps: u32,
}

pub struct InferMemo {
    mode: InferMemoMode,
    entries: FxHashMap<Box<[u32]>, Entry>,
    /// Targets of walks that took at least `min_steps()` steps: only walks to these are keyed, looked up and measured.
    long_targets: FxHashSet<P<Type>>,
    /// Key buffers, one per nesting level of memoized walks.
    key_pool: Vec<Vec<u32>>,
    /// `inferFromTypes` calls so far (wrapping).
    pub(crate) steps: u32,
    stats: bool,
}

impl InferMemo {
    pub(crate) fn new() -> Self {
        let mode = infer_memo_mode();
        InferMemo {
            mode,
            entries: FxHashMap::default(),
            long_targets: FxHashSet::default(),
            key_pool: Vec::new(),
            steps: 0,
            stats: mode != InferMemoMode::Off && (stats_on() || mode == InferMemoMode::Shadow),
        }
    }
}

#[inline]
fn count(on: bool, counter: &AtomicU64, n: u64) {
    if on {
        // Relaxed: a statistic, read once at exit after the checker threads are joined.
        counter.fetch_add(n, Ordering::Relaxed);
    }
}

fn total(counter: &AtomicU64) -> u64 {
    // Relaxed: read at exit, after the checker threads that counted were joined.
    counter.load(Ordering::Relaxed)
}

/// The checker state a walk must leave alone to be stored, measured around it.
#[derive(PartialEq, Eq, Debug)]
struct Effects {
    created: u32,
    instantiation_count: u32,
    total_instantiation_count: u32,
    diagnostics: u32,
    impure: u32,
}

fn info_outcomes(n: P<InferenceState>) -> Box<[InfoOutcome]> {
    n.inferences
        .borrow()
        .iter()
        .map(|i| InfoOutcome {
            candidates: i.candidates.to_vec().into_boxed_slice(),
            contra_candidates: i.contra_candidates.to_vec().into_boxed_slice(),
            priority: i.priority.get(),
            top_level: i.top_level.get(),
        })
        .collect()
}

impl Checker {
    fn infer_memo_effects(&self) -> Effects {
        Effects {
            created: self
                .type_count
                .wrapping_add(self.symbol_count)
                .wrapping_add(self.signature_count),
            instantiation_count: self.instantiation_count,
            total_instantiation_count: self.total_instantiation_count,
            diagnostics: self.diagnostic_adds,
            impure: self.union_front_cache.impure,
        }
    }

    /// `inferFromTypes(n, source, target)` for the walk at the top of `inferTypes`, through the memo.
    #[expect(
        clippy::set_contains_or_insert,
        reason = "a target is marked only after the walk to it turned out long"
    )]
    pub(crate) fn infer_from_types_memo(
        &mut self,
        n: P<InferenceState>,
        source: P<Type>,
        target: P<Type>,
    ) {
        if self.infer_memo.mode == InferMemoMode::Off {
            self.infer_from_types(n, source, target);
            return;
        }
        if !self.infer_memo.long_targets.contains(&target) {
            // The first long walk to a target only marks it: most walks are short, and keying and measuring them
            // would cost more than the memo saves.
            let steps_before = self.infer_memo.steps;
            self.infer_from_types(n, source, target);
            if self.infer_memo.steps.wrapping_sub(steps_before) >= min_steps() {
                self.infer_memo.long_targets.insert(target);
            }
            return;
        }
        let mut key = self.infer_memo.key_pool.pop().unwrap_or_default();
        if !self.infer_memo_key(&mut key, n, source, target) {
            self.infer_memo.key_pool.push(key);
            self.infer_from_types(n, source, target);
            return;
        }
        let stats = self.infer_memo.stats;
        count(stats, &LOOKUPS, 1);
        if let Some(entry) = self.infer_memo.entries.get(key.as_slice()) {
            count(stats, &HITS, 1);
            count(stats, &SKIPPED_STEPS, u64::from(entry.steps));
            let (height, flags) = (entry.height, entry.flags);
            if self.infer_memo.mode == InferMemoMode::Shadow {
                self.infer_memo_shadow(n, source, target, &key);
            } else {
                let inferences = n.inferences.borrow();
                for (info, outcome) in inferences.iter().zip(entry.infos.iter()) {
                    info.candidates.clear();
                    for &t in &outcome.candidates {
                        info.candidates.push(t);
                    }
                    info.contra_candidates.clear();
                    for &t in &outcome.contra_candidates {
                        info.contra_candidates.push(t);
                    }
                    info.priority.set(outcome.priority);
                    info.top_level.set(outcome.top_level);
                }
                if entry.cleared {
                    clear_cached_inferences(&inferences);
                }
            }
            self.infer_memo.key_pool.push(key);
            self.flow_memo.raise_height(height);
            self.flow_memo.add_flags(flags);
            return;
        }
        // A reset of the instantiation count (`checkExpression`) at 0 cannot be seen in the count; the same walk made
        // later at another count would reset it.
        let count_zero = self.instantiation_count == 0;
        let expression_checks = self.expression_checks;
        let serial_before = self.flow_memo.source_now();
        n.cleared_inferences.set(false);
        // Inside an instantiation the walk can take instantiations from the active mappers' caches without counting
        // them; walked again outside it, it would count them (as for flow memo frames, FLAG_EFFECTS).
        let in_instantiation = !self.active_mappers.is_empty();
        let before = self.infer_memo_effects();
        let steps_before = self.infer_memo.steps;
        let frame = self.flow_frame_begin();
        self.infer_from_types(n, source, target);
        let (taint, height, flags) = self.flow_memo.end_transparent(frame);
        let steps = self.infer_memo.steps.wrapping_sub(steps_before);
        let after = self.infer_memo_effects();
        let hidden_reset = count_zero && self.expression_checks != expression_checks;
        let effects = in_instantiation
            || hidden_reset
            || after != before
            || flags & (crate::flowmemo::FLAG_EFFECTS | crate::flowmemo::FLAG_TYPE_CACHE) != 0;
        let tainted = !taint.is_pure() || !self.skip_direct_inference_nodes.is_empty();
        if steps < min_steps() || effects || tainted {
            count(
                stats && steps >= min_steps() && after.created != before.created,
                &NOT_STORED_CREATED,
                1,
            );
            count(
                stats
                    && steps >= min_steps()
                    && after.total_instantiation_count != before.total_instantiation_count,
                &NOT_STORED_INSTANTIATED,
                1,
            );
            count(stats && steps >= min_steps() && effects, &NOT_STORED_EFFECTS, 1);
            count(stats && steps >= min_steps() && tainted, &NOT_STORED_TAINT, 1);
            self.infer_memo.key_pool.push(key);
            return;
        }
        count(stats, &STORES, 1);
        // Flow frames inside the walk add FLAG_COUNT_RESET when the count is 0 at their start, which depends on when
        // the walk is made, not on its inputs; the flag only restricts the flow memo, so a hit replays it whenever the
        // walk did flow work.
        let flowed = self.flow_memo.source_now() != serial_before;
        let flags = if flowed { flags | crate::flowmemo::FLAG_COUNT_RESET } else { flags };
        let entry = Entry {
            infos: info_outcomes(n),
            cleared: n.cleared_inferences.get(),
            height,
            flags,
            steps,
        };
        self.infer_memo.entries.insert(key.into_boxed_slice(), entry);
    }

    /// Builds the walk's key in `key`; false when the call is not memoized.
    fn infer_memo_key(
        &self,
        key: &mut Vec<u32>,
        n: P<InferenceState>,
        source: P<Type>,
        target: P<Type>,
    ) -> bool {
        if !self.skip_direct_inference_nodes.is_empty() {
            return false;
        }
        let inferences = n.inferences.borrow();
        if inferences.len() > MAX_INFOS {
            return false;
        }
        key.clear();
        key.extend_from_slice(&[
            source.id.0,
            target.id.0,
            n.priority.get().memo_bits(),
            u32::from(n.contravariant.get()),
            inferences.len() as u32,
        ]);
        for info in inferences.iter() {
            key.extend_from_slice(&[
                info.type_parameter.get().map_or(0, |t| t.id.0),
                info.priority.get().memo_bits(),
                u32::from(info.top_level.get()) | (u32::from(info.is_fixed.get()) << 1),
                info.implied_arity.get().map_or(u32::MAX, |arity| arity as u32),
            ]);
            for list in [&info.candidates, &info.contra_candidates] {
                list.with_slice(|types| {
                    key.push(types.len() as u32);
                    key.extend(types.iter().map(|t| t.id.0));
                });
            }
        }
        true
    }

    /// Shadow mode: walks a hit for real and panics unless the walk has no effects and ends in the stored outcome.
    #[cold]
    fn infer_memo_shadow(
        &mut self,
        n: P<InferenceState>,
        source: P<Type>,
        target: P<Type>,
        key: &[u32],
    ) {
        n.cleared_inferences.set(false);
        let before = self.infer_memo_effects();
        let lazy_before = self.lazy_member_stats.member_table_declared_instantiated;
        let frame = self.flow_frame_begin();
        self.infer_from_types(n, source, target);
        let (taint, height, flags) = self.flow_memo.end_transparent(frame);
        let after = self.infer_memo_effects();
        // Lazy members (notes/mem-lazy.md) instantiate a declared member symbol when a path first needs it, and which
        // path a relation takes can depend on whether the other side's members are resolved yet: the walk made again
        // may instantiate member symbols the stored walk did not need, as any later caller could. Those are the only
        // creations it may make.
        let lazy_created =
            (self.lazy_member_stats.member_table_declared_instantiated - lazy_before) as u32;
        let effects =
            Effects { created: before.created.wrapping_add(lazy_created), ..before } != after;
        let outcome = info_outcomes(n);
        let entry = &self.infer_memo.entries[key];
        let flags_covered = flags & !entry.flags == 0
            && (flags ^ entry.flags) & !crate::flowmemo::FLAG_COUNT_RESET == 0;
        assert!(
            !(effects
                || !taint.is_pure()
                || outcome != entry.infos
                || n.cleared_inferences.get() != entry.cleared
                || height != entry.height
                || !flags_covered),
            "TSRS_INFER_MEMO=shadow: inferring from type {} to type {} (priority {:?}): the walk again has effects {effects} ({before:?} -> {after:?}; types/symbols/signatures now {}/{}/{}), pure {}, outcome {:?} (stored {:?}), cleared {} (stored {}), height {height} (stored {}), flags {flags} (stored {})",
            source.id.0,
            target.id.0,
            n.priority.get(),
            self.type_count,
            self.symbol_count,
            self.signature_count,
            taint.is_pure(),
            outcome,
            entry.infos,
            n.cleared_inferences.get(),
            entry.cleared,
            entry.height,
            entry.flags
        );
    }

    /// Prints the process totals when `TSRS_INFER_MEMO_STATS` is set or in shadow mode (call at exit).
    pub fn infer_memo_finish() {
        if !stats_on() && env_mode() != Some(InferMemoMode::Shadow) {
            return;
        }
        let lookups = total(&LOOKUPS);
        let hits = total(&HITS);
        eprintln!(
            "tsrs: inference memo: {lookups} lookups (walks with at most {MAX_INFOS} infos), {hits} hits ({:.1}%) skipping {} steps, {} stores; walks of {}+ steps not stored: {} had effects ({} created, {} instantiated), {} tainted",
            if lookups == 0 { 0.0 } else { 100.0 * hits as f64 / lookups as f64 },
            total(&SKIPPED_STEPS),
            total(&STORES),
            min_steps(),
            total(&NOT_STORED_EFFECTS),
            total(&NOT_STORED_CREATED),
            total(&NOT_STORED_INSTANTIATED),
            total(&NOT_STORED_TAINT)
        );
    }
}
