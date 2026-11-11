//! Port of tsgolint's `internal/rules/no_base_to_string/no_base_to_string.go`.

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::{self as ast, Kind, Node};
use tsrs_checker::{self as checker, Checker, Type, TypeFlags};
use tsrs_core::{P, json::Value};

use crate::protocol::RuleMessage;
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};
use crate::utils::{
    get_constrained_type_at_location, get_type_name, is_builtin_symbol_like,
    matches_type_or_base_type,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Usefulness {
    Always,
    Never,
    Sometimes,
}

impl Usefulness {
    fn description(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Never => "will",
            Self::Sometimes => "may",
        }
    }
}

#[derive(Default)]
struct CertaintyMemo {
    values: FxHashMap<P<Type>, Usefulness>,
    resolving: FxHashSet<P<Type>>,
}

impl CertaintyMemo {
    fn start(&mut self, t: P<Type>) -> Option<Usefulness> {
        if let Some(&value) = self.values.get(&t) {
            return Some(value);
        }
        if !self.resolving.insert(t) {
            return Some(Usefulness::Always);
        }
        None
    }

    fn finish(&mut self, t: P<Type>, value: Usefulness) -> Usefulness {
        self.resolving.remove(&t);
        self.values.insert(t, value);
        value
    }
}

pub struct NoBaseToString {
    check_unknown: bool,
    ignored_type_names: Vec<String>,
    to_string_memo: CertaintyMemo,
    join_memo: CertaintyMemo,
}

impl NoBaseToString {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("no-base-to-string: options must be an object".into()),
        };
        let check_unknown = match object.and_then(|object| object.get("checkUnknown")) {
            None | Some(Value::Null) => false,
            Some(Value::Bool(value)) => *value,
            _ => return Err("no-base-to-string: checkUnknown must be a boolean".into()),
        };
        let ignored_type_names = match object.and_then(|object| object.get("ignoredTypeNames")) {
            None | Some(Value::Null) => {
                ["Error", "RegExp", "URL", "URLSearchParams"].map(String::from).to_vec()
            }
            Some(Value::Array(names)) => names
                .iter()
                .map(|name| match name {
                    Value::String(name) => Ok(name.clone()),
                    Value::Null => Ok(String::new()),
                    _ => Err("no-base-to-string: ignoredTypeNames must contain strings".into()),
                })
                .collect::<Result<_, String>>()?,
            _ => return Err("no-base-to-string: ignoredTypeNames must be an array".into()),
        };
        Ok(Self {
            check_unknown,
            ignored_type_names,
            to_string_memo: CertaintyMemo::default(),
            join_memo: CertaintyMemo::default(),
        })
    }

    fn collect_union_type_certainty(
        &mut self,
        checker: &mut Checker,
        t: P<Type>,
        visited: &[P<Type>],
        join: bool,
    ) -> Usefulness {
        let mut all_never = true;
        let mut all_always = true;
        for &part in t.types() {
            let value = if join {
                self.collect_join_certainty(checker, part, visited)
            } else {
                self.collect_to_string_certainty(checker, part, visited)
            };
            all_never &= value == Usefulness::Never;
            all_always &= value == Usefulness::Always;
        }
        if all_never {
            Usefulness::Never
        } else if all_always {
            Usefulness::Always
        } else {
            Usefulness::Sometimes
        }
    }

    fn collect_intersection_type_certainty(
        &mut self,
        checker: &mut Checker,
        t: P<Type>,
        visited: &[P<Type>],
        join: bool,
    ) -> Usefulness {
        if t.types().iter().any(|&part| {
            let value = if join {
                self.collect_join_certainty(checker, part, visited)
            } else {
                self.collect_to_string_certainty(checker, part, visited)
            };
            value == Usefulness::Always
        }) {
            Usefulness::Always
        } else {
            Usefulness::Never
        }
    }

    fn collect_tuple_certainty(
        &mut self,
        checker: &mut Checker,
        t: P<Type>,
        visited: &[P<Type>],
    ) -> Usefulness {
        let mut never = false;
        let mut sometimes = false;
        for &argument in checker.get_type_arguments(t) {
            let value = self.collect_to_string_certainty(checker, argument, visited);
            never |= value == Usefulness::Never;
            sometimes |= value == Usefulness::Sometimes;
        }
        if never {
            Usefulness::Never
        } else if sometimes {
            Usefulness::Sometimes
        } else {
            Usefulness::Always
        }
    }

    fn collect_array_certainty(
        &mut self,
        checker: &mut Checker,
        t: P<Type>,
        visited: &[P<Type>],
    ) -> Usefulness {
        let element = checker
            .get_index_type_of_type(t, checker.number_type)
            .expect("array should have number index type");
        self.collect_to_string_certainty(checker, element, visited)
    }

    fn collect_join_certainty(
        &mut self,
        checker: &mut Checker,
        t: P<Type>,
        visited: &[P<Type>],
    ) -> Usefulness {
        if let Some(value) = self.join_memo.start(t) {
            return value;
        }
        let value = if t.flags().intersects(TypeFlags::Union) {
            self.collect_union_type_certainty(checker, t, visited, true)
        } else if t.flags().intersects(TypeFlags::Intersection) {
            self.collect_intersection_type_certainty(checker, t, visited, true)
        } else if checker::is_tuple_type_exported(t) {
            self.collect_tuple_certainty(checker, t, visited)
        } else if checker.is_array_type(t) {
            self.collect_array_certainty(checker, t, visited)
        } else {
            Usefulness::Always
        };
        self.join_memo.finish(t, value)
    }

    fn collect_to_string_certainty(
        &mut self,
        checker: &mut Checker,
        t: P<Type>,
        visited: &[P<Type>],
    ) -> Usefulness {
        if visited.contains(&t) {
            return Usefulness::Always;
        }
        if let Some(value) = self.to_string_memo.start(t) {
            return value;
        }
        let value = self.compute_to_string_certainty(checker, t, visited);
        self.to_string_memo.finish(t, value)
    }

    fn compute_to_string_certainty(
        &mut self,
        checker: &mut Checker,
        t: P<Type>,
        visited: &[P<Type>],
    ) -> Usefulness {
        if t.flags().intersects(TypeFlags::TypeParameter) {
            return if let Some(constraint) = checker.get_base_constraint_of_type(t) {
                self.collect_to_string_certainty(checker, constraint, visited)
            } else if self.check_unknown {
                Usefulness::Sometimes
            } else {
                Usefulness::Always
            };
        }
        if t.flags().intersects(TypeFlags::BooleanLike) {
            return Usefulness::Always;
        }
        if matches_type_or_base_type(checker, t, &mut |checker, t| {
            let name = get_type_name(checker, t);
            self.ignored_type_names.iter().any(|ignored| ignored == name.as_ref())
        }) {
            return Usefulness::Always;
        }
        if t.flags().intersects(TypeFlags::Intersection) {
            return self.collect_intersection_type_certainty(checker, t, visited, false);
        }
        if t.flags().intersects(TypeFlags::Union) {
            return self.collect_union_type_certainty(checker, t, visited, false);
        }
        if checker::is_tuple_type_exported(t) || checker.is_array_type(t) {
            let mut visited = visited.to_vec();
            visited.push(t);
            return if checker::is_tuple_type_exported(t) {
                self.collect_tuple_certainty(checker, t, &visited)
            } else {
                self.collect_array_certainty(checker, t, &visited)
            };
        }
        let mut found_fallback_on_object = false;
        for name in ["toString", "toLocaleString", "valueOf"] {
            let Some(property) = checker.get_property_of_type(t, name) else {
                continue;
            };
            let declarations = property.declarations();
            if declarations.is_empty() {
                continue;
            }
            if declarations.iter().any(|declaration| {
                !declaration.parent().is_some_and(|parent| {
                    parent.kind() == Kind::InterfaceDeclaration
                        && parent.as_interface_declaration().name().text() == "Object"
                })
            }) {
                return Usefulness::Always;
            }
            found_fallback_on_object = true;
        }
        if found_fallback_on_object {
            Usefulness::Never
        } else if self.check_unknown && t.flags().intersects(TypeFlags::Unknown) {
            Usefulness::Sometimes
        } else {
            Usefulness::Always
        }
    }

    fn report(node: P<Node>, certainty: Usefulness, join: bool, context: &mut RuleContext<'_>) {
        if certainty == Usefulness::Always {
            return;
        }
        let range = trim_node_range(context.source_file, node);
        let name = &context.source_file.text()[range.pos as usize..range.end as usize];
        let (id, description, help) = if join {
            (
                "baseArrayJoin",
                format!(
                    "Using `join()` for {name} {} use Object's default stringification format ('[object Object]') when stringified.",
                    certainty.description()
                ),
                "Consider mapping the values to a meaningful string (e.g. pick a property or call a formatter) before calling `join()`, or implementing a custom `toString()`/`toLocaleString()` on the element type.",
            )
        } else {
            (
                "baseToString",
                format!(
                    "'{name}' {} use Object's default stringification format ('[object Object]') when stringified.",
                    certainty.description()
                ),
                "Consider picking a property (e.g. `user.name`), using a formatter (or `JSON.stringify`), or implementing a custom `toString()`/`toLocaleString()` on the type.",
            )
        };
        context.report_range(range, RuleMessage { id: id.into(), description, help: help.into() });
    }

    fn check_expression(
        &mut self,
        node: P<Node>,
        t: Option<P<Type>>,
        context: &mut RuleContext<'_>,
    ) {
        if ast::is_literal_expression(node) {
            return;
        }
        let t = t.unwrap_or_else(|| context.checker.get_type_at_location(node));
        let certainty = self.collect_to_string_certainty(context.checker, t, &[]);
        Self::report(node, certainty, false, context);
    }
}

impl Rule for NoBaseToString {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(
            listener,
            Listener::Enter(
                Kind::BinaryExpression | Kind::CallExpression | Kind::TemplateExpression
            )
        )
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        match node.kind() {
            Kind::BinaryExpression => {
                let expression = node.as_binary_expression();
                if !matches!(
                    expression.operator_token.kind(),
                    Kind::PlusToken | Kind::PlusEqualsToken
                ) {
                    return;
                }
                let left = context.checker.get_type_at_location(expression.left);
                let right = context.checker.get_type_at_location(expression.right());
                if get_type_name(context.checker, left) == "string" {
                    self.check_expression(expression.right(), Some(right), context);
                } else if get_type_name(context.checker, right) == "string"
                    && expression.left.kind() != Kind::PrivateIdentifier
                {
                    self.check_expression(expression.left, Some(left), context);
                }
            }
            Kind::CallExpression => {
                let call = node.as_call_expression();
                let arguments = call.arguments.nodes();
                if call.expression.kind() == Kind::Identifier
                    && call.expression.text() == "String"
                    && !arguments.is_empty()
                {
                    let t = context.checker.get_type_at_location(call.expression);
                    if (is_builtin_symbol_like(context.program, context.checker, t, &["String"])
                        || is_builtin_symbol_like(
                            context.program,
                            context.checker,
                            t,
                            &["StringConstructor"],
                        ))
                        && arguments[0].kind() != Kind::SpreadElement
                    {
                        self.check_expression(arguments[0], None, context);
                        return;
                    }
                }
                if call.expression.kind() == Kind::PropertyAccessExpression {
                    let member = call.expression.as_property_access_expression();
                    match member.name().text() {
                        "join" => {
                            let t = get_constrained_type_at_location(
                                context.checker,
                                member.expression,
                            );
                            let certainty = self.collect_join_certainty(context.checker, t, &[]);
                            Self::report(member.expression, certainty, true, context);
                        }
                        "toLocaleString" | "toString" => {
                            self.check_expression(member.expression, None, context);
                        }
                        _ => {}
                    }
                }
            }
            Kind::TemplateExpression => {
                if node
                    .parent()
                    .is_some_and(|parent| parent.kind() == Kind::TaggedTemplateExpression)
                {
                    return;
                }
                for &span in node.as_template_expression().template_spans.nodes() {
                    self.check_expression(span.as_template_span().expression, None, context);
                }
            }
            _ => unreachable!("no-base-to-string listener"),
        }
    }
}
