use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::{self as ast, Kind, Node};
use tsrs_checker::{LiteralValue, Type, TypeFlags};
use tsrs_core::{P, json::Value};

use crate::protocol::{Fix, Range, RuleMessage, Suggestion};
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};

fn message(suggestion: bool) -> RuleMessage {
    RuleMessage {
        id: if suggestion { "optionalChainSuggest" } else { "preferOptionalChain" }.into(),
        description: if suggestion { "Change to an optional chain." } else { "Prefer using an optional chain expression instead, as it's more concise and easier to read." }.into(),
        ..Default::default()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum OperandType {
    #[default]
    Invalid,
    Plain,
    NotStrictEqualNull,
    NotStrictEqualUndef,
    NotEqualBoth,
    Not,
    NegatedAndOperand,
    TypeofCheck,
    Comparison,
    EqualNull,
    StrictEqualNull,
    StrictEqualUndef,
}
use OperandType::{
    Comparison, EqualNull, Invalid, NegatedAndOperand, Not, NotEqualBoth, NotStrictEqualNull,
    NotStrictEqualUndef, Plain, StrictEqualNull, StrictEqualUndef, TypeofCheck,
};
impl OperandType {
    fn is_nullish_check(self) -> bool {
        matches!(
            self,
            NotStrictEqualNull
                | NotStrictEqualUndef
                | NotEqualBoth
                | StrictEqualNull
                | EqualNull
                | StrictEqualUndef
                | TypeofCheck
        )
    }
    fn is_strict_nullish_check(self) -> bool {
        matches!(self, NotStrictEqualNull | NotStrictEqualUndef)
    }
    fn is_loose_nullish_check(self) -> bool {
        matches!(self, NotEqualBoth | EqualNull)
    }
    fn is_trailing_comparison(self) -> bool {
        matches!(self, NotStrictEqualNull | NotStrictEqualUndef | NotEqualBoth | Comparison)
    }
    fn is_comparison_or_null_check(self) -> bool {
        self == Comparison || self.is_nullish_check()
    }
}
#[derive(Clone, Copy)]
struct Operand {
    typ: OperandType,
    node: P<Node>,
    compared_expr: Option<P<Node>>,
}
impl Operand {
    fn new(typ: OperandType, node: P<Node>, compared_expr: P<Node>) -> Self {
        Self { typ, node, compared_expr: Some(compared_expr) }
    }
    fn invalid(node: P<Node>) -> Self {
        Self { typ: Invalid, node, compared_expr: None }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum NodeComparisonResult {
    Equal,
    Subset,
    Superset,
    Invalid,
}
#[derive(Clone, Copy)]
struct BinaryParts {
    operator: Kind,
    left: P<Node>,
    right: P<Node>,
}
fn unwrap_binary(node: P<Node>) -> Option<BinaryParts> {
    let node = ast::skip_parentheses(node);
    if node.kind() != Kind::BinaryExpression {
        return None;
    }
    let bin = node.as_binary_expression();
    Some(BinaryParts {
        operator: bin.operator_token.kind(),
        left: ast::skip_parentheses(bin.left),
        right: ast::skip_parentheses(bin.right()),
    })
}
fn is_null(node: P<Node>) -> bool {
    node.kind() == Kind::NullKeyword
}
fn is_undefined_identifier(node: P<Node>) -> bool {
    node.kind() == Kind::Identifier && node.text() == "undefined"
}
fn is_undefined(node: P<Node>) -> bool {
    is_undefined_identifier(node) || node.kind() == Kind::VoidExpression
}
fn is_nullish(node: P<Node>) -> bool {
    is_null(node) || is_undefined(node)
}
fn is_access(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::PropertyAccessExpression | Kind::ElementAccessExpression | Kind::CallExpression
    )
}
fn is_and(operator: Kind) -> bool {
    operator == Kind::AmpersandAmpersandToken
}
fn is_comparison_against(op: Operand, predicate: fn(P<Node>) -> bool) -> bool {
    op.typ == Comparison
        && unwrap_binary(op.node).is_some_and(|bin| {
            matches!(bin.operator, Kind::EqualsEqualsToken | Kind::EqualsEqualsEqualsToken)
                && (predicate(bin.left) || predicate(bin.right))
        })
}
fn is_nullish_comparison(op: Operand) -> bool {
    is_comparison_against(op, is_nullish)
}
fn is_strict_null_check_for_or_chain(op: Operand) -> bool {
    matches!(op.typ, StrictEqualNull | NotStrictEqualNull)
        || op.typ == Comparison
            && unwrap_binary(op.node).is_some_and(|bin| {
                bin.operator == Kind::EqualsEqualsEqualsToken
                    && (is_null(bin.left) || is_null(bin.right))
            })
}
fn is_strict_undefined_check_for_or_chain(op: Operand) -> bool {
    matches!(op.typ, StrictEqualUndef | NotStrictEqualUndef | TypeofCheck)
        || op.typ == Comparison
            && unwrap_binary(op.node).is_some_and(|bin| {
                bin.operator == Kind::EqualsEqualsEqualsToken
                    && (is_undefined(bin.left) || is_undefined(bin.right))
            })
}
fn is_or_chain_nullish_check(op: Operand) -> bool {
    matches!(op.typ, StrictEqualNull | StrictEqualUndef | EqualNull) || is_nullish_comparison(op)
}
fn is_nullish_check_operand(op: Operand) -> bool {
    matches!(
        op.typ,
        NotStrictEqualNull
            | NotStrictEqualUndef
            | NotEqualBoth
            | StrictEqualNull
            | StrictEqualUndef
            | EqualNull
    ) || op.typ == Comparison
        && unwrap_binary(op.node).is_some_and(|bin| {
            is_null(bin.left)
                || is_null(bin.right)
                || is_undefined_identifier(bin.left)
                || is_undefined_identifier(bin.right)
        })
}
fn is_non_nullish_trailing_comparison(op: Operand) -> bool {
    op.typ == Comparison && !is_nullish_comparison(op)
}
#[derive(Default)]
struct NullishCheckAnalysis {
    has_null_check: bool,
    has_undefined_check: bool,
    has_both_check: bool,
}
impl NullishCheckAnalysis {
    fn has_only_null_check(&self) -> bool {
        self.has_null_check && !self.has_undefined_check && !self.has_both_check
    }
    fn has_only_undefined_check(&self) -> bool {
        !self.has_null_check && self.has_undefined_check && !self.has_both_check
    }
    fn has_incomplete_check(&self) -> bool {
        self.has_only_null_check() || self.has_only_undefined_check()
    }
}
fn analyze_nullish_checks(chain: &[Operand], or_chain_mode: bool) -> NullishCheckAnalysis {
    let mut a = NullishCheckAnalysis::default();
    for op in chain {
        match op.typ {
            NotStrictEqualNull | StrictEqualNull => a.has_null_check = true,
            NotStrictEqualUndef | StrictEqualUndef | TypeofCheck => a.has_undefined_check = true,
            NotEqualBoth | EqualNull => a.has_both_check = true,
            Not if or_chain_mode => a.has_both_check = true,
            Plain if !or_chain_mode => a.has_both_check = true,
            Comparison => {
                if let Some(bin) = unwrap_binary(op.node) {
                    let null = is_null(bin.left) || is_null(bin.right);
                    let undef = is_undefined(bin.left) || is_undefined(bin.right);
                    if bin.operator == Kind::EqualsEqualsToken && (null || undef) {
                        a.has_both_check = true;
                    } else if bin.operator == Kind::EqualsEqualsEqualsToken {
                        a.has_null_check |= null;
                        a.has_undefined_check |= undef;
                    }
                }
            }
            _ => {}
        }
    }
    a
}
fn unwrap_for_comparison(mut n: P<Node>) -> P<Node> {
    while matches!(n.kind(), Kind::ParenthesizedExpression | Kind::NonNullExpression) {
        n = n.expression().unwrap();
    }
    n
}
fn equal_node_lists(a: &[P<Node>], b: &[P<Node>]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(&a, &b)| are_nodes_structurally_equal(a, b))
}
fn are_nodes_structurally_equal(a: P<Node>, b: P<Node>) -> bool {
    let a = unwrap_for_comparison(a);
    let b = unwrap_for_comparison(b);
    if a.kind() != b.kind() {
        return false;
    }
    match a.kind() {
        Kind::Identifier
        | Kind::StringLiteral
        | Kind::NumericLiteral
        | Kind::BigIntLiteral
        | Kind::NoSubstitutionTemplateLiteral => a.text() == b.text(),
        Kind::ThisKeyword
        | Kind::NullKeyword
        | Kind::SuperKeyword
        | Kind::TrueKeyword
        | Kind::FalseKeyword
        | Kind::StringKeyword
        | Kind::NumberKeyword
        | Kind::BooleanKeyword
        | Kind::AnyKeyword
        | Kind::UnknownKeyword
        | Kind::NeverKeyword
        | Kind::VoidKeyword
        | Kind::UndefinedKeyword
        | Kind::ObjectKeyword
        | Kind::SymbolKeyword
        | Kind::BigIntKeyword => true,
        Kind::PropertyAccessExpression => {
            let an = a.name();
            let bn = b.name();
            if an.is_none() || bn.is_none() {
                return an == bn;
            }
            an.unwrap().text() == bn.unwrap().text()
                && are_nodes_structurally_equal(a.expression().unwrap(), b.expression().unwrap())
        }
        Kind::ElementAccessExpression => {
            are_nodes_structurally_equal(
                a.as_element_access_expression().argument_expression,
                b.as_element_access_expression().argument_expression,
            ) && are_nodes_structurally_equal(a.expression().unwrap(), b.expression().unwrap())
        }
        Kind::CallExpression => {
            let ac = a.as_call_expression();
            let bc = b.as_call_expression();
            are_nodes_structurally_equal(ac.expression, bc.expression)
                && equal_node_lists(a.arguments(), b.arguments())
                && (ac.type_arguments().is_some() == bc.type_arguments().is_some())
                && equal_node_lists(
                    ac.type_arguments().map_or(&[], |n| n.nodes()),
                    bc.type_arguments().map_or(&[], |n| n.nodes()),
                )
        }
        Kind::AsExpression
        | Kind::TypeAssertionExpression
        | Kind::TypeOfExpression
        | Kind::AwaitExpression => {
            are_nodes_structurally_equal(a.expression().unwrap(), b.expression().unwrap())
        }
        Kind::TypeReference => are_nodes_structurally_equal(
            a.as_type_reference_node().type_name,
            b.as_type_reference_node().type_name,
        ),
        Kind::QualifiedName => {
            let a = a.as_qualified_name();
            let b = b.as_qualified_name();
            are_nodes_structurally_equal(a.left, b.left)
                && are_nodes_structurally_equal(a.right, b.right)
        }
        Kind::MetaProperty => {
            let an = a.name();
            let bn = b.name();
            if an.is_none() || bn.is_none() {
                return an == bn;
            }
            a.as_meta_property().keyword_token == b.as_meta_property().keyword_token
                && an.unwrap().text() == bn.unwrap().text()
        }
        Kind::BinaryExpression => {
            let a = a.as_binary_expression();
            let b = b.as_binary_expression();
            a.operator_token.kind() == b.operator_token.kind()
                && are_nodes_structurally_equal(a.left, b.left)
                && are_nodes_structurally_equal(a.right(), b.right())
        }
        Kind::PrefixUnaryExpression => {
            let a = a.as_prefix_unary_expression();
            let b = b.as_prefix_unary_expression();
            a.operator == b.operator && are_nodes_structurally_equal(a.operand, b.operand)
        }
        Kind::TemplateExpression => {
            let a = a.as_template_expression();
            let b = b.as_template_expression();
            a.head.text() == b.head.text()
                && a.template_spans.nodes().len() == b.template_spans.nodes().len()
                && a.template_spans.nodes().iter().zip(b.template_spans.nodes()).all(|(a, b)| {
                    let a = a.as_template_span();
                    let b = b.as_template_span();
                    are_nodes_structurally_equal(a.expression, b.expression)
                        && a.literal.text() == b.literal.text()
                })
        }
        _ => false,
    }
}
fn is_node_prefix_of(shorter: P<Node>, longer: P<Node>) -> bool {
    let shorter = unwrap_for_comparison(shorter);
    let longer = unwrap_for_comparison(longer);
    if are_nodes_structurally_equal(shorter, longer) {
        return true;
    }
    if matches!(
        longer.kind(),
        Kind::PropertyAccessExpression
            | Kind::ElementAccessExpression
            | Kind::CallExpression
            | Kind::NonNullExpression
    ) {
        return is_node_prefix_of(shorter, longer.expression().unwrap());
    }
    false
}
fn is_inside_jsx(mut node: Option<P<Node>>) -> bool {
    while let Some(n) = node {
        if ast::is_function_like_declaration(n) {
            return false;
        }
        if matches!(
            n.kind(),
            Kind::JsxExpression
                | Kind::JsxAttribute
                | Kind::JsxAttributes
                | Kind::JsxElement
                | Kind::JsxSelfClosingElement
                | Kind::JsxOpeningElement
                | Kind::JsxClosingElement
                | Kind::JsxFragment
        ) {
            return true;
        }
        node = n.parent();
    }
    false
}
fn get_base_identifier(mut node: P<Node>) -> P<Node> {
    while matches!(
        node.kind(),
        Kind::PropertyAccessExpression
            | Kind::ElementAccessExpression
            | Kind::CallExpression
            | Kind::NonNullExpression
            | Kind::ParenthesizedExpression
            | Kind::AsExpression
    ) {
        node = node.expression().unwrap();
    }
    node
}
fn has_side_effects(node: P<Node>) -> bool {
    match node.kind() {
        Kind::PrefixUnaryExpression
            if matches!(
                node.as_prefix_unary_expression().operator,
                Kind::PlusPlusToken | Kind::MinusMinusToken
            ) =>
        {
            true
        }
        Kind::PostfixUnaryExpression | Kind::YieldExpression => true,
        Kind::BinaryExpression => matches!(
            node.as_binary_expression().operator_token.kind(),
            Kind::EqualsToken
                | Kind::PlusEqualsToken
                | Kind::MinusEqualsToken
                | Kind::AsteriskEqualsToken
                | Kind::SlashEqualsToken
        ),
        Kind::PropertyAccessExpression | Kind::CallExpression | Kind::ParenthesizedExpression => {
            has_side_effects(node.expression().unwrap())
        }
        Kind::ElementAccessExpression => {
            has_side_effects(node.expression().unwrap())
                || has_side_effects(node.as_element_access_expression().argument_expression)
        }
        _ => false,
    }
}
#[derive(Clone, Default)]
struct ChainPart {
    text: Vec<u8>,
    optional: bool,
    requires_dot: bool,
    is_private: bool,
    has_non_null: bool,
    is_call: bool,
}
impl ChainPart {
    fn base_text(&self) -> &[u8] {
        if self.has_non_null {
            self.text.strip_suffix(b"!").unwrap_or(&self.text)
        } else {
            &self.text
        }
    }
}
#[derive(Clone, Default)]
struct TypeInfo {
    parts: Vec<P<Type>>,
    has_null: bool,
    has_undefined: bool,
    has_void: bool,
    has_any: bool,
    has_unknown: bool,
    has_bool_literal: bool,
    has_num_literal: bool,
    has_str_literal: bool,
    has_big_int_literal: bool,
    has_big_int_like: bool,
    has_boolean_like: bool,
    has_number_like: bool,
    has_string_like: bool,
}
impl TypeInfo {
    fn is_nullish(&self) -> bool {
        self.has_null || self.has_undefined || self.has_any || self.has_unknown
    }
    fn has_explicit_nullish(&self) -> bool {
        self.has_null || self.has_undefined
    }
    fn is_any_or_unknown(&self) -> bool {
        self.has_any || self.has_unknown
    }
    fn has_both_null_and_undefined(&self) -> bool {
        self.has_null && self.has_undefined
    }
    fn has_no_nullable_types(&self) -> bool {
        !self.is_nullish()
    }
    fn can_be_undefined_like(&self) -> bool {
        self.parts.is_empty()
            || self.has_undefined
            || self.has_void
            || self.has_any
            || self.has_unknown
    }
    fn can_be_nullish_like(&self) -> bool {
        self.has_null || self.can_be_undefined_like()
    }
    fn is_always_undefined_like(&self) -> bool {
        !self.parts.is_empty()
            && !self.is_any_or_unknown()
            && self
                .parts
                .iter()
                .all(|p| p.flags().intersects(TypeFlags::Undefined | TypeFlags::Void))
    }
    fn is_always_nullish_like(&self) -> bool {
        !self.parts.is_empty()
            && !self.is_any_or_unknown()
            && self.parts.iter().all(|p| {
                p.flags().intersects(TypeFlags::Null | TypeFlags::Undefined | TypeFlags::Void)
            })
    }
}
struct Options {
    unsafe_fixes: bool,
    check_any: bool,
    check_big_int: bool,
    check_boolean: bool,
    check_number: bool,
    check_string: bool,
    check_unknown: bool,
    require_nullish: bool,
}
#[derive(Default)]
struct State {
    seen_ranges: FxHashSet<(u32, u32)>,
    type_cache: FxHashMap<P<Node>, TypeInfo>,
    flatten_cache: FxHashMap<P<Node>, Vec<ChainPart>>,
    call_sig_cache: FxHashMap<P<Node>, Vec<(String, String)>>,
    optional_chain_cache: FxHashMap<P<Node>, bool>,
}
pub struct PreferOptionalChain {
    opts: Options,
    state: State,
}
impl PreferOptionalChain {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(o) => Some(o),
            _ => return Err("prefer-optional-chain: options must be an object".into()),
        };
        let boolean = |name: &str, default| match object.and_then(|o| o.get(name)) {
            None | Some(Value::Null) => Ok(default),
            Some(Value::Bool(v)) => Ok(*v),
            _ => Err(format!("prefer-optional-chain: {name} must be a boolean")),
        };
        Ok(Self {
            opts: Options {
                unsafe_fixes: boolean(
                    "allowPotentiallyUnsafeFixesThatModifyTheReturnTypeIKnowWhatImDoing",
                    false,
                )?,
                check_any: boolean("checkAny", true)?,
                check_big_int: boolean("checkBigInt", true)?,
                check_boolean: boolean("checkBoolean", true)?,
                check_number: boolean("checkNumber", true)?,
                check_string: boolean("checkString", true)?,
                check_unknown: boolean("checkUnknown", true)?,
                require_nullish: boolean("requireNullish", false)?,
            },
            state: State::default(),
        })
    }
}
impl Rule for PreferOptionalChain {
    fn listens_to(&self, listener: Listener) -> bool {
        listener == Listener::Enter(Kind::BinaryExpression)
    }
    fn visit(&mut self, _: Listener, node: P<Node>, ctx: &mut RuleContext<'_>) {
        let operator = node.as_binary_expression().operator_token.kind();
        if matches!(
            operator,
            Kind::AmpersandAmpersandToken | Kind::BarBarToken | Kind::QuestionQuestionToken
        ) {
            ChainProcessor { ctx, opts: &self.opts, state: &mut self.state }
                .process_chain(node, operator);
        }
    }
}
struct ChainProcessor<'a, 'b> {
    ctx: &'a mut RuleContext<'b>,
    opts: &'a Options,
    state: &'a mut State,
}
impl ChainProcessor<'_, '_> {
    fn get_node_range(&self, node: P<Node>) -> Range {
        trim_node_range(self.ctx.source_file, node)
    }
    fn source(&self, start: u32, end: u32) -> &str {
        &self.ctx.source_file.text()[start as usize..end as usize]
    }
    fn get_node_text(&self, node: P<Node>) -> &str {
        let r = self.get_node_range(node);
        self.source(r.pos, r.end)
    }
    fn get_type_info(&mut self, node: P<Node>) -> TypeInfo {
        if let Some(info) = self.state.type_cache.get(&node) {
            return info.clone();
        }
        let ty = self.ctx.checker.get_type_at_location(node);
        let mut info =
            TypeInfo { parts: crate::utils::union_type_parts(&ty).to_vec(), ..Default::default() };
        for part in &info.parts {
            let f = part.flags();
            info.has_null |= f.intersects(TypeFlags::Null);
            info.has_undefined |= f.intersects(TypeFlags::Undefined);
            info.has_void |= f.intersects(TypeFlags::Void);
            info.has_any |= f.intersects(TypeFlags::Any);
            info.has_unknown |= f.intersects(TypeFlags::Unknown);
            info.has_bool_literal |= f.intersects(TypeFlags::BooleanLiteral);
            info.has_num_literal |= f.intersects(TypeFlags::NumberLiteral);
            info.has_str_literal |= f.intersects(TypeFlags::StringLiteral);
            info.has_big_int_literal |= f.intersects(TypeFlags::BigIntLiteral);
            info.has_big_int_like |= f.intersects(TypeFlags::BigIntLike);
            info.has_boolean_like |= f.intersects(TypeFlags::BooleanLike);
            info.has_number_like |= f.intersects(TypeFlags::NumberLike);
            info.has_string_like |= f.intersects(TypeFlags::StringLike);
        }
        self.state.type_cache.insert(node, info.clone());
        info
    }
    fn extract_call_signatures(&mut self, node: P<Node>) -> Vec<(String, String)> {
        fn visit(p: &ChainProcessor<'_, '_>, n: P<Node>, out: &mut Vec<(String, String)>) {
            match n.kind() {
                Kind::CallExpression => {
                    let expr = n.expression().unwrap();
                    let key = p.get_node_text(expr).to_owned();
                    let value = p.get_node_text(n).to_owned();
                    if let Some((_, v)) = out.iter_mut().find(|(k, _)| *k == key) {
                        *v = value;
                    } else {
                        out.push((key, value));
                    }
                    visit(p, expr, out);
                }
                Kind::PropertyAccessExpression
                | Kind::ElementAccessExpression
                | Kind::NonNullExpression => visit(p, n.expression().unwrap(), out),
                _ => {}
            }
        }
        if let Some(s) = self.state.call_sig_cache.get(&node) {
            return s.clone();
        }
        let mut sigs = Vec::new();
        visit(self, node, &mut sigs);
        self.state.call_sig_cache.insert(node, sigs.clone());
        sigs
    }
    fn report_chain_with_fixes(
        &mut self,
        node: P<Node>,
        fixes: Vec<Fix>,
        use_suggestion: bool,
        valid_utf8: bool,
    ) {
        if !valid_utf8
            && if use_suggestion { self.ctx.options.fix_suggestions } else { self.ctx.options.fix }
        {
            // Pinned headless output drops records when requested fix text contains invalid UTF-8.
            return;
        }
        if use_suggestion {
            self.ctx.report_node_with_suggestions(node, message(false), || {
                vec![Suggestion { message: message(true), fixes }]
            });
        } else {
            self.ctx.report_node_with_fixes(node, message(false), || fixes);
        }
    }
    fn is_invalid_or_chain_starting_operand(op: Operand) -> bool {
        if op.typ != Comparison {
            return false;
        }
        let Some(bin) = unwrap_binary(op.node) else {
            return false;
        };
        let left = is_nullish(bin.left);
        let right = is_nullish(bin.right);
        if bin.operator == Kind::EqualsEqualsEqualsToken && !left && !right {
            return true;
        }
        if !matches!(
            bin.operator,
            Kind::ExclamationEqualsToken | Kind::ExclamationEqualsEqualsToken
        ) || !left && !right
        {
            return false;
        }
        matches!(
            if right { bin.left } else { bin.right }.kind(),
            Kind::Identifier | Kind::ThisKeyword
        )
    }
    fn should_allow_call_chain_extension(
        &self,
        prev: Operand,
        current: Operand,
        operator: Kind,
    ) -> bool {
        self.opts.unsafe_fixes
            || if is_and(operator) {
                prev.typ == Plain && current.typ == Plain
            } else {
                prev.typ == Not && current.typ == Not
                    || is_or_chain_nullish_check(prev) && is_or_chain_nullish_check(current)
            }
    }
    fn try_extend_through_call_expression(
        last: P<Node>,
        current: Operand,
        first: Option<P<Node>>,
        operator: Kind,
    ) -> NodeComparisonResult {
        if !matches!(ast::skip_parentheses(last).kind(), Kind::CallExpression | Kind::NewExpression)
        {
            return NodeComparisonResult::Invalid;
        }
        if is_and(operator)
            && let Some(mut base) = first
        {
            loop {
                let n = ast::skip_parentheses(base);
                if is_access(n) {
                    base = n.expression().unwrap();
                } else {
                    break;
                }
            }
            if ast::skip_parentheses(base).kind() == Kind::NewExpression {
                return NodeComparisonResult::Invalid;
            }
        }
        if current.compared_expr.is_some_and(|n| is_node_prefix_of(last, n)) {
            NodeComparisonResult::Subset
        } else {
            NodeComparisonResult::Invalid
        }
    }
    fn compare_nodes(&mut self, left: P<Node>, right: P<Node>) -> NodeComparisonResult {
        use NodeComparisonResult as C;
        if has_side_effects(left) || has_side_effects(right) {
            return C::Invalid;
        }
        let unwrapped = ast::skip_parentheses(left);
        if !self.contains_optional_chain(left) {
            let mut root = unwrapped;
            loop {
                let n = ast::skip_parentheses(root);
                if is_access(n) {
                    root = n.expression().unwrap();
                } else {
                    break;
                }
            }
            let rooted_in_new = ast::skip_parentheses(root).kind() == Kind::NewExpression;
            let standalone = unwrapped.kind() == Kind::NewExpression
                || unwrapped.kind() == Kind::CallExpression
                    && ast::skip_parentheses(unwrapped.expression().unwrap()).kind()
                        == Kind::Identifier;
            if standalone
                || rooted_in_new
                || matches!(
                    unwrapped.kind(),
                    Kind::ArrayLiteralExpression
                        | Kind::ObjectLiteralExpression
                        | Kind::FunctionExpression
                        | Kind::ArrowFunction
                        | Kind::ClassExpression
                        | Kind::JsxElement
                        | Kind::JsxSelfClosingElement
                        | Kind::JsxFragment
                        | Kind::TemplateExpression
                        | Kind::AwaitExpression
                )
            {
                return C::Invalid;
            }
        }
        let left_sigs = self.extract_call_signatures(left);
        let right_sigs = self.extract_call_signatures(right);
        for (key, value) in &left_sigs {
            if right_sigs.iter().any(|(k, v)| k == key && v != value) {
                return C::Invalid;
            }
        }
        if are_nodes_structurally_equal(left, right) {
            C::Equal
        } else if is_node_prefix_of(left, right) {
            C::Subset
        } else if is_node_prefix_of(right, left) {
            C::Superset
        } else {
            C::Invalid
        }
    }
    fn includes_nullish(&mut self, node: P<Node>) -> bool {
        self.get_type_info(node).is_nullish()
    }
    fn includes_explicit_nullish(&mut self, node: P<Node>) -> bool {
        self.get_type_info(node).has_explicit_nullish()
    }
    fn type_is_any_or_unknown(&mut self, node: P<Node>) -> bool {
        let i = self.get_type_info(node);
        !i.parts.is_empty()
            && i.parts.iter().all(|p| p.flags().intersects(TypeFlags::Any | TypeFlags::Unknown))
    }
    fn would_change_return_type(&mut self, node: P<Node>) -> bool {
        let i = self.get_type_info(node);
        (i.has_bool_literal || i.has_num_literal || i.has_str_literal || i.has_big_int_literal)
            && !i.has_explicit_nullish()
    }
    fn has_falsy_literal_type(&mut self, node: P<Node>) -> bool {
        self.get_type_info(node).parts.iter().any(|union_part| {
            crate::utils::intersection_type_parts(union_part).iter().any(|part| {
                if !part.flags().intersects(
                    TypeFlags::BooleanLiteral
                        | TypeFlags::StringLiteral
                        | TypeFlags::NumberLiteral
                        | TypeFlags::BigIntLiteral,
                ) {
                    return false;
                }
                match part.as_literal_type().value() {
                    Some(LiteralValue::Boolean(value)) => !value,
                    Some(LiteralValue::String(value)) => value.is_empty(),
                    Some(LiteralValue::Number(value)) => value.to_string() == "0",
                    Some(LiteralValue::BigInt(value)) => value.string() == "0",
                    None => false,
                }
            })
        })
    }
    fn is_or_chain_comparison_safe(op: Operand) -> bool {
        if op.typ != Comparison {
            return true;
        }
        let Some(bin) = unwrap_binary(op.node) else {
            return true;
        };
        let value = if is_access(bin.left) {
            bin.right
        } else if is_access(bin.right) {
            bin.left
        } else {
            return true;
        };
        let null = is_null(value);
        let undef = is_undefined(value);
        let literal = matches!(
            value.kind(),
            Kind::NumericLiteral
                | Kind::StringLiteral
                | Kind::TrueKeyword
                | Kind::FalseKeyword
                | Kind::ObjectLiteralExpression
                | Kind::ArrayLiteralExpression
        );
        match bin.operator {
            Kind::ExclamationEqualsEqualsToken => literal || null,
            Kind::EqualsEqualsEqualsToken => undef,
            Kind::ExclamationEqualsToken => {
                if null || undef {
                    return false;
                }
                if value.kind() == Kind::Identifier && !literal {
                    return false;
                }
                literal
            }
            Kind::EqualsEqualsToken => null || undef,
            Kind::LessThanToken
            | Kind::GreaterThanToken
            | Kind::LessThanEqualsToken
            | Kind::GreaterThanEqualsToken => false,
            _ => true,
        }
    }
    fn should_skip_by_type(&mut self, node: P<Node>) -> bool {
        let i = self.get_type_info(get_base_identifier(node));
        if self.opts.require_nullish && i.has_explicit_nullish() {
            return false;
        }
        i.has_any && !self.opts.check_any
            || i.has_big_int_like && !self.opts.check_big_int
            || i.has_boolean_like && !self.opts.check_boolean
            || i.has_number_like && !self.opts.check_number
            || i.has_string_like && !self.opts.check_string
            || i.has_unknown && !self.opts.check_unknown
    }
    fn flatten_for_fix(&mut self, node: P<Node>) -> Vec<ChainPart> {
        fn visit(
            p: &ChainProcessor<'_, '_>,
            n: P<Node>,
            parent_non_null: bool,
            parts: &mut Vec<ChainPart>,
        ) {
            match n.kind() {
                Kind::ParenthesizedExpression => {
                    let inner = n.expression().unwrap();
                    if matches!(inner.kind(), Kind::AwaitExpression | Kind::YieldExpression) {
                        parts.push(ChainPart {
                            text: p.get_node_text(n).as_bytes().to_vec(),
                            ..Default::default()
                        });
                    } else {
                        visit(p, inner, parent_non_null, parts);
                    }
                }
                Kind::NonNullExpression => visit(p, n.expression().unwrap(), true, parts),
                Kind::PropertyAccessExpression => {
                    let prop = n.as_property_access_expression();
                    visit(p, prop.expression, false, parts);
                    let name = n.name().unwrap();
                    let mut text = p.get_node_text(name).as_bytes().to_vec();
                    if parent_non_null {
                        text.push(b'!');
                    }
                    parts.push(ChainPart {
                        text,
                        optional: prop.question_dot_token().is_some(),
                        requires_dot: true,
                        is_private: name.kind() == Kind::PrivateIdentifier,
                        has_non_null: parent_non_null,
                        ..Default::default()
                    });
                }
                Kind::ElementAccessExpression => {
                    let elem = n.as_element_access_expression();
                    visit(p, elem.expression, false, parts);
                    let text = format!(
                        "[{}]{}",
                        p.get_node_text(elem.argument_expression),
                        if parent_non_null { "!" } else { "" }
                    );
                    parts.push(ChainPart {
                        text: text.into_bytes(),
                        optional: elem.question_dot_token().is_some(),
                        has_non_null: parent_non_null,
                        ..Default::default()
                    });
                }
                Kind::CallExpression => {
                    let call = n.as_call_expression();
                    visit(p, call.expression, false, parts);
                    let mut text = Vec::new();
                    if let Some(args) = call.type_arguments()
                        && !args.nodes().is_empty()
                    {
                        text.push(b'<');
                        text.extend_from_slice(
                            p.source(
                                args.pos().expect_source("parsed type arguments"),
                                args.end().expect_source("parsed type arguments"),
                            )
                            .as_bytes(),
                        );
                        text.push(b'>');
                    }
                    text.push(b'(');
                    if let args = call.arguments
                        && !args.nodes().is_empty()
                    {
                        text.extend_from_slice(
                            &p.ctx.source_file.text().as_bytes()[args
                                .pos()
                                .expect_source("parsed call arguments")
                                as usize
                                ..n.end().expect_source("parsed call") as usize - 1],
                        );
                    }
                    text.push(b')');
                    parts.push(ChainPart {
                        text,
                        optional: call.question_dot_token().is_some(),
                        is_call: true,
                        ..Default::default()
                    });
                }
                _ => {
                    let mut text = p.get_node_text(n).to_owned();
                    if parent_non_null && n.kind() == Kind::Identifier {
                        text.push('!');
                    }
                    if matches!(n.kind(), Kind::AsExpression | Kind::TypeAssertionExpression) {
                        text = format!("({text})");
                    }
                    parts.push(ChainPart {
                        text: text.into_bytes(),
                        has_non_null: parent_non_null,
                        ..Default::default()
                    });
                }
            }
        }
        if let Some(parts) = self.state.flatten_cache.get(&node) {
            return parts.clone();
        }
        let mut parts = Vec::new();
        visit(self, node, false, &mut parts);
        self.state.flatten_cache.insert(node, parts.clone());
        parts
    }
    fn build_optional_chain(
        parts: &[ChainPart],
        checked_lengths: &[usize],
        call_optional: bool,
        strip_non_null: bool,
    ) -> (String, bool) {
        let max_checked = checked_lengths.iter().copied().max().unwrap_or(0);
        let mut optional_parts = vec![false; parts.len()];
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                optional_parts[i] = checked_lengths.contains(&i)
                    || part.optional
                    || part.is_call && i == parts.len() - 1 && call_optional;
            }
            if optional_parts[i] && part.is_private {
                return (String::new(), true);
            }
        }
        let mut result = Vec::new();
        for (i, part) in parts.iter().enumerate() {
            let mut text = part.text.as_slice();
            if strip_non_null
                && i < parts.len() - 1
                && optional_parts[i + 1]
                && part.has_non_null
                && i < max_checked
            {
                text = &text[..text.len() - 1];
            }
            if i > 0 && optional_parts[i] {
                result.extend_from_slice(b"?.");
            } else if i > 0 {
                if part.optional && i > max_checked {
                    result.extend_from_slice(b"?.");
                } else if part.requires_dot {
                    result.push(b'.');
                }
            }
            result.extend_from_slice(text);
        }
        match String::from_utf8(result) {
            Ok(text) => (text, true),
            Err(error) => (String::from_utf8_lossy(error.as_bytes()).into_owned(), false),
        }
    }
    fn contains_optional_chain(&mut self, n: P<Node>) -> bool {
        if let Some(&result) = self.state.optional_chain_cache.get(&n) {
            return result;
        }
        let unwrapped = ast::skip_parentheses(n);
        let result = match unwrapped.kind() {
            Kind::PropertyAccessExpression => {
                unwrapped.as_property_access_expression().question_dot_token().is_some()
                    || self.contains_optional_chain(unwrapped.expression().unwrap())
            }
            Kind::ElementAccessExpression => {
                unwrapped.as_element_access_expression().question_dot_token().is_some()
                    || self.contains_optional_chain(unwrapped.expression().unwrap())
            }
            Kind::CallExpression => {
                unwrapped.as_call_expression().question_dot_token().is_some()
                    || self.contains_optional_chain(unwrapped.expression().unwrap())
            }
            Kind::BinaryExpression => {
                let bin = unwrapped.as_binary_expression();
                self.contains_optional_chain(bin.left) || self.contains_optional_chain(bin.right())
            }
            _ => false,
        };
        self.state.optional_chain_cache.insert(n, result);
        result
    }
    fn all_subsequent_have_optional_chaining(&mut self, chain: &[Operand]) -> bool {
        chain
            .iter()
            .skip(1)
            .all(|op| op.compared_expr.is_none_or(|n| self.contains_optional_chain(n)))
    }
    fn parse_operand(node: P<Node>, operator: Kind) -> Operand {
        let and = is_and(operator);
        let unwrapped = unwrap_for_comparison(node);
        if unwrapped.kind() == Kind::ThisKeyword {
            return Operand::invalid(node);
        }
        let mut base = unwrapped;
        while matches!(
            base.kind(),
            Kind::PropertyAccessExpression
                | Kind::ElementAccessExpression
                | Kind::CallExpression
                | Kind::NonNullExpression
                | Kind::ParenthesizedExpression
                | Kind::AsExpression
                | Kind::TypeAssertionExpression
        ) {
            base = base.expression().unwrap();
        }
        if base.kind() == Kind::BinaryExpression
            && matches!(
                base.as_binary_expression().operator_token.kind(),
                Kind::AmpersandAmpersandToken | Kind::BarBarToken
            )
        {
            return Operand::invalid(node);
        }
        if unwrapped.kind() == Kind::BinaryExpression {
            let bin = unwrapped.as_binary_expression();
            let op = bin.operator_token.kind();
            let pair = if is_nullish(bin.right()) || bin.right().kind() == Kind::StringLiteral {
                Some((bin.left, bin.right()))
            } else if is_nullish(bin.left) || bin.left.kind() == Kind::StringLiteral {
                Some((bin.right(), bin.left))
            } else {
                None
            };
            if let Some((expr, value)) = pair {
                let expr = ast::skip_parentheses(expr);
                if expr.kind() == Kind::TypeOfExpression
                    && value.kind() == Kind::StringLiteral
                    && value.text() == "undefined"
                    && (and
                        && matches!(
                            op,
                            Kind::ExclamationEqualsEqualsToken | Kind::ExclamationEqualsToken
                        )
                        || !and
                            && matches!(
                                op,
                                Kind::EqualsEqualsEqualsToken | Kind::EqualsEqualsToken
                            ))
                {
                    return Operand::new(TypeofCheck, node, expr.expression().unwrap());
                }
                let null = is_null(value);
                let undef = is_undefined(value);
                if and {
                    match op {
                        Kind::ExclamationEqualsEqualsToken if null => {
                            return Operand::new(NotStrictEqualNull, node, expr);
                        }
                        Kind::ExclamationEqualsEqualsToken if undef => {
                            return Operand::new(NotStrictEqualUndef, node, expr);
                        }
                        Kind::ExclamationEqualsToken if null || undef => {
                            return Operand::new(NotEqualBoth, node, expr);
                        }
                        Kind::EqualsEqualsEqualsToken
                            if matches!(expr.kind(), Kind::Identifier | Kind::ThisKeyword) =>
                        {
                            if null {
                                return Operand::new(StrictEqualNull, node, expr);
                            }
                            if undef {
                                return Operand::new(StrictEqualUndef, node, expr);
                            }
                        }
                        Kind::EqualsEqualsToken
                            if (null || undef)
                                && matches!(expr.kind(), Kind::Identifier | Kind::ThisKeyword) =>
                        {
                            return Operand::new(EqualNull, node, expr);
                        }
                        _ => {}
                    }
                } else if is_access(expr) {
                    if null || undef {
                        return Operand::new(Comparison, node, expr);
                    }
                } else {
                    match op {
                        Kind::EqualsEqualsEqualsToken if null => {
                            return Operand::new(NotStrictEqualNull, node, expr);
                        }
                        Kind::EqualsEqualsEqualsToken if undef => {
                            return Operand::new(NotStrictEqualUndef, node, expr);
                        }
                        Kind::EqualsEqualsToken if null || undef => {
                            return Operand::new(NotEqualBoth, node, expr);
                        }
                        _ => {}
                    }
                }
            }
        }
        if unwrapped.kind() == Kind::PrefixUnaryExpression {
            let prefix = unwrapped.as_prefix_unary_expression();
            if prefix.operator == Kind::ExclamationToken {
                if prefix.operand.kind() == Kind::ThisKeyword {
                    return Operand::invalid(node);
                }
                return Operand::new(
                    if and { NegatedAndOperand } else { Not },
                    node,
                    prefix.operand,
                );
            }
        }
        if unwrapped.kind() == Kind::BinaryExpression {
            let bin = unwrapped.as_binary_expression();
            if matches!(
                bin.operator_token.kind(),
                Kind::LessThanToken
                    | Kind::GreaterThanToken
                    | Kind::LessThanEqualsToken
                    | Kind::GreaterThanEqualsToken
                    | Kind::InKeyword
                    | Kind::InstanceOfKeyword
            ) {
                return Operand::invalid(node);
            }
            let left = ast::skip_parentheses(bin.left);
            let right = ast::skip_parentheses(bin.right());
            if is_access(left) && is_access(right) || !is_access(left) && !is_access(right) {
                return Operand::invalid(node);
            }
            return Operand::new(Comparison, node, if is_access(right) { right } else { left });
        }
        Operand::new(Plain, node, unwrapped)
    }
    fn collect_operands(&mut self, node: P<Node>, operator: Kind, out: &mut Vec<P<Node>>) {
        let n = ast::skip_parentheses(node);
        if n.kind() == Kind::BinaryExpression
            && n.as_binary_expression().operator_token.kind() == operator
        {
            let bin = n.as_binary_expression();
            self.collect_operands(bin.left, operator, out);
            self.collect_operands(bin.right(), operator, out);
            let r = self.get_node_range(n);
            self.state.seen_ranges.insert((r.pos, r.end));
        } else {
            out.push(node);
        }
    }
    fn validate_chain(&mut self, chain: &[Operand], operator: Kind) -> bool {
        if chain.len() < 2 {
            return false;
        }
        let mut first = None;
        for op in chain {
            if let Some(n) = op.compared_expr {
                let base = get_base_identifier(n);
                if let Some(f) = first {
                    if !are_nodes_structurally_equal(f, base) {
                        return false;
                    }
                } else {
                    first = Some(base);
                }
            }
        }
        if !chain
            .iter()
            .any(|op| op.compared_expr.is_some_and(|n| is_access(ast::skip_parentheses(n))))
        {
            return false;
        }
        !self.should_skip_for_require_nullish(chain, operator)
    }
    fn should_skip_for_require_nullish(&mut self, chain: &[Operand], operator: Kind) -> bool {
        if !self.opts.require_nullish {
            return false;
        }
        if !is_and(operator) && chain.first().is_some_and(|o| o.typ == Not) {
            return true;
        }
        for (i, op) in chain.iter().enumerate() {
            if op.typ != Plain {
                return false;
            }
            if is_and(operator)
                && i < chain.len() - 1
                && op.compared_expr.is_some_and(|n| self.includes_explicit_nullish(n))
            {
                return false;
            }
        }
        true
    }
    fn process_chain(&mut self, node: P<Node>, operator: Kind) {
        if matches!(operator, Kind::BarBarToken | Kind::QuestionQuestionToken) {
            self.handle_empty_object_pattern(node);
        }
        if operator == Kind::QuestionQuestionToken || is_inside_jsx(Some(node)) {
            return;
        }
        let r = self.get_node_range(node);
        if self.state.seen_ranges.contains(&(r.pos, r.end)) {
            return;
        }
        let mut operand_nodes = Vec::new();
        self.collect_operands(node, operator, &mut operand_nodes);
        if operand_nodes.len() < 2 {
            return;
        }
        let operands: Vec<_> = operand_nodes
            .iter()
            .enumerate()
            .map(|(i, &n)| {
                let op = Self::parse_operand(n, operator);
                let disallow_falsy_literal = is_and(operator) && op.typ == Plain
                    || operator == Kind::BarBarToken && op.typ == Not;
                if i < operand_nodes.len() - 1
                    && disallow_falsy_literal
                    && self.has_falsy_literal_type(op.compared_expr.unwrap())
                {
                    Operand::invalid(n)
                } else {
                    op
                }
            })
            .collect();
        let chains = if is_and(operator) {
            self.build_and_chains(&operands)
        } else {
            self.build_or_chains(&operands)
        };
        for chain in chains {
            if !self.validate_chain(&chain, operator) {
                continue;
            }
            let chain = if is_and(operator) {
                self.validate_and_chain_for_reporting(&chain)
            } else {
                self.validate_or_chain_for_reporting(&chain)
            };
            if chain.is_empty() {
                continue;
            }
            if is_and(operator) {
                self.generate_and_chain_fix_and_report(node, &chain, &operand_nodes);
            } else {
                self.generate_or_chain_fix_and_report(node, &chain, &operand_nodes);
            }
        }
    }
    fn build_and_chains(&mut self, operands: &[Operand]) -> Vec<Vec<Operand>> {
        use NodeComparisonResult as C;
        let mut all = Vec::new();
        let mut current: Vec<Operand> = Vec::new();
        let mut last = None;
        let mut last_check = Invalid;
        let mut complete = false;
        let mut i = 0;
        while i < operands.len() {
            let op = operands[i];
            if matches!(
                op.typ,
                Invalid | NegatedAndOperand | EqualNull | StrictEqualNull | StrictEqualUndef
            ) {
                if current.len() >= 2 {
                    all.push(std::mem::take(&mut current));
                }
                current.clear();
                last = None;
                last_check = Invalid;
                complete = false;
                i += 1;
                continue;
            }
            if current.is_empty() {
                if is_non_nullish_trailing_comparison(op) {
                    i += 1;
                    continue;
                }
                current.push(op);
                last = op.compared_expr;
                if op.typ != Plain {
                    last_check = op.typ;
                }
                complete = false;
                i += 1;
                continue;
            }
            if complete {
                if current.len() >= 2 {
                    all.push(std::mem::take(&mut current));
                }
                current = vec![op];
                last = op.compared_expr;
                last_check = if op.typ == Plain { Invalid } else { op.typ };
                complete = false;
                i += 1;
                continue;
            }
            let mut cmp = self.compare_nodes(last.unwrap(), op.compared_expr.unwrap());
            if self.should_stop_at_strict_nullish_check(*current.last().unwrap()) {
                if current.len() >= 2 {
                    all.push(std::mem::take(&mut current));
                }
                current.clear();
                break;
            }
            if cmp == C::Invalid
                && self.should_allow_call_chain_extension(
                    *current.last().unwrap(),
                    op,
                    Kind::AmpersandAmpersandToken,
                )
                && Self::try_extend_through_call_expression(
                    last.unwrap(),
                    op,
                    current[0].compared_expr,
                    Kind::AmpersandAmpersandToken,
                ) == C::Subset
            {
                cmp = C::Subset;
            }
            if cmp == C::Equal
                && i + 1 < operands.len()
                && self.try_merge_complementary_pair(
                    op,
                    operands[i + 1],
                    Kind::AmpersandAmpersandToken,
                )
            {
                current.extend_from_slice(&operands[i..i + 2]);
                last = operands[i + 1].compared_expr;
                i += 2;
                continue;
            }
            if op.typ.is_nullish_check()
                && last_check != Invalid
                && !are_nullish_checks_consistent(last_check, op.typ)
            {
                if current.len() >= 2 {
                    all.push(std::mem::take(&mut current));
                }
                current.clear();
                break;
            }
            if matches!(cmp, C::Subset | C::Equal) {
                current.push(op);
                last = op.compared_expr;
                if op.typ != Plain && op.typ.is_nullish_check() {
                    last_check = op.typ;
                }
                if is_non_nullish_trailing_comparison(op) {
                    complete = true;
                }
                i += 1;
                continue;
            }
            if current.len() >= 2 {
                all.push(std::mem::take(&mut current));
            }
            current = vec![op];
            last = op.compared_expr;
            last_check = if op.typ == Plain { Invalid } else { op.typ };
            i += 1;
        }
        if current.len() >= 2 {
            all.push(current);
        }
        for chain in &mut all {
            while chain.len() >= 2 {
                let last = chain[chain.len() - 1];
                let prev = chain[chain.len() - 2];
                if last.typ == Plain
                    && prev.typ == Plain
                    && self.compare_nodes(prev.compared_expr.unwrap(), last.compared_expr.unwrap())
                        == C::Equal
                {
                    chain.pop();
                } else {
                    break;
                }
            }
        }
        all
    }
    fn build_or_chains(&mut self, operands: &[Operand]) -> Vec<Vec<Operand>> {
        use NodeComparisonResult as C;
        let mut chain: Vec<Operand> = Vec::new();
        let mut last = None;
        for &op in operands {
            if !matches!(
                op.typ,
                Not | Comparison
                    | Plain
                    | TypeofCheck
                    | NotStrictEqualNull
                    | NotStrictEqualUndef
                    | NotEqualBoth
                    | StrictEqualNull
                    | StrictEqualUndef
                    | EqualNull
            ) {
                if chain.len() >= 2 {
                    break;
                }
                chain.clear();
                last = None;
                continue;
            }
            if chain.is_empty() {
                if Self::is_invalid_or_chain_starting_operand(op) {
                    continue;
                }
                chain.push(op);
                last = op.compared_expr;
                continue;
            }
            let mut cmp = self.compare_nodes(last.unwrap(), op.compared_expr.unwrap());
            if cmp == C::Invalid
                && self.should_allow_call_chain_extension(
                    *chain.last().unwrap(),
                    op,
                    Kind::BarBarToken,
                )
                && Self::try_extend_through_call_expression(
                    last.unwrap(),
                    op,
                    None,
                    Kind::BarBarToken,
                ) == C::Subset
            {
                cmp = C::Subset;
            }
            if matches!(cmp, C::Subset | C::Equal) {
                if cmp == C::Equal && op.typ == Comparison && !is_nullish_comparison(op) {
                    let last_op = *chain.last().unwrap();
                    if matches!(
                        last_op.typ,
                        Not | NotStrictEqualNull | NotStrictEqualUndef | NotEqualBoth | Plain
                    ) || is_non_nullish_trailing_comparison(last_op)
                    {
                        if chain.len() >= 2 {
                            break;
                        }
                        if is_non_nullish_trailing_comparison(last_op) {
                            chain.clear();
                            last = None;
                            continue;
                        }
                    }
                }
                chain.push(op);
                last = op.compared_expr;
                continue;
            }
            if chain.len() >= 2 {
                break;
            }
            chain = vec![op];
            last = op.compared_expr;
        }
        if chain.len() < 2 { Vec::new() } else { vec![chain] }
    }
    fn should_stop_at_strict_nullish_check(&mut self, prev: Operand) -> bool {
        if !prev.typ.is_strict_nullish_check() || prev.compared_expr.is_none() {
            return false;
        }
        let expr = prev.compared_expr.unwrap();
        let unwrapped = ast::skip_parentheses(expr);
        let call = matches!(unwrapped.kind(), Kind::CallExpression | Kind::NewExpression);
        let element = unwrapped.kind() == Kind::ElementAccessExpression;
        if !call && !element {
            return false;
        }
        let any = self.type_is_any_or_unknown(expr);
        let i = self.get_type_info(expr);
        let null = i.has_null || i.has_any || i.has_unknown;
        let undef = i.has_undefined || i.has_any || i.has_unknown;
        let incomplete = !any && null && undef;
        let mismatched =
            !any && matches!(prev.typ, NotStrictEqualUndef | NotStrictEqualNull) && !undef && !null;
        if call {
            incomplete || mismatched
        } else {
            mismatched || incomplete && !self.opts.unsafe_fixes
        }
    }
    fn try_merge_complementary_pair(&mut self, a: Operand, b: Operand, operator: Kind) -> bool {
        let (Some(ae), Some(be)) = (a.compared_expr, b.compared_expr) else {
            return false;
        };
        if self.compare_nodes(ae, be) != NodeComparisonResult::Equal {
            return false;
        }
        if is_and(operator) {
            a.typ == NotStrictEqualNull && matches!(b.typ, NotStrictEqualUndef | TypeofCheck)
                || b.typ == NotStrictEqualNull && matches!(a.typ, NotStrictEqualUndef | TypeofCheck)
        } else {
            a.typ == StrictEqualNull && b.typ == StrictEqualUndef
                || a.typ == StrictEqualUndef && b.typ == StrictEqualNull
        }
    }
    fn validate_and_chain_for_reporting(&mut self, chain: &[Operand]) -> Vec<Operand> {
        let first = chain[0];
        if first.typ == Plain
            && first.compared_expr.is_some_and(|n| self.contains_optional_chain(n))
        {
            return Vec::new();
        }
        if first.typ.is_strict_nullish_check()
            && first.compared_expr.is_some_and(|n| self.contains_optional_chain(n))
            && !is_split_strict_equals_pattern(chain)
        {
            return Vec::new();
        }
        if self.all_operands_check_same_expression(chain)
            || self.should_skip_optimal_strict_checks(chain)
        {
            return Vec::new();
        }
        if chain.len() == 2 {
            let second = chain[1];
            if second.compared_expr.is_some_and(|n| self.contains_optional_chain(n)) {
                let a = self.flatten_for_fix(first.compared_expr.unwrap());
                let b = self.flatten_for_fix(second.compared_expr.unwrap_or(second.node));
                let redundant = b.len() == a.len() + 1
                    && a.iter().zip(&b).all(|(a, b)| a.text == b.text && a.optional == b.optional)
                    && b.last().unwrap().optional;
                if !redundant
                    && b.len() > a.len()
                    && !a.is_empty()
                    && a.iter().zip(&b).all(|(a, b)| a.text == b.text)
                    && b[a.len()..].iter().any(|p| p.optional)
                {
                    return Vec::new();
                }
                if second.typ == Comparison && redundant {
                    return Vec::new();
                }
            }
        }
        if first.typ == Plain && first.compared_expr.is_some_and(|n| self.get_type_info(n).has_void)
        {
            return Vec::new();
        }
        if !self.opts.unsafe_fixes
            && (chain.iter().any(|op| op.node.kind() == Kind::NonNullExpression)
                || self.has_incomplete_nullish_check(chain))
        {
            return Vec::new();
        }
        for (i, op) in chain.iter().enumerate() {
            if op.typ == Plain && i == 0 {
                let expr = op.compared_expr.unwrap();
                if self.should_skip_by_type(expr)
                    || self.would_change_return_type(expr) && !self.opts.unsafe_fixes
                {
                    return Vec::new();
                }
            }
            if op.typ == TypeofCheck
                && op.compared_expr.is_some_and(|n| !self.includes_nullish(n))
                && chain.len() == 2
                && chain
                    .last()
                    .unwrap()
                    .compared_expr
                    .is_some_and(|n| ast::skip_parentheses(n).kind() == Kind::CallExpression)
            {
                return Vec::new();
            }
        }
        if self.is_unsafe_trailing_comparison(chain, *chain.last().unwrap()) {
            return Vec::new();
        }
        chain.to_vec()
    }
    fn has_incomplete_nullish_check(&mut self, chain: &[Operand]) -> bool {
        let mut guards = chain;
        if chain.len() >= 2 {
            let last = chain[chain.len() - 1];
            if last.typ.is_trailing_comparison() {
                guards = &chain[..chain.len() - 1];
            } else if last.typ == Plain
                && let (Some(le), Some(pe)) =
                    (last.compared_expr, chain[chain.len() - 2].compared_expr)
                && self.flatten_for_fix(le).len() > self.flatten_for_fix(pe).len()
            {
                guards = &chain[..chain.len() - 1];
            }
        }
        let a = analyze_nullish_checks(guards, false);
        let has_typeof = guards.iter().any(|o| o.typ == TypeofCheck);
        let trailing_both = guards.len() < chain.len() && chain.last().unwrap().typ == NotEqualBoth;
        let trailing_optional = guards.len() < chain.len()
            && chain.last().unwrap().compared_expr.is_some_and(|n| self.contains_optional_chain(n));
        let first_not_nullish =
            guards.first().and_then(|o| o.compared_expr).is_some_and(|n| !self.includes_nullish(n));
        let mut strict_complete = false;
        if let Some(n) = guards.first().and_then(|o| o.compared_expr) {
            let info = self.get_type_info(n);
            if !info.is_any_or_unknown() {
                strict_complete = a.has_null_check
                    && !a.has_undefined_check
                    && info.has_null
                    && !info.has_undefined
                    || !a.has_null_check
                        && a.has_undefined_check
                        && info.has_undefined
                        && !info.has_null;
            }
        }
        !a.has_both_check
            && !has_typeof
            && !trailing_both
            && !trailing_optional
            && !first_not_nullish
            && !strict_complete
            && a.has_incomplete_check()
    }
    fn is_unsafe_trailing_comparison(&mut self, chain: &[Operand], last: Operand) -> bool {
        let mut trailing = last.typ == Comparison;
        if !trailing
            && chain.len() >= 2
            && matches!(last.typ, NotStrictEqualNull | NotStrictEqualUndef | NotEqualBoth)
            && let (Some(le), Some(pe)) = (last.compared_expr, chain[chain.len() - 2].compared_expr)
        {
            trailing = self.flatten_for_fix(le).len() > self.flatten_for_fix(pe).len();
        }
        if trailing && let Some(bin) = unwrap_binary(last.node) {
            let value = if last
                .compared_expr
                .is_some_and(|n| are_nodes_structurally_equal(n, bin.left))
            {
                Some(bin.right)
            } else if last.compared_expr.is_some_and(|n| are_nodes_structurally_equal(n, bin.right))
            {
                Some(bin.left)
            } else if is_access(bin.left) {
                Some(bin.right)
            } else if is_access(bin.right) {
                Some(bin.left)
            } else {
                None
            };
            if let Some(value) = value {
                return !self.is_safe_trailing_comparison_value(bin.operator, value)
                    && !self.opts.unsafe_fixes;
            }
        }
        false
    }
    fn is_safe_trailing_comparison_value(&mut self, operator: Kind, value: P<Node>) -> bool {
        let i = self.get_type_info(value);
        match operator {
            Kind::EqualsEqualsToken => !i.can_be_nullish_like(),
            Kind::EqualsEqualsEqualsToken => !i.can_be_undefined_like(),
            Kind::ExclamationEqualsToken => i.is_always_nullish_like(),
            Kind::ExclamationEqualsEqualsToken => i.is_always_undefined_like(),
            Kind::LessThanToken
            | Kind::GreaterThanToken
            | Kind::LessThanEqualsToken
            | Kind::GreaterThanEqualsToken => false,
            _ => true,
        }
    }
    fn validate_or_chain_nullish_checks(&mut self, chain: &[Operand]) -> Vec<Operand> {
        if self.opts.unsafe_fixes {
            return chain.to_vec();
        }
        let analysis = analyze_nullish_checks(chain, true);
        let first_not_nullish = chain[0].compared_expr.is_some_and(|n| !self.includes_nullish(n));
        let trailing_optional = chain.len() >= 2
            && chain.last().unwrap().compared_expr.is_some_and(|n| self.contains_optional_chain(n));
        let mut strict_complete = true;
        let mut any_nullable = false;
        for op in chain {
            let Some(n) = op.compared_expr else {
                continue;
            };
            let i = self.get_type_info(n);
            if i.has_no_nullable_types() {
                continue;
            }
            any_nullable = true;
            if i.is_any_or_unknown()
                || i.has_both_null_and_undefined()
                || analysis.has_only_null_check() && !i.has_null && i.has_undefined
                || analysis.has_only_undefined_check() && i.has_null && !i.has_undefined
            {
                strict_complete = false;
                break;
            }
        }
        if !any_nullable {
            strict_complete = false;
        }
        if !analysis.has_both_check
            && !first_not_nullish
            && !trailing_optional
            && !strict_complete
            && analysis.has_incomplete_check()
        {
            return Vec::new();
        }
        let mut truncate = None;
        for (index, op) in chain.iter().enumerate() {
            if index == chain.len() - 1
                || op.compared_expr.is_some_and(|n| self.contains_optional_chain(n))
            {
                continue;
            }
            if let Some(n) = op.compared_expr {
                let info = self.get_type_info(n);
                if info.has_null && info.has_undefined {
                    let should = matches!(
                        op.typ,
                        NotStrictEqualNull
                            | NotStrictEqualUndef
                            | StrictEqualNull
                            | StrictEqualUndef
                    ) || op.typ == Comparison && {
                        let node = ast::skip_parentheses(op.node);
                        node.kind() == Kind::BinaryExpression && {
                            let bin = node.as_binary_expression();
                            bin.operator_token.kind() == Kind::EqualsEqualsEqualsToken
                                && ((is_null(bin.left) || is_null(bin.right()))
                                    != (is_undefined(bin.left) || is_undefined(bin.right())))
                        }
                    };
                    if should {
                        truncate = Some(truncate.map_or(index + 1, |t: usize| t.min(index + 1)));
                    }
                }
            }
        }
        let chain = &chain[..truncate.unwrap_or(chain.len())];
        if chain.len() < 2 {
            return Vec::new();
        }
        if matches!(chain[0].typ, TypeofCheck | StrictEqualNull | StrictEqualUndef | EqualNull)
            && !self.opts.unsafe_fixes
        {
            for &op in &chain[1..] {
                if op.typ == Comparison && !Self::is_or_chain_comparison_safe(op)
                    || matches!(op.typ, Plain | Not)
                {
                    return Vec::new();
                }
            }
        }
        if analysis.has_null_check
            && !analysis.has_undefined_check
            && !analysis.has_both_check
            && !strict_complete
        {
            return Vec::new();
        }
        if chain[0].typ == Comparison && is_nullish_comparison(chain[0]) && !self.opts.unsafe_fixes
        {
            for &op in &chain[1..] {
                if op.typ == Comparison
                    && !Self::is_or_chain_comparison_safe(op)
                    && !is_nullish_comparison(op)
                {
                    return Vec::new();
                }
            }
        }
        chain.to_vec()
    }
    fn has_unsafe_strict_nullish_guard_with_negated_access(&mut self, chain: &[Operand]) -> bool {
        for (i, guard) in chain.iter().enumerate().take(chain.len() - 1) {
            let Some(expr) = guard.compared_expr else {
                continue;
            };
            if !guard.typ.is_strict_nullish_check() {
                continue;
            }
            let ty = self.get_type_info(expr);
            if !ty.is_any_or_unknown() && !ty.has_both_null_and_undefined() {
                continue;
            }
            for candidate in &chain[i + 1..] {
                if candidate.typ != Not {
                    continue;
                }
                let Some(n) = candidate.compared_expr else {
                    continue;
                };
                if matches!(
                    self.compare_nodes(expr, n),
                    NodeComparisonResult::Equal | NodeComparisonResult::Subset
                ) && is_access(ast::skip_parentheses(n))
                {
                    return true;
                }
            }
        }
        false
    }
    fn validate_or_chain_for_reporting(&mut self, chain: &[Operand]) -> Vec<Operand> {
        if !chain.iter().any(|o| o.typ != Plain)
            || self.should_skip_or_chain_optimal_checks(chain)
            || self.all_operands_check_same_expression(chain)
        {
            return Vec::new();
        }
        let all_optional = self.all_subsequent_have_optional_chaining(chain);
        if all_optional {
            let first = chain[0];
            let explicit = matches!(first.typ, StrictEqualNull | EqualNull)
                || is_comparison_against(first, is_null);
            if explicit
                && !chain.iter().any(|o| {
                    o.compared_expr.is_some_and(|n| self.get_type_info(n).has_explicit_nullish())
                })
            {
                return Vec::new();
            }
        }
        let all_strict = chain.iter().all(|op| {
            if op.typ.is_loose_nullish_check() || matches!(op.typ, Plain | Not) {
                return false;
            }
            if op.typ == Comparison && op.node.kind() == Kind::BinaryExpression {
                let bin = op.node.as_binary_expression();
                if bin.operator_token.kind() == Kind::EqualsEqualsToken {
                    let left = ast::skip_parentheses(bin.left);
                    let right = ast::skip_parentheses(bin.right());
                    if is_null(left)
                        || is_null(right)
                        || is_undefined_identifier(left)
                        || is_undefined_identifier(right)
                    {
                        return false;
                    }
                }
            }
            true
        });
        if all_optional && all_strict {
            let mut null = false;
            let mut undef = false;
            for op in chain {
                if let Some(n) = op.compared_expr {
                    let i = self.get_type_info(n);
                    null |= i.has_null;
                    undef |= i.has_undefined;
                }
            }
            if null == undef {
                return Vec::new();
            }
        }
        if chain[0].typ == Not
            && !self.opts.unsafe_fixes
            && let Some(first) = chain[0].compared_expr
        {
            if is_access(ast::skip_parentheses(first)) {
                for &op in &chain[1..] {
                    if op.typ == Comparison && !Self::is_or_chain_comparison_safe(op) {
                        return Vec::new();
                    }
                }
            } else {
                let mut intermediate = false;
                for (i, &op) in chain.iter().enumerate().skip(1) {
                    let comparison = op.typ == Comparison;
                    let safe = comparison && Self::is_or_chain_comparison_safe(op);
                    let middle = comparison && is_nullish_comparison(op) && i < chain.len() - 1;
                    intermediate |= middle;
                    let allowed_plain = op.typ == Plain && i == chain.len() - 1 && intermediate;
                    if op.typ != Not
                        && !safe
                        && !middle
                        && !op.typ.is_nullish_check()
                        && !allowed_plain
                    {
                        return Vec::new();
                    }
                }
            }
        }
        if !self.opts.unsafe_fixes
            && matches!(chain[0].typ, NotEqualBoth | NotStrictEqualNull | NotStrictEqualUndef)
        {
            let first = self.get_type_info(chain[0].compared_expr.unwrap());
            for (i, &op) in chain.iter().enumerate().skip(1) {
                if op.typ == Comparison && !Self::is_or_chain_comparison_safe(op) {
                    if i == chain.len() - 1 && is_nullish_comparison(op) {
                        if first.has_both_null_and_undefined() || first.is_any_or_unknown() {
                            return Vec::new();
                        }
                    } else if !is_nullish_comparison(op) {
                        return Vec::new();
                    }
                }
            }
        }
        if chain[0].typ == Plain
            && let Some(first) = chain[0].compared_expr
        {
            let n = ast::skip_parentheses(first);
            if n.kind() == Kind::MetaProperty {
                return Vec::new();
            }
            if !self.opts.unsafe_fixes
                && !is_access(n)
                && chain[1..].iter().any(|op| op.typ == Comparison)
            {
                return Vec::new();
            }
        }
        let chain = self.validate_or_chain_nullish_checks(chain);
        if chain.len() < 2 {
            return Vec::new();
        }
        if !self.opts.unsafe_fixes
            && self.has_unsafe_strict_nullish_guard_with_negated_access(&chain)
        {
            return Vec::new();
        }
        for op in &chain {
            if matches!(op.typ, Plain | Not)
                && self.would_change_return_type(op.compared_expr.unwrap())
                && !self.opts.unsafe_fixes
            {
                return Vec::new();
            }
        }
        for (i, op) in chain.iter().enumerate().take(chain.len() - 1) {
            if op.typ == Not {
                let negated = op.compared_expr.unwrap();
                for next in &chain[i + 1..] {
                    if next.typ == Not {
                        continue;
                    }
                    if let Some(expr) = next.compared_expr {
                        let n = ast::skip_parentheses(expr);
                        if n.kind() == Kind::CallExpression
                            && self.compare_nodes(negated, n.expression().unwrap())
                                == NodeComparisonResult::Equal
                        {
                            return Vec::new();
                        }
                    }
                }
            }
        }
        chain
    }
    fn all_operands_check_same_expression(&mut self, chain: &[Operand]) -> bool {
        if chain.len() < 2 {
            return false;
        }
        let first = self.flatten_for_fix(chain[0].compared_expr.unwrap());
        chain[1..].iter().all(|o| {
            let parts = self.flatten_for_fix(o.compared_expr.unwrap());
            first.len() == parts.len() && first.iter().zip(&parts).all(|(a, b)| a.text == b.text)
        })
    }
    fn should_skip_optimal_strict_checks(&mut self, chain: &[Operand]) -> bool {
        if chain.len() < 2 || !self.all_subsequent_have_optional_chaining(chain) {
            return false;
        }
        let mut has_nullish = false;
        for op in chain {
            if op.typ == Plain {
                continue;
            }
            if matches!(op.typ, Not | NegatedAndOperand) || op.typ.is_loose_nullish_check() {
                return false;
            }
            if op.typ.is_strict_nullish_check() {
                has_nullish = true;
            }
        }
        has_nullish
            && self.get_type_info(chain[0].compared_expr.unwrap()).has_both_null_and_undefined()
    }
    fn should_skip_or_chain_optimal_checks(&mut self, chain: &[Operand]) -> bool {
        if chain.len() < 2
            || !self.all_subsequent_have_optional_chaining(chain)
            || chain[0].typ != StrictEqualNull
        {
            return false;
        }
        chain[0].compared_expr.is_some_and(|n| {
            let i = self.get_type_info(n);
            !i.has_undefined && !i.has_any && !i.has_unknown
        })
    }
    fn has_shorter_undefined_check_before_strict_null_comparison(
        &mut self,
        chain: &[Operand],
    ) -> bool {
        if chain.len() < 2 {
            return false;
        }
        let last = *chain.last().unwrap();
        if !is_strict_null_check_for_or_chain(last) || last.compared_expr.is_none() {
            return false;
        }
        let last_len = self.flatten_for_fix(last.compared_expr.unwrap()).len();
        chain[..chain.len() - 1].iter().any(|o| {
            is_strict_undefined_check_for_or_chain(*o)
                && o.compared_expr.is_some_and(|n| self.flatten_for_fix(n).len() < last_len)
        })
    }
    fn generate_and_chain_fix_and_report(
        &mut self,
        node: P<Node>,
        chain: &[Operand],
        operand_nodes: &[P<Node>],
    ) {
        let mut endpoint = None;
        let mut trailing_comparison = false;
        let mut trailing_typeof = false;
        let mut complementary = false;
        let mut complementary_node = None;
        let mut loose_strict_plain = false;
        let mut loose_strict_node = None;
        if chain.len() >= 2 {
            let last = chain[chain.len() - 1];
            let prev = chain[chain.len() - 2];
            if let (Some(le), Some(pe)) = (last.compared_expr, prev.compared_expr)
                && self.compare_nodes(le, pe) == NodeComparisonResult::Equal
            {
                let lu = matches!(last.typ, NotStrictEqualUndef | TypeofCheck);
                let ln = last.typ == NotStrictEqualNull;
                let pu = matches!(prev.typ, NotStrictEqualUndef | TypeofCheck);
                let pn = prev.typ == NotStrictEqualNull;
                if lu && pn || ln && pu {
                    complementary = true;
                    endpoint = prev.compared_expr;
                    complementary_node = Some(last.node);
                    trailing_comparison = true;
                    trailing_typeof = prev.typ == TypeofCheck;
                }
            }
        }
        if !complementary && chain.len() >= 3 {
            let last = chain[chain.len() - 1];
            let prev = chain[chain.len() - 2];
            if last.typ == Plain
                && prev.typ.is_strict_nullish_check()
                && let Some(pe) = prev.compared_expr
            {
                let mut matching = false;
                for op in chain[..chain.len() - 2].iter().rev() {
                    if let Some(n) = op.compared_expr
                        && self.compare_nodes(n, pe) == NodeComparisonResult::Equal
                        && (prev.typ == NotStrictEqualNull
                            && matches!(op.typ, NotStrictEqualUndef | TypeofCheck)
                            || prev.typ == NotStrictEqualUndef && op.typ == NotStrictEqualNull)
                    {
                        matching = true;
                        break;
                    }
                }
                if !matching
                    && let Some(n) = chain[..chain.len() - 2]
                        .iter()
                        .rev()
                        .find(|o| o.typ.is_loose_nullish_check())
                        .and_then(|o| o.compared_expr)
                    && self.compare_nodes(n, pe) == NodeComparisonResult::Subset
                {
                    loose_strict_plain = true;
                    endpoint = Some(pe);
                    loose_strict_node = Some(last.node);
                    trailing_comparison = true;
                    trailing_typeof = false;
                }
            }
        }
        if !complementary && !loose_strict_plain {
            for op in chain.iter().rev() {
                match op.typ {
                    Plain => {
                        endpoint = Some(op.node);
                        trailing_comparison = false;
                        trailing_typeof = false;
                        break;
                    }
                    Comparison | NotStrictEqualNull | NotStrictEqualUndef | NotEqualBoth => {
                        endpoint = op.compared_expr;
                        trailing_comparison = true;
                        trailing_typeof = false;
                        break;
                    }
                    TypeofCheck => {
                        endpoint = op.compared_expr;
                        trailing_comparison = true;
                        trailing_typeof = true;
                        break;
                    }
                    _ if op.compared_expr.is_some() => {
                        endpoint = op.compared_expr;
                        trailing_comparison = false;
                        trailing_typeof = false;
                        break;
                    }
                    _ => {}
                }
            }
        }
        let Some(endpoint) = endpoint else {
            return;
        };
        let mut parts = self.flatten_for_fix(endpoint);
        if !parts.is_empty() && chain[0].typ == Plain {
            let first = self.flatten_for_fix(chain[0].node);
            if first.len() == 1
                && first.len() <= parts.len()
                && first[0].text.len() > parts[0].text.len()
            {
                parts[0] = first[0].clone();
            }
        }
        let mut checked_lengths = Vec::new();
        let checks: Vec<_> = chain
            .iter()
            .enumerate()
            .filter_map(|(i, &op)| {
                let call = op.compared_expr.is_some_and(|n| n.kind() == Kind::CallExpression);
                if i == chain.len() - 1 && (op.typ == Plain || op.typ == Not && call) {
                    None
                } else {
                    Some(op)
                }
            })
            .collect();
        let non_typeof = checks.iter().any(|o| o.typ != TypeofCheck && o.compared_expr.is_some());
        for op in &checks {
            if let Some(expr) = op.compared_expr {
                if op.typ == TypeofCheck && non_typeof {
                    continue;
                }
                insert_length(&mut checked_lengths, self.flatten_for_fix(expr).len());
            }
        }
        let min_checked = checked_lengths.iter().copied().min();
        let max_checked = checked_lengths.iter().copied().max();
        if min_checked == Some(1) && checked_lengths.len() == 1 {
            let fill_up_to = if chain.last().unwrap().typ == Plain {
                let p = self.flatten_for_fix(chain.last().unwrap().node);
                let mut fill = p.len().saturating_sub(1);
                if fill > 0 && p.last().is_some_and(|p| p.is_call) {
                    fill -= 1;
                }
                fill
            } else {
                max_checked.unwrap()
            };
            for i in 1..=fill_up_to {
                insert_length(&mut checked_lengths, i);
            }
        }
        if !checks.is_empty() && parts.len() > 1 {
            let mut max_len = 0;
            for op in &checks {
                if let Some(n) = op.compared_expr {
                    max_len = max_len.max(self.flatten_for_fix(n).len());
                }
            }
            for part in parts.iter_mut().take(max_len) {
                if part.has_non_null {
                    part.text = part.base_text().into();
                    part.has_non_null = false;
                }
            }
            let mut all_parts = Vec::new();
            for op in &checks {
                if let Some(n) = op.compared_expr {
                    let p = self.flatten_for_fix(if op.typ == Plain { op.node } else { n });
                    if p.len() <= parts.len()
                        && p.iter().zip(&parts).all(|(a, b)| a.base_text() == b.base_text())
                    {
                        all_parts.push(p);
                    }
                }
            }
            for (i, part) in parts.iter_mut().enumerate() {
                if let Some(shortest) =
                    all_parts.iter().filter(|p| i < p.len()).min_by_key(|p| p.len())
                {
                    part.optional = shortest[i].optional;
                    if shortest[i].has_non_null {
                        if !part.has_non_null {
                            part.text.push(b'!');
                        }
                        part.has_non_null = true;
                    }
                }
            }
        }
        let mut call_optional = false;
        if parts.last().is_some_and(|p| p.is_call) {
            for op in &chain[..chain.len() - 1] {
                if let Some(n) = op.compared_expr
                    && self.flatten_for_fix(n).len() == parts.len() - 1
                {
                    call_optional = true;
                    break;
                }
            }
        }
        let (mut new_code, valid_utf8) =
            Self::build_optional_chain(&parts, &checked_lengths, call_optional, false);
        if new_code.is_empty() {
            return;
        }
        let mut trivia = String::new();
        for op in &chain[1..] {
            let full = op.node.pos().expect_source("parsed operand");
            let trimmed = self.get_node_range(op.node).pos;
            if full < trimmed {
                trivia.push_str(self.source(full, trimmed));
            }
        }
        let trivia = trivia.trim_start_matches([' ', '\t', '\n', '\r']);
        if !trivia.is_empty() {
            new_code = format!("{trivia}{new_code}");
        }
        if trailing_comparison {
            let op = if complementary || loose_strict_plain {
                chain[chain.len() - 2]
            } else {
                chain[chain.len() - 1]
            };
            if op.node.kind() == Kind::BinaryExpression {
                let bin = op.node.as_binary_expression();
                let left = self.get_node_range(bin.left);
                let compared = self.get_node_range(op.compared_expr.unwrap());
                let whole = self.get_node_range(op.node);
                if trailing_typeof {
                    new_code = format!(
                        "{}{new_code}{}",
                        self.source(left.pos, compared.pos),
                        self.source(compared.end, whole.end)
                    );
                } else if compared.pos > left.pos {
                    new_code = format!("{}{new_code}", self.source(whole.pos, compared.pos));
                } else {
                    new_code.push_str(self.source(compared.end, whole.end));
                }
            }
            for extra in [complementary_node, loose_strict_node].into_iter().flatten() {
                let prev = self.get_node_range(chain[chain.len() - 2].node);
                let last = self.get_node_range(extra);
                new_code.push_str(self.source(prev.end, last.pos));
                new_code.push_str(self.get_node_text(extra));
            }
        }
        let mut effective_start = 0;
        if chain.len() >= 2
            && chain[0].typ == TypeofCheck
            && chain[1..].iter().any(|o| o.typ != TypeofCheck)
            && let Some(n) = chain[0].compared_expr
        {
            let symbol = self.ctx.checker.get_symbol_at_location_exported(n);
            if symbol.is_none_or(|s| s.declarations().is_empty()) {
                effective_start = 1;
            }
        }
        let range = if effective_start == 0 && chain.len() == operand_nodes.len() {
            self.get_node_range(node)
        } else {
            Range {
                pos: self.get_node_range(chain[effective_start].node).pos,
                end: self.get_node_range(chain.last().unwrap().node).end,
            }
        };
        let mut suggestion = !self.opts.unsafe_fixes;
        if suggestion {
            let last = *chain.last().unwrap();
            if matches!(
                last.typ,
                EqualNull | NotEqualBoth | StrictEqualUndef | NotStrictEqualUndef | TypeofCheck
            ) {
                suggestion = false;
            }
            if suggestion && trailing_comparison {
                suggestion = false;
            }
            if suggestion {
                for op in chain {
                    if let Some(n) = op.compared_expr {
                        let i = self.get_type_info(n);
                        if i.has_undefined || i.is_any_or_unknown() {
                            suggestion = false;
                            break;
                        }
                    }
                }
            }
            if !suggestion
                && matches!(chain[0].typ, NotEqualBoth | NotStrictEqualNull | NotStrictEqualUndef)
                && last.typ == Plain
                && let Some(n) = chain[0].compared_expr
                && !self.get_type_info(n).is_any_or_unknown()
            {
                suggestion = true;
            }
            if complementary
                && (last.typ == TypeofCheck || chain[chain.len() - 2].typ == NotStrictEqualNull)
            {
                suggestion = true;
            }
        }
        self.report_chain_with_fixes(
            node,
            vec![Fix { range, text: new_code }],
            suggestion,
            valid_utf8,
        );
    }
    fn generate_or_chain_fix_and_report(
        &mut self,
        node: P<Node>,
        chain: &[Operand],
        operand_nodes: &[P<Node>],
    ) {
        let trailing_comparison = chain.last().unwrap().typ.is_comparison_or_null_check();
        let mut trailing_plain = String::new();
        let mut optional_chain = chain;
        if chain.len() >= 3 && chain.last().unwrap().typ == Plain && !self.opts.unsafe_fixes {
            let last = chain[chain.len() - 1];
            let prev = chain[chain.len() - 2];
            if is_nullish_check_operand(prev)
                && let (Some(le), Some(pe)) = (last.compared_expr, prev.compared_expr)
                && self.flatten_for_fix(le).len() > self.flatten_for_fix(pe).len()
            {
                trailing_plain = self.get_node_text(last.node).into();
                optional_chain = &chain[..chain.len() - 1];
            }
        }
        if optional_chain.len() == 1
            && !trailing_plain.is_empty()
            && optional_chain[0].compared_expr.is_some_and(|n| self.contains_optional_chain(n))
        {
            return;
        }
        if chain.len() == 2
            && trailing_plain.is_empty()
            && (chain[0].compared_expr.is_some_and(|n| self.contains_optional_chain(n))
                || self.contains_optional_chain(chain[0].node))
        {
            return;
        }
        let last = *optional_chain.last().unwrap();
        let endpoint = if last.typ == Plain { last.node } else { last.compared_expr.unwrap() };
        let parts = self.flatten_for_fix(endpoint);
        let checks = if last.typ == Plain {
            &optional_chain[..optional_chain.len() - 1]
        } else {
            optional_chain
        };
        let mut checked_lengths = Vec::new();
        for op in checks {
            if let Some(n) = op.compared_expr {
                insert_length(&mut checked_lengths, self.flatten_for_fix(n).len());
            }
        }
        let mut call_optional = false;
        if parts.last().is_some_and(|p| p.is_call) {
            for op in &optional_chain[..optional_chain.len() - 1] {
                if self.flatten_for_fix(op.node).len() == parts.len() - 1 {
                    call_optional = true;
                    break;
                }
            }
        }
        let (code, valid_utf8) =
            Self::build_optional_chain(&parts, &checked_lengths, call_optional, true);
        if code.is_empty() {
            return;
        }
        let mut new_code = if last.typ.is_comparison_or_null_check() {
            if last.node.kind() == Kind::BinaryExpression {
                let bin = last.node.as_binary_expression();
                let compared = self.get_node_range(last.compared_expr.unwrap());
                let left = self.get_node_range(bin.left);
                if compared.pos > left.pos {
                    format!(
                        "{code} {} {}",
                        self.get_node_text(bin.operator_token),
                        self.source(left.pos, left.end)
                            .trim_matches(crate::utils::is_str_white_space)
                    )
                } else {
                    format!(
                        "{code}{}",
                        self.source(
                            bin.operator_token.pos().expect_source("parsed operator"),
                            bin.right().end().expect_source("parsed right operand")
                        )
                    )
                }
            } else {
                code
            }
        } else if last.typ == Not {
            format!("!{code}")
        } else {
            code
        };
        if !trailing_plain.is_empty() {
            let end = self.get_node_range(chain[chain.len() - 2].node).end;
            let start = self.get_node_range(chain.last().unwrap().node).pos;
            new_code.push_str(self.source(end, start));
            new_code.push_str(&trailing_plain);
        }
        let range = if chain.len() == operand_nodes.len() {
            self.get_node_range(node)
        } else {
            Range {
                pos: self.get_node_range(chain[0].node).pos,
                end: self.get_node_range(chain.last().unwrap().node).end,
            }
        };
        let mut suggestion = !self.opts.unsafe_fixes;
        if suggestion
            && matches!(
                chain.last().unwrap().typ,
                Not | EqualNull
                    | NotEqualBoth
                    | StrictEqualUndef
                    | NotStrictEqualUndef
                    | TypeofCheck
            )
        {
            suggestion = false;
        }
        if suggestion && trailing_comparison {
            let mut strict_suggestion = false;
            if !self.opts.unsafe_fixes {
                let analysis = analyze_nullish_checks(chain, true);
                if analysis.has_incomplete_check() {
                    let mut all_match = true;
                    let mut any_nullable = false;
                    for op in chain {
                        let Some(n) = op.compared_expr else {
                            continue;
                        };
                        let i = self.get_type_info(n);
                        if i.has_no_nullable_types() {
                            continue;
                        }
                        any_nullable = true;
                        if i.is_any_or_unknown()
                            || i.has_both_null_and_undefined()
                            || analysis.has_only_null_check() && (!i.has_null || i.has_undefined)
                            || analysis.has_only_undefined_check()
                                && (i.has_null || !i.has_undefined)
                        {
                            all_match = false;
                            break;
                        }
                    }
                    if any_nullable && all_match {
                        strict_suggestion = true;
                    }
                }
            }
            if !strict_suggestion {
                strict_suggestion =
                    self.has_shorter_undefined_check_before_strict_null_comparison(chain);
            }
            if !strict_suggestion {
                suggestion = false;
            }
        }
        if suggestion {
            for op in chain {
                if let Some(n) = op.compared_expr {
                    let i = self.get_type_info(n);
                    if i.is_any_or_unknown() || i.has_both_null_and_undefined() {
                        suggestion = false;
                        break;
                    }
                }
            }
        }
        self.report_chain_with_fixes(
            node,
            vec![Fix { range, text: new_code }],
            suggestion,
            valid_utf8,
        );
    }
    fn handle_empty_object_pattern(&mut self, node: P<Node>) {
        let bin = node.as_binary_expression();
        let right = bin.right();
        let object = if right.kind() == Kind::ObjectLiteralExpression {
            right
        } else if right.kind() == Kind::ParenthesizedExpression
            && right.expression().unwrap().kind() == Kind::ObjectLiteralExpression
        {
            right.expression().unwrap()
        } else {
            return;
        };
        if !object.properties().is_empty() {
            return;
        }
        if self.opts.require_nullish && !self.includes_explicit_nullish(bin.left) {
            return;
        }
        let Some(parent) = node.parent() else {
            return;
        };
        let access = if matches!(
            parent.kind(),
            Kind::PropertyAccessExpression | Kind::ElementAccessExpression
        ) {
            parent
        } else if parent.kind() == Kind::ParenthesizedExpression
            && let Some(grand) = parent.parent()
            && matches!(
                grand.kind(),
                Kind::PropertyAccessExpression | Kind::ElementAccessExpression
            )
        {
            grand
        } else {
            return;
        };
        let (optional, computed, prop) = if access.kind() == Kind::PropertyAccessExpression {
            (
                access.as_property_access_expression().question_dot_token().is_some(),
                false,
                access.name().unwrap(),
            )
        } else {
            (
                access.as_element_access_expression().question_dot_token().is_some(),
                true,
                access.as_element_access_expression().argument_expression,
            )
        };
        if optional {
            return;
        }
        let left = bin.left;
        let mut text = self.get_node_text(left).to_owned();
        if matches!(
            left.kind(),
            Kind::AwaitExpression
                | Kind::BinaryExpression
                | Kind::ConditionalExpression
                | Kind::PrefixUnaryExpression
                | Kind::AsExpression
                | Kind::VoidExpression
                | Kind::TypeOfExpression
                | Kind::PostfixUnaryExpression
                | Kind::DeleteExpression
        ) {
            text = format!("({text})");
        }
        text.push_str("?.");
        if computed {
            text.push('[');
        }
        text.push_str(self.get_node_text(prop));
        if computed {
            text.push(']');
        }
        let range = self.get_node_range(access);
        self.report_chain_with_fixes(
            access,
            vec![Fix { text, range }],
            !self.opts.unsafe_fixes,
            true,
        );
    }
}
fn insert_length(lengths: &mut Vec<usize>, length: usize) {
    if !lengths.contains(&length) {
        lengths.push(length);
    }
}
fn are_nullish_checks_consistent(a: OperandType, b: OperandType) -> bool {
    if a.is_loose_nullish_check() || b.is_loose_nullish_check() {
        return true;
    }
    let an = matches!(a, NotStrictEqualNull | StrictEqualNull);
    let au = matches!(a, NotStrictEqualUndef | StrictEqualUndef | TypeofCheck);
    let bn = matches!(b, NotStrictEqualNull | StrictEqualNull);
    let bu = matches!(b, NotStrictEqualUndef | StrictEqualUndef | TypeofCheck);
    if an && bu || au && bn {
        return true;
    }
    an && bn || au && bu
}
fn is_split_strict_equals_pattern(chain: &[Operand]) -> bool {
    if chain.len() != 2 {
        return false;
    }
    let a = chain[0].typ;
    let b = chain[1].typ;
    matches!(a, NotStrictEqualUndef | TypeofCheck) && b == NotStrictEqualNull
        || a == NotStrictEqualNull && matches!(b, NotStrictEqualUndef | TypeofCheck)
}
