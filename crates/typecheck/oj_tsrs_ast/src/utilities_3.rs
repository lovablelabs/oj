use tsrs_core::{CompilerOptions, ModuleKind, P, compare_text_ranges};

use crate::*;

pub fn is_call_like_expression(node: P<Node>) -> bool {
    match node.kind() {
        Kind::JsxOpeningElement
        | Kind::JsxSelfClosingElement
        | Kind::JsxOpeningFragment
        | Kind::CallExpression
        | Kind::NewExpression
        | Kind::TaggedTemplateExpression
        | Kind::Decorator => true,
        Kind::BinaryExpression => {
            node.as_binary_expression().operator_token.kind() == Kind::InstanceOfKeyword
        }
        _ => false,
    }
}

pub fn is_jsx_call_like(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::JsxOpeningElement | Kind::JsxSelfClosingElement | Kind::JsxOpeningFragment
    )
}

pub fn is_this_in_type_query(mut node: P<Node>) -> bool {
    if !is_this_identifier(node) {
        return false;
    }
    while is_qualified_name(node.parent().unwrap())
        && node.parent().unwrap().as_qualified_name().left == node
    {
        node = node.parent().unwrap();
    }
    node.parent().unwrap().kind() == Kind::TypeQuery
}

pub fn is_class_member_modifier(token: Kind) -> bool {
    is_parameter_property_modifier(token)
        || token == Kind::StaticKeyword
        || token == Kind::OverrideKeyword
        || token == Kind::AccessorKeyword
}

pub fn is_parameter_property_modifier(kind: Kind) -> bool {
    modifier_to_flag(kind).intersects(ModifierFlags::ParameterPropertyModifier)
}

pub fn is_type_reference_type(node: P<Node>) -> bool {
    node.kind() == Kind::TypeReference || node.kind() == Kind::ExpressionWithTypeArguments
}

pub fn is_variable_like(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::BindingElement
            | Kind::EnumMember
            | Kind::Parameter
            | Kind::PropertyAssignment
            | Kind::PropertyDeclaration
            | Kind::PropertySignature
            | Kind::ShorthandPropertyAssignment
            | Kind::VariableDeclaration
    )
}

pub fn get_class_extends_heritage_element(node: P<Node>) -> Option<P<Node>> {
    get_heritage_elements(node, Kind::ExtendsKeyword).first().copied()
}

pub fn is_valid_type_only_alias_use_site(use_site: P<Node>) -> bool {
    use_site.flags().intersects(NodeFlags::Ambient | NodeFlags::JSDoc)
        || is_part_of_type_query(use_site)
        || is_identifier_in_non_emitting_heritage_clause(use_site)
        || is_part_of_possibly_valid_type_or_abstract_computed_property_name(use_site)
        || !(is_expression_node(use_site) || is_shorthand_property_name_use_site(use_site))
}

fn is_identifier_in_non_emitting_heritage_clause(node: P<Node>) -> bool {
    if !is_identifier(node) {
        return false;
    }
    let mut parent = node.parent().unwrap();
    while is_property_access_expression(parent) || is_expression_with_type_arguments(parent) {
        parent = parent.parent().unwrap();
    }
    is_heritage_clause(parent)
        && (parent.as_heritage_clause().token == Kind::ImplementsKeyword
            || is_interface_declaration(parent.parent().unwrap()))
}

fn is_part_of_possibly_valid_type_or_abstract_computed_property_name(mut node: P<Node>) -> bool {
    while node_kind_is(node, &[Kind::Identifier, Kind::PropertyAccessExpression]) {
        node = node.parent().unwrap();
    }
    if node.kind() != Kind::ComputedPropertyName {
        return false;
    }
    let parent = node.parent().unwrap();
    if has_syntactic_modifier(parent, ModifierFlags::Abstract) {
        return true;
    }
    node_kind_is(parent.parent().unwrap(), &[Kind::InterfaceDeclaration, Kind::TypeLiteral])
}

fn is_shorthand_property_name_use_site(use_site: P<Node>) -> bool {
    is_identifier(use_site)
        && is_shorthand_property_assignment(use_site.parent().unwrap())
        && use_site.parent().unwrap().name() == Some(use_site)
}

pub fn get_property_name_for_property_name_node(name: P<Node>) -> std::borrow::Cow<'static, str> {
    match name.kind() {
        Kind::Identifier
        | Kind::PrivateIdentifier
        | Kind::StringLiteral
        | Kind::NoSubstitutionTemplateLiteral
        | Kind::NumericLiteral
        | Kind::BigIntLiteral
        | Kind::JsxNamespacedName => name.text().into(),
        Kind::ComputedPropertyName => {
            let name_expression = name.expression().unwrap();
            if is_string_or_numeric_literal_like(name_expression) {
                return name_expression.text().into();
            }
            if is_signed_numeric_literal(name_expression) {
                let mut text =
                    name_expression.as_prefix_unary_expression().operand.text().to_string();
                if name_expression.as_prefix_unary_expression().operator == Kind::MinusToken {
                    text = format!("-{text}");
                }
                return text.into();
            }
            INTERNAL_SYMBOL_NAME_MISSING.into()
        }
        _ => panic!("Unhandled case in getPropertyNameForPropertyNameNode"),
    }
}

pub fn is_part_of_type_only_import_or_export_declaration(node: P<Node>) -> bool {
    find_ancestor(node, is_type_only_import_or_export_declaration).is_some()
}

pub fn is_emittable_import(node: P<Node>) -> bool {
    match node.kind() {
        Kind::ImportDeclaration => {
            node.import_clause().is_some_and(|import_clause| !import_clause.is_type_only())
        }
        Kind::ExportDeclaration | Kind::ImportEqualsDeclaration => !node.is_type_only(),
        Kind::CallExpression => is_import_call(node),
        _ => false,
    }
}

pub fn is_resolution_mode_override_host(node: impl Into<Option<P<Node>>>) -> bool {
    match node.into() {
        Some(node) => matches!(
            node.kind(),
            Kind::ImportType
                | Kind::ExportDeclaration
                | Kind::ImportDeclaration
                | Kind::JSImportDeclaration
        ),
        None => false,
    }
}

pub fn has_resolution_mode_override(node: impl Into<Option<P<Node>>>) -> bool {
    let Some(node) = node.into() else {
        return false;
    };
    let attributes = match node.kind() {
        Kind::ImportType => node.as_import_type_node().attributes,
        Kind::ImportDeclaration | Kind::JSImportDeclaration => {
            node.as_import_declaration().attributes()
        }
        Kind::ExportDeclaration => node.as_export_declaration().attributes,
        _ => None,
    };
    if let Some(attributes) = attributes {
        return ImportAttributes::get_resolution_mode_override(Some(attributes), None).is_some();
    }
    false
}

pub fn is_template_literal_kind(kind: Kind) -> bool {
    Kind::FIRST_TEMPLATE_TOKEN <= kind && kind <= Kind::LAST_TEMPLATE_TOKEN
}

pub fn get_external_module_import_equals_declaration_expression(node: P<Node>) -> Option<P<Node>> {
    assert!(is_external_module_import_equals_declaration(node));
    node.as_import_equals_declaration().module_reference.expression()
}

pub fn create_modifiers_from_modifier_flags(
    flags: ModifierFlags,
    mut create_modifier: impl FnMut(Kind) -> P<Node>,
) -> Vec<P<Node>> {
    let mut result = Vec::new();
    if flags.intersects(ModifierFlags::Export) {
        result.push(create_modifier(Kind::ExportKeyword));
    }
    if flags.intersects(ModifierFlags::Ambient) {
        result.push(create_modifier(Kind::DeclareKeyword));
    }
    if flags.intersects(ModifierFlags::Default) {
        result.push(create_modifier(Kind::DefaultKeyword));
    }
    if flags.intersects(ModifierFlags::Const) {
        result.push(create_modifier(Kind::ConstKeyword));
    }
    if flags.intersects(ModifierFlags::Public) {
        result.push(create_modifier(Kind::PublicKeyword));
    }
    if flags.intersects(ModifierFlags::Private) {
        result.push(create_modifier(Kind::PrivateKeyword));
    }
    if flags.intersects(ModifierFlags::Protected) {
        result.push(create_modifier(Kind::ProtectedKeyword));
    }
    if flags.intersects(ModifierFlags::Abstract) {
        result.push(create_modifier(Kind::AbstractKeyword));
    }
    if flags.intersects(ModifierFlags::Static) {
        result.push(create_modifier(Kind::StaticKeyword));
    }
    if flags.intersects(ModifierFlags::Override) {
        result.push(create_modifier(Kind::OverrideKeyword));
    }
    if flags.intersects(ModifierFlags::Readonly) {
        result.push(create_modifier(Kind::ReadonlyKeyword));
    }
    if flags.intersects(ModifierFlags::Accessor) {
        result.push(create_modifier(Kind::AccessorKeyword));
    }
    if flags.intersects(ModifierFlags::Async) {
        result.push(create_modifier(Kind::AsyncKeyword));
    }
    if flags.intersects(ModifierFlags::In) {
        result.push(create_modifier(Kind::InKeyword));
    }
    if flags.intersects(ModifierFlags::Out) {
        result.push(create_modifier(Kind::OutKeyword));
    }
    result
}

pub fn get_this_parameter(signature: P<Node>) -> Option<P<Node>> {
    // callback tags do not currently support this parameters
    let parameters = signature.parameters();
    if !parameters.is_empty() {
        let this_parameter = parameters[0];
        if is_this_parameter(this_parameter) {
            return Some(this_parameter);
        }
    }
    None
}

pub fn replace_modifiers(
    factory: &NodeFactory,
    node: P<Node>,
    modifier_array: Option<P<ModifierList>>,
) -> P<Node> {
    match node.kind() {
        Kind::TypeParameter => {
            let d = node.as_type_parameter_declaration();
            return factory.update_type_parameter_declaration(
                node,
                modifier_array,
                node.name().unwrap(),
                d.constraint,
                d.expression,
                d.default_type,
            );
        }
        Kind::Parameter => {
            return factory.update_parameter_declaration(
                node,
                modifier_array,
                node.as_parameter_declaration().dot_dot_dot_token(),
                node.name().unwrap(),
                node.question_token(),
                node.type_node(),
                node.initializer(),
            );
        }
        Kind::ConstructorType => {
            return factory.update_constructor_type_node(
                node,
                modifier_array,
                node.type_parameter_list(),
                node.parameter_list(),
                node.type_node(),
            );
        }
        Kind::PropertySignature => {
            return factory.update_property_signature_declaration(
                node,
                modifier_array,
                node.name().unwrap(),
                node.postfix_token(),
                node.type_node(),
                node.initializer(),
            );
        }
        Kind::PropertyDeclaration => {
            return factory.update_property_declaration(
                node,
                modifier_array,
                node.name().unwrap(),
                node.postfix_token(),
                node.type_node(),
                node.initializer(),
            );
        }
        Kind::MethodSignature => {
            return factory.update_method_signature_declaration(
                node,
                modifier_array,
                node.name().unwrap(),
                node.postfix_token(),
                node.type_parameter_list(),
                node.parameter_list(),
                node.type_node(),
            );
        }
        Kind::MethodDeclaration => {
            let d = node.as_method_declaration();
            return factory.update_method_declaration(
                node,
                modifier_array,
                d.asterisk_token(),
                node.name().unwrap(),
                node.postfix_token(),
                node.type_parameter_list(),
                node.parameter_list(),
                node.type_node(),
                d.full_signature(),
                node.body(),
            );
        }
        Kind::Constructor => {
            return factory.update_constructor_declaration(
                node,
                modifier_array,
                node.type_parameter_list(),
                node.parameter_list(),
                node.type_node(),
                node.as_constructor_declaration().full_signature(),
                node.body(),
            );
        }
        Kind::GetAccessor => {
            return factory.update_get_accessor_declaration(
                node,
                modifier_array,
                node.name().unwrap(),
                node.type_parameter_list(),
                node.parameter_list(),
                node.type_node(),
                node.as_get_accessor_declaration().full_signature(),
                node.body(),
            );
        }
        Kind::SetAccessor => {
            return factory.update_set_accessor_declaration(
                node,
                modifier_array,
                node.name().unwrap(),
                node.type_parameter_list(),
                node.parameter_list(),
                node.type_node(),
                node.as_set_accessor_declaration().full_signature(),
                node.body(),
            );
        }
        Kind::IndexSignature => {
            return factory.update_index_signature_declaration(
                node,
                modifier_array,
                node.parameter_list(),
                node.type_node(),
            );
        }
        Kind::FunctionExpression => {
            let d = node.as_function_expression();
            return factory.update_function_expression(
                node,
                modifier_array,
                d.asterisk_token(),
                node.name(),
                node.type_parameter_list(),
                node.parameter_list(),
                node.type_node(),
                d.full_signature(),
                node.body(),
            );
        }
        Kind::ArrowFunction => {
            let d = node.as_arrow_function();
            return factory.update_arrow_function(
                node,
                modifier_array,
                node.type_parameter_list(),
                node.parameter_list(),
                node.type_node(),
                d.full_signature(),
                d.equals_greater_than_token,
                node.body(),
            );
        }
        Kind::ClassExpression => {
            return factory.update_class_expression(
                node,
                modifier_array,
                node.name(),
                node.type_parameter_list(),
                node.as_class_expression().heritage_clauses(),
                node.member_list().unwrap(),
            );
        }
        Kind::VariableStatement => {
            return factory.update_variable_statement(
                node,
                modifier_array,
                node.as_variable_statement().declaration_list,
            );
        }
        Kind::FunctionDeclaration => {
            let d = node.as_function_declaration();
            return factory.update_function_declaration(
                node,
                modifier_array,
                d.asterisk_token(),
                node.name(),
                node.type_parameter_list(),
                node.parameter_list(),
                node.type_node(),
                d.full_signature(),
                node.body(),
            );
        }
        Kind::ClassDeclaration => {
            return factory.update_class_declaration(
                node,
                modifier_array,
                node.name(),
                node.type_parameter_list(),
                node.as_class_declaration().heritage_clauses(),
                node.member_list().unwrap(),
            );
        }
        Kind::InterfaceDeclaration => {
            return factory.update_interface_declaration(
                node,
                modifier_array,
                node.name().unwrap(),
                node.type_parameter_list(),
                node.as_interface_declaration().heritage_clauses(),
                node.member_list().unwrap(),
            );
        }
        Kind::TypeAliasDeclaration => {
            return factory.update_type_alias_declaration(
                node,
                modifier_array,
                node.name().unwrap(),
                node.type_parameter_list(),
                node.type_node(),
            );
        }
        Kind::EnumDeclaration => {
            return factory.update_enum_declaration(
                node,
                modifier_array,
                node.name().unwrap(),
                node.member_list().unwrap(),
            );
        }
        Kind::ModuleDeclaration => {
            return factory.update_module_declaration(
                node,
                modifier_array,
                node.as_module_declaration().keyword,
                node.name().unwrap(),
                node.attributes(),
                node.body(),
            );
        }
        Kind::ImportEqualsDeclaration => {
            return factory.update_import_equals_declaration(
                node,
                modifier_array,
                node.is_type_only(),
                node.name().unwrap(),
                node.as_import_equals_declaration().module_reference,
            );
        }
        Kind::ImportDeclaration => {
            return factory.update_import_declaration(
                node,
                modifier_array,
                node.import_clause(),
                node.module_specifier().unwrap(),
                node.as_import_declaration().attributes(),
            );
        }
        Kind::ExportAssignment => {
            let d = node.as_export_assignment();
            return factory.update_export_assignment(
                node,
                modifier_array,
                d.is_export_equals,
                node.type_node(),
                node.expression().unwrap(),
            );
        }
        Kind::ExportDeclaration => {
            let d = node.as_export_declaration();
            return factory.update_export_declaration(
                node,
                modifier_array,
                node.is_type_only(),
                d.export_clause,
                node.module_specifier(),
                d.attributes,
            );
        }
        _ => {}
    }
    panic!(
        "Node that does not have modifiers tried to have modifier replaced: {}",
        node.kind() as i16
    )
}

pub fn is_late_visibility_painted_statement(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::ImportDeclaration
            | Kind::JSImportDeclaration
            | Kind::ImportEqualsDeclaration
            | Kind::VariableStatement
            | Kind::ClassDeclaration
            | Kind::FunctionDeclaration
            | Kind::ModuleDeclaration
            | Kind::TypeAliasDeclaration
            | Kind::JSTypeAliasDeclaration
            | Kind::InterfaceDeclaration
            | Kind::EnumDeclaration
    )
}

pub fn is_external_module_augmentation(node: P<Node>) -> bool {
    is_ambient_module(node) && is_module_augmentation_external(node)
}

pub fn get_source_file_of_module(module: P<Symbol>) -> Option<P<SourceFile>> {
    let declaration =
        module.value_declaration().or_else(|| get_non_augmentation_declaration(module));
    get_source_file_of_node(declaration)
}

pub fn get_non_augmentation_declaration(symbol: P<Symbol>) -> Option<P<Node>> {
    symbol
        .declarations()
        .iter()
        .copied()
        .find(|&d| !is_external_module_augmentation(d) && !is_global_scope_augmentation(d))
}

pub fn is_type_declaration(node: P<Node>) -> bool {
    match node.kind() {
        Kind::TypeParameter
        | Kind::ClassDeclaration
        | Kind::InterfaceDeclaration
        | Kind::TypeAliasDeclaration
        | Kind::JSTypeAliasDeclaration
        | Kind::EnumDeclaration => true,
        Kind::ImportClause => node.is_type_only() && node.name().is_some(),
        Kind::ImportSpecifier | Kind::ExportSpecifier => {
            node.parent().unwrap().parent().unwrap().is_type_only()
        }
        _ => false,
    }
}

pub fn is_type_declaration_name(name: P<Node>) -> bool {
    name.kind() == Kind::Identifier
        && is_type_declaration(name.parent().unwrap())
        && get_name_of_declaration(name.parent()) == Some(name)
}

pub fn is_right_side_of_qualified_name_or_property_access(node: P<Node>) -> bool {
    let parent = node.parent().unwrap();
    match parent.kind() {
        Kind::QualifiedName => parent.as_qualified_name().right == node,
        Kind::PropertyAccessExpression | Kind::MetaProperty => parent.name() == Some(node),
        _ => false,
    }
}

pub fn should_transform_import_call(
    _file_name: &str,
    options: &CompilerOptions,
    implied_node_format_for_emit: ModuleKind,
) -> bool {
    let module_kind = options.get_emit_module_kind();
    if ModuleKind::NODE16 <= module_kind && module_kind <= ModuleKind::NODE_NEXT
        || module_kind == ModuleKind::PRESERVE
    {
        return false;
    }
    implied_node_format_for_emit < ModuleKind::ES2015
}

pub fn has_question_token(node: P<Node>) -> bool {
    is_question_token(node.question_token())
}

pub fn is_jsx_opening_like_element(node: P<Node>) -> bool {
    is_jsx_opening_element(node) || is_jsx_self_closing_element(node)
}

pub fn get_invoked_expression(node: P<Node>) -> P<Node> {
    match node.kind() {
        Kind::TaggedTemplateExpression => node.as_tagged_template_expression().tag,
        Kind::JsxOpeningElement | Kind::JsxSelfClosingElement => node.tag_name(),
        Kind::BinaryExpression => node.as_binary_expression().right(),
        Kind::JsxOpeningFragment => node,
        _ => node.expression().unwrap(),
    }
}

pub fn is_call_or_new_expression(node: P<Node>) -> bool {
    is_call_expression(node) || is_new_expression(node)
}

// Go `slices.BinarySearchFunc`: the leftmost insertion index and whether the target was found.
fn binary_search_func<T: Copy, U: Copy>(
    items: &[T],
    target: U,
    mut cmp: impl FnMut(T, U) -> std::cmp::Ordering,
) -> (usize, bool) {
    let i = items.partition_point(|&item| cmp(item, target).is_lt());
    (i, i < items.len() && cmp(items[i], target).is_eq())
}

pub fn index_of_node(nodes: &[P<Node>], node: P<Node>) -> Option<usize> {
    let (index, ok) = binary_search_func(nodes, node, compare_node_positions);
    ok.then_some(index)
}

pub fn compare_node_positions(n1: P<Node>, n2: P<Node>) -> std::cmp::Ordering {
    compare_text_ranges(n1.loc(), n2.loc())
}

pub fn is_unterminated_literal(node: P<Node>) -> bool {
    is_literal_kind(node.kind())
        && node.literal_like_data().unwrap().token_flags().intersects(TokenFlags::Unterminated)
        || is_template_literal_kind(node.kind())
            && node
                .template_literal_like_data()
                .unwrap()
                .template_flags
                .intersects(TokenFlags::Unterminated)
}

// Gets a value indicating whether a class element is either a static or an instance property declaration with an initializer.
pub fn is_initialized_property(member: P<Node>) -> bool {
    member.kind() == Kind::PropertyDeclaration && member.initializer().is_some()
}

pub fn has_decorators(node: P<Node>) -> bool {
    has_syntactic_modifier(node, ModifierFlags::Decorator)
}

pub struct HasFileNameImpl {
    file_name: String,
    path: tsrs_core::tspath::Path,
}

impl HasFileName for HasFileNameImpl {
    fn file_name(&self) -> &str {
        &self.file_name
    }

    fn path(&self) -> &tsrs_core::tspath::Path {
        &self.path
    }
}

pub fn get_semantic_jsx_children(children: &[P<Node>]) -> Vec<P<Node>> {
    children
        .iter()
        .copied()
        .filter(|i| match i.kind() {
            Kind::JsxExpression => i.expression().is_some(),
            Kind::JsxText => !i.as_jsx_text().contains_only_trivia_white_spaces,
            _ => true,
        })
        .collect()
}

pub fn is_assignment_pattern(node: P<Node>) -> bool {
    node.kind() == Kind::ArrayLiteralExpression || node.kind() == Kind::ObjectLiteralExpression
}

pub fn get_elements_of_binding_or_assignment_pattern(name: P<Node>) -> &'static [P<Node>] {
    match name.kind() {
        // `a` in `{a}`
        // `a` in `[a]`
        Kind::ObjectBindingPattern | Kind::ArrayBindingPattern | Kind::ArrayLiteralExpression => {
            name.elements()
        }
        // `a` in `{a}`
        Kind::ObjectLiteralExpression => name.properties(),
        _ => &[],
    }
}

pub fn is_declaration_binding_element(binding_element: P<Node>) -> bool {
    matches!(
        binding_element.kind(),
        Kind::VariableDeclaration | Kind::Parameter | Kind::BindingElement
    )
}

/**
 * Gets the name of an `BindingOrAssignmentElement`.
 */
pub fn get_target_of_binding_or_assignment_element(binding_element: P<Node>) -> Option<P<Node>> {
    if is_declaration_binding_element(binding_element) {
        // `a` in `let { a } = ...`
        // `a` in `let { a = 1 } = ...`
        // `b` in `let { a: b } = ...`
        // `b` in `let { a: b = 1 } = ...`
        // `a` in `let { ...a } = ...`
        // `{b}` in `let { a: {b} } = ...`
        // `{b}` in `let { a: {b} = 1 } = ...`
        // `[b]` in `let { a: [b] } = ...`
        // `[b]` in `let { a: [b] = 1 } = ...`
        // `a` in `let [a] = ...`
        // `a` in `let [a = 1] = ...`
        // `a` in `let [...a] = ...`
        // `{a}` in `let [{a}] = ...`
        // `{a}` in `let [{a} = 1] = ...`
        // `[a]` in `let [[a]] = ...`
        // `[a]` in `let [[a] = 1] = ...`
        return binding_element.name();
    }

    if is_object_literal_element(binding_element) {
        match binding_element.kind() {
            Kind::PropertyAssignment => {
                // `b` in `({ a: b } = ...)`
                // `b` in `({ a: b = 1 } = ...)`
                // `{b}` in `({ a: {b} } = ...)`
                // `{b}` in `({ a: {b} = 1 } = ...)`
                // `[b]` in `({ a: [b] } = ...)`
                // `[b]` in `({ a: [b] = 1 } = ...)`
                // `b.c` in `({ a: b.c } = ...)`
                // `b.c` in `({ a: b.c = 1 } = ...)`
                // `b[0]` in `({ a: b[0] } = ...)`
                // `b[0]` in `({ a: b[0] = 1 } = ...)`
                return get_target_of_binding_or_assignment_element(
                    binding_element.initializer().unwrap(),
                );
            }
            Kind::ShorthandPropertyAssignment => {
                // `a` in `({ a } = ...)`
                // `a` in `({ a = 1 } = ...)`
                return binding_element.name();
            }
            Kind::SpreadAssignment => {
                // `a` in `({ ...a } = ...)`
                return get_target_of_binding_or_assignment_element(
                    binding_element.expression().unwrap(),
                );
            }
            _ => {}
        }

        // no target
        return None;
    }

    if is_assignment_expression(binding_element, true /*excludeCompoundAssignment*/) {
        // `a` in `[a = 1] = ...`
        // `{a}` in `[{a} = 1] = ...`
        // `[a]` in `[[a] = 1] = ...`
        // `a.b` in `[a.b = 1] = ...`
        // `a[0]` in `[a[0] = 1] = ...`
        return get_target_of_binding_or_assignment_element(
            binding_element.as_binary_expression().left,
        );
    }

    if is_spread_element(binding_element) {
        // `a` in `[...a] = ...`
        return get_target_of_binding_or_assignment_element(binding_element.expression().unwrap());
    }

    // `a` in `[a] = ...`
    // `{a}` in `[{a}] = ...`
    // `[a]` in `[[a]] = ...`
    // `a.b` in `[a.b] = ...`
    // `a[0]` in `[a[0]] = ...`
    Some(binding_element)
}

pub fn is_jsdoc_name_reference_context(node: P<Node>) -> bool {
    node.flags().intersects(NodeFlags::JSDoc)
        && find_ancestor(node, |node| is_jsdoc_name_reference(node) || is_jsdoc_link_like(node))
            .is_some()
}

// GetJSDocRoot returns the containing JSDoc node for a node inside a JSDoc comment.
fn get_jsdoc_root(node: P<Node>) -> Option<P<Node>> {
    find_ancestor(node.parent(), |n| n.kind() == Kind::JSDoc)
}

// GetJSDocHost returns the declaration that the JSDoc comment containing the given node is attached to.
fn get_jsdoc_host(node: P<Node>) -> Option<P<Node>> {
    get_jsdoc_root(node)?.parent()
}

// GetHostSignatureFromJSDoc returns the function-like declaration that hosts the JSDoc comment
// containing the given node. This is used to resolve @link references to parameters.
pub fn get_host_signature_from_jsdoc(node: P<Node>) -> Option<P<Node>> {
    let host = get_jsdoc_host(node)?;
    // !!! Strada's getEffectiveJSDocHost applies JS assignment pattern transforms (getSourceOfAssignment, getSourceOfDefaultedAssignment, etc.) not yet ported
    if is_property_signature_declaration(host)
        && let Some(t) = host.type_node()
        && is_function_like(t)
    {
        return Some(t);
    }
    if is_function_like(host) {
        return Some(host);
    }
    None
}

// Finds the declaration that owns the JSDoc for a function-like node.
// Keep these hosts aligned with JSDoc parameter reparsing so unmatched @param diagnostics use the same attachment rules.
// Keep in sync with getNextJSDocCommentLocation in the API's src/ast/jsdoc.ts
pub fn get_next_jsdoc_comment_location(node: P<Node>) -> Option<P<Node>> {
    if let Some(parent) = node.parent() {
        match parent.kind() {
            Kind::PropertyAssignment
            | Kind::ExportAssignment
            | Kind::PropertyDeclaration
            | Kind::VariableDeclaration
            | Kind::SatisfiesExpression
            | Kind::ReturnStatement
            | Kind::VariableStatement
            | Kind::ExpressionStatement => return Some(parent),
            Kind::VariableDeclarationList
                if parent.as_variable_declaration_list().declarations.nodes()[0] == node =>
            {
                return Some(parent);
            }
            _ => {}
        }
    }
    None
}

pub fn is_import_or_import_equals_declaration(node: P<Node>) -> bool {
    is_import_declaration(node) || is_import_equals_declaration(node)
}

pub fn is_variable_parameter_or_property(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::VariableDeclaration
            | Kind::Parameter
            | Kind::PropertySignature
            | Kind::PropertyDeclaration
    )
}

pub fn is_primitive_literal_value(node: P<Node>, include_big_int: bool) -> bool {
    match node.kind() {
        Kind::TrueKeyword
        | Kind::FalseKeyword
        | Kind::NumericLiteral
        | Kind::StringLiteral
        | Kind::NoSubstitutionTemplateLiteral => true,
        Kind::BigIntLiteral => include_big_int,
        Kind::PrefixUnaryExpression => {
            let p = node.as_prefix_unary_expression();
            if p.operator == Kind::MinusToken {
                return is_numeric_literal(p.operand)
                    || (include_big_int && is_big_int_literal(p.operand));
            }
            if p.operator == Kind::PlusToken {
                return is_numeric_literal(p.operand);
            }
            false
        }
        _ => false,
    }
}

pub fn has_inferred_type(node: P<Node>) -> bool {
    // Debug.type<HasInferredType>(node); // !!!
    match node.kind() {
        Kind::Parameter
        | Kind::PropertySignature
        | Kind::PropertyDeclaration
        | Kind::BindingElement
        | Kind::PropertyAccessExpression
        | Kind::ElementAccessExpression
        | Kind::BinaryExpression
        | Kind::CallExpression
        | Kind::VariableDeclaration
        | Kind::ExportAssignment
        | Kind::PropertyAssignment
        | Kind::ShorthandPropertyAssignment
        | Kind::JSDocParameterTag
        | Kind::JSDocPropertyTag => true,
        // assertType<never>(node); // !!!
        _ => false,
    }
}

pub fn is_keyword(token: Kind) -> bool {
    Kind::FIRST_KEYWORD <= token && token <= Kind::LAST_KEYWORD
}

pub fn has_modifier(node: P<Node>, flags: ModifierFlags) -> bool {
    node.modifier_flags().intersects(flags)
}

pub fn is_expando_initializer(
    declaration: P<Node>,
    initializer: impl Into<Option<P<Node>>>,
) -> bool {
    let Some(initializer) = initializer.into() else {
        return false;
    };
    if is_function_expression_or_arrow_function(initializer) {
        return true;
    }
    if is_in_js_file(initializer) {
        return is_class_expression(initializer)
            || (is_object_literal_expression(initializer)
                && initializer.properties().is_empty()
                && declaration.type_node().is_none());
    }
    false
}

pub fn get_containing_function(node: P<Node>) -> Option<P<Node>> {
    find_ancestor(node.parent(), is_function_like)
}

pub fn is_implicitly_exported_jsdoc_declaration(node: P<Node>) -> bool {
    let parent = node.parent().unwrap();
    if !is_source_file(parent) || !is_external_or_common_js_module(parent.as_source_file_p()) {
        return false;
    }
    if is_js_type_alias_declaration(node) {
        return true;
    }
    // A reparsed ModuleDeclaration synthesized from a JSDoc @typedef/@callback
    // dotted name should also be treated as implicitly exported in modules.
    is_module_declaration(node) && node.flags().intersects(NodeFlags::Reparsed)
}

pub fn has_context_sensitive_parameters(node: P<Node>) -> bool {
    // Functions with type parameters are not context sensitive.
    if node.type_parameter_list().is_none() {
        // Functions with any parameters that lack type annotations are context sensitive.
        if node.parameters().iter().any(|p| p.type_node().is_none()) {
            return true;
        }
        if !is_arrow_function(node) {
            // If the first parameter is not an explicit 'this' parameter, then the function has
            // an implicit 'this' parameter which is subject to contextual typing.
            let parameter = node.parameters().first().copied();
            if parameter.is_none_or(|parameter| !is_this_parameter(parameter)) {
                return node.flags().intersects(NodeFlags::ContainsThis);
            }
        }
    }
    false
}

pub fn is_infinity_or_nan_string(name: &str) -> bool {
    name == "Infinity" || name == "-Infinity" || name == "NaN"
}

pub fn get_first_constructor_with_body(node: P<Node>) -> Option<P<Node>> {
    node.members()
        .iter()
        .copied()
        .find(|&member| is_constructor_declaration(member) && node_is_present(member.body()))
}

// Returns true for nodes that are considered executable for the purposes of unreachable code detection.
pub fn is_potentially_executable_node(node: P<Node>) -> bool {
    if Kind::FIRST_STATEMENT <= node.kind() && node.kind() <= Kind::LAST_STATEMENT {
        if is_variable_statement(node) {
            let declaration_list = node.as_variable_statement().declaration_list;
            if get_combined_node_flags(declaration_list).intersects(NodeFlags::BlockScoped) {
                return true;
            }
            let declarations = declaration_list.as_variable_declaration_list().declarations.nodes();
            return declarations.iter().any(|d| d.initializer().is_some());
        }
        return true;
    }
    is_class_declaration(node) || is_enum_declaration(node) || is_module_declaration(node)
}

pub fn has_abstract_modifier(node: P<Node>) -> bool {
    has_syntactic_modifier(node, ModifierFlags::Abstract)
}

pub fn has_ambient_modifier(node: P<Node>) -> bool {
    has_syntactic_modifier(node, ModifierFlags::Ambient)
}

pub fn node_can_be_decorated(
    use_legacy_decorators: bool,
    node: P<Node>,
    parent: Option<P<Node>>,
    grandparent: Option<P<Node>>,
) -> bool {
    // private names cannot be used with decorators yet
    if use_legacy_decorators && node.name().is_some_and(is_private_identifier) {
        return false;
    }
    match node.kind() {
        // class declarations are valid targets
        Kind::ClassDeclaration => true,
        // class expressions are valid targets for native decorators
        Kind::ClassExpression => !use_legacy_decorators,
        // property declarations are valid if their parent is a class declaration.
        Kind::PropertyDeclaration => parent.is_some_and(|parent| {
            use_legacy_decorators && is_class_declaration(parent)
                || !use_legacy_decorators
                    && is_class_like(parent)
                    && !has_abstract_modifier(node)
                    && !has_ambient_modifier(node)
        }),
        // if this method has a body and its parent is a class declaration, this is a valid target.
        Kind::GetAccessor | Kind::SetAccessor | Kind::MethodDeclaration => {
            parent.is_some_and(|parent| {
                node.body().is_some()
                    && (use_legacy_decorators && is_class_declaration(parent)
                        || !use_legacy_decorators && is_class_like(parent))
            })
        }
        Kind::Parameter => {
            // TODO(rbuckton): ParameterDeclaration decorator support for ES decorators must wait until it is standardized
            if !use_legacy_decorators {
                return false;
            }
            // if the parameter's parent has a body and its grandparent is a class declaration, this is a valid target.
            parent.is_some_and(|parent| {
                parent.body().is_some()
                    && (parent.kind() == Kind::Constructor
                        || parent.kind() == Kind::MethodDeclaration
                        || parent.kind() == Kind::SetAccessor)
                    && get_this_parameter(parent) != Some(node)
                    && grandparent
                        .is_some_and(|grandparent| grandparent.kind() == Kind::ClassDeclaration)
            })
        }
        _ => false,
    }
}

pub fn class_or_constructor_parameter_is_decorated(
    use_legacy_decorators: bool,
    node: P<Node>,
) -> bool {
    if node_is_decorated(use_legacy_decorators, node, None, None) {
        return true;
    }
    get_first_constructor_with_body(node).is_some_and(|constructor| {
        child_is_decorated(use_legacy_decorators, constructor, Some(node))
    })
}

pub fn class_element_or_class_element_parameter_is_decorated(
    use_legacy_decorators: bool,
    node: P<Node>,
    parent: P<Node>,
) -> bool {
    let mut parameters: Option<P<NodeList>> = None;
    if is_accessor(node) {
        let decls = get_all_accessor_declarations(parent.members(), node);
        let first_accessor_with_decorators = if has_decorators(decls.first_accessor) {
            Some(decls.first_accessor)
        } else if decls.second_accessor.is_some_and(has_decorators) {
            decls.second_accessor
        } else {
            None
        };
        if first_accessor_with_decorators != Some(node) {
            return false;
        }
        if let Some(set_accessor) = decls.set_accessor {
            parameters = set_accessor.parameter_list();
        }
    } else if is_method_declaration(node) {
        parameters = node.parameter_list();
    }
    if node_is_decorated(use_legacy_decorators, node, Some(parent), None) {
        return true;
    }
    if let Some(parameters) = parameters
        && parameters.nodes().iter().any(|&parameter| {
            if is_this_parameter(parameter) {
                return false;
            }
            node_is_decorated(use_legacy_decorators, parameter, Some(node), Some(parent))
        })
    {
        return true;
    }
    false
}

pub fn node_is_decorated(
    use_legacy_decorators: bool,
    node: P<Node>,
    parent: Option<P<Node>>,
    grandparent: Option<P<Node>>,
) -> bool {
    has_decorators(node) && node_can_be_decorated(use_legacy_decorators, node, parent, grandparent)
}

pub fn node_or_child_is_decorated(
    use_legacy_decorators: bool,
    node: P<Node>,
    parent: Option<P<Node>>,
    grandparent: Option<P<Node>>,
) -> bool {
    node_is_decorated(use_legacy_decorators, node, parent, grandparent)
        || child_is_decorated(use_legacy_decorators, node, parent)
}

pub fn child_is_decorated(
    use_legacy_decorators: bool,
    node: P<Node>,
    parent: Option<P<Node>>,
) -> bool {
    match node.kind() {
        Kind::ClassDeclaration | Kind::ClassExpression => node
            .members()
            .iter()
            .any(|&m| node_or_child_is_decorated(use_legacy_decorators, m, Some(node), parent)),
        Kind::MethodDeclaration | Kind::SetAccessor | Kind::Constructor => node
            .parameters()
            .iter()
            .any(|&p| node_is_decorated(use_legacy_decorators, p, Some(node), parent)),
        _ => false,
    }
}

// The accessor fields hold the accessor nodes (Go's typed `*SetAccessorDeclaration` etc. are data pointers of these nodes).
#[derive(Clone, Copy, Debug)]
pub struct AllAccessorDeclarations {
    pub first_accessor: P<Node>,
    pub second_accessor: Option<P<Node>>,
    pub set_accessor: Option<P<Node>>,
    pub get_accessor: Option<P<Node>>,
}

pub fn get_all_accessor_declarations_for_declaration(
    accessor: P<Node>,
    declarations_of_symbol: &[P<Node>],
) -> AllAccessorDeclarations {
    let other_kind = if accessor.kind() == Kind::SetAccessor {
        Kind::GetAccessor
    } else if accessor.kind() == Kind::GetAccessor {
        Kind::SetAccessor
    } else {
        panic!("Unexpected node kind {:?}", accessor.kind())
    };
    // otherAccessor := GetDeclarationOfKind(c.getSymbolOfDeclaration(accessor), otherKind)
    let other_accessor = declarations_of_symbol.iter().copied().find(|d| d.kind() == other_kind);

    let (first_accessor, second_accessor) = match other_accessor {
        Some(other) if other.pos() < accessor.pos() => (other, Some(accessor)),
        _ => (accessor, other_accessor),
    };

    let (set_accessor, get_accessor) = if accessor.kind() == Kind::SetAccessor {
        (Some(accessor), other_accessor)
    } else {
        (other_accessor, Some(accessor))
    };

    AllAccessorDeclarations { first_accessor, second_accessor, set_accessor, get_accessor }
}

pub fn get_all_accessor_declarations(
    parent_declarations: &[P<Node>],
    accessor: P<Node>,
) -> AllAccessorDeclarations {
    if has_dynamic_name(accessor) {
        // dynamic names can only be match up via checker symbol lookup, just return an object with just this accessor
        return get_all_accessor_declarations_for_declaration(accessor, &[accessor]);
    }

    let accessor_name = get_property_name_for_property_name_node(accessor.name().unwrap());
    let accessor_static = is_static(accessor);
    let mut matches = Vec::new();
    matches.extend(parent_declarations.iter().filter_map(|&member| {
        if !is_accessor(member) || is_static(member) != accessor_static {
            return None;
        }
        let member_name = get_property_name_for_property_name_node(member.name().unwrap());
        if member_name == accessor_name { Some(member) } else { None }
    }));
    get_all_accessor_declarations_for_declaration(accessor, &matches)
}

pub fn is_async_function(node: P<Node>) -> bool {
    match node.kind() {
        Kind::FunctionDeclaration
        | Kind::FunctionExpression
        | Kind::ArrowFunction
        | Kind::MethodDeclaration => {
            let data = node.body_data().unwrap();
            data.body.is_some()
                && data.asterisk_token.is_none()
                && has_syntactic_modifier(node, ModifierFlags::Async)
        }
        _ => false,
    }
}

/**
 * Gets the most likely element type for a `TypeNode`. This is not an exhaustive test
 * as it assumes a rest argument can only be an array type (either T[], or Array<T>).
 *
 * @param node The type node.
 *
 * @internal
 */
pub fn get_rest_parameter_element_type(node: impl Into<Option<P<Node>>>) -> Option<P<Node>> {
    let node = node.into()?;
    if node.kind() == Kind::ArrayType {
        return Some(node.as_array_type_node().element_type);
    }
    if node.kind() == Kind::TypeReference
        && let Some(type_arguments) = node.as_type_reference_node().type_arguments()
    {
        return type_arguments.nodes().first().copied();
    }
    None
}

pub fn tag_names_are_equivalent(lhs: P<Node>, rhs: P<Node>) -> bool {
    if lhs.kind() != rhs.kind() {
        return false;
    }
    match lhs.kind() {
        Kind::Identifier => lhs.text() == rhs.text(),
        Kind::ThisKeyword => true,
        Kind::JsxNamespacedName => {
            lhs.as_jsx_namespaced_name().namespace.text()
                == rhs.as_jsx_namespaced_name().namespace.text()
                && lhs.name().unwrap().text() == rhs.name().unwrap().text()
        }
        Kind::PropertyAccessExpression => {
            lhs.name().unwrap().text() == rhs.name().unwrap().text()
                && tag_names_are_equivalent(lhs.expression().unwrap(), rhs.expression().unwrap())
        }
        _ => panic!("Unhandled case in TagNamesAreEquivalent"),
    }
}

pub fn is_tag_name(node: P<Node>) -> bool {
    node.parent().is_some_and(|parent| is_jsdoc_tag(parent) && parent.tag_name() == node)
}

// We want to store any numbers/strings if they were a name that could be
// related to a declaration.  So, if we have 'import x = require("something")'
// then we want 'something' to be in the name table.  Similarly, if we have
// "a['propname']" then we want to store "propname" in the name table.
pub(crate) fn literal_is_name(node: P<Node>) -> bool {
    is_declaration_name(node)
        || node.parent().unwrap().kind() == Kind::ExternalModuleReference
        || is_argument_of_element_access_expression(node)
        || is_literal_computed_property_declaration_name(node)
}

fn is_argument_of_element_access_expression(node: impl Into<Option<P<Node>>>) -> bool {
    let Some(node) = node.into() else {
        return false;
    };
    node.parent().is_some_and(|parent| {
        parent.kind() == Kind::ElementAccessExpression
            && parent.as_element_access_expression().argument_expression == node
    })
}

// If the given node is part of a subtree of JSDoc nodes that have been cloned into a reparsed construct,
// return the corresponding reparsed clone in the subtree. Otherwise, just return the node.
pub fn get_reparsed_node_for_node(node: impl Into<Option<P<Node>>>) -> Option<P<Node>> {
    let node = node.into()?;
    if node.flags().intersects(NodeFlags::JSDoc)
        && !node.flags().intersects(NodeFlags::Reparsed)
        && let Some(file) = get_source_file_of_node(node)
    {
        let reparsed_clones = file.reparsed_clones();
        if !reparsed_clones.is_empty() {
            let (mut pos, found) =
                binary_search_func(reparsed_clones, node, compare_node_positions);
            if !found && pos > 0 {
                pos -= 1;
            }
            let candidate = reparsed_clones[pos];
            if node.loc().contained_by(candidate.loc())
                && let Some(reparsed) = find_clone_in_node(candidate, node)
            {
                return Some(reparsed);
            }
        }
    }
    Some(node)
}

fn find_clone_in_node(mut node: P<Node>, original: P<Node>) -> Option<P<Node>> {
    loop {
        if node.kind() == original.kind() && node.loc() == original.loc() {
            return Some(node);
        }
        let mut next = None;
        let found_containing_child = node.for_each_child(&mut |n| {
            if original.loc().contained_by(n.loc()) {
                next = Some(n);
                return true;
            }
            false
        });
        if !found_containing_child {
            return None;
        }
        node = next.unwrap();
    }
}

pub fn is_expando_property_declaration(node: impl Into<Option<P<Node>>>) -> bool {
    node.into().is_some_and(is_binary_expression)
}

// IsSuperProperty checks if a node is super.x or super[x].
pub fn is_super_property(node: P<Node>) -> bool {
    (is_property_access_expression(node) || is_element_access_expression(node))
        && node.expression().unwrap().kind() == Kind::SuperKeyword
}

// Indicates whether a node is a potential source of an assigned name for a class, function, or arrow function.
pub fn is_named_evaluation_source(node: P<Node>) -> bool {
    match node.kind() {
        Kind::PropertyAssignment => !is_proto_setter(node.name().unwrap()),
        Kind::ShorthandPropertyAssignment => {
            node.as_shorthand_property_assignment().object_assignment_initializer().is_some()
        }
        Kind::VariableDeclaration => {
            is_identifier(node.name().unwrap()) && node.initializer().is_some()
        }
        Kind::Parameter => {
            is_identifier(node.name().unwrap())
                && node.initializer().is_some()
                && node.as_parameter_declaration().dot_dot_dot_token().is_none()
        }
        Kind::BindingElement => {
            is_identifier(node.name().unwrap())
                && node.initializer().is_some()
                && node.as_binding_element().dot_dot_dot_token().is_none()
        }
        Kind::PropertyDeclaration => node.initializer().is_some(),
        Kind::BinaryExpression => match node.as_binary_expression().operator_token.kind() {
            Kind::EqualsToken
            | Kind::AmpersandAmpersandEqualsToken
            | Kind::BarBarEqualsToken
            | Kind::QuestionQuestionEqualsToken => is_identifier(node.as_binary_expression().left),
            _ => false,
        },
        Kind::ExportAssignment => true,
        _ => false,
    }
}

// Indicates whether a property name is the special `__proto__` property.
// Per the ECMA-262 spec, this only matters for property assignments whose name is
// the Identifier `__proto__`, or the string literal `"__proto__"`, but not for
// computed property names.
fn is_proto_setter(node: P<Node>) -> bool {
    (is_identifier(node) || is_string_literal(node)) && node.text() == "__proto__"
}

// Go `res := node.Clone(f); res.Kind = ast.KindImportDeclaration` (declarations transformTopLevelDeclaration, for a
// JSImportDeclaration): `Node::kind` is immutable in Rust, so the clone is created with the target kind directly.
pub fn clone_as_import_declaration(node: P<Node>, f: &NodeFactory) -> P<Node> {
    let d = node.as_import_declaration();
    let updated = f.new_import_declaration(
        d.modifiers(),
        d.import_clause(),
        d.module_specifier(),
        d.attributes(),
    );
    crate::ast::clone_node(updated, node, &f.hooks)
}

// utilities.go:3028
pub fn is_contextual_keyword(token: Kind) -> bool {
    Kind::FIRST_CONTEXTUAL_KEYWORD <= token && token <= Kind::LAST_CONTEXTUAL_KEYWORD
}

// utilities.go:4170
pub fn is_non_contextual_keyword(token: Kind) -> bool {
    is_keyword(token) && !is_contextual_keyword(token)
}

// utilities.go:1703
pub fn is_external_module_indicator(node: P<Node>) -> bool {
    // Exported top-level member indicates moduleness
    is_any_import_or_re_export(node)
        || is_export_assignment(node)
        || has_syntactic_modifier(node, ModifierFlags::Export)
}

// Language-service-only utilities (added for tsrs_ls; the batch port skipped them).

// utilities.go:687
pub fn is_statement_but_not_declaration(node: P<Node>) -> bool {
    is_statement_kind_but_not_declaration_kind(node.kind())
}

// utilities.go:2201
pub fn is_non_whitespace_token(node: P<Node>) -> bool {
    is_token_kind(node.kind()) && !is_whitespace_only_jsx_text(node)
}

// utilities.go:2205
pub fn is_whitespace_only_jsx_text(node: P<Node>) -> bool {
    node.kind() == Kind::JsxText && node.as_jsx_text().contains_only_trivia_white_spaces
}

// utilities.go:3056
pub fn for_each_child_and_jsdoc(
    node: P<Node>,
    source_file: &'static SourceFile,
    v: &mut dyn FnMut(P<Node>) -> bool,
) -> bool {
    if node.jsdoc(Some(source_file)).iter().any(|&jsdoc| v(jsdoc)) {
        return true;
    }
    node.for_each_child(v)
}

// utilities.go:3144
pub fn is_jsdoc_single_comment_node_list(node_list: Option<P<NodeList>>) -> bool {
    let Some(node_list) = node_list else {
        return false;
    };
    if node_list.nodes().is_empty() {
        return false;
    }
    let Some(parent) = node_list.nodes()[0].parent() else {
        return false;
    };
    is_jsdoc_single_comment_node(parent) && Some(node_list) == parent.comment_list()
}

// utilities.go:3156
pub fn is_jsdoc_single_comment_node_comment(node: Option<P<Node>>) -> bool {
    let Some(node) = node else {
        return false;
    };
    let Some(parent) = node.parent() else {
        return false;
    };
    is_jsdoc_single_comment_node(parent) && node == parent.comment_list().unwrap().nodes()[0]
}

// utilities.go:3165
pub fn is_jsdoc_single_comment_node(node: P<Node>) -> bool {
    has_comment(node.kind()) && node.comment_list().is_some_and(|l| l.nodes().len() == 1)
}

// utilities.go:3806
pub fn is_trivia(token: Kind) -> bool {
    Kind::FIRST_TRIVIA_TOKEN <= token && token <= Kind::LAST_TRIVIA_TOKEN
}

// utilities.go:3848
fn has_comment(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::JSDoc
            | Kind::JSDocUnknownTag
            | Kind::JSDocAugmentsTag
            | Kind::JSDocImplementsTag
            | Kind::JSDocDeprecatedTag
            | Kind::JSDocPublicTag
            | Kind::JSDocPrivateTag
            | Kind::JSDocProtectedTag
            | Kind::JSDocReadonlyTag
            | Kind::JSDocOverrideTag
            | Kind::JSDocCallbackTag
            | Kind::JSDocOverloadTag
            | Kind::JSDocParameterTag
            | Kind::JSDocPropertyTag
            | Kind::JSDocReturnTag
            | Kind::JSDocThisTag
            | Kind::JSDocTypeTag
            | Kind::JSDocTemplateTag
            | Kind::JSDocTypedefTag
            | Kind::JSDocSeeTag
            | Kind::JSDocThrowsTag
            | Kind::JSDocSatisfiesTag
            | Kind::JSDocImportTag
    )
}

// Used by the checker's language-service API (services.go).
// utilities.go:3017
pub fn is_call_like_or_function_like_expression(node: P<Node>) -> bool {
    is_call_like_expression(node) || is_function_expression_or_arrow_function(node)
}

// utilities.go:3063
pub fn has_type_arguments(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::CallExpression
            | Kind::NewExpression
            | Kind::TaggedTemplateExpression
            | Kind::TypeReference
            | Kind::ExpressionWithTypeArguments
            | Kind::ImportType
            | Kind::TypeQuery
            | Kind::JsxOpeningElement
            | Kind::JsxSelfClosingElement
    )
}

// Used by tsrs_ls (lscore).
// utilities.go:1240
pub fn is_deprecated_declaration(declaration: P<Node>) -> bool {
    is_deprecated_declaration_with_cached_flags(declaration, get_combined_node_flags(declaration))
}

// utilities.go:3043
pub fn is_let(node: P<Node>) -> bool {
    get_combined_node_flags(node) & NodeFlags::BlockScoped == NodeFlags::Let
}

// utilities.go:3086
pub fn has_initializer(node: P<Node>) -> bool {
    match node.kind() {
        Kind::VariableDeclaration
        | Kind::Parameter
        | Kind::BindingElement
        | Kind::PropertyDeclaration
        | Kind::PropertyAssignment
        | Kind::EnumMember
        | Kind::ForStatement
        | Kind::ForInStatement
        | Kind::ForOfStatement
        | Kind::JsxAttribute => node.initializer().is_some(),
        _ => false,
    }
}

// utilities.go:3274
pub fn is_string_text_containing_node(node: P<Node>) -> bool {
    node.kind() == Kind::StringLiteral || is_template_literal_kind(node.kind())
}

// utilities.go:3646
pub fn is_right_side_of_property_access(node: P<Node>) -> bool {
    let parent = node.parent().unwrap();
    parent.kind() == Kind::PropertyAccessExpression && parent.name() == Some(node)
}

// utilities.go:1864
pub fn get_super_container(node: P<Node>, stop_on_functions: bool) -> Option<P<Node>> {
    let mut node = node;
    loop {
        node = node.parent()?;
        match node.kind() {
            Kind::ComputedPropertyName => {
                node = node.parent().unwrap();
            }
            Kind::FunctionDeclaration | Kind::FunctionExpression | Kind::ArrowFunction => {
                if !stop_on_functions {
                    continue;
                }
                return Some(node);
            }
            Kind::PropertyDeclaration
            | Kind::PropertySignature
            | Kind::MethodDeclaration
            | Kind::MethodSignature
            | Kind::Constructor
            | Kind::GetAccessor
            | Kind::SetAccessor
            | Kind::ClassStaticBlockDeclaration => {
                return Some(node);
            }
            Kind::Decorator => {
                // Decorators are always applied outside of the body of a class or method.
                let parent = node.parent().unwrap();
                if parent.kind() == Kind::Parameter && is_class_element(parent.parent().unwrap()) {
                    // If the decorator's parent is a ParameterDeclaration, we resolve the this container from
                    // the grandparent class declaration.
                    node = parent.parent().unwrap();
                } else if is_class_element(parent) {
                    // If the decorator's parent is a class element, we resolve the 'this' container
                    // from the parent class declaration.
                    node = parent;
                }
            }
            _ => {}
        }
    }
}

// utilities.go:2592
pub fn is_default_import(
    node: P<Node>, /*ImportDeclaration | ImportEqualsDeclaration | ExportDeclaration*/
) -> bool {
    match node.kind() {
        Kind::ImportDeclaration | Kind::JSImportDeclaration => {
            let import_clause = node.import_clause();
            import_clause
                .is_some_and(|import_clause| import_clause.as_import_clause().name.is_some())
        }
        _ => false,
    }
}

// utilities.go:3650
pub fn is_argument_expression_of_element_access(node: P<Node>) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == Kind::ElementAccessExpression
            && parent.as_element_access_expression().argument_expression == node
    })
}

// utilities.go:3654
pub fn climb_past_property_access(node: P<Node>) -> P<Node> {
    if is_right_side_of_property_access(node) {
        return node.parent().unwrap();
    }
    node
}

// utilities.go:3661
fn climb_past_property_or_element_access(node: P<Node>) -> P<Node> {
    if is_right_side_of_property_access(node) || is_argument_expression_of_element_access(node) {
        return node.parent().unwrap();
    }
    node
}

// utilities.go:3668
fn select_expression_of_call_or_new_expression_or_decorator(node: P<Node>) -> Option<P<Node>> {
    if is_call_expression(node) || is_new_expression(node) || is_decorator(node) {
        return node.expression();
    }
    None
}

// utilities.go:3675
fn select_tag_of_tagged_template_expression(node: P<Node>) -> Option<P<Node>> {
    if is_tagged_template_expression(node) {
        return Some(node.as_tagged_template_expression().tag);
    }
    None
}

// utilities.go:3682
fn select_tag_name_of_jsx_opening_like_element(node: P<Node>) -> Option<P<Node>> {
    if is_jsx_opening_element(node) || is_jsx_self_closing_element(node) {
        return Some(node.tag_name());
    }
    None
}

// utilities.go:3689
pub fn is_call_expression_target(
    node: P<Node>,
    include_element_access: bool,
    skip_past_outer_expressions: bool,
) -> bool {
    is_callee_worker(
        node,
        is_call_expression,
        select_expression_of_call_or_new_expression_or_decorator,
        include_element_access,
        skip_past_outer_expressions,
    )
}

// utilities.go:3693
pub fn is_new_expression_target(
    node: P<Node>,
    include_element_access: bool,
    skip_past_outer_expressions: bool,
) -> bool {
    is_callee_worker(
        node,
        is_new_expression,
        select_expression_of_call_or_new_expression_or_decorator,
        include_element_access,
        skip_past_outer_expressions,
    )
}

// utilities.go:3697
pub fn is_call_or_new_expression_target(
    node: P<Node>,
    include_element_access: bool,
    skip_past_outer_expressions: bool,
) -> bool {
    is_callee_worker(
        node,
        is_call_or_new_expression,
        select_expression_of_call_or_new_expression_or_decorator,
        include_element_access,
        skip_past_outer_expressions,
    )
}

// utilities.go:3701
pub fn is_tagged_template_tag(
    node: P<Node>,
    include_element_access: bool,
    skip_past_outer_expressions: bool,
) -> bool {
    is_callee_worker(
        node,
        is_tagged_template_expression,
        select_tag_of_tagged_template_expression,
        include_element_access,
        skip_past_outer_expressions,
    )
}

// utilities.go:3705
pub fn is_decorator_target(
    node: P<Node>,
    include_element_access: bool,
    skip_past_outer_expressions: bool,
) -> bool {
    is_callee_worker(
        node,
        is_decorator,
        select_expression_of_call_or_new_expression_or_decorator,
        include_element_access,
        skip_past_outer_expressions,
    )
}

// utilities.go:3709
pub fn is_jsx_opening_like_element_tag_name(
    node: P<Node>,
    include_element_access: bool,
    skip_past_outer_expressions: bool,
) -> bool {
    is_callee_worker(
        node,
        is_jsx_opening_like_element,
        select_tag_name_of_jsx_opening_like_element,
        include_element_access,
        skip_past_outer_expressions,
    )
}

// utilities.go:3713
fn is_callee_worker(
    node: P<Node>,
    pred: fn(P<Node>) -> bool,
    callee_selector: fn(P<Node>) -> Option<P<Node>>,
    include_element_access: bool,
    skip_past_outer_expressions: bool,
) -> bool {
    let mut target = if include_element_access {
        climb_past_property_or_element_access(node)
    } else {
        climb_past_property_access(node)
    };
    if skip_past_outer_expressions {
        // Only skip outer expressions if the target is actually an expression node
        if is_expression(target) {
            target = skip_outer_expressions(target, OuterExpressionKinds::All);
        }
    }
    target.parent().is_some_and(|parent| pred(parent) && callee_selector(parent) == Some(target))
}

// utilities.go:4195
pub fn import_from_module_specifier(node: P<Node>) -> P<Node> {
    if let Some(result) = try_get_import_from_module_specifier(node) {
        return result;
    }
    panic!("Unexpected node kind: {:?}", node.parent().unwrap().kind());
}

// utilities.go:4203
pub fn try_get_import_from_module_specifier(
    node: P<Node>, /*StringLiteralLike*/
) -> Option<P<Node>> {
    let parent = node.parent().unwrap();
    match parent.kind() {
        Kind::ImportDeclaration | Kind::JSImportDeclaration | Kind::ExportDeclaration => {
            Some(parent)
        }
        Kind::ExternalModuleReference => parent.parent(),
        Kind::CallExpression => {
            if is_import_call(parent)
                || is_require_call(parent, false /*requireStringLiteralLikeArgument*/)
            {
                return Some(parent);
            }
            None
        }
        Kind::LiteralType => {
            if !is_string_literal(node) {
                return None;
            }
            if is_import_type_node(parent.parent().unwrap()) {
                return parent.parent();
            }
            None
        }
        _ => None,
    }
}

// utilities.go:3021
pub fn node_has_kind(node: Option<P<Node>>, kind: Kind) -> bool {
    let Some(node) = node else {
        return false;
    };
    node.kind() == kind
}

// utilities.go:3106
pub fn get_type_annotation_node(node: P<Node>) -> Option<P<Node>> {
    match node.kind() {
        Kind::VariableDeclaration
        | Kind::Parameter
        | Kind::PropertySignature
        | Kind::PropertyDeclaration
        | Kind::TypePredicate
        | Kind::ParenthesizedType
        | Kind::TypeOperator
        | Kind::MappedType
        | Kind::TypeAssertionExpression
        | Kind::AsExpression
        | Kind::SatisfiesExpression
        | Kind::TypeAliasDeclaration
        | Kind::JSTypeAliasDeclaration
        | Kind::NamedTupleMember
        | Kind::OptionalType
        | Kind::RestType
        | Kind::TemplateLiteralTypeSpan
        | Kind::JSDocTypeExpression
        | Kind::JSDocParameterTag
        | Kind::JSDocPropertyTag
        | Kind::JSDocNullableType
        | Kind::JSDocNonNullableType
        | Kind::JSDocOptionalType => node.type_node(),
        _ => {
            let func_like = node.function_like_data();
            if let Some(func_like) = func_like {
                return func_like.type_();
            }
            None
        }
    }
}

// utilities.go:3123
pub fn is_object_type_declaration(node: P<Node>) -> bool {
    is_class_like(node) || is_interface_declaration(node) || is_type_literal_node(node)
}

// utilities.go:3127
pub fn is_class_or_type_element(node: P<Node>) -> bool {
    is_class_element(node) || is_type_element(node)
}

// utilities.go:3139
pub fn is_type_keyword_token(node: P<Node>) -> bool {
    node.kind() == Kind::TypeKeyword
}

// utilities.go:3282
pub fn is_template_literal_token(node: P<Node>) -> bool {
    is_template_literal_kind(node.kind())
}

// ast.go:3092
pub fn get_declaration_name(declaration: P<Node>) -> String {
    let name = get_non_assigned_name_of_declaration(declaration);
    if let Some(name) = name {
        if is_computed_property_name(name) {
            if is_string_or_numeric_literal_like(name.expression().unwrap()) {
                return name.expression().unwrap().text().to_string();
            }
            if is_property_access_expression(name.expression().unwrap()) {
                return name.expression().unwrap().name().unwrap().text().to_string();
            }
        } else if is_property_name(name) {
            return name.text().to_string();
        }
    }
    String::new()
}

// utilities.go:3961 (added for emit)
pub fn try_get_property_name_of_binding_or_assignment_element(
    binding_element: P<Node>,
) -> Option<P<Node>> {
    match binding_element.kind() {
        Kind::BindingElement => {
            // `a` in `let { a: b } = ...`
            // `[a]` in `let { [a]: b } = ...`
            // `"a"` in `let { "a": b } = ...`
            // `1` in `let { 1: b } = ...`
            if let Some(property_name) = binding_element.property_name() {
                if is_computed_property_name(property_name)
                    && is_string_or_numeric_literal_like(property_name.expression().unwrap())
                {
                    return property_name.expression();
                }
                return Some(property_name);
            }
        }
        Kind::PropertyAssignment => {
            // `a` in `({ a: b } = ...)`
            // `[a]` in `({ [a]: b } = ...)`
            // `"a"` in `({ "a": b } = ...)`
            // `1` in `({ 1: b } = ...)`
            if let Some(property_name) = binding_element.name() {
                if is_computed_property_name(property_name)
                    && is_string_or_numeric_literal_like(property_name.expression().unwrap())
                {
                    return property_name.expression();
                }
                return Some(property_name);
            }
        }
        Kind::SpreadAssignment => {
            // `a` in `({ ...a } = ...)`
            return binding_element.name();
        }
        _ => {}
    }

    let target = get_target_of_binding_or_assignment_element(binding_element);
    if let Some(target) = target
        && is_property_name(target)
    {
        return Some(target);
    }
    None
}

// utilities.go:1708 (added for emit)
pub fn is_export_namespace_as_default_declaration(node: P<Node>) -> bool {
    if is_export_declaration(node) {
        let export_clause = node.as_export_declaration().export_clause.unwrap();
        return is_namespace_export(export_clause)
            && module_export_name_is_default(export_clause.name().unwrap());
    }
    false
}

// utilities.go:4046 (added for emit)
pub fn get_rest_indicator_of_binding_or_assignment_element(
    binding_element: P<Node>,
) -> Option<P<Node>> {
    match binding_element.kind() {
        Kind::Parameter => binding_element.as_parameter_declaration().dot_dot_dot_token(),
        Kind::BindingElement => binding_element.as_binding_element().dot_dot_dot_token(),
        Kind::SpreadElement | Kind::SpreadAssignment => Some(binding_element),
        _ => None,
    }
}
