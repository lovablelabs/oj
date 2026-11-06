use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use tsrs_core::{P, ScriptKind};

use crate::*;

// Atomic ids

static NEXT_NODE_ID: AtomicU64 = AtomicU64::new(0);
static NEXT_SYMBOL_ID: AtomicU64 = AtomicU64::new(0);
static NEXT_NODE_BLOCK_ID: AtomicU64 = AtomicU64::new(0);
static NEXT_SYMBOL_BLOCK_ID: AtomicU64 = AtomicU64::new(0);

/// Go reserves this part of the 64-bit id space for ids assigned by the checker's dense link stores.
pub const BLOCK_ID_OFFSET: u64 = 0x1_0000_0000_0000;
const BLOCK_ID_SIZE: u32 = tsrs_core::LINK_PAGE_SIZE as u32;

// Nodes and symbols keep a 32-bit id in the AST. The high bit tags a block id; the remaining bits are its
// offset from BLOCK_ID_OFFSET. This preserves the Go-visible 64-bit id without enlarging every AST object.
const BLOCK_ID_TAG: u32 = 1 << 31;

#[inline]
fn decode_id(stored: u32) -> u64 {
    if stored & BLOCK_ID_TAG != 0 {
        BLOCK_ID_OFFSET + u64::from(stored & !BLOCK_ID_TAG)
    } else {
        u64::from(stored)
    }
}

#[derive(Default)]
pub struct NodeIdGenerator {
    block: Cell<(u32, u32)>,
}

#[derive(Default)]
pub struct SymbolIdGenerator {
    block: Cell<(u32, u32)>,
}

#[inline]
fn next_block_id(counter: &AtomicU64, block: &Cell<(u32, u32)>) -> u32 {
    let (mut next, mut last) = block.get();
    if next == last {
        // Relaxed: the counter only reserves disjoint id ranges, as for ordinary ids below.
        let first = counter.fetch_add(u64::from(BLOCK_ID_SIZE), Ordering::Relaxed);
        next = u32::try_from(first).expect("more than 2^31 block ids");
        last = next.checked_add(BLOCK_ID_SIZE).expect("more than 2^31 block ids");
        assert!(last <= BLOCK_ID_TAG, "more than 2^31 block ids");
    }
    block.set((next + 1, last));
    BLOCK_ID_TAG | next
}

impl NodeIdGenerator {
    #[inline]
    pub fn get_node_id(&self, node: P<Node>) -> NodeId {
        // Relaxed: an id is immutable once assigned and publishes no other node state.
        let id = node.id.load(Ordering::Relaxed);
        if id != 0 {
            return NodeId(decode_id(id));
        }
        let candidate = next_block_id(&NEXT_NODE_BLOCK_ID, &self.block);
        // Relaxed: competing generators only choose the node's one immutable id.
        let id = match node.id.compare_exchange(0, candidate, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => candidate,
            Err(winner) => winner,
        };
        NodeId(decode_id(id))
    }
}

impl SymbolIdGenerator {
    #[inline]
    pub fn get_symbol_id(&self, symbol: P<Symbol>) -> SymbolId {
        // Relaxed: an id is immutable once assigned and publishes no other symbol state.
        let id = symbol.id.load(Ordering::Relaxed);
        if id != 0 {
            return SymbolId(decode_id(id));
        }
        let candidate = next_block_id(&NEXT_SYMBOL_BLOCK_ID, &self.block);
        // Relaxed: competing generators only choose the symbol's one immutable id.
        let id =
            match symbol.id.compare_exchange(0, candidate, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => candidate,
                Err(winner) => winner,
            };
        SymbolId(decode_id(id))
    }
}

/// Ids a thread in id-block mode (`use_id_blocks`) takes from a counter at a time.
const ID_BLOCK: u64 = 1024;

thread_local! {
    /// Whether this thread takes ids in blocks (`use_id_blocks`).
    static ID_BLOCK_MODE: Cell<bool> = const { Cell::new(false) };
    /// Id-block mode: the rest of the thread's current block of node / symbol ids, `(next, end)`.
    static NODE_ID_BLOCK: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
    static SYMBOL_ID_BLOCK: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
}

/// Makes the current thread take node and symbol ids from the process-wide counters in blocks of `ID_BLOCK`.
///
/// For the threads of a parallel checker group: their ids interleave in an order that depends on timing
/// anyway (as Go's do), and one counter that every checker increments is a contended cache line. Each
/// thread's ids still increase in the order it assigns them, and blocks are taken only after the group
/// started, so ids assigned before, during and after the group keep that order; other threads take ids one at
/// a time, exactly as before.
pub fn use_id_blocks() {
    ID_BLOCK_MODE.set(true);
}

/// Ends `use_id_blocks` on the current thread and drops the rest of its blocks, as the thread's exit would: a reused
/// checker thread must not hand out ids from a block it took in an earlier group.
pub fn stop_id_blocks() {
    ID_BLOCK_MODE.set(false);
    NODE_ID_BLOCK.set((0, 0));
    SYMBOL_ID_BLOCK.set((0, 0));
}

#[inline]
fn next_id(counter: &AtomicU64, block: &'static std::thread::LocalKey<Cell<(u64, u64)>>) -> u64 {
    if !ID_BLOCK_MODE.get() {
        // Relaxed: the counter only hands out distinct numbers; nothing is published through it.
        return counter.fetch_add(1, Ordering::Relaxed) + 1;
    }
    block.with(|b| {
        let (next, end) = b.get();
        if next < end {
            b.set((next + 1, end));
            return next;
        }
        // Relaxed, as above: blocks only need to be disjoint.
        let start = counter.fetch_add(ID_BLOCK, Ordering::Relaxed) + 1;
        b.set((start + 1, start + ID_BLOCK));
        start
    })
}

#[inline]
pub fn get_node_id(node: P<Node>) -> NodeId {
    // Relaxed: the id is immutable and does not publish any other node state.
    let id = node.id.load(Ordering::Relaxed);
    if id != 0 {
        return NodeId(decode_id(id));
    }
    assign_node_id(node)
}

/// The node's id if it has one; unlike `get_node_id`, never assigns one.
#[inline]
pub fn get_assigned_node_id(node: P<Node>) -> Option<NodeId> {
    // Relaxed, as in `get_node_id`: the id is the only data read, and it is written once.
    let id = node.id.load(Ordering::Relaxed);
    (id != 0).then(|| NodeId(decode_id(id)))
}

#[inline(never)]
fn assign_node_id(node: P<Node>) -> NodeId {
    // Worst case, we burn a few ids if we have to CAS.
    let next = next_id(&NEXT_NODE_ID, &NODE_ID_BLOCK);
    // The high bit tags Go's reserved block range; ordinary ids use the lower 31 bits.
    let mut id = u32::try_from(next).expect("more than 2^31 ordinary node ids");
    assert!(id < BLOCK_ID_TAG, "more than 2^31 ordinary node ids");
    // Relaxed: the id is the only data published, and it is written once.
    if node.id.compare_exchange(0, id, Ordering::Relaxed, Ordering::Relaxed).is_err() {
        // Relaxed: only the winning id is needed.
        id = node.id.load(Ordering::Relaxed);
    }
    NodeId(decode_id(id))
}

#[inline]
pub fn get_symbol_id(symbol: P<Symbol>) -> SymbolId {
    // Relaxed: the id is immutable and does not publish any other symbol state.
    let id = symbol.id.load(Ordering::Relaxed);
    if id != 0 {
        return SymbolId(decode_id(id));
    }
    assign_symbol_id(symbol)
}

/// The symbol's id if it has one; unlike `get_symbol_id`, never assigns one.
#[inline]
pub fn get_assigned_symbol_id(symbol: P<Symbol>) -> Option<SymbolId> {
    let id = symbol.id.load(Ordering::Relaxed);
    (id != 0).then(|| SymbolId(decode_id(id)))
}

#[inline(never)]
fn assign_symbol_id(symbol: P<Symbol>) -> SymbolId {
    // Worst case, we burn a few ids if we have to CAS.
    let next = next_id(&NEXT_SYMBOL_ID, &SYMBOL_ID_BLOCK);
    // The high bit tags Go's reserved block range; ordinary ids use the lower 31 bits.
    let mut id = u32::try_from(next).expect("more than 2^31 ordinary symbol ids");
    assert!(id < BLOCK_ID_TAG, "more than 2^31 ordinary symbol ids");
    // Relaxed: the id is the only data published, and it is written once.
    if symbol.id.compare_exchange(0, id, Ordering::Relaxed, Ordering::Relaxed).is_err() {
        // Relaxed: only the winning id is needed.
        id = symbol.id.load(Ordering::Relaxed);
    }
    SymbolId(decode_id(id))
}

pub fn get_symbol_table(data: &Cell<Option<P<SymbolTable>>>) -> P<SymbolTable> {
    if let Some(table) = data.get() {
        return table;
    }
    let table = P::new(SymbolTable::default());
    data.set(Some(table));
    table
}

pub fn get_members(symbol: P<Symbol>) -> P<SymbolTable> {
    if let Some(table) = symbol.members() {
        return table;
    }
    let table = P::new(SymbolTable::default());
    symbol.set_members(Some(table));
    table
}

pub fn get_exports(symbol: P<Symbol>) -> P<SymbolTable> {
    if let Some(table) = symbol.exports() {
        return table;
    }
    let table = P::new(SymbolTable::default());
    symbol.set_exports(Some(table));
    table
}

pub fn get_locals(container: P<Node>) -> P<SymbolTable> {
    get_symbol_table(&container.locals_container_data().unwrap().locals)
}

// Determines if a node is missing (either `nil` or empty)
pub fn node_is_missing(node: impl Into<Option<P<Node>>>) -> bool {
    match node.into() {
        None => true,
        Some(node) => {
            node.pos() == node.end() && !node.pos().is_synthetic() && node.kind() != Kind::EndOfFile
        }
    }
}

// Determines if a node is present
pub fn node_is_present(node: impl Into<Option<P<Node>>>) -> bool {
    !node_is_missing(node)
}

// Determines if a node contains synthetic positions
pub fn node_is_synthesized(node: P<Node>) -> bool {
    position_is_synthesized(node.pos()) || position_is_synthesized(node.end())
}

// Determines whether a position is synthetic
pub fn position_is_synthesized(pos: tsrs_core::TextPos) -> bool {
    pos.is_synthetic()
}

pub fn node_kind_is(node: P<Node>, kinds: &[Kind]) -> bool {
    kinds.contains(&node.kind())
}

pub fn is_modifier(node: P<Node>) -> bool {
    is_modifier_kind(node.kind())
}

// utilities.go:108
pub fn is_modifier_like(node: P<Node>) -> bool {
    is_modifier(node) || is_decorator(node)
}

pub fn is_compound_assignment(token: Kind) -> bool {
    token >= Kind::FIRST_COMPOUND_ASSIGNMENT && token <= Kind::LAST_COMPOUND_ASSIGNMENT
}

pub fn is_assignment_expression(node: P<Node>, exclude_compound_assignment: bool) -> bool {
    if node.kind() == Kind::BinaryExpression {
        let expr = node.as_binary_expression();
        return (expr.operator_token.kind() == Kind::EqualsToken
            || !exclude_compound_assignment && is_assignment_operator(expr.operator_token.kind()))
            && is_left_hand_side_expression(expr.left);
    }
    false
}

pub fn get_right_most_assigned_expression(mut node: P<Node>) -> P<Node> {
    while is_assignment_expression(node, false /*excludeCompoundAssignment*/) {
        node = node.as_binary_expression().right();
    }
    node
}

pub fn is_destructuring_assignment(node: P<Node>) -> bool {
    if is_assignment_expression(node, true /*excludeCompoundAssignment*/) {
        let kind = node.as_binary_expression().left.kind();
        return kind == Kind::ObjectLiteralExpression || kind == Kind::ArrayLiteralExpression;
    }
    false
}

// utilities.go:151
pub fn is_array_binding_or_assignment_element(node: P<Node>) -> bool {
    match node.kind() {
        Kind::BindingElement
        | Kind::OmittedExpression
        | Kind::SpreadElement
        | Kind::ArrayLiteralExpression
        | Kind::ObjectLiteralExpression
        | Kind::Identifier
        | Kind::PropertyAccessExpression
        | Kind::ElementAccessExpression => return true,
        _ => {}
    }
    is_assignment_expression(node, true /*excludeCompoundAssignment*/)
}

pub fn is_binding_pattern(node: P<Node>) -> bool {
    node.kind() == Kind::ObjectBindingPattern || node.kind() == Kind::ArrayBindingPattern
}

pub fn is_for_in_or_of_statement(node: impl Into<Option<P<Node>>>) -> bool {
    match node.into() {
        Some(node) => node.kind() == Kind::ForInStatement || node.kind() == Kind::ForOfStatement,
        None => false,
    }
}

// A node is an assignment target if it is on the left hand side of an '=' token, if it is parented by a property
// assignment in an object literal that is an assignment target, or if it is parented by an array literal that is
// an assignment target. Examples include 'a = xxx', '{ p: a } = xxx', '[{ a }] = xxx'.
// (Note that `p` is not a target in the above examples, only `a`.)
pub fn is_assignment_target(node: P<Node>) -> bool {
    get_assignment_target(node).is_some()
}

// Returns the BinaryExpression, PrefixUnaryExpression, PostfixUnaryExpression, or ForInOrOfStatement that references
// the given node as an assignment target
pub fn get_assignment_target(mut node: P<Node>) -> Option<P<Node>> {
    loop {
        let parent = node.parent().unwrap();
        match parent.kind() {
            Kind::BinaryExpression => {
                if is_assignment_operator(parent.as_binary_expression().operator_token.kind())
                    && parent.as_binary_expression().left == node
                {
                    return Some(parent);
                }
                return None;
            }
            Kind::PrefixUnaryExpression => {
                let operator = parent.as_prefix_unary_expression().operator;
                if operator == Kind::PlusPlusToken || operator == Kind::MinusMinusToken {
                    return Some(parent);
                }
                return None;
            }
            Kind::PostfixUnaryExpression => {
                let operator = parent.as_postfix_unary_expression().operator;
                if operator == Kind::PlusPlusToken || operator == Kind::MinusMinusToken {
                    return Some(parent);
                }
                return None;
            }
            Kind::ForInStatement | Kind::ForOfStatement => {
                if parent.initializer() == Some(node) {
                    return Some(parent);
                }
                return None;
            }
            Kind::ParenthesizedExpression
            | Kind::ArrayLiteralExpression
            | Kind::SpreadElement
            | Kind::NonNullExpression => {
                node = parent;
            }
            Kind::SpreadAssignment => {
                node = parent.parent().unwrap();
            }
            Kind::ShorthandPropertyAssignment => {
                if parent.name() != Some(node) {
                    return None;
                }
                node = parent.parent().unwrap();
            }
            Kind::PropertyAssignment => {
                if parent.name() == Some(node) {
                    return None;
                }
                node = parent.parent().unwrap();
            }
            _ => return None,
        }
    }
}

pub fn is_logical_binary_operator(token: Kind) -> bool {
    token == Kind::BarBarToken || token == Kind::AmpersandAmpersandToken
}

pub fn is_logical_or_coalescing_binary_operator(token: Kind) -> bool {
    is_logical_binary_operator(token) || token == Kind::QuestionQuestionToken
}

pub fn is_logical_or_coalescing_binary_expression(expr: P<Node>) -> bool {
    is_binary_expression(expr)
        && is_logical_or_coalescing_binary_operator(
            expr.as_binary_expression().operator_token.kind(),
        )
}

pub fn is_logical_or_coalescing_assignment_expression(expr: P<Node>) -> bool {
    is_binary_expression(expr)
        && is_logical_or_coalescing_assignment_operator(
            expr.as_binary_expression().operator_token.kind(),
        )
}

pub fn is_logical_expression(mut node: P<Node>) -> bool {
    loop {
        if node.kind() == Kind::ParenthesizedExpression {
            node = node.expression().unwrap();
        } else if node.kind() == Kind::PrefixUnaryExpression
            && node.as_prefix_unary_expression().operator == Kind::ExclamationToken
        {
            node = node.as_prefix_unary_expression().operand;
        } else {
            return is_logical_or_coalescing_binary_expression(node);
        }
    }
}

pub fn is_accessor(node: P<Node>) -> bool {
    node.kind() == Kind::GetAccessor || node.kind() == Kind::SetAccessor
}

pub fn is_property_name_literal(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::Identifier
            | Kind::StringLiteral
            | Kind::NoSubstitutionTemplateLiteral
            | Kind::NumericLiteral
    )
}

pub fn is_member_name(node: P<Node>) -> bool {
    node.kind() == Kind::Identifier || node.kind() == Kind::PrivateIdentifier
}

pub fn is_entity_name(node: P<Node>) -> bool {
    node.kind() == Kind::Identifier || node.kind() == Kind::QualifiedName
}

pub fn is_property_name(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::Identifier
            | Kind::PrivateIdentifier
            | Kind::StringLiteral
            | Kind::NumericLiteral
            | Kind::ComputedPropertyName
    )
}

// Return true if the given identifier is classified as an IdentifierName by inspecting the parent of the node
pub fn is_identifier_name(node: P<Node>) -> bool {
    let parent = node.parent().unwrap();
    match parent.kind() {
        Kind::PropertyDeclaration
        | Kind::PropertySignature
        | Kind::MethodDeclaration
        | Kind::MethodSignature
        | Kind::GetAccessor
        | Kind::SetAccessor
        | Kind::EnumMember
        | Kind::PropertyAssignment
        | Kind::PropertyAccessExpression => parent.name() == Some(node),
        Kind::QualifiedName => parent.as_qualified_name().right == node,
        Kind::BindingElement | Kind::ImportSpecifier => parent.property_name() == Some(node),
        Kind::ExportSpecifier
        | Kind::JsxAttribute
        | Kind::JsxSelfClosingElement
        | Kind::JsxOpeningElement
        | Kind::JsxClosingElement => true,
        _ => false,
    }
}

pub fn is_push_or_unshift_identifier(node: P<Node>) -> bool {
    let text = node.text();
    text == "push" || text == "unshift"
}

pub fn is_boolean_literal(node: P<Node>) -> bool {
    node.kind() == Kind::TrueKeyword || node.kind() == Kind::FalseKeyword
}

pub fn is_literal_expression(node: P<Node>) -> bool {
    is_literal_kind(node.kind())
}

pub fn is_string_literal_like(node: P<Node>) -> bool {
    matches!(node.kind(), Kind::StringLiteral | Kind::NoSubstitutionTemplateLiteral)
}

pub fn is_string_or_numeric_literal_like(node: P<Node>) -> bool {
    is_string_literal_like(node) || is_numeric_literal(node)
}

pub fn is_signed_numeric_literal(node: P<Node>) -> bool {
    if node.kind() == Kind::PrefixUnaryExpression {
        let node = node.as_prefix_unary_expression();
        return (node.operator == Kind::PlusToken || node.operator == Kind::MinusToken)
            && is_numeric_literal(node.operand);
    }
    false
}

// Determines if a node is part of an OptionalChain
pub fn is_optional_chain(node: P<Node>) -> bool {
    if node.flags().intersects(NodeFlags::OptionalChain) {
        return matches!(
            node.kind(),
            Kind::PropertyAccessExpression
                | Kind::ElementAccessExpression
                | Kind::CallExpression
                | Kind::NonNullExpression
        );
    }
    false
}

fn get_question_dot_token(node: P<Node>) -> Option<P<Node>> {
    node.question_dot_token()
}

// Determines if node is the root expression of an OptionalChain
pub fn is_optional_chain_root(node: P<Node>) -> bool {
    is_optional_chain(node)
        && !is_non_null_expression(node)
        && get_question_dot_token(node).is_some()
}

// Determines whether a node is the outermost `OptionalChain` in an ECMAScript `OptionalExpression`:
//
//  1. For `a?.b.c`, the outermost chain is `a?.b.c` (`c` is the end of the chain starting at `a?.`)
//  2. For `a?.b!`, the outermost chain is `a?.b` (`b` is the end of the chain starting at `a?.`)
//  3. For `(a?.b.c).d`, the outermost chain is `a?.b.c` (`c` is the end of the chain starting at `a?.` since parens end the chain)
//  4. For `a?.b.c?.d`, both `a?.b.c` and `a?.b.c?.d` are outermost (`c` is the end of the chain starting at `a?.`, and `d` is
//     the end of the chain starting at `c?.`)
//  5. For `a?.(b?.c).d`, both `b?.c` and `a?.(b?.c)d` are outermost (`c` is the end of the chain starting at `b`, and `d` is
//     the end of the chain starting at `a?.`)
pub fn is_outermost_optional_chain(node: P<Node>) -> bool {
    let parent = node.parent().unwrap();
    !is_optional_chain(parent) || // cases 1, 2, and 3
        is_optional_chain_root(parent) || // case 4
        Some(node) != parent.expression() // case 5
}

// Determines whether a node is the expression preceding an optional chain (i.e. `a` in `a?.b`).
pub fn is_expression_of_optional_chain_root(node: P<Node>) -> bool {
    let parent = node.parent().unwrap();
    is_optional_chain_root(parent) && parent.expression() == Some(node)
}

pub fn is_nullish_coalesce(node: P<Node>) -> bool {
    node.kind() == Kind::BinaryExpression
        && node.as_binary_expression().operator_token.kind() == Kind::QuestionQuestionToken
}

pub fn is_assertion_expression(node: P<Node>) -> bool {
    let kind = node.kind();
    kind == Kind::TypeAssertionExpression || kind == Kind::AsExpression
}

fn is_left_hand_side_expression_kind(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::PropertyAccessExpression
            | Kind::ElementAccessExpression
            | Kind::NewExpression
            | Kind::CallExpression
            | Kind::JsxElement
            | Kind::JsxSelfClosingElement
            | Kind::JsxFragment
            | Kind::TaggedTemplateExpression
            | Kind::ArrayLiteralExpression
            | Kind::ParenthesizedExpression
            | Kind::ObjectLiteralExpression
            | Kind::ClassExpression
            | Kind::FunctionExpression
            | Kind::Identifier
            | Kind::PrivateIdentifier
            | Kind::RegularExpressionLiteral
            | Kind::NumericLiteral
            | Kind::BigIntLiteral
            | Kind::StringLiteral
            | Kind::NoSubstitutionTemplateLiteral
            | Kind::TemplateExpression
            | Kind::FalseKeyword
            | Kind::NullKeyword
            | Kind::ThisKeyword
            | Kind::TrueKeyword
            | Kind::SuperKeyword
            | Kind::NonNullExpression
            | Kind::ExpressionWithTypeArguments
            | Kind::MetaProperty
            | Kind::ImportKeyword
            | Kind::MissingDeclaration
    )
}

// Determines whether a node is a LeftHandSideExpression based only on its kind.
pub fn is_left_hand_side_expression(node: P<Node>) -> bool {
    is_left_hand_side_expression_kind(skip_partially_emitted_expressions(node).kind())
}

fn is_unary_expression_kind(kind: Kind) -> bool {
    match kind {
        Kind::PrefixUnaryExpression
        | Kind::PostfixUnaryExpression
        | Kind::DeleteExpression
        | Kind::TypeOfExpression
        | Kind::VoidExpression
        | Kind::AwaitExpression
        | Kind::TypeAssertionExpression => true,
        _ => is_left_hand_side_expression_kind(kind),
    }
}

fn is_expression_kind(kind: Kind) -> bool {
    match kind {
        Kind::ConditionalExpression
        | Kind::YieldExpression
        | Kind::ArrowFunction
        | Kind::BinaryExpression
        | Kind::SpreadElement
        | Kind::AsExpression
        | Kind::OmittedExpression
        | Kind::PartiallyEmittedExpression
        | Kind::SatisfiesExpression => true,
        _ => is_unary_expression_kind(kind),
    }
}

// Determines whether a node is an expression based only on its kind.
pub fn is_expression(node: P<Node>) -> bool {
    is_expression_kind(skip_partially_emitted_expressions(node).kind())
}

pub fn is_comma_expression(node: P<Node>) -> bool {
    node.kind() == Kind::BinaryExpression
        && node.as_binary_expression().operator_token.kind() == Kind::CommaToken
}

pub fn is_comma_sequence(node: P<Node>) -> bool {
    is_comma_expression(node)
}

pub fn is_iteration_statement(node: P<Node>, look_in_labeled_statements: bool) -> bool {
    match node.kind() {
        Kind::ForStatement
        | Kind::ForInStatement
        | Kind::ForOfStatement
        | Kind::DoStatement
        | Kind::WhileStatement => true,
        Kind::LabeledStatement => {
            look_in_labeled_statements
                && is_iteration_statement(node.statement(), look_in_labeled_statements)
        }
        _ => false,
    }
}

// Determines if a node is a property or element access expression
pub fn is_access_expression(node: P<Node>) -> bool {
    node.kind() == Kind::PropertyAccessExpression || node.kind() == Kind::ElementAccessExpression
}

fn is_function_like_declaration_kind(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::FunctionDeclaration
            | Kind::MethodDeclaration
            | Kind::Constructor
            | Kind::GetAccessor
            | Kind::SetAccessor
            | Kind::FunctionExpression
            | Kind::ArrowFunction
    )
}

// Determines if a node is function-like (but is not a signature declaration)
pub fn is_function_like_declaration(node: impl Into<Option<P<Node>>>) -> bool {
    // TODO(rbuckton): Move `node != nil` test to call sites
    match node.into() {
        Some(node) => is_function_like_declaration_kind(node.kind()),
        None => false,
    }
}

pub fn is_function_like_kind(kind: Kind) -> bool {
    match kind {
        Kind::MethodSignature
        | Kind::CallSignature
        | Kind::JSDocSignature
        | Kind::ConstructSignature
        | Kind::IndexSignature
        | Kind::FunctionType
        | Kind::ConstructorType => true,
        _ => is_function_like_declaration_kind(kind),
    }
}

// Determines if a node is function- or signature-like.
pub fn is_function_like(node: impl Into<Option<P<Node>>>) -> bool {
    // TODO(rbuckton): Move `node != nil` test to call sites
    match node.into() {
        Some(node) => is_function_like_kind(node.kind()),
        None => false,
    }
}

pub fn is_function_like_or_class_static_block_declaration(
    node: impl Into<Option<P<Node>>>,
) -> bool {
    match node.into() {
        Some(node) => is_function_like(node) || is_class_static_block_declaration(node),
        None => false,
    }
}

pub fn is_function_or_source_file(node: P<Node>) -> bool {
    is_function_like(node) || is_source_file(node)
}

pub fn is_class_like(node: P<Node>) -> bool {
    node.kind() == Kind::ClassDeclaration || node.kind() == Kind::ClassExpression
}

pub fn is_class_or_interface_like(node: P<Node>) -> bool {
    node.kind() == Kind::ClassDeclaration
        || node.kind() == Kind::ClassExpression
        || node.kind() == Kind::InterfaceDeclaration
}

pub fn is_class_element(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::Constructor
            | Kind::PropertyDeclaration
            | Kind::MethodDeclaration
            | Kind::GetAccessor
            | Kind::SetAccessor
            | Kind::IndexSignature
            | Kind::ClassStaticBlockDeclaration
            | Kind::SemicolonClassElement
    )
}

pub fn is_method_or_accessor(node: P<Node>) -> bool {
    matches!(node.kind(), Kind::MethodDeclaration | Kind::GetAccessor | Kind::SetAccessor)
}

pub fn is_private_identifier_class_element_declaration(node: P<Node>) -> bool {
    (is_property_declaration(node) || is_method_or_accessor(node))
        && is_private_identifier(node.name().unwrap())
}

pub fn is_object_literal_or_class_expression_method_or_accessor(node: P<Node>) -> bool {
    let kind = node.kind();
    (kind == Kind::MethodDeclaration || kind == Kind::GetAccessor || kind == Kind::SetAccessor)
        && (node.parent().unwrap().kind() == Kind::ObjectLiteralExpression
            || node.parent().unwrap().kind() == Kind::ClassExpression)
}

pub fn is_object_literal_element(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::PropertyAssignment
            | Kind::ShorthandPropertyAssignment
            | Kind::SpreadAssignment
            | Kind::MethodDeclaration
            | Kind::GetAccessor
            | Kind::SetAccessor
    )
}

pub fn is_object_literal_method(node: impl Into<Option<P<Node>>>) -> bool {
    match node.into() {
        Some(node) => {
            node.kind() == Kind::MethodDeclaration
                && node.parent().unwrap().kind() == Kind::ObjectLiteralExpression
        }
        None => false,
    }
}

pub fn is_auto_accessor_property_declaration(node: P<Node>) -> bool {
    is_property_declaration(node) && has_accessor_modifier(node)
}

pub fn is_parameter_property_declaration(node: P<Node>, parent: P<Node>) -> bool {
    is_parameter_declaration(node)
        && has_syntactic_modifier(node, ModifierFlags::ParameterPropertyModifier)
        && parent.kind() == Kind::Constructor
}

pub fn is_type_element(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::ConstructSignature
            | Kind::CallSignature
            | Kind::PropertySignature
            | Kind::MethodSignature
            | Kind::IndexSignature
            | Kind::GetAccessor
            | Kind::SetAccessor
            | Kind::NotEmittedTypeElement
    )
}

pub fn is_jsx_child(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::JsxElement
            | Kind::JsxExpression
            | Kind::JsxSelfClosingElement
            | Kind::JsxText
            | Kind::JsxFragment
    )
}

pub fn is_jsx_attribute_like(node: P<Node>) -> bool {
    is_jsx_attribute(node) || is_jsx_spread_attribute(node)
}

fn is_declaration_statement_kind(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::FunctionDeclaration
            | Kind::MissingDeclaration
            | Kind::ClassDeclaration
            | Kind::InterfaceDeclaration
            | Kind::TypeAliasDeclaration
            | Kind::JSTypeAliasDeclaration
            | Kind::EnumDeclaration
            | Kind::ModuleDeclaration
            | Kind::ImportDeclaration
            | Kind::JSImportDeclaration
            | Kind::ImportEqualsDeclaration
            | Kind::ExportDeclaration
            | Kind::ExportAssignment
            | Kind::NamespaceExportDeclaration
    )
}

// Determines whether a node is a DeclarationStatement. Ideally this does not use Parent pointers, but it may use them
// to rule out a Block node that is part of `try` or `catch` or is the Block-like body of a function.
//
// NOTE: ECMA262 would just call this a Declaration
pub fn is_declaration_statement(node: P<Node>) -> bool {
    is_declaration_statement_kind(node.kind())
}

pub(crate) fn is_statement_kind_but_not_declaration_kind(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::BreakStatement
            | Kind::ContinueStatement
            | Kind::DebuggerStatement
            | Kind::DoStatement
            | Kind::ExpressionStatement
            | Kind::EmptyStatement
            | Kind::ForInStatement
            | Kind::ForOfStatement
            | Kind::ForStatement
            | Kind::IfStatement
            | Kind::LabeledStatement
            | Kind::ReturnStatement
            | Kind::SwitchStatement
            | Kind::ThrowStatement
            | Kind::TryStatement
            | Kind::VariableStatement
            | Kind::WhileStatement
            | Kind::WithStatement
            | Kind::NotEmittedStatement
    )
}

// Determines whether a node is a Statement. Ideally this does not use Parent pointers, but it may use
// them to rule out a Block node that is part of `try` or `catch` or is the Block-like body of a function.
//
// NOTE: ECMA262 would call this either a StatementListItem or ModuleListItem
pub fn is_statement(node: P<Node>) -> bool {
    let kind = node.kind();
    is_statement_kind_but_not_declaration_kind(kind)
        || is_declaration_statement_kind(kind)
        || is_block_statement(node)
}

// Determines whether a node is a BlockStatement. If parents are available, this ensures the Block is
// not part of a `try` statement, `catch` clause, or the Block-like body of a function
fn is_block_statement(node: P<Node>) -> bool {
    if node.kind() != Kind::Block {
        return false;
    }
    if let Some(parent) = node.parent()
        && (parent.kind() == Kind::TryStatement || parent.kind() == Kind::CatchClause)
    {
        return false;
    }
    !is_function_block(node)
}

// Determines whether a node is the Block-like body of a function by walking the parent of the node
pub fn is_function_block(node: impl Into<Option<P<Node>>>) -> bool {
    match node.into() {
        Some(node) => {
            node.kind() == Kind::Block && node.parent().is_some() && is_function_like(node.parent())
        }
        None => false,
    }
}

pub fn is_block_or_catch_scoped(declaration: P<Node>) -> bool {
    get_combined_node_flags(declaration).intersects(NodeFlags::BlockScoped)
        || is_catch_clause_variable_declaration_or_binding_element(declaration)
}

pub fn is_catch_clause_variable_declaration_or_binding_element(declaration: P<Node>) -> bool {
    let node = get_root_declaration(declaration);
    node.kind() == Kind::VariableDeclaration && node.parent().unwrap().kind() == Kind::CatchClause
}

pub fn is_type_node_kind(kind: Kind) -> bool {
    match kind {
        Kind::AnyKeyword
        | Kind::UnknownKeyword
        | Kind::NumberKeyword
        | Kind::BigIntKeyword
        | Kind::ObjectKeyword
        | Kind::BooleanKeyword
        | Kind::StringKeyword
        | Kind::SymbolKeyword
        | Kind::VoidKeyword
        | Kind::UndefinedKeyword
        | Kind::NeverKeyword
        | Kind::IntrinsicKeyword
        | Kind::ExpressionWithTypeArguments
        | Kind::JSDocAllType
        | Kind::JSDocNullableType
        | Kind::JSDocNonNullableType
        | Kind::JSDocOptionalType
        | Kind::JSDocVariadicType => true,
        _ => kind >= Kind::FIRST_TYPE_NODE && kind <= Kind::LAST_TYPE_NODE,
    }
}

pub fn is_type_node(node: P<Node>) -> bool {
    is_type_node_kind(node.kind())
}

pub fn is_jsdoc_kind(kind: Kind) -> bool {
    Kind::FIRST_JSDOC_NODE <= kind && kind <= Kind::LAST_JSDOC_NODE
}

pub fn is_jsdoc_type_assertion(node: impl Into<Option<P<Node>>>) -> bool {
    let Some(node) = node.into() else {
        return false;
    };
    if !is_parenthesized_expression(node) || !is_in_js_file(node) {
        return false;
    }
    let expr = node.expression().unwrap();
    is_as_expression(expr)
        && expr.type_node().is_some_and(|t| t.flags().intersects(NodeFlags::Reparsed))
}

pub fn is_prologue_directive(node: P<Node>) -> bool {
    node.kind() == Kind::ExpressionStatement
        && node.expression().unwrap().kind() == Kind::StringLiteral
}

bitflags::bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct OuterExpressionKinds: u16 {
        const Parentheses = 1 << 0;
        const TypeAssertions = 1 << 1;
        const NonNullAssertions = 1 << 2;
        const PartiallyEmittedExpressions = 1 << 3;
        const ExpressionsWithTypeArguments = 1 << 4;
        const Satisfies = 1 << 5;
        const ExcludeJSDocTypeAssertion = 1 << 6;
        const Assignments = 1 << 7;
        const Comma = 1 << 8;
        const Assertions = Self::TypeAssertions.bits() | Self::NonNullAssertions.bits() | Self::Satisfies.bits();
        const All = Self::Parentheses.bits() | Self::Assertions.bits() | Self::PartiallyEmittedExpressions.bits() | Self::ExpressionsWithTypeArguments.bits();
        const AllExceptAssertionsOrExpressionsWithTypeArguments = Self::All.bits() & !Self::Assertions.bits() & !Self::ExpressionsWithTypeArguments.bits();
        const ExpressionTypePassthrough = Self::Parentheses.bits() | Self::Assignments.bits() | Self::Comma.bits();
    }
}

// Go names these `OEK*`; aliases keep call sites recognizable.
#[expect(
    clippy::upper_case_acronyms,
    reason = "The name matches the upstream Go or protocol vocabulary."
)]
pub type OEK = OuterExpressionKinds;

// Determines whether node is an "outer expression" of the provided kinds
pub fn is_outer_expression(node: P<Node>, kinds: OuterExpressionKinds) -> bool {
    match node.kind() {
        Kind::ParenthesizedExpression => {
            kinds.intersects(OEK::Parentheses)
                && !(kinds.intersects(OEK::ExcludeJSDocTypeAssertion)
                    && is_jsdoc_type_assertion(node))
        }
        Kind::TypeAssertionExpression | Kind::AsExpression => kinds.intersects(OEK::TypeAssertions),
        Kind::SatisfiesExpression => {
            kinds.intersects(OEK::ExpressionsWithTypeArguments | OEK::Satisfies)
        }
        Kind::ExpressionWithTypeArguments => kinds.intersects(OEK::ExpressionsWithTypeArguments),
        Kind::NonNullExpression => kinds.intersects(OEK::NonNullAssertions),
        Kind::PartiallyEmittedExpression => kinds.intersects(OEK::PartiallyEmittedExpressions),
        Kind::BinaryExpression => match node.as_binary_expression().operator_token.kind() {
            Kind::EqualsToken => kinds.intersects(OEK::Assignments),
            Kind::CommaToken => kinds.intersects(OEK::Comma),
            _ => false,
        },
        _ => false,
    }
}

// Descends into an expression, skipping past "outer expressions" of the provided kinds
pub fn skip_outer_expressions(mut node: P<Node>, kinds: OuterExpressionKinds) -> P<Node> {
    while is_outer_expression(node, kinds) {
        if is_binary_expression(node) {
            node = node.as_binary_expression().right();
        } else {
            node = node.expression().unwrap();
        }
    }
    node
}

// Skips past the parentheses of an expression
pub fn skip_parentheses(node: P<Node>) -> P<Node> {
    skip_outer_expressions(node, OEK::Parentheses)
}

pub fn skip_type_parentheses(mut node: P<Node>) -> P<Node> {
    while is_parenthesized_type_node(node) {
        node = node.type_node().unwrap();
    }
    node
}

pub fn skip_partially_emitted_expressions(node: P<Node>) -> P<Node> {
    skip_outer_expressions(node, OEK::PartiallyEmittedExpressions)
}

// Walks up the parents of a parenthesized expression to find the containing node
pub fn walk_up_parenthesized_expressions(node: impl Into<Option<P<Node>>>) -> Option<P<Node>> {
    let mut node = node.into();
    while let Some(n) = node {
        if n.kind() != Kind::ParenthesizedExpression {
            break;
        }
        node = n.parent();
    }
    node
}

// Walks up the parents of a parenthesized type to find the containing node
pub fn walk_up_parenthesized_types(node: impl Into<Option<P<Node>>>) -> Option<P<Node>> {
    let mut node = node.into();
    while let Some(n) = node {
        if n.kind() != Kind::ParenthesizedType {
            break;
        }
        node = n.parent();
    }
    node
}

// Walks up the parents of a node to find the containing SourceFile
// Keep early-returning parent walks as loops; see notes/perf-iterator-regressions.md.
pub fn get_source_file_of_node(node: impl Into<Option<P<Node>>>) -> Option<P<SourceFile>> {
    let mut node = node.into();
    while let Some(n) = node {
        if n.kind() == Kind::SourceFile {
            return Some(n.as_source_file_p());
        }
        node = n.parent();
    }
    None
}

fn set_parent_in_children_visit(parent: Option<P<Node>>, node: P<Node>) -> bool {
    if parent.is_some() {
        node.set_parent(parent);
    }
    node.for_each_child(&mut |child| set_parent_in_children_visit(Some(node), child));
    false
}

pub fn set_parent_in_children(node: P<Node>) {
    set_parent_in_children_visit(None, node);
}

// Walks up the parents of a node to find the ancestor that matches the callback
pub fn find_ancestor(
    node: impl Into<Option<P<Node>>>,
    mut callback: impl FnMut(P<Node>) -> bool,
) -> Option<P<Node>> {
    let mut node = node.into();
    while let Some(n) = node {
        if callback(n) {
            return Some(n);
        }
        node = n.parent();
    }
    None
}

// Walks up the parents of a node to find the ancestor that matches the kind
pub fn find_ancestor_kind(node: impl Into<Option<P<Node>>>, kind: Kind) -> Option<P<Node>> {
    let mut node = node.into();
    while let Some(n) = node {
        if n.kind() == kind {
            return Some(n);
        }
        node = n.parent();
    }
    None
}

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum FindAncestorResult {
    False,
    True,
    Quit,
}

pub fn to_find_ancestor_result(b: bool) -> FindAncestorResult {
    if b {
        return FindAncestorResult::True;
    }
    FindAncestorResult::False
}

// Walks up the parents of a node to find the ancestor that matches the callback
pub fn find_ancestor_or_quit(
    node: impl Into<Option<P<Node>>>,
    mut callback: impl FnMut(P<Node>) -> FindAncestorResult,
) -> Option<P<Node>> {
    let mut node = node.into();
    while let Some(n) = node {
        match callback(n) {
            FindAncestorResult::Quit => return None,
            FindAncestorResult::True => return Some(n),
            FindAncestorResult::False => {}
        }
        node = n.parent();
    }
    None
}

pub fn is_node_descendant_of(
    node: impl Into<Option<P<Node>>>,
    ancestor: impl Into<Option<P<Node>>>,
) -> bool {
    let mut node = node.into();
    let ancestor = ancestor.into();
    while let Some(n) = node {
        if Some(n) == ancestor {
            return true;
        }
        node = n.parent();
    }
    false
}

pub fn modifier_to_flag(token: Kind) -> ModifierFlags {
    match token {
        Kind::StaticKeyword => ModifierFlags::Static,
        Kind::PublicKeyword => ModifierFlags::Public,
        Kind::ProtectedKeyword => ModifierFlags::Protected,
        Kind::PrivateKeyword => ModifierFlags::Private,
        Kind::AbstractKeyword => ModifierFlags::Abstract,
        Kind::AccessorKeyword => ModifierFlags::Accessor,
        Kind::ExportKeyword => ModifierFlags::Export,
        Kind::DeclareKeyword => ModifierFlags::Ambient,
        Kind::ConstKeyword => ModifierFlags::Const,
        Kind::DefaultKeyword => ModifierFlags::Default,
        Kind::AsyncKeyword => ModifierFlags::Async,
        Kind::ReadonlyKeyword => ModifierFlags::Readonly,
        Kind::OverrideKeyword => ModifierFlags::Override,
        Kind::InKeyword => ModifierFlags::In,
        Kind::OutKeyword => ModifierFlags::Out,
        Kind::Decorator => ModifierFlags::Decorator,
        _ => ModifierFlags::None,
    }
}

pub fn modifiers_to_flags(modifiers: &[P<Node>]) -> ModifierFlags {
    let mut flags = ModifierFlags::None;
    flags = modifiers.iter().fold(flags, |mut flags, modifier| {
        flags |= modifier_to_flag(modifier.kind());

        flags
    });
    flags
}

pub fn has_syntactic_modifier(node: P<Node>, flags: ModifierFlags) -> bool {
    node.modifier_flags().intersects(flags)
}

pub fn has_accessor_modifier(node: P<Node>) -> bool {
    has_syntactic_modifier(node, ModifierFlags::Accessor)
}

pub fn has_static_modifier(node: P<Node>) -> bool {
    has_syntactic_modifier(node, ModifierFlags::Static)
}

pub fn is_static(node: P<Node>) -> bool {
    // https://tc39.es/ecma262/#sec-static-semantics-isstatic
    is_class_element(node) && has_static_modifier(node) || is_class_static_block_declaration(node)
}

pub fn can_have_symbol(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::ArrowFunction
            | Kind::BinaryExpression
            | Kind::BindingElement
            | Kind::CallExpression
            | Kind::CallSignature
            | Kind::ClassDeclaration
            | Kind::ClassExpression
            | Kind::ClassStaticBlockDeclaration
            | Kind::Constructor
            | Kind::ConstructorType
            | Kind::ConstructSignature
            | Kind::ElementAccessExpression
            | Kind::EnumDeclaration
            | Kind::EnumMember
            | Kind::ExportAssignment
            | Kind::ExportDeclaration
            | Kind::ExportSpecifier
            | Kind::FunctionDeclaration
            | Kind::FunctionExpression
            | Kind::FunctionType
            | Kind::GetAccessor
            | Kind::ImportClause
            | Kind::ImportEqualsDeclaration
            | Kind::ImportSpecifier
            | Kind::IndexSignature
            | Kind::InterfaceDeclaration
            | Kind::JSTypeAliasDeclaration
            | Kind::JsxAttribute
            | Kind::JsxAttributes
            | Kind::JsxSpreadAttribute
            | Kind::MappedType
            | Kind::MethodDeclaration
            | Kind::MethodSignature
            | Kind::ModuleDeclaration
            | Kind::NamedTupleMember
            | Kind::NamespaceExport
            | Kind::NamespaceExportDeclaration
            | Kind::NamespaceImport
            | Kind::NewExpression
            | Kind::NoSubstitutionTemplateLiteral
            | Kind::NumericLiteral
            | Kind::ObjectLiteralExpression
            | Kind::Parameter
            | Kind::PropertyAccessExpression
            | Kind::PropertyAssignment
            | Kind::PropertyDeclaration
            | Kind::PropertySignature
            | Kind::SetAccessor
            | Kind::ShorthandPropertyAssignment
            | Kind::SourceFile
            | Kind::SpreadAssignment
            | Kind::StringLiteral
            | Kind::TypeAliasDeclaration
            | Kind::TypeLiteral
            | Kind::TypeParameter
            | Kind::VariableDeclaration
    )
}

pub fn can_have_illegal_decorators(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::PropertyAssignment
            | Kind::ShorthandPropertyAssignment
            | Kind::FunctionDeclaration
            | Kind::Constructor
            | Kind::IndexSignature
            | Kind::ClassStaticBlockDeclaration
            | Kind::MissingDeclaration
            | Kind::VariableStatement
            | Kind::InterfaceDeclaration
            | Kind::TypeAliasDeclaration
            | Kind::EnumDeclaration
            | Kind::ModuleDeclaration
            | Kind::ImportEqualsDeclaration
            | Kind::ImportDeclaration
            | Kind::JSImportDeclaration
            | Kind::NamespaceExportDeclaration
            | Kind::ExportDeclaration
            | Kind::ExportAssignment
    )
}

pub fn can_have_illegal_modifiers(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::ClassStaticBlockDeclaration
            | Kind::PropertyAssignment
            | Kind::ShorthandPropertyAssignment
            | Kind::MissingDeclaration
            | Kind::NamespaceExportDeclaration
    )
}

pub fn can_have_modifiers(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::TypeParameter
            | Kind::Parameter
            | Kind::PropertySignature
            | Kind::PropertyDeclaration
            | Kind::MethodSignature
            | Kind::MethodDeclaration
            | Kind::Constructor
            | Kind::GetAccessor
            | Kind::SetAccessor
            | Kind::IndexSignature
            | Kind::ConstructorType
            | Kind::FunctionExpression
            | Kind::ArrowFunction
            | Kind::ClassExpression
            | Kind::VariableStatement
            | Kind::FunctionDeclaration
            | Kind::ClassDeclaration
            | Kind::InterfaceDeclaration
            | Kind::TypeAliasDeclaration
            | Kind::EnumDeclaration
            | Kind::ModuleDeclaration
            | Kind::ImportEqualsDeclaration
            | Kind::ImportDeclaration
            | Kind::JSImportDeclaration
            | Kind::ExportAssignment
            | Kind::ExportDeclaration
    )
}

pub fn can_have_decorators(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::Parameter
            | Kind::PropertyDeclaration
            | Kind::MethodDeclaration
            | Kind::GetAccessor
            | Kind::SetAccessor
            | Kind::ClassExpression
            | Kind::ClassDeclaration
    )
}

pub fn is_function_or_module_block(node: P<Node>) -> bool {
    is_source_file(node)
        || is_module_block(node)
        || is_block(node) && is_function_like(node.parent())
}

pub fn is_function_expression_or_arrow_function(node: P<Node>) -> bool {
    is_function_expression(node) || is_arrow_function(node)
}

// Warning: This has the same semantics as the forEach family of functions in that traversal terminates
// in the event that 'visitor' returns true.
pub fn for_each_return_statement(body: P<Node>, mut visitor: impl FnMut(P<Node>) -> bool) -> bool {
    fn traverse(node: P<Node>, visitor: &mut dyn FnMut(P<Node>) -> bool) -> bool {
        match node.kind() {
            Kind::ReturnStatement => visitor(node),
            Kind::CaseBlock
            | Kind::Block
            | Kind::IfStatement
            | Kind::DoStatement
            | Kind::WhileStatement
            | Kind::ForStatement
            | Kind::ForInStatement
            | Kind::ForOfStatement
            | Kind::WithStatement
            | Kind::SwitchStatement
            | Kind::CaseClause
            | Kind::DefaultClause
            | Kind::LabeledStatement
            | Kind::TryStatement
            | Kind::CatchClause => node.for_each_child(&mut |child| traverse(child, visitor)),
            _ => false,
        }
    }
    traverse(body, &mut visitor)
}

pub fn get_root_declaration(mut node: P<Node>) -> P<Node> {
    while node.kind() == Kind::BindingElement {
        node = node.parent().unwrap().parent().unwrap();
    }
    node
}

pub fn get_combined_modifier_flags(node: P<Node>) -> ModifierFlags {
    let node = get_root_declaration(node);
    let mut flags = node.modifier_flags();
    let mut node = Some(node);
    if let Some(n) = node
        && n.kind() == Kind::VariableDeclaration
    {
        node = n.parent();
    }
    if let Some(n) = node
        && n.kind() == Kind::VariableDeclarationList
    {
        flags |= n.modifier_flags();
        node = n.parent();
    }
    if let Some(n) = node
        && n.kind() == Kind::VariableStatement
    {
        flags |= n.modifier_flags();
    }
    flags
}

pub fn get_combined_node_flags(node: P<Node>) -> NodeFlags {
    let node = get_root_declaration(node);
    let mut flags = node.flags();
    let mut node = Some(node);
    if let Some(n) = node
        && n.kind() == Kind::VariableDeclaration
    {
        node = n.parent();
    }
    if let Some(n) = node
        && n.kind() == Kind::VariableDeclarationList
    {
        flags |= n.flags();
        node = n.parent();
    }
    if let Some(n) = node
        && n.kind() == Kind::VariableStatement
    {
        flags |= n.flags();
    }
    flags
}

// GetJSDocDeprecatedTag returns the first @deprecated JSDoc tag for the given node, or nil if none exists.
pub fn get_jsdoc_deprecated_tag(node: P<Node>) -> Option<P<Node>> {
    for &jsdoc in node.jsdoc(None) {
        if let Some(tags) = jsdoc.as_jsdoc().tags
            && let Some(result) = tags
                .nodes()
                .iter()
                .find_map(|&tag| if is_jsdoc_deprecated_tag(tag) { Some(Some(tag)) } else { None })
        {
            return result;
        }
    }
    None
}

// IsDeprecatedDeclarationWithCachedFlags is the core logic for IsDeprecatedDeclaration,
// parameterized on pre-computed combined flags so the checker can supply cached flags.
pub fn is_deprecated_declaration_with_cached_flags(
    declaration: P<Node>,
    combined_flags: NodeFlags,
) -> bool {
    if !combined_flags.intersects(NodeFlags::PossiblyContainsDeprecatedTag) {
        return false;
    }
    // Walk up to find the node that directly has the flag, since JSDoc is
    // attached to that node (e.g. VariableStatement, not VariableDeclaration).
    let mut n = Some(declaration);
    while let Some(node) = n {
        if node.flags().intersects(NodeFlags::PossiblyContainsDeprecatedTag) {
            return get_jsdoc_deprecated_tag(node).is_some();
        }
        n = node.parent();
    }
    false
}

pub fn is_var_await_using(node: P<Node>) -> bool {
    get_combined_node_flags(node) & NodeFlags::BlockScoped == NodeFlags::AwaitUsing
}

// Gets whether a bound `VariableDeclaration` or `VariableDeclarationList` is part of a `using` declaration.
pub fn is_var_using(node: P<Node>) -> bool {
    get_combined_node_flags(node) & NodeFlags::BlockScoped == NodeFlags::Using
}

// Gets whether a bound `VariableDeclaration` or `VariableDeclarationList` is part of a `const` declaration.
pub fn is_var_const(node: P<Node>) -> bool {
    get_combined_node_flags(node) & NodeFlags::BlockScoped == NodeFlags::Const
}

// Gets whether a bound `VariableDeclaration` or `VariableDeclarationList` is part of a `const`, `using` or `await using` declaration.
pub fn is_var_const_like(node: P<Node>) -> bool {
    let flags = get_combined_node_flags(node) & NodeFlags::BlockScoped;
    flags == NodeFlags::Const || flags == NodeFlags::Using || flags == NodeFlags::AwaitUsing
}

pub fn is_var_let(node: P<Node>) -> bool {
    get_combined_node_flags(node) & NodeFlags::BlockScoped == NodeFlags::Let
}

pub fn is_import_meta(node: P<Node>) -> bool {
    if node.kind() == Kind::MetaProperty {
        return node.as_meta_property().keyword_token == Kind::ImportKeyword
            && node.name().unwrap().text() == "meta";
    }
    false
}

pub fn walk_up_binding_elements_and_patterns(binding: P<Node>) -> P<Node> {
    let mut node = binding.parent().unwrap();
    while is_binding_element(node.parent().unwrap()) {
        node = node.parent().unwrap().parent().unwrap();
    }
    node.parent().unwrap()
}

pub fn is_source_file_js(file: P<SourceFile>) -> bool {
    let script_kind = file.script_kind();
    script_kind == ScriptKind::JS || script_kind == ScriptKind::JSX
}

pub fn is_in_js_file(node: impl Into<Option<P<Node>>>) -> bool {
    match node.into() {
        Some(node) => node.flags().intersects(NodeFlags::JavaScriptFile),
        None => false,
    }
}

pub fn is_declaration(node: P<Node>) -> bool {
    if node.kind() == Kind::TypeParameter {
        return node.parent().is_some();
    }
    is_declaration_node(node)
}

// True if `name` is the name of a declaration node
pub fn is_declaration_name(name: P<Node>) -> bool {
    !is_source_file(name)
        && !is_binding_pattern(name)
        && is_declaration(name.parent().unwrap())
        && name.parent().unwrap().name() == Some(name)
}

// Like 'isDeclarationName', but returns true for LHS of `import { x as y }` or `export { x as y }`.
pub fn is_declaration_name_or_import_property_name(name: P<Node>) -> bool {
    match name.parent().unwrap().kind() {
        Kind::ImportSpecifier | Kind::ExportSpecifier => {
            is_identifier(name) || name.kind() == Kind::StringLiteral
        }
        _ => is_declaration_name(name),
    }
}

pub fn is_literal_computed_property_declaration_name(node: P<Node>) -> bool {
    is_string_or_numeric_literal_like(node)
        && node.parent().unwrap().kind() == Kind::ComputedPropertyName
        && is_declaration(node.parent().unwrap().parent().unwrap())
}

pub fn is_external_module_import_equals_declaration(node: P<Node>) -> bool {
    node.kind() == Kind::ImportEqualsDeclaration
        && node.as_import_equals_declaration().module_reference.kind()
            == Kind::ExternalModuleReference
}

pub fn is_module_or_enum_declaration(node: P<Node>) -> bool {
    node.kind() == Kind::ModuleDeclaration || node.kind() == Kind::EnumDeclaration
}

pub fn is_literal_import_type_node(node: P<Node>) -> bool {
    is_import_type_node(node)
        && is_literal_type_node(node.as_import_type_node().argument)
        && is_string_literal(node.as_import_type_node().argument.as_literal_type_node().literal)
}

pub fn is_jsx_tag_name(node: P<Node>) -> bool {
    let parent = node.parent().unwrap();
    match parent.kind() {
        Kind::JsxOpeningElement | Kind::JsxClosingElement | Kind::JsxSelfClosingElement => {
            parent.tag_name() == node
        }
        _ => false,
    }
}

pub fn is_import_or_export_specifier(node: P<Node>) -> bool {
    is_import_specifier(node) || is_export_specifier(node)
}

pub fn is_exports_identifier(node: P<Node>) -> bool {
    is_identifier(node) && node.text() == "exports"
}

pub fn is_module_identifier(node: P<Node>) -> bool {
    is_identifier(node) && node.text() == "module"
}

pub fn is_this_identifier(node: P<Node>) -> bool {
    // The length first: an identifier is rarely spelled `this` (the keyword is a `ThisKeyword` node).
    is_identifier(node) && node.as_identifier().text_len() == 4 && node.text() == "this"
}

pub fn is_this_parameter(node: P<Node>) -> bool {
    is_parameter_declaration(node) && node.name().is_some_and(is_this_identifier)
}

pub fn is_bindable_static_access_expression(node: P<Node>, exclude_this_keyword: bool) -> bool {
    is_property_access_expression(node)
        && (!exclude_this_keyword && node.expression().unwrap().kind() == Kind::ThisKeyword
            || is_identifier(node.name().unwrap())
                && is_bindable_static_name_expression(
                    node.expression().unwrap(),
                    true, /*excludeThisKeyword*/
                ))
        || is_bindable_static_element_access_expression(node, exclude_this_keyword)
}

fn is_bindable_static_element_access_expression(node: P<Node>, exclude_this_keyword: bool) -> bool {
    is_literal_like_element_access(node)
        && ((!exclude_this_keyword && node.expression().unwrap().kind() == Kind::ThisKeyword)
            || is_entity_name_expression(node.expression().unwrap())
            || is_bindable_static_access_expression(
                node.expression().unwrap(),
                true, /*excludeThisKeyword*/
            ))
}

pub fn is_prototype_access(node: P<Node>) -> bool {
    if is_bindable_static_access_expression(node, false /*excludeThisKeyword*/)
        && let Some(name) = get_element_or_property_access_name(node)
    {
        return name.text() == "prototype";
    }
    false
}

fn is_literal_like_element_access(node: P<Node>) -> bool {
    is_element_access_expression(node)
        && is_string_or_numeric_literal_like(
            node.as_element_access_expression().argument_expression,
        )
}

pub fn is_bindable_static_name_expression(node: P<Node>, exclude_this_keyword: bool) -> bool {
    is_entity_name_expression(node)
        || is_bindable_static_access_expression(node, exclude_this_keyword)
}

// Does not handle signed numeric names like `a[+0]` - handling those would require handling prefix unary expressions
// throughout late binding handling as well, which is awkward (but ultimately probably doable if there is demand)
pub fn get_element_or_property_access_name(node: P<Node>) -> Option<P<Node>> {
    match node.kind() {
        Kind::PropertyAccessExpression => {
            let name = node.name().unwrap();
            if is_identifier(name) {
                return Some(name);
            }
            None
        }
        Kind::ElementAccessExpression => {
            let arg = skip_parentheses(node.as_element_access_expression().argument_expression);
            if is_string_or_numeric_literal_like(arg) {
                return Some(arg);
            }
            None
        }
        _ => panic!("Unhandled case in GetElementOrPropertyAccessName"),
    }
}

pub fn is_expression_with_type_arguments_in_class_extends_clause(node: P<Node>) -> bool {
    try_get_class_extending_expression_with_type_arguments(node).is_some()
}

pub fn try_get_class_extending_expression_with_type_arguments(node: P<Node>) -> Option<P<Node>> {
    if !is_expression_with_type_arguments(node) {
        return None;
    }
    let (cls, is_implements) =
        try_get_class_implementing_or_extending_heritage_clause_element(node);
    if cls.is_some() && !is_implements {
        return cls;
    }
    None
}

pub fn try_get_class_implementing_or_extending_heritage_clause_element(
    node: P<Node>,
) -> (Option<P<Node>>, bool) {
    if (is_expression_with_type_arguments(node) || is_type_reference_node(node))
        && is_heritage_clause(node.parent().unwrap())
        && is_class_like(node.parent().unwrap().parent().unwrap())
    {
        let parent = node.parent().unwrap();
        return (parent.parent(), parent.as_heritage_clause().token == Kind::ImplementsKeyword);
    }
    (None, false)
}

pub fn get_name_of_declaration(declaration: impl Into<Option<P<Node>>>) -> Option<P<Node>> {
    let declaration = declaration.into()?;
    let non_assigned_name = get_non_assigned_name_of_declaration(declaration);
    if non_assigned_name.is_some() {
        return non_assigned_name;
    }
    if is_function_expression(declaration)
        || is_arrow_function(declaration)
        || is_class_expression(declaration)
    {
        return get_assigned_name(declaration);
    }
    None
}

pub fn get_non_assigned_name_of_declaration(declaration: P<Node>) -> Option<P<Node>> {
    // !!!
    match declaration.kind() {
        Kind::BinaryExpression | Kind::CallExpression => {
            match get_assignment_declaration_kind(declaration) {
                JSDeclarationKind::Property
                | JSDeclarationKind::ThisProperty
                | JSDeclarationKind::ExportsProperty => {
                    let left = declaration.as_binary_expression().left;
                    if let Some(name) = get_element_or_property_access_name(left) {
                        return Some(name);
                    }
                    return Some(left);
                }
                JSDeclarationKind::ObjectDefinePropertyValue
                | JSDeclarationKind::ObjectDefinePropertyExports => {
                    return Some(declaration.arguments()[1]);
                }
                _ => {}
            }
            None
        }
        Kind::ExportAssignment => {
            let expr = declaration.expression().unwrap();
            if is_identifier(expr) {
                return Some(expr);
            }
            None
        }
        _ => declaration.name(),
    }
}

pub fn get_assigned_name(node: P<Node>) -> Option<P<Node>> {
    if let Some(parent) = node.parent() {
        match parent.kind() {
            Kind::PropertyAssignment | Kind::BindingElement => return parent.name(),
            Kind::BinaryExpression => {
                if node == parent.as_binary_expression().right() {
                    let left = parent.as_binary_expression().left;
                    match left.kind() {
                        Kind::Identifier => return Some(left),
                        Kind::PropertyAccessExpression => return left.name(),
                        Kind::ElementAccessExpression => {
                            let arg = skip_parentheses(
                                left.as_element_access_expression().argument_expression,
                            );
                            if is_string_or_numeric_literal_like(arg) {
                                return Some(arg);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Kind::VariableDeclaration => {
                let name = parent.name().unwrap();
                if is_identifier(name) {
                    return Some(name);
                }
            }
            _ => {}
        }
    }
    None
}

// utilities.go:80 (added for emit)
pub fn range_is_synthesized(loc: tsrs_core::TextRange) -> bool {
    position_is_synthesized(loc.pos()) || position_is_synthesized(loc.end())
}
