//! Port of tsgolint's `internal/rules/no_misused_spread/no_misused_spread.go`.

use tsrs_ast::{self as ast, Kind, Node, OperatorPrecedenceFlags};
use tsrs_checker::{ObjectFlags, SignatureKind, Type, TypeFlags};
use tsrs_core::{P, json::Value};

use crate::protocol::{Fix, Range, RuleMessage, Suggestion};
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};
use crate::type_matches_specifier::{TypeOrValueSpecifier, type_matches_some_specifier};
use crate::utils::{self, type_recurser, union_type_parts};

pub struct NoMisusedSpread {
    allow: Vec<TypeOrValueSpecifier>,
}

#[derive(Clone, Copy)]
enum MessageId {
    AddAwait,
    NoArraySpreadInObject,
    NoClassDeclarationSpreadInObject,
    NoClassInstanceSpreadInObject,
    NoFunctionSpreadInObject,
    NoIterableSpreadInObject,
    NoMapSpreadInObject,
    NoPromiseSpreadInObject,
    NoStringSpread,
    ReplaceMapSpreadInObject,
}

impl MessageId {
    fn message(self) -> RuleMessage {
        let (id, description, help) = match self {
            Self::AddAwait => ("addAwait", "Add await operator.", ""),
            Self::NoArraySpreadInObject => (
                "noArraySpreadInObject",
                "Using the spread operator on an array in an object will result in a list of indices.",
                "",
            ),
            Self::NoClassDeclarationSpreadInObject => (
                "noClassDeclarationSpreadInObject",
                "Using the spread operator on class declarations will spread only their static properties, and will lose their class prototype.",
                "",
            ),
            Self::NoClassInstanceSpreadInObject => (
                "noClassInstanceSpreadInObject",
                "Using the spread operator on class instances will lose their class prototype.",
                "",
            ),
            Self::NoFunctionSpreadInObject => (
                "noFunctionSpreadInObject",
                "Using the spread operator on a function without additional properties can cause unexpected behavior.",
                "Did you forget to call the function?",
            ),
            Self::NoIterableSpreadInObject => (
                "noIterableSpreadInObject",
                "Using the spread operator on an Iterable in an object can cause unexpected behavior.",
                "",
            ),
            Self::NoMapSpreadInObject => (
                "noMapSpreadInObject",
                "Using the spread operator on a Map in an object will result in an empty object.",
                "Did you mean to use `Object.fromEntries(map)` instead?",
            ),
            Self::NoPromiseSpreadInObject => (
                "noPromiseSpreadInObject",
                "Using the spread operator on Promise in an object can cause unexpected behavior.",
                "Did you forget to await the promise before spreading it?",
            ),
            Self::NoStringSpread => (
                "noStringSpread",
                "Using the spread operator on a string can mishandle special characters, because it produces Unicode code points, which will break complex characters (like emojis) into multiple parts.",
                "Consider using `Intl.Segmenter` for locale-aware string decomposition. Otherwise, if you don't need to preserve emojis or other non-ASCII characters, disable this lint rule on this line or configure the 'allow' rule option.",
            ),
            Self::ReplaceMapSpreadInObject => (
                "replaceMapSpreadInObject",
                "Replace map spread in object with `Object.fromEntries()`",
                "",
            ),
        };
        RuleMessage { id: id.into(), description: description.into(), help: help.into() }
    }
}

fn is_string(t: P<Type>) -> bool {
    type_recurser(t, &mut |t| t.flags().intersects(TypeFlags::StringLike))
}

fn is_map(context: &mut RuleContext<'_>, t: P<Type>) -> bool {
    type_recurser(t, &mut |t| {
        utils::is_builtin_symbol_like(
            context.program,
            context.checker,
            t,
            &["Map", "ReadonlyMap", "WeakMap"],
        )
    })
}

fn is_iterable_map(context: &mut RuleContext<'_>, t: P<Type>) -> bool {
    type_recurser(t, &mut |t| {
        utils::is_builtin_symbol_like(context.program, context.checker, t, &["Map", "ReadonlyMap"])
    })
}

fn insert_before(context: &RuleContext<'_>, node: P<Node>, text: &str) -> Fix {
    let pos = trim_node_range(context.source_file, node).pos;
    Fix { text: text.into(), range: Range { pos, end: pos } }
}

fn insert_after(node: P<Node>, text: &str) -> Fix {
    let end = node.end().expect_source("parsed lint node");
    Fix { text: text.into(), range: Range { pos: end, end } }
}

fn insert_await(context: &RuleContext<'_>, node: P<Node>) -> Vec<Fix> {
    let await_precedence = ast::get_operator_precedence(
        Kind::AwaitExpression,
        Kind::Unknown,
        OperatorPrecedenceFlags::None,
    );
    if ast::get_expression_precedence(node) > await_precedence {
        vec![insert_before(context, node, "await ")]
    } else {
        vec![insert_before(context, node, "await ("), insert_after(node, ")")]
    }
}

fn token_range(context: &RuleContext<'_>, node: P<Node>) -> Range {
    let range = tsrs_scanner::get_range_of_token_at_position(
        context.source_file,
        node.pos().expect_source("parsed lint node"),
    );
    Range { pos: range.source_pos().unwrap(), end: range.source_end().unwrap() }
}

fn map_suggestions(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    argument: P<Node>,
    t: P<Type>,
) -> Vec<Suggestion> {
    if union_type_parts(&t).iter().any(|&part| !is_iterable_map(context, part)) {
        return Vec::new();
    }
    let parent = node.parent().expect("spread parent");
    let fixes = if parent.kind() == Kind::ObjectLiteralExpression && parent.properties().len() == 1
    {
        let properties = parent.as_object_literal_expression().properties;
        let end = parent.end().expect_source("parsed lint node");
        vec![
            Fix { range: token_range(context, parent), text: String::new() },
            Fix { range: token_range(context, node), text: "Object.fromEntries(".into() },
            Fix {
                range: Range {
                    pos: argument.end().expect_source("parsed lint node"),
                    end: properties.end().expect_source("parsed lint list"),
                },
                text: ")".into(),
            },
            Fix { range: Range { pos: end - 1, end }, text: String::new() },
        ]
    } else {
        vec![insert_before(context, argument, "Object.fromEntries("), insert_after(argument, ")")]
    };
    vec![Suggestion { message: MessageId::ReplaceMapSpreadInObject.message(), fixes }]
}

fn is_class_instance(context: &mut RuleContext<'_>, t: P<Type>) -> bool {
    type_recurser(t, &mut |t| {
        if !context.checker.get_signatures_of_type(t, SignatureKind::Construct).is_empty() {
            return false;
        }
        let Some(symbol) = t.symbol() else {
            return false;
        };
        symbol.declarations().iter().any(|&declaration| {
            let symbol_type = context
                .checker
                .get_type_of_symbol_at_location(symbol, Some(declaration))
                .expect("class symbol type");
            !context
                .checker
                .get_signatures_of_type(symbol_type, SignatureKind::Construct)
                .is_empty()
        })
    })
}

fn is_class_declaration(t: P<Type>) -> bool {
    type_recurser(t, &mut |t| {
        if t.flags().intersects(TypeFlags::Object)
            && t.object_flags().intersects(ObjectFlags::InstantiationExpressionType)
        {
            return true;
        }
        t.symbol().and_then(|symbol| symbol.value_declaration()).is_some_and(|declaration| {
            matches!(declaration.kind(), Kind::ClassExpression | Kind::ClassDeclaration)
        })
    })
}

impl NoMisusedSpread {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("no-misused-spread: options must be an object".into()),
        };
        let allow = match object.and_then(|object| object.get("allow")) {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(values)) => {
                values.iter().map(TypeOrValueSpecifier::new).collect::<Result<_, _>>()?
            }
            _ => return Err("no-misused-spread: allow must be an array".into()),
        };
        Ok(Self { allow })
    }

    fn check_array_or_call_spread(&self, context: &mut RuleContext<'_>, node: P<Node>) {
        let t = utils::get_constrained_type_at_location(
            context.checker,
            node.as_spread_element().expression,
        );
        if !type_matches_some_specifier(t, &self.allow, context.program) && is_string(t) {
            context.report_node(node, MessageId::NoStringSpread.message());
        }
    }

    fn check_object_spread(&self, context: &mut RuleContext<'_>, node: P<Node>, argument: P<Node>) {
        let t = utils::get_constrained_type_at_location(context.checker, argument);
        if type_matches_some_specifier(t, &self.allow, context.program) {
            return;
        }
        if type_recurser(t, &mut |t| {
            utils::is_builtin_symbol_like(context.program, context.checker, t, &["Promise"])
        }) {
            let suggestions = if context.options.fix_suggestions {
                vec![Suggestion {
                    message: MessageId::AddAwait.message(),
                    fixes: insert_await(context, ast::skip_parentheses(argument)),
                }]
            } else {
                Vec::new()
            };
            context.report_node_with_suggestions(
                node,
                MessageId::NoPromiseSpreadInObject.message(),
                || suggestions,
            );
            return;
        }
        if type_recurser(t, &mut |t| {
            !context.checker.get_signatures_of_type(t, SignatureKind::Call).is_empty()
                && context.checker.get_properties_of_type(t).is_empty()
        }) {
            context.report_node(node, MessageId::NoFunctionSpreadInObject.message());
            return;
        }
        if is_map(context, t) {
            let suggestions = if context.options.fix_suggestions {
                map_suggestions(context, node, argument, t)
            } else {
                Vec::new()
            };
            context.report_node_with_suggestions(
                node,
                MessageId::NoMapSpreadInObject.message(),
                || suggestions,
            );
            return;
        }
        let id = if type_recurser(t, &mut |t| context.checker.is_array_or_tuple_type(t)) {
            MessageId::NoArraySpreadInObject
        } else if type_recurser(t, &mut |t| {
            utils::get_well_known_symbol_property_of_type(context.checker, t, "iterator").is_some()
        }) && !is_string(t)
        {
            MessageId::NoIterableSpreadInObject
        } else if is_class_instance(context, t) {
            MessageId::NoClassInstanceSpreadInObject
        } else if is_class_declaration(t) {
            MessageId::NoClassDeclarationSpreadInObject
        } else {
            return;
        };
        context.report_node(node, id.message());
    }
}

impl Rule for NoMisusedSpread {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(
            listener,
            Listener::ExpressionEnter(Kind::ArrayLiteralExpression | Kind::ObjectLiteralExpression)
                | Listener::Enter(Kind::CallExpression | Kind::JsxSpreadAttribute)
        )
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        match node.kind() {
            Kind::ArrayLiteralExpression => {
                for &element in node.elements() {
                    if element.kind() == Kind::SpreadElement {
                        self.check_array_or_call_spread(context, element);
                    }
                }
            }
            Kind::CallExpression => {
                for &element in node.as_call_expression().arguments().nodes() {
                    if element.kind() == Kind::SpreadElement {
                        self.check_array_or_call_spread(context, element);
                    }
                }
            }
            Kind::JsxSpreadAttribute => {
                self.check_object_spread(context, node, node.as_jsx_spread_attribute().expression);
            }
            Kind::ObjectLiteralExpression => {
                for &element in node.properties() {
                    if element.kind() == Kind::SpreadAssignment {
                        self.check_object_spread(
                            context,
                            element,
                            element.as_spread_assignment().expression,
                        );
                    }
                }
            }
            _ => unreachable!("no-misused-spread listener"),
        }
    }
}
