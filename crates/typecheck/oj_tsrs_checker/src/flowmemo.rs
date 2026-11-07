//! Persistent memo of control flow analysis results (`TSRS_FLOW_MEMO`; notes/perf-flow-union-inference.md).
//!
//! Go walks the flow graph backwards from each reference (`getFlowTypeOfReference`) and caches only inside one walk
//! (`sharedFlows`) and, per reference key, at loop labels (`flowLoopCache`). So R references to the same variable or
//! property path in a long function each walk the common chain again. This memo keeps the result of a sub-walk, the
//! type at a flow node for one reference key, across walks.
//!
//! Every `getTypeAtFlowNode` call is a frame. A frame's result is stored only if Go, walking the same sub-graph for the
//! same key at any later time, computes the same type and changes nothing observable on the way:
//! - no transient taint: no in-process loop label (Go's incomplete types), circular or in-progress resolution, reduce
//!   label (try/finally), depth or inline-level limit, and no value cached while one of those was active;
//! - no reference taint: nothing read from the reference node beyond its key (its position, for constraint
//!   substitution; the property symbol a property access resolved to);
//! - the result is complete, and the frame did not touch the instantiation counters (`checkExpression` resets them,
//!   instantiations count: TS2589). With caches that only grow, a later walk of the same frame takes a subset of the
//!   paths this one took, so it does not touch them either; a frame that read `flowTypeCache` (which is reset) is
//!   used only while the same cache is current.
//!
//! Taint is tracked with serial numbers. Every frame, walk, loop-stack entry and type resolution takes one; a taint
//! event carries the serial of its source and taints the active frames that are younger than the source (they
//! consumed it). A loop label's frame is older than its stack entry, so the loop result, which Go caches for good, is
//! not tainted by its own back edges, while every frame computed under them is; likewise a resolution's final result.

use std::sync::OnceLock;

use crate::*;

/// Go's limit on nested `getTypeAtFlowNode` calls in one walk (TS2563 when reached).
pub(crate) const FLOW_DEPTH_LIMIT: u32 = 2000;

/// No taint source.
pub(crate) const UNTAINTED: u32 = u32::MAX;

/// The frame instantiated a type, ran inside an instantiation (either may touch the instantiation counters and
/// stack), or created a type, symbol or signature (a later walk may create a new one with another identity).
pub(crate) const FLAG_EFFECTS: u8 = 1;
/// The frame read `flowTypeCache`.
pub(crate) const FLAG_TYPE_CACHE: u8 = 2;
/// The frame may have reset the instantiation count (`checkExpression`): a later walk of it leaves the count where
/// this one did only if the count is already 0.
pub(crate) const FLAG_COUNT_RESET: u8 = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FlowMemoMode {
    Off,
    On,
    /// Every hit is also walked for real; a different answer aborts the process.
    Shadow,
}

fn mode_from_env() -> FlowMemoMode {
    static MODE: OnceLock<FlowMemoMode> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("TSRS_FLOW_MEMO").as_deref() {
        Ok("0" | "off") => FlowMemoMode::Off,
        Ok("shadow") => FlowMemoMode::Shadow,
        _ => FlowMemoMode::On,
    })
}

/// log2 of the slot count (32 bytes each).
fn table_bits() -> u32 {
    static BITS: OnceLock<u32> = OnceLock::new();
    *BITS.get_or_init(|| {
        std::env::var("TSRS_FLOW_MEMO_BITS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|b| (8..=24).contains(b))
            .unwrap_or(12)
    })
}

#[derive(Clone, Copy, Default)]
struct Slot {
    key: u128,
    flow: Option<P<FlowNode>>,
    t: Option<P<Type>>,
    height: u16,
    /// `FlowMemo::type_cache_epoch` when the value was computed, if the frame read that cache; else 0.
    epoch: u32,
    count_reset: bool,
}

/// What `Checker::take_flow_type_cache` saved.
pub(crate) struct SavedFlowTypeCache {
    cache: Option<FxHashMap<P<Node>, P<Type>>>,
    epoch: u32,
}

/// Shadow mode: a memo answer as found, with the full key and origin its slot held then.
pub(crate) struct ShadowHit {
    pub flow: P<FlowNode>,
    pub hit: MemoHit,
    pub full_key: Option<Box<[u8]>>,
    pub origin: (Option<P<Node>>, u32),
}

/// A memo answer: the type, the height of the sub-walk it stands for, and the cache it was computed with.
#[derive(Clone, Copy)]
pub(crate) struct MemoHit {
    pub t: P<Type>,
    pub height: u16,
    pub epoch: u32,
    pub count_reset: bool,
}

impl MemoHit {
    /// The flags a frame that uses this answer takes on (those of the walk it stands for).
    pub fn flags(self) -> u8 {
        (if self.epoch != 0 { FLAG_TYPE_CACHE } else { 0 })
            | if self.count_reset { FLAG_COUNT_RESET } else { 0 }
    }
}

/// The memo's registers for the innermost active frame. Saved whole by `FlowMemo::begin`.
#[derive(Clone, Copy)]
struct Regs {
    /// Oldest source of each kind the frame consumed so far.
    transient: u32,
    reference: u32,
    /// Largest `child height + 1` so far: an upper bound on the depth its sub-walk can reach in any walk (memo and
    /// `sharedFlows` hits count with the height they stand for).
    height: u16,
    /// FLAG_*.
    flags: u8,
}

const FRESH_REGS: Regs = Regs { transient: UNTAINTED, reference: UNTAINTED, height: 0, flags: 0 };

/// What `Checker::flow_frame_begin` saved.
#[derive(Clone, Copy)]
pub(crate) struct FlowFrame {
    pub start: u32,
    saved: Regs,
    instantiation_count: u32,
    total_instantiation_count: u32,
    created: u32,
    in_instantiation: bool,
}

/// What `FlowMemo::end` found: the oldest source of each kind that the frame consumed (UNTAINTED if none was older
/// than the frame), and its flags.
#[derive(Clone, Copy)]
pub(crate) struct FrameTaint {
    pub transient: u32,
    pub reference: u32,
    pub flags: u8,
}

impl FrameTaint {
    pub fn is_pure(self) -> bool {
        self.transient == UNTAINTED && self.reference == UNTAINTED
    }
}

pub struct FlowMemo {
    pub(crate) mode: FlowMemoMode,
    slots: Vec<Slot>,
    /// Shadow mode: the full reference key of each slot, to prove that no two references share a hashed key.
    shadow_keys: Vec<Option<Box<[u8]>>>,
    /// Shadow mode: where each slot's value came from (reference node, depth), for reports.
    shadow_origin: Vec<(Option<P<Node>>, u32)>,
    pub(crate) shadow_origin_next: (Option<P<Node>>, u32),
    serial: u32,
    regs: Regs,
    /// Identifies the current `Checker::flow_type_cache` (a new one per reset, and when an entry changes); saved and
    /// restored with it. Within one, an expression's cached type never changes. Starts at a value serials never take.
    pub(crate) type_cache_epoch: u32,
    /// Checkpoints that the active frames of keyed walks iterated past.
    pub(crate) checkpoints: Vec<P<FlowNode>>,
    /// Shadow mode: memo answers waiting for the frame that found them to end.
    pub(crate) shadow_hits: Vec<ShadowHit>,
    pub(crate) key_buf: Vec<u8>,
}

impl FlowMemo {
    pub(crate) fn new() -> FlowMemo {
        FlowMemo {
            mode: mode_from_env(),
            slots: Vec::new(),
            shadow_keys: Vec::new(),
            shadow_origin: Vec::new(),
            shadow_origin_next: (None, 0),
            serial: 1,
            regs: FRESH_REGS,
            type_cache_epoch: UNTAINTED,
            checkpoints: Vec::new(),
            shadow_hits: Vec::new(),
            key_buf: Vec::new(),
        }
    }

    /// A taint source for something that starts now, without taking a serial: every frame that begins later has a
    /// larger start, every frame that began before has this or a smaller one. Sources need not be unique.
    #[inline]
    pub(crate) fn source_now(&self) -> u32 {
        self.serial - 1
    }

    /// A fresh serial: the taint source of a walk or loop-stack entry, or a frame's start.
    #[inline]
    pub(crate) fn next_serial(&mut self) -> u32 {
        let s = self.serial;
        self.serial += 1;
        if self.serial == UNTAINTED {
            // Serials would wrap (a checker that lives for billions of frames): stop memoizing, keep the rest exact.
            self.mode = FlowMemoMode::Off;
            self.serial = 1;
        }
        s
    }

    #[inline]
    fn begin(
        &mut self,
        instantiation_count: u32,
        total_instantiation_count: u32,
        created: u32,
        in_instantiation: bool,
    ) -> FlowFrame {
        let frame = FlowFrame {
            start: self.next_serial(),
            saved: self.regs,
            instantiation_count,
            total_instantiation_count,
            created,
            in_instantiation,
        };
        self.regs = FRESH_REGS;
        frame
    }

    /// Ends a frame whose sub-walk has height `height` and reports it to the parent; taint and flags carry over.
    #[inline]
    fn end_with(&mut self, frame: FlowFrame, height: u16) -> FrameTaint {
        let r = self.regs;
        let taint = FrameTaint {
            transient: if r.transient < frame.start { r.transient } else { UNTAINTED },
            reference: if r.reference < frame.start { r.reference } else { UNTAINTED },
            flags: r.flags,
        };
        let p = frame.saved;
        self.regs = Regs {
            transient: r.transient.min(p.transient),
            reference: r.reference.min(p.reference),
            height: p.height.max(height.saturating_add(1)),
            flags: r.flags | p.flags,
        };
        taint
    }

    /// Ends a frame that is not a flow walk level (the inference memo's): the parent's registers become what they would
    /// be had the frame not been begun. Returns the frame's taint, the height and the flags its children reported.
    #[inline]
    pub(crate) fn end_transparent(&mut self, frame: FlowFrame) -> (FrameTaint, u16, u8) {
        let r = self.regs;
        let taint = FrameTaint {
            transient: if r.transient < frame.start { r.transient } else { UNTAINTED },
            reference: if r.reference < frame.start { r.reference } else { UNTAINTED },
            flags: r.flags,
        };
        let p = frame.saved;
        self.regs = Regs {
            transient: r.transient.min(p.transient),
            reference: r.reference.min(p.reference),
            height: p.height.max(r.height),
            flags: r.flags | p.flags,
        };
        (taint, r.height, r.flags)
    }

    /// Ends a frame that computed its result: its height is what its children reported.
    #[inline]
    fn end(&mut self, frame: FlowFrame) -> (FrameTaint, u16) {
        let height = self.regs.height;
        (self.end_with(frame, height), height)
    }

    /// The innermost frame's result came from a memo answer for a sub-walk of height `height` at its own level.
    #[inline]
    pub(crate) fn raise_height(&mut self, height: u16) {
        self.regs.height = self.regs.height.max(height);
    }

    #[inline]
    pub(crate) fn add_flags(&mut self, flags: u8) {
        self.regs.flags |= flags;
    }

    #[inline]
    pub(crate) fn save_height(&mut self) -> u16 {
        std::mem::take(&mut self.regs.height)
    }

    #[inline]
    pub(crate) fn restore_height(&mut self, saved: u16) {
        self.regs.height = saved;
    }

    #[inline]
    pub(crate) fn taint(&mut self, source: u32) {
        if source < self.regs.transient {
            self.regs.transient = source;
        }
    }

    #[inline]
    pub(crate) fn taint_reference(&mut self, source: u32) {
        if source < self.regs.reference {
            self.regs.reference = source;
        }
    }

    fn slot_index(&self, flow: P<FlowNode>, key: u128) -> usize {
        let h = (flow.key() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (key as u64)
            ^ ((key >> 64) as u64).rotate_left(29);
        (h.wrapping_mul(0xFF51_AFD7_ED55_8CCD) >> 32) as usize & (self.slots.len() - 1)
    }

    #[inline]
    pub(crate) fn lookup(&self, flow: P<FlowNode>, key: u128) -> Option<MemoHit> {
        if self.slots.is_empty() {
            return None;
        }
        let slot = &self.slots[self.slot_index(flow, key)];
        if slot.flow == Some(flow) && slot.key == key {
            return slot.t.map(|t| MemoHit {
                t,
                height: slot.height,
                epoch: slot.epoch,
                count_reset: slot.count_reset,
            });
        }
        None
    }

    pub(crate) fn store(
        &mut self,
        flow: P<FlowNode>,
        key: u128,
        t: P<Type>,
        height: u16,
        epoch: u32,
        count_reset: bool,
    ) {
        if self.slots.is_empty() {
            self.slots = vec![Slot::default(); 1 << table_bits()];
            if self.mode == FlowMemoMode::Shadow {
                self.shadow_keys = vec![None; 1 << table_bits()];
                self.shadow_origin = vec![(None, 0); 1 << table_bits()];
            }
        }
        let i = self.slot_index(flow, key);
        self.slots[i] = Slot { key, flow: Some(flow), t: Some(t), height, epoch, count_reset };
        if self.mode == FlowMemoMode::Shadow {
            self.shadow_keys[i] = Some(self.key_buf.clone().into_boxed_slice());
            self.shadow_origin[i] = self.shadow_origin_next;
        }
    }

    /// Shadow mode: the answer `lookup` found, with what its slot holds now.
    pub(crate) fn shadow_hit(&self, flow: P<FlowNode>, key: u128, hit: MemoHit) -> ShadowHit {
        let i = self.slot_index(flow, key);
        ShadowHit {
            flow,
            hit,
            full_key: self.shadow_keys.get(i).cloned().flatten(),
            origin: self.shadow_origin.get(i).copied().unwrap_or((None, 0)),
        }
    }
}

/// Where a memo key is serialized: a `Vec`, or a stack buffer for the common short keys.
pub(crate) trait KeySink {
    fn reset(&mut self);
    /// False when full.
    fn put(&mut self, bytes: &[u8]) -> bool;
}

impl KeySink for Vec<u8> {
    fn reset(&mut self) {
        self.clear();
    }

    fn put(&mut self, bytes: &[u8]) -> bool {
        self.extend_from_slice(bytes);
        true
    }
}

struct StackKey {
    bytes: [u8; 128],
    len: usize,
}

impl KeySink for StackKey {
    fn reset(&mut self) {
        self.len = 0;
    }

    #[inline]
    fn put(&mut self, bytes: &[u8]) -> bool {
        let Some(slot) = self.bytes.get_mut(self.len..self.len + bytes.len()) else {
            return false;
        };
        slot.copy_from_slice(bytes);
        self.len += bytes.len();
        true
    }
}

impl Default for FlowMemo {
    fn default() -> Self {
        FlowMemo::new()
    }
}

impl Checker {
    #[inline]
    pub(crate) fn flow_frame_begin(&mut self) -> FlowFrame {
        self.flow_memo.begin(
            self.instantiation_count,
            self.total_instantiation_count,
            self.flow_memo_created(),
            !self.active_mappers.is_empty(),
        )
    }

    /// Ends a frame that computed its result, adding what it did to the instantiation counters to its flags. With no
    /// instantiation in it, the count can only have stayed or gone to 0 (a `checkExpression` reset); if it was 0 at
    /// the start, a reset cannot be told apart.
    #[inline]
    pub(crate) fn flow_frame_end(&mut self, frame: FlowFrame) -> (FrameTaint, u16) {
        self.flow_frame_counters(frame);
        self.flow_memo.end(frame)
    }

    /// `flow_frame_end` for a frame whose answer stands for a sub-walk of height `height` (a `sharedFlows` hit).
    #[inline]
    pub(crate) fn flow_frame_end_with(&mut self, frame: FlowFrame, height: u16) -> FrameTaint {
        self.flow_frame_counters(frame);
        self.flow_memo.end_with(frame, height)
    }

    /// Changes whenever a type, symbol or signature is created.
    #[inline]
    fn flow_memo_created(&self) -> u32 {
        self.type_count.wrapping_add(self.symbol_count).wrapping_add(self.signature_count)
    }

    #[inline]
    fn flow_frame_counters(&mut self, frame: FlowFrame) {
        if self.total_instantiation_count != frame.total_instantiation_count
            || frame.in_instantiation
            || self.flow_memo_created() != frame.created
        {
            self.flow_memo.add_flags(FLAG_EFFECTS);
        }
        if frame.instantiation_count == 0 || self.instantiation_count != frame.instantiation_count {
            self.flow_memo.add_flags(FLAG_COUNT_RESET);
        }
    }

    /// Propagates the taint of a `sharedFlows` entry to the frames that consume it. A transient
    /// source that is no longer active means the value outlived the loop analysis or resolution that produced it (Go
    /// reuses such values): everything active consumed a stale value.
    #[inline]
    pub(crate) fn flow_memo_consume(&mut self, transient: u32, reference: u32) {
        if transient != UNTAINTED {
            self.flow_memo_consume_transient(transient);
        }
        if reference != UNTAINTED {
            self.flow_memo.taint_reference(reference);
        }
    }

    #[cold]
    fn flow_memo_consume_transient(&mut self, source: u32) {
        let live = source != 0
            && (self.flow_loop_stack.iter().any(|info| info.serial == source)
                || self.type_resolutions.iter().any(|r| r.serial == source));
        self.flow_memo.taint(if live { source } else { 0 });
    }

    /// The walk's memo key, computed once per walk: a hash of the reference's structure (root symbol or `this`, the
    /// property names of a property access chain), the declared and initial types and the flow container. Only
    /// identifiers, `this` and chains of plain property accesses on them get one; everything that the walk reads
    /// from such a reference (matching, the Start node's continuation rules, the accessed names) is a function of
    /// that structure. Computing it has no side effects: it reads a resolved symbol only if one is cached and
    /// assigns no node or symbol ids.
    pub(crate) fn flow_memo_key(&mut self, f: P<FlowState>) -> Option<u128> {
        match f.memo_key_state.get() {
            1 => return None,
            2 => return Some(f.memo_key.get()),
            _ => {}
        }
        let key = self.compute_flow_memo_key(f);
        match key {
            Some(key) => {
                f.memo_key.set(key);
                f.memo_key_state.set(2);
            }
            None => f.memo_key_state.set(1),
        }
        key
    }

    /// With 32-bit pointer keys, an identifier or `this` packs into the key exactly: [root symbol (0 for `this`) |
    /// declared type id | initial type id | flow container], with bit 63 clear. Wider pointers and property access
    /// chains are hashed (xxh3-128 of the serialized key, as Go hashes its flow cache keys) with bit 63 set, so they
    /// never equal a packed key.
    fn compute_flow_memo_key(&mut self, f: P<FlowState>) -> Option<u128> {
        let reference = f.reference.get().unwrap();
        let declared = f.declared_type.get().unwrap().id.0;
        let initial = f.initial_type.get().unwrap().id.0;
        let container = P::key_opt(f.flow_container.get());
        let root = match reference.kind() {
            Kind::Identifier => self.flow_memo_root_symbol(reference)?.key(),
            Kind::ThisKeyword => 0,
            _ => return self.compute_hashed_flow_memo_key(f),
        };
        if tsrs_core::PKey::BITS <= 32 && declared < 1 << 31 {
            Some(
                ((root as u128) << 96)
                    | ((declared as u128) << 64)
                    | ((initial as u128) << 32)
                    | container as u128,
            )
        } else {
            self.compute_hashed_flow_memo_key(f)
        }
    }

    fn compute_hashed_flow_memo_key(&mut self, f: P<FlowState>) -> Option<u128> {
        let mut key = StackKey { bytes: [0; 128], len: 0 };
        if self.serialize_flow_memo_key(&mut key, f) {
            return Some(xxhash_rust::xxh3::xxh3_128(&key.bytes[..key.len]) | (1 << 63));
        }
        // Not a key, or longer than the stack buffer.
        let mut buf = std::mem::take(&mut self.flow_memo.key_buf);
        let key = self
            .serialize_flow_memo_key(&mut buf, f)
            .then(|| xxhash_rust::xxh3::xxh3_128(&buf) | (1 << 63));
        self.flow_memo.key_buf = buf;
        key
    }

    /// The full key a hashed or packed memo key stands for (shadow mode compares these). False if the reference has
    /// no key or the sink is full.
    pub(crate) fn serialize_flow_memo_key(&self, sink: &mut impl KeySink, f: P<FlowState>) -> bool {
        sink.reset();
        self.write_flow_memo_reference(sink, f.reference.get().unwrap())
            && sink.put(&f.declared_type.get().unwrap().id.0.to_le_bytes())
            && sink.put(&f.initial_type.get().unwrap().id.0.to_le_bytes())
            && sink.put(&(P::key_opt(f.flow_container.get()) as u64).to_le_bytes())
    }

    /// The symbol an identifier reference resolved to, if it is cached (no side effects), and the identifier is not
    /// `this` in a type query (Go keys and matches those like `this`).
    fn flow_memo_root_symbol(&self, node: P<Node>) -> Option<P<Symbol>> {
        let symbol = self
            .symbol_node_links
            .try_get_if_id_assigned(node)
            .and_then(|links| links.resolved_symbol.get())?;
        if symbol == self.unknown_symbol
            || node.as_identifier().text() == "this" && ast::is_this_in_type_query(node)
        {
            return None;
        }
        Some(symbol)
    }

    fn write_flow_memo_reference(&self, sink: &mut impl KeySink, node: P<Node>) -> bool {
        match node.kind() {
            Kind::Identifier => match self.flow_memo_root_symbol(node) {
                Some(symbol) => sink.put(b"I") && sink.put(&(symbol.key() as u64).to_le_bytes()),
                None => false,
            },
            Kind::ThisKeyword => sink.put(b"T"),
            Kind::PropertyAccessExpression => {
                if node.flags().intersects(NodeFlags::OptionalChain)
                    || !self.write_flow_memo_reference(sink, node.expression().unwrap())
                {
                    return false;
                }
                let name = node.name().unwrap().text();
                sink.put(b"P")
                    && sink.put(&(name.len() as u32).to_le_bytes())
                    && sink.put(name.as_bytes())
            }
            _ => false,
        }
    }

    /// Whether a fresh walk of this frame now could read a value that the memo's walk did not (this walk's transient
    /// `sharedFlows` values, the loop analysis in progress, another `flowTypeCache` when the memo's walk read it, an
    /// active instantiation's cache), or leave the instantiation count elsewhere.
    #[inline]
    pub(crate) fn flow_memo_consult_ok(&self, f: P<FlowState>, hit: MemoHit) -> bool {
        let cache_ok = hit.epoch == 0 || hit.epoch == self.flow_memo.type_cache_epoch;
        let counters_ok =
            self.active_mappers.is_empty() && (!hit.count_reset || self.instantiation_count == 0);
        !f.memo_faithful.get()
            && self.flow_loop_stack.is_empty()
            && cache_ok
            && counters_ok
            && f.impure_shared.get() == 0
    }

    /// Go's `saveFlowTypeCache := c.flowTypeCache; c.flowTypeCache = nil`, with the memo's bookkeeping.
    pub(crate) fn take_flow_type_cache(&mut self) -> SavedFlowTypeCache {
        let saved = SavedFlowTypeCache {
            cache: self.flow_type_cache.take(),
            epoch: self.flow_memo.type_cache_epoch,
        };
        self.flow_memo.type_cache_epoch = self.flow_memo.next_serial();
        saved
    }

    pub(crate) fn restore_flow_type_cache(&mut self, saved: SavedFlowTypeCache) {
        self.flow_type_cache = saved.cache;
        self.flow_memo.type_cache_epoch = saved.epoch;
    }
}
