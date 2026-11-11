//! Port of tsgolint's `internal/rules/no_meaningless_void_operator/no_meaningless_void_operator.go`.

use tsrs_ast::{self as ast, Kind, Node};
use tsrs_checker::TypeFlags;
use tsrs_core::{P, json::Value};

use crate::protocol::{Diagnostic, Fix, RuleMessage, Suggestion};
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};
use crate::utils::union_type_parts;

pub struct NoMeaninglessVoidOperator {
    check_never: bool,
}

impl NoMeaninglessVoidOperator {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("no-meaningless-void-operator: options must be an object".into()),
        };
        let check_never = match object.and_then(|object| object.get("checkNever")) {
            None | Some(Value::Null) => false,
            Some(Value::Bool(value)) => *value,
            _ => return Err("no-meaningless-void-operator: checkNever must be a boolean".into()),
        };
        Ok(Self { check_never })
    }
}

impl Rule for NoMeaninglessVoidOperator {
    fn listens_to(&self, listener: Listener) -> bool {
        listener == Listener::Enter(Kind::VoidExpression)
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        let argument = node.as_void_expression().expression();
        let inner = ast::skip_outer_expressions(
            argument,
            ast::OEK::Parentheses | ast::OEK::Assertions | ast::OEK::Comma,
        );
        if ast::is_assignment_expression(inner, false) {
            return;
        }
        let t = context.checker.get_type_at_location(argument);
        let parts = union_type_parts(&t);
        let always_void_like = parts.iter().all(|t| t.flags().intersects(TypeFlags::VoidLike));
        let always_void_like_or_never =
            parts.iter().all(|t| t.flags().intersects(TypeFlags::VoidLike | TypeFlags::Never));
        if !(always_void_like || self.check_never && always_void_like_or_never) {
            return;
        }
        let text = context.checker.type_to_string_exported(t);
        let message = RuleMessage {
            id: "meaninglessVoidOperator".into(),
            description: format!(
                "void operator shouldn't be used on {text}; it should convey that a return value is being ignored"
            ),
            ..Default::default()
        };
        let remove_void = || {
            let mut range = trim_node_range(context.source_file, node);
            range.end = argument.pos().expect_source("parsed lint node");
            Fix { range, text: String::new() }
        };
        let fixes =
            if always_void_like && context.options.fix { vec![remove_void()] } else { Vec::new() };
        let suggestions = if !always_void_like && context.options.fix_suggestions {
            vec![Suggestion {
                message: RuleMessage {
                    id: "removeVoid".into(),
                    description: "Remove 'void'".into(),
                    ..Default::default()
                },
                fixes: vec![remove_void()],
            }]
        } else {
            Vec::new()
        };
        context.report_diagnostic(Diagnostic {
            range: Some(trim_node_range(context.source_file, node)),
            message,
            fixes,
            suggestions,
            ..Default::default()
        });
    }
}
