//! Non-function declarations of `flow.go`.

use std::sync::LazyLock;

use crate::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FlowType {
    pub(crate) t: Option<P<Type>>,
    pub(crate) incomplete: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SharedFlow {
    pub(crate) flow: P<FlowNode>,
    pub(crate) flow_type: FlowType,
    /// Flow memo (flowmemo.rs): the taint sources and flags of the frame that computed the value, and the height of
    /// the sub-walk it stands for.
    pub(crate) transient: u32,
    pub(crate) reference: u32,
    pub(crate) height: u16,
    pub(crate) flags: u8,
}

/// Arena handle (`P<FlowState>`), pooled through `Checker::free_flow_state` like Go.
#[derive(Default)]
pub struct FlowState {
    pub(crate) reference: Cell<Option<P<Node>>>,
    pub(crate) declared_type: Cell<Option<P<Type>>>,
    pub(crate) initial_type: Cell<Option<P<Type>>>,
    pub(crate) flow_container: Cell<Option<P<Node>>>,
    pub(crate) ref_key: Cell<CacheHashKey>,
    pub(crate) depth: Cell<u32>,
    pub(crate) shared_flow_start: Cell<usize>,
    pub(crate) reduce_labels: RefCell<Vec<P<ast::FlowReduceLabelData>>>,
    pub(crate) next: Cell<Option<P<FlowState>>>,
    // Flow memo (flowmemo.rs), per walk.
    /// The memo key; `memo_key_state` is 0 before it is computed, 1 if the reference has none, 2 if `memo_key` is it.
    pub(crate) memo_key: Cell<u128>,
    pub(crate) memo_key_state: Cell<u8>,
    /// Taint source of everything read from the reference node beyond its key.
    pub(crate) walk_floor: Cell<u32>,
    /// The walk returned a memo result instead of walking a sub-graph.
    pub(crate) memo_used: Cell<bool>,
    /// The walk reached the depth limit after using the memo and unwinds to walk again without it.
    pub(crate) memo_aborted: Cell<bool>,
    /// The walk is that second walk: it fills the memo but does not consult it.
    pub(crate) memo_faithful: Cell<bool>,
    /// `sharedFlows` entries of this walk whose value is transient.
    pub(crate) impure_shared: Cell<u32>,
    /// `reduce_labels.len()`, kept as a number for the memo's per-frame test.
    pub(crate) reduce_depth: Cell<u32>,
}

pub(crate) static TYPEOF_NE_FACTS: LazyLock<FxHashMap<&'static str, TypeFacts>> =
    LazyLock::new(|| {
        FxHashMap::from_iter([
            ("string", TypeFacts::TypeofNEString),
            ("number", TypeFacts::TypeofNENumber),
            ("bigint", TypeFacts::TypeofNEBigInt),
            ("boolean", TypeFacts::TypeofNEBoolean),
            ("symbol", TypeFacts::TypeofNESymbol),
            ("undefined", TypeFacts::NEUndefined),
            ("object", TypeFacts::TypeofNEObject),
            ("function", TypeFacts::TypeofNEFunction),
        ])
    });

pub(crate) const NON_DOTTED_NAME_CACHE_KEY: CacheHashKey = CacheHashKey::hash_string_128("?");
