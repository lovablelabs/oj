//! Port of tsgolint's `internal/rules/no_deprecated/no_deprecated.go`.

use std::borrow::Cow;

use rustc_hash::FxHashSet;

use tsrs_ast::{Kind, Node, NodeFlags, Symbol, SymbolFlags};
use tsrs_checker::{CheckMode, ContextFlags, LiteralValue, Type, TypeFlags};
use tsrs_core::{P, json::Value};

use crate::protocol::RuleMessage;
use crate::rule::{Listener, Rule, RuleContext};
use crate::type_matches_specifier::{
    TypeOrValueSpecifier, symbol_matches_specifier_name_and_source, type_matches_some_specifier,
    value_matches_some_specifier,
};

pub struct NoDeprecated {
    allow: Vec<TypeOrValueSpecifier>,
}

fn jsdoc_deprecation_from_node(mut node: P<Node>) -> String {
    while !node.flags().intersects(NodeFlags::PossiblyContainsDeprecatedTag) {
        let Some(parent) = node.parent() else {
            return String::new();
        };
        node = parent;
    }
    for &jsdoc in node.jsdoc(None) {
        let Some(tags) = jsdoc.as_jsdoc().tags else {
            continue;
        };
        for &tag in tags.nodes() {
            if tag.kind() == Kind::JSDocDeprecatedTag {
                let mut text = String::new();
                if let Some(comment) = tag.as_jsdoc_deprecated_tag().comment() {
                    for &part in comment.nodes() {
                        text.push_str(part.text());
                    }
                }
                return text;
            }
        }
    }
    String::new()
}

fn jsdoc_deprecation(context: &mut RuleContext<'_>, symbol: Option<P<Symbol>>) -> Option<String> {
    for &declaration in symbol?.declarations() {
        if context.checker.is_deprecated_declaration(declaration) {
            return Some(jsdoc_deprecation_from_node(declaration));
        }
    }
    None
}

fn search_aliases(
    context: &mut RuleContext<'_>,
    symbol: Option<P<Symbol>>,
    check_target: bool,
) -> Option<String> {
    let mut symbol = symbol?;
    if !symbol.flags().intersects(SymbolFlags::Alias) {
        return if check_target { jsdoc_deprecation(context, Some(symbol)) } else { None };
    }
    let target = context.checker.get_aliased_symbol(symbol);
    // A circular alias chain (for example `export { x } from './a'` in a.ts) never reaches nil or
    // the target, so stop at the first alias that has been visited.
    let mut visited = FxHashSet::default();
    while symbol.flags().intersects(SymbolFlags::Alias) && visited.insert(symbol) {
        if let Some(reason) = jsdoc_deprecation(context, Some(symbol)) {
            return Some(reason);
        }
        if tsrs_checker::Checker::get_declaration_of_alias_symbol(symbol).is_none() {
            break;
        }
        let Some(next) = context.checker.get_immediate_aliased_symbol(symbol) else {
            break;
        };
        symbol = next;
        if check_target && symbol == target {
            return jsdoc_deprecation(context, Some(symbol));
        }
    }
    None
}

fn is_node_callee_of_parent(node: P<Node>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        Kind::NewExpression | Kind::CallExpression => parent.expression() == Some(node),
        Kind::TaggedTemplateExpression => parent.as_tagged_template_expression().tag == node,
        Kind::JsxOpeningElement => parent.as_jsx_opening_element().tag_name == node,
        _ => false,
    }
}

fn get_call_like_node(mut node: P<Node>) -> Option<P<Node>> {
    while let Some(parent) = node.parent() {
        if parent.kind() != Kind::PropertyAccessExpression
            || parent.as_property_access_expression().name() != node
        {
            break;
        }
        node = parent;
    }
    is_node_callee_of_parent(node).then_some(node)
}

fn call_like_deprecation(context: &mut RuleContext<'_>, node: P<Node>) -> Option<String> {
    let parent = node.parent()?;
    let signature = context.checker.get_resolved_signature(parent, None, CheckMode::Normal);
    let declaration = signature.declaration();
    if let Some(declaration) = declaration
        && context.checker.is_deprecated_declaration(declaration)
    {
        return Some(jsdoc_deprecation_from_node(declaration));
    }
    let symbol = context.checker.get_symbol_at_location_exported(node)?;
    let aliased = if symbol.flags().intersects(SymbolFlags::Alias) {
        Some(context.checker.get_aliased_symbol(symbol))
    } else {
        Some(symbol)
    };
    let kind = aliased
        .and_then(|symbol| symbol.declarations().first().map(|node| node.kind()))
        .unwrap_or(Kind::Unknown);
    if matches!(kind, Kind::MethodDeclaration | Kind::FunctionDeclaration | Kind::MethodSignature) {
        search_aliases(context, Some(symbol), false).or_else(|| {
            if declaration.is_none() { jsdoc_deprecation(context, aliased) } else { None }
        })
    } else {
        search_aliases(context, Some(symbol), true)
    }
}

fn jsx_attribute_deprecation(
    context: &mut RuleContext<'_>,
    element: P<Node>,
    name: &str,
) -> Option<String> {
    let tag = match element.kind() {
        Kind::JsxSelfClosingElement => element.as_jsx_self_closing_element().tag_name,
        Kind::JsxOpeningElement => element.as_jsx_opening_element().tag_name,
        _ => return None,
    };
    let t = context.checker.get_contextual_type(tag, ContextFlags::None)?;
    let symbol = context.checker.get_property_of_type(t, name);
    jsdoc_deprecation(context, symbol)
}

fn literal_name(t: P<Type>) -> Option<Cow<'static, str>> {
    match t.as_literal_type().value()? {
        LiteralValue::String(text) => Some(Cow::Borrowed(text)),
        LiteralValue::Number(value) => Some(Cow::Owned(value.to_string())),
        LiteralValue::BigInt(value) => Some(Cow::Owned(format!("{value}n"))),
        LiteralValue::Boolean(value) => Some(Cow::Borrowed(if value { "true" } else { "false" })),
    }
}

fn object_literal_property_name(
    context: &mut RuleContext<'_>,
    mut name: P<Node>,
) -> Option<Cow<'static, str>> {
    if name.kind() == Kind::ComputedPropertyName {
        name = name.as_computed_property_name().expression;
        if matches!(name.kind(), Kind::StringLiteral | Kind::NumericLiteral | Kind::BigIntLiteral) {
            return Some(Cow::Borrowed(name.text()));
        }
        let t = context.checker.get_type_at_location(name);
        return if t.flags().intersects(
            TypeFlags::StringLiteral | TypeFlags::NumberLiteral | TypeFlags::BigIntLiteral,
        ) {
            literal_name(t)
        } else {
            None
        };
    }
    matches!(
        name.kind(),
        Kind::Identifier
            | Kind::PrivateIdentifier
            | Kind::StringLiteral
            | Kind::NumericLiteral
            | Kind::BigIntLiteral
    )
    .then(|| Cow::Borrowed(name.text()))
}

fn binding_pattern_source_type(
    context: &mut RuleContext<'_>,
    mut current: Option<P<Node>>,
) -> Option<P<Type>> {
    while let Some(node) = current {
        match node.kind() {
            Kind::VariableDeclaration => {
                return node.initializer().map(|node| context.checker.get_type_at_location(node));
            }
            Kind::Parameter => return Some(context.checker.get_type_at_location(node)),
            Kind::BindingElement => {
                let element = node.as_binding_element();
                let source = binding_pattern_source_type(context, node.parent())?;
                let name = if let Some(property) = element.property_name() {
                    if property.kind() == Kind::ComputedPropertyName {
                        return None;
                    }
                    property.text()
                } else {
                    let name = element.name()?;
                    if matches!(name.kind(), Kind::ObjectBindingPattern | Kind::ArrayBindingPattern)
                    {
                        return None;
                    }
                    name.text()
                };
                if name.is_empty() {
                    return None;
                }
                let property = context.checker.get_property_of_type(source, name)?;
                return context.checker.get_type_of_symbol_at_location(property, Some(node));
            }
            Kind::ArrayBindingPattern => {
                let source = binding_pattern_source_type(context, node.parent())?;
                return if let Some(property) = context.checker.get_property_of_type(source, "0") {
                    context.checker.get_type_of_symbol_at_location(property, Some(node))
                } else {
                    Some(source)
                };
            }
            _ => {}
        }
        current = node.parent();
    }
    None
}

fn property_deprecation(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    source: P<Type>,
    name: &str,
) -> Option<String> {
    let property = context.checker.get_property_of_type(source, name);
    let symbol = context.checker.get_symbol_at_location_exported(node);
    search_aliases(context, symbol, true)
        .or_else(|| jsdoc_deprecation(context, property))
        .or_else(|| jsdoc_deprecation(context, symbol))
        .or_else(|| {
            let declaration = symbol?.value_declaration()?;
            let symbol = context.checker.get_shorthand_assignment_value_symbol(Some(declaration));
            if node
                .parent()
                .is_some_and(|parent| parent.kind() == Kind::ShorthandPropertyAssignment)
            {
                search_aliases(context, symbol, true)
            } else {
                jsdoc_deprecation(context, symbol)
            }
        })
}

fn deprecation_reason(context: &mut RuleContext<'_>, node: P<Node>) -> Option<String> {
    if let Some(callee) = get_call_like_node(node) {
        return call_like_deprecation(context, callee);
    }
    if let Some(parent) = node.parent()
        && node.kind() != Kind::SuperKeyword
    {
        if parent.kind() == Kind::JsxAttribute
            && let Some(element) = parent.parent().and_then(|attributes| attributes.parent())
        {
            return jsx_attribute_deprecation(context, element, node.text());
        }
        if parent.kind() == Kind::BindingElement
            && parent.parent().is_some_and(|pattern| {
                matches!(pattern.kind(), Kind::ObjectBindingPattern | Kind::ArrayBindingPattern)
            })
        {
            let pattern = parent.parent().unwrap();
            let source = binding_pattern_source_type(context, Some(pattern))
                .unwrap_or_else(|| context.checker.get_type_at_location(pattern));
            let name = if let Some(property) = parent.as_binding_element().property_name() {
                if property.kind() == Kind::ComputedPropertyName {
                    return None;
                }
                property.text()
            } else {
                node.text()
            };
            if let Some(reason) = property_deprecation(context, node, source, name) {
                return Some(reason);
            }
        }
        if parent.kind() == Kind::ShorthandPropertyAssignment
            && let Some(object) = parent.parent()
        {
            let source = context.checker.get_type_at_location(object);
            if let Some(reason) = property_deprecation(context, node, source, node.text()) {
                return Some(reason);
            }
        }
    }
    let symbol = context.checker.get_symbol_at_location_exported(node);
    search_aliases(context, symbol, true)
}

fn is_declaration(node: P<Node>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        Kind::BindingElement => {
            parent.parent().is_some_and(|pattern| pattern.kind() == Kind::ArrayBindingPattern)
                && parent.name() == Some(node)
        }
        Kind::ClassExpression
        | Kind::VariableDeclaration
        | Kind::EnumMember
        | Kind::ClassDeclaration
        | Kind::MethodDeclaration
        | Kind::PropertyDeclaration
        | Kind::GetAccessor
        | Kind::SetAccessor
        | Kind::ImportEqualsDeclaration => parent.name() == Some(node),
        Kind::PropertyAssignment => {
            parent.initializer() != Some(node)
                && parent
                    .parent()
                    .is_some_and(|object| object.kind() == Kind::ObjectLiteralExpression)
        }
        Kind::ArrowFunction
        | Kind::FunctionDeclaration
        | Kind::FunctionExpression
        | Kind::EnumDeclaration
        | Kind::InterfaceDeclaration
        | Kind::ModuleDeclaration
        | Kind::MethodSignature
        | Kind::PropertySignature
        | Kind::TypeAliasDeclaration
        | Kind::TypeParameter
        | Kind::Parameter => true,
        _ => false,
    }
}

fn is_inside_import(mut current: Option<P<Node>>) -> bool {
    while let Some(node) = current {
        match node.kind() {
            Kind::ImportDeclaration => return true,
            Kind::SourceFile | Kind::Block | Kind::FunctionDeclaration | Kind::ClassDeclaration => {
                return false;
            }
            _ => current = node.parent(),
        }
    }
    false
}

fn report(context: &mut RuleContext<'_>, node: P<Node>, name: &str, reason: &str) {
    let (id, description) = if reason.is_empty() {
        ("deprecated", format!("`{name}` is deprecated."))
    } else {
        ("deprecatedWithReason", format!("`{name}` is deprecated. {}", reason.trim()))
    };
    context.report_node(node, RuleMessage { id: id.into(), description, ..Default::default() });
}

impl NoDeprecated {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("no-deprecated: options must be an object".into()),
        };
        let allow = match object.and_then(|object| object.get("allow")) {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(values)) => {
                values.iter().map(TypeOrValueSpecifier::new).collect::<Result<_, _>>()?
            }
            _ => return Err("no-deprecated: allow must be an array".into()),
        };
        Ok(Self { allow })
    }

    fn check_identifier(&self, context: &mut RuleContext<'_>, node: P<Node>) {
        if is_declaration(node) || is_inside_import(Some(node)) {
            return;
        }
        let Some(reason) = deprecation_reason(context, node) else {
            return;
        };
        if !self.allow.is_empty() {
            let t = context.checker.get_type_at_location(node);
            if type_matches_some_specifier(t, &self.allow, context.program)
                || value_matches_some_specifier(node, &self.allow, context.program, context.checker)
            {
                return;
            }
        }
        let name = match node.kind() {
            Kind::SuperKeyword => Cow::Borrowed("super"),
            Kind::PrivateIdentifier => Cow::Owned(format!("#{}", node.text())),
            _ => Cow::Borrowed(node.text()),
        };
        report(context, node, &name, &reason);
    }

    fn check_object_literal_property(&self, context: &mut RuleContext<'_>, node: P<Node>) {
        let Some(parent) =
            node.parent().filter(|parent| parent.kind() == Kind::ObjectLiteralExpression)
        else {
            return;
        };
        let Some(name_node) = node.name() else {
            return;
        };
        let Some(name) = object_literal_property_name(context, name_node) else {
            return;
        };
        let Some(contextual) =
            context.checker.get_apparent_type_of_contextual_type(parent, ContextFlags::None)
        else {
            return;
        };
        let property = context.checker.get_property_of_type(contextual, &name);
        if property.is_none_or(|property| !context.checker.is_deprecated_symbol(property)) {
            return;
        }
        let Some(reason) = jsdoc_deprecation(context, property) else {
            return;
        };
        if !self.allow.is_empty() {
            let name_type = context.checker.get_type_at_location(name_node);
            if type_matches_some_specifier(contextual, &self.allow, context.program)
                || type_matches_some_specifier(name_type, &self.allow, context.program)
                || self.allow.iter().any(|specifier| {
                    symbol_matches_specifier_name_and_source(
                        property,
                        &name,
                        specifier,
                        context.program,
                    )
                })
            {
                return;
            }
        }
        report(context, name_node, if name.is_empty() { "\"\"" } else { &name }, &reason);
    }

    fn check_element_access(&self, context: &mut RuleContext<'_>, node: P<Node>) {
        let element = node.as_element_access_expression();
        let t = context.checker.get_type_at_location(element.argument_expression);
        if !t.flags().intersects(
            TypeFlags::StringLiteral | TypeFlags::NumberLiteral | TypeFlags::BigIntLiteral,
        ) {
            return;
        }
        let object = context.checker.get_type_at_location(element.expression);
        let Some(name) = literal_name(t) else {
            return;
        };
        let property = context.checker.get_property_of_type(object, &name);
        let Some(reason) = jsdoc_deprecation(context, property) else {
            return;
        };
        if type_matches_some_specifier(object, &self.allow, context.program) {
            return;
        }
        report(context, element.argument_expression, &name, &reason);
    }
}

impl Rule for NoDeprecated {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(
            listener,
            Listener::Enter(
                Kind::Identifier
                    | Kind::ElementAccessExpression
                    | Kind::PropertyAssignment
                    | Kind::ShorthandPropertyAssignment
                    | Kind::MethodDeclaration
                    | Kind::GetAccessor
                    | Kind::SetAccessor
                    | Kind::PrivateIdentifier
                    | Kind::SuperKeyword
            )
        )
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        match node.kind() {
            Kind::Identifier => {
                let Some(parent) = node.parent() else {
                    return;
                };
                if matches!(
                    parent.kind(),
                    Kind::JsxClosingElement | Kind::ExportDeclaration | Kind::NamespaceExport
                ) {
                    return;
                }
                if parent.kind() == Kind::ExportSpecifier {
                    if parent.as_export_specifier().property_name == Some(node) {
                        return;
                    }
                    if parent.jsdoc(None).iter().any(|jsdoc| {
                        jsdoc.as_jsdoc().tags.is_some_and(|tags| {
                            tags.nodes().iter().any(|tag| tag.kind() == Kind::JSDocDeprecatedTag)
                        })
                    }) {
                        return;
                    }
                }
                self.check_identifier(context, node);
            }
            Kind::ElementAccessExpression => self.check_element_access(context, node),
            Kind::PropertyAssignment
            | Kind::ShorthandPropertyAssignment
            | Kind::MethodDeclaration
            | Kind::GetAccessor
            | Kind::SetAccessor => self.check_object_literal_property(context, node),
            Kind::PrivateIdentifier | Kind::SuperKeyword => self.check_identifier(context, node),
            _ => unreachable!("no-deprecated listener"),
        }
    }
}
