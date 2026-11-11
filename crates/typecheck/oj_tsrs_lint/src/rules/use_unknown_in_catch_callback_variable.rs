use tsrs_ast::{self as ast, Kind, Node};
use tsrs_checker::{SignatureKind, Type, TypeFlags};
use tsrs_core::P;

use crate::protocol::{Fix, Range, RuleMessage, Suggestion};
use crate::rule::{Listener, Rule, RuleContext, replace_node, trim_node_range};
use crate::utils::{is_thenable_type, union_type_parts};

fn build_use_unknown_message_base(method: &str) -> String {
    format!("Prefer the safe `: unknown` for a {method} callback variable.")
}

#[derive(Clone, Copy)]
#[expect(
    clippy::enum_variant_names,
    reason = "The variant names preserve the upstream tsgolint message ids."
)]
enum MessageId {
    AddUnknownRestTypeAnnotationSuggestion,
    AddUnknownTypeAnnotationSuggestion,
    WrongRestTypeAnnotationSuggestion,
    WrongTypeAnnotationSuggestion,
}

impl MessageId {
    fn message(self) -> RuleMessage {
        let (id, description) = match self {
            Self::AddUnknownRestTypeAnnotationSuggestion => (
                "addUnknownRestTypeAnnotationSuggestion",
                "Add an explicit `: [unknown]` type annotation to the rejection callback rest variable.",
            ),
            Self::AddUnknownTypeAnnotationSuggestion => (
                "addUnknownTypeAnnotationSuggestion",
                "Add an explicit `: unknown` type annotation to the rejection callback variable.",
            ),
            Self::WrongRestTypeAnnotationSuggestion => (
                "wrongRestTypeAnnotationSuggestion",
                "Change existing type annotation to `: [unknown]`.",
            ),
            Self::WrongTypeAnnotationSuggestion => {
                ("wrongTypeAnnotationSuggestion", "Change existing type annotation to `: unknown`.")
            }
        };
        RuleMessage { id: id.into(), description: description.into(), ..Default::default() }
    }
}

fn build_use_unknown_message(method: &str) -> RuleMessage {
    RuleMessage {
        id: "useUnknown".into(),
        description: build_use_unknown_message_base(method),
        ..Default::default()
    }
}

fn build_use_unknown_array_destructuring_pattern_message(method: &str) -> RuleMessage {
    RuleMessage {
        id: "useUnknownArrayDestructuringPattern".into(),
        description: build_use_unknown_message_base(method)
            + " The thrown error may not be iterable.",
        ..Default::default()
    }
}

fn build_use_unknown_object_destructuring_pattern_message(method: &str) -> RuleMessage {
    RuleMessage {
        id: "useUnknownObjectDestructuringPattern".into(),
        description: build_use_unknown_message_base(method)
            + " The thrown error may be nullable, or may not have the expected shape.",
        ..Default::default()
    }
}

fn is_flaggable_handler_type(context: &mut RuleContext<'_>, t: P<Type>) -> bool {
    for &part in union_type_parts(&t) {
        for &signature in context.checker.get_signatures_of_type(part, SignatureKind::Call) {
            let Some(&first_param) = signature.parameters().first() else {
                continue;
            };
            let mut first_param_type = context.checker.get_type_of_symbol(first_param);
            if first_param
                .value_declaration()
                .is_some_and(|decl| decl.as_parameter_declaration().dot_dot_dot_token().is_some())
            {
                if !context.checker.is_array_or_tuple_type(first_param_type) {
                    return true;
                }
                first_param_type = context.checker.get_type_arguments(first_param_type)[0];
            }
            if !first_param_type.flags().intersects(TypeFlags::Unknown) {
                return true;
            }
        }
    }
    false
}

fn collect_flagged_nodes(context: &mut RuleContext<'_>, node: P<Node>) -> Vec<P<Node>> {
    let node = ast::skip_parentheses(node);
    match node.kind() {
        Kind::BinaryExpression => {
            let binary = node.as_binary_expression();
            if ast::is_logical_expression(node) {
                let mut nodes = collect_flagged_nodes(context, binary.left);
                nodes.extend(collect_flagged_nodes(context, binary.right()));
                return nodes;
            }
            if binary.operator_token.kind() == Kind::CommaToken {
                return collect_flagged_nodes(context, binary.right());
            }
        }
        Kind::ConditionalExpression => {
            let conditional = node.as_conditional_expression();
            let mut nodes = collect_flagged_nodes(context, conditional.when_true);
            nodes.extend(collect_flagged_nodes(context, conditional.when_false));
            return nodes;
        }
        Kind::ArrowFunction | Kind::FunctionExpression => {
            let t = context.checker.get_type_at_location(node);
            if is_flaggable_handler_type(context, t) {
                return vec![node];
            }
        }
        _ => {}
    }
    Vec::new()
}

fn insert_after(node: P<Node>, text: &str) -> Fix {
    let end = node.end().expect_source("parsed callback parameter");
    Fix { range: Range { pos: end, end }, text: text.into() }
}

pub struct UseUnknownInCatchCallbackVariable;

impl Rule for UseUnknownInCatchCallbackVariable {
    fn listens_to(&self, listener: Listener) -> bool {
        listener == Listener::Enter(Kind::CallExpression)
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        let expression = node.as_call_expression();
        let callee = expression.expression;
        if !ast::is_access_expression(callee) {
            return;
        }
        let (property_name, found) = context.checker.get_accessed_property_name(callee);
        if !found {
            return;
        }
        let (method, arg_index_to_check) = match property_name.as_ref() {
            "catch" => ("`catch`", 0),
            "then" => ("`then` rejection", 1),
            _ => return,
        };
        let arguments = expression.arguments.nodes();
        if arguments.len() <= arg_index_to_check {
            return;
        }
        if arguments[..=arg_index_to_check].iter().any(|arg| arg.kind() == Kind::SpreadElement) {
            return;
        }
        let receiver_type = context.checker.get_type_at_location(callee.expression().unwrap());
        if !is_thenable_type(context.checker, callee, receiver_type) {
            return;
        }
        for flagged in collect_flagged_nodes(context, arguments[arg_index_to_check]) {
            let catch_param_node = flagged.parameters()[0];
            let catch_param = catch_param_node.as_parameter_declaration();
            let catch_variable = catch_param.name();
            let catch_type_annotation = catch_param.type_();
            if catch_param.dot_dot_dot_token().is_some() {
                let (id, fixes) = if let Some(annotation) = catch_type_annotation {
                    (
                        MessageId::WrongRestTypeAnnotationSuggestion,
                        vec![replace_node(context.source_file, annotation, "[unknown]".into())],
                    )
                } else {
                    (
                        MessageId::AddUnknownRestTypeAnnotationSuggestion,
                        vec![insert_after(catch_variable, ": [unknown]")],
                    )
                };
                context.report_node_with_suggestions(
                    catch_param_node,
                    build_use_unknown_message(method),
                    || vec![Suggestion { message: id.message(), fixes }],
                );
                continue;
            }
            match catch_variable.kind() {
                Kind::Identifier => {
                    let (id, fixes) = if let Some(annotation) = catch_type_annotation {
                        (
                            MessageId::WrongTypeAnnotationSuggestion,
                            vec![replace_node(context.source_file, annotation, "unknown".into())],
                        )
                    } else {
                        let parenless = flagged.kind() == Kind::ArrowFunction
                            && flagged.parameter_list().unwrap().loc.get().end()
                                == flagged
                                    .as_arrow_function()
                                    .equals_greater_than_token
                                    .unwrap()
                                    .pos();
                        let fixes = if parenless {
                            let pos = trim_node_range(context.source_file, catch_variable).pos;
                            vec![
                                Fix { range: Range { pos, end: pos }, text: "(".into() },
                                insert_after(catch_variable, ": unknown)"),
                            ]
                        } else {
                            vec![insert_after(
                                catch_param.question_token().unwrap_or(catch_variable),
                                ": unknown",
                            )]
                        };
                        (MessageId::AddUnknownTypeAnnotationSuggestion, fixes)
                    };
                    context.report_node_with_suggestions(
                        catch_param_node,
                        build_use_unknown_message(method),
                        || vec![Suggestion { message: id.message(), fixes }],
                    );
                }
                Kind::ArrayBindingPattern => context.report_node(
                    catch_param_node,
                    build_use_unknown_array_destructuring_pattern_message(method),
                ),
                Kind::ObjectBindingPattern => context.report_node(
                    catch_param_node,
                    build_use_unknown_object_destructuring_pattern_message(method),
                ),
                _ => {}
            }
        }
    }
}
