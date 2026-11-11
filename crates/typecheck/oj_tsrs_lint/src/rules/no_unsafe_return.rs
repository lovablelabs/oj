//! Port of tsgolint's `internal/rules/no_unsafe_return/no_unsafe_return.go`.

use tsrs_ast::{self as ast, Kind, ModifierFlags, Node};
use tsrs_checker::{Signature, Type, TypeFlags};
use tsrs_core::P;

use crate::protocol::RuleMessage;
use crate::rule::{Listener, Rule, RuleContext};
use crate::utils::{self, DiscriminatedAnyType};

pub struct NoUnsafeReturn;

fn report(
    context: &mut RuleContext<'_>,
    reporting_node: P<Node>,
    returned_type: P<Type>,
    signatures: &[P<Signature>],
    expects_type: bool,
    mut message: RuleMessage,
) {
    let returned = if utils::is_intrinsic_error_type(returned_type) {
        "error".into()
    } else {
        let Ok(text) =
            String::from_utf8(context.checker.type_to_string_bytes_exported(returned_type))
        else {
            return;
        };
        text
    };
    let mut expected = None;
    if expects_type && let Some(&signature) = signatures.first() {
        let ty = context.checker.get_return_type_of_signature_exported(signature);
        expected = Some(if utils::is_intrinsic_error_type(ty) {
            "error".into()
        } else {
            let Ok(text) = String::from_utf8(context.checker.type_to_string_bytes_exported(ty))
            else {
                return;
            };
            text
        });
    }
    let help = if let Some(expected) = expected {
        format!(
            "Returned expression has type `{returned}`; the function return type is `{expected}`."
        )
    } else {
        format!("Returned expression has type `{returned}`.")
    };
    message.help = format!(
        "{help} {}",
        if message.help.is_empty() {
            "Return a safely typed value or narrow the value before returning it."
        } else {
            &message.help
        }
    );
    context.report_node(reporting_node, message);
}

fn check_return(context: &mut RuleContext<'_>, return_node: P<Node>, reporting_node: P<Node>) {
    let mut ancestor = return_node.parent();
    let function = loop {
        let Some(node) = ancestor else {
            return;
        };
        if ast::is_function_like_declaration(node) {
            break node;
        }
        ancestor = node.parent();
    };
    let return_node_type = context.checker.get_type_at_location(return_node);
    let any_type = utils::discriminate_any_type(context.checker, return_node_type, return_node);
    let constrained_return_node_type =
        utils::get_constrained_type_at_location(context.checker, return_node);
    let contextual = if matches!(function.kind(), Kind::FunctionExpression | Kind::ArrowFunction) {
        utils::get_contextual_type(context.checker, function)
    } else {
        None
    };
    let function_type =
        contextual.unwrap_or_else(|| context.checker.get_type_at_location(function));
    let signatures = utils::collect_all_call_signatures(context.checker, function_type);
    let is_async = ast::has_syntactic_modifier(function, ModifierFlags::Async);
    if function.type_node().is_some() {
        for &signature in &signatures {
            let signature_return_type =
                context.checker.get_return_type_of_signature_exported(signature);
            if return_node_type == signature_return_type
                || signature_return_type.flags().intersects(TypeFlags::Any | TypeFlags::Unknown)
            {
                return;
            }
            if is_async {
                let awaited_signature = context.checker.get_awaited_type(signature_return_type);
                let awaited_return = context.checker.get_awaited_type(return_node_type);
                if awaited_signature == awaited_return
                    || awaited_signature.is_some_and(|ty| {
                        ty.flags().intersects(TypeFlags::Any | TypeFlags::Unknown)
                    })
                {
                    return;
                }
            }
        }
    }
    if any_type != DiscriminatedAnyType::Safe {
        for &signature in &signatures {
            let function_return_type =
                context.checker.get_return_type_of_signature_exported(signature);
            if any_type == DiscriminatedAnyType::Any
                && function_return_type.flags().intersects(TypeFlags::Unknown)
            {
                return;
            }
            if any_type == DiscriminatedAnyType::AnyArray
                && context.checker.is_array_type(function_return_type)
                && context.checker.get_type_arguments(function_return_type)[0]
                    .flags()
                    .intersects(TypeFlags::Unknown)
            {
                return;
            }
            let awaited = context.checker.get_awaited_type(function_return_type);
            if awaited.is_some_and(|ty| {
                any_type == DiscriminatedAnyType::PromiseAny
                    && ty.flags().intersects(TypeFlags::Unknown)
            }) {
                return;
            }
        }
        if any_type == DiscriminatedAnyType::PromiseAny && !is_async {
            return;
        }
        let type_string = if utils::is_intrinsic_error_type(constrained_return_node_type) {
            "error"
        } else {
            match any_type {
                DiscriminatedAnyType::Any => "`any`",
                DiscriminatedAnyType::PromiseAny => "`Promise<any>`",
                DiscriminatedAnyType::AnyArray => "`any[]`",
                DiscriminatedAnyType::Safe => unreachable!("unsafe return classification"),
            }
        };
        let options = context.program.options();
        if !options.get_strict_option_value(options.no_implicit_this)
            && let Some(this) = utils::get_this_expression(return_node)
            && utils::get_constrained_type_at_location(context.checker, this)
                .flags()
                .intersects(TypeFlags::Any)
        {
            report(context, reporting_node, constrained_return_node_type, &signatures, function.type_node().is_some() || contextual.is_some(), RuleMessage {
                        id: "unsafeReturnThis".into(),
                        description: format!("Unsafe return of a value of type {type_string}. `this` is typed as `any`."),
                        help: "Enable `noImplicitThis` or add an explicit `this` parameter to the function.".into(),
                    });
            return;
        }
        report(
            context,
            reporting_node,
            constrained_return_node_type,
            &signatures,
            function.type_node().is_some() || contextual.is_some(),
            RuleMessage {
                id: "unsafeReturn".into(),
                description: format!("Unsafe return of a value of type {type_string}."),
                ..Default::default()
            },
        );
        return;
    }
    let Some(&signature) = signatures.first() else {
        return;
    };
    let function_return_type = context.checker.get_return_type_of_signature_exported(signature);
    if let Some((receiver, sender)) = utils::is_unsafe_assignment(
        return_node_type,
        function_return_type,
        context.checker,
        Some(return_node),
    ) {
        let Ok(sender) = String::from_utf8(context.checker.type_to_string_bytes_exported(sender))
        else {
            return;
        };
        let Ok(receiver) =
            String::from_utf8(context.checker.type_to_string_bytes_exported(receiver))
        else {
            return;
        };
        report(
            context,
            reporting_node,
            constrained_return_node_type,
            &signatures,
            function.type_node().is_some() || contextual.is_some(),
            RuleMessage {
                id: "unsafeReturnAssignment".into(),
                description: format!(
                    "Unsafe return of type `{sender}` from function with return type `{receiver}`."
                ),
                ..Default::default()
            },
        );
    }
}

impl Rule for NoUnsafeReturn {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(listener, Listener::Enter(Kind::ArrowFunction | Kind::ReturnStatement))
    }
    fn visit(&mut self, listener: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        match listener {
            Listener::Enter(Kind::ArrowFunction) => {
                let body = node.body().expect("arrow function body");
                if body.kind() != Kind::Block {
                    check_return(context, body, body);
                }
            }
            Listener::Enter(Kind::ReturnStatement) => {
                if let Some(argument) = node.expression() {
                    check_return(context, argument, node);
                }
            }
            _ => unreachable!("no-unsafe-return listener"),
        }
    }
}
