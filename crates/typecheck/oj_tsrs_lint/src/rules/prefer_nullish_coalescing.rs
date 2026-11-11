//! Port of tsgolint's `internal/rules/prefer_nullish_coalescing/prefer_nullish_coalescing.go`.

use std::collections::VecDeque;

use rustc_hash::FxHashSet;
use tsrs_ast::{self as ast, Kind, Node};
use tsrs_checker::{Type, TypeFlags};
use tsrs_core::{P, json::Value};

use crate::protocol::{Fix, Range, RuleMessage, Suggestion};
use crate::rule::{Listener, Rule, RuleContext, replace_node, trim_node_range};
use crate::utils::{intersection_type_parts, is_strong_precedence_node, union_type_parts};

#[derive(Clone, Copy)]
enum MessageId {
    NoStrictNullCheck,
    PreferNullishOverTernary,
    PreferNullishOverAssignment,
    SuggestNullishCoalescing,
}

impl MessageId {
    fn message(self) -> RuleMessage {
        let (id, description) = match self {
            Self::NoStrictNullCheck => (
                "noStrictNullCheck",
                "This rule requires the `strictNullChecks` compiler option to be turned on to function correctly.",
            ),
            Self::PreferNullishOverTernary => (
                "preferNullishOverTernary",
                "Prefer using nullish coalescing operator (`??`) instead of a ternary expression, as it is simpler to read.",
            ),
            Self::PreferNullishOverAssignment => (
                "preferNullishOverAssignment",
                "Prefer using nullish coalescing operator (`??=`) instead of an assignment expression, as it is simpler to read.",
            ),
            Self::SuggestNullishCoalescing => {
                ("suggestNullishCoalescing", "Change to the nullish coalescing operator.")
            }
        };
        RuleMessage { id: id.into(), description: description.into(), ..Default::default() }
    }
}

fn prefer_nullish_over_or_message(description: &str, equals: &str) -> RuleMessage {
    RuleMessage {
        id: "preferNullishOverOr".into(),
        description: format!(
            "Prefer using nullish coalescing operator (`??{equals}`) instead of a logical {description} (`||{equals}`), as it is a safer operator."
        ),
        ..Default::default()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NullishCheckOperator {
    Empty,
    Not,
    Equal,
    StrictEqual,
    NotEqual,
    NotStrictEq,
}

fn is_logical_or_operator(node: P<Node>) -> bool {
    node.kind() == Kind::BinaryExpression
        && matches!(
            node.as_binary_expression().operator_token.kind(),
            Kind::BarBarToken | Kind::BarBarEqualsToken
        )
}

fn is_member_access_like(node: P<Node>) -> bool {
    let node = ast::skip_parentheses(node);
    matches!(
        node.kind(),
        Kind::Identifier | Kind::PropertyAccessExpression | Kind::ElementAccessExpression
    ) || ast::is_optional_chain(node)
}

fn is_node_equal(a: Option<P<Node>>, b: Option<P<Node>>) -> bool {
    let (Some(a), Some(b)) = (a, b) else {
        return a == b;
    };
    if a.kind() != b.kind() {
        return false;
    }
    match a.kind() {
        Kind::Identifier
        | Kind::StringLiteral
        | Kind::NoSubstitutionTemplateLiteral
        | Kind::NumericLiteral
        | Kind::BigIntLiteral => a.text() == b.text(),
        Kind::PropertyAccessExpression => {
            let a = a.as_property_access_expression();
            let b = b.as_property_access_expression();
            is_node_equal(Some(a.name()), Some(b.name()))
                && is_node_equal(Some(a.expression), Some(b.expression))
        }
        Kind::ElementAccessExpression => {
            let a = a.as_element_access_expression();
            let b = b.as_element_access_expression();
            is_node_equal(Some(a.argument_expression), Some(b.argument_expression))
                && is_node_equal(Some(a.expression), Some(b.expression))
        }
        Kind::NullKeyword | Kind::ThisKeyword => true,
        _ => false,
    }
}

fn get_property_name_from_access(node: P<Node>) -> &'static str {
    match node.kind() {
        Kind::PropertyAccessExpression => node.as_property_access_expression().name().text(),
        Kind::ElementAccessExpression => {
            let argument = node.as_element_access_expression().argument_expression;
            if matches!(argument.kind(), Kind::StringLiteral | Kind::NoSubstitutionTemplateLiteral)
            {
                argument.text()
            } else {
                ""
            }
        }
        _ => "",
    }
}

fn get_object_expression_from_access(node: P<Node>) -> Option<P<Node>> {
    match node.kind() {
        Kind::PropertyAccessExpression => Some(node.as_property_access_expression().expression),
        Kind::ElementAccessExpression => Some(node.as_element_access_expression().expression),
        _ => None,
    }
}

fn are_nodes_similar_member_access(a: P<Node>, b: P<Node>) -> bool {
    let mut a = ast::skip_parentheses(a);
    let mut b = ast::skip_parentheses(b);
    if a.kind() == Kind::NonNullExpression {
        a = a.expression().unwrap();
    }
    if b.kind() == Kind::NonNullExpression {
        b = b.expression().unwrap();
    }
    let is_access = |node: P<Node>| {
        matches!(node.kind(), Kind::PropertyAccessExpression | Kind::ElementAccessExpression)
    };
    if is_access(a) && is_access(b) {
        let a_name = get_property_name_from_access(a);
        let b_name = get_property_name_from_access(b);
        if a_name.is_empty() || b_name.is_empty() {
            if a.kind() != b.kind() {
                return false;
            }
            if a.kind() == Kind::ElementAccessExpression {
                let a = a.as_element_access_expression();
                let b = b.as_element_access_expression();
                return are_nodes_similar_member_access(a.expression, b.expression)
                    && is_node_equal(Some(a.argument_expression), Some(b.argument_expression));
            }
        }
        if a_name != b_name {
            return false;
        }
        return are_nodes_similar_member_access(
            get_object_expression_from_access(a).unwrap(),
            get_object_expression_from_access(b).unwrap(),
        );
    }
    is_node_equal(Some(a), Some(b))
}

fn is_conditional_test(node: P<Node>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if ast::is_logical_expression(parent) || parent.kind() == Kind::ParenthesizedExpression {
        return is_conditional_test(parent);
    }
    if parent.kind() == Kind::ConditionalExpression {
        let conditional = parent.as_conditional_expression();
        if conditional.when_true == node || conditional.when_false == node {
            return is_conditional_test(parent);
        }
    }
    if parent.kind() == Kind::BinaryExpression {
        let binary = parent.as_binary_expression();
        if binary.operator_token.kind() == Kind::CommaToken && binary.right() == node {
            return is_conditional_test(parent);
        }
    }
    if parent.kind() == Kind::PrefixUnaryExpression
        && parent.as_prefix_unary_expression().operator == Kind::ExclamationToken
    {
        return is_conditional_test(parent);
    }
    match parent.kind() {
        Kind::ConditionalExpression => parent.as_conditional_expression().condition == node,
        Kind::DoStatement => parent.as_do_statement().expression == node,
        Kind::IfStatement => parent.as_if_statement().expression == node,
        Kind::ForStatement => parent.as_for_statement().condition == Some(node),
        Kind::WhileStatement => parent.as_while_statement().expression == node,
        _ => false,
    }
}

fn is_boolean_constructor_context(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if parent.kind() == Kind::ParenthesizedExpression || ast::is_logical_expression(parent) {
        return is_boolean_constructor_context(context, parent);
    }
    if parent.kind() == Kind::ConditionalExpression {
        let conditional = parent.as_conditional_expression();
        if conditional.when_true == node || conditional.when_false == node {
            return is_boolean_constructor_context(context, parent);
        }
    }
    if parent.kind() == Kind::BinaryExpression {
        let binary = parent.as_binary_expression();
        if binary.operator_token.kind() == Kind::CommaToken && binary.right() == node {
            return is_boolean_constructor_context(context, parent);
        }
    }
    if parent.kind() == Kind::CallExpression {
        let expression = parent.as_call_expression().expression;
        if expression.kind() == Kind::Identifier && expression.text() == "Boolean" {
            if let Some(symbol) = context.checker.get_symbol_at_location_exported(expression) {
                for &declaration in symbol.declarations() {
                    if ast::get_source_file_of_node(declaration)
                        .is_some_and(|file| !file.is_declaration_file())
                    {
                        return false;
                    }
                }
            }
            return true;
        }
    }
    false
}

pub struct PreferNullishCoalescing {
    ignore_boolean_coercion: bool,
    ignore_conditional_tests: bool,
    ignore_if_statements: bool,
    ignore_mixed_logical_expressions: bool,
    ignore_ternary_tests: bool,
    ignorable_flags: TypeFlags,
}

impl PreferNullishCoalescing {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("prefer-nullish-coalescing: options must be an object".into()),
        };
        let boolean = |name: &str, default| match object.and_then(|object| object.get(name)) {
            None | Some(Value::Null) => Ok(default),
            Some(Value::Bool(value)) => Ok(*value),
            _ => Err(format!("prefer-nullish-coalescing: {name} must be a boolean")),
        };
        let all_primitives = TypeFlags::BigIntLike
            | TypeFlags::BooleanLike
            | TypeFlags::NumberLike
            | TypeFlags::StringLike;
        let ignorable_flags = match object.and_then(|object| object.get("ignorePrimitives")) {
            Some(Value::Bool(true)) => all_primitives,
            Some(Value::Object(primitives)) => {
                let mut flags = TypeFlags::None;
                for (name, flag) in [
                    ("bigint", TypeFlags::BigIntLike),
                    ("boolean", TypeFlags::BooleanLike),
                    ("number", TypeFlags::NumberLike),
                    ("string", TypeFlags::StringLike),
                ] {
                    match primitives.get(name) {
                        Some(Value::Bool(true)) => flags |= flag,
                        None | Some(Value::Null | Value::Bool(false)) => {}
                        _ => {
                            flags = TypeFlags::None;
                            break;
                        }
                    }
                }
                flags
            }
            _ => TypeFlags::None,
        };
        Ok(Self {
            ignore_boolean_coercion: boolean("ignoreBooleanCoercion", false)?,
            ignore_conditional_tests: boolean("ignoreConditionalTests", true)?,
            ignore_if_statements: boolean("ignoreIfStatements", false)?,
            ignore_mixed_logical_expressions: boolean("ignoreMixedLogicalExpressions", false)?,
            ignore_ternary_tests: boolean("ignoreTernaryTests", false)?,
            ignorable_flags,
        })
    }

    fn is_type_eligible_for_prefer_nullish(&self, t: P<Type>) -> bool {
        let is_any_or_unknown = t.flags().intersects(TypeFlags::Any | TypeFlags::Unknown);
        let nullable = is_any_or_unknown
            || union_type_parts(&t).iter().any(|part| part.flags().intersects(TypeFlags::Nullable));
        if !nullable {
            return false;
        }
        if self.ignorable_flags.is_empty() {
            return true;
        }
        if is_any_or_unknown {
            return false;
        }
        !union_type_parts(&t).iter().any(|part| {
            intersection_type_parts(part)
                .iter()
                .any(|part| part.flags().intersects(self.ignorable_flags))
        })
    }

    fn is_truthiness_check_eligible_for_prefer_nullish(
        &self,
        context: &mut RuleContext<'_>,
        node: P<Node>,
        test_node: P<Node>,
    ) -> bool {
        let test_type = context.checker.get_type_at_location(test_node);
        if !self.is_type_eligible_for_prefer_nullish(test_type) {
            return false;
        }
        if self.ignore_conditional_tests && is_conditional_test(node) {
            return false;
        }
        if self.ignore_boolean_coercion
            && is_boolean_constructor_context(context, node)
            && !(node.kind() == Kind::ConditionalExpression
                && node.parent().is_some_and(|parent| parent.kind() == Kind::CallExpression))
        {
            return false;
        }
        true
    }

    fn should_skip_due_to_partial_nullish_check(
        context: &mut RuleContext<'_>,
        has_null_check: bool,
        has_undefined_check: bool,
        operator: NullishCheckOperator,
        target_node: P<Node>,
        allow_not_equal_operator: bool,
    ) -> bool {
        if has_null_check && has_undefined_check {
            return false;
        }
        if !has_null_check && !has_undefined_check {
            return true;
        }
        if operator == NullishCheckOperator::Equal
            || allow_not_equal_operator && operator == NullishCheckOperator::NotEqual
        {
            return false;
        }
        let t = context.checker.get_type_at_location(target_node);
        if t.flags().intersects(TypeFlags::Any | TypeFlags::Unknown) {
            return true;
        }
        let mut has_null_type = false;
        let mut has_undefined_type = false;
        for part in union_type_parts(&t) {
            has_null_type |= part.flags().intersects(TypeFlags::Null);
            has_undefined_type |= part.flags().intersects(TypeFlags::Undefined);
        }
        has_undefined_check && has_null_type || has_null_check && has_undefined_type
    }

    fn check_and_fix_with_prefer_nullish_over_or(
        &self,
        context: &mut RuleContext<'_>,
        node: P<Node>,
        description: &str,
        equals: &str,
    ) {
        let binary = node.as_binary_expression();
        if !self.is_truthiness_check_eligible_for_prefer_nullish(context, node, binary.left) {
            return;
        }
        if self.ignore_mixed_logical_expressions && is_mixed_logical_expression(node) {
            return;
        }
        let suggestions = if context.options.fix_suggestions {
            let mut fixes = Vec::new();
            let mut left_operand_starts_inside_left_expression = false;
            let before = |node: P<Node>, text: &str| {
                let pos = trim_node_range(context.source_file, node).pos;
                Fix { range: Range { pos, end: pos }, text: text.into() }
            };
            let after = |node: P<Node>, text: &str| {
                let end = node.end().expect_source("parsed lint node");
                Fix { range: Range { pos: end, end }, text: text.into() }
            };
            let mut parent = node.parent();
            while parent.is_some_and(|parent| parent.kind() == Kind::ParenthesizedExpression) {
                parent = parent.unwrap().parent();
            }
            if parent.is_some_and(ast::is_logical_expression)
                && node.parent().is_none_or(|parent| parent.kind() != Kind::ParenthesizedExpression)
            {
                let left = binary.left;
                if left.kind() == Kind::BinaryExpression
                    && ast::is_logical_expression(left)
                    && !is_logical_or_operator(left.as_binary_expression().left)
                {
                    fixes.push(before(left.as_binary_expression().right(), "("));
                    left_operand_starts_inside_left_expression = true;
                } else {
                    fixes.push(before(left, "("));
                }
                fixes.push(after(binary.right(), ")"));
            }
            if binary.operator_token.kind() == Kind::BarBarToken {
                if !left_operand_starts_inside_left_expression
                    && ast::is_logical_expression(binary.left)
                    && binary.left.kind() != Kind::ParenthesizedExpression
                {
                    fixes.extend([before(binary.left, "("), after(binary.left, ")")]);
                }
                if ast::is_logical_expression(binary.right())
                    && binary.right().kind() != Kind::ParenthesizedExpression
                {
                    fixes.extend([before(binary.right(), "("), after(binary.right(), ")")]);
                }
            }
            let new_operator =
                if binary.operator_token.kind() == Kind::BarBarToken { "??" } else { "??=" };
            fixes.push(replace_node(
                context.source_file,
                binary.operator_token,
                new_operator.into(),
            ));
            vec![Suggestion { message: MessageId::SuggestNullishCoalescing.message(), fixes }]
        } else {
            Vec::new()
        };
        context.report_node_with_suggestions(
            binary.operator_token,
            prefer_nullish_over_or_message(description, equals),
            || suggestions,
        );
    }

    fn check_conditional_expression(&self, context: &mut RuleContext<'_>, node: P<Node>) {
        if self.ignore_ternary_tests {
            return;
        }
        let Some((operator, nodes_inside_test)) =
            get_operator_and_nodes_inside_test_expression(node)
        else {
            return;
        };
        let (non_nullish_branch, nullish_branch) = get_branch_nodes(node, operator);
        let non_nullish_branch_unwrapped = ast::skip_parentheses(non_nullish_branch);
        let has_truthiness_check = nodes_inside_test.is_empty();
        let nullish_coalescing_left_node = if has_truthiness_check {
            let test = ast::skip_parentheses(node.as_conditional_expression().condition);
            let left = if test.kind() == Kind::PrefixUnaryExpression
                && test.as_prefix_unary_expression().operator == Kind::ExclamationToken
            {
                ast::skip_parentheses(test.as_prefix_unary_expression().operand)
            } else {
                test
            };
            if !are_nodes_similar_member_access(left, non_nullish_branch_unwrapped) {
                return;
            }
            left
        } else {
            let mut has_null_check = false;
            let mut has_undefined_check = false;
            let mut left = None;
            for test in nodes_inside_test {
                if test.kind() == Kind::NullKeyword {
                    has_null_check = true;
                } else if is_undefined_identifier(test) {
                    has_undefined_check = true;
                } else if are_nodes_similar_member_access(test, non_nullish_branch_unwrapped) {
                    if left.is_none() {
                        left = Some(test);
                    }
                } else {
                    return;
                }
            }
            let Some(left) = left else { return };
            if Self::should_skip_due_to_partial_nullish_check(
                context,
                has_null_check,
                has_undefined_check,
                operator,
                left,
                true,
            ) {
                return;
            }
            left
        };
        if has_truthiness_check
            && !self.is_truthiness_check_eligible_for_prefer_nullish(
                context,
                node,
                nullish_coalescing_left_node,
            )
        {
            return;
        }
        let suggestions = if context.options.fix_suggestions {
            let left_text = node_text(context, nullish_coalescing_left_node).trim();
            let right_text = node_text(context, nullish_branch).trim();
            let right_text = if !is_strong_precedence_node(nullish_branch)
                && nullish_branch.kind() != Kind::ParenthesizedExpression
            {
                format!("({right_text})")
            } else {
                right_text.into()
            };
            vec![Suggestion {
                message: MessageId::SuggestNullishCoalescing.message(),
                fixes: vec![replace_node(
                    context.source_file,
                    node,
                    format!("{left_text} ?? {right_text}"),
                )],
            }]
        } else {
            Vec::new()
        };
        context.report_node_with_suggestions(
            node,
            MessageId::PreferNullishOverTernary.message(),
            || suggestions,
        );
    }

    fn check_if_statement(&self, context: &mut RuleContext<'_>, node: P<Node>) {
        if self.ignore_if_statements {
            return;
        }
        let statement = node.as_if_statement();
        if statement.else_statement.is_some() {
            return;
        }
        let consequent = statement.then_statement;
        let assignment = if consequent.kind() == Kind::Block {
            let statements = consequent.statements();
            if statements.len() == 1 && statements[0].kind() == Kind::ExpressionStatement {
                statements[0].expression()
            } else {
                None
            }
        } else if consequent.kind() == Kind::ExpressionStatement {
            consequent.expression()
        } else {
            None
        };
        let Some(assignment) = assignment else { return };
        let assignment = ast::skip_parentheses(assignment);
        if !ast::is_assignment_expression(assignment, false) {
            return;
        }
        let binary = assignment.as_binary_expression();
        if !is_member_access_like(binary.left) {
            return;
        }
        let left = binary.left;
        let right = binary.right();
        let Some((operator, nodes_inside_test)) =
            get_operator_and_nodes_inside_test_expression(node)
        else {
            return;
        };
        if !matches!(
            operator,
            NullishCheckOperator::Not
                | NullishCheckOperator::Equal
                | NullishCheckOperator::StrictEqual
        ) {
            return;
        }
        if nodes_inside_test.is_empty() {
            let mut test = ast::skip_parentheses(statement.expression);
            if test.kind() == Kind::PrefixUnaryExpression {
                test = ast::skip_parentheses(test.as_prefix_unary_expression().operand);
            }
            if !are_nodes_similar_member_access(test, left)
                || !self.is_truthiness_check_eligible_for_prefer_nullish(context, node, test)
            {
                return;
            }
        } else {
            let mut has_null_check = false;
            let mut has_undefined_check = false;
            let mut found_matching_node = false;
            for test in nodes_inside_test {
                if test.kind() == Kind::NullKeyword {
                    has_null_check = true;
                } else if is_undefined_identifier(test) {
                    has_undefined_check = true;
                } else if are_nodes_similar_member_access(test, left) {
                    found_matching_node = true;
                } else {
                    return;
                }
            }
            if !found_matching_node
                || Self::should_skip_due_to_partial_nullish_check(
                    context,
                    has_null_check,
                    has_undefined_check,
                    operator,
                    left,
                    false,
                )
            {
                return;
            }
        }
        let suggestions = if context.options.fix_suggestions {
            let left_text = node_text(context, ast::skip_parentheses(left)).trim();
            let left_text = if left.kind() == Kind::ParenthesizedExpression {
                format!("({left_text})")
            } else {
                left_text.into()
            };
            let right_text = node_text(context, right).trim();
            vec![Suggestion {
                message: MessageId::SuggestNullishCoalescing.message(),
                fixes: vec![replace_node(
                    context.source_file,
                    node,
                    format!("{left_text} ??= {right_text};"),
                )],
            }]
        } else {
            Vec::new()
        };
        context.report_node_with_suggestions(
            node,
            MessageId::PreferNullishOverAssignment.message(),
            || suggestions,
        );
    }
}

fn is_mixed_logical_expression(node: P<Node>) -> bool {
    let binary = node.as_binary_expression();
    let mut seen = FxHashSet::default();
    let mut queue = VecDeque::from([node.parent(), Some(binary.left), Some(binary.right())]);
    while let Some(current) = queue.pop_front() {
        let Some(current) = current else { continue };
        if !seen.insert(current) {
            continue;
        }
        if ast::is_logical_expression(current) {
            let unwrapped = ast::skip_parentheses(current);
            if unwrapped.kind() != Kind::BinaryExpression {
                continue;
            }
            let binary = unwrapped.as_binary_expression();
            let operator = binary.operator_token.kind();
            if operator == Kind::AmpersandAmpersandToken {
                return true;
            }
            if operator == Kind::BarBarToken
                || operator == Kind::EqualsToken && ast::is_logical_expression(binary.left)
            {
                queue.extend([current.parent(), Some(binary.left), Some(binary.right())]);
            }
        }
    }
    false
}

fn is_undefined_identifier(node: P<Node>) -> bool {
    node.kind() == Kind::Identifier && node.text() == "undefined"
}

fn is_null_literal_or_undefined_identifier(node: P<Node>) -> bool {
    node.kind() == Kind::NullKeyword || is_undefined_identifier(node)
}

fn get_operator_and_nodes_inside_test_expression(
    node: P<Node>,
) -> Option<(NullishCheckOperator, Vec<P<Node>>)> {
    let test = match node.kind() {
        Kind::ConditionalExpression => node.as_conditional_expression().condition,
        Kind::IfStatement => node.as_if_statement().expression,
        _ => return None,
    };
    let test = ast::skip_parentheses(test);
    if is_member_access_like(test) {
        return Some((NullishCheckOperator::Empty, Vec::new()));
    }
    if test.kind() == Kind::PrefixUnaryExpression
        && test.as_prefix_unary_expression().operator == Kind::ExclamationToken
    {
        let argument = ast::skip_parentheses(test.as_prefix_unary_expression().operand);
        return is_member_access_like(argument).then(|| (NullishCheckOperator::Not, Vec::new()));
    }
    if test.kind() != Kind::BinaryExpression {
        return None;
    }
    let binary = test.as_binary_expression();
    let left = ast::skip_parentheses(binary.left);
    let right = ast::skip_parentheses(binary.right());
    let operator = match binary.operator_token.kind() {
        Kind::EqualsEqualsToken => NullishCheckOperator::Equal,
        Kind::ExclamationEqualsToken => NullishCheckOperator::NotEqual,
        Kind::EqualsEqualsEqualsToken => NullishCheckOperator::StrictEqual,
        Kind::ExclamationEqualsEqualsToken => NullishCheckOperator::NotStrictEq,
        Kind::BarBarToken | Kind::AmpersandAmpersandToken => {
            if binary.left.kind() != Kind::BinaryExpression
                || binary.right().kind() != Kind::BinaryExpression
            {
                return None;
            }
            let left = binary.left.as_binary_expression();
            let right = binary.right().as_binary_expression();
            let left_left = ast::skip_parentheses(left.left);
            let left_right = ast::skip_parentheses(left.right());
            let right_left = ast::skip_parentheses(right.left);
            let right_right = ast::skip_parentheses(right.right());
            if is_null_literal_or_undefined_identifier(left_left)
                && is_null_literal_or_undefined_identifier(left_right)
                || is_null_literal_or_undefined_identifier(right_left)
                    && is_null_literal_or_undefined_identifier(right_right)
            {
                return None;
            }
            let left_operator = left.operator_token.kind();
            let right_operator = right.operator_token.kind();
            let operator = if binary.operator_token.kind() == Kind::BarBarToken {
                if left_operator == Kind::EqualsEqualsEqualsToken
                    && right_operator == Kind::EqualsEqualsEqualsToken
                {
                    NullishCheckOperator::StrictEqual
                } else if matches!(
                    left_operator,
                    Kind::EqualsEqualsEqualsToken | Kind::EqualsEqualsToken
                ) && matches!(
                    right_operator,
                    Kind::EqualsEqualsEqualsToken | Kind::EqualsEqualsToken
                ) {
                    NullishCheckOperator::Equal
                } else {
                    return None;
                }
            } else if left_operator == Kind::ExclamationEqualsEqualsToken
                && right_operator == Kind::ExclamationEqualsEqualsToken
            {
                NullishCheckOperator::NotStrictEq
            } else if matches!(
                left_operator,
                Kind::ExclamationEqualsEqualsToken | Kind::ExclamationEqualsToken
            ) && matches!(
                right_operator,
                Kind::ExclamationEqualsEqualsToken | Kind::ExclamationEqualsToken
            ) {
                NullishCheckOperator::NotEqual
            } else {
                return None;
            };
            return Some((operator, vec![left_left, left_right, right_left, right_right]));
        }
        _ => return None,
    };
    Some((operator, vec![left, right]))
}

fn get_branch_nodes(node: P<Node>, operator: NullishCheckOperator) -> (P<Node>, P<Node>) {
    let conditional = node.as_conditional_expression();
    if matches!(
        operator,
        NullishCheckOperator::Empty
            | NullishCheckOperator::NotEqual
            | NullishCheckOperator::NotStrictEq
    ) {
        (conditional.when_true, conditional.when_false)
    } else {
        (conditional.when_false, conditional.when_true)
    }
}

fn node_text(context: &RuleContext<'_>, node: P<Node>) -> &'static str {
    &context.source_file.text()[node.pos().expect_source("parsed lint node") as usize
        ..node.end().expect_source("parsed lint node") as usize]
}

impl Rule for PreferNullishCoalescing {
    fn initialize(&mut self, context: &mut RuleContext<'_>) {
        let options = context.program.options();
        if !options.get_strict_option_value(options.strict_null_checks) {
            context.report_range(Range { pos: 0, end: 0 }, MessageId::NoStrictNullCheck.message());
        }
    }

    fn listens_to(&self, listener: Listener) -> bool {
        matches!(
            listener,
            Listener::Enter(
                Kind::BinaryExpression | Kind::ConditionalExpression | Kind::IfStatement
            )
        )
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        match node.kind() {
            Kind::BinaryExpression => match node.as_binary_expression().operator_token.kind() {
                Kind::BarBarToken => {
                    self.check_and_fix_with_prefer_nullish_over_or(context, node, "or", "");
                }
                Kind::BarBarEqualsToken => {
                    self.check_and_fix_with_prefer_nullish_over_or(
                        context,
                        node,
                        "assignment",
                        "=",
                    );
                }
                _ => {}
            },
            Kind::ConditionalExpression => self.check_conditional_expression(context, node),
            Kind::IfStatement => self.check_if_statement(context, node),
            _ => {}
        }
    }
}
