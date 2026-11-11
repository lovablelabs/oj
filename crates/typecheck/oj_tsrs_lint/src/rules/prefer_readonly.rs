use rustc_hash::FxHashSet;
use tsrs_ast::{Kind, ModifierFlags, Node, SymbolFlags};
use tsrs_checker::{Checker, ObjectFlags, Type, TypeFlags};
use tsrs_core::{P, collections::OrderedMap, json::Value};

use crate::protocol::{Diagnostic, Fix, Range, RuleMessage};
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};

#[derive(Clone, Copy, PartialEq, Eq)]
enum TypeToClassRelation {
    ClassAndInstance,
    Class,
    Instance,
    None,
}

struct ClassScope {
    class_type: P<Type>,
    only_inline_lambdas: bool,
    constructor_scope_depth: Option<usize>,
    member_variable_modifications: FxHashSet<String>,
    member_variable_with_constructor_modifications: FxHashSet<String>,
    private_modifiable_members: OrderedMap<String, P<Node>>,
    private_modifiable_statics: OrderedMap<String, P<Node>>,
    static_variable_modifications: FxHashSet<String>,
    deferred_reports: Vec<Diagnostic>,
}

impl ClassScope {
    fn new(checker: &mut Checker, class_node: P<Node>, only_inline_lambdas: bool) -> Self {
        let mut class_type = checker.get_type_at_location(class_node);
        if class_type.flags().intersects(TypeFlags::Intersection)
            && let Some(&first) = class_type.types().first()
        {
            class_type = first;
        }
        let mut scope = Self {
            class_type,
            only_inline_lambdas,
            constructor_scope_depth: None,
            member_variable_modifications: FxHashSet::default(),
            member_variable_with_constructor_modifications: FxHashSet::default(),
            private_modifiable_members: OrderedMap::default(),
            private_modifiable_statics: OrderedMap::default(),
            static_variable_modifications: FxHashSet::default(),
            deferred_reports: Vec::new(),
        };
        for &member in class_node.members() {
            if member.kind() == Kind::PropertyDeclaration {
                scope.add_declared_variable(member);
            }
        }
        scope
    }

    fn add_declared_variable(&mut self, node: P<Node>) {
        let Some(name_node) = node.name() else { return };
        let flags = node.modifier_flags();
        if (!flags.intersects(ModifierFlags::Private)
            && name_node.kind() != Kind::PrivateIdentifier)
            || flags.intersects(ModifierFlags::Accessor | ModifierFlags::Readonly)
            || name_node.kind() == Kind::ComputedPropertyName
        {
            return;
        }
        if self.only_inline_lambdas
            && node
                .initializer()
                .is_none_or(|initializer| initializer.kind() != Kind::ArrowFunction)
        {
            return;
        }
        let name = name_node.text();
        if name.is_empty() {
            return;
        }
        let members = if flags.intersects(ModifierFlags::Static) {
            &mut self.private_modifiable_statics
        } else {
            &mut self.private_modifiable_members
        };
        members.insert(name.into(), node);
    }

    fn add_variable_modification(&mut self, node: P<Node>, checker: &mut Checker) {
        let access = node.as_property_access_expression();
        let modifier_type = checker.get_type_at_location(access.expression);
        let relation = self.get_type_to_class_relation(modifier_type, checker);
        let name = access.name().text();
        if relation == TypeToClassRelation::Instance && self.constructor_scope_depth == Some(0) {
            self.member_variable_with_constructor_modifications.insert(name.into());
            return;
        }
        if matches!(relation, TypeToClassRelation::Instance | TypeToClassRelation::ClassAndInstance)
        {
            self.member_variable_modifications.insert(name.into());
        }
        if matches!(relation, TypeToClassRelation::Class | TypeToClassRelation::ClassAndInstance) {
            self.static_variable_modifications.insert(name.into());
        }
    }

    fn enter_constructor(&mut self, node: P<Node>) {
        self.constructor_scope_depth = Some(0);
        for &parameter in node.parameters() {
            if parameter.modifier_flags().intersects(ModifierFlags::Private) {
                self.add_declared_variable(parameter);
            }
        }
    }

    fn enter_non_constructor(&mut self) {
        if let Some(depth) = &mut self.constructor_scope_depth {
            *depth += 1;
        }
    }

    fn exit_non_constructor(&mut self) {
        if let Some(depth) = &mut self.constructor_scope_depth {
            *depth -= 1;
        }
    }

    fn finalize_unmodified_private_non_readonlys(&self) -> Vec<P<Node>> {
        self.private_modifiable_members
            .iter()
            .filter(|(name, _)| !self.member_variable_modifications.contains(*name))
            .chain(
                self.private_modifiable_statics
                    .iter()
                    .filter(|(name, _)| !self.static_variable_modifications.contains(*name)),
            )
            .map(|(_, &node)| node)
            .collect()
    }

    fn get_type_to_class_relation(&self, t: P<Type>, checker: &mut Checker) -> TypeToClassRelation {
        if t.flags().intersects(TypeFlags::Intersection) {
            let mut result = TypeToClassRelation::None;
            for &part in t.types() {
                match self.get_type_to_class_relation(part, checker) {
                    TypeToClassRelation::Class => {
                        if result == TypeToClassRelation::Instance {
                            return TypeToClassRelation::ClassAndInstance;
                        }
                        result = TypeToClassRelation::Class;
                    }
                    TypeToClassRelation::Instance => {
                        if result == TypeToClassRelation::Class {
                            return TypeToClassRelation::ClassAndInstance;
                        }
                        result = TypeToClassRelation::Instance;
                    }
                    _ => {}
                }
            }
            return result;
        }
        if t.flags().intersects(TypeFlags::Union) {
            // The pinned rule assumes valid private access, so only the first union member is inspected.
            return t.types().first().map_or(TypeToClassRelation::None, |&first| {
                self.get_type_to_class_relation(first, checker)
            });
        }
        if t.symbol().is_none() || !type_is_or_has_base_type(checker, t, self.class_type) {
            return TypeToClassRelation::None;
        }
        if t.flags().intersects(TypeFlags::Object)
            && t.object_flags().intersects(ObjectFlags::Anonymous)
        {
            TypeToClassRelation::Class
        } else {
            TypeToClassRelation::Instance
        }
    }
}

fn type_is_or_has_base_type(checker: &mut Checker, t: P<Type>, base_type: P<Type>) -> bool {
    let Some(base_symbol) = base_type.symbol() else {
        return false;
    };
    let mut queue = vec![t];
    let mut seen = FxHashSet::default();
    while let Some(current) = queue.pop() {
        if !seen.insert(current) {
            continue;
        }
        if current.symbol() == Some(base_symbol) {
            return true;
        }
        if !current.flags().intersects(TypeFlags::Object)
            || !current.object_flags().intersects(ObjectFlags::ClassOrInterface)
        {
            continue;
        }
        queue.extend_from_slice(checker.get_base_types(current));
    }
    false
}

fn is_destructuring_assignment(node: P<Node>) -> bool {
    let Some(mut current) = node.parent() else {
        return false;
    };
    while let Some(parent) = current.parent() {
        if matches!(
            parent.kind(),
            Kind::ObjectLiteralExpression | Kind::ArrayLiteralExpression | Kind::SpreadAssignment
        ) || (parent.kind() == Kind::SpreadElement
            && parent.parent().is_some_and(|p| p.kind() == Kind::ArrayLiteralExpression))
        {
            current = parent;
            continue;
        }
        if parent.kind() == Kind::BinaryExpression
            && current.kind() != Kind::PropertyAccessExpression
        {
            let binary = parent.as_binary_expression();
            return binary.left == current && binary.operator_token.kind() == Kind::EqualsToken;
        }
        break;
    }
    false
}

fn get_type_annotation_for_violating_node(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    violating_type: P<Type>,
    initializer_type: P<Type>,
) -> String {
    let annotation = context.checker.type_to_string_exported(violating_type);
    if annotation.is_empty() || !initializer_type.flags().intersects(TypeFlags::EnumLiteral) {
        return annotation;
    }
    let Some(symbol) =
        context.checker.resolve_name_exported(&annotation, node, SymbolFlags::Type, false)
    else {
        return String::new();
    };
    let value_symbol = context
        .checker
        .resolve_name_exported(&annotation, node, SymbolFlags::Value, false)
        .map(|symbol| context.checker.skip_alias(symbol));
    let symbol = context.checker.skip_alias(symbol);
    if value_symbol.is_some_and(|value| value != symbol) {
        return String::new();
    }
    if context.checker.get_declared_type_of_symbol(symbol) != violating_type {
        return String::new();
    }
    annotation
}

pub struct PreferReadonly {
    only_inline_lambdas: bool,
    class_scope_stack: Vec<ClassScope>,
}

impl PreferReadonly {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("prefer-readonly: options must be an object".into()),
        };
        let only_inline_lambdas = match object.and_then(|object| object.get("onlyInlineLambdas")) {
            None | Some(Value::Null) => false,
            Some(Value::Bool(value)) => *value,
            _ => return Err("prefer-readonly: onlyInlineLambdas must be a boolean".into()),
        };
        Ok(Self { only_inline_lambdas, class_scope_stack: Vec::new() })
    }

    fn build_reports(scope: &ClassScope, context: &mut RuleContext<'_>) -> Vec<Diagnostic> {
        let mut reports = Vec::new();
        for node in scope.finalize_unmodified_private_non_readonlys() {
            let Some(name_node) = node.name() else {
                continue;
            };
            let name_range = trim_node_range(context.source_file, name_node);
            let name_text =
                &context.source_file.text()[name_range.pos as usize..name_range.end as usize];
            let mut fixes = vec![Fix {
                text: "readonly ".into(),
                range: Range { pos: name_range.pos, end: name_range.pos },
            }];
            if node.kind() == Kind::PropertyDeclaration {
                let property = node.as_property_declaration();
                if property.type_().is_none()
                    && name_node.kind() == Kind::Identifier
                    && scope
                        .member_variable_with_constructor_modifications
                        .contains(name_node.text())
                    && let Some(initializer) = property.initializer()
                {
                    let violating_type = context.checker.get_type_at_location(node);
                    let initializer_type = context.checker.get_type_at_location(initializer);
                    if violating_type != initializer_type
                        && initializer_type.flags().intersects(TypeFlags::Literal)
                    {
                        let annotation = get_type_annotation_for_violating_node(
                            context,
                            node,
                            violating_type,
                            initializer_type,
                        );
                        if !annotation.is_empty() {
                            fixes.push(Fix {
                                text: format!(": {annotation}"),
                                range: Range { pos: name_range.end, end: name_range.end },
                            });
                        }
                    }
                }
            }
            reports.push(Diagnostic {
                range: Some(Range {
                    pos: trim_node_range(context.source_file, node).pos,
                    end: name_range.end,
                }),
                message: RuleMessage {
                    id: "preferReadonly".into(),
                    description: format!("Member '{name_text}' is never reassigned."),
                    help: "Mark it as `readonly`.".into(),
                },
                fixes,
                ..Default::default()
            });
        }
        reports
    }

    fn flush_finalized_scope(&mut self, context: &mut RuleContext<'_>) {
        let Some(scope) = self.class_scope_stack.pop() else {
            return;
        };
        let mut reports = Self::build_reports(&scope, context);
        reports.extend(scope.deferred_reports);
        if let Some(parent) = self.class_scope_stack.last_mut() {
            parent.deferred_reports.extend(reports);
            return;
        }
        for mut report in reports {
            if !context.options.fix {
                report.fixes.clear();
            }
            context.report_diagnostic(report);
        }
    }
}

fn handle_property_access_expression(node: P<Node>, scope: &mut ClassScope, checker: &mut Checker) {
    let Some(parent) = node.parent() else { return };
    if parent.kind() == Kind::BinaryExpression {
        if parent.as_binary_expression().left == node
            && tsrs_ast::is_assignment_expression(parent, false)
        {
            scope.add_variable_modification(node, checker);
        }
        return;
    }
    if parent.kind() == Kind::DeleteExpression || is_destructuring_assignment(node) {
        scope.add_variable_modification(node, checker);
        return;
    }
    let unary = match parent.kind() {
        Kind::PostfixUnaryExpression => Some((
            parent.as_postfix_unary_expression().operator,
            parent.as_postfix_unary_expression().operand,
        )),
        Kind::PrefixUnaryExpression => Some((
            parent.as_prefix_unary_expression().operator,
            parent.as_prefix_unary_expression().operand,
        )),
        _ => None,
    };
    if let Some((Kind::PlusPlusToken | Kind::MinusMinusToken, operand)) = unary
        && operand.kind() == Kind::PropertyAccessExpression
    {
        scope.add_variable_modification(operand, checker);
    }
}

impl Rule for PreferReadonly {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(
            listener,
            Listener::Enter(
                Kind::ClassDeclaration
                    | Kind::ClassExpression
                    | Kind::Constructor
                    | Kind::ArrowFunction
                    | Kind::FunctionDeclaration
                    | Kind::FunctionExpression
                    | Kind::MethodDeclaration
                    | Kind::GetAccessor
                    | Kind::SetAccessor
                    | Kind::PropertyAccessExpression
            ) | Listener::Exit(
                Kind::ClassDeclaration
                    | Kind::ClassExpression
                    | Kind::Constructor
                    | Kind::ArrowFunction
                    | Kind::FunctionDeclaration
                    | Kind::FunctionExpression
                    | Kind::MethodDeclaration
                    | Kind::GetAccessor
                    | Kind::SetAccessor
            )
        )
    }

    fn visit(&mut self, listener: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        match listener {
            Listener::Enter(Kind::ClassDeclaration | Kind::ClassExpression) => {
                self.class_scope_stack.push(ClassScope::new(
                    context.checker,
                    node,
                    self.only_inline_lambdas,
                ));
            }
            Listener::Exit(Kind::ClassDeclaration | Kind::ClassExpression) => {
                self.flush_finalized_scope(context);
            }
            _ => {
                let Some(scope) = self.class_scope_stack.last_mut() else {
                    return;
                };
                match listener {
                    Listener::Enter(Kind::Constructor) => scope.enter_constructor(node),
                    Listener::Exit(Kind::Constructor) => scope.constructor_scope_depth = None,
                    Listener::Enter(Kind::PropertyAccessExpression) => {
                        handle_property_access_expression(node, scope, context.checker);
                    }
                    Listener::Enter(_) => scope.enter_non_constructor(),
                    Listener::Exit(_) => scope.exit_non_constructor(),
                    _ => {}
                }
            }
        }
    }
}
