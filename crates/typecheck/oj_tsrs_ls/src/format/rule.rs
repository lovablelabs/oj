use tsrs_ast::Kind;

use super::*;

// rule.go:5
pub(crate) struct RuleImpl {
    debug_name: &'static str,
    context: Vec<ContextPredicate>,
    action: RuleAction,
    flags: RuleFlags,
}

impl RuleImpl {
    // rule.go:12
    pub(crate) fn action(&self) -> RuleAction {
        self.action
    }

    // rule.go:16
    pub(crate) fn context(&self) -> &[ContextPredicate] {
        &self.context
    }

    // rule.go:20
    pub(crate) fn flags(&self) -> RuleFlags {
        self.flags
    }

    // rule.go:24
    #[expect(
        dead_code,
        reason = "Go's String() makes ruleImpl a fmt.Stringer for debug output; no Go code calls it"
    )]
    fn string(&self) -> &'static str {
        self.debug_name
    }
}

// rule.go:28
#[derive(Clone)]
pub struct TokenRange {
    pub(crate) tokens: Vec<Kind>,
    pub(crate) is_specific: bool,
}

// rule.go:33
pub(crate) struct RuleSpec {
    pub(crate) left_token_range: TokenRange,
    pub(crate) right_token_range: TokenRange,
    pub(crate) rule: &'static RuleImpl,
}

// Go's `rule` takes `left`/`right` as `any` (ast.Kind, []ast.Kind or tokenRange); toTokenRange dispatches on the type.
pub trait ToTokenRange {
    fn to_token_range(self) -> TokenRange;
}

impl ToTokenRange for Kind {
    fn to_token_range(self) -> TokenRange {
        TokenRange { is_specific: true, tokens: vec![self] }
    }
}

impl ToTokenRange for &[Kind] {
    fn to_token_range(self) -> TokenRange {
        TokenRange { is_specific: true, tokens: self.to_vec() }
    }
}

impl<const N: usize> ToTokenRange for [Kind; N] {
    fn to_token_range(self) -> TokenRange {
        TokenRange { is_specific: true, tokens: self.to_vec() }
    }
}

impl ToTokenRange for &Vec<Kind> {
    fn to_token_range(self) -> TokenRange {
        TokenRange { is_specific: true, tokens: self.clone() }
    }
}

impl ToTokenRange for &TokenRange {
    fn to_token_range(self) -> TokenRange {
        self.clone()
    }
}

// rule.go:51
/*
 * A rule takes a two tokens (left/right) and a particular context
 * for which you're meant to look at them. You then declare what should the
 * whitespace annotation be between these tokens via the action param.
 *
 * @param debugName Name to print
 * @param left The left side of the comparison
 * @param right The right side of the comparison
 * @param context A set of filters to narrow down the space in which this formatter rule applies
 * @param action a declaration of the expected whitespace
 * @param flags whether the rule deletes a line or not, defaults to no-op
 */
pub(crate) fn rule(
    debug_name: &'static str,
    left: impl ToTokenRange,
    right: impl ToTokenRange,
    context: Vec<ContextPredicate>,
    action: RuleAction,
    flags: &[RuleFlags],
) -> RuleSpec {
    let mut flag = RuleFlags::None;
    if !flags.is_empty() {
        flag = flags[0];
    }
    let left_range = to_token_range(left);
    let right_range = to_token_range(right);
    let rule: &'static RuleImpl =
        Box::leak(Box::new(RuleImpl { debug_name, context, action, flags: flag }));
    RuleSpec { left_token_range: left_range, right_token_range: right_range, rule }
}

// rule.go:71
fn to_token_range(e: impl ToTokenRange) -> TokenRange {
    e.to_token_range()
}

// rule.go:83
pub(crate) type ContextPredicate = &'static (dyn Fn(&FormattingContext) -> bool + Send + Sync);

// rule.go:85
pub(crate) fn any_context() -> Vec<ContextPredicate> {
    Vec::new()
}

// rule.go:87
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RuleAction(u32);

impl RuleAction {
    pub(crate) const NONE: RuleAction = RuleAction(0);
    pub(crate) const STOP_PROCESSING_SPACE_ACTIONS: RuleAction = RuleAction(1 << 0);
    pub(crate) const STOP_PROCESSING_TOKEN_ACTIONS: RuleAction = RuleAction(1 << 1);
    pub(crate) const INSERT_SPACE: RuleAction = RuleAction(1 << 2);
    pub(crate) const INSERT_NEW_LINE: RuleAction = RuleAction(1 << 3);
    pub(crate) const DELETE_SPACE: RuleAction = RuleAction(1 << 4);
    pub(crate) const DELETE_TOKEN: RuleAction = RuleAction(1 << 5);
    pub(crate) const INSERT_TRAILING_SEMICOLON: RuleAction = RuleAction(1 << 6);

    pub(crate) const STOP_ACTION: RuleAction =
        RuleAction(Self::STOP_PROCESSING_SPACE_ACTIONS.0 | Self::STOP_PROCESSING_TOKEN_ACTIONS.0);
    pub(crate) const MODIFY_SPACE_ACTION: RuleAction =
        RuleAction(Self::INSERT_SPACE.0 | Self::INSERT_NEW_LINE.0 | Self::DELETE_SPACE.0);
    pub(crate) const MODIFY_TOKEN_ACTION: RuleAction =
        RuleAction(Self::DELETE_TOKEN.0 | Self::INSERT_TRAILING_SEMICOLON.0);
}

impl std::ops::BitAnd for RuleAction {
    type Output = RuleAction;
    fn bitand(self, rhs: RuleAction) -> RuleAction {
        RuleAction(self.0 & rhs.0)
    }
}

impl std::ops::BitOr for RuleAction {
    type Output = RuleAction;
    fn bitor(self, rhs: RuleAction) -> RuleAction {
        RuleAction(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for RuleAction {
    fn bitor_assign(&mut self, rhs: RuleAction) {
        self.0 |= rhs.0;
    }
}

impl std::ops::Not for RuleAction {
    type Output = RuleAction;
    fn not(self) -> RuleAction {
        RuleAction(!self.0)
    }
}

// rule.go:104
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RuleFlags {
    None,
    CanDeleteNewLines,
}
