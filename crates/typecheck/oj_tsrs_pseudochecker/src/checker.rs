use tsrs_core::P;

// TODO: Late binding/symbol merging?
// In strada, `expressionToTypeNode` used many `resolver` methods whose net effect was just
// calling `Checker.GetMergedSymbol` on a symbol when dealing with accessors. Right now those
// just use Node.Symbol, which will fail to pair up late-bound symbols. In theory, this is actually
// fine, since ID can't possibly know if `set [q1()](a){}` and `get [q2()](): T {}` are connected
// without performing real type checking, regardless, so it shouldn't matter. If anything, it might be
// OK to add a "dumb" late binder that can merge multiple `[a.b.c]: T` together, but not anything else.
// This is an area of active ~~feature-creep~~ development in ID output, prerequisite refactoring would include
// extracting the `mergeSymbol` core checker logic into a reusable component.

/// Go `PseudoChecker`, handled as `P<PseudoChecker>` (immutable; methods take `&self`).
pub struct PseudoChecker {
    pub(crate) strict_null_checks: bool,
}

pub fn new_pseudo_checker(strict_null_checks: bool) -> P<PseudoChecker> {
    P::new(PseudoChecker { strict_null_checks })
}
