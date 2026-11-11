//! Port of tsgolint's `internal/rules/no_unnecessary_boolean_literal_compare/no_unnecessary_boolean_literal_compare.go`.

use tsrs_ast::{self as ast, Kind, Node};
use tsrs_checker::{Type, TypeFlags};
use tsrs_core::{P, json::Value};

use crate::protocol::{Fix, Range, RuleMessage};
use crate::rule::{Listener, Rule, RuleContext, replace_node, trim_node_range};

pub struct NoUnnecessaryBooleanLiteralCompare {
    allow_nullable_false: bool,
    allow_nullable_true: bool,
}

struct BooleanComparison {
    expression: P<Node>,
    literal_boolean: bool,
    negated: bool,
    nullable: bool,
}

#[derive(Clone, Copy)]
enum MessageId {
    ComparingNullableToFalse,
    ComparingNullableToTrueDirect,
    ComparingNullableToTrueNegated,
    Direct,
    Negated,
    NoStrictNullCheck,
}

impl MessageId {
    fn message(self) -> RuleMessage {
        let (id, description) = match self {
            Self::ComparingNullableToFalse => (
                "comparingNullableToFalse",
                "This expression unnecessarily compares a nullable boolean value to false instead of using the ?? operator to provide a default.",
            ),
            Self::ComparingNullableToTrueDirect => (
                "comparingNullableToTrueDirect",
                "This expression unnecessarily compares a nullable boolean value to true instead of using it directly.",
            ),
            Self::ComparingNullableToTrueNegated => (
                "comparingNullableToTrueNegated",
                "This expression unnecessarily compares a nullable boolean value to true instead of negating it.",
            ),
            Self::Direct => (
                "direct",
                "This expression unnecessarily compares a boolean value to a boolean instead of using it directly.",
            ),
            Self::Negated => (
                "negated",
                "This expression unnecessarily compares a boolean value to a boolean instead of negating it.",
            ),
            Self::NoStrictNullCheck => (
                "noStrictNullCheck",
                "This rule requires the `strictNullChecks` compiler option to be turned on to function correctly.",
            ),
        };
        RuleMessage { id: id.into(), description: description.into(), ..Default::default() }
    }
}

fn is_nullable_boolean(t: P<Type>) -> bool {
    if !t.flags().intersects(TypeFlags::Union) {
        return false;
    }
    let flags = t.types().iter().fold(TypeFlags::None, |flags, part| flags | part.flags());
    flags.intersects(TypeFlags::Nullable) && flags.intersects(TypeFlags::BooleanLike)
}

fn get_boolean_comparison(
    context: &mut RuleContext<'_>,
    node: P<Node>,
) -> Option<BooleanComparison> {
    let binary = node.as_binary_expression();
    let negated = match binary.operator_token.kind() {
        Kind::ExclamationEqualsToken | Kind::ExclamationEqualsEqualsToken => true,
        Kind::EqualsEqualsToken | Kind::EqualsEqualsEqualsToken => false,
        _ => return None,
    };
    let deconstruct = |against: P<Node>, expression| match ast::skip_parentheses(against).kind() {
        Kind::TrueKeyword => Some((expression, true)),
        Kind::FalseKeyword => Some((expression, false)),
        _ => None,
    };
    let (expression, literal_boolean) = deconstruct(binary.right(), binary.left)
        .or_else(|| deconstruct(binary.left, binary.right()))?;
    let t = context.checker.get_type_at_location(expression);
    let constraint = if t.flags().intersects(TypeFlags::TypeParameter) {
        context.checker.get_base_constraint_of_type(t)?
    } else {
        t
    };
    let nullable = if constraint.flags().intersects(TypeFlags::BooleanLike) {
        false
    } else if is_nullable_boolean(constraint) {
        true
    } else {
        return None;
    };
    Some(BooleanComparison { expression, literal_boolean, negated, nullable })
}

fn is_conditional_test(mut node: P<Node>) -> bool {
    while let Some(parent) = node.parent() {
        match parent.kind() {
            Kind::ParenthesizedExpression => {}
            Kind::BinaryExpression => {
                if !matches!(
                    parent.as_binary_expression().operator_token.kind(),
                    Kind::AmpersandAmpersandToken | Kind::BarBarToken
                ) {
                    return false;
                }
            }
            Kind::IfStatement => return parent.as_if_statement().expression == node,
            Kind::WhileStatement => return parent.as_while_statement().expression == node,
            Kind::DoStatement => return parent.as_do_statement().expression == node,
            Kind::ForStatement => return parent.as_for_statement().condition == Some(node),
            Kind::ConditionalExpression => {
                return parent.as_conditional_expression().condition == node;
            }
            Kind::PrefixUnaryExpression => {
                return parent.as_prefix_unary_expression().operator == Kind::ExclamationToken;
            }
            _ => return false,
        }
        node = parent;
    }
    false
}

fn comparison_fixes(
    context: &RuleContext<'_>,
    node: P<Node>,
    comparison: &BooleanComparison,
) -> Vec<Fix> {
    let mut parent = node.parent();
    while parent.is_some_and(|parent| parent.kind() == Kind::ParenthesizedExpression) {
        parent = parent.unwrap().parent();
    }
    let is_unary_negation = parent.is_some_and(|parent| {
        parent.kind() == Kind::PrefixUnaryExpression
            && parent.as_prefix_unary_expression().operator == Kind::ExclamationToken
    });
    let should_negate = comparison.negated != comparison.literal_boolean;
    let mutated_node = if is_unary_negation { parent.unwrap() } else { node };
    let text =
        &context.source_file.text()[comparison.expression.pos().expect_source("parsed lint node")
            as usize
            ..comparison.expression.end().expect_source("parsed lint node") as usize];
    if comparison.nullable
        && comparison.literal_boolean
        && should_negate != is_unary_negation
        && !is_conditional_test(mutated_node)
    {
        let text = text.trim();
        let text = if crate::utils::is_strong_precedence_node(comparison.expression) {
            text.to_owned()
        } else {
            format!("({text})")
        };
        return vec![replace_node(context.source_file, mutated_node, format!("(!!{text})"))];
    }
    let mut fixes = vec![replace_node(context.source_file, mutated_node, text.into())];
    let before = |text: &str| {
        let pos = trim_node_range(context.source_file, mutated_node).pos;
        Fix { range: Range { pos, end: pos }, text: text.into() }
    };
    let after = |text: &str| {
        let end = mutated_node.end().expect_source("parsed lint node");
        Fix { range: Range { pos: end, end }, text: text.into() }
    };
    if should_negate == is_unary_negation {
        fixes.push(before("!"));
        if !crate::utils::is_strong_precedence_node(comparison.expression) {
            fixes.extend([before("("), after(")")]);
        }
    }
    if comparison.nullable && !comparison.literal_boolean {
        fixes.extend([before("("), after(" ?? true)")]);
    }
    fixes
}

impl NoUnnecessaryBooleanLiteralCompare {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => {
                return Err(
                    "no-unnecessary-boolean-literal-compare: options must be an object".into()
                );
            }
        };
        let boolean = |name: &str| match object.and_then(|object| object.get(name)) {
            None | Some(Value::Null) => Ok(true),
            Some(Value::Bool(value)) => Ok(*value),
            _ => Err(format!("no-unnecessary-boolean-literal-compare: {name} must be a boolean")),
        };
        Ok(Self {
            allow_nullable_false: boolean("allowComparingNullableBooleansToFalse")?,
            allow_nullable_true: boolean("allowComparingNullableBooleansToTrue")?,
        })
    }
}

impl Rule for NoUnnecessaryBooleanLiteralCompare {
    fn initialize(&mut self, context: &mut RuleContext<'_>) {
        let options = context.program.options();
        if !options.get_strict_option_value(options.strict_null_checks) {
            context.report_range(Range { pos: 0, end: 0 }, MessageId::NoStrictNullCheck.message());
        }
    }

    fn listens_to(&self, listener: Listener) -> bool {
        listener == Listener::Enter(Kind::BinaryExpression)
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        let Some(comparison) = get_boolean_comparison(context, node) else {
            return;
        };
        if comparison.nullable
            && (comparison.literal_boolean && self.allow_nullable_true
                || !comparison.literal_boolean && self.allow_nullable_false)
        {
            return;
        }
        let id = if comparison.nullable {
            if comparison.literal_boolean {
                if comparison.negated {
                    MessageId::ComparingNullableToTrueNegated
                } else {
                    MessageId::ComparingNullableToTrueDirect
                }
            } else {
                MessageId::ComparingNullableToFalse
            }
        } else if comparison.negated {
            MessageId::Negated
        } else {
            MessageId::Direct
        };
        let fixes = if context.options.fix {
            comparison_fixes(context, node, &comparison)
        } else {
            Vec::new()
        };
        context.report_node_with_fixes(node, id.message(), || fixes);
    }
}
