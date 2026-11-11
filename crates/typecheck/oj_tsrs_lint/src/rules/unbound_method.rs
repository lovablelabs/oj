use tsrs_ast::{self as ast, Kind, Node, SourceFile, Symbol};
use tsrs_checker::Type;
use tsrs_core::{P, json::Value};

use crate::protocol::{Diagnostic, LabeledRange, RuleMessage};
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};
use crate::utils::{is_any_builtin_symbol_like, is_builtin_symbol_like, type_recurser};

const BASE_MESSAGE: &str =
    "Avoid referencing unbound methods which may cause unintentional scoping of `this`.";

fn build_unbound_message() -> RuleMessage {
    RuleMessage { id: "unbound".into(), description: BASE_MESSAGE.into(), ..Default::default() }
}

fn build_unbound_without_this_annotation_message() -> RuleMessage {
    RuleMessage {
        id: "unboundWithoutThisAnnotation".into(),
        description: BASE_MESSAGE.into(),
        help: "If your function does not access `this`, you can annotate it with `this: void`, or consider using an arrow function instead.".into(),
    }
}

fn build_unbound_diagnostic(
    source_file: P<SourceFile>,
    node: P<Node>,
    dangerous_reference: P<Node>,
    message: RuleMessage,
) -> Diagnostic {
    let diagnostic_range = trim_node_range(source_file, node);
    let dangerous_range = trim_node_range(source_file, dangerous_reference);
    let mut labeled_ranges = Vec::new();
    if diagnostic_range != dangerous_range {
        labeled_ranges.push(LabeledRange {
            label: "This reference may be unbound and lose `this` context".into(),
            range: dangerous_range,
        });
    }
    Diagnostic { range: Some(diagnostic_range), message, labeled_ranges, ..Default::default() }
}

fn includes_modifier(node: P<Node>, kind: Kind) -> bool {
    node.modifier_nodes().iter().any(|modifier| modifier.kind() == kind)
}

fn is_node_inside_type_declaration(node: P<Node>) -> bool {
    let mut parent = node.parent();
    while let Some(node) = parent {
        match node.kind() {
            Kind::ClassDeclaration | Kind::VariableStatement => {
                if includes_modifier(node, Kind::DeclareKeyword) {
                    return true;
                }
            }
            Kind::MethodDeclaration => {
                if includes_modifier(node, Kind::AbstractKeyword) {
                    return true;
                }
            }
            Kind::FunctionDeclaration => {
                if node.body().is_none() {
                    return true;
                }
            }
            Kind::FunctionType | Kind::InterfaceDeclaration | Kind::TypeAliasDeclaration => {
                return true;
            }
            _ => {}
        }
        parent = node.parent();
    }
    false
}

fn is_safe_use(mut node: P<Node>) -> bool {
    while let Some(parent) = node.parent() {
        match parent.kind() {
            Kind::ParenthesizedExpression
            | Kind::NonNullExpression
            | Kind::AsExpression
            | Kind::TypeAssertionExpression => {}
            Kind::IfStatement
            | Kind::ForStatement
            | Kind::PropertyAccessExpression
            | Kind::ElementAccessExpression
            | Kind::SwitchStatement
            | Kind::WhileStatement
            | Kind::DeleteExpression
            | Kind::TypeOfExpression
            | Kind::VoidExpression => return true,
            Kind::PostfixUnaryExpression => {
                return matches!(
                    parent.as_postfix_unary_expression().operator,
                    Kind::PlusPlusToken | Kind::MinusMinusToken
                );
            }
            Kind::PrefixUnaryExpression => {
                return matches!(
                    parent.as_prefix_unary_expression().operator,
                    Kind::PlusPlusToken | Kind::MinusMinusToken | Kind::ExclamationToken
                );
            }
            Kind::CallExpression => return parent.expression() == Some(node),
            Kind::ConditionalExpression => {
                return parent.as_conditional_expression().condition == node;
            }
            Kind::TaggedTemplateExpression => {
                return parent.as_tagged_template_expression().tag == node;
            }
            Kind::BinaryExpression => {
                let expression = parent.as_binary_expression();
                let operator = expression.operator_token.kind();
                match operator {
                    Kind::AmpersandAmpersandToken => {
                        if expression.left == node {
                            return true;
                        }
                    }
                    Kind::ExclamationEqualsToken
                    | Kind::ExclamationEqualsEqualsToken
                    | Kind::EqualsEqualsToken
                    | Kind::EqualsEqualsEqualsToken
                    | Kind::InstanceOfKeyword => return true,
                    _ => {
                        if !ast::is_logical_binary_operator(operator) {
                            if ast::is_assignment_expression(parent, true) {
                                return node == expression.left
                                    || (ast::is_access_expression(node)
                                        && node.expression().unwrap().kind()
                                            == Kind::SuperKeyword
                                        && ast::is_access_expression(expression.left)
                                        && expression.left.expression().unwrap().kind()
                                            == Kind::ThisKeyword);
                            }
                            return false;
                        }
                    }
                }
            }
            _ => return false,
        }
        node = parent;
    }
    false
}

fn is_not_imported(symbol: P<Symbol>, current_source_file: P<SourceFile>) -> bool {
    symbol.value_declaration().is_some_and(|declaration| {
        ast::get_source_file_of_node(declaration) != Some(current_source_file)
    })
}

const SUPPORTED_GLOBAL_TYPES: &[&str] = &[
    "NumberConstructor",
    "ObjectConstructor",
    "StringConstructor",
    "SymbolConstructor",
    "ArrayConstructor",
    "Array",
    "ProxyConstructor",
    "Console",
    "DateConstructor",
    "Atomics",
    "Math",
    "JSON",
];

fn check_method(value_declaration: P<Node>, ignore_static: bool) -> (bool, bool) {
    let parameters = value_declaration.parameters();
    let first_param_is_this = parameters.first().is_some_and(|parameter| {
        ast::is_parameter_declaration(*parameter)
            && parameter
                .name()
                .is_some_and(|name| ast::is_identifier(name) && name.text() == "this")
    });
    let this_arg_is_void = first_param_is_this
        && parameters[0]
            .type_node()
            .is_some_and(|annotation| annotation.kind() == Kind::VoidKeyword);
    let dangerous = !this_arg_is_void
        && (!ignore_static || !includes_modifier(value_declaration, Kind::StaticKeyword));
    (dangerous, first_param_is_this)
}

fn check_if_method(symbol: P<Symbol>, ignore_static: bool) -> (bool, bool) {
    let Some(value_declaration) = symbol.value_declaration() else {
        return (false, false);
    };
    match value_declaration.kind() {
        Kind::PropertyAssignment | Kind::PropertyDeclaration => {
            let Some(initializer) = value_declaration.initializer() else {
                return (false, false);
            };
            if !ast::is_function_expression(initializer) {
                return (false, false);
            }
            check_method(initializer, ignore_static)
        }
        Kind::MethodDeclaration | Kind::MethodSignature => {
            check_method(value_declaration, ignore_static)
        }
        _ => (false, false),
    }
}

pub struct UnboundMethod {
    ignore_static: bool,
}

impl UnboundMethod {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("unbound-method: options must be an object".into()),
        };
        let ignore_static = match object.and_then(|object| object.get("ignoreStatic")) {
            None | Some(Value::Null) => false,
            Some(Value::Bool(value)) => *value,
            _ => return Err("unbound-method: ignoreStatic must be a boolean".into()),
        };
        Ok(Self { ignore_static })
    }

    fn is_spec_bound_builtin_method(
        object_type: P<Type>,
        property: P<Node>,
        context: &mut RuleContext<'_>,
    ) -> bool {
        if !ast::is_identifier(property) || property.text() != "compare" {
            return false;
        }
        let Some(symbol) = context.checker.get_property_of_type(object_type, property.text())
        else {
            return false;
        };
        !symbol.declarations().is_empty()
            && symbol.declarations().iter().all(|&declaration| {
                declaration.parent().is_some_and(|owner| {
                    owner.kind() == Kind::InterfaceDeclaration
                        && owner.name().is_some_and(|name| name.text() == "Collator")
                        && ast::get_source_file_of_node(declaration).is_some_and(|file| {
                            crate::utils::is_source_file_default_library(context.program, file)
                        })
                })
            })
    }

    fn is_natively_bound(
        object: P<Node>,
        property: P<Node>,
        context: &mut RuleContext<'_>,
    ) -> bool {
        if ast::is_identifier(object)
            && ast::is_identifier(property)
            && natively_bound_member(object.text(), property.text())
        {
            let object_symbol = context.checker.get_symbol_at_location_exported(object);
            if object_symbol.is_some_and(|symbol| is_not_imported(symbol, context.source_file)) {
                return true;
            }
        }
        let object_type = context.checker.get_type_at_location(object);
        if Self::is_spec_bound_builtin_method(object_type, property, context) {
            return true;
        }
        if !is_builtin_symbol_like(
            context.program,
            context.checker,
            object_type,
            SUPPORTED_GLOBAL_TYPES,
        ) {
            return false;
        }
        let property_type = context.checker.get_type_at_location(property);
        is_any_builtin_symbol_like(context.program, context.checker, property_type)
    }

    fn check_if_method_and_report(
        &self,
        node: P<Node>,
        dangerous_reference: P<Node>,
        symbol: Option<P<Symbol>>,
        context: &mut RuleContext<'_>,
    ) -> bool {
        let Some(symbol) = symbol else {
            return false;
        };
        let (dangerous, first_param_is_this) = check_if_method(symbol, self.ignore_static);
        if !dangerous {
            return false;
        }
        let message = if first_param_is_this {
            build_unbound_message()
        } else {
            build_unbound_without_this_annotation_message()
        };
        context.report_diagnostic(build_unbound_diagnostic(
            context.source_file,
            node,
            dangerous_reference,
            message,
        ));
        true
    }

    fn check_binding_property(
        &self,
        pattern_node: P<Node>,
        init_node: Option<P<Node>>,
        property_name: P<Node>,
        parent_is_assignment_pattern_like: bool,
        context: &mut RuleContext<'_>,
    ) {
        if ast::is_computed_property_name(property_name) {
            return;
        }
        if let Some(init_node) = init_node {
            if !Self::is_natively_bound(init_node, property_name, context) {
                let init_type = context.checker.get_type_at_location(init_node);
                let symbol = context.checker.get_property_of_type(init_type, property_name.text());
                if self.check_if_method_and_report(property_name, property_name, symbol, context) {
                    return;
                }
            } else if !parent_is_assignment_pattern_like {
                return;
            }
        }
        let pattern_type = context.checker.get_type_at_location(pattern_node);
        type_recurser(pattern_type, &mut |ty| {
            if Self::is_spec_bound_builtin_method(ty, property_name, context) {
                return false;
            }
            let symbol = context.checker.get_property_of_type(ty, property_name.text());
            self.check_if_method_and_report(property_name, property_name, symbol, context)
        });
    }
}

impl Rule for UnboundMethod {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(
            listener,
            Listener::Enter(Kind::PropertyAccessExpression | Kind::ObjectBindingPattern)
                | Listener::PatternEnter(Kind::ObjectLiteralExpression)
        )
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        match node.kind() {
            Kind::PropertyAccessExpression => {
                if is_safe_use(node)
                    || Self::is_natively_bound(
                        node.expression().unwrap(),
                        node.name().unwrap(),
                        context,
                    )
                {
                    return;
                }
                let symbol = context.checker.get_symbol_at_location_exported(node);
                self.check_if_method_and_report(node, node.name().unwrap(), symbol, context);
            }
            Kind::ObjectLiteralExpression => {
                let Some(parent) =
                    node.parent().filter(|&parent| ast::is_assignment_expression(parent, true))
                else {
                    return;
                };
                let init_node = parent.as_binary_expression().right();
                for &property in node.properties() {
                    if !ast::is_property_assignment(property)
                        && !ast::is_shorthand_property_assignment(property)
                    {
                        continue;
                    }
                    self.check_binding_property(
                        node,
                        Some(init_node),
                        property.name().unwrap(),
                        true,
                        context,
                    );
                }
            }
            Kind::ObjectBindingPattern => {
                if is_node_inside_type_declaration(node) {
                    return;
                }
                let parent = node.parent().unwrap();
                let parent_is_assignment_pattern_like =
                    ast::is_binding_element(parent) || ast::is_parameter_declaration(parent);
                let init_node =
                    if ast::is_variable_declaration(parent) || parent_is_assignment_pattern_like {
                        parent.initializer()
                    } else {
                        None
                    };
                for &property in node.elements() {
                    if !ast::is_binding_element(property) {
                        continue;
                    }
                    let binding_element = property.as_binding_element();
                    let property_name =
                        binding_element.property_name().unwrap_or_else(|| property.name().unwrap());
                    if binding_element.dot_dot_dot_token().is_some()
                        || !ast::is_identifier(property_name)
                    {
                        continue;
                    }
                    self.check_binding_property(
                        node,
                        init_node,
                        property_name,
                        parent_is_assignment_pattern_like,
                        context,
                    );
                }
            }
            _ => unreachable!(),
        }
    }
}

fn natively_bound_member(object: &str, property: &str) -> bool {
    let members: &[&str] = match object {
        "Number" => &["isFinite", "isInteger", "isNaN", "isSafeInteger", "parseFloat", "parseInt"],
        "Object" => &[
            "assign",
            "getOwnPropertyDescriptor",
            "getOwnPropertyDescriptors",
            "getOwnPropertyNames",
            "getOwnPropertySymbols",
            "hasOwn",
            "is",
            "preventExtensions",
            "seal",
            "create",
            "defineProperties",
            "defineProperty",
            "freeze",
            "getPrototypeOf",
            "setPrototypeOf",
            "isExtensible",
            "isFrozen",
            "isSealed",
            "keys",
            "entries",
            "fromEntries",
            "values",
            "groupBy",
        ],
        "String" => &["fromCharCode", "fromCodePoint", "raw"],
        "Symbol" => &["for", "keyFor"],
        "Array" => &["isArray", "from", "of", "fromAsync"],
        "Proxy" => &["revocable"],
        "Date" => &["now", "parse", "UTC"],
        "Atomics" => &[
            "load",
            "store",
            "add",
            "sub",
            "and",
            "or",
            "xor",
            "exchange",
            "compareExchange",
            "isLockFree",
            "wait",
            "waitAsync",
            "notify",
        ],
        "Reflect" => &[
            "defineProperty",
            "deleteProperty",
            "apply",
            "construct",
            "get",
            "getOwnPropertyDescriptor",
            "getPrototypeOf",
            "has",
            "isExtensible",
            "ownKeys",
            "preventExtensions",
            "set",
            "setPrototypeOf",
        ],
        "console" => &[
            "log",
            "info",
            "debug",
            "warn",
            "error",
            "dir",
            "time",
            "timeEnd",
            "timeLog",
            "trace",
            "assert",
            "clear",
            "count",
            "countReset",
            "group",
            "groupEnd",
            "table",
            "dirxml",
            "groupCollapsed",
            "Console",
            "profile",
            "profileEnd",
            "timeStamp",
            "context",
            "createTask",
        ],
        "Math" => &[
            "abs", "acos", "acosh", "asin", "asinh", "atan", "atanh", "atan2", "ceil", "cbrt",
            "expm1", "clz32", "cos", "cosh", "exp", "floor", "fround", "hypot", "imul", "log",
            "log1p", "log2", "log10", "max", "min", "pow", "random", "round", "sign", "sin",
            "sinh", "sqrt", "tan", "tanh", "trunc",
        ],
        "JSON" => &["parse", "stringify", "rawJSON", "isRawJSON"],
        "Intl" => &[
            "getCanonicalLocales",
            "supportedValuesOf",
            "DateTimeFormat",
            "NumberFormat",
            "Collator",
            "PluralRules",
            "RelativeTimeFormat",
            "ListFormat",
            "Locale",
            "DisplayNames",
            "Segmenter",
        ],
        _ => &[],
    };
    members.contains(&property)
}
