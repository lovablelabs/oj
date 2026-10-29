use std::collections::hash_map::Entry;
use std::sync::LazyLock;

use rustc_hash::FxHashMap;
use tsrs_core::tspath;
use tsrs_core::{CompilerOptions, JsxEmit, ModuleKind, P, ResolutionMode, ScriptKind, Tristate};

use crate::*;

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum JSDeclarationKind {
    #[default]
    None,
    // module.exports = expr, except for module.exports = exports
    ModuleExports,
    // exports.name = expr
    // module.exports.name = expr
    ExportsProperty,
    // this.name = expr
    ThisProperty,
    // F.name = expr, F[name] = expr, in JS or TS file
    Property,
    // Object.defineProperty(x, 'name', { value: any, writable?: boolean (false by default) });
    // Object.defineProperty(x, 'name', { get: Function, set: Function });
    // Object.defineProperty(x, 'name', { get: Function });
    // Object.defineProperty(x, 'name', { set: Function });
    ObjectDefinePropertyValue,
    // Object.defineProperty(exports || module.exports, 'name', ...);
    ObjectDefinePropertyExports,
}

pub fn get_assignment_declaration_kind(node: P<Node>) -> JSDeclarationKind {
    match node.kind() {
        Kind::BinaryExpression => {
            let bin = node.as_binary_expression();
            if bin.operator_token.kind() == Kind::EqualsToken && is_access_expression(bin.left) {
                if is_in_js_file(bin.left) {
                    if is_module_exports_access_expression(bin.left)
                        && !is_exports_identifier(bin.right())
                    {
                        return JSDeclarationKind::ModuleExports;
                    }
                    let left_expression = bin.left.expression().unwrap();
                    if (is_module_exports_access_expression(left_expression)
                        || is_exports_identifier(left_expression))
                        && get_element_or_property_access_name(bin.left).is_some()
                    {
                        return JSDeclarationKind::ExportsProperty;
                    }
                    if left_expression.kind() == Kind::ThisKeyword {
                        return JSDeclarationKind::ThisProperty;
                    }
                }
                if bin.left.kind() == Kind::PropertyAccessExpression
                    && is_entity_name_expression_ex(
                        bin.left.expression().unwrap(),
                        is_in_js_file(bin.left),
                    )
                    && is_identifier(bin.left.name().unwrap())
                    || bin.left.kind() == Kind::ElementAccessExpression
                        && is_entity_name_expression_ex(
                            bin.left.expression().unwrap(),
                            is_in_js_file(bin.left),
                        )
                {
                    return JSDeclarationKind::Property;
                }
            }
        }
        Kind::CallExpression
            if is_in_js_file(node) && is_bindable_object_define_property_call(node) =>
        {
            let entity_name = node.arguments()[0];
            if is_exports_identifier(entity_name)
                || is_module_exports_access_expression(entity_name)
            {
                return JSDeclarationKind::ObjectDefinePropertyExports;
            }
            return JSDeclarationKind::ObjectDefinePropertyValue;
        }
        _ => {}
    }
    JSDeclarationKind::None
}

pub fn is_bindable_object_define_property_call(node: P<Node>) -> bool {
    let args = node.arguments();
    if args.len() == 3 {
        let expr = node.expression().unwrap();
        if is_property_access_expression(expr)
            && is_identifier(expr.expression().unwrap())
            && expr.expression().unwrap().text() == "Object"
            && expr.name().unwrap().text() == "defineProperty"
            && is_string_or_numeric_literal_like(args[1])
            && is_bindable_static_name_expression(args[0], true /*excludeThisKeyword*/)
        {
            return true;
        }
    }
    false
}

/**
 * A declaration has a dynamic name if all of the following are true:
 *   1. The declaration has a computed property name.
 *   2. The computed name is *not* expressed as a `StringLiteral`.
 *   3. The computed name is *not* expressed as a `NumericLiteral`.
 *   4. The computed name is *not* expressed as a `PlusToken` or `MinusToken`
 *      immediately followed by a `NumericLiteral`.
 */
pub fn has_dynamic_name(declaration: P<Node>) -> bool {
    get_name_of_declaration(declaration).is_some_and(is_dynamic_name)
}

pub fn is_dynamic_name(name: P<Node>) -> bool {
    let expr = match name.kind() {
        Kind::ComputedPropertyName => name.expression().unwrap(),
        Kind::ElementAccessExpression => {
            skip_parentheses(name.as_element_access_expression().argument_expression)
        }
        _ => return false,
    };
    !is_string_or_numeric_literal_like(expr) && !is_signed_numeric_literal(expr)
}

pub fn is_entity_name_expression(node: P<Node>) -> bool {
    is_entity_name_expression_ex(node, false /*allowJS*/)
}

fn is_entity_name_expression_ex(node: P<Node>, allow_js: bool) -> bool {
    is_identifier(node)
        || is_property_access_entity_name_expression(node, allow_js)
        || allow_js
            && (node.kind() == Kind::ThisKeyword
                || is_element_access_entity_name_expression(node, allow_js))
}

pub fn is_property_access_entity_name_expression(node: P<Node>, allow_js: bool) -> bool {
    is_property_access_expression(node)
        && is_identifier(node.name().unwrap())
        && is_entity_name_expression_ex(node.expression().unwrap(), allow_js)
}

fn is_element_access_entity_name_expression(node: P<Node>, allow_js: bool) -> bool {
    is_element_access_expression(node)
        && is_string_or_numeric_literal_like(
            node.as_element_access_expression().argument_expression,
        )
        && is_entity_name_expression_ex(node.expression().unwrap(), allow_js)
}

pub fn is_dotted_name(node: P<Node>) -> bool {
    match node.kind() {
        Kind::Identifier | Kind::ThisKeyword | Kind::SuperKeyword | Kind::MetaProperty => true,
        Kind::PropertyAccessExpression | Kind::ParenthesizedExpression => {
            is_dotted_name(node.expression().unwrap())
        }
        _ => false,
    }
}

pub fn has_same_property_access_name(node1: P<Node>, node2: P<Node>) -> bool {
    if node1.kind() == Kind::Identifier && node2.kind() == Kind::Identifier {
        return node1.text() == node2.text();
    } else if node1.kind() == Kind::PropertyAccessExpression
        && node2.kind() == Kind::PropertyAccessExpression
    {
        return node1.name().unwrap().text() == node2.name().unwrap().text()
            && has_same_property_access_name(
                node1.expression().unwrap(),
                node2.expression().unwrap(),
            );
    }
    false
}

pub fn is_ambient_module(node: P<Node>) -> bool {
    is_module_declaration(node)
        && (node.name().unwrap().kind() == Kind::StringLiteral
            || is_global_scope_augmentation(node))
}

pub fn is_ambient_module_symbol_name(s: &str) -> bool {
    try_get_ambient_module_name_from_symbol_name(s).is_some()
}

// Ambient module symbols are either of the form `"modulename"` or `InternalSymbolNamePrefix + "\"modulename\"pattern@nodeId"`;
// see `getDeclarationName`.
pub fn try_get_ambient_module_name_from_symbol_name(s: &str) -> Option<&str> {
    if let Some(name) = s.strip_circumfix('"', '"') {
        return Some(name);
    }
    let pattern_prefix = format!("{INTERNAL_SYMBOL_NAME_PREFIX}\"");
    let rest = s.strip_prefix(pattern_prefix.as_str())?;
    let marker_index = rest.rfind("\"pattern@")?;
    if marker_index < 1 {
        return None;
    }
    Some(&rest[..marker_index])
}

pub fn is_external_module(file: P<SourceFile>) -> bool {
    file.external_module_indicator().is_some()
}

pub fn is_external_or_common_js_module(file: P<SourceFile>) -> bool {
    file.external_module_indicator().is_some() || file.common_js_module_indicator().is_some()
}

// TODO: Should we deprecate `IsExternalOrCommonJSModule` in favor of this function?
pub fn is_effective_external_module(
    node: P<SourceFile>,
    compiler_options: &CompilerOptions,
) -> bool {
    is_external_module(node)
        || (is_common_js_containing_module_kind(compiler_options.get_emit_module_kind())
            && node.common_js_module_indicator().is_some())
}

fn is_common_js_containing_module_kind(kind: ModuleKind) -> bool {
    kind == ModuleKind::COMMON_JS || ModuleKind::NODE16 <= kind && kind <= ModuleKind::NODE_NEXT
}

pub fn is_global_scope_augmentation(node: P<Node>) -> bool {
    is_module_declaration(node) && node.as_module_declaration().keyword == Kind::GlobalKeyword
}

pub fn is_module_augmentation_external(node: P<Node>) -> bool {
    // external module augmentation is a ambient module declaration that is either:
    // - defined in the top level scope and source file is an external module
    // - defined inside ambient module declaration located in the top level scope and source file not an external module
    let parent = node.parent().unwrap();
    match parent.kind() {
        Kind::SourceFile => is_external_module(parent.as_source_file_p()),
        Kind::ModuleBlock => {
            let grand_parent = parent.parent().unwrap();
            is_ambient_module(grand_parent)
                && is_source_file(grand_parent.parent().unwrap())
                && !is_external_module(grand_parent.parent().unwrap().as_source_file_p())
        }
        _ => false,
    }
}

pub fn is_module_with_string_literal_name(node: P<Node>) -> bool {
    is_module_declaration(node) && node.name().unwrap().kind() == Kind::StringLiteral
}

pub fn get_containing_class(node: P<Node>) -> Option<P<Node>> {
    find_ancestor(node.parent(), is_class_like)
}

pub fn get_extends_heritage_clause_elements(node: P<Node>) -> &'static [P<Node>] {
    get_heritage_elements(node, Kind::ExtendsKeyword)
}

pub fn get_implements_heritage_clause_elements(node: P<Node>) -> &'static [P<Node>] {
    get_heritage_elements(node, Kind::ImplementsKeyword)
}

pub fn get_heritage_elements(node: P<Node>, kind: Kind) -> &'static [P<Node>] {
    if let Some(clause) = get_heritage_clause(node, kind) {
        return clause.as_heritage_clause().types().nodes();
    }
    &[]
}

// GetHeritageClauseElementName returns the expression or type name of a heritage clause element.
pub fn get_heritage_clause_element_name(node: P<Node>) -> P<Node> {
    if is_type_reference_node(node) {
        return node.as_type_reference_node().type_name;
    }
    node.as_expression_with_type_arguments().expression
}

pub fn is_name_of_heritage_clause_type_reference(mut node: P<Node>) -> bool {
    while is_qualified_name(node.parent().unwrap()) {
        node = node.parent().unwrap();
    }
    let parent = node.parent().unwrap();
    is_type_reference_node(parent)
        && parent.as_type_reference_node().type_name == node
        && is_heritage_clause(parent.parent().unwrap())
}

pub fn get_heritage_clause(node: P<Node>, kind: Kind) -> Option<P<Node>> {
    if let Some(clauses) = get_heritage_clauses(node)
        && let Some(result) = clauses.nodes().iter().find_map(|&clause| {
            if clause.as_heritage_clause().token == kind { Some(Some(clause)) } else { None }
        })
    {
        return result;
    }
    None
}

fn get_heritage_clauses(node: P<Node>) -> Option<P<NodeList>> {
    match node.kind() {
        Kind::ClassDeclaration => node.as_class_declaration().heritage_clauses(),
        Kind::ClassExpression => node.as_class_expression().heritage_clauses(),
        Kind::InterfaceDeclaration => node.as_interface_declaration().heritage_clauses(),
        _ => None,
    }
}

pub fn is_part_of_type_query(mut node: P<Node>) -> bool {
    while node.kind() == Kind::QualifiedName || node.kind() == Kind::Identifier {
        node = node.parent().unwrap();
    }
    node.kind() == Kind::TypeQuery
}

/**
 * This function returns true if the this node's root declaration is a parameter.
 * For example, passing a `ParameterDeclaration` will return true, as will passing a
 * binding element that is a child of a `ParameterDeclaration`.
 *
 * If you are looking to test that a `Node` is a `ParameterDeclaration`, use `isParameter`.
 */
pub fn is_part_of_parameter_declaration(node: P<Node>) -> bool {
    get_root_declaration(node).kind() == Kind::Parameter
}

pub fn is_in_top_level_context(mut node: P<Node>) -> bool {
    // The name of a class or function declaration is a BindingIdentifier in its surrounding scope.
    if is_identifier(node) {
        let parent = node.parent().unwrap();
        if (is_class_declaration(parent) || is_function_declaration(parent))
            && parent.name() == Some(node)
        {
            node = parent;
        }
    }
    let container = get_this_container(
        node, true,  /*includeArrowFunctions*/
        false, /*includeClassComputedPropertyName*/
    );
    is_source_file(container)
}

pub fn get_this_container(
    mut node: P<Node>,
    include_arrow_functions: bool,
    include_class_computed_property_name: bool,
) -> P<Node> {
    loop {
        node = match node.parent() {
            Some(parent) => parent,
            None => panic!("nil parent in getThisContainer"),
        };
        match node.kind() {
            Kind::ComputedPropertyName => {
                if include_class_computed_property_name
                    && is_class_like(node.parent().unwrap().parent().unwrap())
                {
                    return node;
                }
                node = node.parent().unwrap().parent().unwrap();
            }
            Kind::Decorator => {
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
            Kind::ArrowFunction => {
                if include_arrow_functions {
                    return node;
                }
            }
            Kind::FunctionDeclaration
            | Kind::FunctionExpression
            | Kind::ModuleDeclaration
            | Kind::ClassStaticBlockDeclaration
            | Kind::PropertyDeclaration
            | Kind::PropertySignature
            | Kind::MethodDeclaration
            | Kind::MethodSignature
            | Kind::Constructor
            | Kind::GetAccessor
            | Kind::SetAccessor
            | Kind::CallSignature
            | Kind::ConstructSignature
            | Kind::IndexSignature
            | Kind::EnumDeclaration
            | Kind::SourceFile => {
                return node;
            }
            _ => {}
        }
    }
}

pub fn get_immediately_invoked_function_expression(func: P<Node>) -> Option<P<Node>> {
    if is_function_expression_or_arrow_function(func) {
        let mut prev = func;
        let mut parent = func.parent().unwrap();
        while is_parenthesized_expression(parent) {
            prev = parent;
            parent = parent.parent().unwrap();
        }
        if is_call_expression(parent) && parent.expression() == Some(prev) {
            return Some(parent);
        }
    }
    None
}

pub fn is_enum_const(node: P<Node>) -> bool {
    get_combined_modifier_flags(node).intersects(ModifierFlags::Const)
}

pub fn expression_is_alias(node: P<Node>) -> bool {
    is_entity_name_expression(node) || is_class_expression(node)
}

pub fn is_instance_of_expression(node: P<Node>) -> bool {
    is_binary_expression(node)
        && node.as_binary_expression().operator_token.kind() == Kind::InstanceOfKeyword
}

pub fn is_any_import_or_re_export(node: P<Node>) -> bool {
    is_import_node(node) || is_export_declaration(node)
}

fn is_import_node(node: P<Node>) -> bool {
    is_any_import_syntax(node) || node_kind_is(node, &[Kind::JSImportDeclaration])
}

// Checks if the node is a genuine import declation. In particular the re-parsed KindJSImportDeclaration
// is explicitly excluded because the callers of this function are typically not prepared to handle it properly.
// For more permissive check, use IsImportNode.
pub fn is_any_import_syntax(node: P<Node>) -> bool {
    node_kind_is(node, &[Kind::ImportDeclaration, Kind::ImportEqualsDeclaration])
}

pub fn is_json_source_file(file: P<SourceFile>) -> bool {
    file.script_kind() == ScriptKind::JSON
}

pub fn is_in_json_file(node: P<Node>) -> bool {
    node.flags().intersects(NodeFlags::JsonFile)
}

pub fn get_external_module_name(node: P<Node>) -> Option<P<Node>> {
    match node.kind() {
        Kind::ImportDeclaration | Kind::JSImportDeclaration | Kind::ExportDeclaration => {
            node.module_specifier()
        }
        Kind::ImportEqualsDeclaration => {
            let module_reference = node.as_import_equals_declaration().module_reference;
            if module_reference.kind() == Kind::ExternalModuleReference {
                return module_reference.expression();
            }
            None
        }
        Kind::ImportType => get_import_type_node_literal(node),
        Kind::CallExpression => node.arguments().first().copied(),
        Kind::ModuleDeclaration => {
            let name = node.name().unwrap();
            if is_string_literal(name) {
                return Some(name);
            }
            None
        }
        _ => panic!("Unhandled case in getExternalModuleName"),
    }
}

pub fn has_import_attributes(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::ImportDeclaration
            | Kind::JSImportDeclaration
            | Kind::ExportDeclaration
            | Kind::ImportType
    )
}

pub fn get_import_attributes(node: P<Node>) -> Option<P<Node>> {
    match node.kind() {
        Kind::ImportDeclaration | Kind::JSImportDeclaration => {
            node.as_import_declaration().attributes()
        }
        Kind::ExportDeclaration => node.as_export_declaration().attributes,
        Kind::ImportType => node.as_import_type_node().attributes,
        _ => panic!("Unhandled case in getImportAttributes: {:?}", node.kind()),
    }
}

fn get_import_type_node_literal(node: P<Node>) -> Option<P<Node>> {
    if is_import_type_node(node) {
        let import_type_node = node.as_import_type_node();
        if is_literal_type_node(import_type_node.argument) {
            let literal_type_node = import_type_node.argument.as_literal_type_node();
            if is_string_literal(literal_type_node.literal) {
                return Some(literal_type_node.literal);
            }
        }
    }
    None
}

pub fn is_expression_node(mut node: P<Node>) -> bool {
    match node.kind() {
        Kind::SuperKeyword
        | Kind::NullKeyword
        | Kind::TrueKeyword
        | Kind::FalseKeyword
        | Kind::RegularExpressionLiteral
        | Kind::ArrayLiteralExpression
        | Kind::ObjectLiteralExpression
        | Kind::PropertyAccessExpression
        | Kind::ElementAccessExpression
        | Kind::CallExpression
        | Kind::NewExpression
        | Kind::TaggedTemplateExpression
        | Kind::AsExpression
        | Kind::TypeAssertionExpression
        | Kind::SatisfiesExpression
        | Kind::NonNullExpression
        | Kind::ParenthesizedExpression
        | Kind::FunctionExpression
        | Kind::ClassExpression
        | Kind::ArrowFunction
        | Kind::VoidExpression
        | Kind::DeleteExpression
        | Kind::TypeOfExpression
        | Kind::PrefixUnaryExpression
        | Kind::PostfixUnaryExpression
        | Kind::BinaryExpression
        | Kind::ConditionalExpression
        | Kind::SpreadElement
        | Kind::TemplateExpression
        | Kind::OmittedExpression
        | Kind::JsxElement
        | Kind::JsxSelfClosingElement
        | Kind::JsxFragment
        | Kind::YieldExpression
        | Kind::AwaitExpression => true,
        Kind::MetaProperty => {
            // `import.<phase>` in `import.<phase>(...)` is not an expression
            let parent = node.parent().unwrap();
            !is_import_call(parent) || parent.expression() != Some(node)
        }
        Kind::ExpressionWithTypeArguments => !is_heritage_clause(node.parent().unwrap()),
        Kind::QualifiedName => {
            while node.parent().unwrap().kind() == Kind::QualifiedName {
                node = node.parent().unwrap();
            }
            let parent = node.parent().unwrap();
            is_type_query_node(parent)
                || is_jsdoc_link_like(parent)
                || is_jsdoc_name_reference(parent)
                || is_jsx_tag_name(node)
        }
        Kind::PrivateIdentifier => {
            let parent = node.parent().unwrap();
            is_binary_expression(parent)
                && parent.as_binary_expression().left == node
                && parent.as_binary_expression().operator_token.kind() == Kind::InKeyword
        }
        Kind::Identifier => {
            let parent = node.parent().unwrap();
            if is_type_query_node(parent)
                || is_jsdoc_link_like(parent)
                || is_jsdoc_name_reference(parent)
                || is_jsx_tag_name(node)
            {
                return true;
            }
            is_in_expression_context(node)
        }
        Kind::NumericLiteral
        | Kind::BigIntLiteral
        | Kind::StringLiteral
        | Kind::NoSubstitutionTemplateLiteral
        | Kind::ThisKeyword => is_in_expression_context(node),
        _ => false,
    }
}

pub fn is_in_expression_context(node: P<Node>) -> bool {
    let parent = node.parent().unwrap();
    match parent.kind() {
        Kind::VariableDeclaration
        | Kind::Parameter
        | Kind::PropertyDeclaration
        | Kind::PropertySignature
        | Kind::EnumMember
        | Kind::PropertyAssignment
        | Kind::BindingElement => parent.initializer() == Some(node),
        Kind::ExpressionStatement
        | Kind::IfStatement
        | Kind::DoStatement
        | Kind::WhileStatement
        | Kind::ReturnStatement
        | Kind::WithStatement
        | Kind::SwitchStatement
        | Kind::CaseClause
        | Kind::DefaultClause
        | Kind::ThrowStatement
        | Kind::TypeAssertionExpression
        | Kind::AsExpression
        | Kind::TemplateSpan
        | Kind::ComputedPropertyName
        | Kind::SatisfiesExpression => parent.expression() == Some(node),
        Kind::ForStatement => {
            let s = parent.as_for_statement();
            s.initializer == Some(node)
                && s.initializer.unwrap().kind() != Kind::VariableDeclarationList
                || s.condition == Some(node)
                || s.incrementor == Some(node)
        }
        Kind::ForInStatement | Kind::ForOfStatement => {
            let s = parent.as_for_in_or_of_statement();
            s.initializer == node && s.initializer.kind() != Kind::VariableDeclarationList
                || s.expression == node
        }
        Kind::Decorator
        | Kind::JsxExpression
        | Kind::JsxSpreadAttribute
        | Kind::SpreadAssignment => true,
        Kind::ExpressionWithTypeArguments => {
            parent.expression() == Some(node) && !is_part_of_type_node(parent)
        }
        Kind::ShorthandPropertyAssignment => {
            parent.as_shorthand_property_assignment().object_assignment_initializer() == Some(node)
        }
        Kind::FunctionExpression | Kind::ClassExpression => {
            // The name of a function or class expression is a declaration name, not an expression.
            parent.name() != Some(node)
        }
        _ => is_expression_node(parent),
    }
}

pub fn is_part_of_type_node(node: P<Node>) -> bool {
    let kind = node.kind();
    if kind >= Kind::FIRST_TYPE_NODE && kind <= Kind::LAST_TYPE_NODE {
        return true;
    }
    match node.kind() {
        Kind::AnyKeyword
        | Kind::UnknownKeyword
        | Kind::NumberKeyword
        | Kind::BigIntKeyword
        | Kind::StringKeyword
        | Kind::BooleanKeyword
        | Kind::SymbolKeyword
        | Kind::ObjectKeyword
        | Kind::UndefinedKeyword
        | Kind::NullKeyword
        | Kind::NeverKeyword => true,
        Kind::VoidKeyword => node.parent().unwrap().kind() != Kind::VoidExpression,
        Kind::ExpressionWithTypeArguments => is_part_of_type_expression_with_type_arguments(node),
        Kind::TypeParameter => {
            let parent_kind = node.parent().unwrap().kind();
            parent_kind == Kind::MappedType || parent_kind == Kind::InferType
        }
        Kind::Identifier => {
            let parent = node.parent().unwrap();
            if is_qualified_name(parent) && parent.as_qualified_name().right == node {
                return is_part_of_type_node_in_parent(parent);
            }
            if is_property_access_expression(parent) && parent.name() == Some(node) {
                return is_part_of_type_node_in_parent(parent);
            }
            is_part_of_type_node_in_parent(node)
        }
        Kind::QualifiedName | Kind::PropertyAccessExpression | Kind::ThisKeyword => {
            is_part_of_type_node_in_parent(node)
        }
        _ => false,
    }
}

fn is_part_of_type_node_in_parent(node: P<Node>) -> bool {
    let parent = node.parent().unwrap();
    if parent.kind() == Kind::TypeQuery {
        return false;
    }
    if parent.kind() == Kind::ImportType {
        return !parent.as_import_type_node().is_type_of;
    }

    // Do not recursively call isPartOfTypeNode on the parent. In the example:
    //
    //     let a: A.B.C;
    //
    // Calling isPartOfTypeNode would consider the qualified name A.B a type node.
    // Only C and A.B.C are type nodes.
    if parent.kind() >= Kind::FIRST_TYPE_NODE && parent.kind() <= Kind::LAST_TYPE_NODE {
        return true;
    }
    match parent.kind() {
        Kind::ExpressionWithTypeArguments => is_part_of_type_expression_with_type_arguments(parent),
        Kind::TypeParameter => Some(node) == parent.as_type_parameter_declaration().constraint,
        Kind::VariableDeclaration
        | Kind::Parameter
        | Kind::PropertyDeclaration
        | Kind::PropertySignature
        | Kind::FunctionDeclaration
        | Kind::FunctionExpression
        | Kind::ArrowFunction
        | Kind::Constructor
        | Kind::MethodDeclaration
        | Kind::MethodSignature
        | Kind::GetAccessor
        | Kind::SetAccessor
        | Kind::CallSignature
        | Kind::ConstructSignature
        | Kind::IndexSignature
        | Kind::TypeAssertionExpression => Some(node) == parent.type_node(),
        Kind::CallExpression | Kind::NewExpression | Kind::TaggedTemplateExpression => {
            parent.type_arguments().contains(&node)
        }
        _ => false,
    }
}

fn is_part_of_type_expression_with_type_arguments(node: P<Node>) -> bool {
    let parent = node.parent().unwrap();
    is_heritage_clause(parent)
        && (!is_class_like(parent.parent().unwrap())
            || parent.as_heritage_clause().token == Kind::ImplementsKeyword)
        || is_jsdoc_implements_tag(parent)
        || is_jsdoc_augments_tag(parent)
}

pub fn is_jsdoc_link_like(node: P<Node>) -> bool {
    node_kind_is(node, &[Kind::JSDocLink, Kind::JSDocLinkCode, Kind::JSDocLinkPlain])
}

pub fn is_jsdoc_tag(node: P<Node>) -> bool {
    node.kind() >= Kind::FIRST_JSDOC_TAG_NODE && node.kind() <= Kind::LAST_JSDOC_TAG_NODE
}

pub fn is_import_call(node: P<Node>) -> bool {
    if !is_call_expression(node) {
        return false;
    }
    let e = node.expression().unwrap();
    e.kind() == Kind::ImportKeyword || is_import_phase_meta_property(e)
}

pub fn is_import_phase_meta_property(node: P<Node>) -> bool {
    is_import_defer_meta_property(node) || is_import_source_meta_property(node)
}

pub fn is_import_defer_meta_property(node: P<Node>) -> bool {
    is_import_meta_property(node, "defer")
}

pub fn is_import_source_meta_property(node: P<Node>) -> bool {
    is_import_meta_property(node, "source")
}

fn is_import_meta_property(node: P<Node>, name: &str) -> bool {
    is_meta_property(node)
        && node.as_meta_property().keyword_token == Kind::ImportKeyword
        && node.as_meta_property().name().text() == name
}

pub fn is_source_phase_import(node: P<Node>) -> bool {
    if is_import_declaration(node) {
        let clause = node.as_import_declaration().import_clause();
        return clause.is_some_and(|clause| {
            clause.as_import_clause().phase_modifier() == Kind::SourceKeyword
        });
    }
    is_source_phase_import_call(node)
}

pub fn is_source_phase_import_call(node: P<Node>) -> bool {
    is_call_expression(node) && node.expression().is_some_and(is_import_source_meta_property)
}

pub fn is_computed_non_literal_name(name: P<Node>) -> bool {
    is_computed_property_name(name)
        && !is_string_or_numeric_literal_like(name.expression().unwrap())
}

pub fn is_question_token(node: impl Into<Option<P<Node>>>) -> bool {
    match node.into() {
        Some(node) => node.kind() == Kind::QuestionToken,
        None => false,
    }
}

pub fn entity_name_to_string(
    name: P<Node>,
    get_text_of_node: Option<&dyn Fn(P<Node>) -> String>,
) -> String {
    match name.kind() {
        Kind::ThisKeyword => "this".to_string(),
        Kind::Identifier | Kind::PrivateIdentifier => match get_text_of_node {
            Some(get_text_of_node) if !node_is_synthesized(name) => get_text_of_node(name),
            _ => name.text().to_string(),
        },
        Kind::QualifiedName => {
            entity_name_to_string(name.as_qualified_name().left, get_text_of_node)
                + "."
                + &entity_name_to_string(name.as_qualified_name().right, get_text_of_node)
        }
        Kind::PropertyAccessExpression => {
            entity_name_to_string(name.expression().unwrap(), get_text_of_node)
                + "."
                + &entity_name_to_string(name.name().unwrap(), get_text_of_node)
        }
        Kind::JsxNamespacedName => {
            entity_name_to_string(name.as_jsx_namespaced_name().namespace, get_text_of_node)
                + ":"
                + &entity_name_to_string(name.name().unwrap(), get_text_of_node)
        }
        _ => panic!("Unhandled case in EntityNameToString"),
    }
}

pub fn get_text_of_property_name(name: P<Node>) -> String {
    try_get_text_of_property_name(name).unwrap_or_default()
}

pub fn try_get_text_of_property_name(name: P<Node>) -> Option<String> {
    match name.kind() {
        Kind::Identifier
        | Kind::PrivateIdentifier
        | Kind::StringLiteral
        | Kind::NumericLiteral
        | Kind::BigIntLiteral
        | Kind::NoSubstitutionTemplateLiteral => Some(name.text().to_string()),
        Kind::ComputedPropertyName => {
            let expression = name.expression().unwrap();
            if is_string_or_numeric_literal_like(expression) {
                return Some(expression.text().to_string());
            }
            None
        }
        Kind::JsxNamespacedName => Some(format!(
            "{}:{}",
            name.as_jsx_namespaced_name().namespace.text(),
            name.name().unwrap().text()
        )),
        _ => None,
    }
}

pub fn is_jsdoc_node(node: P<Node>) -> bool {
    node.kind() >= Kind::FIRST_JSDOC_NODE && node.kind() <= Kind::LAST_JSDOC_NODE
}

pub fn get_new_target_container(node: P<Node>) -> Option<P<Node>> {
    let container = get_this_container(
        node, false, /*includeArrowFunctions*/
        false, /*includeClassComputedPropertyName*/
    );
    match container.kind() {
        Kind::Constructor | Kind::FunctionDeclaration | Kind::FunctionExpression => Some(container),
        _ => None,
    }
}

pub fn get_enclosing_block_scope_container(node: P<Node>) -> Option<P<Node>> {
    find_ancestor(node.parent(), |current| is_block_scope(current, current.parent()))
}

fn is_block_scope(node: P<Node>, parent_node: Option<P<Node>>) -> bool {
    match node.kind() {
        Kind::SourceFile
        | Kind::CaseBlock
        | Kind::CatchClause
        | Kind::ModuleDeclaration
        | Kind::ForStatement
        | Kind::ForInStatement
        | Kind::ForOfStatement
        | Kind::Constructor
        | Kind::MethodDeclaration
        | Kind::GetAccessor
        | Kind::SetAccessor
        | Kind::FunctionDeclaration
        | Kind::FunctionExpression
        | Kind::ArrowFunction
        | Kind::PropertyDeclaration
        | Kind::ClassStaticBlockDeclaration => true,
        Kind::Block => {
            // function block is not considered block-scope container
            // see comment in binder.ts: bind(...), case for SyntaxKind.Block
            !is_function_like_or_class_static_block_declaration(parent_node)
        }
        _ => false,
    }
}

bitflags::bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub struct SemanticMeaning: u32 {
        const None = 0;
        const Value = 1 << 0;
        const Type = 1 << 1;
        const Namespace = 1 << 2;
        const All = Self::Value.bits() | Self::Type.bits() | Self::Namespace.bits();
    }
}

// utilities.go:2250
pub fn get_meaning_from_declaration(node: P<Node>) -> SemanticMeaning {
    match node.kind() {
        Kind::VariableDeclaration
        | Kind::Parameter
        | Kind::BindingElement
        | Kind::PropertyDeclaration
        | Kind::PropertySignature
        | Kind::PropertyAssignment
        | Kind::ShorthandPropertyAssignment
        | Kind::MethodDeclaration
        | Kind::MethodSignature
        | Kind::Constructor
        | Kind::GetAccessor
        | Kind::SetAccessor
        | Kind::FunctionDeclaration
        | Kind::FunctionExpression
        | Kind::ArrowFunction
        | Kind::CatchClause
        | Kind::JsxAttribute => SemanticMeaning::Value,
        Kind::TypeParameter
        | Kind::InterfaceDeclaration
        | Kind::TypeAliasDeclaration
        | Kind::JSTypeAliasDeclaration
        | Kind::TypeLiteral => SemanticMeaning::Type,
        Kind::EnumMember | Kind::ClassDeclaration => SemanticMeaning::Value | SemanticMeaning::Type,
        Kind::ModuleDeclaration => {
            if (is_ambient_module(node))
                || (get_module_instance_state(node) == ModuleInstanceState::Instantiated)
            {
                SemanticMeaning::Namespace | SemanticMeaning::Value
            } else {
                SemanticMeaning::Namespace
            }
        }
        // An external module can be a Value
        Kind::SourceFile => SemanticMeaning::Namespace | SemanticMeaning::Value,
        _ => SemanticMeaning::All,
    }
}

pub fn is_property_access_or_qualified_name(node: P<Node>) -> bool {
    node.kind() == Kind::PropertyAccessExpression || node.kind() == Kind::QualifiedName
}

// utilities.go:2312
pub fn is_label_name(node: P<Node>) -> bool {
    is_label_of_labeled_statement(node) || is_jump_statement_target(node)
}

fn is_label_of_labeled_statement(node: P<Node>) -> bool {
    if !is_identifier(node) {
        return false;
    }
    if !is_labeled_statement(node.parent().unwrap()) {
        return false;
    }
    Some(node) == node.parent().unwrap().label()
}

pub fn is_jump_statement_target(node: P<Node>) -> bool {
    if !is_identifier(node) {
        return false;
    }
    if !is_break_or_continue_statement(node.parent().unwrap()) {
        return false;
    }
    Some(node) == node.parent().unwrap().label()
}

pub fn is_break_or_continue_statement(node: P<Node>) -> bool {
    node.kind() == Kind::BreakStatement || node.kind() == Kind::ContinueStatement
}

// GetModuleInstanceState is used during binding as well as in transformations and tests, and therefore may be invoked
// with a node that does not yet have its `Parent` pointer set. In this case, an `ancestors` represents a stack of
// virtual `Parent` pointers that can be used to walk up the tree. Since `getModuleInstanceStateForAliasTarget` may
// potentially walk up out of the provided `Node`, merely setting the parent pointers for a given `ModuleDeclaration`
// prior to invoking `GetModuleInstanceState` is not sufficient. It is, however, necessary that the `Parent` pointers
// for all ancestors of the `Node` provided to `GetModuleInstanceState` have been set.
//
// Go passes the ancestor stack by value (append/reslice); the port copies it where Go's slice semantics would.

// Push a virtual parent pointer onto `ancestors` and return it.
fn push_ancestor(ancestors: &[P<Node>], parent: P<Node>) -> Vec<P<Node>> {
    let mut result = ancestors.to_vec();
    result.push(parent);
    result
}

// If a virtual `Parent` exists on the stack, returns the previous stack entry and the virtual `Parent“.
// Otherwise, we return `nil` and the value of `node.Parent`.
fn pop_ancestor(ancestors: &[P<Node>], node: P<Node>) -> (&[P<Node>], Option<P<Node>>) {
    if ancestors.is_empty() {
        return (&[], node.parent());
    }
    let n = ancestors.len() - 1;
    (&ancestors[..n], Some(ancestors[n]))
}

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum ModuleInstanceState {
    #[default]
    Unknown,
    NonInstantiated,
    Instantiated,
    ConstEnumOnly,
}

pub fn get_module_instance_state(node: P<Node>) -> ModuleInstanceState {
    get_module_instance_state_ex(node, &[], &mut FxHashMap::default())
}

// Go `getModuleInstanceState` (unexported); renamed to avoid clashing with the exported wrapper.
fn get_module_instance_state_ex(
    node: P<Node>,
    ancestors: &[P<Node>],
    visited: &mut FxHashMap<NodeId, ModuleInstanceState>,
) -> ModuleInstanceState {
    let module = node.as_module_declaration();
    if let Some(body) = module.body() {
        get_module_instance_state_cached(body, &push_ancestor(ancestors, node), visited)
    } else {
        ModuleInstanceState::Instantiated
    }
}

fn get_module_instance_state_cached(
    node: P<Node>,
    ancestors: &[P<Node>],
    visited: &mut FxHashMap<NodeId, ModuleInstanceState>,
) -> ModuleInstanceState {
    let node_id = get_node_id(node);
    match visited.entry(node_id) {
        Entry::Occupied(entry) => {
            let cached = *entry.get();
            if cached != ModuleInstanceState::Unknown {
                return cached;
            }
            return ModuleInstanceState::NonInstantiated;
        }
        Entry::Vacant(entry) => {
            entry.insert(ModuleInstanceState::Unknown);
        }
    }
    let result = get_module_instance_state_worker(node, ancestors, visited);
    visited.insert(node_id, result);
    result
}

fn get_module_instance_state_worker(
    node: P<Node>,
    ancestors: &[P<Node>],
    visited: &mut FxHashMap<NodeId, ModuleInstanceState>,
) -> ModuleInstanceState {
    // A module is uninstantiated if it contains only
    match node.kind() {
        Kind::InterfaceDeclaration | Kind::TypeAliasDeclaration | Kind::JSTypeAliasDeclaration => {
            return ModuleInstanceState::NonInstantiated;
        }
        Kind::EnumDeclaration => {
            if is_enum_const(node) {
                return ModuleInstanceState::ConstEnumOnly;
            }
        }
        Kind::ImportDeclaration | Kind::JSImportDeclaration | Kind::ImportEqualsDeclaration => {
            if !has_syntactic_modifier(node, ModifierFlags::Export) {
                return ModuleInstanceState::NonInstantiated;
            }
        }
        Kind::ExportDeclaration => {
            let decl = node.as_export_declaration();
            if let Some(export_clause) = decl.export_clause
                && decl.module_specifier.is_none()
                && export_clause.kind() == Kind::NamedExports
            {
                let mut state = ModuleInstanceState::NonInstantiated;
                let ancestors = push_ancestor(&push_ancestor(ancestors, node), export_clause);
                state = match export_clause.elements().iter().try_fold(
                    state,
                    |mut state, &specifier| {
                        let specifier_state = get_module_instance_state_for_alias_target(
                            specifier, &ancestors, visited,
                        );
                        if specifier_state > state {
                            state = specifier_state;
                        }
                        if state == ModuleInstanceState::Instantiated {
                            return Err(state);
                        }
                        Ok(state)
                    },
                ) {
                    Ok(value) => value,
                    Err(value) => return value,
                };
                return state;
            }
        }
        Kind::ModuleBlock => {
            let mut state = ModuleInstanceState::NonInstantiated;
            let ancestors = push_ancestor(ancestors, node);
            node.for_each_child(&mut |n| {
                let child_state = get_module_instance_state_cached(n, &ancestors, visited);
                match child_state {
                    ModuleInstanceState::NonInstantiated => false,
                    ModuleInstanceState::ConstEnumOnly => {
                        state = ModuleInstanceState::ConstEnumOnly;
                        false
                    }
                    ModuleInstanceState::Instantiated => {
                        state = ModuleInstanceState::Instantiated;
                        true
                    }
                    ModuleInstanceState::Unknown => {
                        panic!("Unhandled case in getModuleInstanceStateWorker")
                    }
                }
            });
            return state;
        }
        Kind::ModuleDeclaration => {
            return get_module_instance_state_ex(node, ancestors, visited);
        }
        _ => {}
    }
    ModuleInstanceState::Instantiated
}

fn get_module_instance_state_for_alias_target(
    node: P<Node>,
    ancestors: &[P<Node>],
    visited: &mut FxHashMap<NodeId, ModuleInstanceState>,
) -> ModuleInstanceState {
    let name = node.property_name_or_name().unwrap();
    if name.kind() != Kind::Identifier {
        // Skip for invalid syntax like this: export { "x" }
        return ModuleInstanceState::Instantiated;
    }
    let (mut ancestors, mut p) = pop_ancestor(ancestors, node);
    while let Some(parent) = p {
        if is_block(parent) || is_module_block(parent) || is_source_file(parent) {
            let mut found = ModuleInstanceState::Unknown;
            let statements_ancestors = push_ancestor(ancestors, parent);
            for &statement in parent.statements() {
                if node_has_name(statement, name) {
                    let state =
                        get_module_instance_state_cached(statement, &statements_ancestors, visited);
                    if found == ModuleInstanceState::Unknown || state > found {
                        found = state;
                    }
                    if found == ModuleInstanceState::Instantiated {
                        return found;
                    }
                    if statement.kind() == Kind::ImportEqualsDeclaration {
                        // Treat re-exports of import aliases as instantiated since they're ambiguous. This is consistent
                        // with `export import x = mod.x` being treated as instantiated:
                        //   import x = mod.x;
                        //   export { x };
                        found = ModuleInstanceState::Instantiated;
                    }
                }
            }
            if found != ModuleInstanceState::Unknown {
                return found;
            }
        }
        (ancestors, p) = pop_ancestor(ancestors, parent);
    }
    // Couldn't locate, assume could refer to a value
    ModuleInstanceState::Instantiated
}

pub fn is_instantiated_module(node: P<Node>, preserve_const_enums: bool) -> bool {
    let module_state = get_module_instance_state(node);
    module_state == ModuleInstanceState::Instantiated
        || (preserve_const_enums && module_state == ModuleInstanceState::ConstEnumOnly)
}

fn node_has_name(statement: P<Node>, id: P<Node>) -> bool {
    if let Some(name) = statement.name() {
        return is_identifier(name) && name.text() == id.text();
    }
    if is_variable_statement(statement) {
        let declarations = statement
            .as_variable_statement()
            .declaration_list
            .as_variable_declaration_list()
            .declarations
            .nodes();
        return declarations.iter().any(|&d| node_has_name(d, id));
    }
    false
}

pub fn is_internal_module_import_equals_declaration(node: P<Node>) -> bool {
    is_import_equals_declaration(node)
        && node.as_import_equals_declaration().module_reference.kind()
            != Kind::ExternalModuleReference
}

pub fn is_const_assertion(node: P<Node>) -> bool {
    match node.kind() {
        Kind::AsExpression | Kind::TypeAssertionExpression => {
            is_const_type_reference(node.type_node().unwrap())
        }
        _ => false,
    }
}

pub fn is_const_type_reference(node: P<Node>) -> bool {
    is_type_reference_node(node)
        && node.type_arguments().is_empty()
        && is_identifier(node.as_type_reference_node().type_name)
        && node.as_type_reference_node().type_name.text() == "const"
}

pub fn is_global_source_file(node: P<Node>) -> bool {
    node.kind() == Kind::SourceFile && !is_external_or_common_js_module(node.as_source_file_p())
}

pub fn get_declaration_of_kind(symbol: P<Symbol>, kind: Kind) -> Option<P<Node>> {
    symbol.declarations().iter().copied().find(|declaration| declaration.kind() == kind)
}

pub fn find_constructor_declaration(node: P<Node>) -> Option<P<Node>> {
    node.members()
        .iter()
        .copied()
        .find(|&member| is_constructor_declaration(member) && node_is_present(member.body()))
}

pub fn get_first_identifier(node: P<Node>) -> P<Node> {
    match node.kind() {
        Kind::Identifier => node,
        Kind::QualifiedName => get_first_identifier(node.as_qualified_name().left),
        Kind::PropertyAccessExpression => {
            get_first_identifier(node.as_property_access_expression().expression)
        }
        _ => panic!("Unhandled case in GetFirstIdentifier"),
    }
}

pub fn get_namespace_declaration_node(node: P<Node>) -> Option<P<Node>> {
    match node.kind() {
        Kind::ImportDeclaration | Kind::JSImportDeclaration => {
            if let Some(import_clause) = node.import_clause()
                && let Some(named_bindings) = import_clause.as_import_clause().named_bindings
                && is_namespace_import(named_bindings)
            {
                return Some(named_bindings);
            }
        }
        Kind::ImportEqualsDeclaration => return Some(node),
        Kind::ExportDeclaration => {
            if let Some(export_clause) = node.as_export_declaration().export_clause
                && is_namespace_export(export_clause)
            {
                return Some(export_clause);
            }
        }
        _ => panic!("Unhandled case in getNamespaceDeclarationNode"),
    }
    None
}

pub fn module_export_name_is_default(node: P<Node>) -> bool {
    node.text() == INTERNAL_SYMBOL_NAME_DEFAULT
}

pub fn get_implied_node_format_for_file(path: &str, package_json_type: &str) -> ModuleKind {
    let mut implied_node_format = ResolutionMode::NONE;
    if tspath::file_extension_is_one_of(
        path,
        &[tspath::EXTENSION_DMTS, tspath::EXTENSION_MTS, tspath::EXTENSION_MJS],
    ) {
        implied_node_format = ResolutionMode::ESM;
    } else if tspath::file_extension_is_one_of(
        path,
        &[tspath::EXTENSION_DCTS, tspath::EXTENSION_CTS, tspath::EXTENSION_CJS],
    ) {
        implied_node_format = ResolutionMode::COMMON_JS;
    } else if tspath::file_extension_is_one_of(
        path,
        &[
            tspath::EXTENSION_DTS,
            tspath::EXTENSION_TS,
            tspath::EXTENSION_TSX,
            tspath::EXTENSION_JS,
            tspath::EXTENSION_JSX,
        ],
    ) {
        implied_node_format = if package_json_type == "module" {
            ResolutionMode::ESM
        } else {
            ResolutionMode::COMMON_JS
        };
    }

    implied_node_format
}

pub fn get_emit_module_format_of_file_worker(
    file_name: &str,
    options: &CompilerOptions,
    source_file_meta_data: &SourceFileMetaData,
) -> ModuleKind {
    let result = get_implied_node_format_for_emit_worker(
        file_name,
        options.get_emit_module_kind(),
        source_file_meta_data,
    );
    if result != ModuleKind::NONE {
        return result;
    }
    options.get_emit_module_kind()
}

pub fn get_implied_node_format_for_emit_worker(
    file_name: &str,
    emit_module_kind: ModuleKind,
    source_file_meta_data: &SourceFileMetaData,
) -> ResolutionMode {
    if ModuleKind::NODE16 <= emit_module_kind && emit_module_kind <= ModuleKind::NODE_NEXT {
        return source_file_meta_data.implied_node_format;
    }
    if source_file_meta_data.implied_node_format == ModuleKind::COMMON_JS
        && (source_file_meta_data.package_json_type == "commonjs"
            || tspath::file_extension_is_one_of(
                file_name,
                &[tspath::EXTENSION_CJS, tspath::EXTENSION_CTS],
            ))
    {
        return ModuleKind::COMMON_JS;
    }
    if source_file_meta_data.implied_node_format == ModuleKind::ES_NEXT
        && (source_file_meta_data.package_json_type == "module"
            || tspath::file_extension_is_one_of(
                file_name,
                &[tspath::EXTENSION_MJS, tspath::EXTENSION_MTS],
            ))
    {
        return ModuleKind::ES_NEXT;
    }
    ModuleKind::NONE
}

pub fn get_declaration_container(node: P<Node>) -> Option<P<Node>> {
    find_ancestor(get_root_declaration(node), |node| {
        !matches!(
            node.kind(),
            Kind::VariableDeclaration
                | Kind::VariableDeclarationList
                | Kind::ImportSpecifier
                | Kind::NamedImports
                | Kind::NamespaceImport
                | Kind::ImportClause
        )
    })
    .unwrap()
    .parent()
}

// Indicates that a symbol is an alias that does not merge with a local declaration.
// OR Is a JSContainer which may merge an alias with a local declaration
pub fn is_non_local_alias(symbol: impl Into<Option<P<Symbol>>>, excludes: SymbolFlags) -> bool {
    let Some(symbol) = symbol.into() else {
        return false;
    };
    let flags = symbol.flags.get();
    flags & (SymbolFlags::Alias | excludes) == SymbolFlags::Alias
        || flags.intersects(SymbolFlags::Alias) && flags.intersects(SymbolFlags::Assignment)
}

// An alias symbol is created by one of the following declarations:
//
//	import <symbol> = ...
//	const <symbol> = ... (JS only)
//	const { <symbol>, ... } = ... (JS only)
//	import <symbol> from ...
//	import * as <symbol> from ...
//	import { x as <symbol> } from ...
//	export { x as <symbol> } from ...
//	export * as ns <symbol> from ...
//	export = <EntityNameExpression>
//	export default <EntityNameExpression>
//	module.exports = <EntityNameExpression> (JS only)
//	module.exports.<symbol> = <EntityNameExpression> (JS only)
//	exports.<symbol> = <EntityNameExpression> (JS only)
pub fn is_alias_symbol_declaration(node: P<Node>) -> bool {
    match node.kind() {
        Kind::ImportEqualsDeclaration
        | Kind::NamespaceExportDeclaration
        | Kind::NamespaceImport
        | Kind::NamespaceExport
        | Kind::ImportSpecifier
        | Kind::ExportSpecifier => true,
        Kind::ImportClause => node.name().is_some(),
        Kind::ExportAssignment => expression_is_alias(node.expression().unwrap()),
        Kind::VariableDeclaration | Kind::BindingElement => {
            is_variable_declaration_initialized_to_require(node)
        }
        Kind::BinaryExpression => match get_assignment_declaration_kind(node) {
            JSDeclarationKind::ModuleExports | JSDeclarationKind::ExportsProperty => {
                expression_is_alias(node.as_binary_expression().right())
            }
            _ => false,
        },
        _ => false,
    }
}

pub fn is_parse_tree_node(node: P<Node>) -> bool {
    !node.flags.get().intersects(NodeFlags::Synthesized)
}

// Returns a token if position is in [start-of-leading-trivia, end), includes JSDoc only if requested
pub fn get_node_at_position(file: P<SourceFile>, position: u32, include_jsdoc: bool) -> P<Node> {
    let mut current = file.as_node();
    loop {
        let mut child: Option<P<Node>> = None;
        if include_jsdoc
            && let Some(jsdoc) = current.jsdoc(Some(file.get())).iter().find_map(|&jsdoc| {
                if node_contains_position(jsdoc, position) { Some(jsdoc) } else { None }
            })
        {
            child = Some(jsdoc);
        }
        if child.is_none() {
            current.for_each_child(&mut |node| {
                if node_contains_position(node, position) {
                    child = Some(node);
                    return true;
                }
                false
            });
        }
        match child {
            Some(c) if !is_meta_property(c) => current = c,
            _ => return current,
        }
    }
}

fn node_contains_position(node: P<Node>, position: u32) -> bool {
    let (Some(pos), Some(end)) = (node.pos().source_offset(), node.end().source_offset()) else {
        return false;
    };
    node.kind() >= Kind::FIRST_NODE
        && pos <= position
        && (position < end || position == end && node.kind() == Kind::EndOfFile)
}

// Go scans for the first 'i' or 'r' and compares; the first "import" or "require" at or after `start` is the
// same position, found here with two substring searches (the "require" search stops where "import" was found).
fn find_import_or_require(text: &str, start: u32) -> Option<(u32, u32)> {
    static IMPORT: LazyLock<memchr::memmem::Finder<'static>> =
        LazyLock::new(|| memchr::memmem::Finder::new("import"));
    static REQUIRE: LazyLock<memchr::memmem::Finder<'static>> =
        LazyLock::new(|| memchr::memmem::Finder::new("require"));
    let bytes = text.as_bytes();
    let index = (start as usize).min(bytes.len());
    let import = IMPORT.find(&bytes[index..]).map(|i| index + i);
    let require_end = match import {
        Some(i) => (i + "require".len()).min(bytes.len()),
        None => bytes.len(),
    };
    if let Some(i) = REQUIRE.find(&bytes[index..require_end]) {
        return Some(((index + i) as u32, 7));
    }
    import.map(|i| (i as u32, 6))
}

pub fn for_each_dynamic_import_or_require_call(
    file: P<SourceFile>,
    include_type_space_imports: bool,
    require_string_literal_like_argument: bool,
    mut cb: impl FnMut(P<Node>, P<Node>) -> bool,
) -> bool {
    let is_java_script_file = is_in_js_file(file.as_node());
    let mut next = find_import_or_require(file.text(), 0);
    while let Some((last_index, size)) = next {
        let node = get_node_at_position(
            file,
            last_index,
            is_java_script_file && include_type_space_imports,
        );
        if (is_java_script_file && is_require_call(node, require_string_literal_like_argument))
            || (is_import_call(node)
                && !node.arguments().is_empty()
                && (!require_string_literal_like_argument
                    || is_string_literal_like(node.arguments()[0])))
        {
            if cb(node, node.arguments()[0]) {
                return true;
            }
        } else if include_type_space_imports
            && is_literal_import_type_node(node)
            && cb(node, node.as_import_type_node().argument.as_literal_type_node().literal)
        {
            return true;
        }
        // skip past import/require
        next = find_import_or_require(file.text(), last_index + size);
    }
    false
}

// Returns true if the node is a CallExpression to the identifier 'require' with
// exactly one argument (of the form 'require("name")').
// This function does not test if the node is in a JavaScript file or not.
pub fn is_require_call(node: P<Node>, require_string_literal_like_argument: bool) -> bool {
    if !is_call_expression(node) {
        return false;
    }
    let call = node.as_call_expression();
    if !is_identifier(call.expression) || call.expression.text() != "require" {
        return false;
    }
    if call.arguments.nodes().len() != 1 {
        return false;
    }
    !require_string_literal_like_argument || is_string_literal_like(call.arguments.nodes()[0])
}

pub fn get_jsx_implicit_import_base(
    compiler_options: &CompilerOptions,
    file: Option<P<SourceFile>>,
) -> String {
    let jsx_import_source_pragma = get_pragma_from_source_file(file, "jsximportsource");
    let jsx_runtime_pragma = get_pragma_from_source_file(file, "jsxruntime");
    if get_pragma_argument(jsx_runtime_pragma, "factory") == "classic" {
        return String::new();
    }
    if compiler_options.jsx == JsxEmit::REACT_JSX
        || compiler_options.jsx == JsxEmit::REACT_JSX_DEV
        || !compiler_options.jsx_import_source.is_empty()
        || jsx_import_source_pragma.is_some()
        || get_pragma_argument(jsx_runtime_pragma, "factory") == "automatic"
    {
        let mut result = get_pragma_argument(jsx_import_source_pragma, "factory").to_string();
        if result.is_empty() {
            result.clone_from(&compiler_options.jsx_import_source);
        }
        if result.is_empty() {
            result = "react".to_string();
        }
        return result;
    }
    String::new()
}

pub fn get_jsx_runtime_import(base: &str, options: &CompilerOptions) -> String {
    if base.is_empty() {
        return base.to_string();
    }
    format!(
        "{}/{}",
        base,
        if options.jsx == JsxEmit::REACT_JSX_DEV { "jsx-dev-runtime" } else { "jsx-runtime" }
    )
}

pub fn get_pragma_from_source_file(
    file: Option<P<SourceFile>>,
    name: &str,
) -> Option<&'static Pragma> {
    let mut result = None;
    if let Some(file) = file
        && let Some(pragma) = file.pragmas().iter().rev().find(|pragma| pragma.name == name)
    {
        result = Some(pragma); // Last one wins
    }
    result
}

fn get_pragma_argument(pragma: Option<&'static Pragma>, name: &str) -> &'static str {
    if let Some(pragma) = pragma
        && let Some(arg) = pragma.args.get(name)
    {
        return arg.value.as_str();
    }
    ""
}

// Of the form: `const x = require("x")` or `const { x } = require("x")` or with `var` or `let`
// The variable must not be exported and must not have a type annotation, even a jsdoc one.
// The initializer must be a call to `require` with a string literal or a string literal-like argument.
pub fn is_variable_declaration_initialized_to_require(mut node: P<Node>) -> bool {
    if node.kind() == Kind::BindingElement {
        node = node.parent().unwrap().parent().unwrap();
    }
    is_variable_declaration_initialized_with_require_helper(
        node, false, /*allowAccessedRequire*/
    )
}

// utilities.go:2811
pub fn is_require_variable_statement(node: P<Node>) -> bool {
    if is_variable_statement(node) {
        let declarations = node
            .as_variable_statement()
            .declaration_list
            .as_variable_declaration_list()
            .declarations
            .nodes();
        if !declarations.is_empty() {
            return declarations.iter().all(|&d| is_variable_declaration_initialized_to_require(d));
        }
    }
    false
}

pub fn is_variable_declaration_initialized_to_bare_or_accessed_require(node: P<Node>) -> bool {
    is_variable_declaration_initialized_with_require_helper(
        node, true, /*allowAccessedRequire*/
    )
}

fn is_variable_declaration_initialized_with_require_helper(
    node: P<Node>,
    allow_accessed_require: bool,
) -> bool {
    if !is_in_js_file(node) {
        return false;
    }
    if node.kind() != Kind::VariableDeclaration {
        return false;
    }
    let Some(mut initializer) = node.initializer() else {
        return false;
    };
    if allow_accessed_require {
        initializer = get_leftmost_access_expression(initializer);
    }

    !node.parent().unwrap().parent().unwrap().modifier_flags().intersects(ModifierFlags::Export)
        && node.type_node().is_none()
        && is_require_call(initializer, true /*requireStringLiteralLikeArgument*/)
}

pub fn get_module_specifier_of_bare_or_accessed_require(node: P<Node>) -> Option<P<Node>> {
    if is_variable_declaration_initialized_with_require_helper(
        node, false, /*allowAccessedRequire*/
    ) {
        return Some(node.initializer().unwrap().arguments()[0]);
    }
    if is_variable_declaration_initialized_with_require_helper(
        node, true, /*allowAccessedRequire*/
    ) {
        let leftmost = get_leftmost_access_expression(node.initializer().unwrap());
        if is_require_call(leftmost, true /*requireStringLiteralLikeArgument*/) {
            return Some(leftmost.arguments()[0]);
        }
    }
    None
}

pub fn is_module_exports_access_expression(node: P<Node>) -> bool {
    if is_access_expression(node)
        && is_module_identifier(node.expression().unwrap())
        && let Some(name) = get_element_or_property_access_name(node)
    {
        return name.text() == "exports";
    }
    false
}

pub fn is_check_js_enabled_for_file(
    source_file: P<SourceFile>,
    compiler_options: &CompilerOptions,
) -> bool {
    if let Some(check_js_directive) = source_file.check_js_directive() {
        return check_js_directive.enabled;
    }
    compiler_options.check_js == Tristate::True
}

pub fn is_plain_js_file(file: Option<P<SourceFile>>, check_js: Tristate) -> bool {
    match file {
        Some(file) => {
            (file.script_kind() == ScriptKind::JS || file.script_kind() == ScriptKind::JSX)
                && file.check_js_directive().is_none()
                && check_js == Tristate::Unknown
        }
        None => false,
    }
}

pub fn get_leftmost_access_expression(mut expr: P<Node>) -> P<Node> {
    while is_access_expression(expr) {
        expr = expr.expression().unwrap();
    }
    expr
}

pub fn is_type_only_import_declaration(node: P<Node>) -> bool {
    match node.kind() {
        Kind::ImportSpecifier => {
            node.is_type_only() || node.parent().unwrap().parent().unwrap().is_type_only()
        }
        Kind::NamespaceImport => node.parent().unwrap().is_type_only(),
        Kind::ImportClause | Kind::ImportEqualsDeclaration => node.is_type_only(),
        _ => false,
    }
}

fn is_type_only_export_declaration(node: P<Node>) -> bool {
    match node.kind() {
        Kind::ExportSpecifier => {
            node.is_type_only() || node.parent().unwrap().parent().unwrap().is_type_only()
        }
        Kind::ExportDeclaration => {
            let d = node.as_export_declaration();
            d.is_type_only && d.module_specifier.is_some() && d.export_clause.is_none()
        }
        Kind::NamespaceExport => node.parent().unwrap().is_type_only(),
        _ => false,
    }
}

pub fn is_type_only_import_or_export_declaration(node: P<Node>) -> bool {
    is_type_only_import_declaration(node) || is_type_only_export_declaration(node)
}

pub fn is_exclusively_type_only_import_or_export(node: P<Node>) -> bool {
    match node.kind() {
        Kind::ExportDeclaration => node.is_type_only(),
        Kind::ImportDeclaration | Kind::JSImportDeclaration | Kind::JSDocImportTag => {
            match node.import_clause() {
                Some(import_clause) => import_clause.is_type_only(),
                None => false,
            }
        }
        _ => false,
    }
}

pub fn get_class_like_declaration_of_symbol(symbol: P<Symbol>) -> Option<P<Node>> {
    symbol.declarations().iter().copied().find(|&d| is_class_like(d))
}
