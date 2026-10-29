use crate::*;
use rustc_hash::FxHashMap;
use std::fmt::Display;
use tsrs_ast as ast;
use tsrs_ast::*;
use tsrs_binder as binder;
use tsrs_core::{TextRange, stringutil, tspath};
use tsrs_diagnostics as diagnostics;
use tsrs_diagnostics::Message;
use tsrs_scanner as scanner;

impl Checker {
    // grammarchecks.go:19
    pub(crate) fn grammar_error_on_first_token(
        &mut self,
        node: P<Node>,
        message: &'static Message,
        args: &[&dyn Display],
    ) -> bool {
        let source_file = ast::get_source_file_of_node(node).unwrap();
        if !Self::has_parse_diagnostics(source_file) {
            let span = scanner::get_range_of_token_at_position(
                source_file,
                node.pos().expect_source("parsed nodes have source positions"),
            );
            self.add_diagnostic(ast::new_diagnostic(Some(source_file), span, message, args));
            return true;
        }
        false
    }

    // grammarchecks.go:29
    pub(crate) fn grammar_error_at_pos(
        &mut self,
        node_for_source_file: P<Node>,
        start: u32,
        length: u32,
        message: &'static Message,
        args: &[&dyn Display],
    ) -> bool {
        let source_file = ast::get_source_file_of_node(node_for_source_file).unwrap();
        if !Self::has_parse_diagnostics(source_file) {
            self.add_diagnostic(ast::new_diagnostic(
                Some(source_file),
                TextRange::new(
                    start,
                    start.checked_add(length).expect("diagnostic range overflow"),
                ),
                message,
                args,
            ));
            return true;
        }
        false
    }

    // grammarchecks.go:38
    pub(crate) fn grammar_error_on_node(
        &mut self,
        node: P<Node>,
        message: &'static Message,
        args: &[&dyn Display],
    ) -> bool {
        let source_file = ast::get_source_file_of_node(node).unwrap();
        if !Self::has_parse_diagnostics(source_file) {
            self.error(Some(node), message, args);
            return true;
        }
        false
    }

    // grammarchecks.go:47
    fn grammar_error_on_node_skipped_on_no_emit(
        &mut self,
        node: P<Node>,
        message: &'static Message,
        args: &[&dyn Display],
    ) -> bool {
        let source_file = ast::get_source_file_of_node(node).unwrap();
        if !Self::has_parse_diagnostics(source_file) {
            let d = new_diagnostic_for_node(Some(node), Some(message), args);
            d.set_skipped_on_no_emit();
            self.add_diagnostic(d);
            return true;
        }
        false
    }
}

// grammarchecks.go:58
#[expect(
    clippy::redundant_pub_crate,
    reason = "Only the public API is re-exported; implementation helpers remain crate-private."
)]
pub(crate) fn get_identifier_from_entity_name_expression(node: P<Node>) -> Option<P<Node>> {
    match node.kind() {
        Kind::Identifier => Some(node),
        Kind::PropertyAccessExpression => Some(node.as_property_access_expression().name),
        _ => None,
    }
}

// Body of the scanner error callback installed by checkGrammarRegularExpressionLiteral. The Rust scanner buffers
// errors instead of calling back, so they are drained (in report order) right after every scanner call.
fn on_reg_exp_scanner_errors(
    c: &mut Checker,
    s: &mut Scanner,
    source_file: P<SourceFile>,
    last_error: &mut Option<P<Diagnostic>>,
) {
    for e in s.take_errors() {
        let (message, start, length) = (e.message, e.start, e.length);
        let args: Vec<&dyn Display> = e.args.iter().map(|a| a as &dyn Display).collect();
        if message.category() == diagnostics::Category::Message
            && last_error.is_some_and(|le| Some(start) == le.pos() && Some(length) == le.len())
        {
            // For providing spelling suggestions.
            let err =
                ast::new_diagnostic(None, TextRange::new(start, start + length), message, &args);
            last_error.unwrap().add_related_info(err);
        } else if last_error.is_none_or(|le| Some(start) != le.pos()) {
            let d = ast::new_diagnostic(
                Some(source_file),
                TextRange::new(start, start + length),
                message,
                &args,
            );
            *last_error = Some(c.add_diagnostic(d));
        }
    }
}

impl Checker {
    // grammarchecks.go:69
    pub(crate) fn check_grammar_regular_expression_literal(&mut self, node: P<Node>) -> bool {
        let source_file = ast::get_source_file_of_node(node).unwrap();
        if !Self::has_parse_diagnostics(source_file) {
            let mut last_error: Option<P<Diagnostic>> = None;
            if self.reg_exp_scanner.is_none() {
                self.reg_exp_scanner = Some(Box::new(Scanner::new()));
            }
            let mut s = self.reg_exp_scanner.take().unwrap();
            s.set_script_target(self.language_version);
            s.set_language_variant(source_file.language_variant());
            s.set_on_error(true);
            s.set_text(source_file.text());
            s.reset_token_state(node.pos().expect_source("parsed nodes have source positions"));
            s.scan();
            on_reg_exp_scanner_errors(self, &mut s, source_file, &mut last_error);
            let token_is_regular_expression_literal =
                s.re_scan_slash_token(true) == Kind::RegularExpressionLiteral;
            on_reg_exp_scanner_errors(self, &mut s, source_file, &mut last_error);
            s.set_text("");
            s.set_on_error(false);
            self.reg_exp_scanner = Some(s);
            assert!(token_is_regular_expression_literal);
            return last_error.is_some();
        }
        false
    }

    // grammarchecks.go:100
    pub(crate) fn check_grammar_private_identifier_expression(&mut self, priv_id: P<Node>) -> bool {
        let priv_id_as_node = priv_id;
        if ast::get_containing_class(priv_id).is_none() {
            return self.grammar_error_on_node(
                priv_id,
                &diagnostics::PRIVATE_IDENTIFIERS_ARE_NOT_ALLOWED_OUTSIDE_CLASS_BODIES,
                &[],
            );
        }

        let parent = priv_id.parent().unwrap();
        if !is_for_in_statement(parent) {
            if !ast::is_expression_node(priv_id_as_node) {
                return self.grammar_error_on_node(priv_id_as_node, &diagnostics::PRIVATE_IDENTIFIERS_ARE_ONLY_ALLOWED_IN_CLASS_BODIES_AND_MAY_ONLY_BE_USED_AS_PART_OF_A_CLASS_MEMBER_DECLARATION_PROPERTY_ACCESS_OR_ON_THE_LEFT_HAND_SIDE_OF_AN_IN_EXPRESSION, &[]);
            }

            let is_in_operation = is_binary_expression(parent)
                && parent.as_binary_expression().operator_token.kind() == Kind::InKeyword;
            if self.get_symbol_for_private_identifier_expression(priv_id_as_node).is_none()
                && !is_in_operation
            {
                return self.grammar_error_on_node(
                    priv_id_as_node,
                    &diagnostics::CANNOT_FIND_NAME_0,
                    &[&priv_id.text()],
                );
            }
        }

        false
    }

    // grammarchecks.go:120
    pub(crate) fn check_grammar_mapped_type(&mut self, node: P<Node>) -> bool {
        let members = node.as_mapped_type_node().members.unwrap();
        if !members.nodes().is_empty() {
            return self.grammar_error_on_node(
                members.nodes()[0],
                &diagnostics::A_MAPPED_TYPE_MAY_NOT_DECLARE_PROPERTIES_OR_METHODS,
                &[],
            );
        }
        false
    }

    // grammarchecks.go:127
    pub(crate) fn check_grammar_decorator(&mut self, decorator: P<Node>) -> bool {
        let source_file = ast::get_source_file_of_node(decorator).unwrap();
        if !Self::has_parse_diagnostics(source_file) {
            let mut node = decorator.as_decorator().expression;

            // DecoratorParenthesizedExpression :
            //   `(` Expression `)`

            if is_parenthesized_expression(node) {
                return false;
            }

            let mut can_have_call_expression = true;
            let mut error_node: Option<P<Node>> = None;
            loop {
                // Allow TS syntax such as non-null assertions and instantiation expressions
                if is_expression_with_type_arguments(node) || is_non_null_expression(node) {
                    node = node.expression().unwrap();
                    continue;
                }

                // DecoratorCallExpression :
                //   DecoratorMemberExpression Arguments

                if is_call_expression(node) {
                    let call_expr = node.as_call_expression();
                    if !can_have_call_expression {
                        error_node = Some(node);
                    }
                    if call_expr.question_dot_token().is_some() {
                        // Even if we already have an error node, error at the `?.` token since it appears earlier.
                        error_node = call_expr.question_dot_token();
                    }
                    node = call_expr.expression;
                    can_have_call_expression = false;
                    continue;
                }

                // DecoratorMemberExpression :
                //   IdentifierReference
                //   DecoratorMemberExpression `.` IdentifierName
                //   DecoratorMemberExpression `.` PrivateIdentifier

                if is_property_access_expression(node) {
                    let property_access_expr = node.as_property_access_expression();
                    if property_access_expr.question_dot_token().is_some() {
                        // Even if we already have an error node, error at the `?.` token since it appears earlier.
                        error_node = property_access_expr.question_dot_token();
                    }
                    node = property_access_expr.expression;
                    can_have_call_expression = false;
                    continue;
                }

                if !is_identifier(node) {
                    // Even if we already have an error node, error at this node since it appears earlier.
                    error_node = Some(node);
                }

                break;
            }

            if let Some(error_node) = error_node {
                let err = self.error(Some(decorator.as_decorator().expression), &diagnostics::EXPRESSION_MUST_BE_ENCLOSED_IN_PARENTHESES_TO_BE_USED_AS_A_DECORATOR, &[]);
                err.add_related_info(create_diagnostic_for_node(
                    Some(error_node),
                    &diagnostics::INVALID_SYNTAX_IN_DECORATOR,
                    &[],
                ));
                return true;
            }
        }

        false
    }

    // grammarchecks.go:199
    pub(crate) fn check_grammar_export_declaration(&mut self, node: P<Node>) -> bool {
        let export_decl = node.as_export_declaration();
        if export_decl.is_type_only
            && let Some(export_clause) = export_decl.export_clause
            && export_clause.kind() == Kind::NamedExports
        {
            return self.check_grammar_type_only_named_imports_or_exports(export_clause);
        }
        false
    }

    // grammarchecks.go:206
    pub(crate) fn check_grammar_module_element_context(
        &mut self,
        node: P<Node>,
        error_message: &'static Message,
    ) -> bool {
        let parent_kind = node.parent().unwrap().kind();
        let is_in_appropriate_context = parent_kind == Kind::SourceFile
            || parent_kind == Kind::ModuleBlock
            || parent_kind == Kind::ModuleDeclaration;
        if !is_in_appropriate_context {
            self.grammar_error_on_first_token(node, error_message, &[]);
        }
        !is_in_appropriate_context
    }

    // grammarchecks.go:214
    pub(crate) fn check_grammar_modifiers(&mut self, node: P<Node>) -> bool {
        if node.modifiers().is_none() {
            return false;
        }
        if self.report_obvious_decorator_errors(node) || self.report_obvious_modifier_errors(node) {
            return true;
        }
        if ast::is_this_parameter(node) {
            return self.grammar_error_on_first_token(
                node,
                &diagnostics::NEITHER_DECORATORS_NOR_MODIFIERS_MAY_BE_APPLIED_TO_THIS_PARAMETERS,
                &[],
            );
        }
        let mut block_scope_kind = NodeFlags::None;
        if is_variable_statement(node) {
            block_scope_kind =
                node.as_variable_statement().declaration_list.flags() & NodeFlags::BlockScoped;
        }
        let mut last_static: Option<P<Node>> = None;
        let mut last_declare: Option<P<Node>> = None;
        let mut last_async: Option<P<Node>> = None;
        let mut last_override: Option<P<Node>> = None;
        let mut first_decorator: Option<P<Node>> = None;
        let mut flags = ModifierFlags::None;
        let mut saw_export_before_decorators = false;
        // We parse decorators and modifiers in four contiguous chunks:
        // [...leadingDecorators, ...leadingModifiers, ...trailingDecorators, ...trailingModifiers]. It is an error to
        // have both leading and trailing decorators.
        let mut has_leading_decorators = false;
        let parent = node.parent().unwrap();
        let modifiers = node.modifier_nodes();
        for &modifier in modifiers {
            if is_decorator(modifier) {
                if !ast::node_can_be_decorated(
                    self.legacy_decorators,
                    node,
                    Some(parent),
                    parent.parent(),
                ) {
                    if node.kind() == Kind::MethodDeclaration && !ast::node_is_present(node.body())
                    {
                        return self.grammar_error_on_first_token(node, &diagnostics::A_DECORATOR_CAN_ONLY_DECORATE_A_METHOD_IMPLEMENTATION_NOT_AN_OVERLOAD, &[]);
                    }
                    return self.grammar_error_on_first_token(
                        node,
                        &diagnostics::DECORATORS_ARE_NOT_VALID_HERE,
                        &[],
                    );
                } else if self.legacy_decorators
                    && (node.kind() == Kind::GetAccessor || node.kind() == Kind::SetAccessor)
                {
                    let symbol = self.get_symbol_of_declaration(node).unwrap();
                    let declarations: Vec<P<Node>> = symbol.declarations().to_vec();
                    let accessors =
                        ast::get_all_accessor_declarations_for_declaration(node, &declarations);
                    if ast::has_decorators(accessors.first_accessor)
                        && Some(node) == accessors.second_accessor
                    {
                        return self.grammar_error_on_first_token(node, &diagnostics::DECORATORS_CANNOT_BE_APPLIED_TO_MULTIPLE_GET_SLASHSET_ACCESSORS_OF_THE_SAME_NAME, &[]);
                    }
                }

                // if we've seen any modifiers aside from `export`, `default`, or another decorator, then this is an invalid position
                if flags.intersects(!(ModifierFlags::ExportDefault | ModifierFlags::Decorator)) {
                    return self.grammar_error_on_node(
                        modifier,
                        &diagnostics::DECORATORS_ARE_NOT_VALID_HERE,
                        &[],
                    );
                }

                // if we've already seen leading decorators and leading modifiers, then trailing decorators are an invalid position
                if has_leading_decorators && flags.intersects(ModifierFlags::Modifier) {
                    assert!(first_decorator.is_some(), "Expected firstDecorator to be set");
                    let source_file = ast::get_source_file_of_node(modifier).unwrap();
                    if !Self::has_parse_diagnostics(source_file) {
                        let err = self.error(Some(modifier), &diagnostics::DECORATORS_MAY_NOT_APPEAR_AFTER_EXPORT_OR_EXPORT_DEFAULT_IF_THEY_ALSO_APPEAR_BEFORE_EXPORT, &[]);
                        err.add_related_info(create_diagnostic_for_node(
                            first_decorator,
                            &diagnostics::DECORATOR_USED_BEFORE_EXPORT_HERE,
                            &[],
                        ));
                        return true;
                    }
                    return false;
                }

                flags |= ModifierFlags::Decorator;

                // if we have not yet seen a modifier, then these are leading decorators
                if !flags.intersects(ModifierFlags::Modifier) {
                    has_leading_decorators = true;
                } else if flags.intersects(ModifierFlags::Export) {
                    saw_export_before_decorators = true;
                }

                if first_decorator.is_none() {
                    first_decorator = Some(modifier);
                }
            } else {
                let modifier_not_reparsed = !modifier.flags().intersects(NodeFlags::Reparsed);
                if modifier.kind() != Kind::ReadonlyKeyword {
                    if node.kind() == Kind::PropertySignature
                        || node.kind() == Kind::MethodSignature
                    {
                        return self.grammar_error_on_node(
                            modifier,
                            &diagnostics::X_0_MODIFIER_CANNOT_APPEAR_ON_A_TYPE_MEMBER,
                            &[&scanner::token_to_string(modifier.kind())],
                        );
                    }
                    if node.kind() == Kind::IndexSignature
                        && (modifier.kind() != Kind::StaticKeyword || !ast::is_class_like(parent))
                    {
                        return self.grammar_error_on_node(
                            modifier,
                            &diagnostics::X_0_MODIFIER_CANNOT_APPEAR_ON_AN_INDEX_SIGNATURE,
                            &[&scanner::token_to_string(modifier.kind())],
                        );
                    }
                }
                if modifier.kind() != Kind::InKeyword
                    && modifier.kind() != Kind::OutKeyword
                    && modifier.kind() != Kind::ConstKeyword
                    && node.kind() == Kind::TypeParameter
                {
                    return self.grammar_error_on_node(
                        modifier,
                        &diagnostics::X_0_MODIFIER_CANNOT_APPEAR_ON_A_TYPE_PARAMETER,
                        &[&scanner::token_to_string(modifier.kind())],
                    );
                }
                match modifier.kind() {
                    Kind::ConstKeyword => {
                        if node.kind() != Kind::EnumDeclaration
                            && node.kind() != Kind::TypeParameter
                        {
                            return self.grammar_error_on_node(
                                node,
                                &diagnostics::A_CLASS_MEMBER_CANNOT_HAVE_THE_0_KEYWORD,
                                &[&scanner::token_to_string(Kind::ConstKeyword)],
                            );
                        }
                        if node.kind() == Kind::TypeParameter
                            && !(ast::is_function_like_declaration(parent)
                                || ast::is_class_like(parent)
                                || is_function_type_node(parent)
                                || is_constructor_type_node(parent)
                                || is_call_signature_declaration(parent)
                                || is_construct_signature_declaration(parent)
                                || ast::is_method_signature_declaration(parent))
                        {
                            return self.grammar_error_on_node(modifier, &diagnostics::X_0_MODIFIER_CAN_ONLY_APPEAR_ON_A_TYPE_PARAMETER_OF_A_FUNCTION_METHOD_OR_CLASS, &[&scanner::token_to_string(modifier.kind())]);
                        }
                    }
                    Kind::OverrideKeyword => {
                        // If node.kind === SyntaxKind.Parameter, checkParameter reports an error if it's not a parameter property.
                        if flags.intersects(ModifierFlags::Override) {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_ALREADY_SEEN,
                                &[&"override"],
                            );
                        } else if flags.intersects(ModifierFlags::Ambient) {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_CANNOT_BE_USED_WITH_1_MODIFIER,
                                &[&"override", &"declare"],
                            );
                        } else if flags.intersects(ModifierFlags::Readonly) && modifier_not_reparsed
                        {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_MUST_PRECEDE_1_MODIFIER,
                                &[&"override", &"readonly"],
                            );
                        } else if flags.intersects(ModifierFlags::Accessor) && modifier_not_reparsed
                        {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_MUST_PRECEDE_1_MODIFIER,
                                &[&"override", &"accessor"],
                            );
                        } else if flags.intersects(ModifierFlags::Async) && modifier_not_reparsed {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_MUST_PRECEDE_1_MODIFIER,
                                &[&"override", &"async"],
                            );
                        }
                        flags |= ModifierFlags::Override;
                        last_override = Some(modifier);
                    }
                    Kind::PublicKeyword | Kind::ProtectedKeyword | Kind::PrivateKeyword => {
                        let text = visibility_to_string(ast::modifier_to_flag(modifier.kind()));

                        if flags.intersects(ModifierFlags::AccessibilityModifier) {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::ACCESSIBILITY_MODIFIER_ALREADY_SEEN,
                                &[],
                            );
                        } else if flags.intersects(ModifierFlags::Override) && modifier_not_reparsed
                        {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_MUST_PRECEDE_1_MODIFIER,
                                &[&text, &"override"],
                            );
                        } else if flags.intersects(ModifierFlags::Static) && modifier_not_reparsed {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_MUST_PRECEDE_1_MODIFIER,
                                &[&text, &"static"],
                            );
                        } else if flags.intersects(ModifierFlags::Accessor) && modifier_not_reparsed
                        {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_MUST_PRECEDE_1_MODIFIER,
                                &[&text, &"accessor"],
                            );
                        } else if flags.intersects(ModifierFlags::Readonly) && modifier_not_reparsed
                        {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_MUST_PRECEDE_1_MODIFIER,
                                &[&text, &"readonly"],
                            );
                        } else if flags.intersects(ModifierFlags::Async) && modifier_not_reparsed {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_MUST_PRECEDE_1_MODIFIER,
                                &[&text, &"async"],
                            );
                        } else if parent.kind() == Kind::ModuleBlock
                            || parent.kind() == Kind::SourceFile
                        {
                            return self.grammar_error_on_node(modifier, &diagnostics::X_0_MODIFIER_CANNOT_APPEAR_ON_A_MODULE_OR_NAMESPACE_ELEMENT, &[&text]);
                        } else if flags.intersects(ModifierFlags::Abstract) {
                            if modifier.kind() == Kind::PrivateKeyword {
                                return self.grammar_error_on_node(
                                    modifier,
                                    &diagnostics::X_0_MODIFIER_CANNOT_BE_USED_WITH_1_MODIFIER,
                                    &[&text, &"abstract"],
                                );
                            } else if modifier_not_reparsed {
                                return self.grammar_error_on_node(
                                    modifier,
                                    &diagnostics::X_0_MODIFIER_MUST_PRECEDE_1_MODIFIER,
                                    &[&text, &"abstract"],
                                );
                            }
                        } else if ast::is_private_identifier_class_element_declaration(node) {
                            return self.grammar_error_on_node(modifier, &diagnostics::AN_ACCESSIBILITY_MODIFIER_CANNOT_BE_USED_WITH_A_PRIVATE_IDENTIFIER, &[]);
                        }
                        flags |= ast::modifier_to_flag(modifier.kind());
                    }
                    Kind::StaticKeyword => {
                        if flags.intersects(ModifierFlags::Static) {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_ALREADY_SEEN,
                                &[&"static"],
                            );
                        } else if flags.intersects(ModifierFlags::Readonly) && modifier_not_reparsed
                        {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_MUST_PRECEDE_1_MODIFIER,
                                &[&"static", &"readonly"],
                            );
                        } else if flags.intersects(ModifierFlags::Async) && modifier_not_reparsed {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_MUST_PRECEDE_1_MODIFIER,
                                &[&"static", &"async"],
                            );
                        } else if flags.intersects(ModifierFlags::Accessor) && modifier_not_reparsed
                        {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_MUST_PRECEDE_1_MODIFIER,
                                &[&"static", &"accessor"],
                            );
                        } else if parent.kind() == Kind::ModuleBlock
                            || parent.kind() == Kind::SourceFile
                        {
                            return self.grammar_error_on_node(modifier, &diagnostics::X_0_MODIFIER_CANNOT_APPEAR_ON_A_MODULE_OR_NAMESPACE_ELEMENT, &[&"static"]);
                        } else if node.kind() == Kind::Parameter {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_CANNOT_APPEAR_ON_A_PARAMETER,
                                &[&"static"],
                            );
                        } else if flags.intersects(ModifierFlags::Abstract) {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_CANNOT_BE_USED_WITH_1_MODIFIER,
                                &[&"static", &"abstract"],
                            );
                        } else if flags.intersects(ModifierFlags::Override) && modifier_not_reparsed
                        {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_MUST_PRECEDE_1_MODIFIER,
                                &[&"static", &"override"],
                            );
                        }
                        flags |= ModifierFlags::Static;
                        last_static = Some(modifier);
                    }
                    Kind::AccessorKeyword => {
                        if flags.intersects(ModifierFlags::Accessor) {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_ALREADY_SEEN,
                                &[&"accessor"],
                            );
                        } else if flags.intersects(ModifierFlags::Readonly) {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_CANNOT_BE_USED_WITH_1_MODIFIER,
                                &[&"accessor", &"readonly"],
                            );
                        } else if flags.intersects(ModifierFlags::Ambient) {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_CANNOT_BE_USED_WITH_1_MODIFIER,
                                &[&"accessor", &"declare"],
                            );
                        } else if node.kind() != Kind::PropertyDeclaration {
                            return self.grammar_error_on_node(modifier, &diagnostics::X_ACCESSOR_MODIFIER_CAN_ONLY_APPEAR_ON_A_PROPERTY_DECLARATION, &[]);
                        }

                        flags |= ModifierFlags::Accessor;
                    }
                    Kind::ReadonlyKeyword => {
                        if flags.intersects(ModifierFlags::Readonly) {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_ALREADY_SEEN,
                                &[&"readonly"],
                            );
                        } else if node.kind() != Kind::PropertyDeclaration
                            && node.kind() != Kind::PropertySignature
                            && node.kind() != Kind::IndexSignature
                            && node.kind() != Kind::Parameter
                        {
                            // If node.kind === SyntaxKind.Parameter, checkParameter reports an error if it's not a parameter property.
                            return self.grammar_error_on_node(modifier, &diagnostics::X_READONLY_MODIFIER_CAN_ONLY_APPEAR_ON_A_PROPERTY_DECLARATION_OR_INDEX_SIGNATURE, &[]);
                        } else if flags.intersects(ModifierFlags::Accessor) {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_CANNOT_BE_USED_WITH_1_MODIFIER,
                                &[&"readonly", &"accessor"],
                            );
                        }
                        flags |= ModifierFlags::Readonly;
                    }
                    Kind::ExportKeyword => {
                        if self.compiler_options.verbatim_module_syntax == Tristate::True
                            && !node.flags().intersects(NodeFlags::Ambient)
                            && node.kind() != Kind::TypeAliasDeclaration
                            && node.kind() != Kind::InterfaceDeclaration
                            && node.kind() != Kind::ModuleDeclaration
                            && parent.kind() == Kind::SourceFile
                            && self.emit_module_format_of_file(
                                ast::get_source_file_of_node(node).unwrap(),
                            ) == ModuleKind::COMMON_JS
                        {
                            return self.grammar_error_on_node(modifier, &diagnostics::A_TOP_LEVEL_EXPORT_MODIFIER_CANNOT_BE_USED_ON_VALUE_DECLARATIONS_IN_A_COMMON_JS_MODULE_WHEN_VERBATIM_MODULE_SYNTAX_IS_ENABLED, &[]);
                        }
                        if flags.intersects(ModifierFlags::Export) {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_ALREADY_SEEN,
                                &[&"export"],
                            );
                        } else if flags.intersects(ModifierFlags::Ambient) && modifier_not_reparsed
                        {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_MUST_PRECEDE_1_MODIFIER,
                                &[&"export", &"declare"],
                            );
                        } else if flags.intersects(ModifierFlags::Abstract) && modifier_not_reparsed
                        {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_MUST_PRECEDE_1_MODIFIER,
                                &[&"export", &"abstract"],
                            );
                        } else if flags.intersects(ModifierFlags::Async) && modifier_not_reparsed {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_MUST_PRECEDE_1_MODIFIER,
                                &[&"export", &"async"],
                            );
                        } else if ast::is_class_like(parent) && !is_js_type_alias_declaration(node)
                        {
                            return self.grammar_error_on_node(modifier, &diagnostics::X_0_MODIFIER_CANNOT_APPEAR_ON_CLASS_ELEMENTS_OF_THIS_KIND, &[&"export"]);
                        } else if node.kind() == Kind::Parameter {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_CANNOT_APPEAR_ON_A_PARAMETER,
                                &[&"export"],
                            );
                        } else if block_scope_kind == NodeFlags::Using {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_CANNOT_APPEAR_ON_A_USING_DECLARATION,
                                &[&"export"],
                            );
                        } else if block_scope_kind == NodeFlags::AwaitUsing {
                            return self.grammar_error_on_node(modifier, &diagnostics::X_0_MODIFIER_CANNOT_APPEAR_ON_AN_AWAIT_USING_DECLARATION, &[&"export"]);
                        }
                        flags |= ModifierFlags::Export;
                    }
                    Kind::DefaultKeyword => {
                        let container = if parent.kind() == Kind::SourceFile {
                            parent
                        } else {
                            parent.parent().unwrap()
                        };
                        if container.kind() == Kind::ModuleDeclaration
                            && !ast::is_ambient_module(container)
                        {
                            return self.grammar_error_on_node(modifier, &diagnostics::A_DEFAULT_EXPORT_CAN_ONLY_BE_USED_IN_AN_ECMA_SCRIPT_STYLE_MODULE, &[]);
                        } else if block_scope_kind == NodeFlags::Using {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_CANNOT_APPEAR_ON_A_USING_DECLARATION,
                                &[&"default"],
                            );
                        } else if block_scope_kind == NodeFlags::AwaitUsing {
                            return self.grammar_error_on_node(modifier, &diagnostics::X_0_MODIFIER_CANNOT_APPEAR_ON_AN_AWAIT_USING_DECLARATION, &[&"default"]);
                        } else if !flags.intersects(ModifierFlags::Export) && modifier_not_reparsed
                        {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_MUST_PRECEDE_1_MODIFIER,
                                &[&"export", &"default"],
                            );
                        } else if saw_export_before_decorators {
                            return self.grammar_error_on_node(
                                first_decorator.unwrap(),
                                &diagnostics::DECORATORS_ARE_NOT_VALID_HERE,
                                &[],
                            );
                        }

                        flags |= ModifierFlags::Default;
                    }
                    Kind::DeclareKeyword => {
                        if flags.intersects(ModifierFlags::Ambient) {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_ALREADY_SEEN,
                                &[&"declare"],
                            );
                        } else if flags.intersects(ModifierFlags::Async) {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_CANNOT_BE_USED_IN_AN_AMBIENT_CONTEXT,
                                &[&"async"],
                            );
                        } else if flags.intersects(ModifierFlags::Override) {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_CANNOT_BE_USED_IN_AN_AMBIENT_CONTEXT,
                                &[&"override"],
                            );
                        } else if ast::is_class_like(parent) && !is_property_declaration(node) {
                            return self.grammar_error_on_node(modifier, &diagnostics::X_0_MODIFIER_CANNOT_APPEAR_ON_CLASS_ELEMENTS_OF_THIS_KIND, &[&"declare"]);
                        } else if node.kind() == Kind::Parameter {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_CANNOT_APPEAR_ON_A_PARAMETER,
                                &[&"declare"],
                            );
                        } else if block_scope_kind == NodeFlags::Using {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_CANNOT_APPEAR_ON_A_USING_DECLARATION,
                                &[&"declare"],
                            );
                        } else if block_scope_kind == NodeFlags::AwaitUsing {
                            return self.grammar_error_on_node(modifier, &diagnostics::X_0_MODIFIER_CANNOT_APPEAR_ON_AN_AWAIT_USING_DECLARATION, &[&"declare"]);
                        } else if parent.flags().intersects(NodeFlags::Ambient)
                            && parent.kind() == Kind::ModuleBlock
                        {
                            return self.grammar_error_on_node(modifier, &diagnostics::A_DECLARE_MODIFIER_CANNOT_BE_USED_IN_AN_ALREADY_AMBIENT_CONTEXT, &[]);
                        } else if ast::is_private_identifier_class_element_declaration(node) {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_CANNOT_BE_USED_WITH_A_PRIVATE_IDENTIFIER,
                                &[&"declare"],
                            );
                        } else if flags.intersects(ModifierFlags::Accessor) {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_CANNOT_BE_USED_WITH_1_MODIFIER,
                                &[&"declare", &"accessor"],
                            );
                        }
                        flags |= ModifierFlags::Ambient;
                        last_declare = Some(modifier);
                    }
                    Kind::AbstractKeyword => {
                        if flags.intersects(ModifierFlags::Abstract) {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_ALREADY_SEEN,
                                &[&"abstract"],
                            );
                        }
                        if node.kind() != Kind::ClassDeclaration
                            && node.kind() != Kind::ConstructorType
                        {
                            if node.kind() != Kind::MethodDeclaration
                                && node.kind() != Kind::PropertyDeclaration
                                && node.kind() != Kind::GetAccessor
                                && node.kind() != Kind::SetAccessor
                            {
                                return self.grammar_error_on_node(modifier, &diagnostics::X_ABSTRACT_MODIFIER_CAN_ONLY_APPEAR_ON_A_CLASS_METHOD_OR_PROPERTY_DECLARATION, &[]);
                            }
                            if !(parent.kind() == Kind::ClassDeclaration
                                && ast::has_syntactic_modifier(parent, ModifierFlags::Abstract))
                            {
                                let message = if node.kind() == Kind::PropertyDeclaration {
                                    &diagnostics::ABSTRACT_PROPERTIES_CAN_ONLY_APPEAR_WITHIN_AN_ABSTRACT_CLASS
                                } else {
                                    &diagnostics::ABSTRACT_METHODS_CAN_ONLY_APPEAR_WITHIN_AN_ABSTRACT_CLASS
                                };
                                return self.grammar_error_on_node(modifier, message, &[]);
                            }
                            if flags.intersects(ModifierFlags::Static) {
                                return self.grammar_error_on_node(
                                    modifier,
                                    &diagnostics::X_0_MODIFIER_CANNOT_BE_USED_WITH_1_MODIFIER,
                                    &[&"static", &"abstract"],
                                );
                            }
                            if flags.intersects(ModifierFlags::Private) {
                                return self.grammar_error_on_node(
                                    modifier,
                                    &diagnostics::X_0_MODIFIER_CANNOT_BE_USED_WITH_1_MODIFIER,
                                    &[&"private", &"abstract"],
                                );
                            }
                            if flags.intersects(ModifierFlags::Async)
                                && let Some(last_async) = last_async
                            {
                                return self.grammar_error_on_node(
                                    last_async,
                                    &diagnostics::X_0_MODIFIER_CANNOT_BE_USED_WITH_1_MODIFIER,
                                    &[&"async", &"abstract"],
                                );
                            }
                            if flags.intersects(ModifierFlags::Override) && modifier_not_reparsed {
                                return self.grammar_error_on_node(
                                    modifier,
                                    &diagnostics::X_0_MODIFIER_MUST_PRECEDE_1_MODIFIER,
                                    &[&"abstract", &"override"],
                                );
                            }
                            if flags.intersects(ModifierFlags::Accessor) && modifier_not_reparsed {
                                return self.grammar_error_on_node(
                                    modifier,
                                    &diagnostics::X_0_MODIFIER_MUST_PRECEDE_1_MODIFIER,
                                    &[&"abstract", &"accessor"],
                                );
                            }
                        }
                        if let Some(name) = node.name()
                            && name.kind() == Kind::PrivateIdentifier
                        {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_CANNOT_BE_USED_WITH_A_PRIVATE_IDENTIFIER,
                                &[&"abstract"],
                            );
                        }

                        flags |= ModifierFlags::Abstract;
                    }
                    Kind::AsyncKeyword => {
                        if flags.intersects(ModifierFlags::Async) {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_ALREADY_SEEN,
                                &[&"async"],
                            );
                        } else if flags.intersects(ModifierFlags::Ambient)
                            || parent.flags().intersects(NodeFlags::Ambient)
                        {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_CANNOT_BE_USED_IN_AN_AMBIENT_CONTEXT,
                                &[&"async"],
                            );
                        } else if node.kind() == Kind::Parameter {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_CANNOT_APPEAR_ON_A_PARAMETER,
                                &[&"async"],
                            );
                        }
                        if flags.intersects(ModifierFlags::Abstract) {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_CANNOT_BE_USED_WITH_1_MODIFIER,
                                &[&"async", &"abstract"],
                            );
                        }
                        flags |= ModifierFlags::Async;
                        last_async = Some(modifier);
                    }
                    Kind::InKeyword | Kind::OutKeyword => {
                        let in_out_flag = if modifier.kind() == Kind::InKeyword {
                            ModifierFlags::In
                        } else {
                            ModifierFlags::Out
                        };
                        let in_out_text =
                            if modifier.kind() == Kind::InKeyword { "in" } else { "out" };
                        let parent = node.parent();
                        if node.kind() != Kind::TypeParameter
                            || parent.is_some_and(|parent| {
                                !(is_interface_declaration(parent)
                                    || ast::is_class_like(parent)
                                    || ast::is_type_or_js_type_alias_declaration(parent))
                            })
                        {
                            return self.grammar_error_on_node(modifier, &diagnostics::X_0_MODIFIER_CAN_ONLY_APPEAR_ON_A_TYPE_PARAMETER_OF_A_CLASS_INTERFACE_OR_TYPE_ALIAS, &[&in_out_text]);
                        }
                        if flags.intersects(in_out_flag) {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_ALREADY_SEEN,
                                &[&in_out_text],
                            );
                        }
                        if in_out_flag.intersects(ModifierFlags::In)
                            && flags.intersects(ModifierFlags::Out)
                        {
                            return self.grammar_error_on_node(
                                modifier,
                                &diagnostics::X_0_MODIFIER_MUST_PRECEDE_1_MODIFIER,
                                &[&"in", &"out"],
                            );
                        }
                        flags |= in_out_flag;
                    }
                    _ => {}
                }
            }
        }

        if node.kind() == Kind::Constructor {
            if flags.intersects(ModifierFlags::Static) {
                return self.grammar_error_on_node(
                    last_static.unwrap(),
                    &diagnostics::X_0_MODIFIER_CANNOT_APPEAR_ON_A_CONSTRUCTOR_DECLARATION,
                    &[&"static"],
                );
            }
            if flags.intersects(ModifierFlags::Override) {
                return self.grammar_error_on_node(
                    last_override.unwrap(),
                    &diagnostics::X_0_MODIFIER_CANNOT_APPEAR_ON_A_CONSTRUCTOR_DECLARATION,
                    &[&"override"],
                );
            }
            if flags.intersects(ModifierFlags::Async) {
                return self.grammar_error_on_node(
                    last_async.unwrap(),
                    &diagnostics::X_0_MODIFIER_CANNOT_APPEAR_ON_A_CONSTRUCTOR_DECLARATION,
                    &[&"async"],
                );
            }
            return false;
        } else if (node.kind() == Kind::ImportDeclaration
            || node.kind() == Kind::JSImportDeclaration
            || node.kind() == Kind::ImportEqualsDeclaration)
            && flags.intersects(ModifierFlags::Ambient)
        {
            return self.grammar_error_on_node(
                last_declare.unwrap(),
                &diagnostics::A_0_MODIFIER_CANNOT_BE_USED_WITH_AN_IMPORT_DECLARATION,
                &[&"declare"],
            );
        } else if node.kind() == Kind::Parameter
            && flags.intersects(ModifierFlags::ParameterPropertyModifier)
            && node.name().is_some_and(ast::is_binding_pattern)
        {
            return self.grammar_error_on_node(
                node,
                &diagnostics::A_PARAMETER_PROPERTY_MAY_NOT_BE_DECLARED_USING_A_BINDING_PATTERN,
                &[],
            );
        } else if node.kind() == Kind::Parameter
            && flags.intersects(ModifierFlags::ParameterPropertyModifier)
            && node.as_parameter_declaration().dot_dot_dot_token().is_some()
        {
            return self.grammar_error_on_node(
                node,
                &diagnostics::A_PARAMETER_PROPERTY_CANNOT_BE_DECLARED_USING_A_REST_PARAMETER,
                &[],
            );
        }
        if flags.intersects(ModifierFlags::Async) {
            return self.check_grammar_async_modifier(node, last_async);
        }
        false
    }

    // grammarchecks.go:571
    fn report_obvious_modifier_errors(&mut self, node: P<Node>) -> bool {
        let Some(modifier) = Self::find_first_illegal_modifier(node) else {
            return false;
        };
        self.grammar_error_on_first_token(modifier, &diagnostics::MODIFIERS_CANNOT_APPEAR_HERE, &[])
    }

    // grammarchecks.go:579
    fn find_first_modifier_except(node: P<Node>, allowed_modifier: Kind) -> Option<P<Node>> {
        let modifier = node.modifier_nodes().iter().copied().find(|&m| ast::is_modifier(m));
        if let Some(modifier) = modifier
            && modifier.kind() != allowed_modifier
        {
            return Some(modifier);
        }
        None
    }

    // grammarchecks.go:587
    fn find_first_illegal_modifier(node: P<Node>) -> Option<P<Node>> {
        match node.kind() {
            Kind::GetAccessor
            | Kind::SetAccessor
            | Kind::Constructor
            | Kind::PropertyDeclaration
            | Kind::PropertySignature
            | Kind::MethodDeclaration
            | Kind::MethodSignature
            | Kind::IndexSignature
            | Kind::ModuleDeclaration
            | Kind::ImportDeclaration
            | Kind::JSImportDeclaration
            | Kind::ImportEqualsDeclaration
            | Kind::ExportDeclaration
            | Kind::ExportAssignment
            | Kind::FunctionExpression
            | Kind::ArrowFunction
            | Kind::Parameter
            | Kind::TypeParameter
            | Kind::JSTypeAliasDeclaration => None,
            Kind::ClassStaticBlockDeclaration
            | Kind::PropertyAssignment
            | Kind::ShorthandPropertyAssignment
            | Kind::NamespaceExportDeclaration
            | Kind::MissingDeclaration => {
                node.modifier_nodes().iter().copied().find(|&m| ast::is_modifier(m))
            }
            _ => {
                let parent = node.parent().unwrap();
                if parent.kind() == Kind::ModuleBlock || parent.kind() == Kind::SourceFile {
                    return None;
                }
                match node.kind() {
                    Kind::FunctionDeclaration => {
                        Self::find_first_modifier_except(node, Kind::AsyncKeyword)
                    }
                    Kind::ClassDeclaration | Kind::ConstructorType => {
                        Self::find_first_modifier_except(node, Kind::AbstractKeyword)
                    }
                    Kind::ClassExpression
                    | Kind::InterfaceDeclaration
                    | Kind::TypeAliasDeclaration => {
                        node.modifier_nodes().iter().copied().find(|&m| ast::is_modifier(m))
                    }
                    Kind::VariableStatement => {
                        if node
                            .as_variable_statement()
                            .declaration_list
                            .flags()
                            .intersects(NodeFlags::Using)
                        {
                            return Self::find_first_modifier_except(node, Kind::AwaitKeyword);
                        }
                        node.modifier_nodes().iter().copied().find(|&m| ast::is_modifier(m))
                    }
                    Kind::EnumDeclaration => {
                        Self::find_first_modifier_except(node, Kind::ConstKeyword)
                    }
                    _ => panic!("Unhandled case in findFirstIllegalModifier."),
                }
            }
        }
    }

    // grammarchecks.go:642
    fn report_obvious_decorator_errors(&mut self, node: P<Node>) -> bool {
        let Some(decorator) = Self::find_first_illegal_decorator(node) else {
            return false;
        };
        self.grammar_error_on_first_token(
            decorator,
            &diagnostics::DECORATORS_ARE_NOT_VALID_HERE,
            &[],
        )
    }

    // grammarchecks.go:650
    fn find_first_illegal_decorator(node: P<Node>) -> Option<P<Node>> {
        if ast::can_have_illegal_decorators(node) {
            node.modifier_nodes().iter().copied().find(|&m| is_decorator(m))
        } else {
            None
        }
    }

    // grammarchecks.go:659
    fn check_grammar_async_modifier(
        &mut self,
        node: P<Node>,
        async_modifier: Option<P<Node>>,
    ) -> bool {
        match node.kind() {
            Kind::MethodDeclaration
            | Kind::FunctionDeclaration
            | Kind::FunctionExpression
            | Kind::ArrowFunction => return false,
            _ => {}
        }

        self.grammar_error_on_node(
            async_modifier.unwrap(),
            &diagnostics::X_0_MODIFIER_CANNOT_BE_USED_HERE,
            &[&"async"],
        )
    }

    // grammarchecks.go:671
    pub(crate) fn check_grammar_for_disallowed_trailing_comma(
        &mut self,
        list: Option<P<NodeList>>,
        diag: &'static Message,
    ) -> bool {
        if let Some(list) = list
            && list.has_trailing_comma()
        {
            return self.grammar_error_at_pos(
                list.nodes()[0],
                list.end()
                    .expect_source("parsed node lists have source positions")
                    .checked_sub(",".len() as u32)
                    .expect("trailing comma starts before the list"),
                ",".len() as u32,
                diag,
                &[],
            );
        }
        false
    }

    // grammarchecks.go:678
    fn check_grammar_type_parameter_list(
        &mut self,
        type_parameters: Option<P<NodeList>>,
        file: P<SourceFile>,
    ) -> bool {
        if let Some(type_parameters) = type_parameters
            && type_parameters.nodes().is_empty()
        {
            let start = type_parameters
                .pos()
                .expect_source("parsed node lists have source positions")
                .checked_sub("<".len() as u32)
                .expect("type parameter list starts after its opening token");
            let end = scanner::skip_trivia(
                file.text(),
                type_parameters.end().expect_source("parsed node lists have source positions"),
            ) + ">".len() as u32;
            return self.grammar_error_at_pos(
                file.as_node(),
                start,
                end - start,
                &diagnostics::TYPE_PARAMETER_LIST_CANNOT_BE_EMPTY,
                &[],
            );
        }
        false
    }

    // grammarchecks.go:687
    fn check_grammar_parameter_list(&mut self, parameters: P<NodeList>) -> bool {
        let mut seen_optional_parameter = false;
        let parameter_count = parameters.nodes().len();

        for i in 0..parameter_count {
            let parameter_node = parameters.nodes()[i];
            let parameter = parameter_node.as_parameter_declaration();
            if let Some(dot_dot_dot_token) = parameter.dot_dot_dot_token() {
                if i != parameter_count - 1 {
                    return self.grammar_error_on_node(
                        dot_dot_dot_token,
                        &diagnostics::A_REST_PARAMETER_MUST_BE_LAST_IN_A_PARAMETER_LIST,
                        &[],
                    );
                }
                if !parameter_node.flags().intersects(NodeFlags::Ambient) {
                    self.check_grammar_for_disallowed_trailing_comma(Some(parameters), &diagnostics::A_REST_PARAMETER_OR_BINDING_PATTERN_MAY_NOT_HAVE_A_TRAILING_COMMA);
                }

                if let Some(question_token) = parameter.question_token() {
                    return self.grammar_error_on_node(
                        question_token,
                        &diagnostics::A_REST_PARAMETER_CANNOT_BE_OPTIONAL,
                        &[],
                    );
                }

                if parameter.initializer().is_some() {
                    return self.grammar_error_on_node(
                        parameter.name,
                        &diagnostics::A_REST_PARAMETER_CANNOT_HAVE_AN_INITIALIZER,
                        &[],
                    );
                }
            } else if crate::is_optional_declaration(parameter_node) {
                seen_optional_parameter = true;
                // A reparsed '?' token indicates a bracketed name in @param tag
                if parameter
                    .question_token()
                    .is_some_and(|q| !q.flags().intersects(NodeFlags::Reparsed))
                    && parameter.initializer().is_some()
                {
                    return self.grammar_error_on_node(
                        parameter.name,
                        &diagnostics::PARAMETER_CANNOT_HAVE_QUESTION_MARK_AND_INITIALIZER,
                        &[],
                    );
                }
            } else if seen_optional_parameter && parameter.initializer().is_none() {
                return self.grammar_error_on_node(
                    parameter.name,
                    &diagnostics::A_REQUIRED_PARAMETER_CANNOT_FOLLOW_AN_OPTIONAL_PARAMETER,
                    &[],
                );
            }
        }

        false
    }

    // grammarchecks.go:722
    fn check_grammar_for_use_strict_simple_parameter_list(&mut self, node: P<Node>) -> bool {
        if self.language_version >= ScriptTarget::ES2016 {
            let body = node.body();
            let mut use_strict_directive: Option<P<Node>> = None;
            if let Some(body) = body
                && is_block(body)
            {
                use_strict_directive = binder::find_use_strict_prologue(
                    ast::get_source_file_of_node(node).unwrap(),
                    body.statements(),
                );
            }
            if let Some(use_strict_directive) = use_strict_directive {
                let non_simple_parameters: Vec<P<Node>> = node
                    .parameters()
                    .iter()
                    .copied()
                    .filter(|&n| {
                        let parameter = n.as_parameter_declaration();
                        parameter.initializer().is_some()
                            || ast::is_binding_pattern(parameter.name)
                            || crate::is_rest_parameter(n)
                    })
                    .collect();
                if !non_simple_parameters.is_empty() {
                    for &parameter in &non_simple_parameters {
                        let err = self.error(
                            Some(parameter),
                            &diagnostics::THIS_PARAMETER_IS_NOT_ALLOWED_WITH_USE_STRICT_DIRECTIVE,
                            &[],
                        );
                        err.add_related_info(create_diagnostic_for_node(
                            Some(use_strict_directive),
                            &diagnostics::X_USE_STRICT_DIRECTIVE_USED_HERE,
                            &[],
                        ));
                    }

                    let err = self.error(Some(use_strict_directive), &diagnostics::X_USE_STRICT_DIRECTIVE_CANNOT_BE_USED_WITH_NON_SIMPLE_PARAMETER_LIST, &[]);
                    for (index, &parameter) in non_simple_parameters.iter().enumerate() {
                        let related_message = if index == 0 {
                            &diagnostics::NON_SIMPLE_PARAMETER_DECLARED_HERE
                        } else {
                            &diagnostics::X_AND_HERE
                        };
                        err.add_related_info(create_diagnostic_for_node(
                            Some(parameter),
                            related_message,
                            &[],
                        ));
                    }

                    return true;
                }
            }
        }
        false
    }

    // grammarchecks.go:758
    pub(crate) fn check_grammar_function_like_declaration(&mut self, node: P<Node>) -> bool {
        // Prevent cascading error by short-circuit
        let file = ast::get_source_file_of_node(node).unwrap();
        let func_data = node.function_like_data().unwrap();
        self.check_grammar_modifiers(node)
            || self.check_grammar_type_parameter_list(func_data.type_parameters.get(), file)
            || self.check_grammar_parameter_list(func_data.parameters.get().unwrap())
            || self.check_grammar_arrow_function(node, file)
            || (ast::is_function_like_declaration(node)
                && self.check_grammar_for_use_strict_simple_parameter_list(node))
    }

    // grammarchecks.go:767
    pub(crate) fn check_grammar_class_like_declaration(&mut self, node: P<Node>) -> bool {
        let file = ast::get_source_file_of_node(node).unwrap();
        self.check_grammar_class_declaration_heritage_clauses(node, file)
            || self.check_grammar_type_parameter_list(node.type_parameter_list(), file)
    }

    // grammarchecks.go:772
    fn check_grammar_arrow_function(&mut self, node: P<Node>, file: P<SourceFile>) -> bool {
        if !is_arrow_function(node) {
            return false;
        }

        let arrow_func = node.as_arrow_function();
        let type_parameters = arrow_func.function_like_base.type_parameters.get();
        if let Some(type_parameters) = type_parameters {
            let type_param_nodes = type_parameters.nodes();
            let has_constraint = !type_param_nodes.is_empty()
                && type_param_nodes[0].as_type_parameter_declaration().constraint.is_some();
            if !(type_param_nodes.len() > 1
                || type_parameters.has_trailing_comma()
                || has_constraint)
                && tspath::file_extension_is_one_of(
                    file.file_name(),
                    &[tspath::EXTENSION_MTS, tspath::EXTENSION_CTS],
                )
            {
                // TODO(danielr): should we return early here?
                self.grammar_error_on_node(type_parameters.nodes()[0], &diagnostics::THIS_SYNTAX_IS_RESERVED_IN_FILES_WITH_THE_MTS_OR_CTS_EXTENSION_ADD_A_TRAILING_COMMA_OR_EXPLICIT_CONSTRAINT, &[]);
            }
        }

        let equals_greater_than_token = arrow_func.equals_greater_than_token.unwrap();
        let arrow_full_text = &file.text()[equals_greater_than_token
            .pos()
            .expect_source("parsed tokens have source positions")
            as usize
            ..equals_greater_than_token.end().expect_source("parsed tokens have source positions")
                as usize];
        arrow_full_text.chars().any(|ch| stringutil::is_line_break(ch as u32))
            && self.grammar_error_on_node(
                equals_greater_than_token,
                &diagnostics::LINE_TERMINATOR_NOT_PERMITTED_BEFORE_ARROW,
                &[],
            )
    }

    // grammarchecks.go:796
    fn check_grammar_index_signature_parameters(&mut self, node: P<Node>) -> bool {
        let index_signature = node.as_index_signature_declaration();
        let parameters = index_signature.function_like_base.parameters.get().unwrap();
        let param_nodes = parameters.nodes();

        if param_nodes.is_empty() {
            return self.grammar_error_on_node(
                node,
                &diagnostics::AN_INDEX_SIGNATURE_MUST_HAVE_EXACTLY_ONE_PARAMETER,
                &[],
            );
        }

        let parameter_node = param_nodes[0];
        let parameter = parameter_node.as_parameter_declaration();
        if param_nodes.len() != 1 {
            return self.grammar_error_on_node(
                parameter.name,
                &diagnostics::AN_INDEX_SIGNATURE_MUST_HAVE_EXACTLY_ONE_PARAMETER,
                &[],
            );
        }

        self.check_grammar_for_disallowed_trailing_comma(
            Some(parameters),
            &diagnostics::AN_INDEX_SIGNATURE_CANNOT_HAVE_A_TRAILING_COMMA,
        );
        if let Some(dot_dot_dot_token) = parameter.dot_dot_dot_token() {
            return self.grammar_error_on_node(
                dot_dot_dot_token,
                &diagnostics::AN_INDEX_SIGNATURE_CANNOT_HAVE_A_REST_PARAMETER,
                &[],
            );
        }
        if parameter_node.modifiers().is_some() {
            return self.grammar_error_on_node(
                parameter.name,
                &diagnostics::AN_INDEX_SIGNATURE_PARAMETER_CANNOT_HAVE_AN_ACCESSIBILITY_MODIFIER,
                &[],
            );
        }
        if let Some(question_token) = parameter.question_token() {
            return self.grammar_error_on_node(
                question_token,
                &diagnostics::AN_INDEX_SIGNATURE_PARAMETER_CANNOT_HAVE_A_QUESTION_MARK,
                &[],
            );
        }
        if parameter.initializer().is_some() {
            return self.grammar_error_on_node(
                parameter.name,
                &diagnostics::AN_INDEX_SIGNATURE_PARAMETER_CANNOT_HAVE_AN_INITIALIZER,
                &[],
            );
        }
        let Some(type_node) = parameter.type_() else {
            return self.grammar_error_on_node(
                parameter.name,
                &diagnostics::AN_INDEX_SIGNATURE_PARAMETER_MUST_HAVE_A_TYPE_ANNOTATION,
                &[],
            );
        };
        let t = self.get_type_from_type_node(type_node);
        if some_type(self, t, |_, t| t.flags().intersects(TypeFlags::StringOrNumberLiteralOrUnique))
            || self.is_generic_type(t)
        {
            return self.grammar_error_on_node(parameter.name, &diagnostics::AN_INDEX_SIGNATURE_PARAMETER_TYPE_CANNOT_BE_A_LITERAL_TYPE_OR_GENERIC_TYPE_CONSIDER_USING_A_MAPPED_OBJECT_TYPE_INSTEAD, &[]);
        }
        if !every_type(self, t, super::checker::Checker::is_valid_index_key_type) {
            return self.grammar_error_on_node(parameter.name, &diagnostics::AN_INDEX_SIGNATURE_PARAMETER_TYPE_MUST_BE_STRING_NUMBER_SYMBOL_OR_A_TEMPLATE_LITERAL_TYPE, &[]);
        }
        if index_signature.function_like_base.type_.get().is_none() {
            return self.grammar_error_on_node(
                node,
                &diagnostics::AN_INDEX_SIGNATURE_MUST_HAVE_A_TYPE_ANNOTATION,
                &[],
            );
        }
        false
    }

    // grammarchecks.go:840
    pub(crate) fn check_grammar_index_signature(&mut self, node: P<Node>) -> bool {
        // Prevent cascading error by short-circuit
        self.check_grammar_modifiers(node) || self.check_grammar_index_signature_parameters(node)
    }

    // grammarchecks.go:845
    fn check_grammar_for_at_least_one_type_argument(
        &mut self,
        node: P<Node>,
        type_arguments: Option<P<NodeList>>,
    ) -> bool {
        if let Some(type_arguments) = type_arguments
            && type_arguments.nodes().is_empty()
        {
            let source_file = ast::get_source_file_of_node(node).unwrap();
            let start = type_arguments
                .pos()
                .expect_source("parsed node lists have source positions")
                .checked_sub("<".len() as u32)
                .expect("type argument list starts after its opening token");
            let end = scanner::skip_trivia(
                source_file.text(),
                type_arguments.end().expect_source("parsed node lists have source positions"),
            ) + ">".len() as u32;
            return self.grammar_error_at_pos(
                source_file.as_node(),
                start,
                end - start,
                &diagnostics::TYPE_ARGUMENT_LIST_CANNOT_BE_EMPTY,
                &[],
            );
        }
        false
    }

    // grammarchecks.go:855
    pub(crate) fn check_grammar_type_arguments(
        &mut self,
        node: P<Node>,
        type_arguments: Option<P<NodeList>>,
    ) -> bool {
        self.check_grammar_for_disallowed_trailing_comma(
            type_arguments,
            &diagnostics::TRAILING_COMMA_NOT_ALLOWED,
        ) || self.check_grammar_for_at_least_one_type_argument(node, type_arguments)
    }

    // grammarchecks.go:859
    pub(crate) fn check_grammar_tagged_template_chain(&mut self, node: P<Node>) -> bool {
        let tagged_template = node.as_tagged_template_expression();
        if tagged_template.question_dot_token.is_some()
            || node.flags().intersects(NodeFlags::OptionalChain)
        {
            return self.grammar_error_on_node(
                tagged_template.template,
                &diagnostics::TAGGED_TEMPLATE_EXPRESSIONS_ARE_NOT_PERMITTED_IN_AN_OPTIONAL_CHAIN,
                &[],
            );
        }
        false
    }

    // grammarchecks.go:866
    fn check_grammar_heritage_clause(&mut self, node: P<Node>) -> bool {
        let heritage_clause = node.as_heritage_clause();
        let types = heritage_clause.types.get();
        if self.check_grammar_for_disallowed_trailing_comma(
            Some(types),
            &diagnostics::TRAILING_COMMA_NOT_ALLOWED,
        ) {
            return true;
        }
        if types.nodes().is_empty() {
            let list_type = scanner::token_to_string(heritage_clause.token);
            // TODO(danielr): why not error on the token?
            return self.grammar_error_at_pos(
                node,
                types.pos().expect_source("parsed node lists have source positions"),
                0,
                &diagnostics::X_0_LIST_CANNOT_BE_EMPTY,
                &[&list_type],
            );
        }

        types.nodes().iter().any(|&node| self.check_grammar_expression_with_type_arguments(node))
    }

    // grammarchecks.go:885
    pub(crate) fn check_grammar_expression_with_type_arguments(&mut self, node: P<Node>) -> bool {
        if is_expression_with_type_arguments(node)
            && node.expression().unwrap().kind() == Kind::ImportKeyword
            && node.type_argument_list().is_some()
        {
            return self.grammar_error_on_node(node, &diagnostics::THIS_USE_OF_IMPORT_IS_INVALID_IMPORT_CALLS_CAN_BE_WRITTEN_BUT_THEY_MUST_HAVE_PARENTHESES_AND_CANNOT_HAVE_TYPE_ARGUMENTS, &[]);
        }
        self.check_grammar_type_arguments(node, node.type_argument_list())
    }

    // grammarchecks.go:892
    fn check_grammar_class_declaration_heritage_clauses(
        &mut self,
        node: P<Node>,
        _file: P<SourceFile>,
    ) -> bool {
        let mut seen_extends_clause = false;
        let mut seen_implements_clause = false;

        let class_like_data = node.class_like_data().unwrap();

        if !self.check_grammar_modifiers(node)
            && let Some(heritage_clauses) = class_like_data.heritage_clauses.get()
        {
            for &heritage_clause_node in heritage_clauses.nodes() {
                let heritage_clause = heritage_clause_node.as_heritage_clause();
                if heritage_clause.token == Kind::ExtendsKeyword {
                    if seen_extends_clause {
                        return self.grammar_error_on_first_token(
                            heritage_clause_node,
                            &diagnostics::X_EXTENDS_CLAUSE_ALREADY_SEEN,
                            &[],
                        );
                    }

                    if seen_implements_clause {
                        return self.grammar_error_on_first_token(
                            heritage_clause_node,
                            &diagnostics::X_EXTENDS_CLAUSE_MUST_PRECEDE_IMPLEMENTS_CLAUSE,
                            &[],
                        );
                    }

                    let type_nodes = heritage_clause.types.get().nodes();
                    if type_nodes.len() > 1 {
                        return self.grammar_error_on_first_token(
                            type_nodes[1],
                            &diagnostics::CLASSES_CAN_ONLY_EXTEND_A_SINGLE_CLASS,
                            &[],
                        );
                    }

                    seen_extends_clause = true;
                } else {
                    assert!(
                        heritage_clause.token == Kind::ImplementsKeyword,
                        "Unexpected token {:?}",
                        heritage_clause.token
                    );
                    if seen_implements_clause {
                        return self.grammar_error_on_first_token(
                            heritage_clause_node,
                            &diagnostics::X_IMPLEMENTS_CLAUSE_ALREADY_SEEN,
                            &[],
                        );
                    }

                    seen_implements_clause = true;
                }

                // Grammar checking heritageClause inside class declaration
                self.check_grammar_heritage_clause(heritage_clause_node);
            }
        }

        false
    }

    // grammarchecks.go:935
    pub(crate) fn check_grammar_interface_declaration(&mut self, node: P<Node>) -> bool {
        if let Some(heritage_clauses) = node.as_interface_declaration().heritage_clauses {
            let mut seen_extends_clause = false;
            for &heritage_clause_node in heritage_clauses.nodes() {
                let heritage_clause = heritage_clause_node.as_heritage_clause();

                match heritage_clause.token {
                    Kind::ExtendsKeyword => {
                        if seen_extends_clause {
                            return self.grammar_error_on_first_token(
                                heritage_clause_node,
                                &diagnostics::X_EXTENDS_CLAUSE_ALREADY_SEEN,
                                &[],
                            );
                        }
                        seen_extends_clause = true;
                    }
                    Kind::ImplementsKeyword => {
                        return self.grammar_error_on_first_token(
                            heritage_clause_node,
                            &diagnostics::INTERFACE_DECLARATION_CANNOT_HAVE_IMPLEMENTS_CLAUSE,
                            &[],
                        );
                    }
                    _ => panic!("Unexpected token {:?}", heritage_clause.token),
                }

                // Grammar checking heritageClause inside class declaration
                self.check_grammar_heritage_clause(heritage_clause_node);
            }
        }

        false
    }

    // grammarchecks.go:961
    pub(crate) fn check_grammar_computed_property_name(&mut self, node: P<Node>) -> bool {
        // If node is not a computedPropertyName, just skip the grammar checking
        if node.kind() != Kind::ComputedPropertyName {
            return false;
        }

        let computed_property_name = node.as_computed_property_name();
        if computed_property_name.expression.kind() == Kind::BinaryExpression
            && computed_property_name.expression.as_binary_expression().operator_token.kind()
                == Kind::CommaToken
        {
            return self.grammar_error_on_node(
                computed_property_name.expression,
                &diagnostics::A_COMMA_EXPRESSION_IS_NOT_ALLOWED_IN_A_COMPUTED_PROPERTY_NAME,
                &[],
            );
        }
        false
    }

    // grammarchecks.go:974
    pub(crate) fn check_grammar_for_generator(&mut self, node: P<Node>) -> bool {
        if let Some(body_data) = node.body_data()
            && let Some(asterisk_token) = body_data.asterisk_token
        {
            assert!(
                !(node.kind() != Kind::FunctionDeclaration
                    && node.kind() != Kind::FunctionExpression
                    && node.kind() != Kind::MethodDeclaration),
                "Unexpected node kind {:?}",
                node.kind()
            );
            if node.flags().intersects(NodeFlags::Ambient) {
                return self.grammar_error_on_node(
                    asterisk_token,
                    &diagnostics::GENERATORS_ARE_NOT_ALLOWED_IN_AN_AMBIENT_CONTEXT,
                    &[],
                );
            }
            if body_data.body.is_none() {
                return self.grammar_error_on_node(
                    asterisk_token,
                    &diagnostics::AN_OVERLOAD_SIGNATURE_CANNOT_BE_DECLARED_AS_A_GENERATOR,
                    &[],
                );
            }
        }

        false
    }

    // grammarchecks.go:990
    fn check_grammar_for_invalid_question_mark(
        &mut self,
        postfix_token: Option<P<Node>>,
        message: &'static Message,
    ) -> bool {
        match postfix_token {
            Some(postfix_token) => {
                postfix_token.kind() == Kind::QuestionToken
                    && self.grammar_error_on_node(postfix_token, message, &[])
            }
            None => false,
        }
    }

    // grammarchecks.go:994
    fn check_grammar_for_invalid_exclamation_token(
        &mut self,
        postfix_token: Option<P<Node>>,
        message: &'static Message,
    ) -> bool {
        match postfix_token {
            Some(postfix_token) => {
                postfix_token.kind() == Kind::ExclamationToken
                    && self.grammar_error_on_node(postfix_token, message, &[])
            }
            None => false,
        }
    }

    // grammarchecks.go:998
    pub(crate) fn check_grammar_object_literal_expression(
        &mut self,
        node: P<Node>,
        in_destructuring: bool,
    ) -> bool {
        let mut seen: FxHashMap<String, DeclarationMeaning> = FxHashMap::default();

        let properties = node.as_object_literal_expression().properties.nodes();
        for &prop in properties {
            if prop.kind() == Kind::SpreadAssignment {
                let spread_assignment = prop.as_spread_assignment();
                if in_destructuring {
                    // a rest property cannot be destructured any further
                    let expression = ast::skip_parentheses(spread_assignment.expression);
                    if is_array_literal_expression(expression)
                        || is_object_literal_expression(expression)
                    {
                        return self.grammar_error_on_node(
                            spread_assignment.expression,
                            &diagnostics::A_REST_ELEMENT_CANNOT_CONTAIN_A_BINDING_PATTERN,
                            &[],
                        );
                    }
                }
                continue;
            }
            let name = prop.name().unwrap();
            if name.kind() == Kind::ComputedPropertyName {
                // If the name is not a ComputedPropertyName, the grammar checking will skip it
                self.check_grammar_computed_property_name(name);
            }

            if prop.kind() == Kind::ShorthandPropertyAssignment && !in_destructuring {
                let shorthand_prop = prop.as_shorthand_property_assignment();
                if let Some(object_assignment_initializer) =
                    shorthand_prop.object_assignment_initializer.get()
                {
                    // having objectAssignmentInitializer is only valid in an ObjectAssignmentPattern.
                    // Outside of destructuring, it is a syntax error.

                    // Try to grab the last node prior to the initializer,
                    // then error on the first token following (which should be the `=` token).
                    let mut last_node_before_initializer: Option<P<Node>> = None;
                    prop.for_each_child(&mut |child| {
                        if child != object_assignment_initializer {
                            last_node_before_initializer = Some(child);
                            return false;
                        }
                        true
                    });

                    self.grammar_error_on_first_token(last_node_before_initializer.unwrap(), &diagnostics::DID_YOU_MEAN_TO_USE_A_COLON_AN_CAN_ONLY_FOLLOW_A_PROPERTY_NAME_WHEN_THE_CONTAINING_OBJECT_LITERAL_IS_PART_OF_A_DESTRUCTURING_PATTERN, &[]);
                }
            }

            if name.kind() == Kind::PrivateIdentifier {
                self.grammar_error_on_node(
                    name,
                    &diagnostics::PRIVATE_IDENTIFIERS_ARE_NOT_ALLOWED_OUTSIDE_CLASS_BODIES,
                    &[],
                );
            }

            // Modifiers are never allowed on properties except for 'async' on a method declaration
            let modifiers = prop.modifier_nodes();
            if !modifiers.is_empty() {
                if ast::can_have_modifiers(prop) {
                    for &m in modifiers {
                        if ast::is_modifier(m)
                            && (m.kind() != Kind::AsyncKeyword
                                || prop.kind() != Kind::MethodDeclaration)
                        {
                            self.grammar_error_on_node(
                                m,
                                &diagnostics::X_0_MODIFIER_CANNOT_BE_USED_HERE,
                                &[&scanner::get_text_of_node(m)],
                            );
                        }
                    }
                } else if ast::can_have_illegal_modifiers(prop) {
                    for &m in modifiers {
                        if ast::is_modifier(m) {
                            self.grammar_error_on_node(
                                m,
                                &diagnostics::X_0_MODIFIER_CANNOT_BE_USED_HERE,
                                &[&scanner::get_text_of_node(m)],
                            );
                        }
                    }
                }
            }

            // ECMA-262 11.1.5 Object Initializer
            // If previous is not undefined then throw a SyntaxError exception if any of the following conditions are true
            // a.This production is contained in strict code and IsDataDescriptor(previous) is true and
            // IsDataDescriptor(propId.descriptor) is true.
            //    b.IsDataDescriptor(previous) is true and IsAccessorDescriptor(propId.descriptor) is true.
            //    c.IsAccessorDescriptor(previous) is true and IsDataDescriptor(propId.descriptor) is true.
            //    d.IsAccessorDescriptor(previous) is true and IsAccessorDescriptor(propId.descriptor) is true
            // and either both previous and propId.descriptor have[[Get]] fields or both previous and propId.descriptor have[[Set]] fields

            let current_kind: DeclarationMeaning = match prop.kind() {
                Kind::ShorthandPropertyAssignment | Kind::PropertyAssignment => {
                    let common_prop = if prop.kind() == Kind::ShorthandPropertyAssignment {
                        prop.class_like_data();
                        &prop.as_shorthand_property_assignment().named_member_base
                    } else {
                        &prop.as_property_assignment().named_member_base
                    };

                    // Grammar checking for computedPropertyName and shorthandPropertyAssignment
                    self.check_grammar_for_invalid_exclamation_token(common_prop.postfix_token, &diagnostics::A_DEFINITE_ASSIGNMENT_ASSERTION_IS_NOT_PERMITTED_IN_THIS_CONTEXT);
                    self.check_grammar_for_invalid_question_mark(
                        common_prop.postfix_token,
                        &diagnostics::AN_OBJECT_MEMBER_CANNOT_BE_DECLARED_OPTIONAL,
                    );

                    if name.kind() == Kind::NumericLiteral {
                        self.check_grammar_numeric_literal(name);
                    }

                    if name.kind() == Kind::BigIntLiteral {
                        self.add_error_or_suggestion(
                            true,
                            create_diagnostic_for_node(
                                Some(name),
                                &diagnostics::A_BIGINT_LITERAL_CANNOT_BE_USED_AS_A_PROPERTY_NAME,
                                &[],
                            ),
                        );
                    }

                    DeclarationMeaning::PropertyAssignment
                }
                Kind::MethodDeclaration => DeclarationMeaning::Method,
                Kind::GetAccessor => DeclarationMeaning::GetAccessor,
                Kind::SetAccessor => DeclarationMeaning::SetAccessor,
                _ => panic!("Unexpected node kind {:?}", prop.kind()),
            };

            if !in_destructuring {
                let (effective_name, ok) =
                    self.get_effective_property_name_for_property_name_node(name);
                if !ok {
                    continue;
                }

                let entry = seen.entry(effective_name).or_default();
                let existing_kind = *entry;
                if existing_kind.is_empty() {
                    *entry = current_kind;
                } else if current_kind.intersects(DeclarationMeaning::Method)
                    && existing_kind.intersects(DeclarationMeaning::Method)
                {
                    self.grammar_error_on_node(
                        name,
                        &diagnostics::DUPLICATE_IDENTIFIER_0,
                        &[&scanner::get_text_of_node(name)],
                    );
                } else if current_kind.intersects(DeclarationMeaning::PropertyAssignment)
                    && existing_kind.intersects(DeclarationMeaning::PropertyAssignment)
                {
                    self.grammar_error_on_node(name, &diagnostics::AN_OBJECT_LITERAL_CANNOT_HAVE_MULTIPLE_PROPERTIES_WITH_THE_SAME_NAME, &[&scanner::get_text_of_node(name)]);
                } else if current_kind.intersects(DeclarationMeaning::GetOrSetAccessor)
                    && existing_kind.intersects(DeclarationMeaning::GetOrSetAccessor)
                {
                    if existing_kind != DeclarationMeaning::GetOrSetAccessor
                        && current_kind != existing_kind
                    {
                        *entry = current_kind | existing_kind;
                    } else {
                        return self.grammar_error_on_node(name, &diagnostics::AN_OBJECT_LITERAL_CANNOT_HAVE_MULTIPLE_GET_SLASHSET_ACCESSORS_WITH_THE_SAME_NAME, &[]);
                    }
                } else {
                    return self.grammar_error_on_node(name, &diagnostics::AN_OBJECT_LITERAL_CANNOT_HAVE_PROPERTY_AND_ACCESSOR_WITH_THE_SAME_NAME, &[]);
                }
            }
        }

        false
    }

    // grammarchecks.go:1138
    pub(crate) fn check_grammar_jsx_element(&mut self, node: P<Node>) -> bool {
        self.check_grammar_jsx_name(node.tag_name());
        self.check_grammar_type_arguments(node, node.type_argument_list());
        let mut seen: FxHashSet<&'static str> = FxHashSet::default();
        if let Some(result) = node.attributes().unwrap().properties().iter().find_map(|&attr_node| {
            if attr_node.kind() == Kind::JsxSpreadAttribute {
                return None;
            }
            let attr = attr_node.as_jsx_attribute();
            let name = attr.name;
            let initializer = attr.initializer;
            let text_of_name = name.text();
            if !seen.insert(text_of_name) {
                return Some(self.grammar_error_on_node(name, &diagnostics::JSX_ELEMENTS_CANNOT_HAVE_MULTIPLE_ATTRIBUTES_WITH_THE_SAME_NAME, &[]));
            }
            if let Some(initializer) = initializer
                && initializer.kind() == Kind::JsxExpression && initializer.expression().is_none() {
                    return Some(self.grammar_error_on_node(initializer, &diagnostics::JSX_ATTRIBUTES_MUST_ONLY_BE_ASSIGNED_A_NON_EMPTY_EXPRESSION, &[]));
                }

            None
        }) {
            return result;
        }
        false
    }

    // grammarchecks.go:1162
    fn check_grammar_jsx_name(&mut self, node: P<Node>) -> bool {
        if is_property_access_expression(node) && is_jsx_namespaced_name(node.expression().unwrap())
        {
            return self.grammar_error_on_node(
                node.expression().unwrap(),
                &diagnostics::JSX_PROPERTY_ACCESS_EXPRESSIONS_CANNOT_INCLUDE_JSX_NAMESPACE_NAMES,
                &[],
            );
        }

        if is_jsx_namespaced_name(node)
            && self.compiler_options.get_jsx_transform_enabled()
            && !scanner::is_intrinsic_jsx_name(node.as_jsx_namespaced_name().namespace.text())
        {
            return self.grammar_error_on_node(
                node,
                &diagnostics::REACT_COMPONENTS_CANNOT_INCLUDE_JSX_NAMESPACE_NAMES,
                &[],
            );
        }

        false
    }

    // grammarchecks.go:1174
    pub(crate) fn check_grammar_jsx_expression(&mut self, node: P<Node>) -> bool {
        if let Some(expression) = node.as_jsx_expression().expression
            && ast::is_comma_sequence(expression)
        {
            return self.grammar_error_on_node(expression, &diagnostics::JSX_EXPRESSIONS_MAY_NOT_USE_THE_COMMA_OPERATOR_DID_YOU_MEAN_TO_WRITE_AN_ARRAY, &[]);
        }

        false
    }

    // grammarchecks.go:1182
    pub(crate) fn check_grammar_for_in_or_for_of_statement(
        &mut self,
        for_in_or_of_statement: P<Node>,
    ) -> bool {
        let as_node = for_in_or_of_statement;
        let stmt = for_in_or_of_statement.as_for_in_or_of_statement();
        if self.check_grammar_statement_in_ambient_context(as_node) {
            return true;
        }

        if for_in_or_of_statement.kind() == Kind::ForOfStatement
            && let Some(await_modifier) = stmt.await_modifier
            && !for_in_or_of_statement.flags().intersects(NodeFlags::AwaitContext)
        {
            let source_file = ast::get_source_file_of_node(as_node).unwrap();
            if ast::is_in_top_level_context(as_node) {
                if !Self::has_parse_diagnostics(source_file) {
                    if !ast::is_effective_external_module(source_file, &self.compiler_options) {
                        self.add_diagnostic(create_diagnostic_for_node(Some(await_modifier), &diagnostics::X_FOR_AWAIT_LOOPS_ARE_ONLY_ALLOWED_AT_THE_TOP_LEVEL_OF_A_FILE_WHEN_THAT_FILE_IS_A_MODULE_BUT_THIS_FILE_HAS_NO_IMPORTS_OR_EXPORTS_CONSIDER_ADDING_AN_EMPTY_EXPORT_TO_MAKE_THIS_FILE_A_MODULE, &[]));
                    }
                    let mut fallthrough_es = false;
                    let mut fallthrough_default = false;
                    match self.module_kind {
                        ModuleKind::NODE16
                        | ModuleKind::NODE18
                        | ModuleKind::NODE20
                        | ModuleKind::NODE_NEXT => {
                            let source_file_meta_data =
                                self.program.get_source_file_meta_data(source_file.path());
                            if source_file_meta_data.implied_node_format == ModuleKind::COMMON_JS {
                                self.add_diagnostic(create_diagnostic_for_node(Some(await_modifier), &diagnostics::THE_CURRENT_FILE_IS_A_COMMON_JS_MODULE_AND_CANNOT_USE_AWAIT_AT_THE_TOP_LEVEL, &[]));
                            } else {
                                fallthrough_es = true;
                            }
                        }
                        ModuleKind::ES2022
                        | ModuleKind::ES_NEXT
                        | ModuleKind::PRESERVE
                        | ModuleKind::SYSTEM => fallthrough_es = true,
                        _ => fallthrough_default = true,
                    }
                    if fallthrough_es && (self.language_version < ScriptTarget::ES2017) {
                        fallthrough_default = true;
                    }
                    if fallthrough_default {
                        self.add_diagnostic(create_diagnostic_for_node(Some(await_modifier), &diagnostics::TOP_LEVEL_FOR_AWAIT_LOOPS_ARE_ONLY_ALLOWED_WHEN_THE_MODULE_OPTION_IS_SET_TO_ES2022_ESNEXT_SYSTEM_NODE16_NODE18_NODE20_NODENEXT_OR_PRESERVE_AND_THE_TARGET_OPTION_IS_SET_TO_ES2017_OR_HIGHER, &[]));
                    }
                }
            } else {
                // use of 'for-await-of' in non-async function
                if !Self::has_parse_diagnostics(source_file) {
                    let diagnostic = create_diagnostic_for_node(Some(await_modifier), &diagnostics::X_FOR_AWAIT_LOOPS_ARE_ONLY_ALLOWED_WITHIN_ASYNC_FUNCTIONS_AND_AT_THE_TOP_LEVELS_OF_MODULES, &[]);
                    let containing_func = ast::get_containing_function(for_in_or_of_statement);
                    if let Some(containing_func) = containing_func
                        && containing_func.kind() != Kind::Constructor
                    {
                        assert!(
                            !ast::get_function_flags(Some(containing_func))
                                .intersects(FunctionFlags::Async),
                            "Enclosing function should never be an async function."
                        );
                        let related_info = create_diagnostic_for_node(
                            Some(containing_func),
                            &diagnostics::DID_YOU_MEAN_TO_MARK_THIS_FUNCTION_AS_ASYNC,
                            &[],
                        );
                        diagnostic.add_related_info(related_info);
                    }
                    self.add_diagnostic(diagnostic);
                    return true;
                }
            }
        }

        if is_for_of_statement(as_node)
            && !for_in_or_of_statement.flags().intersects(NodeFlags::AwaitContext)
            && is_identifier(stmt.initializer)
            && stmt.initializer.text() == "async"
        {
            self.grammar_error_on_node(
                stmt.initializer,
                &diagnostics::THE_LEFT_HAND_SIDE_OF_A_FOR_OF_STATEMENT_MAY_NOT_BE_ASYNC,
                &[],
            );
            return false;
        }

        if stmt.initializer.kind() == Kind::VariableDeclarationList {
            let variable_list = stmt.initializer;
            if !self.check_grammar_variable_declaration_list(variable_list) {
                let declarations = variable_list.as_variable_declaration_list().declarations;

                // declarations.length can be zero if there is an error in variable declaration in for-of or for-in
                // See http://www.ecma-international.org/ecma-262/6.0/#sec-for-in-and-for-of-statements for details
                // For example:
                //      var let = 10;
                //      for (let of [1,2,3]) {} // this is invalid ES6 syntax
                //      for (let in [1,2,3]) {} // this is invalid ES6 syntax
                // We will then want to skip on grammar checking on variableList declaration
                if declarations.nodes().is_empty() {
                    return false;
                }

                if declarations.nodes().len() > 1 {
                    let diagnostic = if for_in_or_of_statement.kind() == Kind::ForInStatement {
                        &diagnostics::ONLY_A_SINGLE_VARIABLE_DECLARATION_IS_ALLOWED_IN_A_FOR_IN_STATEMENT
                    } else {
                        &diagnostics::ONLY_A_SINGLE_VARIABLE_DECLARATION_IS_ALLOWED_IN_A_FOR_OF_STATEMENT
                    };
                    return self.grammar_error_on_first_token(
                        declarations.nodes()[1],
                        diagnostic,
                        &[],
                    );
                }

                let first_variable_declaration_node = declarations.nodes()[0];
                let first_variable_declaration =
                    first_variable_declaration_node.as_variable_declaration();
                if first_variable_declaration.initializer().is_some() {
                    let diagnostic = if for_in_or_of_statement.kind() == Kind::ForInStatement {
                        &diagnostics::THE_VARIABLE_DECLARATION_OF_A_FOR_IN_STATEMENT_CANNOT_HAVE_AN_INITIALIZER
                    } else {
                        &diagnostics::THE_VARIABLE_DECLARATION_OF_A_FOR_OF_STATEMENT_CANNOT_HAVE_AN_INITIALIZER
                    };
                    return self.grammar_error_on_node(
                        first_variable_declaration.name,
                        diagnostic,
                        &[],
                    );
                }
                if first_variable_declaration.type_().is_some() {
                    let diagnostic = if for_in_or_of_statement.kind() == Kind::ForInStatement {
                        &diagnostics::THE_LEFT_HAND_SIDE_OF_A_FOR_IN_STATEMENT_CANNOT_USE_A_TYPE_ANNOTATION
                    } else {
                        &diagnostics::THE_LEFT_HAND_SIDE_OF_A_FOR_OF_STATEMENT_CANNOT_USE_A_TYPE_ANNOTATION
                    };
                    return self.grammar_error_on_node(
                        first_variable_declaration_node,
                        diagnostic,
                        &[],
                    );
                }
            }
        }

        false
    }

    // grammarchecks.go:1289
    pub(crate) fn check_grammar_accessor(&mut self, accessor: P<Node>) -> bool {
        let body = accessor.body();
        let parent_kind = accessor.parent().unwrap().kind();
        if !accessor.flags().intersects(NodeFlags::Ambient)
            && parent_kind != Kind::TypeLiteral
            && parent_kind != Kind::InterfaceDeclaration
            && body.is_none()
            && !ast::has_syntactic_modifier(accessor, ModifierFlags::Abstract)
        {
            return self.grammar_error_at_pos(
                accessor,
                accessor
                    .end()
                    .expect_source("parsed nodes have source positions")
                    .checked_sub(1)
                    .expect("accessor ends after its terminator"),
                ";".len() as u32,
                &diagnostics::X_0_EXPECTED,
                &[&"{"],
            );
        }
        if let Some(body) = body {
            if ast::has_syntactic_modifier(accessor, ModifierFlags::Abstract) {
                return self.grammar_error_on_node(
                    accessor,
                    &diagnostics::AN_ABSTRACT_ACCESSOR_CANNOT_HAVE_AN_IMPLEMENTATION,
                    &[],
                );
            }
            if parent_kind == Kind::TypeLiteral || parent_kind == Kind::InterfaceDeclaration {
                return self.grammar_error_on_node(
                    body,
                    &diagnostics::AN_IMPLEMENTATION_CANNOT_BE_DECLARED_IN_AMBIENT_CONTEXTS,
                    &[],
                );
            }
        }

        let func_data = accessor.function_like_data();
        let mut type_parameters: Option<P<NodeList>> = None;
        if let Some(func_data) = func_data {
            type_parameters = func_data.type_parameters.get();
        }

        if type_parameters.is_some() {
            return self.grammar_error_on_node(
                accessor.name().unwrap(),
                &diagnostics::AN_ACCESSOR_CANNOT_HAVE_TYPE_PARAMETERS,
                &[],
            );
        }
        if !Self::does_accessor_have_correct_parameter_count(accessor) {
            return self.grammar_error_on_node(
                accessor.name().unwrap(),
                if accessor.kind() == Kind::GetAccessor {
                    &diagnostics::A_GET_ACCESSOR_CANNOT_HAVE_PARAMETERS
                } else {
                    &diagnostics::A_SET_ACCESSOR_MUST_HAVE_EXACTLY_ONE_PARAMETER
                },
                &[],
            );
        }
        if accessor.kind() == Kind::SetAccessor {
            if func_data.unwrap().type_.get().is_some() {
                return self.grammar_error_on_node(
                    accessor.name().unwrap(),
                    &diagnostics::A_SET_ACCESSOR_CANNOT_HAVE_A_RETURN_TYPE_ANNOTATION,
                    &[],
                );
            }

            let Some(parameter_node) = get_set_accessor_value_parameter(accessor) else {
                panic!("Return value does not match parameter count assertion.");
            };
            let parameter = parameter_node.as_parameter_declaration();
            if let Some(dot_dot_dot_token) = parameter.dot_dot_dot_token() {
                return self.grammar_error_on_node(
                    dot_dot_dot_token,
                    &diagnostics::A_SET_ACCESSOR_CANNOT_HAVE_REST_PARAMETER,
                    &[],
                );
            }
            if let Some(question_token) = parameter.question_token() {
                return self.grammar_error_on_node(
                    question_token,
                    &diagnostics::A_SET_ACCESSOR_CANNOT_HAVE_AN_OPTIONAL_PARAMETER,
                    &[],
                );
            }
            if parameter.initializer().is_some() {
                return self.grammar_error_on_node(
                    accessor.name().unwrap(),
                    &diagnostics::A_SET_ACCESSOR_PARAMETER_CANNOT_HAVE_AN_INITIALIZER,
                    &[],
                );
            }
        }

        false
    }

    // Does the accessor have the right number of parameters?
    //
    //	A `get` accessor has no parameters or a single `this` parameter.
    //	A `set` accessor has one parameter or a `this` parameter and one more parameter.
    // grammarchecks.go:1345
    fn does_accessor_have_correct_parameter_count(accessor: P<Node>) -> bool {
        // `getAccessorThisParameter` returns `nil` if the accessor's arity is incorrect,
        // even if there is a `this` parameter declared.
        Self::get_accessor_this_parameter(accessor).is_some()
            || accessor.parameters().len() == usize::from(accessor.kind() != Kind::GetAccessor)
    }

    // grammarchecks.go:1351
    pub(crate) fn check_grammar_type_operator_node(&mut self, node: P<Node>) -> bool {
        let type_operator = node.as_type_operator_node();
        if type_operator.operator == Kind::UniqueKeyword {
            let inner_type = type_operator.type_;
            if inner_type.kind() != Kind::SymbolKeyword {
                return self.grammar_error_on_node(
                    inner_type,
                    &diagnostics::X_0_EXPECTED,
                    &[&scanner::token_to_string(Kind::SymbolKeyword)],
                );
            }
            let parent = ast::walk_up_parenthesized_types(node.parent()).unwrap();
            match parent.kind() {
                Kind::VariableDeclaration => {
                    let decl = parent.as_variable_declaration();
                    if decl.name.kind() != Kind::Identifier {
                        return self.grammar_error_on_node(node, &diagnostics::X_UNIQUE_SYMBOL_TYPES_MAY_NOT_BE_USED_ON_A_VARIABLE_DECLARATION_WITH_A_BINDING_NAME, &[]);
                    }
                    if !crate::is_variable_declaration_in_variable_statement(parent) {
                        return self.grammar_error_on_node(node, &diagnostics::X_UNIQUE_SYMBOL_TYPES_ARE_ONLY_ALLOWED_ON_VARIABLES_IN_A_VARIABLE_STATEMENT, &[]);
                    }
                    if !parent.parent().unwrap().flags().intersects(NodeFlags::Const) {
                        return self.grammar_error_on_node(parent.as_variable_declaration().name, &diagnostics::A_VARIABLE_WHOSE_TYPE_IS_A_UNIQUE_SYMBOL_TYPE_MUST_BE_CONST, &[]);
                    }
                }
                Kind::PropertyDeclaration => {
                    if !ast::is_static(parent) || !has_readonly_modifier(parent) {
                        return self.grammar_error_on_node(parent.as_property_declaration().name(), &diagnostics::A_PROPERTY_OF_A_CLASS_WHOSE_TYPE_IS_A_UNIQUE_SYMBOL_TYPE_MUST_BE_BOTH_STATIC_AND_READONLY, &[]);
                    }
                }
                Kind::PropertySignature => {
                    if !ast::has_syntactic_modifier(parent, ModifierFlags::Readonly) {
                        return self.grammar_error_on_node(parent.as_property_signature_declaration().name(), &diagnostics::A_PROPERTY_OF_AN_INTERFACE_OR_TYPE_LITERAL_WHOSE_TYPE_IS_A_UNIQUE_SYMBOL_TYPE_MUST_BE_READONLY, &[]);
                    }
                }
                _ => {
                    return self.grammar_error_on_node(
                        node,
                        &diagnostics::X_UNIQUE_SYMBOL_TYPES_ARE_NOT_ALLOWED_HERE,
                        &[],
                    );
                }
            }
        } else if type_operator.operator == Kind::ReadonlyKeyword {
            let inner_type = type_operator.type_;
            if inner_type.kind() != Kind::ArrayType && inner_type.kind() != Kind::TupleType {
                return self.grammar_error_on_first_token(node, &diagnostics::X_READONLY_TYPE_MODIFIER_IS_ONLY_PERMITTED_ON_ARRAY_AND_TUPLE_LITERAL_TYPES, &[&scanner::token_to_string(Kind::SymbolKeyword)]);
            }
        }

        false
    }

    // grammarchecks.go:1391
    fn check_grammar_for_invalid_dynamic_name(
        &mut self,
        node: P<Node>,
        message: &'static Message,
    ) -> bool {
        if !self.is_non_bindable_dynamic_name(node) {
            return false;
        }
        let expression = if is_element_access_expression(node) {
            ast::skip_parentheses(node.as_element_access_expression().argument_expression)
        } else {
            node.expression().unwrap()
        };

        if !ast::is_entity_name_expression(expression) {
            return self.grammar_error_on_node(node, message, &[]);
        }

        false
    }

    // Indicates whether a declaration name is a dynamic name that cannot be late-bound.
    // grammarchecks.go:1410
    pub(crate) fn is_non_bindable_dynamic_name(&mut self, node: P<Node>) -> bool {
        ast::is_dynamic_name(node) && !self.is_late_bindable_name(node)
    }

    // grammarchecks.go:1414
    pub(crate) fn check_grammar_method(&mut self, node: P<Node>) -> bool {
        if self.check_grammar_function_like_declaration(node) {
            return true;
        }

        let parent = node.parent().unwrap();
        if node.kind() == Kind::MethodDeclaration {
            if parent.kind() == Kind::ObjectLiteralExpression {
                // We only disallow modifier on a method declaration if it is a property of object-literal-expression
                if let Some(modifiers) = node.modifiers() {
                    let nodes = modifiers.nodes();
                    if !(nodes.len() == 1 && nodes[0].kind() == Kind::AsyncKeyword) {
                        return self.grammar_error_on_first_token(
                            node,
                            &diagnostics::MODIFIERS_CANNOT_APPEAR_HERE,
                            &[],
                        );
                    }
                }

                let method_decl = node.as_method_declaration();
                if self.check_grammar_for_invalid_question_mark(
                    method_decl.named_member_base.postfix_token,
                    &diagnostics::AN_OBJECT_MEMBER_CANNOT_BE_DECLARED_OPTIONAL,
                ) {
                    return true;
                }
                if self.check_grammar_for_invalid_exclamation_token(
                    method_decl.named_member_base.postfix_token,
                    &diagnostics::A_DEFINITE_ASSIGNMENT_ASSERTION_IS_NOT_PERMITTED_IN_THIS_CONTEXT,
                ) {
                    return true;
                }
                if node.body().is_none() {
                    return self.grammar_error_at_pos(
                        node,
                        node.end()
                            .expect_source("parsed nodes have source positions")
                            .checked_sub(1)
                            .expect("method ends after its terminator"),
                        ";".len() as u32,
                        &diagnostics::X_0_EXPECTED,
                        &[&"{"],
                    );
                }
            }
            if self.check_grammar_for_generator(node) {
                return true;
            }
        }

        if ast::is_class_like(parent) {
            // Technically, computed properties in ambient contexts is disallowed
            // for property declarations and accessors too, not just methods.
            // However, property declarations disallow computed names in general,
            // and accessors are not allowed in ambient contexts in general,
            // so this error only really matters for methods.
            if node.flags().intersects(NodeFlags::Ambient) {
                return self.check_grammar_for_invalid_dynamic_name(node.name().unwrap(), &diagnostics::A_COMPUTED_PROPERTY_NAME_IN_AN_AMBIENT_CONTEXT_MUST_REFER_TO_AN_EXPRESSION_WHOSE_TYPE_IS_A_LITERAL_TYPE_OR_A_UNIQUE_SYMBOL_TYPE);
            } else if node.kind() == Kind::MethodDeclaration && node.body().is_none() {
                return self.check_grammar_for_invalid_dynamic_name(node.name().unwrap(), &diagnostics::A_COMPUTED_PROPERTY_NAME_IN_A_METHOD_OVERLOAD_MUST_REFER_TO_AN_EXPRESSION_WHOSE_TYPE_IS_A_LITERAL_TYPE_OR_A_UNIQUE_SYMBOL_TYPE);
            }
        } else if parent.kind() == Kind::InterfaceDeclaration {
            return self.check_grammar_for_invalid_dynamic_name(node.name().unwrap(), &diagnostics::A_COMPUTED_PROPERTY_NAME_IN_AN_INTERFACE_MUST_REFER_TO_AN_EXPRESSION_WHOSE_TYPE_IS_A_LITERAL_TYPE_OR_A_UNIQUE_SYMBOL_TYPE);
        } else if parent.kind() == Kind::TypeLiteral {
            return self.check_grammar_for_invalid_dynamic_name(node.name().unwrap(), &diagnostics::A_COMPUTED_PROPERTY_NAME_IN_A_TYPE_LITERAL_MUST_REFER_TO_AN_EXPRESSION_WHOSE_TYPE_IS_A_LITERAL_TYPE_OR_A_UNIQUE_SYMBOL_TYPE);
        }

        false
    }

    // grammarchecks.go:1462
    pub(crate) fn check_grammar_break_or_continue_statement(&mut self, node: P<Node>) -> bool {
        let target_label = node.label();
        let mut current: Option<P<Node>> = Some(node);
        while let Some(cur) = current {
            if ast::is_function_like_or_class_static_block_declaration(cur) {
                return self.grammar_error_on_node(
                    node,
                    &diagnostics::JUMP_TARGET_CANNOT_CROSS_FUNCTION_BOUNDARY,
                    &[],
                );
            }

            match cur.kind() {
                Kind::LabeledStatement => {
                    if let Some(target_label) = target_label
                        && cur.label().unwrap().text() == target_label.text()
                    {
                        // found matching label - verify that label usage is correct
                        // continue can only target labels that are on iteration statements
                        let is_misplaced_continue_label = node.kind() == Kind::ContinueStatement
                            && !ast::is_iteration_statement(
                                cur.statement(),
                                true, /*lookInLabeledStatements*/
                            );

                        if is_misplaced_continue_label {
                            return self.grammar_error_on_node(node, &diagnostics::A_CONTINUE_STATEMENT_CAN_ONLY_JUMP_TO_A_LABEL_OF_AN_ENCLOSING_ITERATION_STATEMENT, &[]);
                        }

                        return false;
                    }
                }
                Kind::SwitchStatement => {
                    if node.kind() == Kind::BreakStatement && target_label.is_none() {
                        // unlabeled break within switch statement - ok
                        return false;
                    }
                }
                _ => {
                    if ast::is_iteration_statement(cur, false /*lookInLabeledStatements*/)
                        && target_label.is_none()
                    {
                        // unlabeled break or continue within iteration statement - ok
                        return false;
                    }
                }
            }

            current = cur.parent();
        }

        if target_label.is_some() {
            let message = if node.kind() == Kind::BreakStatement {
                &diagnostics::A_BREAK_STATEMENT_CAN_ONLY_JUMP_TO_A_LABEL_OF_AN_ENCLOSING_STATEMENT
            } else {
                &diagnostics::A_CONTINUE_STATEMENT_CAN_ONLY_JUMP_TO_A_LABEL_OF_AN_ENCLOSING_ITERATION_STATEMENT
            };

            self.grammar_error_on_node(node, message, &[])
        } else {
            let message = if node.kind() == Kind::BreakStatement {
                &diagnostics::A_BREAK_STATEMENT_CAN_ONLY_BE_USED_WITHIN_AN_ENCLOSING_ITERATION_OR_SWITCH_STATEMENT
            } else {
                &diagnostics::A_CONTINUE_STATEMENT_CAN_ONLY_BE_USED_WITHIN_AN_ENCLOSING_ITERATION_STATEMENT
            };
            self.grammar_error_on_node(node, message, &[])
        }
    }

    // grammarchecks.go:1518
    pub(crate) fn check_grammar_binding_element(&mut self, node: P<Node>) -> bool {
        let binding_element = node.as_binding_element();
        if binding_element.dot_dot_dot_token().is_some() {
            let elements = node.parent().unwrap().element_list();
            if Some(node) != elements.nodes().last().copied() {
                return self.grammar_error_on_node(
                    node,
                    &diagnostics::A_REST_ELEMENT_MUST_BE_LAST_IN_A_DESTRUCTURING_PATTERN,
                    &[],
                );
            }
            self.check_grammar_for_disallowed_trailing_comma(
                Some(elements),
                &diagnostics::A_REST_PARAMETER_OR_BINDING_PATTERN_MAY_NOT_HAVE_A_TRAILING_COMMA,
            );

            if binding_element.property_name().is_some() {
                return self.grammar_error_on_node(
                    binding_element.name.unwrap(),
                    &diagnostics::A_REST_ELEMENT_CANNOT_HAVE_A_PROPERTY_NAME,
                    &[],
                );
            }
        }

        if binding_element.dot_dot_dot_token().is_some()
            && let Some(initializer) = binding_element.initializer()
        {
            // Error on equals token which immediately precedes the initializer
            return self.grammar_error_at_pos(
                node,
                initializer
                    .pos()
                    .expect_source("parsed nodes have source positions")
                    .checked_sub(1)
                    .expect("initializer starts after its equals token"),
                1,
                &diagnostics::A_REST_ELEMENT_CANNOT_HAVE_AN_INITIALIZER,
                &[],
            );
        }

        false
    }

    // grammarchecks.go:1539
    pub(crate) fn check_grammar_variable_declaration(&mut self, node: P<Node>) -> bool {
        let decl = node.as_variable_declaration();
        let node_flags = self.get_combined_node_flags_cached(node);
        let block_scope_kind = node_flags & NodeFlags::BlockScoped;
        if ast::is_binding_pattern(decl.name) {
            if block_scope_kind == NodeFlags::AwaitUsing {
                return self.grammar_error_on_node(
                    node,
                    &diagnostics::X_0_DECLARATIONS_MAY_NOT_HAVE_BINDING_PATTERNS,
                    &[&"await using"],
                );
            } else if block_scope_kind == NodeFlags::Using {
                return self.grammar_error_on_node(
                    node,
                    &diagnostics::X_0_DECLARATIONS_MAY_NOT_HAVE_BINDING_PATTERNS,
                    &[&"using"],
                );
            }
        }

        let parent = node.parent().unwrap();
        let grandparent = parent.parent().unwrap();
        if grandparent.kind() != Kind::ForInStatement && grandparent.kind() != Kind::ForOfStatement
        {
            if node_flags.intersects(NodeFlags::Ambient) {
                self.check_ambient_initializer(node);
            } else if decl.initializer().is_none() {
                if ast::is_binding_pattern(decl.name) && !ast::is_binding_pattern(parent) {
                    return self.grammar_error_on_node(
                        node,
                        &diagnostics::A_DESTRUCTURING_DECLARATION_MUST_HAVE_AN_INITIALIZER,
                        &[],
                    );
                }
                if block_scope_kind == NodeFlags::AwaitUsing {
                    return self.grammar_error_on_node(
                        node,
                        &diagnostics::X_0_DECLARATIONS_MUST_BE_INITIALIZED,
                        &[&"await using"],
                    );
                } else if block_scope_kind == NodeFlags::Using {
                    return self.grammar_error_on_node(
                        node,
                        &diagnostics::X_0_DECLARATIONS_MUST_BE_INITIALIZED,
                        &[&"using"],
                    );
                } else if block_scope_kind == NodeFlags::Const {
                    return self.grammar_error_on_node(
                        node,
                        &diagnostics::X_0_DECLARATIONS_MUST_BE_INITIALIZED,
                        &[&"const"],
                    );
                }
            }
        }

        if let Some(exclamation_token) = decl.exclamation_token()
            && (grandparent.kind() != Kind::VariableStatement
                || decl.type_().is_none()
                || decl.initializer().is_some()
                || node_flags.intersects(NodeFlags::Ambient))
        {
            let message = if decl.initializer().is_some() {
                &diagnostics::DECLARATIONS_WITH_INITIALIZERS_CANNOT_ALSO_HAVE_DEFINITE_ASSIGNMENT_ASSERTIONS
            } else if decl.type_().is_none() {
                &diagnostics::DECLARATIONS_WITH_DEFINITE_ASSIGNMENT_ASSERTIONS_MUST_ALSO_HAVE_TYPE_ANNOTATIONS
            } else {
                &diagnostics::A_DEFINITE_ASSIGNMENT_ASSERTION_IS_NOT_PERMITTED_IN_THIS_CONTEXT
            };
            return self.grammar_error_on_node(exclamation_token, message, &[]);
        }

        if self.emit_module_format_of_file(ast::get_source_file_of_node(node).unwrap())
            < ModuleKind::SYSTEM
            && !grandparent.flags().intersects(NodeFlags::Ambient)
            && ast::has_syntactic_modifier(grandparent, ModifierFlags::Export)
        {
            self.check_grammar_for_es_module_marker_in_binding_name(decl.name);
        }

        // 1. LexicalDeclaration : LetOrConst BindingList ;
        // It is a Syntax Error if the BoundNames of BindingList contains "let".
        // 2. ForDeclaration: ForDeclaration : LetOrConst ForBinding
        // It is a Syntax Error if the BoundNames of ForDeclaration contains "let".

        // It is a SyntaxError if a VariableDeclaration or VariableDeclarationNoIn occurs within strict code
        // and its Identifier is eval or arguments
        !block_scope_kind.is_empty()
            && self.check_grammar_name_in_let_or_const_declarations(decl.name)
    }

    // grammarchecks.go:1596
    fn check_grammar_for_es_module_marker_in_binding_name(&mut self, name: P<Node>) -> bool {
        if is_identifier(name) {
            if name.text() == "__esModule" {
                return self.grammar_error_on_node_skipped_on_no_emit(name, &diagnostics::IDENTIFIER_EXPECTED_ES_MODULE_IS_RESERVED_AS_AN_EXPORTED_MARKER_WHEN_TRANSFORMING_ECMA_SCRIPT_MODULES, &[]);
            }
        } else if let Some(result) = name.elements().iter().find_map(|&element| {
            if let Some(element_name) = element.name() {
                return Some(self.check_grammar_for_es_module_marker_in_binding_name(element_name));
            }

            None
        }) {
            return result;
        }
        false
    }

    // grammarchecks.go:1611
    fn check_grammar_name_in_let_or_const_declarations(&mut self, name: P<Node>) -> bool {
        if name.kind() == Kind::Identifier {
            if name.text() == "let" {
                return self.grammar_error_on_node(name, &diagnostics::X_LET_IS_NOT_ALLOWED_TO_BE_USED_AS_A_NAME_IN_LET_OR_CONST_DECLARATIONS, &[]);
            }
        } else {
            let elements = name.elements();
            for &element in elements {
                let binding_element = element.as_binding_element();
                if let Some(binding_element_name) = binding_element.name {
                    self.check_grammar_name_in_let_or_const_declarations(binding_element_name);
                }
            }
        }
        false
    }

    // grammarchecks.go:1628
    pub(crate) fn check_grammar_variable_declaration_list(
        &mut self,
        declaration_list: P<Node>,
    ) -> bool {
        let declarations = declaration_list.as_variable_declaration_list().declarations;
        if self.check_grammar_for_disallowed_trailing_comma(
            Some(declarations),
            &diagnostics::TRAILING_COMMA_NOT_ALLOWED,
        ) {
            return true;
        }

        if declarations.nodes().is_empty() {
            let start = declarations.pos().expect_source("parsed node lists have source positions");
            let end = declarations.end().expect_source("parsed node lists have source positions");
            return self.grammar_error_at_pos(
                declaration_list,
                start,
                end.checked_sub(start).expect("declaration list ends after it starts"),
                &diagnostics::VARIABLE_DECLARATION_LIST_CANNOT_BE_EMPTY,
                &[],
            );
        }

        let block_scope_flags = declaration_list.flags() & NodeFlags::BlockScoped;
        if block_scope_flags == NodeFlags::Using || block_scope_flags == NodeFlags::AwaitUsing {
            let parent = declaration_list.parent().unwrap();
            if is_for_in_statement(parent) {
                return self.grammar_error_on_node(
                    declaration_list,
                    if block_scope_flags == NodeFlags::Using {
                        &diagnostics::THE_LEFT_HAND_SIDE_OF_A_FOR_IN_STATEMENT_CANNOT_BE_A_USING_DECLARATION
                    } else {
                        &diagnostics::THE_LEFT_HAND_SIDE_OF_A_FOR_IN_STATEMENT_CANNOT_BE_AN_AWAIT_USING_DECLARATION
                    },
                    &[],
                );
            }
            if declaration_list.flags().intersects(NodeFlags::Ambient) {
                return self.grammar_error_on_node(
                    declaration_list,
                    if block_scope_flags == NodeFlags::Using {
                        &diagnostics::X_USING_DECLARATIONS_ARE_NOT_ALLOWED_IN_AMBIENT_CONTEXTS
                    } else {
                        &diagnostics::X_AWAIT_USING_DECLARATIONS_ARE_NOT_ALLOWED_IN_AMBIENT_CONTEXTS
                    },
                    &[],
                );
            }
            if is_variable_statement(parent)
                && (is_case_clause(parent.parent().unwrap())
                    || is_default_clause(parent.parent().unwrap()))
            {
                return self.grammar_error_on_node(
                    declaration_list,
                    if block_scope_flags == NodeFlags::Using {
                        &diagnostics::X_USING_DECLARATIONS_ARE_NOT_ALLOWED_IN_CASE_OR_DEFAULT_CLAUSES_UNLESS_CONTAINED_WITHIN_A_BLOCK
                    } else {
                        &diagnostics::X_AWAIT_USING_DECLARATIONS_ARE_NOT_ALLOWED_IN_CASE_OR_DEFAULT_CLAUSES_UNLESS_CONTAINED_WITHIN_A_BLOCK
                    },
                    &[],
                );
            }
        }

        if block_scope_flags == NodeFlags::AwaitUsing {
            return self.check_grammar_await_or_await_using(declaration_list);
        }

        false
    }

    // grammarchecks.go:1658
    pub(crate) fn check_grammar_await_or_await_using(&mut self, node: P<Node>) -> bool {
        // Grammar checking
        let mut has_error = false;
        let container = get_containing_function_or_class_static_block(node);
        if container.is_some_and(is_class_static_block_declaration) {
            // NOTE: We report this regardless as to whether there are parse diagnostics.
            let message = if is_await_expression(node) {
                &diagnostics::X_AWAIT_EXPRESSION_CANNOT_BE_USED_INSIDE_A_CLASS_STATIC_BLOCK
            } else {
                &diagnostics::X_AWAIT_USING_STATEMENTS_CANNOT_BE_USED_INSIDE_A_CLASS_STATIC_BLOCK
            };
            self.error(Some(node), message, &[]);
            has_error = true;
        } else if !node.flags().intersects(NodeFlags::AwaitContext) {
            if ast::is_in_top_level_context(node) {
                let source_file = ast::get_source_file_of_node(node).unwrap();
                if !Self::has_parse_diagnostics(source_file) {
                    let mut span = TextRange::new(0, 0);
                    let mut span_calculated = false;
                    if !ast::is_effective_external_module(source_file, &self.compiler_options) {
                        span = scanner::get_range_of_token_at_position(
                            source_file,
                            node.pos().expect_source("parsed nodes have source positions"),
                        );
                        span_calculated = true;
                        let message = if is_await_expression(node) {
                            &diagnostics::X_AWAIT_EXPRESSIONS_ARE_ONLY_ALLOWED_AT_THE_TOP_LEVEL_OF_A_FILE_WHEN_THAT_FILE_IS_A_MODULE_BUT_THIS_FILE_HAS_NO_IMPORTS_OR_EXPORTS_CONSIDER_ADDING_AN_EMPTY_EXPORT_TO_MAKE_THIS_FILE_A_MODULE
                        } else {
                            &diagnostics::X_AWAIT_USING_STATEMENTS_ARE_ONLY_ALLOWED_AT_THE_TOP_LEVEL_OF_A_FILE_WHEN_THAT_FILE_IS_A_MODULE_BUT_THIS_FILE_HAS_NO_IMPORTS_OR_EXPORTS_CONSIDER_ADDING_AN_EMPTY_EXPORT_TO_MAKE_THIS_FILE_A_MODULE
                        };
                        let diagnostic = ast::new_diagnostic(Some(source_file), span, message, &[]);
                        self.add_diagnostic(diagnostic);
                        has_error = true;
                    }
                    let mut fallthrough_es = false;
                    let mut fallthrough_default = false;
                    match self.module_kind {
                        ModuleKind::NODE16
                        | ModuleKind::NODE18
                        | ModuleKind::NODE20
                        | ModuleKind::NODE_NEXT => {
                            let source_file_meta_data =
                                self.program.get_source_file_meta_data(source_file.path());
                            if source_file_meta_data.implied_node_format == ModuleKind::COMMON_JS {
                                if !span_calculated {
                                    span = scanner::get_range_of_token_at_position(
                                        source_file,
                                        node.pos()
                                            .expect_source("parsed nodes have source positions"),
                                    );
                                }
                                self.add_diagnostic(ast::new_diagnostic(Some(source_file), span, &diagnostics::THE_CURRENT_FILE_IS_A_COMMON_JS_MODULE_AND_CANNOT_USE_AWAIT_AT_THE_TOP_LEVEL, &[]));
                                has_error = true;
                            } else {
                                fallthrough_es = true;
                            }
                        }
                        ModuleKind::ES2022
                        | ModuleKind::ES_NEXT
                        | ModuleKind::PRESERVE
                        | ModuleKind::SYSTEM => fallthrough_es = true,
                        _ => fallthrough_default = true,
                    }
                    if fallthrough_es && (self.language_version < ScriptTarget::ES2017) {
                        fallthrough_default = true;
                    }
                    if fallthrough_default {
                        if !span_calculated {
                            span = scanner::get_range_of_token_at_position(
                                source_file,
                                node.pos().expect_source("parsed nodes have source positions"),
                            );
                        }
                        let message = if is_await_expression(node) {
                            &diagnostics::TOP_LEVEL_AWAIT_EXPRESSIONS_ARE_ONLY_ALLOWED_WHEN_THE_MODULE_OPTION_IS_SET_TO_ES2022_ESNEXT_SYSTEM_NODE16_NODE18_NODE20_NODENEXT_OR_PRESERVE_AND_THE_TARGET_OPTION_IS_SET_TO_ES2017_OR_HIGHER
                        } else {
                            &diagnostics::TOP_LEVEL_AWAIT_USING_STATEMENTS_ARE_ONLY_ALLOWED_WHEN_THE_MODULE_OPTION_IS_SET_TO_ES2022_ESNEXT_SYSTEM_NODE16_NODE18_NODE20_NODENEXT_OR_PRESERVE_AND_THE_TARGET_OPTION_IS_SET_TO_ES2017_OR_HIGHER
                        };
                        self.add_diagnostic(ast::new_diagnostic(
                            Some(source_file),
                            span,
                            message,
                            &[],
                        ));
                        has_error = true;
                    }
                }
            } else {
                // use of 'await' in non-async function
                let source_file = ast::get_source_file_of_node(node).unwrap();
                if !Self::has_parse_diagnostics(source_file) {
                    let span = scanner::get_range_of_token_at_position(
                        source_file,
                        node.pos().expect_source("parsed nodes have source positions"),
                    );
                    let message = if is_await_expression(node) {
                        &diagnostics::X_AWAIT_EXPRESSIONS_ARE_ONLY_ALLOWED_WITHIN_ASYNC_FUNCTIONS_AND_AT_THE_TOP_LEVELS_OF_MODULES
                    } else {
                        &diagnostics::X_AWAIT_USING_STATEMENTS_ARE_ONLY_ALLOWED_WITHIN_ASYNC_FUNCTIONS_AND_AT_THE_TOP_LEVELS_OF_MODULES
                    };
                    let diagnostic = ast::new_diagnostic(Some(source_file), span, message, &[]);
                    if let Some(container) = container
                        && container.kind() != Kind::Constructor
                        && !has_async_modifier(container)
                    {
                        let related_info = new_diagnostic_for_node(
                            Some(container),
                            Some(&diagnostics::DID_YOU_MEAN_TO_MARK_THIS_FUNCTION_AS_ASYNC),
                            &[],
                        );
                        diagnostic.add_related_info(related_info);
                    }
                    self.add_diagnostic(diagnostic);
                    has_error = true;
                }
            }
        }

        if is_await_expression(node)
            && Self::is_in_parameter_initializer_before_containing_function(node)
        {
            // NOTE: We report this regardless as to whether there are parse diagnostics.
            self.error(
                Some(node),
                &diagnostics::X_AWAIT_EXPRESSIONS_CANNOT_BE_USED_IN_A_PARAMETER_INITIALIZER,
                &[],
            );
            has_error = true;
        }

        has_error
    }

    // grammarchecks.go:1759
    pub(crate) fn check_grammar_yield_expression(&mut self, node: P<Node>) -> bool {
        let mut has_error = false;
        if !node.flags().intersects(NodeFlags::YieldContext) {
            self.grammar_error_on_first_token(
                node,
                &diagnostics::A_YIELD_EXPRESSION_IS_ONLY_ALLOWED_IN_A_GENERATOR_BODY,
                &[],
            );
            has_error = true;
        }
        if Self::is_in_parameter_initializer_before_containing_function(node) {
            self.error(
                Some(node),
                &diagnostics::X_YIELD_EXPRESSIONS_CANNOT_BE_USED_IN_A_PARAMETER_INITIALIZER,
                &[],
            );
            has_error = true;
        }
        has_error
    }

    // grammarchecks.go:1772
    pub(crate) fn check_grammar_for_disallowed_block_scoped_variable_statement(
        &mut self,
        node: P<Node>,
    ) -> bool {
        if !Self::container_allows_block_scoped_variable(node.parent().unwrap()) {
            let block_scope_kind = self
                .get_combined_node_flags_cached(node.as_variable_statement().declaration_list)
                & NodeFlags::BlockScoped;
            if !block_scope_kind.is_empty() {
                let keyword = if block_scope_kind == NodeFlags::Let {
                    "let"
                } else if block_scope_kind == NodeFlags::Const {
                    "const"
                } else if block_scope_kind == NodeFlags::Using {
                    "using"
                } else if block_scope_kind == NodeFlags::AwaitUsing {
                    "await using"
                } else {
                    panic!("Unknown BlockScope flag")
                };
                self.error(
                    Some(node),
                    &diagnostics::X_0_DECLARATIONS_CAN_ONLY_BE_DECLARED_INSIDE_A_BLOCK,
                    &[&keyword],
                );
            }
        }

        false
    }

    // grammarchecks.go:1796
    pub(crate) fn container_allows_block_scoped_variable(parent: P<Node>) -> bool {
        match parent.kind() {
            Kind::IfStatement
            | Kind::DoStatement
            | Kind::WhileStatement
            | Kind::WithStatement
            | Kind::ForStatement
            | Kind::ForInStatement
            | Kind::ForOfStatement => return false,
            Kind::LabeledStatement => {
                return Self::container_allows_block_scoped_variable(parent.parent().unwrap());
            }
            _ => {}
        }

        true
    }

    // grammarchecks.go:1813
    pub(crate) fn check_grammar_meta_property(&mut self, node: P<Node>) -> bool {
        let meta_property = node.as_meta_property();
        let node_name = meta_property.name;
        let name_text = node_name.text();

        match meta_property.keyword_token {
            Kind::NewKeyword => {
                if name_text != "target" {
                    return self.grammar_error_on_node(
                        node_name,
                        &diagnostics::X_0_IS_NOT_A_VALID_META_PROPERTY_FOR_KEYWORD_1_DID_YOU_MEAN_2,
                        &[
                            &name_text,
                            &scanner::token_to_string(meta_property.keyword_token),
                            &"target",
                        ],
                    );
                }
            }
            Kind::ImportKeyword if name_text != "meta" => {
                let parent = node.parent().unwrap();
                let is_callee = is_call_expression(parent) && parent.expression() == Some(node);
                if ast::is_import_phase_meta_property(node) {
                    if !is_callee {
                        return self.grammar_error_at_pos(
                            node,
                            node.end().expect_source("parsed nodes have source positions"),
                            0,
                            &diagnostics::X_0_EXPECTED,
                            &[&"("],
                        );
                    }
                } else {
                    if is_callee {
                        return self.grammar_error_on_node(node_name, &diagnostics::X_0_IS_NOT_A_VALID_META_PROPERTY_FOR_KEYWORD_IMPORT_DID_YOU_MEAN_META_DEFER_OR_SOURCE, &[&name_text]);
                    }
                    return self.grammar_error_on_node(
                        node_name,
                        &diagnostics::X_0_IS_NOT_A_VALID_META_PROPERTY_FOR_KEYWORD_1_DID_YOU_MEAN_2,
                        &[
                            &name_text,
                            &scanner::token_to_string(meta_property.keyword_token),
                            &"meta",
                        ],
                    );
                }
            }
            _ => {}
        }

        false
    }

    // grammarchecks.go:1841
    pub(crate) fn check_grammar_constructor_type_parameters(&mut self, node: P<Node>) -> bool {
        let range_ = node.as_constructor_declaration().function_like_base.type_parameters.get();
        if let Some(range_) = range_ {
            let end = range_.end().expect_source("parsed node lists have source positions");
            let pos = if range_.pos() == range_.end() {
                range_.pos().expect_source("parsed node lists have source positions")
            } else {
                scanner::skip_trivia(
                    ast::get_source_file_of_node(node).unwrap().text(),
                    range_.pos().expect_source("parsed node lists have source positions"),
                )
            };
            return self.grammar_error_at_pos(
                node,
                pos,
                end.checked_sub(pos).expect("type parameter list ends after it starts"),
                &diagnostics::TYPE_PARAMETERS_CANNOT_APPEAR_ON_A_CONSTRUCTOR_DECLARATION,
                &[],
            );
        }

        false
    }

    // grammarchecks.go:1856
    pub(crate) fn check_grammar_constructor_type_annotation(&mut self, node: P<Node>) -> bool {
        let t = node.as_constructor_declaration().function_like_base.type_.get();
        if let Some(t) = t {
            return self.grammar_error_on_node(
                t,
                &diagnostics::TYPE_ANNOTATION_CANNOT_APPEAR_ON_A_CONSTRUCTOR_DECLARATION,
                &[],
            );
        }
        false
    }

    // grammarchecks.go:1864
    pub(crate) fn check_grammar_property(&mut self, node: P<Node>) -> bool {
        let property_name = node.name().unwrap();
        let parent = node.parent().unwrap();
        if is_computed_property_name(property_name)
            && is_binary_expression(property_name.expression().unwrap())
            && property_name.expression().unwrap().as_binary_expression().operator_token.kind()
                == Kind::InKeyword
        {
            return self.grammar_error_on_node(
                parent.members()[0],
                &diagnostics::A_MAPPED_TYPE_MAY_NOT_DECLARE_PROPERTIES_OR_METHODS,
                &[],
            );
        }
        if ast::is_class_like(parent) {
            if is_string_literal(property_name) && property_name.text() == "constructor" {
                return self.grammar_error_on_node(
                    property_name,
                    &diagnostics::CLASSES_MAY_NOT_HAVE_A_FIELD_NAMED_CONSTRUCTOR,
                    &[],
                );
            }
            if self.check_grammar_for_invalid_dynamic_name(property_name, &diagnostics::A_COMPUTED_PROPERTY_NAME_IN_A_CLASS_PROPERTY_DECLARATION_MUST_HAVE_A_SIMPLE_LITERAL_TYPE_OR_A_UNIQUE_SYMBOL_TYPE) {
                return true;
            }
            if ast::is_auto_accessor_property_declaration(node)
                && self.check_grammar_for_invalid_question_mark(
                    node.postfix_token(),
                    &diagnostics::AN_ACCESSOR_PROPERTY_CANNOT_BE_DECLARED_OPTIONAL,
                )
            {
                return true;
            }
        } else if is_interface_declaration(parent) {
            if self.check_grammar_for_invalid_dynamic_name(property_name, &diagnostics::A_COMPUTED_PROPERTY_NAME_IN_AN_INTERFACE_MUST_REFER_TO_AN_EXPRESSION_WHOSE_TYPE_IS_A_LITERAL_TYPE_OR_A_UNIQUE_SYMBOL_TYPE) {
                return true;
            }
            // Interfaces cannot contain property declarations
            assert!(
                ast::is_property_signature_declaration(node),
                "Unexpected node kind {:?}",
                node.kind()
            );
            if let Some(initializer) = node.initializer() {
                return self.grammar_error_on_node(
                    initializer,
                    &diagnostics::AN_INTERFACE_PROPERTY_CANNOT_HAVE_AN_INITIALIZER,
                    &[],
                );
            }
        } else if is_type_literal_node(parent) {
            if self.check_grammar_for_invalid_dynamic_name(node.name().unwrap(), &diagnostics::A_COMPUTED_PROPERTY_NAME_IN_A_TYPE_LITERAL_MUST_REFER_TO_AN_EXPRESSION_WHOSE_TYPE_IS_A_LITERAL_TYPE_OR_A_UNIQUE_SYMBOL_TYPE) {
                return true;
            }
            // Type literals cannot contain property declarations
            assert!(
                ast::is_property_signature_declaration(node),
                "Unexpected node kind {:?}",
                node.kind()
            );
            if let Some(initializer) = node.initializer() {
                return self.grammar_error_on_node(
                    initializer,
                    &diagnostics::A_TYPE_LITERAL_PROPERTY_CANNOT_HAVE_AN_INITIALIZER,
                    &[],
                );
            }
        }

        if node.flags().intersects(NodeFlags::Ambient) {
            self.check_ambient_initializer(node);
        }

        if is_property_declaration(node) {
            let prop_decl = node.as_property_declaration();
            let postfix_token = prop_decl.named_member_base.postfix_token;
            if let Some(postfix_token) = postfix_token
                && postfix_token.kind() == Kind::ExclamationToken
            {
                if prop_decl.initializer().is_some() {
                    return self.grammar_error_on_node(postfix_token, &diagnostics::DECLARATIONS_WITH_INITIALIZERS_CANNOT_ALSO_HAVE_DEFINITE_ASSIGNMENT_ASSERTIONS, &[]);
                } else if prop_decl.type_().is_none() {
                    return self.grammar_error_on_node(postfix_token, &diagnostics::DECLARATIONS_WITH_DEFINITE_ASSIGNMENT_ASSERTIONS_MUST_ALSO_HAVE_TYPE_ANNOTATIONS, &[]);
                } else if !ast::is_class_like(parent)
                    || node.flags().intersects(NodeFlags::Ambient)
                    || ast::is_static(node)
                    || ast::has_abstract_modifier(node)
                {
                    return self.grammar_error_on_node(postfix_token, &diagnostics::A_DEFINITE_ASSIGNMENT_ASSERTION_IS_NOT_PERMITTED_IN_THIS_CONTEXT, &[]);
                }
            }
        }

        false
    }

    // grammarchecks.go:1925
    fn check_ambient_initializer(&mut self, node: P<Node>) -> bool {
        let initializer: Option<P<Node>>;
        let type_node: Option<P<Node>>;
        match node.kind() {
            Kind::VariableDeclaration => {
                let var_decl = node.as_variable_declaration();
                initializer = var_decl.initializer();
                type_node = var_decl.type_();
            }
            Kind::PropertyDeclaration => {
                let prop_decl = node.as_property_declaration();
                initializer = prop_decl.initializer();
                type_node = prop_decl.type_();
            }
            Kind::PropertySignature => {
                let prop_sig = node.as_property_signature_declaration();
                initializer = prop_sig.initializer();
                type_node = prop_sig.type_();
            }
            _ => panic!("Unexpected node kind {:?}", node.kind()),
        }

        if let Some(initializer) = initializer {
            let is_invalid_initializer =
                !(is_initializer_string_or_number_literal_expression(initializer)
                    || self.is_initializer_simple_literal_enum_reference(initializer)
                    || initializer.kind() == Kind::TrueKeyword
                    || initializer.kind() == Kind::FalseKeyword
                    || is_initializer_big_int_literal_expression(initializer));
            let is_const_or_readonly = crate::is_declaration_readonly(node)
                || is_variable_declaration(node) && self.is_var_const_like(node);
            if is_const_or_readonly && type_node.is_none() {
                if is_invalid_initializer {
                    return self.grammar_error_on_node(initializer, &diagnostics::A_CONST_INITIALIZER_IN_AN_AMBIENT_CONTEXT_MUST_BE_A_STRING_OR_NUMERIC_LITERAL_OR_LITERAL_ENUM_REFERENCE, &[]);
                }
            } else {
                return self.grammar_error_on_node(
                    initializer,
                    &diagnostics::INITIALIZERS_ARE_NOT_ALLOWED_IN_AMBIENT_CONTEXTS,
                    &[],
                );
            }
        }

        false
    }
}

// grammarchecks.go:1960
fn is_initializer_string_or_number_literal_expression(expr: P<Node>) -> bool {
    ast::is_string_or_numeric_literal_like(expr)
        || expr.kind() == Kind::PrefixUnaryExpression
            && expr.as_prefix_unary_expression().operator == Kind::MinusToken
            && expr.as_prefix_unary_expression().operand.kind() == Kind::NumericLiteral
}

// grammarchecks.go:1965
fn is_initializer_big_int_literal_expression(expr: P<Node>) -> bool {
    if expr.kind() == Kind::BigIntLiteral {
        return true;
    }

    if expr.kind() == Kind::PrefixUnaryExpression {
        let unary_expr = expr.as_prefix_unary_expression();
        return unary_expr.operator == Kind::MinusToken
            && unary_expr.operand.kind() == Kind::BigIntLiteral;
    }

    false
}

impl Checker {
    // grammarchecks.go:1978
    fn is_initializer_simple_literal_enum_reference(&mut self, expr: P<Node>) -> bool {
        if is_property_access_expression(expr) {
            return self.check_expression_cached(expr).flags().intersects(TypeFlags::EnumLike);
        }

        if is_element_access_expression(expr) {
            let element_access = expr.as_element_access_expression();

            return is_initializer_string_or_number_literal_expression(
                element_access.argument_expression,
            ) && ast::is_entity_name_expression(element_access.expression)
                && self.check_expression_cached(expr).flags().intersects(TypeFlags::EnumLike);
        }

        false
    }

    // grammarchecks.go:1994
    fn check_grammar_top_level_element_for_required_declare_modifier(
        &mut self,
        node: P<Node>,
    ) -> bool {
        // A declare modifier is required for any top level .d.ts declaration except export=, export default, export as namespace
        // interfaces and imports categories:
        //
        //  DeclarationElement:
        //     ExportAssignment
        //     export_opt   InterfaceDeclaration
        //     export_opt   TypeAliasDeclaration
        //     export_opt   ImportDeclaration
        //     export_opt   ExternalImportDeclaration
        //     export_opt   AmbientDeclaration
        //
        // TODO: The spec needs to be amended to reflect this grammar.
        if node.kind() == Kind::InterfaceDeclaration
            || node.kind() == Kind::TypeAliasDeclaration
            || node.kind() == Kind::ImportDeclaration
            || node.kind() == Kind::JSImportDeclaration
            || node.kind() == Kind::ImportEqualsDeclaration
            || node.kind() == Kind::ExportDeclaration
            || node.kind() == Kind::ExportAssignment
            || node.kind() == Kind::NamespaceExportDeclaration
            || ast::has_syntactic_modifier(
                node,
                ModifierFlags::Ambient | ModifierFlags::Export | ModifierFlags::Default,
            )
        {
            return false;
        }

        self.grammar_error_on_first_token(node, &diagnostics::TOP_LEVEL_DECLARATIONS_IN_D_TS_FILES_MUST_START_WITH_EITHER_A_DECLARE_OR_EXPORT_MODIFIER, &[])
    }

    // grammarchecks.go:2014
    fn check_grammar_top_level_elements_for_required_declare_modifier(
        &mut self,
        file: P<SourceFile>,
    ) -> bool {
        file.statements.nodes().iter().any(|&decl| {
            if (ast::is_declaration_node(decl) || decl.kind() == Kind::VariableStatement)
                && self.check_grammar_top_level_element_for_required_declare_modifier(decl)
            {
                return true;
            }

            false
        })
    }

    // grammarchecks.go:2025
    pub(crate) fn check_grammar_source_file(&mut self, node: P<SourceFile>) -> bool {
        node.as_node().flags().intersects(NodeFlags::Ambient)
            && self.check_grammar_top_level_elements_for_required_declare_modifier(node)
    }

    // grammarchecks.go:2029
    pub(crate) fn check_grammar_statement_in_ambient_context(&mut self, node: P<Node>) -> bool {
        if node.flags().intersects(NodeFlags::Ambient) {
            // Find containing block which is either Block, ModuleBlock, SourceFile
            let links = self.node_links.get(node);
            let parent = node.parent().unwrap();
            if !links.has_reported_statement_in_ambient_context.get()
                && (ast::is_function_like(parent) || ast::is_accessor(parent))
            {
                let reported = self.grammar_error_on_first_token(
                    node,
                    &diagnostics::AN_IMPLEMENTATION_CANNOT_BE_DECLARED_IN_AMBIENT_CONTEXTS,
                    &[],
                );
                links.has_reported_statement_in_ambient_context.set(reported);
                return links.has_reported_statement_in_ambient_context.get();
            }

            // We are either parented by another statement, or some sort of block.
            // If we're in a block, we only want to really report an error once
            // to prevent noisiness.  So use a bit on the block to indicate if
            // this has already been reported, and don't report if it has.
            //
            if parent.kind() == Kind::Block
                || parent.kind() == Kind::ModuleBlock
                || parent.kind() == Kind::SourceFile
            {
                let links = self.node_links.get(parent);
                // Check if the containing block ever report this error
                if !links.has_reported_statement_in_ambient_context.get() {
                    let reported = self.grammar_error_on_first_token(
                        node,
                        &diagnostics::STATEMENTS_ARE_NOT_ALLOWED_IN_AMBIENT_CONTEXTS,
                        &[],
                    );
                    links.has_reported_statement_in_ambient_context.set(reported);
                    return links.has_reported_statement_in_ambient_context.get();
                }
            } else {
                // We must be parented by a statement.  If so, there's no need
                // to report the error as our parent will have already done it.
                // debug.Assert(ast.IsStatement(node.Parent)) // !!! commented out in strada - fails if uncommented
            }
        }
        false
    }

    // grammarchecks.go:2059
    pub(crate) fn check_grammar_numeric_literal(&mut self, node: P<Node>) {
        let node_text = scanner::get_text_of_node(node);
        let numeric_literal = node.as_numeric_literal();

        // Realism (size) checking
        // We should test against `getTextOfNode(node)` rather than `node.text`, because `node.text` for large numeric literals can contain "."
        // e.g. `node.text` for numeric literal `1100000000000000000000` is `1.1e21`.
        let is_fractional = node_text.contains('.');
        let is_scientific = numeric_literal
            .literal_like_node_base
            .token_flags
            .get()
            .intersects(TokenFlags::Scientific);

        // Scientific notation (e.g. 2e54 and 1e00000000010) can't be converted to bigint
        // Fractional numbers (e.g. 9000000000000000.001) are inherently imprecise anyway
        if is_fractional || is_scientific {
            return;
        }

        // Here `node` is guaranteed to be a numeric literal representing an integer.
        // We need to judge whether the integer `node` represents is <= 2 ** 53 - 1, which can be accomplished by comparing to `value` defined below because:
        // 1) when `node` represents an integer <= 2 ** 53 - 1, `node.text` is its exact string representation and thus `value` precisely represents the integer.
        // 2) otherwise, although `node.text` may be imprecise string representation, its mathematical value and consequently `value` cannot be less than 2 ** 53,
        //    thus the result of the predicate won't be affected.
        let value = jsnum::from_string(numeric_literal.literal_like_node_base.text.as_str());
        if value <= jsnum::MAX_SAFE_INTEGER {
            return;
        }

        self.add_error_or_suggestion(false, create_diagnostic_for_node(Some(node), &diagnostics::NUMERIC_LITERALS_WITH_ABSOLUTE_VALUES_EQUAL_TO_2_53_OR_GREATER_ARE_TOO_LARGE_TO_BE_REPRESENTED_ACCURATELY_AS_INTEGERS, &[]));
    }

    // grammarchecks.go:2087
    pub(crate) fn check_grammar_big_int_literal(&mut self, node: P<Node>) -> bool {
        let parent = node.parent().unwrap();
        let literal_type = is_literal_type_node(parent)
            || is_prefix_unary_expression(parent) && is_literal_type_node(parent.parent().unwrap());
        if !literal_type {
            // Don't error on BigInt literals in ambient contexts
            if !node.flags().intersects(NodeFlags::Ambient)
                && self.language_version < ScriptTarget::ES2020
                && self.grammar_error_on_node(node, &diagnostics::BIG_INT_LITERALS_ARE_NOT_AVAILABLE_WHEN_TARGETING_LOWER_THAN_ES2020, &[]) {
                    return true;
                }
        }
        false
    }

    // grammarchecks.go:2100
    pub(crate) fn check_grammar_import_clause(&mut self, node: P<Node>) -> bool {
        let import_clause = node.as_import_clause();
        match import_clause.phase_modifier.get() {
            Kind::TypeKeyword => {
                if !node.flags().intersects(NodeFlags::JSDoc)
                    && import_clause.name.is_some()
                    && import_clause.named_bindings.is_some()
                {
                    return self.grammar_error_on_node(node, &diagnostics::A_TYPE_ONLY_IMPORT_CAN_SPECIFY_A_DEFAULT_IMPORT_OR_NAMED_BINDINGS_BUT_NOT_BOTH, &[]);
                }
                if let Some(named_bindings) = import_clause.named_bindings
                    && named_bindings.kind() == Kind::NamedImports
                {
                    return self.check_grammar_type_only_named_imports_or_exports(named_bindings);
                }
            }
            Kind::DeferKeyword => {
                if import_clause.name.is_some() {
                    return self.grammar_error_on_node(
                        node,
                        &diagnostics::DEFAULT_IMPORTS_ARE_NOT_ALLOWED_IN_A_DEFERRED_IMPORT,
                        &[],
                    );
                }
                let Some(named_bindings) = import_clause.named_bindings else {
                    return self.grammar_error_on_node(
                        node,
                        &diagnostics::A_DEFERRED_IMPORT_MUST_SPECIFY_A_NAMESPACE_BINDING,
                        &[],
                    );
                };
                if named_bindings.kind() == Kind::NamedImports {
                    return self.grammar_error_on_node(
                        node,
                        &diagnostics::NAMED_IMPORTS_ARE_NOT_ALLOWED_IN_A_DEFERRED_IMPORT,
                        &[],
                    );
                }
                if !self.module_kind.supports_deferred_imports() {
                    return self.grammar_error_on_node(node, &diagnostics::DEFERRED_IMPORTS_ARE_ONLY_SUPPORTED_WHEN_THE_MODULE_FLAG_IS_SET_TO_ESNEXT_OR_PRESERVE, &[]);
                }
            }
            Kind::SourceKeyword => {
                if import_clause.named_bindings.is_some() {
                    return self.grammar_error_on_node(node, &diagnostics::NAMED_AND_NAMESPACE_IMPORTS_ARE_NOT_ALLOWED_IN_A_SOURCE_PHASE_IMPORT, &[]);
                }
                if import_clause.name.is_none() {
                    return self.grammar_error_on_node(
                        node,
                        &diagnostics::A_SOURCE_PHASE_IMPORT_MUST_SPECIFY_A_LOCAL_BINDING,
                        &[],
                    );
                }
                if self.module_kind.supports_source_phase_imports() {
                    let module_specifier =
                        get_module_specifier_from_node(node.parent().unwrap()).unwrap();
                    if self.get_emit_syntax_for_module_specifier_expression(module_specifier)
                        == ModuleKind::COMMON_JS
                    {
                        return self.grammar_error_on_node(node, &diagnostics::SOURCE_PHASE_IMPORTS_ARE_NOT_ALLOWED_ON_STATEMENTS_THAT_COMPILE_TO_COMMON_JS_REQUIRE_CALLS, &[]);
                    }
                } else {
                    return self.grammar_error_on_node(node, &diagnostics::SOURCE_PHASE_IMPORTS_ARE_ONLY_SUPPORTED_WHEN_THE_MODULE_OPTION_IS_SET_TO_ESNEXT_NODENEXT_OR_PRESERVE, &[]);
                }
            }
            _ => {}
        }
        false
    }

    // grammarchecks.go:2123
    pub(crate) fn check_grammar_import_attribute_values(&mut self, node: P<Node>) -> bool {
        let mut has_error = false;
        for &attribute in node.as_import_attributes().attributes.nodes() {
            let value = attribute.as_import_attribute().value;
            if is_string_literal(value) {
                continue;
            }
            has_error = true;
            self.error(
                Some(value),
                &diagnostics::IMPORT_ATTRIBUTE_VALUES_MUST_BE_STRING_LITERAL_EXPRESSIONS,
                &[],
            );
        }
        has_error
    }

    // grammarchecks.go:2136
    fn check_grammar_type_only_named_imports_or_exports(
        &mut self,
        named_bindings: P<Node>,
    ) -> bool {
        let node_list = named_bindings.element_list();
        if let Some(result) = node_list.nodes().iter().find_map(|&specifier| {
            let specifier_is_type_only: bool;
            let message: &'static Message;
            if specifier.kind() == Kind::ImportSpecifier {
                specifier_is_type_only = specifier.is_type_only();
                message = &diagnostics::THE_TYPE_MODIFIER_CANNOT_BE_USED_ON_A_NAMED_IMPORT_WHEN_IMPORT_TYPE_IS_USED_ON_ITS_IMPORT_STATEMENT;
            } else {
                specifier_is_type_only = specifier.is_type_only();
                message = &diagnostics::THE_TYPE_MODIFIER_CANNOT_BE_USED_ON_A_NAMED_EXPORT_WHEN_EXPORT_TYPE_IS_USED_ON_ITS_EXPORT_STATEMENT;
            }

            if specifier_is_type_only {
                return Some(self.grammar_error_on_first_token(specifier, message, &[]));
            }

            None
        }) {
            return result;
        }

        false
    }

    // grammarchecks.go:2157
    pub(crate) fn check_grammar_import_call_expression(&mut self, node: P<Node>) -> bool {
        if self.compiler_options.verbatim_module_syntax == Tristate::True
            && self.module_kind == ModuleKind::COMMON_JS
        {
            return self.grammar_error_on_node(
                node,
                get_verbatim_module_syntax_error_message(node),
                &[],
            );
        }

        let expression = node.expression().unwrap();
        if ast::is_import_source_meta_property(expression) {
            if !self.module_kind.supports_source_phase_imports() {
                return self.grammar_error_on_node(node, &diagnostics::SOURCE_PHASE_IMPORTS_ARE_ONLY_SUPPORTED_WHEN_THE_MODULE_OPTION_IS_SET_TO_ESNEXT_NODENEXT_OR_PRESERVE, &[]);
            }
        } else if ast::is_import_defer_meta_property(expression) {
            if !self.module_kind.supports_deferred_imports() {
                return self.grammar_error_on_node(node, &diagnostics::DEFERRED_IMPORTS_ARE_ONLY_SUPPORTED_WHEN_THE_MODULE_FLAG_IS_SET_TO_ESNEXT_OR_PRESERVE, &[]);
            }
        } else if self.module_kind == ModuleKind::ES2015 {
            return self.grammar_error_on_node(node, &diagnostics::DYNAMIC_IMPORTS_ARE_ONLY_SUPPORTED_WHEN_THE_MODULE_FLAG_IS_SET_TO_ES2020_ES2022_ESNEXT_COMMONJS_AMD_SYSTEM_UMD_NODE16_NODE18_NODE20_OR_NODENEXT, &[]);
        }

        let node_as_call = node.as_call_expression();
        if ast::is_source_phase_import_call(node)
            && let Some(question_dot_token) = node_as_call.question_dot_token()
        {
            return self.grammar_error_on_node(
                question_dot_token,
                &diagnostics::OPTIONAL_CHAINING_CANNOT_BE_USED_WITH_IMPORT_SOURCE,
                &[],
            );
        }
        if node_as_call.type_arguments().is_some() {
            return self.grammar_error_on_node(node, &diagnostics::THIS_USE_OF_IMPORT_IS_INVALID_IMPORT_CALLS_CAN_BE_WRITTEN_BUT_THEY_MUST_HAVE_PARENTHESES_AND_CANNOT_HAVE_TYPE_ARGUMENTS, &[]);
        }

        let node_arguments = node_as_call.arguments;
        let argument_nodes = node_arguments.nodes();
        if !(ModuleKind::NODE16 <= self.module_kind && self.module_kind <= ModuleKind::NODE_NEXT)
            && self.module_kind != ModuleKind::ES_NEXT
            && self.module_kind != ModuleKind::PRESERVE
        {
            // We are allowed trailing comma after proposal-import-assertions.
            self.check_grammar_for_disallowed_trailing_comma(
                Some(node_arguments),
                &diagnostics::TRAILING_COMMA_NOT_ALLOWED,
            );

            if argument_nodes.len() > 1 {
                let import_attributes_argument = argument_nodes[1];
                return self.grammar_error_on_node(import_attributes_argument, &diagnostics::DYNAMIC_IMPORTS_ONLY_SUPPORT_A_SECOND_ARGUMENT_WHEN_THE_MODULE_OPTION_IS_SET_TO_ESNEXT_NODE16_NODE18_NODE20_NODENEXT_OR_PRESERVE, &[]);
            }
        }

        if argument_nodes.is_empty() || argument_nodes.len() > 2 {
            return self.grammar_error_on_node(node, &diagnostics::DYNAMIC_IMPORTS_CAN_ONLY_ACCEPT_A_MODULE_SPECIFIER_AND_AN_OPTIONAL_SET_OF_ATTRIBUTES_AS_ARGUMENTS, &[]);
        }

        // see: parseArgumentOrArrayLiteralElement...we use this function which parse arguments of callExpression to parse specifier for dynamic import.
        // parseArgumentOrArrayLiteralElement allows spread element to be in an argument list which is not allowed as specifier in dynamic import.
        let spread_element = argument_nodes.iter().copied().find(|&n| is_spread_element(n));
        if let Some(spread_element) = spread_element {
            return self.grammar_error_on_node(
                spread_element,
                &diagnostics::ARGUMENT_OF_DYNAMIC_IMPORT_CANNOT_BE_SPREAD_ELEMENT,
                &[],
            );
        }
        false
    }

    // grammarchecks.go:2200
    pub(crate) fn check_grammar_import_attributes_type(&mut self, attributes: P<Node>) -> bool {
        let members = attributes.as_type_literal_node().members;
        for &member in members.nodes() {
            if member.kind() != Kind::PropertySignature {
                return self.grammar_error_on_node(
                    member,
                    &diagnostics::AN_IMPORT_ATTRIBUTES_TYPE_MAY_ONLY_CONTAIN_PROPERTY_SIGNATURES,
                    &[],
                );
            }
            let property_signature = member.as_property_signature_declaration();
            if let Some(modifiers) = member.modifiers()
                && let Some(result) = modifiers.nodes().iter().find_map(|&modifier| {
                    if modifier.kind() == Kind::ReadonlyKeyword {
                        Some(self.grammar_error_on_node(modifier, &diagnostics::AN_IMPORT_ATTRIBUTES_PROPERTY_CANNOT_HAVE_A_READONLY_MODIFIER, &[]))
                    } else {
                        None
                    }
                }) {
                    return result;
                }
            let Some(type_node) = property_signature.type_() else {
                return self.grammar_error_on_node(
                    member,
                    &diagnostics::AN_IMPORT_ATTRIBUTES_PROPERTY_MUST_HAVE_A_TYPE_ANNOTATION,
                    &[],
                );
            };
            if member.question_token().is_some() {
                return self.grammar_error_on_node(
                    member,
                    &diagnostics::AN_IMPORT_ATTRIBUTES_PROPERTY_CANNOT_BE_OPTIONAL,
                    &[],
                );
            }
            let name = property_signature.name();
            if !(ast::is_string_literal_like(name) || is_identifier(name)) {
                return self.grammar_error_on_node(name, &diagnostics::AN_IMPORT_ATTRIBUTES_PROPERTY_MUST_HAVE_A_STRING_LITERAL_OR_IDENTIFIER_NAME, &[]);
            }
            if name.text() == "resolution-mode" {
                return self.grammar_error_on_node(
                    name,
                    &diagnostics::X_0_IS_NOT_A_VALID_KEY_FOR_AN_IMPORT_ATTRIBUTES_TYPE,
                    &[&name.text()],
                );
            }

            if !ast::is_literal_type_node(type_node)
                || !ast::is_string_literal(type_node.as_literal_type_node().literal)
            {
                return self.grammar_error_on_node(type_node, &diagnostics::AN_IMPORT_ATTRIBUTES_PROPERTY_MUST_HAVE_A_STRING_LITERAL_TYPE_ANNOTATION, &[]);
            }
        }
        false
    }
}
