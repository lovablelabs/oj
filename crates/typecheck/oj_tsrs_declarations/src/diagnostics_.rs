use crate::*;

// Non-function declarations in diagnostics.go (hand-ported in types.rs):
//   type GetSymbolAccessibilityDiagnostic (diagnostics.go:10)
//   type SymbolAccessibilityDiagnostic (diagnostics.go:12)

// The `wrap*DiagnosticSelector` functions return a `GetSymbolAccessibilityDiagnostic` (`Rc<dyn Fn>`) that captures
// the selector, so the selector is `Fn + 'static` (Go passes named functions and one closure).

// diagnostics.go:18
pub fn wrap_simple_diagnostic_selector(
    node: P<Node>,
    selector: impl Fn(P<Node>, &SymbolAccessibilityResult) -> Option<&'static Message> + 'static,
) -> GetSymbolAccessibilityDiagnostic {
    Rc::new(move |symbol_accessibility_result: &SymbolAccessibilityResult| {
        let diagnostic_message = selector(node, symbol_accessibility_result)?;
        Some(P::new(SymbolAccessibilityDiagnostic {
            error_node: Some(node),
            diagnostic_message,
            type_name: ast::get_name_of_declaration(node),
        }))
    })
}

// diagnostics.go:32
pub fn wrap_named_diagnostic_selector(
    node: P<Node>,
    selector: impl Fn(P<Node>, &SymbolAccessibilityResult) -> &'static Message + 'static,
) -> GetSymbolAccessibilityDiagnostic {
    Rc::new(move |symbol_accessibility_result: &SymbolAccessibilityResult| {
        let diagnostic_message = selector(node, symbol_accessibility_result);
        let name = ast::get_name_of_declaration(node);
        Some(P::new(SymbolAccessibilityDiagnostic {
            error_node: name,
            diagnostic_message,
            type_name: name,
        }))
    })
}

// diagnostics.go:47
pub fn wrap_fallback_error_diagnostic_selector(
    node: P<Node>,
    selector: impl Fn(P<Node>, &SymbolAccessibilityResult) -> &'static Message + 'static,
) -> GetSymbolAccessibilityDiagnostic {
    Rc::new(move |symbol_accessibility_result: &SymbolAccessibilityResult| {
        let diagnostic_message = selector(node, symbol_accessibility_result);
        let mut error_node = ast::get_name_of_declaration(node);
        if error_node.is_none() {
            error_node = Some(node);
        }
        Some(P::new(SymbolAccessibilityDiagnostic {
            error_node,
            diagnostic_message,
            type_name: None,
        }))
    })
}

// diagnostics.go:64
pub fn select_diagnostic_based_on_module_name(
    symbol_accessibility_result: &SymbolAccessibilityResult,
    module_not_nameable: &'static Message,
    private_module: &'static Message,
    non_module: &'static Message,
) -> &'static Message {
    if !symbol_accessibility_result.error_module_name.is_empty() {
        if symbol_accessibility_result.accessibility == SymbolAccessibility::CannotBeNamed {
            return module_not_nameable;
        }
        return private_module;
    }
    non_module
}

// diagnostics.go:74
pub fn select_diagnostic_based_on_module_name_no_name_check(
    symbol_accessibility_result: &SymbolAccessibilityResult,
    private_module: &'static Message,
    non_module: &'static Message,
) -> &'static Message {
    if !symbol_accessibility_result.error_module_name.is_empty() {
        return private_module;
    }
    non_module
}

// diagnostics.go:81
pub fn create_get_symbol_accessibility_diagnostic_for_node_name(
    node: P<Node>,
) -> GetSymbolAccessibilityDiagnostic {
    if ast::is_set_accessor_declaration(node) || ast::is_get_accessor_declaration(node) {
        wrap_simple_diagnostic_selector(node, |n, r| {
            Some(get_accessor_name_visibility_diagnostic_message(n, r))
        })
    } else if ast::is_method_declaration(node) || ast::is_method_signature_declaration(node) {
        wrap_simple_diagnostic_selector(node, |n, r| {
            Some(get_method_name_visibility_diagnostic_message(n, r))
        })
    } else {
        create_get_symbol_accessibility_diagnostic_for_node(node)
    }
}

// diagnostics.go:91
pub fn get_accessor_name_visibility_diagnostic_message(
    node: P<Node>,
    symbol_accessibility_result: &SymbolAccessibilityResult,
) -> &'static Message {
    if ast::is_static(node) {
        select_diagnostic_based_on_module_name(
            symbol_accessibility_result,
            &diagnostics::PUBLIC_STATIC_PROPERTY_0_OF_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_1_FROM_EXTERNAL_MODULE_2_BUT_CANNOT_BE_NAMED,
            &diagnostics::PUBLIC_STATIC_PROPERTY_0_OF_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_1_FROM_PRIVATE_MODULE_2,
            &diagnostics::PUBLIC_STATIC_PROPERTY_0_OF_EXPORTED_CLASS_HAS_OR_IS_USING_PRIVATE_NAME_1,
        )
    } else if node.parent().unwrap().kind() == Kind::ClassDeclaration {
        select_diagnostic_based_on_module_name(
            symbol_accessibility_result,
            &diagnostics::PUBLIC_PROPERTY_0_OF_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_1_FROM_EXTERNAL_MODULE_2_BUT_CANNOT_BE_NAMED,
            &diagnostics::PUBLIC_PROPERTY_0_OF_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_1_FROM_PRIVATE_MODULE_2,
            &diagnostics::PUBLIC_PROPERTY_0_OF_EXPORTED_CLASS_HAS_OR_IS_USING_PRIVATE_NAME_1,
        )
    } else {
        select_diagnostic_based_on_module_name_no_name_check(
            symbol_accessibility_result,
            &diagnostics::PROPERTY_0_OF_EXPORTED_INTERFACE_HAS_OR_IS_USING_NAME_1_FROM_PRIVATE_MODULE_2,
            &diagnostics::PROPERTY_0_OF_EXPORTED_INTERFACE_HAS_OR_IS_USING_PRIVATE_NAME_1,
        )
    }
}

// diagnostics.go:115
pub fn get_method_name_visibility_diagnostic_message(
    node: P<Node>,
    symbol_accessibility_result: &SymbolAccessibilityResult,
) -> &'static Message {
    if ast::is_static(node) {
        select_diagnostic_based_on_module_name(
            symbol_accessibility_result,
            &diagnostics::PUBLIC_STATIC_METHOD_0_OF_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_1_FROM_EXTERNAL_MODULE_2_BUT_CANNOT_BE_NAMED,
            &diagnostics::PUBLIC_STATIC_METHOD_0_OF_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_1_FROM_PRIVATE_MODULE_2,
            &diagnostics::PUBLIC_STATIC_METHOD_0_OF_EXPORTED_CLASS_HAS_OR_IS_USING_PRIVATE_NAME_1,
        )
    } else if node.parent().unwrap().kind() == Kind::ClassDeclaration {
        select_diagnostic_based_on_module_name(
            symbol_accessibility_result,
            &diagnostics::PUBLIC_METHOD_0_OF_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_1_FROM_EXTERNAL_MODULE_2_BUT_CANNOT_BE_NAMED,
            &diagnostics::PUBLIC_METHOD_0_OF_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_1_FROM_PRIVATE_MODULE_2,
            &diagnostics::PUBLIC_METHOD_0_OF_EXPORTED_CLASS_HAS_OR_IS_USING_PRIVATE_NAME_1,
        )
    } else {
        select_diagnostic_based_on_module_name_no_name_check(
            symbol_accessibility_result,
            &diagnostics::METHOD_0_OF_EXPORTED_INTERFACE_HAS_OR_IS_USING_NAME_1_FROM_PRIVATE_MODULE_2,
            &diagnostics::METHOD_0_OF_EXPORTED_INTERFACE_HAS_OR_IS_USING_PRIVATE_NAME_1,
        )
    }
}

// diagnostics.go:139
pub fn create_get_symbol_accessibility_diagnostic_for_node(
    node: P<Node>,
) -> GetSymbolAccessibilityDiagnostic {
    if ast::is_variable_declaration(node)
        || ast::is_property_declaration(node)
        || ast::is_property_signature_declaration(node)
        || ast::is_property_access_expression(node)
        || ast::is_element_access_expression(node)
        || ast::is_binary_expression(node)
        || ast::is_binding_element(node)
        || ast::is_constructor_declaration(node)
    {
        wrap_simple_diagnostic_selector(
            node,
            get_variable_declaration_type_visibility_diagnostic_message,
        )
    } else if ast::is_set_accessor_declaration(node) || ast::is_get_accessor_declaration(node) {
        wrap_named_diagnostic_selector(
            node,
            get_accessor_declaration_type_visibility_diagnostic_message,
        )
    } else if ast::is_construct_signature_declaration(node)
        || ast::is_call_signature_declaration(node)
        || ast::is_method_declaration(node)
        || ast::is_method_signature_declaration(node)
        || ast::is_function_declaration(node)
        || ast::is_index_signature_declaration(node)
    {
        wrap_fallback_error_diagnostic_selector(node, get_return_type_visibility_diagnostic_message)
    } else if ast::is_parameter_declaration(node) {
        if ast::is_parameter_property_declaration(node, node.parent().unwrap())
            && ast::has_syntactic_modifier(node.parent().unwrap(), ModifierFlags::Private)
        {
            return wrap_simple_diagnostic_selector(
                node,
                get_variable_declaration_type_visibility_diagnostic_message,
            );
        }
        wrap_simple_diagnostic_selector(node, |n, r| {
            Some(get_parameter_declaration_type_visibility_diagnostic_message(n, r))
        })
    } else if ast::is_type_parameter_declaration(node) {
        wrap_simple_diagnostic_selector(node, |n, r| {
            Some(get_type_parameter_constraint_visibility_diagnostic_message(n, r))
        })
    } else if ast::is_expression_with_type_arguments(node) {
        // unique node selection behavior, inline closure
        Rc::new(move |_symbol_accessibility_result: &SymbolAccessibilityResult| {
            let diagnostic_message: &'static Message;
            let parent = node.parent().unwrap();
            let grand_parent = parent.parent().unwrap();
            // Heritage clause is written by user so it can always be named
            if ast::is_class_declaration(grand_parent) {
                // Class or Interface implemented/extended is inaccessible
                if ast::is_heritage_clause(parent)
                    && parent.as_heritage_clause().token == Kind::ImplementsKeyword
                {
                    diagnostic_message = &diagnostics::IMPLEMENTS_CLAUSE_OF_EXPORTED_CLASS_0_HAS_OR_IS_USING_PRIVATE_NAME_1;
                } else if grand_parent.name().is_some() {
                    diagnostic_message = &diagnostics::X_EXTENDS_CLAUSE_OF_EXPORTED_CLASS_0_HAS_OR_IS_USING_PRIVATE_NAME_1;
                } else {
                    diagnostic_message = &diagnostics::X_EXTENDS_CLAUSE_OF_EXPORTED_CLASS_HAS_OR_IS_USING_PRIVATE_NAME_0;
                }
            } else {
                // interface is inaccessible
                diagnostic_message = &diagnostics::X_EXTENDS_CLAUSE_OF_EXPORTED_INTERFACE_0_HAS_OR_IS_USING_PRIVATE_NAME_1;
            }

            Some(P::new(SymbolAccessibilityDiagnostic {
                diagnostic_message,
                error_node: Some(node),
                type_name: ast::get_name_of_declaration(grand_parent),
            }))
        })
    } else if ast::is_import_equals_declaration(node) {
        wrap_simple_diagnostic_selector(node, |_, _| {
            Some(&diagnostics::IMPORT_DECLARATION_0_IS_USING_PRIVATE_NAME_1)
        })
    } else if ast::is_type_alias_declaration(node) || ast::is_js_type_alias_declaration(node) {
        // unique node selection behavior, inline closure
        Rc::new(move |symbol_accessibility_result: &SymbolAccessibilityResult| {
            let diagnostic_message = select_diagnostic_based_on_module_name_no_name_check(
                symbol_accessibility_result,
                &diagnostics::EXPORTED_TYPE_ALIAS_0_HAS_OR_IS_USING_PRIVATE_NAME_1_FROM_MODULE_2,
                &diagnostics::EXPORTED_TYPE_ALIAS_0_HAS_OR_IS_USING_PRIVATE_NAME_1,
            );
            let error_node = node.type_node();
            let type_name = node.name();
            Some(P::new(SymbolAccessibilityDiagnostic {
                error_node,
                diagnostic_message,
                type_name,
            }))
        })
    } else if ast::is_call_expression(node) {
        // JS object.defineProperty call
        // unique node selection behavior, inline closure
        Rc::new(move |symbol_accessibility_result: &SymbolAccessibilityResult| {
            let diagnostic_message = select_diagnostic_based_on_module_name(
                symbol_accessibility_result,
                &diagnostics::EXPORTED_VARIABLE_0_HAS_OR_IS_USING_NAME_1_FROM_EXTERNAL_MODULE_2_BUT_CANNOT_BE_NAMED,
                &diagnostics::EXPORTED_VARIABLE_0_HAS_OR_IS_USING_NAME_1_FROM_PRIVATE_MODULE_2,
                &diagnostics::EXPORTED_VARIABLE_0_HAS_OR_IS_USING_PRIVATE_NAME_1,
            );
            let error_node = node.arguments()[1];
            let type_name = node.arguments()[1];
            Some(P::new(SymbolAccessibilityDiagnostic {
                error_node: Some(error_node),
                diagnostic_message,
                type_name: Some(type_name),
            }))
        })
    } else {
        panic!(
            "Attempted to set a declaration diagnostic context for unhandled node kind: {}",
            node.kind_string()
        );
    }
}

// diagnostics.go:223
pub fn get_variable_declaration_type_visibility_diagnostic_message(
    node: P<Node>,
    symbol_accessibility_result: &SymbolAccessibilityResult,
) -> Option<&'static Message> {
    if node.kind() == Kind::VariableDeclaration || node.kind() == Kind::BindingElement {
        return Some(select_diagnostic_based_on_module_name(
            symbol_accessibility_result,
            &diagnostics::EXPORTED_VARIABLE_0_HAS_OR_IS_USING_NAME_1_FROM_EXTERNAL_MODULE_2_BUT_CANNOT_BE_NAMED,
            &diagnostics::EXPORTED_VARIABLE_0_HAS_OR_IS_USING_NAME_1_FROM_PRIVATE_MODULE_2,
            &diagnostics::EXPORTED_VARIABLE_0_HAS_OR_IS_USING_PRIVATE_NAME_1,
        ));

        // This check is to ensure we don't report error on constructor parameter property as that error would be reported during parameter emit
        // The only exception here is if the constructor was marked as private. we are not emitting the constructor parameters at all.
    } else if node.kind() == Kind::PropertyDeclaration
        || node.kind() == Kind::PropertyAccessExpression
        || node.kind() == Kind::ElementAccessExpression
        || node.kind() == Kind::BinaryExpression
        || node.kind() == Kind::PropertySignature
        || (node.kind() == Kind::Parameter
            && ast::has_syntactic_modifier(node.parent().unwrap(), ModifierFlags::Private))
    {
        // TODO(jfreeman): Deal with computed properties in error reporting.
        if ast::is_static(node) {
            return Some(select_diagnostic_based_on_module_name(
                symbol_accessibility_result,
                &diagnostics::PUBLIC_STATIC_PROPERTY_0_OF_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_1_FROM_EXTERNAL_MODULE_2_BUT_CANNOT_BE_NAMED,
                &diagnostics::PUBLIC_STATIC_PROPERTY_0_OF_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_1_FROM_PRIVATE_MODULE_2,
                &diagnostics::PUBLIC_STATIC_PROPERTY_0_OF_EXPORTED_CLASS_HAS_OR_IS_USING_PRIVATE_NAME_1,
            ));
        } else if node.parent().unwrap().kind() == Kind::ClassDeclaration
            || node.kind() == Kind::Parameter
        {
            return Some(select_diagnostic_based_on_module_name(
                symbol_accessibility_result,
                &diagnostics::PUBLIC_PROPERTY_0_OF_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_1_FROM_EXTERNAL_MODULE_2_BUT_CANNOT_BE_NAMED,
                &diagnostics::PUBLIC_PROPERTY_0_OF_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_1_FROM_PRIVATE_MODULE_2,
                &diagnostics::PUBLIC_PROPERTY_0_OF_EXPORTED_CLASS_HAS_OR_IS_USING_PRIVATE_NAME_1,
            ));
        }
        // Interfaces cannot have types that cannot be named
        return Some(select_diagnostic_based_on_module_name_no_name_check(
            symbol_accessibility_result,
            &diagnostics::PROPERTY_0_OF_EXPORTED_INTERFACE_HAS_OR_IS_USING_NAME_1_FROM_PRIVATE_MODULE_2,
            &diagnostics::PROPERTY_0_OF_EXPORTED_INTERFACE_HAS_OR_IS_USING_PRIVATE_NAME_1,
        ));
    }
    None // TODO: Audit behavior - should this panic? potentially silent error state in strada
}

// diagnostics.go:263
pub fn get_accessor_declaration_type_visibility_diagnostic_message(
    node: P<Node>,
    symbol_accessibility_result: &SymbolAccessibilityResult,
) -> &'static Message {
    if node.kind() == Kind::SetAccessor {
        // Getters can infer the return type from the returned expression, but setters cannot, so the
        // "_from_external_module_1_but_cannot_be_named" case cannot occur.
        if ast::is_static(node) {
            select_diagnostic_based_on_module_name_no_name_check(
                symbol_accessibility_result,
                &diagnostics::PARAMETER_TYPE_OF_PUBLIC_STATIC_SETTER_0_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_1_FROM_PRIVATE_MODULE_2,
                &diagnostics::PARAMETER_TYPE_OF_PUBLIC_STATIC_SETTER_0_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_PRIVATE_NAME_1,
            )
        } else {
            select_diagnostic_based_on_module_name_no_name_check(
                symbol_accessibility_result,
                &diagnostics::PARAMETER_TYPE_OF_PUBLIC_SETTER_0_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_1_FROM_PRIVATE_MODULE_2,
                &diagnostics::PARAMETER_TYPE_OF_PUBLIC_SETTER_0_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_PRIVATE_NAME_1,
            )
        }
    } else if ast::is_static(node) {
        select_diagnostic_based_on_module_name(
            symbol_accessibility_result,
            &diagnostics::RETURN_TYPE_OF_PUBLIC_STATIC_GETTER_0_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_1_FROM_EXTERNAL_MODULE_2_BUT_CANNOT_BE_NAMED,
            &diagnostics::RETURN_TYPE_OF_PUBLIC_STATIC_GETTER_0_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_1_FROM_PRIVATE_MODULE_2,
            &diagnostics::RETURN_TYPE_OF_PUBLIC_STATIC_GETTER_0_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_PRIVATE_NAME_1,
        )
    } else {
        select_diagnostic_based_on_module_name(
            symbol_accessibility_result,
            &diagnostics::RETURN_TYPE_OF_PUBLIC_GETTER_0_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_1_FROM_EXTERNAL_MODULE_2_BUT_CANNOT_BE_NAMED,
            &diagnostics::RETURN_TYPE_OF_PUBLIC_GETTER_0_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_1_FROM_PRIVATE_MODULE_2,
            &diagnostics::RETURN_TYPE_OF_PUBLIC_GETTER_0_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_PRIVATE_NAME_1,
        )
    }
}

// diagnostics.go:299
pub fn get_return_type_visibility_diagnostic_message(
    node: P<Node>,
    symbol_accessibility_result: &SymbolAccessibilityResult,
) -> &'static Message {
    match node.kind() {
        Kind::ConstructSignature => {
            // Interfaces cannot have return types that cannot be named
            select_diagnostic_based_on_module_name_no_name_check(
                symbol_accessibility_result,
                &diagnostics::RETURN_TYPE_OF_CONSTRUCTOR_SIGNATURE_FROM_EXPORTED_INTERFACE_HAS_OR_IS_USING_NAME_0_FROM_PRIVATE_MODULE_1,
                &diagnostics::RETURN_TYPE_OF_CONSTRUCTOR_SIGNATURE_FROM_EXPORTED_INTERFACE_HAS_OR_IS_USING_PRIVATE_NAME_0,
            )
        }
        Kind::CallSignature => {
            // Interfaces cannot have return types that cannot be named
            select_diagnostic_based_on_module_name_no_name_check(
                symbol_accessibility_result,
                &diagnostics::RETURN_TYPE_OF_CALL_SIGNATURE_FROM_EXPORTED_INTERFACE_HAS_OR_IS_USING_NAME_0_FROM_PRIVATE_MODULE_1,
                &diagnostics::RETURN_TYPE_OF_CALL_SIGNATURE_FROM_EXPORTED_INTERFACE_HAS_OR_IS_USING_PRIVATE_NAME_0,
            )
        }
        Kind::IndexSignature => {
            // Interfaces cannot have return types that cannot be named
            select_diagnostic_based_on_module_name_no_name_check(
                symbol_accessibility_result,
                &diagnostics::RETURN_TYPE_OF_INDEX_SIGNATURE_FROM_EXPORTED_INTERFACE_HAS_OR_IS_USING_NAME_0_FROM_PRIVATE_MODULE_1,
                &diagnostics::RETURN_TYPE_OF_INDEX_SIGNATURE_FROM_EXPORTED_INTERFACE_HAS_OR_IS_USING_PRIVATE_NAME_0,
            )
        }

        Kind::MethodDeclaration | Kind::MethodSignature => {
            if ast::is_static(node) {
                select_diagnostic_based_on_module_name(
                    symbol_accessibility_result,
                    &diagnostics::RETURN_TYPE_OF_PUBLIC_STATIC_METHOD_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_0_FROM_EXTERNAL_MODULE_1_BUT_CANNOT_BE_NAMED,
                    &diagnostics::RETURN_TYPE_OF_PUBLIC_STATIC_METHOD_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_0_FROM_PRIVATE_MODULE_1,
                    &diagnostics::RETURN_TYPE_OF_PUBLIC_STATIC_METHOD_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_PRIVATE_NAME_0,
                )
            } else if node.parent().unwrap().kind() == Kind::ClassDeclaration {
                select_diagnostic_based_on_module_name(
                    symbol_accessibility_result,
                    &diagnostics::RETURN_TYPE_OF_PUBLIC_METHOD_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_0_FROM_EXTERNAL_MODULE_1_BUT_CANNOT_BE_NAMED,
                    &diagnostics::RETURN_TYPE_OF_PUBLIC_METHOD_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_0_FROM_PRIVATE_MODULE_1,
                    &diagnostics::RETURN_TYPE_OF_PUBLIC_METHOD_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_PRIVATE_NAME_0,
                )
            } else {
                // Interfaces cannot have return types that cannot be named
                select_diagnostic_based_on_module_name_no_name_check(
                    symbol_accessibility_result,
                    &diagnostics::RETURN_TYPE_OF_METHOD_FROM_EXPORTED_INTERFACE_HAS_OR_IS_USING_NAME_0_FROM_PRIVATE_MODULE_1,
                    &diagnostics::RETURN_TYPE_OF_METHOD_FROM_EXPORTED_INTERFACE_HAS_OR_IS_USING_PRIVATE_NAME_0,
                )
            }
        }
        Kind::FunctionDeclaration => select_diagnostic_based_on_module_name(
            symbol_accessibility_result,
            &diagnostics::RETURN_TYPE_OF_EXPORTED_FUNCTION_HAS_OR_IS_USING_NAME_0_FROM_EXTERNAL_MODULE_1_BUT_CANNOT_BE_NAMED,
            &diagnostics::RETURN_TYPE_OF_EXPORTED_FUNCTION_HAS_OR_IS_USING_NAME_0_FROM_PRIVATE_MODULE_1,
            &diagnostics::RETURN_TYPE_OF_EXPORTED_FUNCTION_HAS_OR_IS_USING_PRIVATE_NAME_0,
        ),
        _ => panic!("This is unknown kind for signature: {}", node.kind_string()),
    }
}

// diagnostics.go:358
pub fn get_parameter_declaration_type_visibility_diagnostic_message(
    node: P<Node>,
    symbol_accessibility_result: &SymbolAccessibilityResult,
) -> &'static Message {
    let parent = node.parent().unwrap();
    match parent.kind() {
        Kind::Constructor => select_diagnostic_based_on_module_name(
            symbol_accessibility_result,
            &diagnostics::PARAMETER_0_OF_CONSTRUCTOR_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_1_FROM_EXTERNAL_MODULE_2_BUT_CANNOT_BE_NAMED,
            &diagnostics::PARAMETER_0_OF_CONSTRUCTOR_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_1_FROM_PRIVATE_MODULE_2,
            &diagnostics::PARAMETER_0_OF_CONSTRUCTOR_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_PRIVATE_NAME_1,
        ),

        Kind::ConstructSignature | Kind::ConstructorType => {
            // Interfaces cannot have parameter types that cannot be named
            select_diagnostic_based_on_module_name_no_name_check(
                symbol_accessibility_result,
                &diagnostics::PARAMETER_0_OF_CONSTRUCTOR_SIGNATURE_FROM_EXPORTED_INTERFACE_HAS_OR_IS_USING_NAME_1_FROM_PRIVATE_MODULE_2,
                &diagnostics::PARAMETER_0_OF_CONSTRUCTOR_SIGNATURE_FROM_EXPORTED_INTERFACE_HAS_OR_IS_USING_PRIVATE_NAME_1,
            )
        }

        Kind::CallSignature => {
            // Interfaces cannot have parameter types that cannot be named
            select_diagnostic_based_on_module_name_no_name_check(
                symbol_accessibility_result,
                &diagnostics::PARAMETER_0_OF_CALL_SIGNATURE_FROM_EXPORTED_INTERFACE_HAS_OR_IS_USING_NAME_1_FROM_PRIVATE_MODULE_2,
                &diagnostics::PARAMETER_0_OF_CALL_SIGNATURE_FROM_EXPORTED_INTERFACE_HAS_OR_IS_USING_PRIVATE_NAME_1,
            )
        }

        Kind::IndexSignature => {
            // Interfaces cannot have parameter types that cannot be named
            select_diagnostic_based_on_module_name_no_name_check(
                symbol_accessibility_result,
                &diagnostics::PARAMETER_0_OF_INDEX_SIGNATURE_FROM_EXPORTED_INTERFACE_HAS_OR_IS_USING_NAME_1_FROM_PRIVATE_MODULE_2,
                &diagnostics::PARAMETER_0_OF_INDEX_SIGNATURE_FROM_EXPORTED_INTERFACE_HAS_OR_IS_USING_PRIVATE_NAME_1,
            )
        }

        Kind::MethodDeclaration | Kind::MethodSignature => {
            if ast::is_static(parent) {
                select_diagnostic_based_on_module_name(
                    symbol_accessibility_result,
                    &diagnostics::PARAMETER_0_OF_PUBLIC_STATIC_METHOD_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_1_FROM_EXTERNAL_MODULE_2_BUT_CANNOT_BE_NAMED,
                    &diagnostics::PARAMETER_0_OF_PUBLIC_STATIC_METHOD_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_1_FROM_PRIVATE_MODULE_2,
                    &diagnostics::PARAMETER_0_OF_PUBLIC_STATIC_METHOD_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_PRIVATE_NAME_1,
                )
            } else if parent.parent().unwrap().kind() == Kind::ClassDeclaration {
                select_diagnostic_based_on_module_name(
                    symbol_accessibility_result,
                    &diagnostics::PARAMETER_0_OF_PUBLIC_METHOD_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_1_FROM_EXTERNAL_MODULE_2_BUT_CANNOT_BE_NAMED,
                    &diagnostics::PARAMETER_0_OF_PUBLIC_METHOD_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_NAME_1_FROM_PRIVATE_MODULE_2,
                    &diagnostics::PARAMETER_0_OF_PUBLIC_METHOD_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_PRIVATE_NAME_1,
                )
            } else {
                // Interfaces cannot have parameter types that cannot be named
                select_diagnostic_based_on_module_name_no_name_check(
                    symbol_accessibility_result,
                    &diagnostics::PARAMETER_0_OF_METHOD_FROM_EXPORTED_INTERFACE_HAS_OR_IS_USING_NAME_1_FROM_PRIVATE_MODULE_2,
                    &diagnostics::PARAMETER_0_OF_METHOD_FROM_EXPORTED_INTERFACE_HAS_OR_IS_USING_PRIVATE_NAME_1,
                )
            }
        }

        Kind::FunctionDeclaration | Kind::FunctionType | Kind::ArrowFunction | Kind::FunctionExpression => select_diagnostic_based_on_module_name(
            symbol_accessibility_result,
            &diagnostics::PARAMETER_0_OF_EXPORTED_FUNCTION_HAS_OR_IS_USING_NAME_1_FROM_EXTERNAL_MODULE_2_BUT_CANNOT_BE_NAMED,
            &diagnostics::PARAMETER_0_OF_EXPORTED_FUNCTION_HAS_OR_IS_USING_NAME_1_FROM_PRIVATE_MODULE_2,
            &diagnostics::PARAMETER_0_OF_EXPORTED_FUNCTION_HAS_OR_IS_USING_PRIVATE_NAME_1,
        ),
        Kind::SetAccessor | Kind::GetAccessor => select_diagnostic_based_on_module_name(
            symbol_accessibility_result,
            &diagnostics::PARAMETER_0_OF_ACCESSOR_HAS_OR_IS_USING_NAME_1_FROM_EXTERNAL_MODULE_2_BUT_CANNOT_BE_NAMED,
            &diagnostics::PARAMETER_0_OF_ACCESSOR_HAS_OR_IS_USING_NAME_1_FROM_PRIVATE_MODULE_2,
            &diagnostics::PARAMETER_0_OF_ACCESSOR_HAS_OR_IS_USING_PRIVATE_NAME_1,
        ),

        _ => panic!("Unknown parent for parameter: {}", parent.kind_string()),
    }
}

// diagnostics.go:436
pub fn get_type_parameter_constraint_visibility_diagnostic_message(
    node: P<Node>,
    _symbol_accessibility_result: &SymbolAccessibilityResult,
) -> &'static Message {
    // Type parameter constraints are named by user so we should always be able to name it
    let parent = node.parent().unwrap();
    match parent.kind() {
        Kind::ClassDeclaration => &diagnostics::TYPE_PARAMETER_0_OF_EXPORTED_CLASS_HAS_OR_IS_USING_PRIVATE_NAME_1,
        Kind::InterfaceDeclaration => &diagnostics::TYPE_PARAMETER_0_OF_EXPORTED_INTERFACE_HAS_OR_IS_USING_PRIVATE_NAME_1,
        Kind::MappedType => &diagnostics::TYPE_PARAMETER_0_OF_EXPORTED_MAPPED_OBJECT_TYPE_IS_USING_PRIVATE_NAME_1,
        Kind::ConstructorType | Kind::ConstructSignature => &diagnostics::TYPE_PARAMETER_0_OF_CONSTRUCTOR_SIGNATURE_FROM_EXPORTED_INTERFACE_HAS_OR_IS_USING_PRIVATE_NAME_1,
        Kind::CallSignature => &diagnostics::TYPE_PARAMETER_0_OF_CALL_SIGNATURE_FROM_EXPORTED_INTERFACE_HAS_OR_IS_USING_PRIVATE_NAME_1,
        Kind::MethodDeclaration | Kind::MethodSignature => {
            if ast::is_static(parent) {
                &diagnostics::TYPE_PARAMETER_0_OF_PUBLIC_STATIC_METHOD_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_PRIVATE_NAME_1
            } else if parent.parent().unwrap().kind() == Kind::ClassDeclaration {
                &diagnostics::TYPE_PARAMETER_0_OF_PUBLIC_METHOD_FROM_EXPORTED_CLASS_HAS_OR_IS_USING_PRIVATE_NAME_1
            } else {
                &diagnostics::TYPE_PARAMETER_0_OF_METHOD_FROM_EXPORTED_INTERFACE_HAS_OR_IS_USING_PRIVATE_NAME_1
            }
        }
        Kind::FunctionType | Kind::FunctionDeclaration => &diagnostics::TYPE_PARAMETER_0_OF_EXPORTED_FUNCTION_HAS_OR_IS_USING_PRIVATE_NAME_1,

        Kind::InferType => &diagnostics::EXTENDS_CLAUSE_FOR_INFERRED_TYPE_0_HAS_OR_IS_USING_PRIVATE_NAME_1,

        Kind::TypeAliasDeclaration | Kind::JSTypeAliasDeclaration => &diagnostics::TYPE_PARAMETER_0_OF_EXPORTED_TYPE_ALIAS_HAS_OR_IS_USING_PRIVATE_NAME_1,

        _ => panic!("This is unknown parent for type parameter: {}", parent.kind_string()),
    }
}

// diagnostics.go:471
pub fn get_related_suggestion_by_declaration_kind(kind: Kind) -> Option<&'static Message> {
    match kind {
        Kind::ArrowFunction | Kind::FunctionExpression => Some(&diagnostics::ADD_A_RETURN_TYPE_TO_THE_FUNCTION_EXPRESSION),
        Kind::MethodDeclaration => Some(&diagnostics::ADD_A_RETURN_TYPE_TO_THE_METHOD),
        Kind::GetAccessor => Some(&diagnostics::ADD_A_RETURN_TYPE_TO_THE_GET_ACCESSOR_DECLARATION),
        Kind::SetAccessor => Some(&diagnostics::ADD_A_TYPE_TO_PARAMETER_OF_THE_SET_ACCESSOR_DECLARATION),
        Kind::FunctionDeclaration | Kind::ConstructSignature => Some(&diagnostics::ADD_A_RETURN_TYPE_TO_THE_FUNCTION_DECLARATION),
        Kind::Parameter => Some(&diagnostics::ADD_A_TYPE_ANNOTATION_TO_THE_PARAMETER_0),
        Kind::VariableDeclaration => Some(&diagnostics::ADD_A_TYPE_ANNOTATION_TO_THE_VARIABLE_0),
        Kind::PropertyDeclaration | Kind::PropertySignature => Some(&diagnostics::ADD_A_TYPE_ANNOTATION_TO_THE_PROPERTY_0),
        Kind::ExportAssignment => Some(&diagnostics::MOVE_THE_EXPRESSION_IN_DEFAULT_EXPORT_TO_A_VARIABLE_AND_ADD_A_TYPE_ANNOTATION_TO_IT),
        _ => None,
    }
}

// diagnostics.go:502
pub fn get_error_by_declaration_kind(kind: Kind) -> Option<&'static Message> {
    match kind {
        Kind::FunctionExpression | Kind::FunctionDeclaration | Kind::ArrowFunction => Some(&diagnostics::FUNCTION_MUST_HAVE_AN_EXPLICIT_RETURN_TYPE_ANNOTATION_WITH_ISOLATED_DECLARATIONS),
        Kind::MethodDeclaration | Kind::ConstructSignature => Some(&diagnostics::METHOD_MUST_HAVE_AN_EXPLICIT_RETURN_TYPE_ANNOTATION_WITH_ISOLATED_DECLARATIONS),
        Kind::GetAccessor | Kind::SetAccessor => Some(&diagnostics::AT_LEAST_ONE_ACCESSOR_MUST_HAVE_AN_EXPLICIT_TYPE_ANNOTATION_WITH_ISOLATED_DECLARATIONS),
        Kind::Parameter => Some(&diagnostics::PARAMETER_MUST_HAVE_AN_EXPLICIT_TYPE_ANNOTATION_WITH_ISOLATED_DECLARATIONS),
        Kind::VariableDeclaration => Some(&diagnostics::VARIABLE_MUST_HAVE_AN_EXPLICIT_TYPE_ANNOTATION_WITH_ISOLATED_DECLARATIONS),
        Kind::PropertyDeclaration | Kind::PropertySignature => Some(&diagnostics::PROPERTY_MUST_HAVE_AN_EXPLICIT_TYPE_ANNOTATION_WITH_ISOLATED_DECLARATIONS),
        Kind::ComputedPropertyName => Some(&diagnostics::COMPUTED_PROPERTY_NAMES_ON_CLASS_OR_OBJECT_LITERALS_CANNOT_BE_INFERRED_WITH_ISOLATED_DECLARATIONS),
        Kind::SpreadAssignment => Some(&diagnostics::OBJECTS_THAT_CONTAIN_SPREAD_ASSIGNMENTS_CAN_T_BE_INFERRED_WITH_ISOLATED_DECLARATIONS),
        Kind::ShorthandPropertyAssignment => Some(&diagnostics::OBJECTS_THAT_CONTAIN_SHORTHAND_PROPERTIES_CAN_T_BE_INFERRED_WITH_ISOLATED_DECLARATIONS),
        Kind::ArrayLiteralExpression => Some(&diagnostics::ONLY_CONST_ARRAYS_CAN_BE_INFERRED_WITH_ISOLATED_DECLARATIONS),
        Kind::ExportAssignment => Some(&diagnostics::DEFAULT_EXPORTS_CAN_T_BE_INFERRED_WITH_ISOLATED_DECLARATIONS),
        Kind::SpreadElement => Some(&diagnostics::ARRAYS_WITH_SPREAD_ELEMENTS_CAN_T_INFERRED_WITH_ISOLATED_DECLARATIONS),
        _ => None,
    }
}

// diagnostics.go:543
pub fn is_declaration_enough_for_errors(node: P<Node>) -> bool {
    ast::is_export_assignment(node)
        || ast::is_statement(node)
        || ast::is_variable_declaration(node)
        || ast::is_property_declaration(node)
        || ast::is_parameter_declaration(node)
}

// diagnostics.go:547
pub fn is_function_like_and_not_constructor(node: P<Node>) -> bool {
    ast::is_function_like_declaration(node) && !ast::is_constructor_declaration(node)
}

// diagnostics.go:551
pub fn find_nearest_declaration(node: P<Node>) -> Option<P<Node>> {
    let result = ast::find_ancestor(node, is_declaration_enough_for_errors)?;
    if ast::is_export_assignment(result) {
        return Some(result);
    }
    if ast::is_return_statement(result) {
        return ast::find_ancestor(result, is_function_like_and_not_constructor);
    }
    if ast::is_statement(result) {
        return None;
    }
    Some(result)
}

// diagnostics.go:568
pub fn create_entity_in_type_node_error(node: P<Node>) -> P<Diagnostic> {
    let diag = create_diagnostic_for_node(
        node,
        &diagnostics::TYPE_CONTAINING_PRIVATE_NAME_0_CAN_T_BE_USED_WITH_ISOLATED_DECLARATIONS,
        &[&scanner::get_text_of_node(node)],
    );
    add_parent_declaration_related_info(node, diag);
    diag
}

// diagnostics.go:574
pub fn add_parent_declaration_related_info(node: P<Node>, diag: P<Diagnostic>) {
    let Some(parent_declaration) = find_nearest_declaration(node) else {
        return;
    };
    let mut target_str = String::new();
    if !ast::is_export_assignment(parent_declaration) && parent_declaration.name().is_some() {
        target_str = scanner::get_text_of_node(parent_declaration.name().unwrap());
    }
    diag.add_related_info(create_diagnostic_for_node(
        parent_declaration,
        get_related_suggestion_by_declaration_kind(parent_declaration.kind()).unwrap(),
        &[&target_str],
    ));
}

// diagnostics.go:586
pub fn create_accessor_type_error(node: P<Node>) -> P<Diagnostic> {
    let all_declarations = ast::get_all_accessor_declarations_for_declaration(
        node,
        node.symbol().unwrap().declarations(),
    );
    let get_accessor = all_declarations.get_accessor;
    let set_accessor = all_declarations.set_accessor;
    let mut target_node = node;
    if ast::is_set_accessor_declaration(node) && !node.parameters().is_empty() {
        target_node = node.parameters()[0];
    }
    let diag = create_diagnostic_for_node(
        target_node,
        get_error_by_declaration_kind(node.kind()).unwrap(),
        &[],
    );
    if let Some(set_accessor) = set_accessor {
        diag.add_related_info(create_diagnostic_for_node(
            set_accessor,
            get_related_suggestion_by_declaration_kind(set_accessor.kind()).unwrap(),
            &[],
        ));
    }
    if let Some(get_accessor) = get_accessor {
        diag.add_related_info(create_diagnostic_for_node(
            get_accessor,
            get_related_suggestion_by_declaration_kind(get_accessor.kind()).unwrap(),
            &[],
        ));
    }
    diag
}

// diagnostics.go:604
pub fn create_object_literal_error(node: P<Node>) -> P<Diagnostic> {
    let diag =
        create_diagnostic_for_node(node, get_error_by_declaration_kind(node.kind()).unwrap(), &[]);
    add_parent_declaration_related_info(node, diag);
    diag
}

// diagnostics.go:610
pub fn create_array_literal_error(node: P<Node>) -> P<Diagnostic> {
    let diag =
        create_diagnostic_for_node(node, get_error_by_declaration_kind(node.kind()).unwrap(), &[]);
    add_parent_declaration_related_info(node, diag);
    diag
}

// diagnostics.go:616
pub fn create_return_type_error(node: P<Node>) -> P<Diagnostic> {
    let diag =
        create_diagnostic_for_node(node, get_error_by_declaration_kind(node.kind()).unwrap(), &[]);
    add_parent_declaration_related_info(node, diag);
    diag.add_related_info(create_diagnostic_for_node(
        node,
        get_related_suggestion_by_declaration_kind(node.kind()).unwrap(),
        &[],
    ));
    diag
}

// diagnostics.go:623
pub fn create_binding_element_error(node: P<Node>) -> P<Diagnostic> {
    create_diagnostic_for_node(node, &diagnostics::BINDING_ELEMENTS_WITH_INITIALIZERS_CAN_T_BE_EXPORTED_DIRECTLY_WITH_ISOLATED_DECLARATIONS, &[])
}

// diagnostics.go:627
pub fn create_variable_or_property_error(node: P<Node>) -> P<Diagnostic> {
    let diag =
        create_diagnostic_for_node(node, get_error_by_declaration_kind(node.kind()).unwrap(), &[]);
    diag.add_related_info(create_diagnostic_for_node(
        node,
        get_related_suggestion_by_declaration_kind(node.kind()).unwrap(),
        &[&scanner::get_text_of_node(node.name().unwrap())],
    ));
    diag
}

// diagnostics.go:633
pub fn create_expression_error(node: P<Node>) -> P<Diagnostic> {
    create_expression_error_ex(node, None)
}

// diagnostics.go:637
pub fn create_class_expression_error(node: P<Node>) -> P<Diagnostic> {
    create_expression_error_ex(node, Some(&diagnostics::INFERENCE_FROM_CLASS_EXPRESSIONS_IS_NOT_SUPPORTED_WITH_ISOLATED_DECLARATIONS))
}

// diagnostics.go:641
pub fn is_parent_for_idd_iagnostic(node: P<Node>) -> FindAncestorResult {
    if ast::is_export_assignment(node) {
        return FindAncestorResult::True;
    }
    if ast::is_statement(node) {
        return FindAncestorResult::Quit;
    }
    ast::to_find_ancestor_result(
        !ast::is_parenthesized_expression(node) && !ast::is_assertion_expression(node),
    )
}

// diagnostics.go:651
pub fn create_expression_error_ex(
    node: P<Node>,
    mut diagnostic_message: Option<&'static Message>,
) -> P<Diagnostic> {
    let Some(parent_declaration) = find_nearest_declaration(node) else {
        if diagnostic_message.is_none() {
            diagnostic_message =
                Some(&diagnostics::EXPRESSION_TYPE_CAN_T_BE_INFERRED_WITH_ISOLATED_DECLARATIONS);
        }
        return create_diagnostic_for_node(node, diagnostic_message.unwrap(), &[]);
    };

    let mut target_str = String::new();
    if !ast::is_export_assignment(parent_declaration) && parent_declaration.name().is_some() {
        target_str = scanner::get_text_of_node(parent_declaration.name().unwrap());
    }
    let parent = ast::find_ancestor_or_quit(node.parent(), is_parent_for_idd_iagnostic);

    if Some(parent_declaration) == parent {
        if diagnostic_message.is_none() {
            diagnostic_message = get_error_by_declaration_kind(parent_declaration.kind());
        }
        let diag = create_diagnostic_for_node(node, diagnostic_message.unwrap(), &[]);
        diag.add_related_info(create_diagnostic_for_node(
            parent_declaration,
            get_related_suggestion_by_declaration_kind(parent_declaration.kind()).unwrap(),
            &[&target_str],
        ));
        return diag;
    }
    if diagnostic_message.is_none() {
        diagnostic_message =
            Some(&diagnostics::EXPRESSION_TYPE_CAN_T_BE_INFERRED_WITH_ISOLATED_DECLARATIONS);
    }
    let diag = create_diagnostic_for_node(node, diagnostic_message.unwrap(), &[]);
    diag.add_related_info(create_diagnostic_for_node(
        parent_declaration,
        get_related_suggestion_by_declaration_kind(parent_declaration.kind()).unwrap(),
        &[&target_str],
    ));
    diag.add_related_info(create_diagnostic_for_node(node, &diagnostics::ADD_SATISFIES_AND_A_TYPE_ASSERTION_TO_THIS_EXPRESSION_SATISFIES_T_AS_T_TO_MAKE_THE_TYPE_EXPLICIT, &[]));
    diag
}

// diagnostics.go:683
pub fn create_get_isolated_declaration_errors(resolver: Resolver) -> GetIsolatedDeclarationError {
    let create_parameter_error = move |c: &mut Checker, node: P<Node>| -> P<Diagnostic> {
        if ast::is_set_accessor_declaration(node.parent().unwrap()) {
            return create_accessor_type_error(node.parent().unwrap());
        }
        let add_undefined = resolver.requires_adding_implicit_undefined_unsafe(c, node, None, None); // skip checker lock - node builder will already have one
        if !add_undefined && node.initializer().is_some() {
            return create_expression_error(node.initializer().unwrap());
        }
        let mut message = get_error_by_declaration_kind(node.kind());
        if add_undefined {
            message = Some(&diagnostics::DECLARATION_EMIT_FOR_THIS_PARAMETER_REQUIRES_IMPLICITLY_ADDING_UNDEFINED_TO_ITS_TYPE_THIS_IS_NOT_SUPPORTED_WITH_ISOLATED_DECLARATIONS);
        }
        let diag = create_diagnostic_for_node(node, message.unwrap(), &[]);
        let target_str = scanner::get_text_of_node(node.name().unwrap());
        diag.add_related_info(create_diagnostic_for_node(
            node,
            get_related_suggestion_by_declaration_kind(node.kind()).unwrap(),
            &[&target_str],
        ));
        diag
    };

    Rc::new(move |c: &mut Checker, node: P<Node>| -> P<Diagnostic> {
        let heritage_clause = ast::find_ancestor(node, ast::is_heritage_clause);
        if heritage_clause.is_some() {
            return create_diagnostic_for_node(
                node,
                &diagnostics::EXTENDS_CLAUSE_CAN_T_CONTAIN_AN_EXPRESSION_WITH_ISOLATED_DECLARATIONS,
                &[],
            );
        }
        if ast::is_part_of_type_node(node) || ast::is_type_query_node(node) {
            return create_entity_in_type_node_error(node);
        }
        if ast::is_entity_name(node) || ast::is_entity_name_expression(node) {
            return create_entity_in_type_node_error(node);
        }
        match node.kind() {
            Kind::GetAccessor | Kind::SetAccessor => create_accessor_type_error(node),
            Kind::ComputedPropertyName
            | Kind::ShorthandPropertyAssignment
            | Kind::SpreadAssignment => create_object_literal_error(node),
            Kind::ArrayLiteralExpression | Kind::SpreadElement => create_array_literal_error(node),
            Kind::MethodDeclaration
            | Kind::ConstructSignature
            | Kind::FunctionExpression
            | Kind::ArrowFunction
            | Kind::FunctionDeclaration => create_return_type_error(node),
            Kind::BindingElement => create_binding_element_error(node),
            Kind::PropertyDeclaration | Kind::VariableDeclaration => {
                create_variable_or_property_error(node)
            }
            Kind::Parameter => create_parameter_error(c, node),
            Kind::PropertyAssignment => create_expression_error(node.initializer().unwrap()),
            Kind::ClassExpression => create_class_expression_error(node),
            _ => create_expression_error(node),
        }
    })
}
