use tsrs_ast::{Kind, Node};
use tsrs_checker::{Type, TypeFlags};
use tsrs_core::{P, json::Value};

use crate::protocol::{Diagnostic, LabeledRange, Range, RuleMessage};
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};
use crate::utils;

fn build_bigint_and_number_diagnostic(
    expr_range: Range,
    left_range: Range,
    right_range: Range,
    left_type: &str,
    right_type: &str,
) -> Diagnostic {
    Diagnostic {
        range: Some(expr_range),
        message: RuleMessage {
            id: "bigintAndNumber".into(),
            description: "Numeric '+' operations must either be both bigints or both numbers."
                .into(),
            ..Default::default()
        },
        labeled_ranges: vec![
            LabeledRange { label: format!("Type: {left_type}"), range: left_range },
            LabeledRange { label: format!("Type: {right_type}"), range: right_range },
        ],
        ..Default::default()
    }
}

fn build_invalid_diagnostic(
    expr_range: Range,
    invalid_type: &str,
    string_like: &str,
) -> Diagnostic {
    Diagnostic {
        range: Some(expr_range),
        message: RuleMessage {
            id: "invalid".into(),
            description: format!("Invalid operand of type '{invalid_type}' for a '+' operation."),
            help: format!("Operands must each be a number or {string_like}."),
        },
        ..Default::default()
    }
}

fn build_mismatched_diagnostic(
    expr_range: Range,
    left_range: Range,
    right_range: Range,
    string_like: &str,
    left_type: &str,
    right_type: &str,
) -> Diagnostic {
    Diagnostic {
        range: Some(expr_range),
        message: RuleMessage {
            id: "mismatched".into(),
            description: "Operands of '+' operations must be of the same type.".into(),
            help: format!("Operands must both be a number or both be {string_like}."),
        },
        labeled_ranges: vec![
            LabeledRange { label: format!("Type: {left_type}"), range: left_range },
            LabeledRange { label: format!("Type: {right_type}"), range: right_range },
        ],
        ..Default::default()
    }
}

pub struct RestrictPlusOperands {
    allow_any: bool,
    allow_number_and_string: bool,
    allow_reg_exp: bool,
    skip_compound_assignments: bool,
    string_like: String,
    invalid_flags: TypeFlags,
}

impl RestrictPlusOperands {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("restrict-plus-operands: options must be an object".into()),
        };
        let boolean = |name: &str, default| match object.and_then(|object| object.get(name)) {
            None | Some(Value::Null) => Ok(default),
            Some(Value::Bool(value)) => Ok(*value),
            _ => Err(format!("restrict-plus-operands: {name} must be a boolean")),
        };
        let allow_any = boolean("allowAny", true)?;
        let allow_boolean = boolean("allowBoolean", true)?;
        let allow_nullish = boolean("allowNullish", true)?;
        let allow_number_and_string = boolean("allowNumberAndString", true)?;
        let allow_reg_exp = boolean("allowRegExp", true)?;
        let skip_compound_assignments = boolean("skipCompoundAssignments", false)?;
        let mut string_likes = Vec::new();
        if allow_any {
            string_likes.push("`any`");
        }
        if allow_boolean {
            string_likes.push("`boolean`");
        }
        if allow_nullish {
            string_likes.push("`null`");
        }
        if allow_reg_exp {
            string_likes.push("`RegExp`");
        }
        if allow_nullish {
            string_likes.push("`undefined`");
        }
        let string_like = match string_likes.as_slice() {
            [] => "string".into(),
            [only] => format!("string, allowing a string + {only}"),
            _ => format!("string, allowing a string + any of: {}", string_likes.join(", ")),
        };
        let mut invalid_flags = TypeFlags::ESSymbolLike | TypeFlags::Never | TypeFlags::Unknown;
        if !allow_any {
            invalid_flags |= TypeFlags::Any;
        }
        if !allow_boolean {
            invalid_flags |= TypeFlags::BooleanLike;
        }
        if !allow_nullish {
            invalid_flags |= TypeFlags::Nullable;
        }
        Ok(Self {
            allow_any,
            allow_number_and_string,
            allow_reg_exp,
            skip_compound_assignments,
            string_like,
            invalid_flags,
        })
    }

    fn check_invalid_plus_operand(
        &self,
        base_type: P<Type>,
        other_type: P<Type>,
        context: &mut RuleContext<'_>,
    ) -> (TypeFlags, Option<String>) {
        let mut found_invalid = false;
        let mut flags = TypeFlags::None;
        let base_type_string = context.checker.type_to_string_exported(base_type);
        for &part in utils::union_type_parts(&base_type) {
            flags |= part.flags();
            if part.flags().intersects(self.invalid_flags) {
                return (flags, Some(base_type_string));
            }
            if part == context.checker.global_reg_exp_type {
                if self.allow_reg_exp && !other_type.flags().intersects(TypeFlags::NumberLike) {
                    continue;
                }
            } else if (self.allow_any || !part.flags().intersects(TypeFlags::Any))
                && !utils::intersection_type_parts(&part)
                    .iter()
                    .all(|part| part.flags().intersects(TypeFlags::Object))
            {
                continue;
            }
            found_invalid = true;
        }
        (flags, found_invalid.then_some(base_type_string))
    }

    fn check_plus_operands(&self, node: P<Node>, context: &mut RuleContext<'_>) {
        let expression = node.as_binary_expression();
        let left_type = utils::get_constrained_type_at_location(context.checker, expression.left);
        let left_type = context.checker.get_base_type_of_literal_type(left_type);
        let right_type =
            utils::get_constrained_type_at_location(context.checker, expression.right());
        let right_type = context.checker.get_base_type_of_literal_type(right_type);
        let left_type_string = context.checker.type_to_string_exported(left_type);
        let right_type_string = context.checker.type_to_string_exported(right_type);
        let left_range = trim_node_range(context.source_file, expression.left);
        let right_range = trim_node_range(context.source_file, expression.right());
        if left_type == right_type
            && left_type
                .flags()
                .intersects(TypeFlags::BigIntLike | TypeFlags::NumberLike | TypeFlags::StringLike)
        {
            return;
        }
        let (left_flags, left_invalid) =
            self.check_invalid_plus_operand(left_type, right_type, context);
        let (right_flags, right_invalid) =
            self.check_invalid_plus_operand(right_type, left_type, context);
        if let Some(invalid_type) = &left_invalid {
            context.report_diagnostic(build_invalid_diagnostic(
                left_range,
                invalid_type,
                &self.string_like,
            ));
        }
        if let Some(invalid_type) = &right_invalid {
            context.report_diagnostic(build_invalid_diagnostic(
                right_range,
                invalid_type,
                &self.string_like,
            ));
        }
        if left_invalid.is_some() || right_invalid.is_some() {
            return;
        }
        for (base_flags, other_flags) in [(left_flags, right_flags), (right_flags, left_flags)] {
            if !self.allow_number_and_string
                && base_flags.intersects(TypeFlags::StringLike)
                && other_flags.intersects(TypeFlags::NumberLike | TypeFlags::BigIntLike)
            {
                context.report_diagnostic(build_mismatched_diagnostic(
                    trim_node_range(context.source_file, node),
                    left_range,
                    right_range,
                    &self.string_like,
                    &left_type_string,
                    &right_type_string,
                ));
                return;
            }
            if base_flags.intersects(TypeFlags::NumberLike)
                && other_flags.intersects(TypeFlags::BigIntLike)
            {
                context.report_diagnostic(build_bigint_and_number_diagnostic(
                    trim_node_range(context.source_file, node),
                    left_range,
                    right_range,
                    &left_type_string,
                    &right_type_string,
                ));
                return;
            }
        }
    }
}

impl Rule for RestrictPlusOperands {
    fn listens_to(&self, listener: Listener) -> bool {
        listener == Listener::Enter(Kind::BinaryExpression)
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        let operator = node.as_binary_expression().operator_token.kind();
        if operator == Kind::PlusToken
            || (!self.skip_compound_assignments && operator == Kind::PlusEqualsToken)
        {
            self.check_plus_operands(node, context);
        }
    }
}
