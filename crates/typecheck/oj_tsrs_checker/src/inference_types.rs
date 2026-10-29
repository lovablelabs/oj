//! Non-function declarations of `inference.go`.

use crate::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct InferenceKey {
    pub(crate) source: TypeId,
    pub(crate) target: TypeId,
    pub(crate) priority: InferencePriority,
    pub(crate) contravariant: bool,
    pub(crate) bivariant: bool,
}

impl std::hash::Hash for InferenceKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // Preserve every key field in two words, avoiding a separate hash mix for each small field.
        let types = u64::from(self.source.0) | (u64::from(self.target.0) << 32);
        let inference = u64::from(self.priority.memo_bits())
            | (u64::from(self.contravariant) << 32)
            | (u64::from(self.bivariant) << 33);
        state.write_u64(types);
        state.write_u64(inference);
    }
}

/// Arena handle (`P<InferenceState>`), pooled through `Checker::freeinference_state` like Go.
#[derive(Default)]
pub struct InferenceState {
    pub(crate) inferences: RefCell<Vec<P<InferenceInfo>>>,
    pub(crate) original_source: Cell<Option<P<Type>>>,
    pub(crate) original_target: Cell<Option<P<Type>>>,
    pub(crate) priority: Cell<InferencePriority>,
    pub(crate) inference_priority: Cell<InferencePriority>,
    pub(crate) contravariant: Cell<bool>,
    pub(crate) bivariant: Cell<bool>,
    pub(crate) expanding_flags: Cell<ExpandingFlags>,
    pub(crate) propagation_type: Cell<Option<P<Type>>>,
    // Scratch owned by this checker's inference-state pool. Forks start with an empty pool, so a
    // frozen seed's unused states are never reused and this table needs no shared-graph overlay.
    pub(crate) visited: RefCell<FxHashMap<InferenceKey, InferencePriority>>,
    pub(crate) source_stack: RefCell<Vec<P<Type>>>,
    pub(crate) target_stack: RefCell<Vec<P<Type>>>,
    pub(crate) next: Cell<Option<P<InferenceState>>>,
    /// The walk called `clear_cached_inferences` (the inference memo, infermemo.rs).
    pub(crate) cleared_inferences: Cell<bool>,
}
