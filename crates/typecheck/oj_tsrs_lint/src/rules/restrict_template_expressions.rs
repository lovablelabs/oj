use tsrs_ast::{Kind, Node};
use tsrs_checker::{Type, TypeFlags};
use tsrs_core::{P, json::Value};

use crate::protocol::{Diagnostic, LabeledRange, RuleMessage};
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};
use crate::type_matches_specifier::{TypeOrValueSpecifier, type_matches_some_specifier};
use crate::utils;

pub struct RestrictTemplateExpressions {
    allow: Vec<TypeOrValueSpecifier>,
    allowed_flags: TypeFlags,
    allow_array: bool,
    allow_reg_exp: bool,
}

impl RestrictTemplateExpressions {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("restrict-template-expressions: options must be an object".into()),
        };
        let boolean = |name: &str, default| match object.and_then(|object| object.get(name)) {
            None | Some(Value::Null) => Ok(default),
            Some(Value::Bool(value)) => Ok(*value),
            _ => Err(format!("restrict-template-expressions: {name} must be a boolean")),
        };
        let allow = match object.and_then(|object| object.get("allow")) {
            None | Some(Value::Null) => {
                vec![TypeOrValueSpecifier::new(&crate::protocol::object([
                    ("from", Value::String("lib".into())),
                    (
                        "name",
                        Value::Array(
                            ["Error", "URL", "URLSearchParams"]
                                .map(|name| Value::String(name.into()))
                                .to_vec(),
                        ),
                    ),
                ]))?]
            }
            Some(Value::Array(values)) => {
                values.iter().map(TypeOrValueSpecifier::new).collect::<Result<_, _>>()?
            }
            _ => return Err("restrict-template-expressions: allow must be an array".into()),
        };
        let mut allowed_flags = TypeFlags::StringLike;
        for (name, default, flags) in [
            ("allowAny", true, TypeFlags::Any),
            ("allowBoolean", true, TypeFlags::BooleanLike),
            ("allowNullish", true, TypeFlags::Nullable),
            ("allowNumber", true, TypeFlags::NumberLike | TypeFlags::BigIntLike),
            ("allowNever", false, TypeFlags::Never),
        ] {
            if boolean(name, default)? {
                allowed_flags |= flags;
            }
        }
        Ok(Self {
            allow,
            allowed_flags,
            allow_array: boolean("allowArray", false)?,
            allow_reg_exp: boolean("allowRegExp", true)?,
        })
    }

    fn is_type_allowed(&self, context: &mut RuleContext<'_>, inner_type: P<Type>) -> bool {
        utils::union_type_parts(&inner_type).iter().all(|t| {
            utils::intersection_type_parts(t).iter().any(|&t| {
                t.flags().intersects(self.allowed_flags)
                    || utils::matches_type_or_base_type(context.checker, t, &mut |_, t| {
                        type_matches_some_specifier(t, &self.allow, context.program)
                    })
                    || (self.allow_array && context.checker.is_array_or_tuple_type(t) && {
                        let element = context
                            .checker
                            .get_index_type_of_type(t, context.checker.number_type)
                            .expect("array or tuple number index type");
                        self.is_type_allowed(context, element)
                    })
                    || (self.allow_reg_exp && t == context.checker.global_reg_exp_type)
            })
        })
    }
}

impl Rule for RestrictTemplateExpressions {
    fn listens_to(&self, listener: Listener) -> bool {
        listener == Listener::Enter(Kind::TemplateExpression)
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        if node.parent().is_some_and(|parent| parent.kind() == Kind::TaggedTemplateExpression) {
            return;
        }
        for &span in node.as_template_expression().template_spans.nodes() {
            let expression = span.expression().expect("template span expression");
            let expression_type =
                utils::get_constrained_type_at_location(context.checker, expression);
            if !self.is_type_allowed(context, expression_type) {
                let range = trim_node_range(context.source_file, expression);
                let type_name = context.checker.type_to_string_exported(expression_type);
                context.report_diagnostic(Diagnostic {
                    range: Some(range),
                    message: RuleMessage {
                        id: "invalidType".into(),
                        description: "Invalid type used in template literal expression.".into(),
                        help: String::new(),
                    },
                    labeled_ranges: vec![LabeledRange {
                        label: format!("Type: {type_name}"),
                        range,
                    }],
                    ..Default::default()
                });
            }
        }
    }
}
