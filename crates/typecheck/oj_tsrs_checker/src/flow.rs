use crate::flowmemo::{
    FLAG_COUNT_RESET, FLAG_EFFECTS, FLAG_TYPE_CACHE, FLOW_DEPTH_LIMIT, FlowMemoMode, FrameTaint,
    MemoHit, ShadowHit, UNTAINTED,
};
use crate::*;
use tsrs_ast as ast;
use tsrs_ast::*;
use tsrs_core::TextPos;
use tsrs_diagnostics as diagnostics;

// Non-function declarations of flow.go (FlowType, SharedFlow, FlowState, typeofNEFacts,
// nonDottedNameCacheKey) are in flow_types.rs.

impl FlowType {
    // flow.go:24
    fn is_nil(self) -> bool {
        self.t.is_none()
    }
}

impl FlowState {
    fn ref_node(&self) -> P<Node> {
        self.reference.get().unwrap()
    }

    fn declared(&self) -> P<Type> {
        self.declared_type.get().unwrap()
    }

    fn initial(&self) -> P<Type> {
        self.initial_type.get().unwrap()
    }
}

fn flow_type_of(t: P<Type>) -> FlowType {
    FlowType { t: Some(t), incomplete: false }
}

/// A frame that may use and fill the memo: the walk's key, and where its entries in `FlowMemo::checkpoints` and
/// `FlowMemo::shadow_hits` begin (flowmemo.rs).
#[derive(Clone, Copy)]
struct FrameMemo {
    key: u128,
    checkpoints: u32,
    shadow: u32,
}

enum FlowStep {
    Next(P<FlowNode>),
    Done(FlowType),
}

/// About one in four flow nodes, chosen by the node: a checkpoint for the flow memo on long linear chains. (None
/// measured worse: notes/perf-flow-union-inference.md.)
#[inline]
fn is_flow_memo_checkpoint(flow: P<FlowNode>) -> bool {
    (flow.key() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 62 == 0
}

impl Checker {
    // flow.go:28
    fn new_flow_type(&self, t: P<Type>, incomplete: bool) -> FlowType {
        let mut t = t;
        if incomplete && t.flags().intersects(TypeFlags::Never) {
            t = self.silent_never_type;
        }
        FlowType { t: Some(t), incomplete }
    }

    // flow.go:52
    fn get_flow_state(&mut self) -> P<FlowState> {
        let f = match self.free_flow_state {
            Some(f) => f,
            None => P::new(FlowState::default()),
        };
        self.free_flow_state = f.next.get();
        f
    }

    // flow.go:61
    fn put_flow_state(&mut self, f: P<FlowState>) {
        f.reference.set(None);
        f.declared_type.set(None);
        f.initial_type.set(None);
        f.flow_container.set(None);
        f.ref_key.set(CacheHashKey::default());
        f.depth.set(0);
        f.shared_flow_start.set(0);
        f.reduce_labels.borrow_mut().clear();
        f.memo_key_state.set(0);
        f.memo_used.set(false);
        f.memo_aborted.set(false);
        f.memo_faithful.set(false);
        f.impure_shared.set(0);
        f.reduce_depth.set(0);
        f.next.set(self.free_flow_state);
        self.free_flow_state = Some(f);
    }
}

// flow.go:69
#[expect(
    clippy::redundant_pub_crate,
    reason = "Only the public API is re-exported; implementation helpers remain crate-private."
)]
pub(crate) fn get_flow_node_of_node(node: P<Node>) -> Option<P<FlowNode>> {
    node.flow_node()
}

impl Checker {
    // flow.go:77
    pub(crate) fn get_flow_type_of_reference(
        &mut self,
        reference: P<Node>,
        declared_type: P<Type>,
    ) -> P<Type> {
        self.get_flow_type_of_reference_ex(reference, declared_type, declared_type, None, None)
    }

    // flow.go:81
    pub(crate) fn get_flow_type_of_reference_ex(
        &mut self,
        reference: P<Node>,
        declared_type: P<Type>,
        initial_type: P<Type>,
        flow_container: Option<P<Node>>,
        flow_node: Option<P<FlowNode>>,
    ) -> P<Type> {
        if self.flow_analysis_disabled {
            return self.error_type;
        }
        let flow_node = match flow_node {
            Some(flow_node) => flow_node,
            None => match get_flow_node_of_node(reference) {
                Some(flow_node) => flow_node,
                None => return declared_type,
            },
        };
        let f = self.get_flow_state();
        f.reference.set(Some(reference));
        f.declared_type.set(Some(declared_type));
        f.initial_type.set(Some(initial_type));
        f.flow_container.set(flow_container);
        f.shared_flow_start.set(self.shared_flows.len());
        f.walk_floor.set(self.flow_memo.next_serial());
        self.flow_invocation_count += 1;
        let mut census_container = None;
        let census_t0 = self.census_mut().map(|c| c.now_ns());
        if let Some(census_t0) = census_t0 {
            census_container = ast::get_containing_function(reference);
            let sampled = (census_container.map_or(0, tsrs_core::P::to_bits) as u64)
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                >> 60
                == 0;
            let key = sampled.then(|| {
                let text = if !reference.pos().is_synthetic()
                    && ast::get_source_file_of_node(reference).is_some()
                {
                    tsrs_scanner::get_text_of_node(reference)
                } else {
                    format!("#{}", reference.to_bits())
                };
                let mut bytes = text.into_bytes();
                bytes.extend_from_slice(&declared_type.id.0.to_le_bytes());
                bytes.extend_from_slice(&initial_type.id.0.to_le_bytes());
                bytes.extend_from_slice(
                    &(census_container.map_or(0, tsrs_core::P::to_bits) as u64).to_le_bytes(),
                );
                xxhash_rust::xxh3::xxh3_64(&bytes)
            });
            let census = self.census.as_mut().unwrap();
            if sampled {
                census.flow_sampled_invocations += 1;
            }
            let steps = census.flow_steps;
            census.flow_stack.push((steps, 0, key));
            census.charge_since(census_t0);
        }
        let census_span = self.census_begin(crate::workcensus::Cat::Flow, || {
            crate::workcensus::CKey::OptNode(census_container)
        });
        if self.flow_memo.mode != FlowMemoMode::Off {
            self.flow_memo_key(f);
        }
        // A nested walk counts its depth from 0: its frames' heights are not this frame's.
        let saved_height = self.flow_memo.save_height();
        let mut flow_type = self.get_type_at_flow_node(f, flow_node);
        if f.memo_aborted.get() {
            // The walk used memo results and then reached the depth limit. Go's walk computes those sub-walks and
            // keeps their shared nodes in sharedFlows, so it may not be this deep here: walk again without the memo.
            self.shared_flows.truncate(f.shared_flow_start.get());
            f.memo_aborted.set(false);
            f.memo_used.set(false);
            f.memo_faithful.set(true);
            f.impure_shared.set(0);
            f.depth.set(0);
            flow_type = self.get_type_at_flow_node(f, flow_node);
        }
        self.flow_memo.restore_height(saved_height);
        let evolved_type = flow_type.t.unwrap();
        if let Some(timing) = self.census_end(census_span) {
            let census = self.census.as_mut().unwrap();
            let (start, nested, _) = census.flow_stack.pop().unwrap();
            let total = census.flow_steps - start;
            let own = total - nested;
            if let Some(parent) = census.flow_stack.last_mut() {
                parent.1 += total;
            }
            let b = crate::workcensus::bucket(own as usize) as usize;
            let h = &mut census.flow_hist[b.min(23)];
            h.count += 1;
            h.a += own;
            if timing.outer {
                h.incl_ns += timing.incl_ns;
            }
            census.record(
                crate::workcensus::Cat::Flow,
                crate::workcensus::CKey::OptNode(census_container),
                timing,
                own,
                0,
                0,
            );
            let s = census
                .stats
                .get_mut(&(
                    crate::workcensus::Cat::Flow,
                    crate::workcensus::CKey::OptNode(census_container),
                ))
                .unwrap();
            s.b = s.b.max(own);
        }
        self.shared_flows.truncate(f.shared_flow_start.get());
        self.put_flow_state(f);
        // When the reference is 'x' in an 'x.length', 'x.push(value)', 'x.unshift(value)' or x[n] = value' operation,
        // we give type 'any[]' to 'x' instead of using the type determined by control flow analysis such that operations
        // on empty arrays are possible without implicit any errors and new element types can be inferred without
        // type mismatch errors.
        let result_type = if evolved_type.object_flags().intersects(ObjectFlags::EvolvingArray)
            && self.is_evolving_array_operation_target(reference)
        {
            self.auto_array_type
        } else {
            self.finalize_evolving_array_type(evolved_type)
        };
        if result_type == self.unreachable_never_type
            || reference.parent().is_some()
                && ast::is_non_null_expression(reference.parent().unwrap())
                && !result_type.flags().intersects(TypeFlags::Never)
                && self
                    .get_type_with_facts(result_type, TypeFacts::NEUndefinedOrNull)
                    .flags()
                    .intersects(TypeFlags::Never)
        {
            return declared_type;
        }
        result_type
    }

    // flow.go:117
    fn get_type_at_flow_node(&mut self, f: P<FlowState>, flow: P<FlowNode>) -> FlowType {
        let entry_flow = flow;
        let mut flow = flow;
        if f.depth.get() == FLOW_DEPTH_LIMIT {
            if f.memo_used.get() {
                // Go, which walked the sub-graphs this walk took from the memo, may not be this deep here:
                // get_flow_type_of_reference_ex walks again without the memo.
                f.memo_aborted.set(true);
                return flow_type_of(self.error_type);
            }
            // We have made 2000 recursive invocations. To avoid overflowing the call stack we report an error
            // and disable further control flow analysis in the containing function or module body.
            self.flow_memo.taint(0);
            self.flow_analysis_disabled = true;
            self.report_flow_control_error(f.ref_node());
            return flow_type_of(self.error_type);
        }
        // Flow memo (flowmemo.rs): in a walk with a memo key every call is a frame, which asks the memo at its first
        // node, at the node that ends its iteration and at checkpoints, and stores its answer under them. The key's
        // meaning holds only outside inlined conditions and reduce labels, while flow analysis is on. A walk without
        // a key does no bookkeeping: what it consumes reaches the frames of enclosing walks through the memo's
        // registers directly, and nothing of it is stored.
        let entry_depth = f.depth.get();
        let tracked = f.memo_key_state.get() == 2;
        let frame = tracked.then(|| self.flow_frame_begin());
        let memo = (tracked
            && self.inline_level == 0
            && !self.flow_analysis_disabled
            && f.reduce_depth.get() == 0)
            .then(|| FrameMemo {
                key: f.memo_key.get(),
                checkpoints: self.flow_memo.checkpoints.len() as u32,
                shadow: if self.flow_memo.mode == FlowMemoMode::Shadow {
                    self.flow_memo.shadow_hits.len() as u32
                } else {
                    0
                },
            });
        f.depth.set(entry_depth + 1);
        let mut shared_flow: Option<P<FlowNode>> = None;
        let mut steps = 0u32;
        let mut hit: Option<MemoHit> = None;
        let t = loop {
            steps += 1;
            if let Some(census) = self.census_mut() {
                census.flow_steps += 1;
                if let Some(&(_, _, Some(key))) = census.flow_stack.last() {
                    census.flow_sampled_steps += 1;
                    if !census.flow_seen.insert((key, flow.to_bits())) {
                        census.flow_sampled_repeats += 1;
                    }
                }
            }
            let flags = flow.flags();
            if flags.intersects(FlowFlags::Shared) {
                // We cache results of flow type resolution for shared nodes that were previously visited in
                // the same getFlowTypeOfReference invocation. A node is considered shared when it is the
                // antecedent of more than one node.
                for i in f.shared_flow_start.get()..self.shared_flows.len() {
                    if self.shared_flows[i].flow == flow {
                        let shared = self.shared_flows[i];
                        f.depth.set(entry_depth);
                        if let Some(frame) = frame {
                            // A later walk of this frame may compute the node instead: it consumes what that took.
                            self.flow_memo_consume(shared.transient, shared.reference);
                            self.flow_memo.add_flags(shared.flags);
                            let taint = self.flow_frame_end_with(frame, shared.height);
                            if let Some(memo) = memo {
                                self.flow_memo.checkpoints.truncate(memo.checkpoints as usize);
                                self.flow_memo_shadow_settle(
                                    f,
                                    memo,
                                    entry_depth,
                                    shared.flow_type,
                                    taint,
                                );
                            }
                        }
                        return shared.flow_type;
                    }
                }
                shared_flow = Some(flow);
            }
            if let Some(memo) = memo
                && let Some((memo_hit, ends_iteration)) =
                    self.flow_memo_at_node(f, entry_flow, flow, flags, memo.key, entry_depth)
            {
                if !ends_iteration {
                    // Go walks on from here and records the last shared node of the whole iteration, which this
                    // frame does not know; leave the record out (a later visit walks or hits the memo again).
                    shared_flow = None;
                }
                hit = Some(memo_hit);
                break flow_type_of(memo_hit.t);
            }
            match self.get_type_at_flow_node_step(f, flow, flags) {
                FlowStep::Next(next) => flow = next,
                FlowStep::Done(t) => break t,
            }
        };
        f.depth.set(entry_depth);
        let Some(frame) = frame else {
            if let Some(shared_flow) = shared_flow {
                // Record visited node and the associated type in the cache.
                self.shared_flows.push(SharedFlow {
                    flow: shared_flow,
                    flow_type: t,
                    transient: UNTAINTED,
                    reference: UNTAINTED,
                    height: 0,
                    flags: 0,
                });
            }
            return t;
        };
        if f.memo_aborted.get() {
            self.flow_frame_end(frame);
            if let Some(memo) = memo {
                self.flow_memo.checkpoints.truncate(memo.checkpoints as usize);
                self.flow_memo.shadow_hits.truncate(memo.shadow as usize);
            }
            return t;
        }
        if let Some(hit) = hit {
            // This frame's answer came from the memo: its height is that sub-walk's (iteration adds no depth).
            f.memo_used.set(true);
            self.flow_memo.raise_height(hit.height);
            self.flow_memo.add_flags(hit.flags());
        }
        let (taint, height) = self.flow_frame_end(frame);
        if let Some(shared_flow) = shared_flow {
            // Record visited node and the associated type in the cache.
            if taint.transient != UNTAINTED {
                f.impure_shared.set(f.impure_shared.get() + 1);
            }
            self.shared_flows.push(SharedFlow {
                flow: shared_flow,
                flow_type: t,
                transient: taint.transient,
                reference: taint.reference,
                height,
                flags: taint.flags,
            });
        }
        if let Some(memo) = memo {
            self.flow_memo_fill(
                f,
                memo,
                entry_flow,
                flow,
                entry_depth,
                t,
                taint,
                height,
                steps,
                hit,
            );
            self.flow_memo.checkpoints.truncate(memo.checkpoints as usize);
        }
        t
    }

    /// One node of `getTypeAtFlowNode`'s loop: the node passes the type on (`Next`) or determines it (`Done`).
    #[expect(
        clippy::inline_always,
        reason = "out of line it costs a call per flow node: +0.17% check instructions on xstate (notes/perf-flow-union-inference.md)"
    )]
    #[inline(always)]
    fn get_type_at_flow_node_step(
        &mut self,
        f: P<FlowState>,
        flow: P<FlowNode>,
        flags: FlowFlags,
    ) -> FlowStep {
        let t: FlowType;
        if flags.intersects(FlowFlags::Assignment) {
            t = self.get_type_at_flow_assignment(f, flow);
            if t.is_nil() {
                return FlowStep::Next(flow.antecedent().unwrap());
            }
        } else if flags.intersects(FlowFlags::Call) {
            t = self.get_type_at_flow_call(f, flow);
            if t.is_nil() {
                return FlowStep::Next(flow.antecedent().unwrap());
            }
        } else if flags.intersects(FlowFlags::Condition) {
            t = self.get_type_at_flow_condition(f, flow);
        } else if flags.intersects(FlowFlags::SwitchClause) {
            t = self.get_type_at_switch_clause(f, flow);
        } else if flags.intersects(FlowFlags::BranchLabel) {
            let antecedents =
                get_branch_label_antecedents(flow, &f.reduce_labels.borrow()).unwrap();
            if antecedents.next.get().is_none() {
                return FlowStep::Next(antecedents.flow);
            }
            t = self.get_type_at_flow_branch_label(f, flow, antecedents);
        } else if flags.intersects(FlowFlags::LoopLabel) {
            let antecedents = flow.antecedents().unwrap();
            if antecedents.next.get().is_none() {
                return FlowStep::Next(antecedents.flow);
            }
            t = self.get_type_at_flow_loop_label(f, flow);
        } else if flags.intersects(FlowFlags::ArrayMutation) {
            t = self.get_type_at_flow_array_mutation(f, flow);
            if t.is_nil() {
                return FlowStep::Next(flow.antecedent().unwrap());
            }
        } else if flags.intersects(FlowFlags::ReduceLabel) {
            // Flow memo: the antecedents of a branch label below depend on the reduce labels above it, which the
            // memo key does not have; and Go's sharedFlows keep what a shared node got under a reduce label for the
            // rest of the walk.
            self.flow_memo.taint(0);
            f.reduce_labels.borrow_mut().push(flow.node().unwrap().as_flow_reduce_label_data_p());
            f.reduce_depth.set(f.reduce_depth.get() + 1);
            t = self.get_type_at_flow_node(f, flow.antecedent().unwrap());
            f.reduce_depth.set(f.reduce_depth.get() - 1);
            f.reduce_labels.borrow_mut().pop();
        } else if flags.intersects(FlowFlags::Start) {
            // Check if we should continue with the control flow of the containing function.
            let container = flow.node();
            if let Some(container) = container {
                let reference = f.ref_node();
                if !(Some(container) == f.flow_container.get()
                    || ast::is_property_access_expression(reference)
                    || ast::is_element_access_expression(reference)
                    || reference.kind() == Kind::ThisKeyword && !ast::is_arrow_function(container))
                {
                    assert!(container.has_flow_node_data());
                    return FlowStep::Next(container.flow_node().unwrap());
                }
            }
            // At the top of the flow we have the initial type.
            t = flow_type_of(f.initial());
        } else {
            // Unreachable code errors are reported in the binding phase. Here we
            // simply return the non-auto declared type to reduce follow-on errors.
            t = flow_type_of(self.convert_auto_to_any(f.declared()));
        }
        FlowStep::Done(t)
    }

    /// Whether to ask the memo before the handler of this node, and its answer (with whether the node ends the
    /// iteration). A frame asks at its first node, at a node that ends its iteration (a condition, a switch clause,
    /// a label with more than one antecedent), and at checkpoints: about one in four of the nodes it iterates past,
    /// chosen by the node itself so that walks starting at different places agree on them. The answer at any of them
    /// is the frame's answer (the nodes iterated past pass the type on).
    fn flow_memo_at_node(
        &mut self,
        f: P<FlowState>,
        entry_flow: P<FlowNode>,
        flow: P<FlowNode>,
        flags: FlowFlags,
        key: u128,
        entry_depth: u32,
    ) -> Option<(MemoHit, bool)> {
        let ends_iteration = flags.intersects(FlowFlags::Condition | FlowFlags::SwitchClause)
            || flags.intersects(FlowFlags::BranchLabel | FlowFlags::LoopLabel)
                && flow.antecedents().is_some_and(|a| a.next.get().is_some());
        if !ends_iteration && flow != entry_flow {
            if !is_flow_memo_checkpoint(flow) {
                return None;
            }
            self.flow_memo.checkpoints.push(flow);
        }
        let hit = self.flow_memo_lookup(f, flow, key, entry_depth)?;
        if self.flow_memo.mode == FlowMemoMode::Shadow {
            let shadow_hit = self.flow_memo.shadow_hit(flow, key, hit);
            self.flow_memo.shadow_hits.push(shadow_hit);
            return None;
        }
        Some((hit, ends_iteration))
    }

    fn flow_memo_lookup(
        &self,
        f: P<FlowState>,
        flow: P<FlowNode>,
        key: u128,
        entry_depth: u32,
    ) -> Option<MemoHit> {
        let hit = self.flow_memo.lookup(flow, key)?;
        if !self.flow_memo_consult_ok(f, hit) {
            return None;
        }
        // Go's walk of this sub-graph reaches at most `height` levels below here: it must not reach the limit, or Go
        // would report TS2563 inside it.
        if entry_depth + u32::from(hit.height) >= FLOW_DEPTH_LIMIT {
            return None;
        }
        Some(hit)
    }

    /// Stores the frame's answer under its first node, the node that ended its iteration and the checkpoints it
    /// iterated past (the type is the same at all of them; so is the height, iteration adds no depth).
    fn flow_memo_fill(
        &mut self,
        f: P<FlowState>,
        memo: FrameMemo,
        entry_flow: P<FlowNode>,
        final_flow: P<FlowNode>,
        entry_depth: u32,
        t: FlowType,
        taint: FrameTaint,
        height: u16,
        steps: u32,
        hit: Option<MemoHit>,
    ) {
        self.flow_memo_shadow_settle(f, memo, entry_depth, t, taint);
        let key = memo.key;
        if !taint.is_pure() {
            return;
        }
        if taint.flags & FLAG_EFFECTS != 0 {
            return;
        }
        if t.incomplete || self.flow_analysis_disabled {
            return;
        }
        if height == 0 && steps < 4 && hit.is_none() {
            // A short iteration ending without recursion costs less to walk again than to keep.
            return;
        }
        if self.flow_memo.mode == FlowMemoMode::Shadow {
            self.flow_memo_shadow_key_bytes(f);
            self.flow_memo.shadow_origin_next = (f.reference.get(), entry_depth);
        }
        let epoch =
            if taint.flags & FLAG_TYPE_CACHE != 0 { self.flow_memo.type_cache_epoch } else { 0 };
        let count_reset = taint.flags & FLAG_COUNT_RESET != 0;
        let t = t.t.unwrap();
        if entry_flow != final_flow || hit.is_none() {
            self.flow_memo.store(entry_flow, key, t, height, epoch, count_reset);
        }
        if final_flow != entry_flow && hit.is_none() {
            self.flow_memo.store(final_flow, key, t, height, epoch, count_reset);
        }
        for i in memo.checkpoints as usize..self.flow_memo.checkpoints.len() {
            let checkpoint = self.flow_memo.checkpoints[i];
            if checkpoint != final_flow {
                self.flow_memo.store(checkpoint, key, t, height, epoch, count_reset);
            }
        }
    }

    /// Shadow mode: checks the answers the memo had for this frame's nodes against what the frame computed.
    #[inline]
    fn flow_memo_shadow_settle(
        &mut self,
        f: P<FlowState>,
        memo: FrameMemo,
        entry_depth: u32,
        t: FlowType,
        taint: FrameTaint,
    ) {
        if self.flow_memo.shadow_hits.len() > memo.shadow as usize {
            for shadow_hit in self.flow_memo.shadow_hits.split_off(memo.shadow as usize) {
                self.flow_memo_shadow_check(f, entry_depth, t, taint, shadow_hit);
            }
        }
    }

    /// Shadow mode: the frame was walked although the memo had an answer; they must agree.
    #[cold]
    fn flow_memo_shadow_check(
        &mut self,
        f: P<FlowState>,
        entry_depth: u32,
        t: FlowType,
        taint: FrameTaint,
        shadow_hit: ShadowHit,
    ) {
        self.flow_memo_shadow_key_bytes(f);
        let ShadowHit { flow, hit, full_key, origin: (origin, origin_depth) } = shadow_hit;
        let same_key = full_key.as_deref() == Some(self.flow_memo.key_buf.as_slice());
        if t.t == Some(hit.t) && !t.incomplete && same_key {
            return;
        }
        let reference = f.ref_node();
        let file = ast::get_source_file_of_node(reference)
            .map_or(String::new(), |s| s.file_name().to_string());
        let walked = t.t.map_or(String::new(), |t| self.type_to_string(t, None));
        let memo = self.type_to_string(hit.t, None);
        panic!(
            "TSRS_FLOW_MEMO=shadow: memo and walk disagree for reference at {file}:{:?} (flow node {:?} of node at {:?} in a frame at depth {entry_depth}; memo from reference at {:?} depth {origin_depth}): walked {walked} (type {:?}, incomplete {}, taint {}/{}), memo {memo} (type {:?}, height {}), same key {same_key}",
            reference.pos(),
            flow.flags(),
            flow.node().map(|n| (n.pos(), n.end(), n.kind())),
            origin.map(|n| n.pos()),
            t.t.map(|t| t.id.0),
            t.incomplete,
            taint.transient,
            taint.reference,
            hit.t.id.0,
            hit.height
        );
    }

    /// Shadow mode: the walk's full memo key in `flow_memo.key_buf`.
    fn flow_memo_shadow_key_bytes(&mut self, f: P<FlowState>) {
        let mut buf = std::mem::take(&mut self.flow_memo.key_buf);
        let ok = self.serialize_flow_memo_key(&mut buf, f);
        assert!(ok, "TSRS_FLOW_MEMO=shadow: a keyed walk's key does not serialize");
        self.flow_memo.key_buf = buf;
    }
}

// flow.go:208
fn get_branch_label_antecedents(
    flow: P<FlowNode>,
    reduce_labels: &[P<ast::FlowReduceLabelData>],
) -> Option<P<FlowList>> {
    if let Some(data) = reduce_labels.iter().rev().find(|data| data.target == flow) {
        return data.antecedents;
    }
    flow.antecedents()
}

impl Checker {
    // flow.go:220
    fn get_type_at_flow_assignment(&mut self, f: P<FlowState>, flow: P<FlowNode>) -> FlowType {
        let node = flow.node().unwrap();
        // Assignments only narrow the computed type if the declared type is a union type. Thus, we
        // only need to evaluate the assigned type if the declared type is a union type.
        if self.is_matching_reference(f.ref_node(), node) {
            if !self.is_reachable_flow_node(flow) {
                return flow_type_of(self.unreachable_never_type);
            }
            if get_assignment_target_kind(node) == AssignmentKind::Compound {
                let flow_type = self.get_type_at_flow_node(f, flow.antecedent().unwrap());
                if f.memo_aborted.get() {
                    return flow_type;
                }
                let t = self.get_base_type_of_literal_type(flow_type.t.unwrap());
                return self.new_flow_type(t, flow_type.incomplete);
            }
            if f.declared() == self.auto_type || f.declared() == self.auto_array_type {
                if Self::is_empty_array_assignment(node) {
                    let never_type = self.never_type;
                    return flow_type_of(self.get_evolving_array_type(never_type));
                }
                let initial_or_assigned = self.get_initial_or_assigned_type(f, flow);
                let assigned_type = self.get_widened_literal_type(initial_or_assigned);
                if self.is_type_assignable_to(assigned_type, f.declared()) {
                    return flow_type_of(assigned_type);
                }
                return flow_type_of(self.any_array_type);
            }
            let mut t = f.declared();
            if is_in_compound_like_assignment(node) {
                t = self.get_base_type_of_literal_type(t);
            }
            if t.flags().intersects(TypeFlags::Union) {
                let initial_or_assigned = self.get_initial_or_assigned_type(f, flow);
                return flow_type_of(self.get_assignment_reduced_type(t, initial_or_assigned));
            }
            return flow_type_of(t);
        }
        // We didn't have a direct match. However, if the reference is a dotted name, this
        // may be an assignment to a left hand part of the reference. For example, for a
        // reference 'x.y.z', we may be at an assignment to 'x.y' or 'x'. In that case,
        // return the declared type.
        if self.contains_matching_reference(f.ref_node(), node) {
            if !self.is_reachable_flow_node(flow) {
                return flow_type_of(self.unreachable_never_type);
            }
            // A matching dotted name might also be an expando property on a function *expression*,
            // in which case we continue control flow analysis back to the function's declaration
            if ast::is_variable_declaration(node)
                && (ast::is_in_js_file(node) || ast::is_var_const_like(node))
                && let Some(init) = node.initializer()
                && ast::is_function_expression_or_arrow_function(init)
            {
                return self.get_type_at_flow_node(f, flow.antecedent().unwrap());
            }
            return flow_type_of(f.declared());
        }
        // for (const _ in ref) acts as a nonnull on ref
        if ast::is_variable_declaration(node)
            && ast::is_for_in_statement(node.parent().unwrap().parent().unwrap())
        {
            let for_in_expression = node.parent().unwrap().parent().unwrap().expression().unwrap();
            if self.is_matching_reference(f.ref_node(), for_in_expression)
                || self.optional_chain_contains_reference(for_in_expression, f.ref_node())
            {
                let antecedent = self.get_type_at_flow_node(f, flow.antecedent().unwrap());
                if f.memo_aborted.get() {
                    return antecedent;
                }
                let antecedent_type = antecedent.t.unwrap();
                let finalized = self.finalize_evolving_array_type(antecedent_type);
                return flow_type_of(self.get_non_nullable_type_if_needed(finalized));
            }
        }
        // Assignment doesn't affect reference
        FlowType::default()
    }

    // flow.go:276
    fn get_initial_or_assigned_type(&mut self, f: P<FlowState>, flow: P<FlowNode>) -> P<Type> {
        let node = flow.node().unwrap();
        let t = if ast::is_variable_declaration(node) || ast::is_binding_element(node) {
            self.get_initial_type(node)
        } else {
            self.get_assigned_type(node)
        };
        let (narrowable, position_read) =
            self.get_narrowable_type_for_flow_reference(t, f.ref_node());
        if position_read {
            // Flow memo: the answer depends on where the reference is, not only on its key.
            self.flow_memo.taint_reference(f.walk_floor.get());
        }
        narrowable
    }

    /// `isConstantReference(f.reference)`. For an access expression it reads the property symbol that the reference
    /// node resolved to, which another reference with the same key may not share.
    fn is_constant_reference_of_walk(&mut self, f: P<FlowState>) -> bool {
        let reference = f.ref_node();
        if ast::is_access_expression(reference) {
            self.flow_memo.taint_reference(f.walk_floor.get());
        }
        self.is_constant_reference(reference)
    }

    // flow.go:283
    fn is_empty_array_assignment(node: P<Node>) -> bool {
        ast::is_variable_declaration(node)
            && node.initializer().is_some()
            && is_empty_array_literal(node.initializer().unwrap())
            || !ast::is_binding_element(node)
                && ast::is_binary_expression(node.parent().unwrap())
                && is_empty_array_literal(node.parent().unwrap().as_binary_expression().right())
    }

    // flow.go:288
    fn get_type_at_flow_call(&mut self, f: P<FlowState>, flow: P<FlowNode>) -> FlowType {
        let node = flow.node().unwrap();
        let signature = self.get_effects_signature(node);
        if let Some(signature) = signature {
            let predicate = self.get_type_predicate_of_signature(signature);
            if let Some(predicate) = predicate
                && (predicate.kind.get() == TypePredicateKind::AssertsThis
                    || predicate.kind.get() == TypePredicateKind::AssertsIdentifier)
            {
                let flow_type = self.get_type_at_flow_node(f, flow.antecedent().unwrap());
                if f.memo_aborted.get() {
                    return flow_type;
                }
                let t = self.finalize_evolving_array_type(flow_type.t.unwrap());
                let parameter_index = predicate.parameter_index.get();
                let narrowed_type = if predicate.t.get().is_some() {
                    self.narrow_type_by_type_predicate(
                        f, t, predicate, node, true, /*assumeTrue*/
                    )
                } else if predicate.kind.get() == TypePredicateKind::AssertsIdentifier
                    && parameter_index
                        .is_some_and(|parameter_index| parameter_index < node.arguments().len())
                {
                    self.narrow_type_by_assertion(f, t, node.arguments()[parameter_index.unwrap()])
                } else {
                    t
                };
                if narrowed_type == t {
                    return flow_type;
                }
                return self.new_flow_type(narrowed_type, flow_type.incomplete);
            }
            if self.get_return_type_of_signature(signature).flags().intersects(TypeFlags::Never) {
                return flow_type_of(self.unreachable_never_type);
            }
        }
        FlowType::default()
    }

    // flow.go:316
    fn narrow_type_by_type_predicate(
        &mut self,
        f: P<FlowState>,
        t: P<Type>,
        predicate: P<TypePredicate>,
        call_expression: P<Node>,
        assume_true: bool,
    ) -> P<Type> {
        let mut t = t;
        // Don't narrow from 'any' if the predicate type is exactly 'Object' or 'Function'
        if let Some(predicate_type) = predicate.t.get()
            && !(is_type_any(Some(t))
                && (predicate_type == self.global_object_type
                    || predicate_type == self.global_function_type))
        {
            let predicate_argument = Self::get_type_predicate_argument(predicate, call_expression);
            if let Some(predicate_argument) = predicate_argument {
                if self.is_matching_reference(f.ref_node(), predicate_argument) {
                    return self.get_narrowed_type(
                        t,
                        predicate_type,
                        assume_true,
                        false, /*checkDerived*/
                    );
                }
                if self.strict_null_checks
                    && self.optional_chain_contains_reference(predicate_argument, f.ref_node())
                    && (assume_true && !self.has_type_facts(predicate_type, TypeFacts::EQUndefined)
                        || !assume_true
                            && every_type(
                                self,
                                predicate_type,
                                super::checker::Checker::is_nullable_type,
                            ))
                {
                    t = self.get_adjusted_type_with_facts(t, TypeFacts::NEUndefinedOrNull);
                }
                let access = self.get_discriminant_property_access(f, predicate_argument, t);
                if let Some(access) = access {
                    return self.narrow_type_by_discriminant(t, access, move |c, t| {
                        c.get_narrowed_type(
                            t,
                            predicate_type,
                            assume_true,
                            false, /*checkDerived*/
                        )
                    });
                }
            }
        }
        t
    }

    // flow.go:338
    fn narrow_type_by_assertion(&mut self, f: P<FlowState>, t: P<Type>, expr: P<Node>) -> P<Type> {
        let node = ast::skip_parentheses(expr);
        if node.kind() == Kind::FalseKeyword {
            return self.unreachable_never_type;
        }
        if node.kind() == Kind::BinaryExpression {
            let binary = node.as_binary_expression();
            if binary.operator_token.kind() == Kind::AmpersandAmpersandToken {
                let left_type = self.narrow_type_by_assertion(f, t, binary.left);
                return self.narrow_type_by_assertion(f, left_type, binary.right());
            }
            if binary.operator_token.kind() == Kind::BarBarToken {
                let left_type = self.narrow_type_by_assertion(f, t, binary.left);
                let right_type = self.narrow_type_by_assertion(f, t, binary.right());
                return self.get_union_type(&[left_type, right_type]);
            }
        }
        self.narrow_type(f, t, node, true /*assumeTrue*/)
    }

    // flow.go:354
    fn get_type_at_flow_condition(&mut self, f: P<FlowState>, flow: P<FlowNode>) -> FlowType {
        let flow_type = self.get_type_at_flow_node(f, flow.antecedent().unwrap());
        if f.memo_aborted.get() || flow_type.t.unwrap().flags().intersects(TypeFlags::Never) {
            return flow_type;
        }
        // If we have an antecedent type (meaning we're reachable in some way), we first
        // attempt to narrow the antecedent type. If that produces the never type, and if
        // the antecedent type is incomplete (i.e. a transient type in a loop), then we
        // take the type guard as an indication that control *could* reach here once we
        // have the complete type. We proceed by switching to the silent never type which
        // doesn't report errors when operators are applied to it. Note that this is the
        // *only* place a silent never type is ever generated.
        let assume_true = flow.flags().intersects(FlowFlags::TrueCondition);
        let non_evolving_type = self.finalize_evolving_array_type(flow_type.t.unwrap());
        let narrowed_type =
            self.narrow_type(f, non_evolving_type, flow.node().unwrap(), assume_true);
        if narrowed_type == non_evolving_type {
            return flow_type;
        }
        self.new_flow_type(narrowed_type, flow_type.incomplete)
    }

    // Narrow the given type based on the given expression having the assumed boolean value. The returned type
    // will be a subtype or the same type as the argument.
    // flow.go:377
    fn narrow_type(
        &mut self,
        f: P<FlowState>,
        t: P<Type>,
        expr: P<Node>,
        assume_true: bool,
    ) -> P<Type> {
        // for `a?.b`, we emulate a synthetic `a !== null && a !== undefined` condition for `a`
        if ast::is_expression_of_optional_chain_root(expr) || {
            let parent = expr.parent().unwrap();
            ast::is_binary_expression(parent)
                && (parent.as_binary_expression().operator_token.kind()
                    == Kind::QuestionQuestionToken
                    || parent.as_binary_expression().operator_token.kind()
                        == Kind::QuestionQuestionEqualsToken)
                && parent.as_binary_expression().left == expr
        } {
            return self.narrow_type_by_optionality(f, t, expr, assume_true);
        }
        match expr.kind() {
            Kind::Identifier
            | Kind::ThisKeyword
            | Kind::SuperKeyword
            | Kind::PropertyAccessExpression
            | Kind::ElementAccessExpression => {
                if expr.kind() == Kind::Identifier {
                    // When narrowing a reference to a const variable, non-assigned parameter, or readonly property, we inline
                    // up to five levels of aliased conditional expressions that are themselves declared as const variables.
                    if !self.is_matching_reference(f.ref_node(), expr) {
                        if self.inline_level < 5 {
                            let symbol = self.get_resolved_symbol(expr);
                            if self.is_constant_variable(symbol) {
                                let declaration = symbol.value_declaration();
                                if let Some(declaration) = declaration
                                    && ast::is_variable_declaration(declaration)
                                    && declaration.type_node().is_none()
                                    && declaration.initializer().is_some()
                                    && self.is_constant_reference_of_walk(f)
                                {
                                    self.inline_level += 1;
                                    let result = self.narrow_type(
                                        f,
                                        t,
                                        declaration.initializer().unwrap(),
                                        assume_true,
                                    );
                                    self.inline_level -= 1;
                                    return result;
                                }
                            }
                        } else {
                            // Flow memo: whether this inlines depends on how deep the inlining already is, which no
                            // key has.
                            self.flow_memo.taint(0);
                        }
                    }
                }
                return self.narrow_type_by_truthiness(f, t, expr, assume_true);
            }
            Kind::CallExpression => {
                return self.narrow_type_by_call_expression(f, t, expr, assume_true);
            }
            Kind::ParenthesizedExpression | Kind::NonNullExpression | Kind::SatisfiesExpression => {
                return self.narrow_type(f, t, expr.expression().unwrap(), assume_true);
            }
            Kind::BinaryExpression => {
                return self.narrow_type_by_binary_expression(f, t, expr, assume_true);
            }
            Kind::PrefixUnaryExpression
                if expr.as_prefix_unary_expression().operator == Kind::ExclamationToken =>
            {
                return self.narrow_type(
                    f,
                    t,
                    expr.as_prefix_unary_expression().operand,
                    !assume_true,
                );
            }
            _ => {}
        }
        t
    }

    // flow.go:415
    fn narrow_type_by_optionality(
        &mut self,
        f: P<FlowState>,
        t: P<Type>,
        expr: P<Node>,
        assume_present: bool,
    ) -> P<Type> {
        if self.is_matching_reference(f.ref_node(), expr) {
            return self.get_adjusted_type_with_facts(
                t,
                if assume_present {
                    TypeFacts::NEUndefinedOrNull
                } else {
                    TypeFacts::EQUndefinedOrNull
                },
            );
        }
        let access = self.get_discriminant_property_access(f, expr, t);
        if let Some(access) = access {
            return self.narrow_type_by_discriminant(t, access, move |c, t| {
                c.get_type_with_facts(
                    t,
                    if assume_present {
                        TypeFacts::NEUndefinedOrNull
                    } else {
                        TypeFacts::EQUndefinedOrNull
                    },
                )
            });
        }
        t
    }

    // flow.go:428
    fn narrow_type_by_truthiness(
        &mut self,
        f: P<FlowState>,
        t: P<Type>,
        expr: P<Node>,
        assume_true: bool,
    ) -> P<Type> {
        let mut t = t;
        if self.is_matching_reference(f.ref_node(), expr) {
            return self.get_adjusted_type_with_facts(
                t,
                if assume_true { TypeFacts::Truthy } else { TypeFacts::Falsy },
            );
        }
        if self.strict_null_checks
            && assume_true
            && self.optional_chain_contains_reference(expr, f.ref_node())
        {
            t = self.get_adjusted_type_with_facts(t, TypeFacts::NEUndefinedOrNull);
        }
        let access = self.get_discriminant_property_access(f, expr, t);
        if let Some(access) = access {
            return self.narrow_type_by_discriminant(t, access, move |c, t| {
                c.get_type_with_facts(
                    t,
                    if assume_true { TypeFacts::Truthy } else { TypeFacts::Falsy },
                )
            });
        }
        t
    }

    // flow.go:444
    fn narrow_type_by_call_expression(
        &mut self,
        f: P<FlowState>,
        t: P<Type>,
        call_expression: P<Node>,
        assume_true: bool,
    ) -> P<Type> {
        if self.has_matching_argument(call_expression, f.ref_node()) {
            let mut predicate: Option<P<TypePredicate>> = None;
            if assume_true || !is_call_chain(call_expression) {
                let signature = self.get_effects_signature(call_expression);
                if let Some(signature) = signature {
                    predicate = self.get_type_predicate_of_signature(signature);
                }
            }
            if let Some(predicate) = predicate
                && (predicate.kind.get() == TypePredicateKind::This
                    || predicate.kind.get() == TypePredicateKind::Identifier)
            {
                return self.narrow_type_by_type_predicate(
                    f,
                    t,
                    predicate,
                    call_expression,
                    assume_true,
                );
            }
        }
        let reference = f.ref_node();
        if self.contains_missing_type(t)
            && ast::is_access_expression(reference)
            && ast::is_property_access_expression(call_expression.expression().unwrap())
        {
            let call_access = call_expression.expression().unwrap();
            let candidate = Self::get_reference_candidate(call_access.expression().unwrap());
            if self.is_matching_reference(reference.expression().unwrap(), candidate)
                && ast::is_identifier(call_access.name().unwrap())
                && call_access.name().unwrap().text() == "hasOwnProperty"
                && call_expression.arguments().len() == 1
            {
                let argument = call_expression.arguments()[0];
                let (accessed_name, ok) = self.get_accessed_property_name(reference);
                if ok && ast::is_string_literal_like(argument) && accessed_name == argument.text() {
                    return self.get_type_with_facts(
                        t,
                        if assume_true { TypeFacts::NEUndefined } else { TypeFacts::EQUndefined },
                    );
                }
            }
        }
        t
    }

    // flow.go:469
    fn narrow_type_by_binary_expression(
        &mut self,
        f: P<FlowState>,
        t: P<Type>,
        expr: P<Node>,
        assume_true: bool,
    ) -> P<Type> {
        let mut t = t;
        let binary = expr.as_binary_expression();
        match binary.operator_token.kind() {
            Kind::EqualsToken
            | Kind::BarBarEqualsToken
            | Kind::AmpersandAmpersandEqualsToken
            | Kind::QuestionQuestionEqualsToken => {
                let narrowed = self.narrow_type(f, t, binary.right(), assume_true);
                return self.narrow_type_by_truthiness(f, narrowed, binary.left, assume_true);
            }
            Kind::EqualsEqualsToken
            | Kind::ExclamationEqualsToken
            | Kind::EqualsEqualsEqualsToken
            | Kind::ExclamationEqualsEqualsToken => {
                let operator = binary.operator_token.kind();
                let left = Self::get_reference_candidate(binary.left);
                let right = Self::get_reference_candidate(binary.right());
                if left.kind() == Kind::TypeOfExpression && ast::is_string_literal_like(right) {
                    return self.narrow_type_by_typeof(f, t, left, operator, right, assume_true);
                }
                if right.kind() == Kind::TypeOfExpression && ast::is_string_literal_like(left) {
                    return self.narrow_type_by_typeof(f, t, right, operator, left, assume_true);
                }
                if self.is_matching_reference(f.ref_node(), left) {
                    return self.narrow_type_by_equality(t, operator, right, assume_true);
                }
                if self.is_matching_reference(f.ref_node(), right) {
                    return self.narrow_type_by_equality(t, operator, left, assume_true);
                }
                if self.strict_null_checks {
                    if self.optional_chain_contains_reference(left, f.ref_node()) {
                        t = self.narrow_type_by_optional_chain_containment(
                            f,
                            t,
                            operator,
                            right,
                            assume_true,
                        );
                    } else if self.optional_chain_contains_reference(right, f.ref_node()) {
                        t = self.narrow_type_by_optional_chain_containment(
                            f,
                            t,
                            operator,
                            left,
                            assume_true,
                        );
                    }
                }
                let left_access = self.get_discriminant_property_access(f, left, t);
                if let Some(left_access) = left_access {
                    return self.narrow_type_by_discriminant_property(
                        t,
                        left_access,
                        operator,
                        right,
                        assume_true,
                    );
                }
                let right_access = self.get_discriminant_property_access(f, right, t);
                if let Some(right_access) = right_access {
                    return self.narrow_type_by_discriminant_property(
                        t,
                        right_access,
                        operator,
                        left,
                        assume_true,
                    );
                }
                if self.is_matching_constructor_reference(f, left) {
                    return self.narrow_type_by_constructor(t, operator, right, assume_true);
                }
                if self.is_matching_constructor_reference(f, right) {
                    return self.narrow_type_by_constructor(t, operator, left, assume_true);
                }
                if ast::is_boolean_literal(right) && !ast::is_access_expression(left) {
                    return self.narrow_type_by_boolean_comparison(
                        f,
                        t,
                        left,
                        right,
                        operator,
                        assume_true,
                    );
                }
                if ast::is_boolean_literal(left) && !ast::is_access_expression(right) {
                    return self.narrow_type_by_boolean_comparison(
                        f,
                        t,
                        right,
                        left,
                        operator,
                        assume_true,
                    );
                }
            }
            Kind::InstanceOfKeyword => {
                return self.narrow_type_by_instanceof(f, t, expr, assume_true);
            }
            Kind::InKeyword => {
                if ast::is_private_identifier(binary.left) {
                    return self.narrow_type_by_private_identifier_in_in_expression(
                        f,
                        t,
                        expr,
                        assume_true,
                    );
                }
                let target = Self::get_reference_candidate(binary.right());
                let reference = f.ref_node();
                if self.contains_missing_type(t)
                    && ast::is_access_expression(reference)
                    && self.is_matching_reference(reference.expression().unwrap(), target)
                {
                    let left_type = self.get_type_of_expression(binary.left);
                    if is_type_usable_as_property_name(left_type) {
                        let (accessed_name, ok) = self.get_accessed_property_name(reference);
                        if ok && accessed_name == get_property_name_from_type(left_type) {
                            return self.get_type_with_facts(
                                t,
                                if assume_true {
                                    TypeFacts::NEUndefined
                                } else {
                                    TypeFacts::EQUndefined
                                },
                            );
                        }
                    }
                }
                if self.is_matching_reference(f.ref_node(), target) {
                    let left_type = self.get_type_of_expression(binary.left);
                    if is_type_usable_as_property_name(left_type) {
                        return self.narrow_type_by_in_keyword(f, t, left_type, assume_true);
                    }
                }
            }
            Kind::CommaToken => {
                return self.narrow_type(f, t, binary.right(), assume_true);
            }
            Kind::AmpersandAmpersandToken => {
                // Ordinarily we won't see && and || expressions in control flow analysis because the Binder breaks those
                // expressions down to individual conditional control flows. However, we may encounter them when analyzing
                // aliased conditional expressions.
                if assume_true {
                    let left_type = self.narrow_type(f, t, binary.left, true /*assumeTrue*/);
                    return self.narrow_type(
                        f,
                        left_type,
                        binary.right(),
                        true, /*assumeTrue*/
                    );
                }
                let left_type = self.narrow_type(f, t, binary.left, false /*assumeTrue*/);
                let right_type = self.narrow_type(f, t, binary.right(), false /*assumeTrue*/);
                return self.get_union_type(&[left_type, right_type]);
            }
            Kind::BarBarToken => {
                if assume_true {
                    let left_type = self.narrow_type(f, t, binary.left, true /*assumeTrue*/);
                    let right_type =
                        self.narrow_type(f, t, binary.right(), true /*assumeTrue*/);
                    return self.get_union_type(&[left_type, right_type]);
                }
                let left_type = self.narrow_type(f, t, binary.left, false /*assumeTrue*/);
                return self.narrow_type(f, left_type, binary.right(), false /*assumeTrue*/);
            }
            _ => {}
        }
        t
    }

    // flow.go:556
    fn narrow_type_by_equality(
        &mut self,
        t: P<Type>,
        operator: Kind,
        value: P<Node>,
        assume_true: bool,
    ) -> P<Type> {
        let mut assume_true = assume_true;
        if t.flags().intersects(TypeFlags::Any) {
            return t;
        }
        if operator == Kind::ExclamationEqualsToken
            || operator == Kind::ExclamationEqualsEqualsToken
        {
            assume_true = !assume_true;
        }
        let value_type = self.get_type_of_expression(value);
        let double_equals =
            operator == Kind::EqualsEqualsToken || operator == Kind::ExclamationEqualsToken;
        if value_type.flags().intersects(TypeFlags::Nullable) {
            if !self.strict_null_checks {
                return t;
            }
            let facts = if double_equals {
                if assume_true {
                    TypeFacts::EQUndefinedOrNull
                } else {
                    TypeFacts::NEUndefinedOrNull
                }
            } else if value_type.flags().intersects(TypeFlags::Null) {
                if assume_true { TypeFacts::EQNull } else { TypeFacts::NENull }
            } else if assume_true {
                TypeFacts::EQUndefined
            } else {
                TypeFacts::NEUndefined
            };
            return self.get_adjusted_type_with_facts(t, facts);
        }
        if assume_true {
            if !double_equals
                && (t.flags().intersects(TypeFlags::Unknown)
                    || some_type(self, t, super::checker::Checker::is_empty_anonymous_object_type))
            {
                if value_type.flags().intersects(TypeFlags::Primitive | TypeFlags::NonPrimitive)
                    || self.is_empty_anonymous_object_type(value_type)
                {
                    return value_type;
                }
                if value_type.flags().intersects(TypeFlags::Object) {
                    return self.non_primitive_type;
                }
            }
            if !double_equals
                && value_type.flags().intersects(TypeFlags::Primitive)
                && self.is_uniform_union_type(t)
            {
                let regular_type = self.get_regular_type_of_literal_type(value_type);
                if self.union_contains_type(t, regular_type, false /*matchSymbol*/) {
                    return regular_type;
                }
            }
            let filtered_type = self.filter_type(t, move |c, t| {
                c.are_types_comparable(t, value_type)
                    || double_equals && is_coercible_under_double_equals(t, value_type)
            });
            return self.replace_primitives_with_literals(filtered_type, value_type);
        }
        if is_unit_type(value_type) {
            if self.is_uniform_union_type(t) {
                let regular_type = self.get_regular_type_of_literal_type(value_type);
                let filtered_type = self.remove_type(t, regular_type);
                if filtered_type != t {
                    return filtered_type;
                }
            }
            return self.filter_type(t, move |c, t| {
                !(c.is_unit_like_type(t) && c.are_types_comparable(t, value_type))
            });
        }
        t
    }

    // flow.go:614
    fn narrow_type_by_typeof(
        &mut self,
        f: P<FlowState>,
        t: P<Type>,
        type_of_expr: P<Node>,
        operator: Kind,
        literal: P<Node>,
        assume_true: bool,
    ) -> P<Type> {
        let mut t = t;
        let mut assume_true = assume_true;
        // We have '==', '!=', '===', or !==' operator with 'typeof xxx' and string literal operands
        if operator == Kind::ExclamationEqualsToken
            || operator == Kind::ExclamationEqualsEqualsToken
        {
            assume_true = !assume_true;
        }
        let target = Self::get_reference_candidate(type_of_expr.as_type_of_expression().expression);
        if !self.is_matching_reference(f.ref_node(), target) {
            if self.strict_null_checks
                && self.optional_chain_contains_reference(target, f.ref_node())
                && assume_true == (literal.text() != "undefined")
            {
                t = self.get_adjusted_type_with_facts(t, TypeFacts::NEUndefinedOrNull);
            }
            let property_access = self.get_discriminant_property_access(f, target, t);
            if let Some(property_access) = property_access {
                return self.narrow_type_by_discriminant(t, property_access, move |c, t| {
                    c.narrow_type_by_literal_expression(t, literal, assume_true)
                });
            }
            return t;
        }
        self.narrow_type_by_literal_expression(t, literal, assume_true)
    }

    // flow.go:646
    fn narrow_type_by_literal_expression(
        &mut self,
        t: P<Type>,
        literal: P<Node>,
        assume_true: bool,
    ) -> P<Type> {
        if assume_true {
            return self.narrow_type_by_type_name(t, literal.text());
        }
        let facts = match TYPEOF_NE_FACTS.get(literal.text()) {
            Some(&facts) => facts,
            None => TypeFacts::TypeofNEHostObject,
        };
        self.get_adjusted_type_with_facts(t, facts)
    }

    // flow.go:657
    fn narrow_type_by_type_name(&mut self, t: P<Type>, type_name: &str) -> P<Type> {
        match type_name {
            "string" => {
                return self.narrow_type_by_type_facts(
                    t,
                    self.string_type,
                    TypeFacts::TypeofEQString,
                );
            }
            "number" => {
                return self.narrow_type_by_type_facts(
                    t,
                    self.number_type,
                    TypeFacts::TypeofEQNumber,
                );
            }
            "bigint" => {
                return self.narrow_type_by_type_facts(
                    t,
                    self.bigint_type,
                    TypeFacts::TypeofEQBigInt,
                );
            }
            "boolean" => {
                return self.narrow_type_by_type_facts(
                    t,
                    self.boolean_type,
                    TypeFacts::TypeofEQBoolean,
                );
            }
            "symbol" => {
                return self.narrow_type_by_type_facts(
                    t,
                    self.es_symbol_type,
                    TypeFacts::TypeofEQSymbol,
                );
            }
            "object" => {
                if t.flags().intersects(TypeFlags::Any) {
                    return t;
                }
                let object_type = self.narrow_type_by_type_facts(
                    t,
                    self.non_primitive_type,
                    TypeFacts::TypeofEQObject,
                );
                let null_type =
                    self.narrow_type_by_type_facts(t, self.null_type, TypeFacts::EQNull);
                return self.get_union_type(&[object_type, null_type]);
            }
            "function" => {
                if t.flags().intersects(TypeFlags::Any) {
                    return t;
                }
                return self.narrow_type_by_type_facts(
                    t,
                    self.global_function_type,
                    TypeFacts::TypeofEQFunction,
                );
            }
            "undefined" => {
                return self.narrow_type_by_type_facts(
                    t,
                    self.undefined_type,
                    TypeFacts::EQUndefined,
                );
            }
            _ => {}
        }
        self.narrow_type_by_type_facts(t, self.non_primitive_type, TypeFacts::TypeofEQHostObject)
    }

    // flow.go:685
    fn narrow_type_by_type_facts(
        &mut self,
        t: P<Type>,
        implied_type: P<Type>,
        facts: TypeFacts,
    ) -> P<Type> {
        self.map_type(t, move |c, t| {
            if c.is_type_related_to(t, implied_type, c.strict_subtype_relation) {
                if c.has_type_facts(t, facts) {
                    return Some(t);
                }
                return Some(c.never_type);
            } else if c.is_type_subtype_of(implied_type, t) {
                return Some(implied_type);
            } else if c.has_type_facts(t, facts) {
                return Some(c.get_intersection_type(&[t, implied_type]));
            }
            Some(c.never_type)
        })
        .unwrap()
    }

    // flow.go:702
    fn narrow_type_by_discriminant_property(
        &mut self,
        t: P<Type>,
        access: P<Node>,
        operator: Kind,
        value: P<Node>,
        assume_true: bool,
    ) -> P<Type> {
        if (operator == Kind::EqualsEqualsEqualsToken
            || operator == Kind::ExclamationEqualsEqualsToken)
            && t.flags().intersects(TypeFlags::Union)
        {
            let key_property_name = self.get_key_property_name(t);
            if !key_property_name.is_empty() {
                let (accessed_name, ok) = self.get_accessed_property_name(access);
                if ok && key_property_name == accessed_name {
                    let value_type = self.get_type_of_expression(value);
                    let candidate = self.get_constituent_type_for_key_type(t, value_type);
                    if let Some(candidate) = candidate {
                        if assume_true && operator == Kind::EqualsEqualsEqualsToken
                            || !assume_true && operator == Kind::ExclamationEqualsEqualsToken
                        {
                            return candidate;
                        }
                        if let Some(prop_type) =
                            self.get_type_of_property_of_type(candidate, &key_property_name)
                            && is_unit_type(prop_type)
                        {
                            return self.remove_type(t, candidate);
                        }
                        return t;
                    }
                }
            }
        }
        self.narrow_type_by_discriminant(t, access, move |c, t| {
            c.narrow_type_by_equality(t, operator, value, assume_true)
        })
    }

    // flow.go:725
    fn narrow_type_by_discriminant(
        &mut self,
        t: P<Type>,
        access: P<Node>,
        narrow_type: impl FnMut(&mut Checker, P<Type>) -> P<Type>,
    ) -> P<Type> {
        let mut narrow_type = narrow_type;
        let (prop_name, ok) = self.get_accessed_property_name(access);
        if !ok {
            return t;
        }
        let optional_chain = ast::is_optional_chain(access);
        let remove_nullable = self.strict_null_checks
            && (optional_chain || is_non_null_access(access))
            && self.maybe_type_of_kind(t, TypeFlags::Nullable);
        let mut non_null_type = t;
        if remove_nullable {
            non_null_type = self.get_type_with_facts(t, TypeFacts::NEUndefinedOrNull);
        }
        let Some(mut prop_type) = self.get_type_of_property_of_type(non_null_type, &prop_name)
        else {
            return t;
        };
        if remove_nullable && optional_chain {
            prop_type = self.get_optional_type(prop_type, false);
        }
        let narrowed_prop_type = narrow_type(self, prop_type);
        self.filter_type(t, move |c, t| {
            let discriminant_type = c
                .get_type_of_property_or_index_signature_of_type(t, &prop_name)
                .unwrap_or(c.unknown_type);
            !discriminant_type.flags().intersects(TypeFlags::Never)
                && !narrowed_prop_type.flags().intersects(TypeFlags::Never)
                && c.are_types_comparable(narrowed_prop_type, discriminant_type)
        })
    }

    // flow.go:750
    fn is_matching_constructor_reference(&mut self, f: P<FlowState>, expr: P<Node>) -> bool {
        let mut name: Option<P<Node>> = None;
        if ast::is_property_access_expression(expr) {
            name = expr.name();
        } else if ast::is_element_access_expression(expr)
            && ast::is_string_literal_like(expr.as_element_access_expression().argument_expression)
        {
            name = Some(expr.as_element_access_expression().argument_expression);
        }
        name.is_some()
            && name.unwrap().text() == "constructor"
            && self.is_matching_reference(f.ref_node(), expr.expression().unwrap())
    }

    // flow.go:760
    fn narrow_type_by_constructor(
        &mut self,
        t: P<Type>,
        operator: Kind,
        identifier: P<Node>,
        assume_true: bool,
    ) -> P<Type> {
        // Do not narrow when checking inequality.
        if assume_true
            && operator != Kind::EqualsEqualsToken
            && operator != Kind::EqualsEqualsEqualsToken
            || !assume_true
                && operator != Kind::ExclamationEqualsToken
                && operator != Kind::ExclamationEqualsEqualsToken
        {
            return t;
        }
        // Get the type of the constructor identifier expression, if it is not a function then do not narrow.
        let identifier_type = self.get_type_of_expression(identifier);
        if !self.is_function_type(identifier_type) && !self.is_constructor_type(identifier_type) {
            return t;
        }
        // Get the prototype property of the type identifier so we can find out its type.
        let Some(prototype_property) = self.get_property_of_type(identifier_type, "prototype")
        else {
            return t;
        };
        // Get the type of the prototype, if it is undefined, or the global `Object` or `Function` types then do not narrow.
        let prototype_type = self.get_type_of_symbol(prototype_property);
        let mut candidate: Option<P<Type>> = None;
        if !is_type_any(Some(prototype_type)) {
            candidate = Some(prototype_type);
        }
        let candidate = match candidate {
            Some(candidate)
                if candidate != self.global_object_type
                    && candidate != self.global_function_type =>
            {
                candidate
            }
            _ => return t,
        };
        // If the type that is being narrowed is `any` then just return the `candidate` type since every type is a subtype of `any`.
        if is_type_any(Some(t)) {
            return candidate;
        }
        // Filter out types that are not considered to be "constructed by" the `candidate` type.
        self.filter_type(t, move |c, t| c.is_constructed_by(t, candidate))
    }

    // flow.go:794
    fn is_constructed_by(&mut self, source: P<Type>, target: P<Type>) -> bool {
        // If either the source or target type are a class type then we need to check that they are the same exact type.
        // This is because you may have a class `A` that defines some set of properties, and another class `B`
        // that defines the same set of properties as class `A`, in that case they are structurally the same
        // type, but when you do something like `instanceOfA.constructor === B` it will return false.
        if source.flags().intersects(TypeFlags::Object)
            && source.object_flags().intersects(ObjectFlags::Class)
            || target.flags().intersects(TypeFlags::Object)
                && target.object_flags().intersects(ObjectFlags::Class)
        {
            return source.symbol() == target.symbol();
        }
        // For all other types just check that the `source` type is a subtype of the `target` type.
        self.is_type_subtype_of(source, target)
    }

    // flow.go:806
    fn narrow_type_by_boolean_comparison(
        &mut self,
        f: P<FlowState>,
        t: P<Type>,
        expr: P<Node>,
        bool_value: P<Node>,
        operator: Kind,
        assume_true: bool,
    ) -> P<Type> {
        let assume_true = (assume_true != (bool_value.kind() == Kind::TrueKeyword))
            != (operator != Kind::ExclamationEqualsEqualsToken
                && operator != Kind::ExclamationEqualsToken);
        self.narrow_type(f, t, expr, assume_true)
    }

    // flow.go:811
    fn narrow_type_by_instanceof(
        &mut self,
        f: P<FlowState>,
        t: P<Type>,
        expr: P<Node>,
        assume_true: bool,
    ) -> P<Type> {
        let binary = expr.as_binary_expression();
        let left = Self::get_reference_candidate(binary.left);
        if !self.is_matching_reference(f.ref_node(), left) {
            if assume_true
                && self.strict_null_checks
                && self.optional_chain_contains_reference(left, f.ref_node())
            {
                return self.get_adjusted_type_with_facts(t, TypeFacts::NEUndefinedOrNull);
            }
            return t;
        }
        let right = binary.right();
        let right_type = self.get_type_of_expression(right);
        if !self.is_type_derived_from(right_type, self.global_object_type) {
            return t;
        }
        // if the right-hand side has an object type with a custom `[Symbol.hasInstance]` method, and that method
        // has a type predicate, use the type predicate to perform narrowing. This allows normal `object` types to
        // participate in `instanceof`, as per Step 2 of https://tc39.es/ecma262/#sec-instanceofoperator.
        let mut predicate: Option<P<TypePredicate>> = None;
        if let Some(signature) = self.get_effects_signature(expr) {
            predicate = self.get_type_predicate_of_signature(signature);
        }
        if let Some(predicate) = predicate
            && predicate.kind.get() == TypePredicateKind::Identifier
            && predicate.parameter_index.get() == Some(0)
        {
            return self.get_narrowed_type(
                t,
                predicate.t.get().unwrap(),
                assume_true,
                true, /*checkDerived*/
            );
        }
        if !self.is_type_derived_from(right_type, self.global_function_type) {
            return t;
        }
        let instance_type = self.map_type(right_type, |c, t| Some(c.get_instance_type(t))).unwrap();
        // Don't narrow from `any` if the target type is exactly `Object` or `Function`, and narrow
        // in the false branch only if the target is a non-empty object type.
        if is_type_any(Some(t))
            && (instance_type == self.global_object_type
                || instance_type == self.global_function_type)
            || !(assume_true
                || instance_type.flags().intersects(TypeFlags::Object)
                    && !self.is_empty_anonymous_object_type(instance_type))
        {
            return t;
        }
        self.get_narrowed_type(t, instance_type, assume_true, true /*checkDerived*/)
    }

    // flow.go:846
    fn get_narrowed_type(
        &mut self,
        t: P<Type>,
        candidate: P<Type>,
        assume_true: bool,
        check_derived: bool,
    ) -> P<Type> {
        if !t.flags().intersects(TypeFlags::Union) {
            return self.get_narrowed_type_worker(t, candidate, assume_true, check_derived);
        }
        let key = NarrowedTypeKey { t, candidate, assume_true, check_derived };
        if let Some(&narrowed_type) = self.narrowed_types.get(&key) {
            return narrowed_type;
        }
        let narrowed_type = self.get_narrowed_type_worker(t, candidate, assume_true, check_derived);
        self.narrowed_types.insert(key, narrowed_type);
        narrowed_type
    }

    // flow.go:859
    fn get_narrowed_type_worker(
        &mut self,
        t: P<Type>,
        candidate: P<Type>,
        assume_true: bool,
        check_derived: bool,
    ) -> P<Type> {
        let mut t = t;
        if !assume_true {
            if t == candidate {
                return self.never_type;
            }
            if check_derived {
                return self.filter_type(t, move |c, t| !c.is_type_derived_from(t, candidate));
            }
            if t.flags().intersects(TypeFlags::Unknown) {
                t = self.unknown_union_type;
            }
            let true_type = self.get_narrowed_type(
                t, candidate, true,  /*assumeTrue*/
                false, /*checkDerived*/
            );
            let filtered = self.filter_type(t, move |c, t| !c.is_type_subset_of(t, true_type));
            return self.recombine_unknown_type(filtered);
        }
        if t.flags().intersects(TypeFlags::AnyOrUnknown) {
            return candidate;
        }
        if t == candidate {
            return candidate;
        }
        // We first attempt to filter the current type, narrowing constituents as appropriate and removing
        // constituents that are unrelated to the candidate.
        let mut key_property_name = String::new();
        if t.flags().intersects(TypeFlags::Union) {
            key_property_name = self.get_key_property_name(t);
        }
        let narrowed_type = self
            .map_type(candidate, |c, n| {
                // If a discriminant property is available, use that to reduce the type.
                let mut matching = t;
                if !key_property_name.is_empty()
                    && let Some(discriminant) =
                        c.get_type_of_property_of_type(n, &key_property_name)
                    && let Some(constituent) = c.get_constituent_type_for_key_type(t, discriminant)
                {
                    matching = constituent;
                }
                // For each constituent t in the current type, if t and c are directly related, pick the most
                // specific of the two. When t and c are related in both directions, we prefer c for type predicates
                // because that is the asserted type, but t for `instanceof` because generics aren't reflected in
                // prototype object types.
                let map_type = move |c: &mut Checker, t: P<Type>| -> Option<P<Type>> {
                    if check_derived {
                        if c.is_type_derived_from(t, n) {
                            return Some(t);
                        } else if c.is_type_derived_from(n, t) {
                            return Some(n);
                        }
                        Some(c.never_type)
                    } else {
                        if crate::relater_1::are_different_plain_literals(t, n) {
                            return Some(c.never_type); // each relation below is false
                        }
                        if c.is_type_strict_subtype_of(t, n) {
                            return Some(t);
                        } else if c.is_type_strict_subtype_of(n, t) {
                            return Some(n);
                        } else if c.is_type_subtype_of(t, n) {
                            return Some(t);
                        } else if c.is_type_subtype_of(n, t) {
                            return Some(n);
                        }
                        Some(c.never_type)
                    }
                };
                let directly_related = c.map_type(matching, map_type).unwrap();
                if !directly_related.flags().intersects(TypeFlags::Never) {
                    return Some(directly_related);
                }
                // If no constituents are directly related, create intersections for any generic constituents that
                // are related by constraint.
                let is_related = move |c: &mut Checker, s: P<Type>, t: P<Type>| -> bool {
                    if check_derived {
                        c.is_type_derived_from(s, t)
                    } else {
                        c.is_type_subtype_of(s, t)
                    }
                };
                c.map_type(t, |c, t| {
                    if c.maybe_type_of_kind(t, TypeFlags::Instantiable) {
                        let constraint = c.get_base_constraint_of_type(t);
                        if constraint.is_none() || is_related(c, n, constraint.unwrap()) {
                            return Some(c.get_intersection_type(&[t, n]));
                        }
                    }
                    Some(c.never_type)
                })
            })
            .unwrap();
        // If filtering produced a non-empty type, return that. Otherwise, pick the most specific of the two
        // based on assignability, or as a last resort produce an intersection.
        if !narrowed_type.flags().intersects(TypeFlags::Never) {
            return narrowed_type;
        } else if self.is_type_subtype_of(candidate, t) {
            return candidate;
        } else if self.is_type_assignable_to(t, candidate) {
            return t;
        } else if self.is_type_assignable_to(candidate, t) {
            return candidate;
        }
        self.get_intersection_type(&[t, candidate])
    }

    // flow.go:966
    fn get_instance_type(&mut self, constructor_type: P<Type>) -> P<Type> {
        let prototype_property_type =
            self.get_type_of_property_of_type(constructor_type, "prototype");
        if let Some(prototype_property_type) = prototype_property_type
            && !is_type_any(Some(prototype_property_type))
        {
            return prototype_property_type;
        }
        let construct_signatures =
            self.get_signatures_of_type(constructor_type, SignatureKind::Construct);
        if !construct_signatures.is_empty() {
            let mut types = Vec::with_capacity(construct_signatures.len());
            types.extend(construct_signatures.iter().map(|&signature| {
                let erased = self.get_erased_signature(signature);
                self.get_return_type_of_signature(erased)
            }));
            return self.get_union_type(&types);
        }
        // We use the empty object type to indicate we don't know the type of objects created by
        // this constructor function.
        self.empty_object_type
    }

    // flow.go:982
    fn narrow_type_by_private_identifier_in_in_expression(
        &mut self,
        f: P<FlowState>,
        t: P<Type>,
        expr: P<Node>,
        assume_true: bool,
    ) -> P<Type> {
        let binary = expr.as_binary_expression();
        let target = Self::get_reference_candidate(binary.right());
        if !self.is_matching_reference(f.ref_node(), target) {
            return t;
        }
        let Some(symbol) = self.get_symbol_for_private_identifier_expression(binary.left) else {
            return t;
        };
        let class_symbol = symbol.parent().unwrap();
        let target_type = if ast::has_static_modifier(symbol.value_declaration().unwrap()) {
            self.get_type_of_symbol(class_symbol)
        } else {
            self.get_declared_type_of_symbol(class_symbol)
        };
        self.get_narrowed_type(t, target_type, assume_true, true /*checkDerived*/)
    }

    // flow.go:1001
    fn narrow_type_by_in_keyword(
        &mut self,
        _f: P<FlowState>,
        t: P<Type>,
        name_type: P<Type>,
        assume_true: bool,
    ) -> P<Type> {
        let name = get_property_name_from_type(name_type);
        let is_known_property = some_type(self, t, |c, t| {
            c.is_type_presence_possible(t, &name, true /*assumeTrue*/)
        });
        if is_known_property {
            // If the check is for a known property (i.e. a property declared in some constituent of
            // the target type), we filter the target type by presence of absence of the property.
            return self.filter_type(t, |c, t| c.is_type_presence_possible(t, &name, assume_true));
        }
        if assume_true {
            // If the check is for an unknown property, we intersect the target type with `Record<X, unknown>`,
            // where X is the name of the property.
            let record_symbol = self.get_global_record_symbol();
            if let Some(record_symbol) = record_symbol {
                let unknown_type = self.unknown_type;
                let record_type = self.get_type_alias_instantiation(
                    record_symbol,
                    &[name_type, unknown_type],
                    None,
                );
                return self.get_intersection_type(&[t, record_type]);
            }
        }
        t
    }

    // flow.go:1024
    fn is_type_presence_possible(
        &mut self,
        t: P<Type>,
        prop_name: &str,
        assume_true: bool,
    ) -> bool {
        let prop = self.get_property_of_type(t, prop_name);
        if let Some(prop) = prop {
            return prop.flags().intersects(SymbolFlags::Optional)
                || prop.check_flags().intersects(CheckFlags::Partial)
                || assume_true;
        }
        self.get_applicable_index_info_for_name(t, prop_name).is_some() || !assume_true
    }

    // flow.go:1032
    fn narrow_type_by_optional_chain_containment(
        &mut self,
        _f: P<FlowState>,
        t: P<Type>,
        operator: Kind,
        value: P<Node>,
        assume_true: bool,
    ) -> P<Type> {
        // We are in a branch of obj?.foo === value (or any one of the other equality operators). We narrow obj as follows:
        // When operator is === and type of value excludes undefined, null and undefined is removed from type of obj in true branch.
        // When operator is !== and type of value excludes undefined, null and undefined is removed from type of obj in false branch.
        // When operator is == and type of value excludes null and undefined, null and undefined is removed from type of obj in true branch.
        // When operator is != and type of value excludes null and undefined, null and undefined is removed from type of obj in false branch.
        // When operator is === and type of value is undefined, null and undefined is removed from type of obj in false branch.
        // When operator is !== and type of value is undefined, null and undefined is removed from type of obj in true branch.
        // When operator is == and type of value is null or undefined, null and undefined is removed from type of obj in false branch.
        // When operator is != and type of value is null or undefined, null and undefined is removed from type of obj in true branch.
        let equals_operator =
            operator == Kind::EqualsEqualsToken || operator == Kind::EqualsEqualsEqualsToken;
        let nullable_flags =
            if operator == Kind::EqualsEqualsToken || operator == Kind::ExclamationEqualsToken {
                TypeFlags::Nullable
            } else {
                TypeFlags::Undefined
            };
        let value_type = self.get_type_of_expression(value);
        // Note that we include any and unknown in the exclusion test because their domain includes null and undefined.
        let remove_nullable = equals_operator != assume_true
            && every_type(self, value_type, |_, t| t.flags().intersects(nullable_flags))
            || equals_operator == assume_true
                && every_type(self, value_type, |_, t| {
                    !t.flags().intersects(TypeFlags::AnyOrUnknown | nullable_flags)
                });
        if remove_nullable {
            return self.get_adjusted_type_with_facts(t, TypeFacts::NEUndefinedOrNull);
        }
        t
    }

    // flow.go:1059
    fn get_type_at_switch_clause(&mut self, f: P<FlowState>, flow: P<FlowNode>) -> FlowType {
        let data = flow.node().unwrap();
        let expr = ast::skip_parentheses(
            data.as_flow_switch_clause_data().switch_statement.expression().unwrap(),
        );
        let flow_type = self.get_type_at_flow_node(f, flow.antecedent().unwrap());
        if f.memo_aborted.get() {
            return flow_type;
        }
        let mut t = flow_type.t.unwrap();
        if self.is_matching_reference(f.ref_node(), expr) {
            t = self.narrow_type_by_switch_on_discriminant(t, data);
        } else if expr.kind() == Kind::TypeOfExpression
            && self.is_matching_reference(f.ref_node(), expr.expression().unwrap())
        {
            t = self.narrow_type_by_switch_on_type_of(t, data);
        } else if expr.kind() == Kind::TrueKeyword {
            t = self.narrow_type_by_switch_on_true(f, t, data);
        } else {
            if self.strict_null_checks {
                if self.optional_chain_contains_reference(expr, f.ref_node()) {
                    t = self.narrow_type_by_switch_optional_chain_containment(t, data, |_, t| {
                        !t.flags().intersects(TypeFlags::Undefined | TypeFlags::Never)
                    });
                } else if ast::is_type_of_expression(expr)
                    && self
                        .optional_chain_contains_reference(expr.expression().unwrap(), f.ref_node())
                {
                    t = self.narrow_type_by_switch_optional_chain_containment(t, data, |_, t| {
                        !(t.flags().intersects(TypeFlags::Never)
                            || t.flags().intersects(TypeFlags::StringLiteral)
                                && get_string_literal_value(t) == "undefined")
                    });
                }
            }
            let access = self.get_discriminant_property_access(f, expr, t);
            if let Some(access) = access {
                t = self.narrow_type_by_switch_on_discriminant_property(t, access, data);
            }
        }
        self.new_flow_type(t, flow_type.incomplete)
    }

    // flow.go:1091
    fn narrow_type_by_switch_on_discriminant(&mut self, t: P<Type>, data: P<Node>) -> P<Type> {
        let data = data.as_flow_switch_clause_data();
        // We only narrow if all case expressions specify
        // values with unit types, except for the case where
        // `type` is unknown. In this instance we map object
        // types to the nonPrimitive type and narrow with that.
        let switch_types = self.get_switch_clause_types(data.switch_statement);
        if switch_types.is_empty() {
            return t;
        }
        let clause_types = &switch_types[data.clause_start..data.clause_end];
        let has_default_clause =
            data.clause_start == data.clause_end || clause_types.contains(&self.never_type);
        if t.flags().intersects(TypeFlags::Unknown) && !has_default_clause {
            let mut ground_clause_types: Option<Vec<P<Type>>> = None;
            for (i, &s) in clause_types.iter().enumerate() {
                if s.flags().intersects(TypeFlags::Primitive | TypeFlags::NonPrimitive) {
                    if let Some(ground_clause_types) = &mut ground_clause_types {
                        ground_clause_types.push(s);
                    }
                } else if s.flags().intersects(TypeFlags::Object) {
                    if ground_clause_types.is_none() {
                        ground_clause_types = Some(clause_types[..i].to_vec());
                    }
                    ground_clause_types.as_mut().unwrap().push(self.non_primitive_type);
                } else {
                    return t;
                }
            }
            return match &ground_clause_types {
                None => self.get_union_type(clause_types),
                Some(ground_clause_types) => self.get_union_type(ground_clause_types),
            };
        }
        let discriminant_type = self.get_union_type(clause_types);
        let mut case_type: Option<P<Type>> = None;
        if discriminant_type.flags().intersects(TypeFlags::Never) {
            case_type = Some(self.never_type);
        } else {
            if discriminant_type.flags().intersects(TypeFlags::Primitive)
                && self.is_uniform_union_type(t)
            {
                let regular_type = self.get_regular_type_of_literal_type(discriminant_type);
                if self.union_contains_type(t, regular_type, false /*matchSymbol*/) {
                    case_type = Some(regular_type);
                }
            }
            if case_type.is_none() {
                let filtered =
                    self.filter_type(t, move |c, t| c.are_types_comparable(discriminant_type, t));
                case_type =
                    Some(self.replace_primitives_with_literals(filtered, discriminant_type));
            }
        }
        let case_type = case_type.unwrap();
        if !has_default_clause {
            return case_type;
        }
        let default_type = self.filter_type(t, |c, t| {
            if !c.is_unit_like_type(t) {
                return true;
            }
            let mut u = c.undefined_type;
            if !t.flags().intersects(TypeFlags::Undefined) {
                let unit_type = checker::Checker::extract_unit_type(t);
                u = c.get_regular_type_of_literal_type(unit_type);
            }
            !switch_types.iter().any(|&st| is_unit_type(st) && c.are_types_comparable(st, u))
        });
        if case_type.flags().intersects(TypeFlags::Never) {
            return default_type;
        }
        self.get_union_type(&[case_type, default_type])
    }

    // flow.go:1157
    fn narrow_type_by_switch_on_type_of(&mut self, t: P<Type>, data: P<Node>) -> P<Type> {
        let data = data.as_flow_switch_clause_data();
        let Some(witnesses) = self.get_switch_clause_type_of_witnesses(data.switch_statement)
        else {
            return t;
        };
        let clauses =
            data.switch_statement.as_switch_statement().case_block.as_case_block().clauses.nodes();
        // Equal start and end denotes implicit fallthrough; undefined marks explicit default clause.
        let default_index = clauses.iter().position(|clause| clause.kind() == Kind::DefaultClause);
        let clause_start = data.clause_start;
        let clause_end = data.clause_end;
        let has_default_clause = clause_start == clause_end
            || default_index.is_some_and(|index| index >= clause_start && index < clause_end);
        if has_default_clause {
            // In the default clause we filter constituents down to those that are not-equal to all handled cases.
            let not_equal_facts =
                Self::get_not_equal_facts_from_typeof_switch(clause_start, clause_end, witnesses);
            return self.filter_type(t, move |c, t| {
                c.get_type_facts(t, not_equal_facts) == not_equal_facts
            });
        }
        // In the non-default cause we create a union of the type narrowed by each of the listed cases.
        let clause_witnesses = &witnesses[clause_start..clause_end];
        let mut types = Vec::with_capacity(clause_witnesses.len());
        types.extend(clause_witnesses.iter().map(|&text| {
            if text.is_empty() { self.never_type } else { self.narrow_type_by_type_name(t, text) }
        }));
        self.get_union_type(&types)
    }

    // flow.go:1187
    fn narrow_type_by_switch_on_true(
        &mut self,
        f: P<FlowState>,
        t: P<Type>,
        data: P<Node>,
    ) -> P<Type> {
        let mut t = t;
        let data = data.as_flow_switch_clause_data();
        let clauses =
            data.switch_statement.as_switch_statement().case_block.as_case_block().clauses.nodes();
        let default_index = clauses.iter().position(|clause| clause.kind() == Kind::DefaultClause);
        let clause_start = data.clause_start;
        let clause_end = data.clause_end;
        let has_default_clause = clause_start == clause_end
            || default_index.is_some_and(|index| index >= clause_start && index < clause_end);
        // First, narrow away all of the cases that preceded this set of cases.
        for element in &clauses[..clause_start] {
            let clause = *element;
            if clause.kind() == Kind::CaseClause {
                t = self.narrow_type(f, t, clause.expression().unwrap(), false /*assumeTrue*/);
            }
        }
        // If our current set has a default, then none the other cases were hit either.
        // There's no point in narrowing by the other cases in the set, since we can
        // get here through other paths.
        if has_default_clause {
            for element in clauses.iter().skip(clause_end) {
                let clause = *element;
                if clause.kind() == Kind::CaseClause {
                    t = self.narrow_type(
                        f,
                        t,
                        clause.expression().unwrap(),
                        false, /*assumeTrue*/
                    );
                }
            }
            return t;
        }
        // Now, narrow based on the cases in this set.
        let mut types = Vec::with_capacity(clause_end - clause_start);
        types.extend(clauses[clause_start..clause_end].iter().map(|&clause| {
            if clause.kind() == Kind::CaseClause {
                self.narrow_type(f, t, clause.expression().unwrap(), true /*assumeTrue*/)
            } else {
                self.never_type
            }
        }));
        self.get_union_type(&types)
    }

    // flow.go:1223
    fn narrow_type_by_switch_optional_chain_containment(
        &mut self,
        t: P<Type>,
        data: P<Node>,
        clause_check: impl FnMut(&mut Checker, P<Type>) -> bool,
    ) -> P<Type> {
        let mut clause_check = clause_check;
        let data = data.as_flow_switch_clause_data();
        let every_clause_checks = data.clause_start != data.clause_end && {
            let switch_types = self.get_switch_clause_types(data.switch_statement);

            !(switch_types[data.clause_start..data.clause_end]
                .iter()
                .any(|&s| !clause_check(self, s)))
        };
        if every_clause_checks {
            return self.get_type_with_facts(t, TypeFacts::NEUndefinedOrNull);
        }
        t
    }

    // flow.go:1231
    fn narrow_type_by_switch_on_discriminant_property(
        &mut self,
        t: P<Type>,
        access: P<Node>,
        data: P<Node>,
    ) -> P<Type> {
        let clause_data = data.as_flow_switch_clause_data();
        if clause_data.clause_start < clause_data.clause_end
            && t.flags().intersects(TypeFlags::Union)
        {
            let (accessed_name, _) = self.get_accessed_property_name(access);
            if !accessed_name.is_empty() && self.get_key_property_name(t) == accessed_name {
                let clause_types = self.get_switch_clause_types(clause_data.switch_statement)
                    [clause_data.clause_start..clause_data.clause_end]
                    .to_vec();
                let mut types = Vec::with_capacity(clause_types.len());
                types.extend(clause_types.into_iter().map(|s| {
                    let result = self.get_constituent_type_for_key_type(t, s);
                    match result {
                        Some(result) => result,
                        None => self.unknown_type,
                    }
                }));
                let candidate = self.get_union_type(&types);
                if candidate != self.unknown_type {
                    return candidate;
                }
            }
        }
        self.narrow_type_by_discriminant(t, access, move |c, t| {
            c.narrow_type_by_switch_on_discriminant(t, data)
        })
    }

    // flow.go:1253
    fn get_type_at_flow_branch_label(
        &mut self,
        f: P<FlowState>,
        _flow: P<FlowNode>,
        antecedents: P<FlowList>,
    ) -> FlowType {
        let antecedent_start = self.antecedent_types.len();
        let mut subtype_reduction = false;
        let mut seen_incomplete = false;
        let mut bypass_flow: Option<P<FlowNode>> = None;
        let mut next = Some(antecedents);
        while let Some(list) = next {
            next = list.next.get();
            let antecedent = list.flow;
            if bypass_flow.is_none()
                && antecedent.flags().intersects(FlowFlags::SwitchClause)
                && antecedent.node().unwrap().as_flow_switch_clause_data().is_empty()
            {
                // The antecedent is the bypass branch of a potentially exhaustive switch statement.
                bypass_flow = Some(antecedent);
                continue;
            }
            let flow_type = self.get_type_at_flow_node(f, antecedent);
            if f.memo_aborted.get() {
                self.antecedent_types.truncate(antecedent_start);
                return flow_type;
            }
            let flow_type_t = flow_type.t.unwrap();
            // If the type at a particular antecedent path is the declared type and the
            // reference is known to always be assigned (i.e. when declared and initial types
            // are the same), there is no reason to process more antecedents since the only
            // possible outcome is subtypes that will be removed in the final union type anyway.
            if flow_type_t == f.declared() && f.declared() == f.initial() {
                self.antecedent_types.truncate(antecedent_start);
                return flow_type_of(flow_type_t);
            }
            if !self.antecedent_types[antecedent_start..].contains(&flow_type_t) {
                self.antecedent_types.push(flow_type_t);
            }
            // If an antecedent type is not a subset of the declared type, we need to perform
            // subtype reduction. This happens when a "foreign" type is injected into the control
            // flow using the instanceof operator or a user defined type predicate.
            if !self.is_type_subset_of(flow_type_t, f.initial()) {
                subtype_reduction = true;
            }
            if flow_type.incomplete {
                seen_incomplete = true;
            }
        }
        if let Some(bypass_flow) = bypass_flow {
            let flow_type = self.get_type_at_flow_node(f, bypass_flow);
            if f.memo_aborted.get() {
                self.antecedent_types.truncate(antecedent_start);
                return flow_type;
            }
            let flow_type_t = flow_type.t.unwrap();
            // If the bypass flow contributes a type we haven't seen yet and the switch statement
            // isn't exhaustive, process the bypass flow type. Since exhaustiveness checks increase
            // the risk of circularities, we only want to perform them when they make a difference.
            if !flow_type_t.flags().intersects(TypeFlags::Never)
                && !self.antecedent_types[antecedent_start..].contains(&flow_type_t)
                && !self.is_exhaustive_switch_statement(
                    bypass_flow.node().unwrap().as_flow_switch_clause_data().switch_statement,
                )
            {
                if flow_type_t == f.declared() && f.declared() == f.initial() {
                    self.antecedent_types.truncate(antecedent_start);
                    return flow_type_of(flow_type_t);
                }
                self.antecedent_types.push(flow_type_t);
                if !self.is_type_subset_of(flow_type_t, f.initial()) {
                    subtype_reduction = true;
                }
                if flow_type.incomplete {
                    seen_incomplete = true;
                }
            }
        }
        let types = self.antecedent_types[antecedent_start..].to_vec();
        let union_type = self.get_union_or_evolving_array_type(
            f,
            &types,
            if subtype_reduction { UnionReduction::Subtype } else { UnionReduction::Literal },
        );
        let result = self.new_flow_type(union_type, seen_incomplete);
        self.antecedent_types.truncate(antecedent_start);
        result
    }

    // At flow control branch or loop junctions, if the type along every antecedent code path
    // is an evolving array type, we construct a combined evolving array type. Otherwise we
    // finalize all evolving array types.
    // flow.go:1314
    fn get_union_or_evolving_array_type(
        &mut self,
        f: P<FlowState>,
        types: &[P<Type>],
        subtype_reduction: UnionReduction,
    ) -> P<Type> {
        if is_evolving_array_type_list(types) {
            let mut element_types = Vec::with_capacity(types.len());
            element_types
                .extend(types.iter().map(|&t| self.get_element_type_of_evolving_array_type(t)));
            let union_type = self.get_union_type(&element_types);
            return self.get_evolving_array_type(union_type);
        }
        let mut finalized_types = Vec::with_capacity(types.len());
        finalized_types.extend(types.iter().map(|&t| self.finalize_evolving_array_type(t)));
        let union_type =
            self.get_union_type_ex(&finalized_types, subtype_reduction, AliasArg::None, None);
        let result = self.recombine_unknown_type(union_type);
        let declared_type = f.declared();
        if result != declared_type
            && (result.flags() & declared_type.flags()).intersects(TypeFlags::Union)
            && result.as_union_type().types() == declared_type.as_union_type().types()
        {
            return declared_type;
        }
        result
    }

    // flow.go:1325
    fn get_type_at_flow_loop_label(&mut self, f: P<FlowState>, flow: P<FlowNode>) -> FlowType {
        if f.ref_key.get().is_zero() {
            let ref_key = self.get_flow_reference_key(f);
            f.ref_key.set(ref_key);
        }
        if f.ref_key.get() == NON_DOTTED_NAME_CACHE_KEY {
            // No cache key is generated when binding patterns are in unnarrowable situations
            return flow_type_of(f.declared());
        }
        let key = FlowLoopKey { flow_node: flow, ref_key: f.ref_key.get() };
        // If we have previously computed the control flow type for the reference at
        // this flow loop junction, return the cached type.
        if let Some(&cached) = self.flow_loop_cache.get(&key) {
            return flow_type_of(cached);
        }
        // If this flow loop junction and reference are already being processed, return
        // the union of the types computed for each branch so far, marked as incomplete.
        // It is possible to see an empty array in cases where loops are nested and the
        // back edge of the outer loop reaches an inner loop that is already being analyzed.
        // In such cases we restart the analysis of the inner loop, which will then see
        // a non-empty in-process array for the outer loop and eventually terminate because
        // the first antecedent of a loop junction is always the non-looping control flow
        // path that leads to the top.
        let in_process = self
            .flow_loop_stack
            .iter()
            .find(|loop_info| loop_info.key == key && !loop_info.types.is_empty())
            .map(|loop_info| (loop_info.types.clone(), loop_info.serial));
        if let Some((in_process_types, serial)) = in_process {
            // Everything computed from these types (every frame younger than the loop's stack entry) is transient.
            self.flow_memo.taint(serial);
            let union_type = self.get_union_or_evolving_array_type(
                f,
                &in_process_types,
                UnionReduction::Literal,
            );
            return self.new_flow_type(union_type, true /*incomplete*/);
        }
        // Add the flow loop junction and reference to the in-process stack and analyze
        // each antecedent code path.
        let mut antecedent_types: Vec<P<Type>> = Vec::with_capacity(4);
        let mut subtype_reduction = false;
        let mut first_antecedent_type = FlowType::default();
        let mut next = flow.antecedents();
        while let Some(list) = next {
            next = list.next.get();
            let flow_type: FlowType;
            if first_antecedent_type.is_nil() {
                // The first antecedent of a loop junction is always the non-looping control
                // flow path that leads to the top.
                first_antecedent_type = self.get_type_at_flow_node(f, list.flow);
                if f.memo_aborted.get() {
                    return first_antecedent_type;
                }
                flow_type = first_antecedent_type;
            } else {
                // All but the first antecedent are the looping control flow paths that lead
                // back to the loop junction. We track these on the flow loop stack.
                let serial = self.flow_memo.next_serial();
                self.flow_loop_stack.push(FlowLoopInfo {
                    key,
                    types: antecedent_types.clone(),
                    serial,
                });
                let save_flow_type_cache = self.take_flow_type_cache();
                flow_type = self.get_type_at_flow_node(f, list.flow);
                self.restore_flow_type_cache(save_flow_type_cache);
                self.flow_loop_stack.pop();
                if f.memo_aborted.get() {
                    return flow_type;
                }
                // If we see a value appear in the cache it is a sign that control flow analysis
                // was restarted and completed by checkExpressionCached. We can simply pick up
                // the resulting type and bail out.
                if let Some(&cached) = self.flow_loop_cache.get(&key) {
                    return flow_type_of(cached);
                }
            }
            let flow_type_t = flow_type.t.unwrap();
            if !antecedent_types.contains(&flow_type_t) {
                antecedent_types.push(flow_type_t);
            }
            // If an antecedent type is not a subset of the declared type, we need to perform
            // subtype reduction. This happens when a "foreign" type is injected into the control
            // flow using the instanceof operator or a user defined type predicate.
            if !self.is_type_subset_of(flow_type_t, f.initial()) {
                subtype_reduction = true;
            }
            // If the type at a particular antecedent path is the declared type there is no
            // reason to process more antecedents since the only possible outcome is subtypes
            // that will be removed in the final union type anyway.
            if flow_type_t == f.declared() {
                break;
            }
        }
        // The result is incomplete if the first antecedent (the non-looping control flow path)
        // is incomplete.
        let result = self.get_union_or_evolving_array_type(
            f,
            &antecedent_types,
            if subtype_reduction { UnionReduction::Subtype } else { UnionReduction::Literal },
        );
        if first_antecedent_type.incomplete {
            return self.new_flow_type(result, true /*incomplete*/);
        }
        self.flow_loop_cache.insert(key, result);
        flow_type_of(result)
    }

    // flow.go:1404
    fn get_type_at_flow_array_mutation(&mut self, f: P<FlowState>, flow: P<FlowNode>) -> FlowType {
        if f.declared() == self.auto_type || f.declared() == self.auto_array_type {
            let node = flow.node().unwrap();
            let expr = if ast::is_call_expression(node) {
                node.expression().unwrap().expression().unwrap()
            } else {
                node.as_binary_expression().left.expression().unwrap()
            };
            let candidate = Self::get_reference_candidate(expr);
            if self.is_matching_reference(f.ref_node(), candidate) {
                let flow_type = self.get_type_at_flow_node(f, flow.antecedent().unwrap());
                if f.memo_aborted.get() {
                    return flow_type;
                }
                let flow_type_t = flow_type.t.unwrap();
                if flow_type_t.object_flags().intersects(ObjectFlags::EvolvingArray) {
                    let mut evolved_type = flow_type_t;
                    if ast::is_call_expression(node) {
                        evolved_type =
                            node.arguments().iter().fold(evolved_type, |mut evolved_type, &arg| {
                                evolved_type =
                                    self.add_evolving_array_element_type(evolved_type, arg);

                                evolved_type
                            });
                    } else {
                        // We must get the context free expression type so as to not recur in an uncached fashion on the LHS (which causes exponential blowup in compile time)
                        let index_type = self.get_context_free_type_of_expression(
                            node.as_binary_expression()
                                .left
                                .as_element_access_expression()
                                .argument_expression,
                        );
                        if self.is_type_assignable_to_kind(index_type, TypeFlags::NumberLike) {
                            evolved_type = self.add_evolving_array_element_type(
                                evolved_type,
                                node.as_binary_expression().right(),
                            );
                        }
                    }
                    return self.new_flow_type(evolved_type, flow_type.incomplete);
                }
                return flow_type;
            }
        }
        FlowType::default()
    }

    // flow.go:1436
    fn get_discriminant_property_access(
        &mut self,
        f: P<FlowState>,
        expr: P<Node>,
        computed_type: P<Type>,
    ) -> Option<P<Node>> {
        // As long as the computed type is a subset of the declared type, we use the full declared type to detect
        // a discriminant property. In cases where the computed type isn't a subset, e.g because of a preceding type
        // predicate narrowing, we use the actual computed type.
        let declared_type = f.declared();
        if declared_type.flags().intersects(TypeFlags::Union)
            || computed_type.flags().intersects(TypeFlags::Union)
        {
            let access = self.get_candidate_discriminant_property_access(f, expr);
            if let Some(access) = access {
                let (name, ok) = self.get_accessed_property_name(access);
                if ok {
                    let mut t = computed_type;
                    if declared_type.flags().intersects(TypeFlags::Union)
                        && self.is_type_subset_of(computed_type, declared_type)
                    {
                        t = declared_type;
                    }
                    if self.is_discriminant_property(Some(t), &name) {
                        return Some(access);
                    }
                }
            }
        }
        None
    }

    // flow.go:1457
    fn get_candidate_discriminant_property_access(
        &mut self,
        f: P<FlowState>,
        expr: P<Node>,
    ) -> Option<P<Node>> {
        let reference = f.ref_node();
        if ast::is_binding_pattern(reference)
            || ast::is_function_expression_or_arrow_function(reference)
            || ast::is_object_literal_method(reference)
        {
            // When the reference is a binding pattern or function or arrow expression, we are narrowing a pseudo-reference in
            // getNarrowedTypeOfSymbol. An identifier for a destructuring variable declared in the same binding pattern or
            // parameter declared in the same parameter list is a candidate.
            if ast::is_identifier(expr) {
                let symbol = self.get_resolved_symbol(expr);
                let declaration = self
                    .get_export_symbol_of_value_symbol_if_exported(Some(symbol))
                    .unwrap()
                    .value_declaration();
                if let Some(declaration) = declaration
                    && (ast::is_binding_element(declaration)
                        || ast::is_parameter_declaration(declaration))
                    && Some(reference) == declaration.parent()
                    && declaration.initializer().is_none()
                    && !has_dot_dot_dot_token(declaration)
                {
                    return Some(declaration);
                }
            }
        } else if ast::is_access_expression(expr) {
            // An access expression is a candidate if the reference matches the left hand expression.
            if self.is_matching_reference(reference, expr.expression().unwrap()) {
                return Some(expr);
            }
        } else if ast::is_identifier(expr) {
            let symbol = self.get_resolved_symbol(expr);
            if self.is_constant_variable(symbol) {
                let declaration = symbol.value_declaration().unwrap();
                let initializer = get_candidate_variable_declaration_initializer(declaration);
                // Given 'const x = obj.kind', allow 'x' as an alias for 'obj.kind'
                if let Some(initializer) = initializer
                    && ast::is_access_expression(initializer)
                    && self.is_matching_reference(reference, initializer.expression().unwrap())
                {
                    return Some(initializer);
                }
                // Given 'const { kind: x } = obj', allow 'x' as an alias for 'obj.kind'
                if ast::is_binding_element(declaration) && declaration.initializer().is_none() {
                    let initializer = get_candidate_variable_declaration_initializer(
                        declaration.parent().unwrap().parent().unwrap(),
                    );
                    if let Some(initializer) = initializer
                        && (ast::is_identifier(initializer)
                            || ast::is_access_expression(initializer))
                        && self.is_matching_reference(reference, initializer)
                    {
                        return Some(declaration);
                    }
                }
            }
        }
        None
    }
}

// flow.go:1496
fn get_candidate_variable_declaration_initializer(node: P<Node>) -> Option<P<Node>> {
    if ast::is_variable_declaration(node)
        && node.type_node().is_none()
        && let Some(initializer) = node.initializer()
    {
        return Some(ast::skip_parentheses(initializer));
    }
    None
}

impl Checker {
    // An evolving array type tracks the element types that have so far been seen in an
    // 'x.push(value)' or 'x[n] = value' operation along the control flow graph. Evolving
    // array types are ultimately converted into manifest array types (using getFinalArrayType)
    // and never escape the getFlowTypeOfReference function.
    // flow.go:1509
    fn get_evolving_array_type(&mut self, element_type: P<Type>) -> P<Type> {
        let key =
            CachedTypeKey { kind: CachedTypeKind::EvolvingArrayType, type_id: element_type.id };
        let result = self.cached_types.get(&key).copied();
        if let Some(result) = result {
            result
        } else {
            let result = self.new_object_type(ObjectFlags::EvolvingArray, None);
            result.as_evolving_array_type().element_type.set(Some(element_type));
            self.cached_types.insert(key, result);
            result
        }
    }

    // flow.go:1520
    fn get_element_type_of_evolving_array_type(&self, t: P<Type>) -> P<Type> {
        if t.object_flags().intersects(ObjectFlags::EvolvingArray) {
            return t.as_evolving_array_type().element_type.get().unwrap();
        }
        self.never_type
    }
}

// flow.go:1527
fn is_evolving_array_type_list(types: &[P<Type>]) -> bool {
    let mut has_evolving_array_type = false;
    for &t in types {
        if !t.flags().intersects(TypeFlags::Never) {
            if !t.object_flags().intersects(ObjectFlags::EvolvingArray) {
                return false;
            }
            has_evolving_array_type = true;
        }
    }
    has_evolving_array_type
}

impl Checker {
    // Return true if the given node is 'x' in an 'x.length', x.push(value)', 'x.unshift(value)' or
    // 'x[n] = value' operation, where 'n' is an expression of type any, undefined, or a number-like type.
    // flow.go:1542
    pub(crate) fn is_evolving_array_operation_target(&mut self, node: P<Node>) -> bool {
        let root = Self::get_reference_root(node);
        let parent = root.parent().unwrap();
        let is_length_push_or_unshift = ast::is_property_access_expression(parent)
            && (parent.name().unwrap().text() == "length"
                || ast::is_call_expression(parent.parent().unwrap())
                    && ast::is_identifier(parent.name().unwrap())
                    && ast::is_push_or_unshift_identifier(parent.name().unwrap()));
        let is_element_assignment = ast::is_element_access_expression(parent)
            && parent.expression() == Some(root)
            && ast::is_binary_expression(parent.parent().unwrap())
            && parent.parent().unwrap().as_binary_expression().operator_token.kind()
                == Kind::EqualsToken
            && parent.parent().unwrap().as_binary_expression().left == parent
            && !ast::is_assignment_target(parent.parent().unwrap())
            && {
                let index_type = self.get_type_of_expression(
                    parent.as_element_access_expression().argument_expression,
                );
                self.is_type_assignable_to_kind(index_type, TypeFlags::NumberLike)
            };
        is_length_push_or_unshift || is_element_assignment
    }

    // When adding evolving array element types we do not perform subtype reduction. Instead,
    // we defer subtype reduction until the evolving array type is finalized into a manifest
    // array type.
    // flow.go:1557
    fn add_evolving_array_element_type(
        &mut self,
        evolving_array_type: P<Type>,
        node: P<Node>,
    ) -> P<Type> {
        let context_free_type = self.get_context_free_type_of_expression(node);
        let base_type = self.get_base_type_of_literal_type(context_free_type);
        let new_element_type = self.get_regular_type_of_object_literal(base_type);
        let element_type = evolving_array_type.as_evolving_array_type().element_type.get().unwrap();
        if self.is_type_subset_of(new_element_type, element_type) {
            return evolving_array_type;
        }
        let union_type = self.get_union_type(&[element_type, new_element_type]);
        self.get_evolving_array_type(union_type)
    }

    // flow.go:1566
    fn finalize_evolving_array_type(&mut self, t: P<Type>) -> P<Type> {
        if t.object_flags().intersects(ObjectFlags::EvolvingArray) {
            return self.get_final_array_type(t.as_evolving_array_type());
        }
        t
    }

    // flow.go:1573
    fn get_final_array_type(&mut self, t: &'static EvolvingArrayType) -> P<Type> {
        if t.final_array_type.get().is_none() {
            let final_array_type = self.create_final_array_type(t.element_type.get().unwrap());
            t.final_array_type.set(Some(final_array_type));
        }
        t.final_array_type.get().unwrap()
    }

    // flow.go:1580
    fn create_final_array_type(&mut self, element_type: P<Type>) -> P<Type> {
        if element_type.flags().intersects(TypeFlags::Never) {
            return self.auto_array_type;
        } else if element_type.flags().intersects(TypeFlags::Union) {
            let union_type = self.get_union_type_ex(
                element_type.types(),
                UnionReduction::Subtype,
                AliasArg::None,
                None,
            );
            return self.create_array_type(union_type);
        }
        self.create_array_type(element_type)
    }

    // flow.go:1590
    fn report_flow_control_error(&mut self, node: P<Node>) {
        let block = ast::find_ancestor(node, ast::is_function_or_module_block).unwrap();
        let source_file = ast::get_source_file_of_node(node).unwrap();
        let span = tsrs_scanner::get_range_of_token_at_position(
            source_file,
            block
                .statement_list()
                .unwrap()
                .pos()
                .expect_source("parsed node lists have source positions"),
        );
        self.add_diagnostic(ast::new_diagnostic(Some(source_file), span, &diagnostics::THE_CONTAINING_FUNCTION_OR_MODULE_BODY_IS_TOO_LARGE_FOR_CONTROL_FLOW_ANALYSIS, &[]));
    }

    // flow.go:1597
    pub(crate) fn is_matching_reference(&mut self, source: P<Node>, target: P<Node>) -> bool {
        match target.kind() {
            Kind::ParenthesizedExpression | Kind::NonNullExpression => {
                return self.is_matching_reference(source, target.expression().unwrap());
            }
            Kind::BinaryExpression => {
                return ast::is_assignment_expression(target, false)
                    && self.is_matching_reference(source, target.as_binary_expression().left)
                    || ast::is_binary_expression(target)
                        && target.as_binary_expression().operator_token.kind() == Kind::CommaToken
                        && self
                            .is_matching_reference(source, target.as_binary_expression().right());
            }
            _ => {}
        }
        match source.kind() {
            Kind::MetaProperty => {
                return ast::is_meta_property(target)
                    && source.as_meta_property().keyword_token
                        == target.as_meta_property().keyword_token
                    && source.name().unwrap().text() == target.name().unwrap().text();
            }
            Kind::Identifier | Kind::PrivateIdentifier => {
                if ast::is_this_in_type_query(source) {
                    return target.kind() == Kind::ThisKeyword;
                }
                if ast::is_identifier(target)
                    && self.get_resolved_symbol(source) == self.get_resolved_symbol(target)
                {
                    return true;
                }
                if ast::is_variable_declaration(target) || ast::is_binding_element(target) {
                    let source_symbol = self.get_resolved_symbol(source);
                    let export_symbol = self
                        .get_export_symbol_of_value_symbol_if_exported(Some(source_symbol))
                        .unwrap();
                    return Some(export_symbol) == self.get_symbol_of_declaration(target);
                }
                return false;
            }
            Kind::ThisKeyword => {
                return target.kind() == Kind::ThisKeyword;
            }
            Kind::SuperKeyword => {
                return target.kind() == Kind::SuperKeyword;
            }
            Kind::NonNullExpression | Kind::ParenthesizedExpression | Kind::SatisfiesExpression => {
                return self.is_matching_reference(source.expression().unwrap(), target);
            }
            Kind::PropertyAccessExpression | Kind::ElementAccessExpression => {
                let (source_property_name, ok) = self.get_accessed_property_name(source);
                if ok && ast::is_access_expression(target) {
                    let (target_property_name, ok) = self.get_accessed_property_name(target);
                    if ok {
                        return target_property_name == source_property_name
                            && self.is_matching_reference(
                                source.expression().unwrap(),
                                target.expression().unwrap(),
                            );
                    }
                }
                if ast::is_element_access_expression(source)
                    && ast::is_element_access_expression(target)
                {
                    let source_arg = source.as_element_access_expression().argument_expression;
                    let target_arg = target.as_element_access_expression().argument_expression;
                    if ast::is_identifier(source_arg) && ast::is_identifier(target_arg) {
                        let symbol = self.get_resolved_symbol(source_arg);
                        if symbol == self.get_resolved_symbol(target_arg)
                            && (self.is_constant_variable(symbol)
                                || Self::is_parameter_or_mutable_local_variable(symbol)
                                    && !self.is_symbol_assigned(symbol))
                        {
                            return self.is_matching_reference(
                                source.expression().unwrap(),
                                target.expression().unwrap(),
                            );
                        }
                    }
                }
            }
            Kind::QualifiedName => {
                if ast::is_access_expression(target) {
                    let (target_property_name, ok) = self.get_accessed_property_name(target);
                    if ok {
                        return source.as_qualified_name().right.text() == target_property_name
                            && self.is_matching_reference(
                                source.as_qualified_name().left,
                                target.expression().unwrap(),
                            );
                    }
                }
            }
            Kind::BinaryExpression => {
                return ast::is_binary_expression(source)
                    && source.as_binary_expression().operator_token.kind() == Kind::CommaToken
                    && self.is_matching_reference(source.as_binary_expression().right(), target);
            }
            _ => {}
        }
        false
    }

    // Return the flow cache key for a "dotted name" (i.e. a sequence of identifiers
    // separated by dots). The key consists of the id of the symbol referenced by the
    // leftmost identifier followed by zero or more property names separated by dots.
    // The result is nonDottedNameCacheKey if the reference isn't a dotted name.
    // flow.go:1657
    fn get_flow_reference_key(&mut self, f: P<FlowState>) -> CacheHashKey {
        let mut b = KeyBuilder::default();
        if self.write_flow_cache_key(
            &mut b,
            f.ref_node(),
            f.declared(),
            f.initial(),
            f.flow_container.get(),
        ) {
            return b.hash();
        }
        NON_DOTTED_NAME_CACHE_KEY // Reference isn't a dotted name
    }

    // flow.go:1665
    fn write_flow_cache_key(
        &mut self,
        b: &mut KeyBuilder,
        node: P<Node>,
        declared_type: P<Type>,
        initial_type: P<Type>,
        flow_container: Option<P<Node>>,
    ) -> bool {
        match node.kind() {
            Kind::Identifier | Kind::ThisKeyword => {
                if node.kind() == Kind::Identifier && !ast::is_this_in_type_query(node) {
                    let symbol = self.get_resolved_symbol(node);
                    if symbol == self.unknown_symbol {
                        return false;
                    }
                    b.write_symbol(symbol);
                }
                b.write_byte(b':');
                b.write_type(declared_type);
                if initial_type != declared_type {
                    b.write_byte(b'=');
                    b.write_type(initial_type);
                }
                if flow_container.is_some() {
                    b.write_byte(b'@');
                    b.write_node(flow_container);
                }
                return true;
            }
            Kind::NonNullExpression | Kind::ParenthesizedExpression => {
                return self.write_flow_cache_key(
                    b,
                    node.expression().unwrap(),
                    declared_type,
                    initial_type,
                    flow_container,
                );
            }
            Kind::QualifiedName => {
                if !self.write_flow_cache_key(
                    b,
                    node.as_qualified_name().left,
                    declared_type,
                    initial_type,
                    flow_container,
                ) {
                    return false;
                }
                b.write_byte(b'.');
                b.write_string(node.as_qualified_name().right.text());
                return true;
            }
            Kind::PropertyAccessExpression | Kind::ElementAccessExpression => {
                let (prop_name, ok) = self.get_accessed_property_name(node);
                if ok {
                    if !self.write_flow_cache_key(
                        b,
                        node.expression().unwrap(),
                        declared_type,
                        initial_type,
                        flow_container,
                    ) {
                        return false;
                    }
                    b.write_byte(b'.');
                    b.write_string(&prop_name);
                    return true;
                }
                if ast::is_element_access_expression(node)
                    && ast::is_identifier(node.as_element_access_expression().argument_expression)
                {
                    let symbol = self.get_resolved_symbol(
                        node.as_element_access_expression().argument_expression,
                    );
                    if self.is_constant_variable(symbol)
                        || Self::is_parameter_or_mutable_local_variable(symbol)
                            && !self.is_symbol_assigned(symbol)
                    {
                        if !self.write_flow_cache_key(
                            b,
                            node.expression().unwrap(),
                            declared_type,
                            initial_type,
                            flow_container,
                        ) {
                            return false;
                        }
                        b.write_string(".@");
                        b.write_symbol(symbol);
                        return true;
                    }
                }
            }
            Kind::ObjectBindingPattern
            | Kind::ArrayBindingPattern
            | Kind::FunctionDeclaration
            | Kind::FunctionExpression
            | Kind::ArrowFunction
            | Kind::MethodDeclaration => {
                b.write_node(Some(node));
                b.write_byte(b'#');
                b.write_type(declared_type);
                return true;
            }
            _ => {}
        }
        false
    }

    // flow.go:1727
    // Public for lint rules (tsgolint's shim exposes Checker_getAccessedPropertyName).
    /// tsrs: a property access's name is borrowed from the node (the common case, asked per narrowing step).
    fn get_accessed_property_name(
        &mut self,
        access: P<Node>,
    ) -> (std::borrow::Cow<'static, str>, bool) {
        if ast::is_property_access_expression(access) {
            return (access.name().unwrap().text().into(), true);
        }
        let (name, ok) = if ast::is_element_access_expression(access) {
            self.try_get_element_access_expression_name(access)
        } else if ast::is_binding_element(access) {
            self.get_destructuring_property_name(access)
        } else if ast::is_parameter_declaration(access) {
            let index = access
                .parent()
                .unwrap()
                .parameters()
                .iter()
                .position(|&p| p == access)
                .expect("parameter belongs to its declaration");
            (index.to_string(), true)
        } else {
            (String::new(), false)
        };
        (name.into(), ok)
    }

    // flow.go:1743
    pub(crate) fn try_get_element_access_expression_name(
        &mut self,
        node: P<Node>,
    ) -> (String, bool) {
        let argument_expression = node.as_element_access_expression().argument_expression;
        if ast::is_string_or_numeric_literal_like(argument_expression) {
            return (argument_expression.text().to_string(), true);
        } else if ast::is_entity_name_expression(argument_expression) {
            return self.try_get_name_from_entity_name_expression(argument_expression);
        }
        (String::new(), false)
    }

    // flow.go:1753
    fn try_get_name_from_entity_name_expression(&mut self, node: P<Node>) -> (String, bool) {
        let symbol = self.resolve_entity_name(
            node,
            SymbolFlags::Value,
            true, /*ignoreErrors*/
            false,
            None,
        );
        let symbol = match symbol {
            Some(symbol)
                if self.is_constant_variable(symbol)
                    || symbol.flags().intersects(SymbolFlags::EnumMember) =>
            {
                symbol
            }
            _ => return (String::new(), false),
        };
        let Some(declaration) = symbol.value_declaration() else {
            return (String::new(), false);
        };
        let t = self.try_get_type_from_type_node(declaration);
        if let Some(t) = t {
            let (name, ok) = try_get_name_from_type(t);
            if ok {
                return (name, true);
            }
        }
        // We exclude binding elements because their initializers don't solely determine their types and resolving
        // full types can cause circularities (see https://github.com/microsoft/TypeScript/issues/63192).
        if has_only_expression_initializer(declaration)
            && !ast::is_binding_element(declaration)
            && self.is_block_scoped_name_declared_before_use(declaration, node)
        {
            if let Some(initializer) = declaration.initializer() {
                let initializer_type = self.get_type_of_expression(initializer);
                return try_get_name_from_type(initializer_type);
            } else if ast::is_enum_member(declaration) {
                return match ast::try_get_text_of_property_name(declaration.name().unwrap()) {
                    Some(text) => (text, true),
                    None => (String::new(), false),
                };
            }
        }
        (String::new(), false)
    }
}

// flow.go:1782
fn try_get_name_from_type(t: P<Type>) -> (String, bool) {
    if t.flags().intersects(TypeFlags::UniqueESSymbol) {
        return (t.as_unique_es_symbol_type().name.get().to_string(), true);
    } else if t.flags().intersects(TypeFlags::StringOrNumberLiteral) {
        return (evaluator::any_to_string(t.as_literal_type().value.get().unwrap()), true);
    }
    (String::new(), false)
}

impl Checker {
    // flow.go:1792
    pub(crate) fn get_destructuring_property_name(&mut self, node: P<Node>) -> (String, bool) {
        let parent = node.parent().unwrap();
        if ast::is_binding_element(node) && ast::is_object_binding_pattern(parent) {
            return self
                .get_literal_property_name_text(get_binding_element_property_name(node).unwrap());
        }
        if ast::is_property_assignment(node) || ast::is_shorthand_property_assignment(node) {
            return self.get_literal_property_name_text(node.name().unwrap());
        }
        if ast::is_array_literal_expression(parent) || ast::is_array_binding_pattern(parent) {
            let index = parent
                .elements()
                .iter()
                .position(|&e| e == node)
                .expect("element belongs to its parent");
            return (index.to_string(), true);
        }
        (String::new(), false)
    }

    // flow.go:1806
    fn get_literal_property_name_text(&mut self, name: P<Node>) -> (String, bool) {
        let t = self.get_literal_type_from_property_name(name);
        if t.flags().intersects(TypeFlags::StringLiteral | TypeFlags::NumberLiteral) {
            return (evaluator::any_to_string(t.as_literal_type().value.get().unwrap()), true);
        }
        (String::new(), false)
    }

    // flow.go:1814
    pub(crate) fn is_constant_reference(&mut self, node: P<Node>) -> bool {
        match node.kind() {
            Kind::ThisKeyword => {
                return true;
            }
            Kind::Identifier => {
                if !ast::is_this_in_type_query(node) {
                    let symbol = self.get_resolved_symbol(node);
                    return self.is_constant_variable(symbol)
                        || Self::is_parameter_or_mutable_local_variable(symbol)
                            && !self.is_symbol_assigned(symbol)
                        || symbol.value_declaration().is_some()
                            && ast::is_function_expression(symbol.value_declaration().unwrap());
                }
            }
            Kind::PropertyAccessExpression | Kind::ElementAccessExpression => {
                // The resolvedSymbol property is initialized by checkPropertyAccess or checkElementAccess before we get here.
                if self.is_constant_reference(node.expression().unwrap()) {
                    let symbol = self.get_resolved_symbol_or_nil(node);
                    if let Some(symbol) = symbol {
                        return self.is_readonly_symbol(symbol);
                    }
                }
            }
            Kind::ObjectBindingPattern | Kind::ArrayBindingPattern => {
                let root_declaration = ast::get_root_declaration(node.parent().unwrap());
                if ast::is_parameter_declaration(root_declaration)
                    || ast::is_variable_declaration(root_declaration)
                        && ast::is_catch_clause(root_declaration.parent().unwrap())
                {
                    return !self.is_some_symbol_assigned(root_declaration);
                }
                return ast::is_variable_declaration(root_declaration)
                    && self.is_var_const_like(root_declaration);
            }
            _ => {}
        }
        false
    }

    // flow.go:1841
    fn contains_matching_reference(&mut self, source: P<Node>, target: P<Node>) -> bool {
        let mut source = source;
        while ast::is_access_expression(source) {
            source = source.expression().unwrap();
            if self.is_matching_reference(source, target) {
                return true;
            }
        }
        false
    }

    // flow.go:1851
    fn optional_chain_contains_reference(&mut self, source: P<Node>, target: P<Node>) -> bool {
        let mut source = source;
        while ast::is_optional_chain(source) {
            source = source.expression().unwrap();
            if self.is_matching_reference(source, target) {
                return true;
            }
        }
        false
    }

    // flow.go:1861
    fn get_reference_candidate(node: P<Node>) -> P<Node> {
        match node.kind() {
            Kind::ParenthesizedExpression => {
                return Self::get_reference_candidate(node.expression().unwrap());
            }
            Kind::BinaryExpression => match node.as_binary_expression().operator_token.kind() {
                Kind::EqualsToken
                | Kind::BarBarEqualsToken
                | Kind::AmpersandAmpersandEqualsToken
                | Kind::QuestionQuestionEqualsToken => {
                    return Self::get_reference_candidate(node.as_binary_expression().left);
                }
                Kind::CommaToken => {
                    return Self::get_reference_candidate(node.as_binary_expression().right());
                }
                _ => {}
            },
            _ => {}
        }
        node
    }

    // flow.go:1876
    fn get_reference_root(node: P<Node>) -> P<Node> {
        let parent = node.parent().unwrap();
        if ast::is_parenthesized_expression(parent)
            || ast::is_binary_expression(parent)
                && parent.as_binary_expression().operator_token.kind() == Kind::EqualsToken
                && parent.as_binary_expression().left == node
            || ast::is_binary_expression(parent)
                && parent.as_binary_expression().operator_token.kind() == Kind::CommaToken
                && parent.as_binary_expression().right() == node
        {
            return Self::get_reference_root(parent);
        }
        node
    }

    // flow.go:1886
    pub(crate) fn has_matching_argument(
        &mut self,
        expression: P<Node>,
        reference: P<Node>,
    ) -> bool {
        if expression.arguments().iter().any(|&argument| {
            self.is_or_contains_matching_reference(reference, argument)
                || self.optional_chain_contains_reference(argument, reference)
        }) {
            return true;
        }
        let callee = expression.expression().unwrap();
        if ast::is_property_access_expression(callee)
            && self.is_or_contains_matching_reference(reference, callee.expression().unwrap())
        {
            return true;
        }
        false
    }

    // flow.go:1898
    fn is_or_contains_matching_reference(&mut self, source: P<Node>, target: P<Node>) -> bool {
        self.is_matching_reference(source, target)
            || self.contains_matching_reference(source, target)
    }

    // Return a new type in which occurrences of the string, number and bigint primitives and placeholder template
    // literal types in typeWithPrimitives have been replaced with occurrences of compatible and more specific types
    // from typeWithLiterals. This is essentially a limited form of intersection between the two types. We avoid a
    // true intersection because it is more costly and, when applied to union types, generates a large number of
    // types we don't actually care about.
    // flow.go:1907
    fn replace_primitives_with_literals(
        &mut self,
        type_with_primitives: P<Type>,
        type_with_literals: P<Type>,
    ) -> P<Type> {
        if self.maybe_type_of_kind(
            type_with_primitives,
            TypeFlags::String | TypeFlags::TemplateLiteral | TypeFlags::Number | TypeFlags::BigInt,
        ) && self.maybe_type_of_kind(
            type_with_literals,
            TypeFlags::StringLiteral
                | TypeFlags::TemplateLiteral
                | TypeFlags::StringMapping
                | TypeFlags::NumberLiteral
                | TypeFlags::BigIntLiteral,
        ) {
            return self
                .map_type(type_with_primitives, move |c, t| {
                    if t.flags().intersects(TypeFlags::String) {
                        Some(c.extract_types_of_kind(
                            type_with_literals,
                            TypeFlags::String
                                | TypeFlags::StringLiteral
                                | TypeFlags::TemplateLiteral
                                | TypeFlags::StringMapping,
                        ))
                    } else if c.is_pattern_literal_type(t)
                        && !c.maybe_type_of_kind(
                            type_with_literals,
                            TypeFlags::String
                                | TypeFlags::TemplateLiteral
                                | TypeFlags::StringMapping,
                        )
                    {
                        Some(c.extract_types_of_kind(type_with_literals, TypeFlags::StringLiteral))
                    } else if t.flags().intersects(TypeFlags::Number) {
                        Some(c.extract_types_of_kind(
                            type_with_literals,
                            TypeFlags::Number | TypeFlags::NumberLiteral,
                        ))
                    } else if t.flags().intersects(TypeFlags::BigInt) {
                        Some(c.extract_types_of_kind(
                            type_with_literals,
                            TypeFlags::BigInt | TypeFlags::BigIntLiteral,
                        ))
                    } else {
                        Some(t)
                    }
                })
                .unwrap();
        }
        type_with_primitives
    }
}

// flow.go:1928
fn is_coercible_under_double_equals(source: P<Type>, target: P<Type>) -> bool {
    source.flags().intersects(TypeFlags::Number | TypeFlags::String | TypeFlags::BooleanLiteral)
        && target.flags().intersects(TypeFlags::Number | TypeFlags::String | TypeFlags::Boolean)
}

impl Checker {
    // flow.go:1933
    fn is_exhaustive_switch_statement(&mut self, node: P<Node>) -> bool {
        let links = self.switch_statement_links.get(node);
        if links.exhaustive_state.get() == ExhaustiveState::Unknown {
            // Indicate resolution is in process
            links.exhaustive_state.set(ExhaustiveState::Computing);
            let is_exhaustive = self.compute_exhaustive_switch_statement(node);
            if links.exhaustive_state.get() == ExhaustiveState::Computing {
                links.exhaustive_state.set(if is_exhaustive {
                    ExhaustiveState::True
                } else {
                    ExhaustiveState::False
                });
            }
        } else if links.exhaustive_state.get() == ExhaustiveState::Computing {
            // Resolve circularity to false
            links.exhaustive_state.set(ExhaustiveState::False);
        }
        links.exhaustive_state.get() == ExhaustiveState::True
    }

    // flow.go:1949
    fn compute_exhaustive_switch_statement(&mut self, node: P<Node>) -> bool {
        let expression = node.expression().unwrap();
        if ast::is_type_of_expression(expression) {
            let Some(witnesses) = self.get_switch_clause_type_of_witnesses(node) else {
                return false;
            };
            let operand_type = self.check_expression_cached(expression.expression().unwrap());
            let operand_constraint = self.get_base_constraint_or_type(operand_type);
            // Get the not-equal flags for all handled cases.
            let not_equal_facts = Self::get_not_equal_facts_from_typeof_switch(0, 0, witnesses);
            if operand_constraint.flags().intersects(TypeFlags::AnyOrUnknown) {
                // We special case the top types to be exhaustive when all cases are handled.
                return TypeFacts::AllTypeofNE & not_equal_facts == TypeFacts::AllTypeofNE;
            }
            // A missing not-equal flag indicates that the type wasn't handled by some case.
            return !some_type(self, operand_constraint, |c, t| {
                c.get_type_facts(t, not_equal_facts) == not_equal_facts
            });
        }
        let expression_type = self.check_expression_cached(expression);
        let t = self.get_base_constraint_or_type(expression_type);
        if !is_literal_type(t) {
            return false;
        }
        let switch_types = self.get_switch_clause_types(node);
        if switch_types.is_empty()
            || switch_types.iter().any(|&t| is_neither_unit_type_nor_never(t))
        {
            return false;
        }
        let regular_type =
            self.map_type(t, |c, t| Some(c.get_regular_type_of_literal_type(t))).unwrap();
        Self::each_type_contained_in(regular_type, &switch_types)
    }

    // flow.go:1978
    fn each_type_contained_in(source: P<Type>, types: &[P<Type>]) -> bool {
        if source.flags().intersects(TypeFlags::Union) {
            return !source.as_union_type().types().iter().any(|t| !types.contains(t));
        }
        types.contains(&source)
    }

    // Get the type names from all cases in a switch on `typeof`. The default clause and/or duplicate type names are
    // represented as empty strings. Return nil if one or more case clause expressions are not string literals.
    // flow.go:1989
    fn get_switch_clause_type_of_witnesses(
        &mut self,
        node: P<Node>,
    ) -> Option<&'static [&'static str]> {
        let links = self.switch_statement_links.get(node);
        if !links.witnesses_computed.get() {
            let clauses = node.as_switch_statement().case_block.as_case_block().clauses.nodes();
            let mut witnesses: Option<Vec<&'static str>> = Some(vec![""; clauses.len()]);
            for (i, &clause) in clauses.iter().enumerate() {
                if clause.kind() == Kind::CaseClause {
                    let expression = clause.expression().unwrap();
                    if !ast::is_string_literal_like(expression) {
                        witnesses = None;
                        break;
                    }
                    let text = expression.text();
                    let w = witnesses.as_mut().unwrap();
                    if !w.contains(&text) {
                        w[i] = text;
                    }
                }
            }
            links.witnesses.set(witnesses.map(alloc_vec));
            links.witnesses_computed.set(true);
        }
        links.witnesses.get()
    }

    // Return the combined not-equal type facts for all cases except those between the start and end indices.
    // flow.go:2012
    fn get_not_equal_facts_from_typeof_switch(
        start: usize,
        end: usize,
        witnesses: &[&str],
    ) -> TypeFacts {
        let mut facts = TypeFacts::None;
        for (i, &witness) in witnesses.iter().enumerate() {
            if (i < start || i >= end) && !witness.is_empty() {
                let f = match TYPEOF_NE_FACTS.get(witness) {
                    Some(&f) => f,
                    None => TypeFacts::TypeofNEHostObject,
                };
                facts |= f;
            }
        }
        facts
    }

    // flow.go:2026
    fn get_switch_clause_types(&mut self, node: P<Node>) -> Vec<P<Type>> {
        let links = self.switch_statement_links.get(node);
        if !links.switch_types_computed.get() {
            let clauses = node.as_switch_statement().case_block.as_case_block().clauses.nodes();
            let mut types = Vec::with_capacity(clauses.len());
            types.extend(clauses.iter().map(|&clause| self.get_type_of_switch_clause(clause)));
            links.switch_types.set(alloc_vec(types));
            links.switch_types_computed.set(true);
        }
        links.switch_types.get().to_vec()
    }

    // flow.go:2040
    fn get_type_of_switch_clause(&mut self, clause: P<Node>) -> P<Type> {
        if clause.kind() == Kind::CaseClause {
            let t = self.get_type_of_expression(clause.expression().unwrap());
            return self.get_regular_type_of_literal_type(t);
        }
        self.never_type
    }

    // flow.go:2047
    pub(crate) fn get_effects_signature(&mut self, node: P<Node>) -> Option<P<Signature>> {
        let links = self.signature_links.get(node);
        let effects_signature = links.effects_signature(node);
        let mut signature = effects_signature.get();
        if signature.is_none() {
            // A call expression parented by an expression statement is a potential assertion. Other call
            // expressions are potential type predicate function calls. In order to avoid triggering
            // circularities in control flow analysis, we use getTypeOfDottedName when resolving the call
            // target expression of an assertion.
            let mut func_type: Option<P<Type>> = None;
            if ast::is_binary_expression(node) {
                let right_type =
                    self.check_non_null_expression(node.as_binary_expression().right());
                func_type = self.get_symbol_has_instance_method_of_object_type(right_type);
            } else if ast::is_expression_statement(node.parent().unwrap()) {
                func_type = self
                    .get_type_of_dotted_name(node.expression().unwrap(), None /*diagnostic*/);
            } else if node.expression().unwrap().kind() != Kind::SuperKeyword {
                let expression = node.expression().unwrap();
                if ast::is_optional_chain(node) {
                    let expression_type = self.check_expression(expression);
                    let optional_type =
                        self.get_optional_expression_type(expression_type, expression);
                    func_type = Some(self.check_non_null_type(optional_type, expression));
                } else {
                    func_type = Some(self.check_non_null_expression(expression));
                }
            }
            let mut apparent_type: Option<P<Type>> = None;
            if let Some(func_type) = func_type {
                apparent_type = Some(self.get_apparent_type(func_type));
            }
            let signatures = self.get_signatures_of_type(
                apparent_type.unwrap_or(self.unknown_type),
                SignatureKind::Call,
            );
            if signatures.len() == 1 && signatures[0].type_parameters().is_empty() {
                signature = Some(signatures[0]);
            } else if signatures.iter().any(|&s| self.has_type_predicate_or_never_return_type(s)) {
                signature = Some(self.get_resolved_signature(node, None, CheckMode::Normal));
            }
            if !(signature.is_some()
                && self.has_type_predicate_or_never_return_type(signature.unwrap()))
            {
                signature = Some(self.unknown_signature);
            }
            effects_signature.set(signature);
        }
        if signature == Some(self.unknown_signature) {
            return None;
        }
        signature
    }

    /**
     * Get the type of the `[Symbol.hasInstance]` method of an object type.
     */
    // flow.go:2093
    pub(crate) fn get_symbol_has_instance_method_of_object_type(
        &mut self,
        t: P<Type>,
    ) -> Option<P<Type>> {
        let has_instance_property_name =
            self.get_property_name_for_known_symbol_name("hasInstance");
        if self.all_types_assignable_to_kind(t, TypeFlags::NonPrimitive) {
            let has_instance_property = self.get_property_of_type(t, &has_instance_property_name);
            if let Some(has_instance_property) = has_instance_property {
                let has_instance_property_type = self.get_type_of_symbol(has_instance_property);
                if !self
                    .get_signatures_of_type(has_instance_property_type, SignatureKind::Call)
                    .is_empty()
                {
                    return Some(has_instance_property_type);
                }
            }
        }
        None
    }

    // flow.go:2107
    pub(crate) fn get_property_name_for_known_symbol_name(&mut self, symbol_name: &str) -> String {
        let ctor_type = self.get_global_es_symbol_constructor_symbol_or_nil();
        if let Some(ctor_type) = ctor_type {
            let ctor_symbol_type = self.get_type_of_symbol(ctor_type);
            let unique_type = self.get_type_of_property_of_type(ctor_symbol_type, symbol_name);
            if let Some(unique_type) = unique_type
                && is_type_usable_as_property_name(unique_type)
            {
                return get_property_name_from_type(unique_type).into_owned();
            }
        }
        format!("{}@{}", ast::INTERNAL_SYMBOL_NAME_PREFIX, symbol_name)
    }

    // We require the dotted function name in an assertion expression to be comprised of identifiers
    // that reference function, method, class or value module symbols; or variable, property or
    // parameter symbols with declarations that have explicit type annotations. Such references are
    // resolvable with no possibility of triggering circularities in control flow analysis.
    // flow.go:2122
    pub(crate) fn get_type_of_dotted_name(
        &mut self,
        node: P<Node>,
        diagnostic: Option<P<Diagnostic>>,
    ) -> Option<P<Type>> {
        if !node.flags().intersects(NodeFlags::InWithStatement) {
            match node.kind() {
                Kind::Identifier => {
                    let resolved = self.get_resolved_symbol(node);
                    let symbol =
                        self.get_export_symbol_of_value_symbol_if_exported(Some(resolved)).unwrap();
                    return self.get_explicit_type_of_symbol(symbol, diagnostic);
                }
                Kind::ThisKeyword => {
                    return self.get_explicit_this_type(node);
                }
                Kind::SuperKeyword => {
                    return Some(self.check_super_expression(node));
                }
                Kind::PropertyAccessExpression => {
                    let t = self.get_type_of_dotted_name(node.expression().unwrap(), diagnostic);
                    if let Some(t) = t {
                        let name = node.name().unwrap();
                        let mut prop: Option<P<Symbol>> = None;
                        if ast::is_private_identifier(name) {
                            if let Some(symbol) = t.symbol() {
                                let private_name =
                                    tsrs_binder::get_symbol_name_for_private_identifier(
                                        symbol,
                                        name.text(),
                                    );
                                prop = self.get_property_of_type(t, &private_name);
                            }
                        } else {
                            prop = self.get_property_of_type(t, name.text());
                        }
                        if let Some(prop) = prop {
                            return self.get_explicit_type_of_symbol(prop, diagnostic);
                        }
                    }
                }
                Kind::ParenthesizedExpression => {
                    return self.get_type_of_dotted_name(node.expression().unwrap(), diagnostic);
                }
                _ => {}
            }
        }
        None
    }

    // flow.go:2155
    fn get_explicit_type_of_symbol(
        &mut self,
        symbol: P<Symbol>,
        diagnostic: Option<P<Diagnostic>>,
    ) -> Option<P<Type>> {
        let symbol = self.resolve_symbol(symbol);
        if !self.resolving_explicit_type_of_symbol.add_if_absent(symbol) {
            return None;
        }
        let result = self.get_explicit_type_of_symbol_worker(symbol, diagnostic);
        self.resolving_explicit_type_of_symbol.delete(&symbol);
        result
    }

    // Body of getExplicitTypeOfSymbol after the circularity guard (Go uses `defer` to remove the guard).
    fn get_explicit_type_of_symbol_worker(
        &mut self,
        symbol: P<Symbol>,
        diagnostic: Option<P<Diagnostic>>,
    ) -> Option<P<Type>> {
        if symbol.flags().intersects(
            SymbolFlags::Function
                | SymbolFlags::Method
                | SymbolFlags::Class
                | SymbolFlags::ValueModule,
        ) {
            return Some(self.get_type_of_symbol(symbol));
        }
        if symbol.flags().intersects(SymbolFlags::Variable | SymbolFlags::Property) {
            if symbol.check_flags().intersects(CheckFlags::Mapped) {
                let origin = self.mapped_symbol_links.get(symbol).synthetic_origin.get();
                if let Some(origin) = origin
                    && self.get_explicit_type_of_symbol(origin, diagnostic).is_some()
                {
                    return Some(self.get_type_of_symbol(symbol));
                }
            }
            let declaration = symbol.value_declaration();
            if let Some(declaration) = declaration {
                if Self::is_declaration_with_explicit_type_annotation(declaration) {
                    return Some(self.get_type_of_symbol(symbol));
                }
                if ast::is_variable_declaration(declaration)
                    && ast::is_for_of_statement(declaration.parent().unwrap().parent().unwrap())
                {
                    let statement = declaration.parent().unwrap().parent().unwrap();
                    let expression_type = self.get_type_of_dotted_name(
                        statement.expression().unwrap(),
                        None, /*diagnostic*/
                    );
                    if let Some(expression_type) = expression_type {
                        let use_ = if statement.as_for_in_or_of_statement().await_modifier.is_some()
                        {
                            IterationUse::ForAwaitOf
                        } else {
                            IterationUse::ForOf
                        };
                        let undefined_type = self.undefined_type;
                        return Some(self.check_iterated_type_or_element_type(
                            use_,
                            expression_type,
                            undefined_type,
                            None, /*errorNode*/
                        ));
                    }
                }
                if let Some(diagnostic) = diagnostic {
                    let symbol_string = self.symbol_to_string(symbol);
                    diagnostic.add_related_info(create_diagnostic_for_node(
                        Some(declaration),
                        &diagnostics::X_0_NEEDS_AN_EXPLICIT_TYPE_ANNOTATION,
                        &[&symbol_string],
                    ));
                }
            }
        }
        None
    }

    // flow.go:2197
    fn is_declaration_with_explicit_type_annotation(node: P<Node>) -> bool {
        (ast::is_variable_declaration(node)
            || ast::is_property_declaration(node)
            || ast::is_property_signature_declaration(node)
            || ast::is_parameter_declaration(node))
            && node.type_node().is_some()
            || Self::is_expando_property_function_with_return_type_annotation(node)
    }

    // flow.go:2202
    fn is_expando_property_function_with_return_type_annotation(node: P<Node>) -> bool {
        if ast::is_binary_expression(node) {
            let expr = node.as_binary_expression().right();
            if ast::is_function_like(expr) && expr.type_node().is_some() {
                return true;
            }
        }
        false
    }

    // flow.go:2211
    fn has_type_predicate_or_never_return_type(&mut self, sig: P<Signature>) -> bool {
        if self.get_type_predicate_of_signature(sig).is_some() {
            return true;
        }
        match sig.declaration.get() {
            Some(declaration) => {
                let return_type =
                    self.get_return_type_from_annotation(declaration).unwrap_or(self.unknown_type);
                return_type.flags().intersects(TypeFlags::Never)
            }
            None => false,
        }
    }

    // flow.go:2215
    fn get_explicit_this_type(&mut self, node: P<Node>) -> Option<P<Type>> {
        let container = ast::get_this_container(
            node, false, /*includeArrowFunctions*/
            false, /*includeClassComputedPropertyName*/
        );
        if ast::is_function_like(container) {
            let signature = self.get_signature_from_declaration(container);
            if let Some(this_parameter) = signature.this_parameter() {
                return self.get_explicit_type_of_symbol(this_parameter, None);
            }
        }
        if let Some(parent) = container.parent()
            && ast::is_class_like(parent)
        {
            let symbol = self.get_symbol_of_declaration(parent).unwrap();
            if ast::is_static(container) {
                return Some(self.get_type_of_symbol(symbol));
            }
            return self.get_declared_type_of_symbol(symbol).as_interface_type().this_type.get();
        }
        None
    }

    // flow.go:2234
    fn get_initial_type(&mut self, node: P<Node>) -> P<Type> {
        match node.kind() {
            Kind::VariableDeclaration => {
                return self.get_initial_type_of_variable_declaration(node);
            }
            Kind::BindingElement => return self.get_initial_type_of_binding_element(node),
            _ => {}
        }
        panic!("Unhandled case in getInitialType");
    }

    // flow.go:2244
    fn get_initial_type_of_variable_declaration(&mut self, node: P<Node>) -> P<Type> {
        if let Some(initializer) = node.initializer() {
            return self.get_type_of_initializer(initializer);
        }
        let grandparent = node.parent().unwrap().parent().unwrap();
        if ast::is_for_in_statement(grandparent) {
            return self.string_type;
        }
        if ast::is_for_of_statement(grandparent) {
            return self.check_right_hand_side_of_for_of(grandparent);
        }
        self.error_type
    }

    // flow.go:2260
    pub(crate) fn get_type_of_initializer(&mut self, node: P<Node>) -> P<Type> {
        // Return the cached type if one is available. If the type of the variable was inferred
        // from its initializer, we'll already have cached the type. Otherwise we compute it now
        // without caching such that transient types are reflected.
        if self.type_node_links.has(node) {
            let t = self.type_node_links.get(node).resolved_type.get();
            if let Some(t) = t {
                return t;
            }
        }
        self.get_type_of_expression(node)
    }

    // flow.go:2273
    fn get_initial_type_of_binding_element(&mut self, node: P<Node>) -> P<Type> {
        let pattern = node.parent().unwrap();
        let parent_type = self.get_initial_type(pattern.parent().unwrap());
        let t = if ast::is_object_binding_pattern(pattern) {
            self.get_type_of_destructured_property(
                parent_type,
                get_binding_element_property_name(node).unwrap(),
            )
        } else if !has_dot_dot_dot_token(node) {
            let index = pattern
                .elements()
                .iter()
                .position(|&e| e == node)
                .expect("binding element belongs to its pattern");
            self.get_type_of_destructured_array_element(parent_type, index)
        } else {
            self.get_type_of_destructured_spread_expression(parent_type)
        };
        self.get_type_with_default(t, node.initializer())
    }

    // flow.go:2288
    fn get_assigned_type(&mut self, node: P<Node>) -> P<Type> {
        let parent = node.parent().unwrap();
        match parent.kind() {
            Kind::ForInStatement => return self.string_type,
            Kind::ForOfStatement => {
                return self.check_right_hand_side_of_for_of(parent);
            }
            Kind::BinaryExpression => return self.get_assigned_type_of_binary_expression(parent),
            Kind::DeleteExpression => return self.undefined_type,
            Kind::ArrayLiteralExpression => {
                return self.get_assigned_type_of_array_literal_element(parent, node);
            }
            Kind::SpreadElement => return self.get_assigned_type_of_spread_expression(parent),
            Kind::PropertyAssignment => {
                return self.get_assigned_type_of_property_assignment(parent);
            }
            Kind::ShorthandPropertyAssignment => {
                return self.get_assigned_type_of_shorthand_property_assignment(parent);
            }
            _ => {}
        }
        self.error_type
    }

    // flow.go:2314
    fn get_assigned_type_of_binary_expression(&mut self, node: P<Node>) -> P<Type> {
        let parent = node.parent().unwrap();
        let is_destructuring_default_assignment = ast::is_array_literal_expression(parent)
            && Self::is_destructuring_assignment_target(parent)
            || ast::is_property_assignment(parent)
                && Self::is_destructuring_assignment_target(parent.parent().unwrap());
        if is_destructuring_default_assignment {
            let assigned_type = self.get_assigned_type(node);
            return self
                .get_type_with_default(assigned_type, Some(node.as_binary_expression().right()));
        }
        self.get_type_of_expression(node.as_binary_expression().right())
    }

    // flow.go:2323
    fn get_assigned_type_of_array_literal_element(
        &mut self,
        node: P<Node>,
        element: P<Node>,
    ) -> P<Type> {
        let assigned_type = self.get_assigned_type(node);
        let index = node
            .elements()
            .iter()
            .position(|&e| e == element)
            .expect("element belongs to its array literal");
        self.get_type_of_destructured_array_element(assigned_type, index)
    }

    // flow.go:2327
    fn get_type_of_destructured_array_element(&mut self, t: P<Type>, index: usize) -> P<Type> {
        if every_type(self, t, super::checker::Checker::is_tuple_like_type)
            && let Some(element_type) = self.get_tuple_element_type(t, index)
        {
            return element_type;
        }
        let undefined_type = self.undefined_type;
        let element_type = self.check_iterated_type_or_element_type(
            IterationUse::Destructuring,
            t,
            undefined_type,
            None, /*errorNode*/
        );
        self.include_undefined_in_index_signature(element_type)
    }

    // flow.go:2339
    pub(crate) fn include_undefined_in_index_signature(&mut self, t: P<Type>) -> P<Type> {
        if self.compiler_options.no_unchecked_indexed_access == Tristate::True {
            return self.get_union_type(&[t, self.missing_type]);
        }
        t
    }

    // flow.go:2349
    fn get_assigned_type_of_spread_expression(&mut self, node: P<Node>) -> P<Type> {
        let assigned_type = self.get_assigned_type(node.parent().unwrap());
        self.get_type_of_destructured_spread_expression(assigned_type)
    }

    // flow.go:2353
    fn get_type_of_destructured_spread_expression(&mut self, t: P<Type>) -> P<Type> {
        let undefined_type = self.undefined_type;
        let element_type = self.check_iterated_type_or_element_type(
            IterationUse::Destructuring,
            t,
            undefined_type,
            None, /*errorNode*/
        );
        self.create_array_type(element_type)
    }

    // flow.go:2361
    fn get_assigned_type_of_property_assignment(&mut self, node: P<Node>) -> P<Type> {
        let assigned_type = self.get_assigned_type(node.parent().unwrap());
        self.get_type_of_destructured_property(assigned_type, node.name().unwrap())
    }

    // flow.go:2365
    fn get_type_of_destructured_property(&mut self, t: P<Type>, name: P<Node>) -> P<Type> {
        let name_type = self.get_literal_type_from_property_name(name);
        if !is_type_usable_as_property_name(name_type) {
            return self.error_type;
        }
        let text = get_property_name_from_type(name_type);
        if let Some(prop_type) = self.get_type_of_property_of_type(t, &text) {
            return prop_type;
        }
        if let Some(index_info) = self.get_applicable_index_info_for_name(t, &text) {
            return self.include_undefined_in_index_signature(index_info.value_type.get().unwrap());
        }
        self.error_type
    }

    // flow.go:2380
    fn get_assigned_type_of_shorthand_property_assignment(&mut self, node: P<Node>) -> P<Type> {
        let assigned_type = self.get_assigned_type_of_property_assignment(node);
        self.get_type_with_default(
            assigned_type,
            node.as_shorthand_property_assignment().object_assignment_initializer(),
        )
    }

    // flow.go:2384
    pub(crate) fn is_destructuring_assignment_target(parent: P<Node>) -> bool {
        let grandparent = parent.parent().unwrap();
        ast::is_binary_expression(grandparent) && grandparent.as_binary_expression().left == parent
            || ast::is_for_of_statement(grandparent) && grandparent.initializer() == Some(parent)
    }

    // flow.go:2389
    fn get_type_with_default(
        &mut self,
        t: P<Type>,
        default_expression: Option<P<Node>>,
    ) -> P<Type> {
        if let Some(default_expression) = default_expression {
            let non_undefined_type = self.get_non_undefined_type(t);
            let default_type = self.get_type_of_expression(default_expression);
            return self.get_union_type(&[non_undefined_type, default_type]);
        }
        t
    }

    // Remove those constituent types of declaredType to which no constituent type of assignedType is assignable.
    // For example, when a variable of type number | string | boolean is assigned a value of type number | boolean,
    // we remove type string.
    // flow.go:2399
    fn get_assignment_reduced_type(
        &mut self,
        declared_type: P<Type>,
        assigned_type: P<Type>,
    ) -> P<Type> {
        if declared_type == assigned_type {
            return declared_type;
        }
        if assigned_type.flags().intersects(TypeFlags::Never) {
            return assigned_type;
        }
        let key = AssignmentReducedKey { id1: declared_type.id, id2: assigned_type.id };
        if let Some(&result) = self.assignment_reduced_types.get(&key) {
            return result;
        }
        let result = self.get_assignment_reduced_type_worker(declared_type, assigned_type);
        self.assignment_reduced_types.insert(key, result);
        result
    }

    // flow.go:2415
    fn get_assignment_reduced_type_worker(
        &mut self,
        declared_type: P<Type>,
        assigned_type: P<Type>,
    ) -> P<Type> {
        let filtered_type = self
            .filter_type(declared_type, move |c, t| c.type_maybe_assignable_to(assigned_type, t));
        // Ensure that we narrow to fresh types if the assignment is a fresh boolean literal type.
        let mut reduced_type = filtered_type;
        if assigned_type.flags().intersects(TypeFlags::BooleanLiteral)
            && is_fresh_literal_type(assigned_type)
        {
            reduced_type = self
                .map_type(filtered_type, |c, t| Some(c.get_fresh_type_of_literal_type(t)))
                .unwrap();
        }
        // Our crude heuristic produces an invalid result in some cases: see GH#26130.
        // For now, when that happens, we give up and don't narrow at all.  (This also
        // means we'll never narrow for erroneous assignments where the assigned type
        // is not assignable to the declared type.)
        if self.is_type_assignable_to(assigned_type, reduced_type) {
            return reduced_type;
        }
        declared_type
    }

    // flow.go:2434
    fn type_maybe_assignable_to(&mut self, source: P<Type>, target: P<Type>) -> bool {
        if !source.flags().intersects(TypeFlags::Union) {
            return self.is_type_assignable_to(source, target);
        }
        // Quick exit when source union contains the target type
        if contains_type(self, source.types(), target) {
            return true;
        }
        // Otherwise, check if any constituent type of the source union is assignable to the target type
        source.types().iter().any(|&t| self.is_type_assignable_to(t, target))
    }

    // flow.go:2451
    fn get_type_predicate_argument(
        predicate: P<TypePredicate>,
        call_expression: P<Node>,
    ) -> Option<P<Node>> {
        if predicate.kind.get() == TypePredicateKind::Identifier
            || predicate.kind.get() == TypePredicateKind::AssertsIdentifier
        {
            let arguments = call_expression.arguments();
            let parameter_index = predicate.parameter_index.get();
            if let Some(parameter_index) =
                parameter_index.filter(|&parameter_index| parameter_index < arguments.len())
            {
                return Some(arguments[parameter_index]);
            }
        } else {
            let invoked_expression = ast::skip_parentheses(call_expression.expression().unwrap());
            if ast::is_access_expression(invoked_expression) {
                return Some(ast::skip_parentheses(invoked_expression.expression().unwrap()));
            }
        }
        None
    }

    // flow.go:2466
    pub(crate) fn get_flow_type_in_constructor(
        &mut self,
        symbol: P<Symbol>,
        constructor: P<Node>,
    ) -> Option<P<Type>> {
        let symbol_name = symbol.name();
        let access_name =
            if symbol_name.starts_with(&format!("{}#", ast::INTERNAL_SYMBOL_NAME_PREFIX)) {
                let start = symbol_name.find('@').map_or(0, |i| i + 1);
                self.factory.new_private_identifier(alloc_str(&symbol_name[start..]))
            } else {
                self.factory.new_identifier(symbol_name)
            };
        let this_keyword = self.factory.new_keyword_expression(Kind::ThisKeyword);
        let reference = self.factory.new_property_access_expression(
            this_keyword,
            None,
            access_name,
            NodeFlags::None,
        );
        reference.expression().unwrap().set_parent(Some(reference));
        reference.set_parent(Some(constructor));
        reference.set_flow_node(constructor.as_constructor_declaration().return_flow_node.get());
        let flow_type = self.get_flow_type_of_property(reference, Some(symbol));
        if self.no_implicit_any
            && (flow_type == self.auto_type || flow_type == self.auto_array_type)
        {
            let symbol_string = self.symbol_to_string(symbol);
            let type_string = self.type_to_string_exported(flow_type);
            self.error(
                symbol.value_declaration(),
                &diagnostics::MEMBER_0_IMPLICITLY_HAS_AN_1_TYPE,
                &[&symbol_string, &type_string],
            );
        }
        // We don't infer a type if assignments are only null or undefined.
        if every_type(self, flow_type, super::checker::Checker::is_nullable_type) {
            return None;
        }
        Some(self.convert_auto_to_any(flow_type))
    }

    // flow.go:2488
    pub(crate) fn get_flow_type_in_static_blocks(
        &mut self,
        symbol: P<Symbol>,
        static_blocks: &[P<Node>],
    ) -> Option<P<Type>> {
        let symbol_name = symbol.name();
        let access_name =
            if symbol_name.starts_with(&format!("{}#", ast::INTERNAL_SYMBOL_NAME_PREFIX)) {
                let start = symbol_name.find('@').map_or(0, |i| i + 1);
                self.factory.new_private_identifier(alloc_str(&symbol_name[start..]))
            } else {
                self.factory.new_identifier(symbol_name)
            };
        if let Some(result) = static_blocks.iter().find_map(|&static_block| {
            let this_keyword = self.factory.new_keyword_expression(Kind::ThisKeyword);
            let reference = self.factory.new_property_access_expression(
                this_keyword,
                None,
                access_name,
                NodeFlags::None,
            );
            reference.expression().unwrap().set_parent(Some(reference));
            reference.set_parent(Some(static_block));
            reference.set_flow_node(
                static_block.as_class_static_block_declaration().return_flow_node.get(),
            );
            let flow_type = self.get_flow_type_of_property(reference, Some(symbol));
            if self.no_implicit_any
                && (flow_type == self.auto_type || flow_type == self.auto_array_type)
            {
                let symbol_string = self.symbol_to_string(symbol);
                let type_string = self.type_to_string_exported(flow_type);
                self.error(
                    symbol.value_declaration(),
                    &diagnostics::MEMBER_0_IMPLICITLY_HAS_AN_1_TYPE,
                    &[&symbol_string, &type_string],
                );
            }
            // We don't infer a type if assignments are only null or undefined.
            if every_type(self, flow_type, super::checker::Checker::is_nullable_type) {
                return None;
            }
            Some(Some(self.convert_auto_to_any(flow_type)))
        }) {
            return result;
        }
        None
    }

    // flow.go:2513
    pub(crate) fn is_reachable_flow_node(&mut self, flow: P<FlowNode>) -> bool {
        let f = self.get_flow_state();
        let result = self.is_reachable_flow_node_worker(f, flow, false /*noCacheCheck*/);
        self.put_flow_state(f);
        self.last_flow_node = Some(flow);
        self.last_flow_node_reachable = result;
        result
    }

    // flow.go:2522
    fn is_reachable_flow_node_worker(
        &mut self,
        f: P<FlowState>,
        flow: P<FlowNode>,
        no_cache_check: bool,
    ) -> bool {
        let mut flow = flow;
        let mut no_cache_check = no_cache_check;
        loop {
            if Some(flow) == self.last_flow_node {
                return self.last_flow_node_reachable;
            }
            let flags = flow.flags();
            if flags.intersects(FlowFlags::Shared) {
                if !no_cache_check && f.reduce_labels.borrow().is_empty() {
                    if let Some(&reachable) = self.flow_node_reachable.get(&flow) {
                        return reachable;
                    }
                    let reachable =
                        self.is_reachable_flow_node_worker(f, flow, true /*noCacheCheck*/);
                    self.flow_node_reachable.insert(flow, reachable);
                    return reachable;
                }
                no_cache_check = false;
            }
            if flags
                .intersects(FlowFlags::Assignment | FlowFlags::Condition | FlowFlags::ArrayMutation)
            {
                flow = flow.antecedent().unwrap();
            } else if flags.intersects(FlowFlags::Call) {
                let node = flow.node().unwrap();
                if let Some(signature) = self.get_effects_signature(node) {
                    if let Some(predicate) = self.get_type_predicate_of_signature(signature)
                        && predicate.kind.get() == TypePredicateKind::AssertsIdentifier
                        && predicate.t.get().is_none()
                    {
                        let arguments = node.arguments();
                        let parameter_index = predicate.parameter_index.get();
                        if let Some(parameter_index) = parameter_index
                            .filter(|&parameter_index| parameter_index < arguments.len())
                            && Self::is_false_expression(arguments[parameter_index])
                        {
                            return false;
                        }
                    }
                    if self
                        .get_return_type_of_signature(signature)
                        .flags()
                        .intersects(TypeFlags::Never)
                    {
                        return false;
                    }
                }
                flow = flow.antecedent().unwrap();
            } else if flags.intersects(FlowFlags::BranchLabel) {
                // A branching point is reachable if any branch is reachable.
                let mut next = get_branch_label_antecedents(flow, &f.reduce_labels.borrow());
                while let Some(list) = next {
                    next = list.next.get();
                    if self.is_reachable_flow_node_worker(f, list.flow, false /*noCacheCheck*/) {
                        return true;
                    }
                }
                return false;
            } else if flags.intersects(FlowFlags::LoopLabel) {
                let Some(antecedents) = flow.antecedents() else {
                    return false;
                };
                // A loop is reachable if the control flow path that leads to the top is reachable.
                flow = antecedents.flow;
            } else if flags.intersects(FlowFlags::SwitchClause) {
                // The control flow path representing an unmatched value in a switch statement with
                // no default clause is unreachable if the switch statement is exhaustive.
                let data = flow.node().unwrap().as_flow_switch_clause_data();
                if data.clause_start == data.clause_end
                    && self.is_exhaustive_switch_statement(data.switch_statement)
                {
                    return false;
                }
                flow = flow.antecedent().unwrap();
            } else if flags.intersects(FlowFlags::ReduceLabel) {
                // Cache is unreliable once we start adjusting labels
                self.last_flow_node = None;
                f.reduce_labels
                    .borrow_mut()
                    .push(flow.node().unwrap().as_flow_reduce_label_data_p());
                let result = self.is_reachable_flow_node_worker(
                    f,
                    flow.antecedent().unwrap(),
                    false, /*noCacheCheck*/
                );
                f.reduce_labels.borrow_mut().pop();
                return result;
            } else {
                return !flags.intersects(FlowFlags::Unreachable);
            }
        }
    }

    // flow.go:2589
    fn is_false_expression(expr: P<Node>) -> bool {
        let node = ast::skip_parentheses(expr);
        if node.kind() == Kind::FalseKeyword {
            return true;
        }
        if ast::is_binary_expression(node) {
            let binary = node.as_binary_expression();
            return binary.operator_token.kind() == Kind::AmpersandAmpersandToken
                && (Self::is_false_expression(binary.left)
                    || Self::is_false_expression(binary.right()))
                || binary.operator_token.kind() == Kind::BarBarToken
                    && Self::is_false_expression(binary.left)
                    && Self::is_false_expression(binary.right());
        }
        false
    }

    // Return true if the given flow node is preceded by a 'super(...)' call in every possible code path
    // leading to the node.
    // flow.go:2604
    pub(crate) fn is_post_super_flow_node(
        &mut self,
        flow: P<FlowNode>,
        no_cache_check: bool,
    ) -> bool {
        let f = self.get_flow_state();
        let result = self.is_post_super_flow_node_worker(f, flow, no_cache_check);
        self.put_flow_state(f);
        result
    }

    // flow.go:2611
    fn is_post_super_flow_node_worker(
        &mut self,
        f: P<FlowState>,
        flow: P<FlowNode>,
        no_cache_check: bool,
    ) -> bool {
        let mut flow = flow;
        let mut no_cache_check = no_cache_check;
        loop {
            let flags = flow.flags();
            if flags.intersects(FlowFlags::Shared) {
                if !no_cache_check {
                    if let Some(&post_super) = self.flow_node_post_super.get(&flow) {
                        return post_super;
                    }
                    let post_super =
                        self.is_post_super_flow_node_worker(f, flow, true /*noCacheCheck*/);
                    self.flow_node_post_super.insert(flow, post_super);
                }
                no_cache_check = false;
            }
            if flags.intersects(
                FlowFlags::Assignment
                    | FlowFlags::Condition
                    | FlowFlags::ArrayMutation
                    | FlowFlags::SwitchClause,
            ) {
                flow = flow.antecedent().unwrap();
            } else if flags.intersects(FlowFlags::Call) {
                if flow.node().unwrap().expression().unwrap().kind() == Kind::SuperKeyword {
                    return true;
                }
                flow = flow.antecedent().unwrap();
            } else if flags.intersects(FlowFlags::BranchLabel) {
                let mut next = get_branch_label_antecedents(flow, &f.reduce_labels.borrow());
                while let Some(list) = next {
                    next = list.next.get();
                    if !self
                        .is_post_super_flow_node_worker(f, list.flow, false /*noCacheCheck*/)
                    {
                        return false;
                    }
                }
                return true;
            } else if flags.intersects(FlowFlags::LoopLabel) {
                // A loop is post-super if the control flow path that leads to the top is post-super.
                flow = flow.antecedents().unwrap().flow;
            } else if flags.intersects(FlowFlags::ReduceLabel) {
                f.reduce_labels
                    .borrow_mut()
                    .push(flow.node().unwrap().as_flow_reduce_label_data_p());
                let result = self.is_post_super_flow_node_worker(
                    f,
                    flow.antecedent().unwrap(),
                    false, /*noCacheCheck*/
                );
                f.reduce_labels.borrow_mut().pop();
                return result;
            } else {
                // Unreachable nodes are considered post-super to silence errors
                return flags.intersects(FlowFlags::Unreachable);
            }
        }
    }

    // Check if a parameter, catch variable, or mutable local variable is definitely assigned anywhere
    // flow.go:2655
    pub(crate) fn is_symbol_assigned_definitely(&mut self, symbol: P<Symbol>) -> bool {
        self.ensure_assignments_marked(symbol);
        self.marked_assignment_symbol_links.get(symbol).has_definite_assignment.get()
    }

    // Check if a parameter, catch variable, or mutable local variable is assigned anywhere
    // flow.go:2661
    pub(crate) fn is_symbol_assigned(&mut self, symbol: P<Symbol>) -> bool {
        self.ensure_assignments_marked(symbol);
        self.marked_assignment_symbol_links.get(symbol).last_assignment_pos.get()
            != LastAssignmentPosition::None
    }

    // Return true if there are no assignments to the given symbol or if the given location
    // is past the last assignment to the symbol.
    // flow.go:2668
    pub(crate) fn is_past_last_assignment(
        &mut self,
        symbol: P<Symbol>,
        location: Option<P<Node>>,
    ) -> bool {
        self.ensure_assignments_marked(symbol);
        let last_assignment_pos =
            self.marked_assignment_symbol_links.get(symbol).last_assignment_pos.get();
        match last_assignment_pos {
            LastAssignmentPosition::None => true,
            LastAssignmentPosition::Position(pos) => {
                location.is_some_and(|location| pos < location.pos())
            }
            LastAssignmentPosition::BeyondAll => false,
        }
    }

    // flow.go:2674
    fn ensure_assignments_marked(&mut self, symbol: P<Symbol>) {
        let Some(parent) =
            ast::find_ancestor(symbol.value_declaration(), ast::is_function_or_source_file)
        else {
            return;
        };
        let links = self.node_links.get(parent);
        if !links.flags.get().intersects(NodeCheckFlags::AssignmentsMarked) {
            links.flags.set(links.flags.get() | NodeCheckFlags::AssignmentsMarked);
            if !self.has_parent_with_assignments_marked(parent) {
                self.mark_node_assignments(parent);
            }
        }
    }

    // flow.go:2688
    fn has_parent_with_assignments_marked(&mut self, node: P<Node>) -> bool {
        ast::find_ancestor(node.parent(), |node| {
            ast::is_function_or_source_file(node)
                && self
                    .node_links
                    .get(node)
                    .flags
                    .get()
                    .intersects(NodeCheckFlags::AssignmentsMarked)
        })
        .is_some()
    }

    // For all assignments within the given root node, record the last assignment source position for all
    // referenced parameters and mutable local variables. When assignments occur in nested functions  or
    // references occur in export specifiers, record math.MaxInt32 as the assignment position. When
    // assignments occur in compound statements, record the ending source position of the compound statement
    // as the assignment position (this is more conservative than full control flow analysis, but requires
    // only a single walk over the AST).
    // flow.go:2700
    pub(crate) fn mark_node_assignments_worker(&mut self, node: P<Node>) -> bool {
        match node.kind() {
            Kind::Identifier => {
                let assignment_kind = get_assignment_target_kind(node);
                if assignment_kind != AssignmentKind::None {
                    let symbol = self.get_resolved_symbol(node);
                    if Self::is_parameter_or_mutable_local_variable(symbol) {
                        let links = self.marked_assignment_symbol_links.get(symbol);
                        let pos = links.last_assignment_pos.get();
                        if pos != LastAssignmentPosition::BeyondAll {
                            let referencing_function =
                                ast::find_ancestor(node, ast::is_function_or_source_file);
                            let declaring_function = ast::find_ancestor(
                                symbol.value_declaration(),
                                ast::is_function_or_source_file,
                            );
                            if referencing_function == declaring_function {
                                links.last_assignment_pos.set(LastAssignmentPosition::Position(
                                    Self::extend_assignment_position(
                                        Some(node),
                                        symbol.value_declaration().unwrap(),
                                    ),
                                ));
                            } else {
                                links.last_assignment_pos.set(LastAssignmentPosition::BeyondAll);
                            }
                        }
                        if assignment_kind == AssignmentKind::Definite {
                            links.has_definite_assignment.set(true);
                        }
                    }
                }
                return false;
            }
            Kind::ExportSpecifier => {
                let export_declaration_node = node.parent().unwrap().parent().unwrap();
                let export_declaration = export_declaration_node.as_export_declaration();
                let name = node.property_name_or_name().unwrap();
                if !node.is_type_only()
                    && !export_declaration.is_type_only
                    && export_declaration.module_specifier.is_none()
                    && !ast::is_string_literal(name)
                {
                    let symbol = self.resolve_entity_name(
                        name,
                        SymbolFlags::Value,
                        true, /*ignoreErrors*/
                        true, /*dontResolveAlias*/
                        None,
                    );
                    if let Some(symbol) = symbol
                        && Self::is_parameter_or_mutable_local_variable(symbol)
                    {
                        let links = self.marked_assignment_symbol_links.get(symbol);
                        links.last_assignment_pos.set(LastAssignmentPosition::BeyondAll);
                    }
                }
                return false;
            }
            Kind::InterfaceDeclaration
            | Kind::TypeAliasDeclaration
            | Kind::JSTypeAliasDeclaration
            | Kind::EnumDeclaration => {
                return false;
            }
            _ => {}
        }
        if ast::is_type_node(node) {
            return false;
        }
        node.for_each_child(&mut |child| self.mark_node_assignments(child))
    }

    // Extend the position of the given assignment target node to the end of any intervening variable statement,
    // expression statement, compound statement, or class declaration occurring between the node and the given
    // declaration node.
    // flow.go:2749
    fn extend_assignment_position(node: Option<P<Node>>, declaration: P<Node>) -> TextPos {
        let mut node = node;
        let mut pos = node.unwrap().pos();
        while let Some(n) = node {
            if n.pos() <= declaration.pos() {
                break;
            }
            match n.kind() {
                Kind::VariableStatement
                | Kind::ExpressionStatement
                | Kind::IfStatement
                | Kind::DoStatement
                | Kind::WhileStatement
                | Kind::ForStatement
                | Kind::ForInStatement
                | Kind::ForOfStatement
                | Kind::WithStatement
                | Kind::SwitchStatement
                | Kind::TryStatement
                | Kind::ClassDeclaration => {
                    pos = n.end();
                }
                _ => {}
            }
            node = n.parent();
        }
        pos
    }
}
