use tsrs_ast::{self as ast, Kind, Node};
use tsrs_core::P;

use crate::protocol::RuleMessage;
use crate::rule::{Listener, Rule, RuleContext, replace_node};

fn build_use_this_type_message() -> RuleMessage {
    RuleMessage {
        id: "useThisType".into(),
        description: "Use `this` type instead.".into(),
        ..Default::default()
    }
}

fn try_get_name_in_type_node(name: &str, node: P<Node>) -> Option<P<Node>> {
    let node = ast::skip_parentheses(node);
    if node.kind() == Kind::TypeReference {
        let type_name = node.as_type_reference_node().type_name;
        if type_name.kind() == Kind::Identifier && type_name.text() == name {
            return Some(node);
        }
    } else if node.kind() == Kind::UnionType {
        for &ty in node.as_union_type_node().types().nodes() {
            if let Some(found) = try_get_name_in_type_node(name, ty) {
                return Some(found);
            }
        }
    }
    None
}

fn check_function(context: &mut RuleContext<'_>, function: P<Node>, original_class: P<Node>) {
    let (Some(return_type), Some(body)) = (function.type_node(), function.body()) else {
        return;
    };
    let Some(class_name) = original_class.name() else {
        return;
    };
    let Some(node) = try_get_name_in_type_node(class_name.text(), return_type) else {
        return;
    };
    if let Some(first_argument) = function.parameters().first().and_then(|param| param.name())
        && first_argument.kind() == Kind::Identifier
        && first_argument.text() == "this"
    {
        return;
    }

    let class_type = context.checker.get_type_at_location(original_class);
    let interface_type = class_type.try_as_interface_type();
    if body.kind() == Kind::Block {
        let mut has_return_this = false;
        let has_return_class_type = ast::for_each_return_statement(body, |statement| {
            let Some(expression) = statement.expression() else {
                return false;
            };
            if expression.kind() == Kind::ThisKeyword {
                has_return_this = true;
                return false;
            }
            let Some(interface_type) = interface_type else {
                return false;
            };
            let ty = context.checker.get_type_at_location(expression);
            if class_type == ty {
                return true;
            }
            if interface_type.this_type() == Some(ty) {
                has_return_this = true;
            }
            false
        });
        if has_return_class_type || !has_return_this {
            return;
        }
    } else {
        let Some(interface_type) = interface_type else {
            return;
        };
        let ty = context.checker.get_type_at_location(body);
        if interface_type.this_type() != Some(ty) {
            return;
        }
    }

    let source_file = context.source_file;
    context.report_node_with_fixes(node, build_use_this_type_message(), || {
        vec![replace_node(source_file, node, "this".into())]
    });
}

pub struct PreferReturnThisType;

impl Rule for PreferReturnThisType {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(listener, Listener::Enter(Kind::PropertyDeclaration | Kind::MethodDeclaration))
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        let Some(parent) = node.parent().filter(|&parent| ast::is_class_like(parent)) else {
            return;
        };
        if node.kind() == Kind::PropertyDeclaration {
            let Some(initializer) = node.initializer() else {
                return;
            };
            if ast::is_function_expression_or_arrow_function(initializer) {
                check_function(context, initializer, parent);
            }
        } else {
            check_function(context, node, parent);
        }
    }
}
