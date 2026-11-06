//! Go package `evaluator` (`internal/evaluator/evaluator.go`).

use crate::*;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Result {
    pub value: Option<LiteralValue>,
    pub is_syntactically_string: bool,
    pub(crate) resolved_other_files: bool,
    pub has_external_references: bool,
}

pub(crate) fn new_result(
    value: Option<LiteralValue>,
    is_syntactically_string: bool,
    resolved_other_files: bool,
    has_external_references: bool,
) -> Result {
    Result { value, is_syntactically_string, resolved_other_files, has_external_references }
}

/// Go `type Evaluator func(expr *ast.Node, location *ast.Node) Result`, with the host passed back.
pub(crate) type Evaluator<H> = fn(&mut H, P<Node>, P<Node>) -> Result;

/// Go `NewEvaluator(evaluateEntity, outerExpressionsToSkip)(expr, location)`: the evaluator closure body, with its
/// captured values passed explicitly.
pub(crate) fn evaluate<H>(
    host: &mut H,
    evaluate_entity: Evaluator<H>,
    outer_expressions_to_skip: ast::OuterExpressionKinds,
    expr: P<Node>,
    location: P<Node>,
) -> Result {
    let mut is_syntactically_string = false;
    let mut resolved_other_files = false;
    let mut has_external_references = false;
    // It's unclear when/whether we should consider skipping other kinds of outer expressions.
    // Type assertions intentionally break evaluation when evaluating literal types, such as:
    //     type T = `one ${"two" as any} three`; // string
    // But it's less clear whether such an assertion should break enum member evaluation:
    //     enum E {
    //       A = "one" as any
    //     }
    // SatisfiesExpressions and non-null assertions seem to have even less reason to break
    // emitting enum members as literals. However, these expressions also break Babel's
    // evaluation (but not esbuild's), and the isolatedModules errors we give depend on
    // our evaluation results, so we're currently being conservative so as to issue errors
    // on code that might break Babel.
    let expr = ast::skip_outer_expressions(
        expr,
        outer_expressions_to_skip | ast::OuterExpressionKinds::Parentheses,
    );
    match expr.kind() {
        Kind::PrefixUnaryExpression => {
            let result = evaluate(
                host,
                evaluate_entity,
                outer_expressions_to_skip,
                expr.as_prefix_unary_expression().operand(),
                location,
            );
            resolved_other_files = result.resolved_other_files;
            has_external_references = result.has_external_references;
            if let Some(LiteralValue::Number(value)) = result.value {
                match expr.as_prefix_unary_expression().operator() {
                    Kind::PlusToken => {
                        return Result {
                            value: Some(LiteralValue::Number(value)),
                            is_syntactically_string,
                            resolved_other_files,
                            has_external_references,
                        };
                    }
                    Kind::MinusToken => {
                        return Result {
                            value: Some(LiteralValue::Number(-value)),
                            is_syntactically_string,
                            resolved_other_files,
                            has_external_references,
                        };
                    }
                    Kind::TildeToken => {
                        return Result {
                            value: Some(LiteralValue::Number(value.bitwise_not())),
                            is_syntactically_string,
                            resolved_other_files,
                            has_external_references,
                        };
                    }
                    _ => {}
                }
            }
        }
        Kind::BinaryExpression => {
            let bin = expr.as_binary_expression();
            let left =
                evaluate(host, evaluate_entity, outer_expressions_to_skip, bin.left(), location);
            let right =
                evaluate(host, evaluate_entity, outer_expressions_to_skip, bin.right(), location);
            let operator = bin.operator_token().kind();
            is_syntactically_string = (left.is_syntactically_string
                || right.is_syntactically_string)
                && bin.operator_token().kind() == Kind::PlusToken;
            resolved_other_files = left.resolved_other_files || right.resolved_other_files;
            has_external_references = left.has_external_references || right.has_external_references;
            let left_num =
                if let Some(LiteralValue::Number(n)) = left.value { Some(n) } else { None };
            let right_num =
                if let Some(LiteralValue::Number(n)) = right.value { Some(n) } else { None };
            if let (Some(left_num), Some(right_num)) = (left_num, right_num) {
                let value = match operator {
                    Kind::BarToken => Some(left_num.bitwise_or(right_num)),
                    Kind::AmpersandToken => Some(left_num.bitwise_and(right_num)),
                    Kind::GreaterThanGreaterThanToken => {
                        Some(left_num.signed_right_shift(right_num))
                    }
                    Kind::GreaterThanGreaterThanGreaterThanToken => {
                        Some(left_num.unsigned_right_shift(right_num))
                    }
                    Kind::LessThanLessThanToken => Some(left_num.left_shift(right_num)),
                    Kind::CaretToken => Some(left_num.bitwise_xor(right_num)),
                    Kind::AsteriskToken => Some(left_num * right_num),
                    Kind::SlashToken => Some(left_num / right_num),
                    Kind::PlusToken => Some(left_num + right_num),
                    Kind::MinusToken => Some(left_num - right_num),
                    Kind::PercentToken => Some(left_num.remainder(right_num)),
                    Kind::AsteriskAsteriskToken => Some(left_num.exponentiate(right_num)),
                    _ => None,
                };
                if let Some(value) = value {
                    return Result {
                        value: Some(LiteralValue::Number(value)),
                        is_syntactically_string,
                        resolved_other_files,
                        has_external_references,
                    };
                }
            }
            let left_str =
                if let Some(LiteralValue::String(s)) = left.value { Some(s) } else { None };
            let right_str =
                if let Some(LiteralValue::String(s)) = right.value { Some(s) } else { None };
            if (left_str.is_some() || left_num.is_some())
                && (right_str.is_some() || right_num.is_some())
                && operator == Kind::PlusToken
            {
                let mut s = match left_num {
                    Some(n) => n.string(),
                    None => left_str.unwrap().to_string(),
                };
                match right_num {
                    Some(n) => s.push_str(&n.string()),
                    None => s.push_str(right_str.unwrap()),
                }
                return Result {
                    value: Some(LiteralValue::String(alloc_str(&s))),
                    is_syntactically_string,
                    resolved_other_files,
                    has_external_references,
                };
            }
        }
        Kind::StringLiteral | Kind::NoSubstitutionTemplateLiteral => {
            return Result {
                value: Some(LiteralValue::String(expr.text())),
                is_syntactically_string: true,
                resolved_other_files: false,
                has_external_references: false,
            };
        }
        Kind::TemplateExpression => {
            return evaluate_template_expression(
                host,
                evaluate_entity,
                outer_expressions_to_skip,
                expr,
                location,
            );
        }
        Kind::NumericLiteral => {
            return Result {
                value: Some(LiteralValue::Number(jsnum::from_string(expr.text()))),
                is_syntactically_string: false,
                resolved_other_files: false,
                has_external_references: false,
            };
        }
        Kind::Identifier => {
            return evaluate_entity(host, expr, location);
        }
        Kind::ElementAccessExpression | Kind::PropertyAccessExpression
            if ast::is_entity_name_expression(expr.expression().unwrap()) =>
        {
            return evaluate_entity(host, expr, location);
        }
        _ => {}
    }
    Result { value: None, is_syntactically_string, resolved_other_files, has_external_references }
}

fn evaluate_template_expression<H>(
    host: &mut H,
    evaluate_entity: Evaluator<H>,
    outer_expressions_to_skip: ast::OuterExpressionKinds,
    expr: P<Node>,
    location: P<Node>,
) -> Result {
    let mut sb = String::new();
    let template = expr.as_template_expression();
    sb.push_str(template.head().text());
    let mut resolved_other_files = false;
    let mut has_external_references = false;
    for span in template.template_spans().nodes() {
        let span_result = evaluate(
            host,
            evaluate_entity,
            outer_expressions_to_skip,
            span.as_template_span().expression(),
            location,
        );
        if span_result.value.is_none() {
            return Result {
                value: None,
                is_syntactically_string: true,
                resolved_other_files: false,
                has_external_references: false,
            };
        }
        sb.push_str(&any_to_string(span_result.value.unwrap()));
        sb.push_str(span.as_template_span().literal().text());
        resolved_other_files = resolved_other_files || span_result.resolved_other_files;
        has_external_references = has_external_references || span_result.has_external_references;
    }
    Result {
        value: Some(LiteralValue::String(alloc_str(&sb))),
        is_syntactically_string: true,
        resolved_other_files,
        has_external_references,
    }
}

pub fn any_to_string(v: LiteralValue) -> String {
    match v {
        LiteralValue::String(s) => s.to_string(),
        LiteralValue::Number(n) => n.string(),
        LiteralValue::Boolean(b) => (if b { "true" } else { "false" }).to_string(),
        LiteralValue::BigInt(b) => b.string(),
    }
}

pub(crate) fn is_truthy(v: LiteralValue) -> bool {
    match v {
        LiteralValue::String(s) => !s.is_empty(),
        LiteralValue::Number(n) => n.0 != 0.0 && !n.is_nan(),
        LiteralValue::Boolean(b) => b,
        LiteralValue::BigInt(b) => b != jsnum::PseudoBigInt::default(),
    }
}
