//! Port of tsgolint's `internal/rules/no_confusing_void_expression/no_confusing_void_expression.go`.

use tsrs_ast::{self as ast, Kind, Node};
use tsrs_checker::{ContextFlags, SignatureKind, TypeFlags};
use tsrs_core::{P, json::Value};

use crate::protocol::{Diagnostic, Fix, Range, RuleMessage, Suggestion};
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};
use crate::utils::{get_constrained_type_at_location, union_type_parts};

pub struct NoConfusingVoidExpression {
    ignore_arrow_shorthand: bool,
    ignore_void_operator: bool,
    ignore_void_returning_functions: bool,
}

#[derive(Clone, Copy)]
enum MessageId {
    InvalidVoidExpr,
    InvalidVoidExprArrow,
    InvalidVoidExprArrowWrapVoid,
    InvalidVoidExprReturn,
    InvalidVoidExprReturnLast,
    InvalidVoidExprReturnWrapVoid,
    InvalidVoidExprWrapVoid,
    VoidExprWrapVoid,
}

impl MessageId {
    fn message(self) -> RuleMessage {
        let (id, description, help) = match self {
            Self::InvalidVoidExpr => (
                "invalidVoidExpr",
                "Placing a void expression inside another expression is forbidden.",
                "Move it to its own statement instead.",
            ),
            Self::InvalidVoidExprArrow => (
                "invalidVoidExprArrow",
                "Returning a void expression from an arrow function shorthand is forbidden.",
                "Add braces to the arrow function.",
            ),
            Self::InvalidVoidExprArrowWrapVoid => (
                "invalidVoidExprArrowWrapVoid",
                "Void expressions returned from an arrow function shorthand must be marked explicitly with the `void` operator.",
                "",
            ),
            Self::InvalidVoidExprReturn => (
                "invalidVoidExprReturn",
                "Returning a void expression from a function is forbidden.",
                "Move it before the `return` statement.",
            ),
            Self::InvalidVoidExprReturnLast => (
                "invalidVoidExprReturnLast",
                "Returning a void expression from a function is forbidden.",
                "Remove the `return` statement.",
            ),
            Self::InvalidVoidExprReturnWrapVoid => (
                "invalidVoidExprReturnWrapVoid",
                "Void expressions returned from a function must be marked explicitly with the `void` operator.",
                "",
            ),
            Self::InvalidVoidExprWrapVoid => (
                "invalidVoidExprWrapVoid",
                "Void expressions used inside another expression must be moved to its own statement or marked explicitly with the `void` operator.",
                "",
            ),
            Self::VoidExprWrapVoid => {
                ("voidExprWrapVoid", "Mark with an explicit `void` operator.", "")
            }
        };
        RuleMessage { id: id.into(), description: description.into(), help: help.into() }
    }
}

fn insert_before(context: &RuleContext<'_>, node: P<Node>, text: &str) -> Fix {
    let pos = trim_node_range(context.source_file, node).pos;
    Fix { range: Range { pos, end: pos }, text: text.into() }
}

fn insert_after(node: P<Node>, text: &str) -> Fix {
    let end = node.end().expect_source("parsed lint node");
    Fix { range: Range { pos: end, end }, text: text.into() }
}

fn can_fix(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    get_constrained_type_at_location(context.checker, node).flags().intersects(TypeFlags::VoidLike)
}

fn is_void_returning_function(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    if let Some(annotation) = node.type_node() {
        let t = context.checker.get_type_from_type_node(annotation);
        return union_type_parts(&t).iter().any(|t| t.flags().intersects(TypeFlags::Void));
    }
    if !matches!(node.kind(), Kind::ArrowFunction | Kind::FunctionExpression) {
        return false;
    }
    let Some(t) = context.checker.get_contextual_type(node, ContextFlags::None) else {
        return false;
    };
    union_type_parts(&t).iter().any(|&part| {
        context.checker.get_signatures_of_type(part, SignatureKind::Call).iter().any(|&signature| {
            let t = context.checker.get_return_type_of_signature(signature);
            union_type_parts(&t).iter().any(|t| t.flags().intersects(TypeFlags::Void))
        })
    })
}

fn is_final_return(node: P<Node>) -> bool {
    let Some(block) = node.parent() else {
        return false;
    };
    block.kind() == Kind::Block
        && ast::is_function_like_declaration(block.parent())
        && block.as_block().statements.nodes().last() == Some(&node)
}

fn return_fix(context: &RuleContext<'_>, statement: P<Node>) -> Fix {
    let expression = statement.as_return_statement().expression().unwrap();
    let token = tsrs_scanner::scan_token_at_position(
        context.source_file,
        expression.pos().expect_source("parsed lint node"),
    );
    let replacement =
        if matches!(token, Kind::OpenParenToken | Kind::OpenBracketToken | Kind::BacktickToken) {
            ";"
        } else {
            ""
        };
    let range = tsrs_scanner::get_range_of_token_at_position(
        context.source_file,
        statement.pos().expect_source("parsed lint node"),
    );
    Fix {
        range: Range { pos: range.source_pos().unwrap(), end: range.source_end().unwrap() },
        text: replacement.into(),
    }
}

impl NoConfusingVoidExpression {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("no-confusing-void-expression: options must be an object".into()),
        };
        let boolean = |name: &str| match object.and_then(|object| object.get(name)) {
            None | Some(Value::Null) => Ok(false),
            Some(Value::Bool(value)) => Ok(*value),
            _ => Err(format!("no-confusing-void-expression: {name} must be a boolean")),
        };
        Ok(Self {
            ignore_arrow_shorthand: boolean("ignoreArrowShorthand")?,
            ignore_void_operator: boolean("ignoreVoidOperator")?,
            ignore_void_returning_functions: boolean("ignoreVoidReturningFunctions")?,
        })
    }

    fn find_invalid_ancestor(&self, mut node: P<Node>) -> Option<P<Node>> {
        loop {
            let parent = node.parent().expect("void expression parent");
            match parent.kind() {
                Kind::ParenthesizedExpression => {}
                Kind::BinaryExpression => {
                    let binary = parent.as_binary_expression();
                    if ast::is_logical_or_coalescing_binary_operator(binary.operator_token.kind())
                        && binary.right() == node
                    {
                        node = parent;
                        continue;
                    }
                    if binary.operator_token.kind() == Kind::CommaToken && binary.left == node {
                        return None;
                    }
                    return Some(parent);
                }
                Kind::ExpressionStatement => return None,
                Kind::ConditionalExpression => {
                    let conditional = parent.as_conditional_expression();
                    if conditional.when_true != node && conditional.when_false != node {
                        return Some(parent);
                    }
                }
                Kind::ArrowFunction if self.ignore_arrow_shorthand => return None,
                Kind::VoidExpression if self.ignore_void_operator => return None,
                _ => return Some(parent),
            }
            node = parent;
        }
    }

    fn check_expression(&self, node: P<Node>, context: &mut RuleContext<'_>) {
        let Some(ancestor) = self.find_invalid_ancestor(node) else {
            return;
        };
        if !can_fix(context, node) {
            return;
        }
        let mut fixes = Vec::new();
        let mut suggestions = Vec::new();
        let id = if ancestor.kind() == Kind::ArrowFunction {
            if self.ignore_void_returning_functions && is_void_returning_function(context, ancestor)
            {
                return;
            }
            if self.ignore_void_operator {
                if context.options.fix {
                    fixes.push(insert_before(context, node, "void "));
                }
                MessageId::InvalidVoidExprArrowWrapVoid
            } else {
                if context.options.fix {
                    let body = ancestor.body().unwrap();
                    if body.kind() != Kind::Block && can_fix(context, body) {
                        let inner = ast::skip_parentheses(body);
                        fixes.push(Fix {
                            range: Range {
                                pos: body.pos().expect_source("parsed lint node"),
                                end: inner.pos().expect_source("parsed lint node"),
                            },
                            text: "{ ".into(),
                        });
                        fixes.push(Fix {
                            range: Range {
                                pos: inner.end().expect_source("parsed lint node"),
                                end: body.end().expect_source("parsed lint node"),
                            },
                            text: "; }".into(),
                        });
                    }
                }
                MessageId::InvalidVoidExprArrow
            }
        } else if ancestor.kind() == Kind::ReturnStatement {
            if self.ignore_void_returning_functions {
                let mut parent = ancestor.parent();
                while let Some(node) = parent {
                    if ast::is_function_like_declaration(node) {
                        if is_void_returning_function(context, node) {
                            return;
                        }
                        break;
                    }
                    parent = node.parent();
                }
            }
            if self.ignore_void_operator {
                if context.options.fix {
                    fixes.push(insert_before(context, node, "void "));
                }
                MessageId::InvalidVoidExprReturnWrapVoid
            } else if is_final_return(ancestor) {
                if context.options.fix
                    && can_fix(context, ancestor.as_return_statement().expression().unwrap())
                {
                    fixes.push(return_fix(context, ancestor));
                }
                MessageId::InvalidVoidExprReturnLast
            } else {
                if context.options.fix {
                    fixes.push(return_fix(context, ancestor));
                    fixes.push(insert_after(ancestor, "; return;"));
                    if ancestor.parent().is_some_and(|parent| parent.kind() != Kind::Block) {
                        fixes.push(insert_before(context, ancestor, "{ "));
                        fixes.push(insert_after(ancestor, " }"));
                    }
                }
                MessageId::InvalidVoidExprReturn
            }
        } else if self.ignore_void_operator {
            if context.options.fix_suggestions {
                suggestions.push(Suggestion {
                    message: MessageId::VoidExprWrapVoid.message(),
                    fixes: vec![insert_before(context, node, "void ")],
                });
            }
            MessageId::InvalidVoidExprWrapVoid
        } else {
            MessageId::InvalidVoidExpr
        };
        context.report_diagnostic(Diagnostic {
            range: Some(trim_node_range(context.source_file, node)),
            message: id.message(),
            fixes,
            suggestions,
            ..Default::default()
        });
    }
}

impl Rule for NoConfusingVoidExpression {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(
            listener,
            Listener::Enter(
                Kind::AwaitExpression | Kind::CallExpression | Kind::TaggedTemplateExpression
            )
        )
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        self.check_expression(node, context);
    }
}
