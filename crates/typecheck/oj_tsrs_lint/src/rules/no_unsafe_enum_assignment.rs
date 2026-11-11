//! Port of tsgolint's `internal/rules/no_unsafe_enum_assignment/no_unsafe_enum_assignment.go`,
//! `type_comparison.go` and `function_signature.go`.

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::{self as ast, Kind, Node};
use tsrs_checker::{
    CheckMode, Checker, ContextFlags, ElementFlags, LiteralValue, ObjectFlags, Type, TypeFlags,
};
use tsrs_core::P;

use crate::protocol::RuleMessage;
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};
use crate::utils::{self, intersection_type_parts, union_type_parts};

const MAXIMUM_TYPE_COMPARISON_DEPTH: u32 = 5;

#[derive(Clone, Copy)]
enum MessageId {
    Access,
    Argument,
    Assertion,
    Assignment,
    Mutation,
    Return,
}

impl MessageId {
    fn message(self, names: &str) -> RuleMessage {
        let (id, description) = match self {
            Self::Access => (
                "unsafeEnumAccess",
                "The computed key used here does not have a shared enum type with the expected enum",
            ),
            Self::Argument => (
                "unsafeEnumArgument",
                "The argument passed here does not have a shared enum type with the expected enum",
            ),
            Self::Assertion => (
                "unsafeEnumAssertion",
                "The value asserted here does not have a shared enum type with the expected enum",
            ),
            Self::Assignment => (
                "unsafeEnumAssignment",
                "The value assigned here does not have a shared enum type with the expected enum",
            ),
            Self::Mutation => (
                "unsafeEnumMutation",
                "This mutation can produce a value outside of the expected enum",
            ),
            Self::Return => (
                "unsafeEnumReturn",
                "The value returned here does not have a shared enum type with the expected enum",
            ),
        };
        RuleMessage {
            id: id.into(),
            description: format!("{description} {names}."),
            ..Default::default()
        }
    }
}

/// Go `enumTypeChecker` plus the rule's `checked` set; a rule instance lives for one file.
#[derive(Default)]
pub struct NoUnsafeEnumAssignment {
    generic_types: FxHashMap<P<Type>, bool>,
    mismatches: FxHashMap<(P<Type>, P<Type>), bool>,
    checked: FxHashSet<P<Node>>,
}

fn constraint(checker: &mut Checker, t: P<Type>) -> P<Type> {
    checker.get_base_constraint_of_type(t).unwrap_or(t)
}

fn type_arguments(checker: &mut Checker, t: P<Type>) -> &'static [P<Type>] {
    if t.flags().intersects(TypeFlags::Object)
        && t.object_flags().intersects(ObjectFlags::Reference)
    {
        checker.get_type_arguments(t)
    } else {
        &[]
    }
}

fn primitive_like(t: P<Type>, flags: TypeFlags) -> bool {
    union_type_parts(&t)
        .iter()
        .all(|part| intersection_type_parts(part).iter().any(|part| part.flags().intersects(flags)))
}

fn direct_mismatch(checker: &mut Checker, sender: P<Type>, receiver: P<Type>) -> bool {
    let receiver_enums = utils::get_enum_types(checker, receiver);
    let mut non_enums = Vec::new();
    let mut number_enum = false;
    let mut string_enum = false;
    for &part in union_type_parts(&receiver) {
        if part.flags().intersects(TypeFlags::EnumLike) {
            if part.flags().intersects(TypeFlags::NumberLiteral) {
                number_enum = true;
            } else {
                string_enum = true;
            }
        }
        if utils::get_enum_types(checker, part).is_empty() {
            non_enums.push(part);
        }
    }
    for &part in union_type_parts(&sender) {
        if !(number_enum && primitive_like(part, TypeFlags::NumberLike)
            || string_enum && primitive_like(part, TypeFlags::StringLike))
        {
            continue;
        }
        let shared = utils::get_enum_types(checker, part)
            .iter()
            .any(|sender_enum| receiver_enums.contains(sender_enum));
        if !shared
            && !non_enums.iter().any(|&receiver| checker.is_type_assignable_to(part, receiver))
        {
            return true;
        }
    }
    false
}

/// The quoted enum names reachable from `types`, sorted and joined by ", ".
fn describe(checker: &mut Checker, types: &[P<Type>]) -> String {
    fn visit(
        checker: &mut Checker,
        t: P<Type>,
        depth: u32,
        names: &mut FxHashSet<String>,
        visited: &mut FxHashMap<P<Type>, u32>,
    ) {
        if depth > MAXIMUM_TYPE_COMPARISON_DEPTH {
            return;
        }
        let t = constraint(checker, t);
        if visited.get(&t).is_some_and(|&previous| previous <= depth) {
            return;
        }
        visited.insert(t, depth);
        for enum_type in utils::get_enum_types(checker, t) {
            names.insert(checker.type_to_string_exported(enum_type));
        }
        for &argument in type_arguments(checker, t) {
            visit(checker, argument, depth + 1, names, visited);
        }
        if let Some(element) = checker.get_number_index_type(t) {
            visit(checker, element, depth + 1, names, visited);
        }
        for &property in checker.get_properties_of_type(t) {
            let property_type = checker.get_type_of_symbol(property);
            visit(checker, property_type, depth + 1, names, visited);
        }
    }
    let mut names = FxHashSet::default();
    let mut visited = FxHashMap::default();
    for &t in types {
        visit(checker, t, 0, &mut names, &mut visited);
    }
    let mut names: Vec<_> = names.into_iter().map(|name| format!("'{name}'")).collect();
    names.sort_unstable();
    names.join(", ")
}

fn mapped_key_constraint_types(checker: &mut Checker, declaration: P<Node>) -> Vec<P<Type>> {
    if !matches!(
        declaration.kind(),
        Kind::GetAccessor
            | Kind::Parameter
            | Kind::PropertyDeclaration
            | Kind::PropertySignature
            | Kind::VariableDeclaration
    ) {
        return Vec::new();
    }
    let Some(type_node) = declaration.type_node() else {
        return Vec::new();
    };
    if type_node.kind() == Kind::TypeReference {
        let t = checker.get_type_from_type_node(type_node);
        if t.flags().intersects(TypeFlags::Object)
            && t.object_flags().intersects(ObjectFlags::Mapped)
        {
            if let Some(symbol) = t.symbol()
                && symbol.declarations().iter().any(|&declaration| {
                    declaration.kind() == Kind::MappedType
                        && declaration.as_mapped_type_node().name_type.is_some()
                })
            {
                return Vec::new();
            }
            // Resolving the members sets the mapped type's constraint.
            checker.get_properties_of_type(t);
            if let Some(constraint) = t.as_mapped_type().constraint_type() {
                let constrained =
                    checker.get_base_constraint_of_type(constraint).unwrap_or(constraint);
                if !utils::get_enum_types(checker, constrained).is_empty() {
                    return vec![constraint];
                }
            }
        }
    }
    if type_node.kind() == Kind::MappedType
        && let Some(constraint) = type_node
            .as_mapped_type_node()
            .type_parameter
            .as_type_parameter_declaration()
            .constraint
    {
        return vec![checker.get_type_from_type_node(constraint)];
    }
    let mut result = Vec::new();
    if type_node.kind() == Kind::TypeLiteral {
        for &member in type_node.as_type_literal_node().members.nodes() {
            if let Some(name) = member.name()
                && name.kind() == Kind::ComputedPropertyName
            {
                result.push(checker.get_type_at_location(name.expression().unwrap()));
            }
        }
    }
    result
}

/// Go `functionSignature`.
struct FunctionSignature {
    parameters: Vec<P<Type>>,
    rest: Vec<P<Type>>,
    rest_index: usize,
    index: usize,
    consumed: bool,
}

impl FunctionSignature {
    fn new(checker: &mut Checker, node: P<Node>) -> Self {
        let signature = checker.get_resolved_signature(node, None, CheckMode::Normal);
        let mut result = Self {
            parameters: Vec::new(),
            rest: Vec::new(),
            rest_index: 0,
            index: 0,
            consumed: false,
        };
        for (index, &parameter) in signature.parameters().iter().enumerate() {
            let t = checker
                .get_type_of_symbol_at_location(parameter, Some(node))
                .expect("call-like locations resolve parameter types");
            let constrained = constraint(checker, t);
            if parameter
                .declarations()
                .first()
                .is_some_and(|&declaration| utils::is_rest_parameter_declaration(declaration))
            {
                result.rest_index = index;
                result.rest = if constrained.is_tuple_type() {
                    type_arguments(checker, constrained).to_vec()
                } else if let Some(element) = checker.get_number_index_type(constrained) {
                    vec![element]
                } else {
                    vec![constrained]
                };
                break;
            }
            result.parameters.push(t);
        }
        result
    }

    fn next(&mut self) -> Option<P<Type>> {
        let index = self.index;
        self.index += 1;
        if index < self.parameters.len() && !self.consumed {
            return Some(self.parameters[index]);
        }
        if self.rest.is_empty() {
            return None;
        }
        if self.consumed || index - self.rest_index >= self.rest.len() {
            return self.rest.last().copied();
        }
        Some(self.rest[index - self.rest_index])
    }
}

fn report(context: &mut RuleContext<'_>, node: P<Node>, id: MessageId, types: &[P<Type>]) {
    let names = describe(context.checker, types);
    let mut range = trim_node_range(context.source_file, node);
    if node.kind() == Kind::Parameter {
        range.pos = trim_node_range(context.source_file, node.name().unwrap()).pos;
    }
    context.report_range(range, id.message(&names));
}

fn check_mutation(context: &mut RuleContext<'_>, target: P<Node>, reporting: P<Node>) {
    let t = context.checker.get_type_at_location(target);
    let constrained = constraint(context.checker, t);
    if !utils::get_enum_types(context.checker, constrained).is_empty() {
        report(context, reporting, MessageId::Mutation, &[t]);
    }
}

fn parent_function(node: P<Node>) -> Option<P<Node>> {
    let mut current = node.parent();
    while let Some(node) = current {
        if ast::is_function_like_declaration(node) {
            return Some(node);
        }
        current = node.parent();
    }
    None
}

fn literal_member_name(t: P<Type>) -> Option<String> {
    match t.as_literal_type().value()? {
        LiteralValue::String(value) => Some(value.to_string()),
        LiteralValue::Number(value) => Some(value.to_string()),
        LiteralValue::Boolean(_) | LiteralValue::BigInt(_) => None,
    }
}

impl NoUnsafeEnumAssignment {
    fn is_generic(&mut self, checker: &mut Checker, t: P<Type>) -> bool {
        if let Some(&generic) = self.generic_types.get(&t) {
            return generic;
        }
        self.generic_types.insert(t, false);
        let mut generic = t.flags().intersects(TypeFlags::Instantiable);
        if !generic && t.flags().intersects(TypeFlags::UnionOrIntersection) {
            generic = t.types().iter().any(|&part| self.is_generic(checker, part));
        }
        if !generic && let Some(alias) = t.alias() {
            generic =
                alias.type_arguments().iter().any(|&argument| self.is_generic(checker, argument));
        }
        if !generic {
            generic = type_arguments(checker, t)
                .iter()
                .any(|&argument| self.is_generic(checker, argument));
        }
        self.generic_types.insert(t, generic);
        generic
    }

    fn mismatch(&mut self, checker: &mut Checker, sender: P<Type>, receiver: P<Type>) -> bool {
        if let Some(&mismatch) = self.mismatches.get(&(sender, receiver)) {
            return mismatch;
        }
        let mismatch =
            self.deep_mismatch(checker, sender, receiver, &mut FxHashMap::default(), 0, false);
        self.mismatches.insert((sender, receiver), mismatch);
        mismatch
    }

    fn deep_mismatch(
        &mut self,
        checker: &mut Checker,
        sender: P<Type>,
        receiver: P<Type>,
        visited: &mut FxHashMap<(P<Type>, P<Type>, bool), u32>,
        depth: u32,
        within_generic_members: bool,
    ) -> bool {
        if depth > MAXIMUM_TYPE_COMPARISON_DEPTH {
            return false;
        }
        let sender = constraint(checker, sender);
        let receiver = constraint(checker, receiver);
        if sender == receiver {
            return false;
        }
        let visit = (sender, receiver, within_generic_members);
        if visited.get(&visit).is_some_and(|&previous| previous <= depth) {
            return false;
        }
        visited.insert(visit, depth);
        if direct_mismatch(checker, sender, receiver) {
            return true;
        }
        let sender_arguments = type_arguments(checker, sender);
        let receiver_arguments = type_arguments(checker, receiver);
        if sender_arguments.len() == receiver_arguments.len() {
            for (&argument, &receiver_argument) in sender_arguments.iter().zip(receiver_arguments) {
                if self.deep_mismatch(
                    checker,
                    argument,
                    receiver_argument,
                    visited,
                    depth + 1,
                    within_generic_members,
                ) {
                    return true;
                }
            }
        }
        let generic = self.is_generic(checker, sender) || self.is_generic(checker, receiver);
        if generic && within_generic_members {
            return false;
        }
        let sender_element = checker.get_number_index_type(sender);
        let receiver_element = checker.get_number_index_type(receiver);
        if let (Some(sender_element), Some(receiver_element)) = (sender_element, receiver_element)
            && self.deep_mismatch(
                checker,
                sender_element,
                receiver_element,
                visited,
                depth + 1,
                generic || within_generic_members,
            )
        {
            return true;
        }
        for &receiver_property in checker.get_properties_of_type(receiver) {
            let Some(sender_property) =
                checker.get_property_of_type(sender, receiver_property.name())
            else {
                continue;
            };
            let sender_type = checker.get_type_of_symbol(sender_property);
            let receiver_type = checker.get_type_of_symbol(receiver_property);
            if self.deep_mismatch(
                checker,
                sender_type,
                receiver_type,
                visited,
                depth + 1,
                generic || within_generic_members,
            ) {
                return true;
            }
        }
        false
    }

    fn mark_checked(&mut self, node: P<Node>) {
        let node = ast::skip_parentheses(node);
        self.checked.insert(node);
        match node.kind() {
            Kind::ArrayLiteralExpression => {
                for &element in node.elements() {
                    self.mark_checked(element);
                }
            }
            Kind::ObjectLiteralExpression => {
                for &property in node.properties() {
                    self.mark_checked(property);
                }
            }
            Kind::ConditionalExpression => {
                let expression = node.as_conditional_expression();
                self.mark_checked(expression.when_true);
                self.mark_checked(expression.when_false);
            }
            Kind::BinaryExpression => {
                let expression = node.as_binary_expression();
                if matches!(
                    expression.operator_token.kind(),
                    Kind::AmpersandAmpersandToken | Kind::BarBarToken | Kind::QuestionQuestionToken
                ) {
                    self.mark_checked(expression.left);
                    self.mark_checked(expression.right());
                }
            }
            Kind::PropertyAssignment => self.mark_checked(node.initializer().unwrap()),
            Kind::ShorthandPropertyAssignment => self.mark_checked(node.name().unwrap()),
            Kind::SpreadElement
            | Kind::SpreadAssignment
            | Kind::AsExpression
            | Kind::TypeAssertionExpression
            | Kind::NonNullExpression
            | Kind::SatisfiesExpression => self.mark_checked(node.expression().unwrap()),
            _ => {}
        }
    }

    /// `(a & b)`, `(a | b)`, `(a ^ b)` and `~a` whose operands are all safe for `receiver`.
    fn safe_bitwise(checker: &mut Checker, node: P<Node>, receiver: P<Type>) -> bool {
        let node = ast::skip_parentheses(node);
        let safe_operand = |checker: &mut Checker, operand: P<Node>| {
            Self::safe_bitwise(checker, operand, receiver) || {
                let operand_type = checker.get_type_at_location(operand);
                !direct_mismatch(checker, operand_type, receiver)
            }
        };
        match node.kind() {
            Kind::BinaryExpression => {
                let expression = node.as_binary_expression();
                matches!(
                    expression.operator_token.kind(),
                    Kind::AmpersandToken | Kind::CaretToken | Kind::BarToken
                ) && safe_operand(checker, expression.left)
                    && safe_operand(checker, expression.right())
            }
            Kind::PrefixUnaryExpression => {
                let expression = node.as_prefix_unary_expression();
                expression.operator == Kind::TildeToken && safe_operand(checker, expression.operand)
            }
            _ => false,
        }
    }

    fn unsafe_assignment(
        &mut self,
        checker: &mut Checker,
        sender_node: P<Node>,
        receiver: P<Type>,
        sender: Option<P<Type>>,
    ) -> bool {
        let sender = sender.unwrap_or_else(|| checker.get_type_at_location(sender_node));
        self.mismatch(checker, sender, receiver)
            && !Self::safe_bitwise(checker, sender_node, receiver)
    }

    fn check_assignment(
        &mut self,
        context: &mut RuleContext<'_>,
        receiver: P<Type>,
        sender: P<Node>,
        reporting: P<Node>,
        id: MessageId,
        sender_type: Option<P<Type>>,
    ) {
        let sender = ast::skip_parentheses(sender);
        if matches!(sender.kind(), Kind::ArrayLiteralExpression | Kind::ObjectLiteralExpression)
            || !self.unsafe_assignment(context.checker, sender, receiver, sender_type)
        {
            return;
        }
        report(context, reporting, id, &[receiver]);
        self.mark_checked(sender);
    }

    fn check_arguments(
        &mut self,
        context: &mut RuleContext<'_>,
        node: P<Node>,
        arguments: &[P<Node>],
        tagged: bool,
    ) {
        let mut signature = FunctionSignature::new(context.checker, node);
        if tagged {
            signature.next();
        }
        for &argument in arguments {
            if argument.kind() == Kind::SpreadElement {
                let spread_type =
                    context.checker.get_type_at_location(argument.expression().unwrap());
                if spread_type.is_tuple_type() {
                    let mut mismatches = Vec::new();
                    for &element in type_arguments(context.checker, spread_type) {
                        if let Some(parameter) = signature.next()
                            && self.mismatch(context.checker, element, parameter)
                        {
                            mismatches.push(parameter);
                        }
                    }
                    if !mismatches.is_empty() {
                        report(context, argument, MessageId::Argument, &mismatches);
                    }
                    if spread_type
                        .target()
                        .expect("tuple reference target")
                        .as_tuple_type()
                        .combined_flags()
                        .intersects(ElementFlags::Variable)
                    {
                        signature.consumed = true;
                    }
                    continue;
                }
            }
            if let Some(parameter) = signature.next() {
                self.check_assignment(
                    context,
                    parameter,
                    argument,
                    argument,
                    MessageId::Argument,
                    None,
                );
            }
        }
    }

    fn check_return(&mut self, context: &mut RuleContext<'_>, sender: P<Node>, reporting: P<Node>) {
        let Some(function) = parent_function(sender) else {
            return;
        };
        let signature = context.checker.get_signature_from_declaration_exported(function);
        let mut receiver = Some(context.checker.get_return_type_of_signature(signature));
        let mut sender_type = Some(context.checker.get_type_at_location(sender));
        if ast::is_async_function(function) {
            receiver = receiver.and_then(|t| context.checker.get_awaited_type(t));
            sender_type = sender_type.and_then(|t| context.checker.get_awaited_type(t));
        }
        if let (Some(receiver), Some(sender_type)) = (receiver, sender_type) {
            self.check_assignment(
                context,
                receiver,
                sender,
                reporting,
                MessageId::Return,
                Some(sender_type),
            );
        }
    }

    fn check_contextual(
        &mut self,
        context: &mut RuleContext<'_>,
        receiver_node: P<Node>,
        sender: P<Node>,
        reporting: P<Node>,
    ) {
        if let Some(receiver) =
            context.checker.get_contextual_type(receiver_node, ContextFlags::None)
        {
            self.check_assignment(
                context,
                receiver,
                sender,
                reporting,
                MessageId::Assignment,
                None,
            );
        }
    }

    fn check_property_declaration(&mut self, context: &mut RuleContext<'_>, node: P<Node>) {
        let Some(initializer) = node.initializer() else {
            return;
        };
        let name = node.name().unwrap();
        let member_name = match name.kind() {
            Kind::ComputedPropertyName => {
                let t = context.checker.get_type_at_location(name.expression().unwrap());
                if t.flags().intersects(TypeFlags::StringLiteral | TypeFlags::NumberLiteral) {
                    literal_member_name(t)
                } else {
                    None
                }
            }
            Kind::Identifier | Kind::StringLiteral | Kind::NumericLiteral => {
                Some(name.text().to_string())
            }
            _ => None,
        };
        let mut heritage_types = Vec::new();
        if let Some(member_name) = member_name
            && let Some(clauses) = utils::get_heritage_clauses(node.parent().unwrap())
        {
            let is_static = ast::is_static(node);
            for &clause in clauses.nodes() {
                let clause = clause.as_heritage_clause();
                if is_static && clause.token != Kind::ExtendsKeyword {
                    continue;
                }
                for &heritage in clause.types.get().nodes() {
                    let heritage =
                        if is_static { heritage.expression().unwrap() } else { heritage };
                    let heritage_type = context.checker.get_type_at_location(heritage);
                    if let Some(symbol) =
                        context.checker.get_property_of_type(heritage_type, &member_name)
                    {
                        heritage_types.push(context.checker.get_type_of_symbol(symbol));
                    }
                }
            }
        }
        if !heritage_types.is_empty()
            && heritage_types
                .iter()
                .all(|&t| self.unsafe_assignment(context.checker, initializer, t, None))
        {
            report(context, node, MessageId::Assignment, &heritage_types);
            self.mark_checked(initializer);
            return;
        }
        let receiver = context.checker.get_type_at_location(node);
        self.check_assignment(context, receiver, initializer, node, MessageId::Assignment, None);
    }

    fn check_element_access(&mut self, context: &mut RuleContext<'_>, node: P<Node>) {
        let expression = node.as_element_access_expression();
        let Some(symbol) = context.checker.get_symbol_at_location_exported(expression.expression)
        else {
            return;
        };
        let mut receivers = Vec::new();
        for &declaration in symbol.declarations() {
            receivers.extend(mapped_key_constraint_types(context.checker, declaration));
        }
        if receivers.is_empty() {
            return;
        }
        let sender = context.checker.get_type_at_location(expression.argument_expression);
        if receivers.iter().all(|&receiver| {
            self.mismatch(context.checker, sender, receiver)
                || !context.checker.is_type_assignable_to(sender, receiver)
        }) {
            report(context, expression.argument_expression, MessageId::Access, &receivers);
        }
    }
}

impl Rule for NoUnsafeEnumAssignment {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(
            listener,
            Listener::Enter(
                Kind::VariableDeclaration
                    | Kind::Parameter
                    | Kind::BindingElement
                    | Kind::ShorthandPropertyAssignment
                    | Kind::PropertyDeclaration
                    | Kind::BinaryExpression
                    | Kind::PrefixUnaryExpression
                    | Kind::PostfixUnaryExpression
                    | Kind::AsExpression
                    | Kind::TypeAssertionExpression
                    | Kind::SatisfiesExpression
                    | Kind::ReturnStatement
                    | Kind::ArrowFunction
                    | Kind::CallExpression
                    | Kind::NewExpression
                    | Kind::TaggedTemplateExpression
                    | Kind::JsxAttribute
                    | Kind::ElementAccessExpression
            ) | Listener::ExpressionEnter(
                Kind::ArrayLiteralExpression | Kind::ObjectLiteralExpression
            )
        )
    }

    fn visit(&mut self, listener: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        match listener {
            Listener::ExpressionEnter(Kind::ArrayLiteralExpression) => {
                if self.checked.contains(&node) {
                    return;
                }
                for &element in node.elements() {
                    if element.kind() != Kind::OmittedExpression {
                        self.check_contextual(context, element, element, element);
                    }
                }
            }
            Listener::ExpressionEnter(Kind::ObjectLiteralExpression) => {
                if self.checked.contains(&node) {
                    return;
                }
                for &property in node.properties() {
                    let (sender, receiver_node) = match property.kind() {
                        Kind::SpreadAssignment => (property.expression().unwrap(), node),
                        Kind::PropertyAssignment => {
                            let sender = property.initializer().unwrap();
                            (sender, sender)
                        }
                        Kind::ShorthandPropertyAssignment => {
                            let sender = property.name().unwrap();
                            (sender, sender)
                        }
                        _ => continue,
                    };
                    self.check_contextual(context, receiver_node, sender, property);
                }
            }
            _ => match node.kind() {
                Kind::VariableDeclaration | Kind::Parameter | Kind::BindingElement => {
                    if let Some(initializer) = node.initializer() {
                        let receiver = context.checker.get_type_at_location(node.name().unwrap());
                        self.check_assignment(
                            context,
                            receiver,
                            initializer,
                            node,
                            MessageId::Assignment,
                            None,
                        );
                    }
                }
                Kind::ShorthandPropertyAssignment => {
                    if let Some(initializer) =
                        node.as_shorthand_property_assignment().object_assignment_initializer()
                    {
                        let receiver = context.checker.get_type_at_location(node.name().unwrap());
                        self.check_assignment(
                            context,
                            receiver,
                            initializer,
                            node,
                            MessageId::Assignment,
                            None,
                        );
                    }
                }
                Kind::PropertyDeclaration => self.check_property_declaration(context, node),
                Kind::BinaryExpression => {
                    if !ast::is_assignment_expression(node, false) {
                        return;
                    }
                    let expression = node.as_binary_expression();
                    match expression.operator_token.kind() {
                        Kind::EqualsToken
                        | Kind::AmpersandAmpersandEqualsToken
                        | Kind::AmpersandEqualsToken
                        | Kind::QuestionQuestionEqualsToken
                        | Kind::CaretEqualsToken
                        | Kind::BarEqualsToken
                        | Kind::BarBarEqualsToken => {
                            let receiver = context.checker.get_type_at_location(expression.left);
                            self.check_assignment(
                                context,
                                receiver,
                                expression.right(),
                                node,
                                MessageId::Assignment,
                                None,
                            );
                        }
                        _ => check_mutation(context, expression.left, node),
                    }
                }
                Kind::PrefixUnaryExpression => {
                    let expression = node.as_prefix_unary_expression();
                    if matches!(expression.operator, Kind::PlusPlusToken | Kind::MinusMinusToken) {
                        check_mutation(context, expression.operand, node);
                    }
                }
                Kind::PostfixUnaryExpression => {
                    check_mutation(context, node.as_postfix_unary_expression().operand, node);
                }
                Kind::AsExpression | Kind::TypeAssertionExpression => {
                    let receiver = context.checker.get_type_at_location(node.type_node().unwrap());
                    self.check_assignment(
                        context,
                        receiver,
                        node.expression().unwrap(),
                        node,
                        MessageId::Assertion,
                        None,
                    );
                }
                Kind::SatisfiesExpression => {
                    if !self.checked.contains(&node) {
                        let receiver =
                            context.checker.get_type_at_location(node.type_node().unwrap());
                        self.check_assignment(
                            context,
                            receiver,
                            node.expression().unwrap(),
                            node,
                            MessageId::Assignment,
                            None,
                        );
                    }
                }
                Kind::ReturnStatement => {
                    if let Some(expression) = node.expression() {
                        self.check_return(context, expression, node);
                    }
                }
                Kind::ArrowFunction => {
                    if let Some(body) = node.body()
                        && body.kind() != Kind::Block
                    {
                        self.check_return(context, body, body);
                    }
                }
                Kind::CallExpression => {
                    self.check_arguments(context, node, node.arguments(), false);
                }
                Kind::NewExpression => {
                    if node.as_new_expression().arguments.is_some() {
                        self.check_arguments(context, node, node.arguments(), false);
                    }
                }
                Kind::TaggedTemplateExpression => {
                    let template = node.as_tagged_template_expression().template;
                    let arguments: Vec<_> = if template.kind() == Kind::TemplateExpression {
                        template
                            .as_template_expression()
                            .template_spans
                            .nodes()
                            .iter()
                            .map(|span| span.expression().unwrap())
                            .collect()
                    } else {
                        Vec::new()
                    };
                    self.check_arguments(context, node, &arguments, true);
                }
                Kind::JsxAttribute => {
                    if let Some(initializer) = node.initializer()
                        && initializer.kind() == Kind::JsxExpression
                        && let Some(expression) = initializer.expression()
                    {
                        self.check_contextual(context, expression, expression, expression);
                    }
                }
                Kind::ElementAccessExpression => self.check_element_access(context, node),
                _ => {}
            },
        }
    }
}
