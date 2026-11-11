use tsrs_ast::{self as ast, Kind, Node};
use tsrs_checker::TypeFlags;
use tsrs_core::{P, json::Value};

use crate::protocol::RuleMessage;
use crate::rule::{Listener, Rule, RuleContext};
use crate::utils::{get_constrained_type_at_location, get_type_name, union_type_parts};

fn build_require_compare_message() -> RuleMessage {
    RuleMessage {
        id: "requireCompare".into(),
        description: "Require 'compare' argument.".into(),
        ..Default::default()
    }
}

pub struct RequireArraySortCompare {
    ignore_string_arrays: bool,
}

impl RequireArraySortCompare {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("require-array-sort-compare: options must be an object".into()),
        };
        let ignore_string_arrays = match object.and_then(|object| object.get("ignoreStringArrays"))
        {
            None | Some(Value::Null) => true,
            Some(Value::Bool(value)) => *value,
            _ => {
                return Err(
                    "require-array-sort-compare: ignoreStringArrays must be a boolean".into()
                );
            }
        };
        Ok(Self { ignore_string_arrays })
    }
}

impl Rule for RequireArraySortCompare {
    fn listens_to(&self, listener: Listener) -> bool {
        listener == Listener::Enter(Kind::CallExpression)
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        let expression = node.as_call_expression();
        if !expression.arguments.nodes().is_empty() {
            return;
        }
        let callee = expression.expression;
        if !ast::is_access_expression(callee) {
            return;
        }
        let (property_name, found) = context.checker.get_accessed_property_name(callee);
        if !found || !matches!(property_name.as_ref(), "sort" | "toSorted") {
            return;
        }
        let callee_obj_type =
            get_constrained_type_at_location(context.checker, callee.expression().unwrap());
        if self.ignore_string_arrays && context.checker.is_array_or_tuple_type(callee_obj_type) {
            let arguments = context.checker.get_type_arguments(callee_obj_type);
            if arguments.iter().all(|&t| {
                let t = context.checker.get_base_constraint_of_type(t).unwrap_or(t);
                t.flags().intersects(TypeFlags::String)
                    || get_type_name(context.checker, t) == "string"
            }) {
                return;
            }
        }
        if union_type_parts(&callee_obj_type)
            .iter()
            .all(|&t| context.checker.is_array_type(t) || t.is_tuple_type())
        {
            context.report_node(node, build_require_compare_message());
        }
    }
}
