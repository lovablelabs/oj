// Port of ls/inlay_hints.go.

use std::cell::RefCell;

use rustc_hash::FxHashMap;
use tsrs_ast::{self as ast, Kind, Node, SourceFile, Symbol, SymbolFlags};
use tsrs_checker::{
    self as checker, Checker, ElementFlags, Flags, Signature, Type, TypeFlags, TypePredicate,
};
use tsrs_core::context::Context;
use tsrs_core::stringutil;
use tsrs_core::{P, TextRange};
use tsrs_lsproto as lsproto;
use tsrs_printer as printer;
use tsrs_scanner as scanner;

use crate::astnav;
use crate::languageservice::LanguageService;
use crate::lsconv::{self, Converters};
use crate::lsutil::{self, IncludeInlayParameterNameHints, InlayHintsPreferences, QuotePreference};
use crate::spanmap::Feature;
use crate::utilities::get_leading_comment_ranges_of_node;

impl LanguageService {
    // inlay_hints.go:25
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Language service operations share the fallible response signature used by LSP and cross-project dispatch."
    )]
    pub fn provide_inlay_hint(
        &self,
        ctx: &Context,
        params: &lsproto::InlayHintParams,
    ) -> Result<lsproto::InlayHintResponse, lsproto::Error> {
        let user_preferences = self.user_preferences();
        let inlay_hint_preferences = &user_preferences.inlay_hints;
        if !is_any_inlay_hint_enabled(inlay_hint_preferences) {
            return Ok(lsproto::InlayHintsOrNull { inlay_hints: None });
        }

        let (program, file) = self.get_program_and_file(&params.text_document.uri);
        let quote_preference = lsutil::get_quote_preference(file, user_preferences);

        let mapped_ranges = self.converters.from_lsp_range_intersecting_for_source_file(
            file,
            params.range,
            Feature::InlayHints,
        );
        let mut result = Vec::with_capacity(mapped_ranges.len());
        result.extend(mapped_ranges.iter().flat_map(|mapped| {
            let projection = mapped.script;
            let mut checker = program.get_type_checker_for_file(ctx, projection);
            let mut inlay_hint_state = InlayHintState {
                ctx,
                span: mapped.span,
                preferences: inlay_hint_preferences,
                quote_preference,
                file: projection,
                checker: &mut checker,
                converters: &self.converters,
                result: Vec::new(),
            };
            inlay_hint_state.visit(projection.as_node());
            inlay_hint_state.result
        }));
        Ok(lsproto::InlayHintsOrNull { inlay_hints: Some(result) })
    }
}

// inlay_hints.go:61
struct InlayHintState<'a> {
    ctx: &'a Context,
    span: TextRange,
    preferences: &'a InlayHintsPreferences,
    quote_preference: QuotePreference,
    file: P<SourceFile>,
    checker: &'a mut Checker,
    converters: &'a Converters,
    result: Vec<lsproto::InlayHint>,
}

impl InlayHintState<'_> {
    // inlay_hints.go:72
    fn visit(&mut self, node: P<Node>) -> bool {
        if node.end() == node.pos() || node.flags().intersects(ast::NodeFlags::Reparsed) {
            return false;
        }

        match node.kind() {
            Kind::ModuleDeclaration
            | Kind::ClassDeclaration
            | Kind::InterfaceDeclaration
            | Kind::FunctionDeclaration
            | Kind::ClassExpression
            | Kind::FunctionExpression
            | Kind::MethodDeclaration
            | Kind::ArrowFunction
                if self.ctx.err().is_some() =>
            {
                return true;
            }
            _ => {}
        }

        if !self.span.intersects(node.loc()) {
            return false;
        }

        if ast::is_type_node(node) && !ast::is_expression_with_type_arguments(node) {
            return false;
        }

        if (self.preferences.include_inlay_variable_type_hints.is_true()
            && ast::is_variable_declaration(node))
            || (self.preferences.include_inlay_property_declaration_type_hints.is_true()
                && ast::is_property_declaration(node))
        {
            self.visit_variable_like_declaration(node);
        } else if self.preferences.include_inlay_enum_member_value_hints.is_true()
            && ast::is_enum_member(node)
        {
            self.visit_enum_member(node);
        } else if should_show_parameter_name_hints(self.preferences)
            && (ast::is_call_expression(node) || ast::is_new_expression(node))
        {
            self.visit_call_or_new_expression(node);
        } else {
            if self.preferences.include_inlay_function_parameter_type_hints.is_true()
                && ast::is_function_like_declaration(node)
                && ast::has_context_sensitive_parameters(node)
            {
                self.visit_function_like_for_parameter_type(node);
            }
            if self.preferences.include_inlay_function_like_return_type_hints.is_true()
                && is_signature_supporting_return_annotation(node)
            {
                self.visit_function_declaration_like_for_return_type(node);
            }
        }
        node.for_each_child(&mut |child| self.visit(child))
    }

    // FunctionDeclaration | MethodDeclaration | GetAccessor | FunctionExpression | ArrowFunction
    // inlay_hints.go:117
    fn visit_function_declaration_like_for_return_type(&mut self, decl: P<Node>) {
        if ast::is_arrow_function(decl)
            && astnav::find_child_of_kind(decl, Kind::OpenParenToken, self.file).is_none()
        {
            return;
        }

        let type_annotation = decl.type_node();
        if type_annotation.is_some() || decl.body().is_none() {
            return;
        }

        let signature = self.checker.get_signature_from_declaration_exported(decl);

        let type_predicate = self.checker.get_type_predicate_of_signature(signature);

        if let Some(type_predicate) = type_predicate.filter(|p| p.type_().is_some()) {
            let hint_parts = self.type_predicate_to_inlay_hint_parts(type_predicate);
            let position = self.get_type_annotation_position(decl);
            self.add_type_hints(hint_parts, position);
            return;
        }

        let return_type = self.checker.get_return_type_of_signature(signature);
        if is_module_reference_type(return_type) {
            return;
        }

        let hint_parts = self.type_to_inlay_hint_parts(return_type);
        let position = self.get_type_annotation_position(decl);
        self.add_type_hints(hint_parts, position);
    }

    // inlay_hints.go:151
    fn visit_call_or_new_expression(&mut self, expr: P<Node>) {
        let args = expr.arguments();
        if args.is_empty() {
            return;
        }

        let signature = self.checker.get_resolved_signature_exported(expr);

        let mut signature_param_pos: usize = 0;
        for &original_arg in args {
            let arg = ast::skip_parentheses(original_arg);
            if should_show_literal_parameter_name_hints_only(self.preferences)
                && !is_hintable_literal(arg)
            {
                signature_param_pos += 1;
                continue;
            }

            let mut spread_args = 0;
            if ast::is_spread_element(arg) {
                let spread_type = self.checker.get_type_at_location(arg.expression().unwrap());
                if spread_type.is_tuple_type() {
                    let tuple_type = spread_type.target().unwrap().as_tuple_type();
                    let element_flags = tuple_type.element_flags();
                    let fixed_length = tuple_type.fixed_length();
                    if fixed_length == 0 {
                        continue;
                    }
                    let required_args = element_flags
                        .iter()
                        .position(|f| !f.intersects(ElementFlags::Required))
                        .unwrap_or(fixed_length);
                    if required_args > 0 {
                        spread_args = required_args;
                    }
                }
            }

            let identifier_info =
                self.get_parameter_identifier_info_at_position(signature, signature_param_pos);
            signature_param_pos += if spread_args > 0 { spread_args } else { 1 };
            let Some(identifier_info) = identifier_info else {
                return;
            };

            let parameter = identifier_info.parameter;
            let parameter_name = identifier_info.name;
            let is_first_variadic_argument = identifier_info.is_rest_parameter;
            let parameter_name_not_same_as_argument = self
                .preferences
                .include_inlay_parameter_name_hints_when_argument_matches_name
                .is_true()
                || !identifier_or_access_expression_postfix_matches_parameter_name(
                    arg,
                    &parameter_name,
                );
            if !parameter_name_not_same_as_argument && !is_first_variadic_argument {
                continue;
            }

            if self.leading_comments_contains_parameter_name(arg, &parameter_name) {
                continue;
            }

            self.add_parameter_hints(
                &parameter_name,
                parameter,
                astnav::get_start_of_node(original_arg, self.file, false /*includeJSDoc*/),
                is_first_variadic_argument,
            );
        }
    }

    // inlay_hints.go:217
    fn visit_enum_member(&mut self, member: P<Node>) {
        if member.initializer().is_some() {
            return;
        }

        if let Some(enum_value) = self.checker.get_constant_value(member) {
            self.add_enum_member_value_hints(
                &checker::evaluator::any_to_string(enum_value),
                member.end().expect_source("enum members have source positions"),
            );
        }
    }

    // inlay_hints.go:228
    fn visit_variable_like_declaration(&mut self, decl: P<Node>) {
        let missing_initializer_without_specific_property_type = decl.initializer().is_none()
            && (!ast::is_property_declaration(decl)
                || self.checker.get_type_at_location(decl).flags().intersects(TypeFlags::Any));
        if missing_initializer_without_specific_property_type
            || ast::is_binding_pattern(decl.name().unwrap())
            || (ast::is_variable_declaration(decl) && !is_hintable_declaration(decl))
        {
            return;
        }

        let type_annotation = decl.type_node();
        if type_annotation.is_some() {
            return;
        }

        let declaration_type = self.checker.get_type_at_location(decl);
        if is_module_reference_type(declaration_type) {
            return;
        }

        let hint_parts = self.type_to_inlay_hint_parts(declaration_type);
        let mut hint_text = String::new();
        if let Some(s) = &hint_parts.string {
            hint_text.clone_from(s);
        } else if let Some(label_parts) = &hint_parts.inlay_hint_label_parts {
            hint_text.extend(label_parts.iter().map(|part| part.value.as_str()));
        }
        let name = decl.name().unwrap();
        if !self.preferences.include_inlay_variable_type_hints_when_type_matches_name.is_true()
            && !ast::is_computed_property_name(name)
            && stringutil::equate_string_case_insensitive(name.text(), &hint_text)
        {
            return;
        }
        self.add_type_hints(
            hint_parts,
            name.end().expect_source("declaration names have source positions"),
        );
    }

    // inlay_hints.go:264
    fn visit_function_like_for_parameter_type(&mut self, node: P<Node>) {
        let signature = self.checker.get_signature_from_declaration_exported(node);

        let mut pos = 0;
        for &param in node.parameters() {
            if is_hintable_declaration(param) {
                let symbol = if ast::is_this_parameter(param) {
                    signature.this_parameter()
                } else {
                    Some(signature.parameters()[pos])
                };
                self.add_parameter_type_hint(param, symbol);
            }
            if ast::is_this_parameter(param) {
                continue;
            }
            pos += 1;
        }
    }

    // inlay_hints.go:288
    fn add_parameter_type_hint(&mut self, node: P<Node>, symbol: Option<P<Symbol>>) {
        let type_annotation = node.type_node();
        let Some(symbol) = symbol.filter(|_| type_annotation.is_none()) else {
            return;
        };
        let Some(type_hints) = self.get_parameter_declaration_type_hints(symbol) else {
            return;
        };
        let pos = match node.question_token() {
            Some(question_token) => {
                question_token.end().expect_source("parameter tokens have source positions")
            }
            None => {
                node.name().unwrap().end().expect_source("parameter names have source positions")
            }
        };
        self.add_type_hints(type_hints, pos);
    }

    // inlay_hints.go:306
    fn get_parameter_declaration_type_hints(
        &mut self,
        symbol: P<Symbol>,
    ) -> Option<lsproto::StringOrInlayHintLabelParts> {
        let value_declaration =
            symbol.value_declaration().filter(|d| ast::is_parameter_declaration(*d))?;

        let signature_param_type =
            self.checker.get_type_of_symbol_at_location(symbol, Some(value_declaration)).unwrap();
        if is_module_reference_type(signature_param_type) {
            return None;
        }

        Some(self.type_to_inlay_hint_parts(signature_param_type))
    }

    // inlay_hints.go:320
    fn type_to_inlay_hint_parts(&mut self, t: P<Type>) -> lsproto::StringOrInlayHintLabelParts {
        let flags = Flags::IgnoreErrors
            | Flags::AllowUniqueESSymbolType
            | Flags::UseAliasDefinedOutsideCurrentScope;
        let id_to_symbol: P<RefCell<FxHashMap<P<Node>, P<Symbol>>>> =
            P::new(RefCell::new(FxHashMap::default()));
        // !!! Avoid type node reuse so we collect identifier symbols.
        let type_node = self.checker.type_to_type_node(
            t,
            None, /*enclosingDeclaration*/
            flags,
            Some(id_to_symbol),
        );
        let type_node = type_node.expect("should always get typenode");
        let id_to_symbol = id_to_symbol.borrow();
        lsproto::StringOrInlayHintLabelParts {
            inlay_hint_label_parts: Some(self.get_inlay_hint_label_parts(type_node, &id_to_symbol)),
            ..Default::default()
        }
    }

    // inlay_hints.go:332
    fn type_predicate_to_inlay_hint_parts(
        &mut self,
        type_predicate: P<TypePredicate>,
    ) -> lsproto::StringOrInlayHintLabelParts {
        let flags = Flags::IgnoreErrors
            | Flags::AllowUniqueESSymbolType
            | Flags::UseAliasDefinedOutsideCurrentScope;
        let id_to_symbol: P<RefCell<FxHashMap<P<Node>, P<Symbol>>>> =
            P::new(RefCell::new(FxHashMap::default()));
        // !!! Avoid type node reuse so we collect identifier symbols.
        let type_node = self.checker.type_predicate_to_type_predicate_node(
            type_predicate,
            None, /*enclosingDeclaration*/
            flags,
            Some(id_to_symbol),
        );
        let type_node = type_node.expect("should always get typePredicateNode");
        let id_to_symbol = id_to_symbol.borrow();
        lsproto::StringOrInlayHintLabelParts {
            inlay_hint_label_parts: Some(self.get_inlay_hint_label_parts(type_node, &id_to_symbol)),
            ..Default::default()
        }
    }

    // inlay_hints.go:344
    fn add_type_hints(&mut self, mut hint: lsproto::StringOrInlayHintLabelParts, position: u32) {
        let (lsp_position, fidelity) = self.converters.to_lsp_position_for_feature(
            &self.file,
            position.into(),
            Feature::InlayHints,
        );
        if fidelity.is_none() {
            return;
        }
        if let Some(s) = &hint.string {
            hint.string = Some(format!(": {s}"));
        } else {
            let mut parts =
                vec![lsproto::InlayHintLabelPart { value: ": ".to_string(), ..Default::default() }];
            parts.extend(hint.inlay_hint_label_parts.take().unwrap());
            hint.inlay_hint_label_parts = Some(parts);
        }
        self.result.push(lsproto::InlayHint {
            label: hint,
            position: lsp_position,
            kind: Some(lsproto::InlayHintKind::TYPE),
            padding_left: Some(true),
            ..Default::default()
        });
    }

    // inlay_hints.go:362
    fn add_enum_member_value_hints(&mut self, text: &str, position: u32) {
        let (lsp_position, fidelity) = self.converters.to_lsp_position_for_feature(
            &self.file,
            position.into(),
            Feature::InlayHints,
        );
        if fidelity.is_none() {
            return;
        }
        self.result.push(lsproto::InlayHint {
            label: lsproto::StringOrInlayHintLabelParts {
                string: Some(format!("= {text}")),
                ..Default::default()
            },
            position: lsp_position,
            padding_left: Some(true),
            ..Default::default()
        });
    }

    // inlay_hints.go:376
    fn add_parameter_hints(
        &mut self,
        text: &str,
        parameter: P<Node>,
        position: u32,
        is_first_variadic_argument: bool,
    ) {
        let (lsp_position, fidelity) = self.converters.to_lsp_position_for_feature(
            &self.file,
            position.into(),
            Feature::InlayHints,
        );
        if fidelity.is_none() {
            return;
        }
        let hint_text = format!("{}{}", if is_first_variadic_argument { "..." } else { "" }, text);
        let display_parts = vec![
            self.get_node_display_part(&hint_text, parameter),
            lsproto::InlayHintLabelPart { value: ":".to_string(), ..Default::default() },
        ];
        let label_parts = lsproto::StringOrInlayHintLabelParts {
            inlay_hint_label_parts: Some(display_parts),
            ..Default::default()
        };

        self.result.push(lsproto::InlayHint {
            label: label_parts,
            position: lsp_position,
            kind: Some(lsproto::InlayHintKind::PARAMETER),
            padding_right: Some(true),
            ..Default::default()
        });
    }
}

// inlay_hints.go:398
fn should_show_parameter_name_hints(preferences: &InlayHintsPreferences) -> bool {
    preferences.include_inlay_parameter_name_hints == IncludeInlayParameterNameHints::Literals
        || preferences.include_inlay_parameter_name_hints == IncludeInlayParameterNameHints::All
}

// inlay_hints.go:403
fn should_show_literal_parameter_name_hints_only(preferences: &InlayHintsPreferences) -> bool {
    preferences.include_inlay_parameter_name_hints == IncludeInlayParameterNameHints::Literals
}

// node is FunctionDeclaration | ArrowFunction | FunctionExpression | MethodDeclaration | GetAccessor
// inlay_hints.go:408
fn is_signature_supporting_return_annotation(node: P<Node>) -> bool {
    ast::is_arrow_function(node)
        || ast::is_function_expression(node)
        || ast::is_function_declaration(node)
        || ast::is_method_declaration(node)
        || ast::is_get_accessor_declaration(node)
}

// inlay_hints.go:413
fn is_hintable_declaration(node: P<Node>) -> bool {
    if (ast::is_part_of_parameter_declaration(node)
        || ast::is_variable_declaration(node) && ast::is_var_const(node))
        && node.initializer().is_some()
    {
        let initializer = ast::skip_parentheses(node.initializer().unwrap());
        return !(is_hintable_literal(initializer)
            || ast::is_new_expression(initializer)
            || ast::is_object_literal_expression(initializer)
            || ast::is_assertion_expression(initializer));
    }
    true
}

// inlay_hints.go:423
fn is_hintable_literal(node: P<Node>) -> bool {
    match node.kind() {
        Kind::PrefixUnaryExpression => {
            let operand = node.as_prefix_unary_expression().operand;
            return ast::is_literal_expression(operand)
                || ast::is_identifier(operand) && ast::is_infinity_or_nan_string(operand.text());
        }
        Kind::TrueKeyword
        | Kind::FalseKeyword
        | Kind::NullKeyword
        | Kind::NoSubstitutionTemplateLiteral
        | Kind::TemplateExpression => {
            return true;
        }
        Kind::Identifier => {
            let name = node.text();
            return name == "undefined" || ast::is_infinity_or_nan_string(name);
        }
        _ => {}
    }
    ast::is_literal_expression(node)
}

// inlay_hints.go:438
fn is_module_reference_type(t: P<Type>) -> bool {
    t.symbol().is_some_and(|symbol| symbol.flags().intersects(SymbolFlags::Module))
}

// The closures of getInlayHintLabelParts share `parts`.
struct LabelPartsBuilder<'a, 'b> {
    state: &'a InlayHintState<'b>,
    id_to_symbol: &'a FxHashMap<P<Node>, P<Symbol>>,
    parts: Vec<lsproto::InlayHintLabelPart>,
}

fn part(value: impl Into<String>) -> lsproto::InlayHintLabelPart {
    lsproto::InlayHintLabelPart { value: value.into(), ..Default::default() }
}

impl LabelPartsBuilder<'_, '_> {
    fn visit_for_display_parts(&mut self, node: Option<P<Node>>) {
        let Some(node) = node else {
            return;
        };

        let token_string = scanner::token_to_string(node.kind());
        if !token_string.is_empty() {
            self.parts.push(part(token_string));
            return;
        }

        if ast::is_literal_expression(node) {
            self.parts.push(part(self.state.get_literal_text(node)));
            return;
        }

        match node.kind() {
            Kind::Identifier => {
                let identifier_text = node.text();
                let mut name: Option<P<Node>> = None;
                if let Some(symbol) = self.id_to_symbol.get(&node)
                    && !symbol.declarations().is_empty()
                {
                    name = ast::get_name_of_declaration(symbol.declarations()[0]);
                }
                if let Some(name) = name {
                    self.parts.push(self.state.get_node_display_part(identifier_text, name));
                } else {
                    self.parts.push(part(identifier_text));
                }
            }
            Kind::QualifiedName => {
                self.visit_for_display_parts(Some(node.as_qualified_name().left));
                self.parts.push(part("."));
                self.visit_for_display_parts(Some(node.as_qualified_name().right));
            }
            Kind::TypePredicate => {
                if node.as_type_predicate_node().asserts_modifier.is_some() {
                    self.parts.push(part("asserts "));
                }
                self.visit_for_display_parts(Some(node.as_type_predicate_node().parameter_name));
                if node.type_node().is_some() {
                    self.parts.push(part(" is "));
                    self.visit_for_display_parts(node.type_node());
                }
            }
            Kind::TypeReference => {
                self.visit_for_display_parts(Some(node.as_type_reference_node().type_name));
                if !node.type_arguments().is_empty() {
                    self.parts.push(part("<"));
                    self.visit_display_part_list(node.type_arguments(), ",");
                    self.parts.push(part(">"));
                }
            }
            Kind::TypeParameter => {
                if !node.modifier_nodes().is_empty() {
                    self.visit_display_part_list(node.modifier_nodes(), "");
                }
                self.visit_for_display_parts(node.name());
                if let Some(constraint) = node.as_type_parameter_declaration().constraint {
                    self.parts.push(part(" extends "));
                    self.visit_for_display_parts(Some(constraint));
                }
                if let Some(default_type) = node.as_type_parameter_declaration().default_type {
                    self.parts.push(part(" = "));
                    self.visit_for_display_parts(Some(default_type));
                }
            }
            Kind::Parameter => {
                if !node.modifier_nodes().is_empty() {
                    self.visit_display_part_list(node.modifier_nodes(), " ");
                }
                if node.as_parameter_declaration().dot_dot_dot_token().is_some() {
                    self.parts.push(part("..."));
                }
                self.visit_for_display_parts(node.name());
                if node.question_token().is_some() {
                    self.parts.push(part("?"));
                }
                if node.type_node().is_some() {
                    self.parts.push(part(": "));
                    self.visit_for_display_parts(node.type_node());
                }
            }
            Kind::ConstructorType => {
                self.parts.push(part("new "));
                self.visit_parameters_and_type_parameters(node);
                self.parts.push(part(" => "));
                self.visit_for_display_parts(node.type_node());
            }
            Kind::TypeQuery => {
                self.parts.push(part("typeof "));
                self.visit_for_display_parts(Some(node.as_type_query_node().expr_name));
                if !node.type_arguments().is_empty() {
                    self.parts.push(part("<"));
                    self.visit_display_part_list(node.type_arguments(), ", ");
                    self.parts.push(part(">"));
                }
            }
            Kind::TypeLiteral => {
                self.parts.push(part("{"));
                if !node.members().is_empty() {
                    self.parts.push(part(" "));
                    self.visit_display_part_list(node.members(), "; ");
                    self.parts.push(part(" "));
                }
                self.parts.push(part("}"));
            }
            Kind::ArrayType => {
                self.visit_for_display_parts(Some(node.as_array_type_node().element_type));
                self.parts.push(part("[]"));
            }
            Kind::TupleType | Kind::ArrayBindingPattern => {
                self.parts.push(part("["));
                self.visit_display_part_list(node.elements(), ", ");
                self.parts.push(part("]"));
            }
            Kind::NamedTupleMember => {
                if node.as_named_tuple_member().dot_dot_dot_token.is_some() {
                    self.parts.push(part("..."));
                }
                self.visit_for_display_parts(node.name());
                if node.question_token().is_some() {
                    self.parts.push(part("?"));
                }
                self.parts.push(part(": "));
                self.visit_for_display_parts(node.type_node());
            }
            Kind::OptionalType => {
                self.visit_for_display_parts(node.type_node());
                self.parts.push(part("?"));
            }
            Kind::RestType => {
                self.parts.push(part("..."));
                self.visit_for_display_parts(node.type_node());
            }
            Kind::UnionType => {
                self.visit_display_part_list(node.as_union_type_node().types().nodes(), " | ");
            }
            Kind::IntersectionType => {
                self.visit_display_part_list(
                    node.as_intersection_type_node().types().nodes(),
                    " & ",
                );
            }
            Kind::ConditionalType => {
                let conditional = node.as_conditional_type_node();
                self.visit_for_display_parts(Some(conditional.check_type));
                self.parts.push(part(" extends "));
                self.visit_for_display_parts(Some(conditional.extends_type));
                self.parts.push(part(" ? "));
                self.visit_for_display_parts(Some(conditional.true_type));
                self.parts.push(part(" : "));
                self.visit_for_display_parts(Some(conditional.false_type));
            }
            Kind::InferType => {
                self.parts.push(part("infer "));
                self.visit_for_display_parts(Some(node.as_infer_type_node().type_parameter));
            }
            Kind::ParenthesizedType => {
                self.parts.push(part("("));
                self.visit_for_display_parts(node.type_node());
                self.parts.push(part(")"));
            }
            Kind::TypeOperator => {
                self.parts
                    .push(part(scanner::token_to_string(node.as_type_operator_node().operator)));
                self.visit_for_display_parts(node.type_node());
            }
            Kind::IndexedAccessType => {
                self.visit_for_display_parts(Some(node.as_indexed_access_type_node().object_type));
                self.parts.push(part("["));
                self.visit_for_display_parts(Some(node.as_indexed_access_type_node().index_type));
                self.parts.push(part("]"));
            }
            Kind::MappedType => {
                let mapped = node.as_mapped_type_node();
                self.parts.push(part("{ "));
                if let Some(readonly_token) = mapped.readonly_token {
                    if readonly_token.kind() == Kind::PlusToken {
                        self.parts.push(part("+"));
                    } else if readonly_token.kind() == Kind::MinusToken {
                        self.parts.push(part("-"));
                    }
                    self.parts.push(part("readonly "));
                }
                self.parts.push(part("["));
                self.visit_for_display_parts(Some(mapped.type_parameter));
                if let Some(name_type) = mapped.name_type {
                    self.parts.push(part(" as "));
                    self.visit_for_display_parts(Some(name_type));
                }
                self.parts.push(part("]"));
                if let Some(question_token) = node.question_token() {
                    if question_token.kind() == Kind::PlusToken {
                        self.parts.push(part("+"));
                    } else if question_token.kind() == Kind::MinusToken {
                        self.parts.push(part("-"));
                    }
                    self.parts.push(part("?"));
                }
                self.parts.push(part(": "));
                if node.type_node().is_some() {
                    self.visit_for_display_parts(node.type_node());
                }
                self.parts.push(part("; }"));
            }
            Kind::LiteralType => {
                self.visit_for_display_parts(Some(node.as_literal_type_node().literal));
            }
            Kind::FunctionType => {
                self.visit_parameters_and_type_parameters(node);
                self.parts.push(part(" => "));
                self.visit_for_display_parts(node.type_node());
            }
            Kind::ImportType => {
                let import_type = node.as_import_type_node();
                if import_type.is_type_of {
                    self.parts.push(part("typeof "));
                }
                self.parts.push(part("import("));
                self.visit_for_display_parts(Some(import_type.argument));
                self.parts.push(part(")"));
                if let Some(qualifier) = import_type.qualifier {
                    self.parts.push(part("."));
                    self.visit_for_display_parts(Some(qualifier));
                }
                if !node.type_arguments().is_empty() {
                    self.parts.push(part("<"));
                    self.visit_display_part_list(node.type_arguments(), ", ");
                    self.parts.push(part(">"));
                }
            }
            Kind::PropertySignature => {
                if !node.modifier_nodes().is_empty() {
                    self.visit_display_part_list(node.modifier_nodes(), " ");
                    self.parts.push(part(" "));
                }
                self.visit_for_display_parts(node.name());
                if let Some(postfix_token) = node.postfix_token() {
                    self.parts.push(part(scanner::token_to_string(postfix_token.kind())));
                }
                if node.type_node().is_some() {
                    self.parts.push(part(": "));
                    self.visit_for_display_parts(node.type_node());
                }
            }
            Kind::IndexSignature => {
                self.parts.push(part("["));
                self.visit_display_part_list(node.parameters(), ", ");
                self.parts.push(part("]"));
                if node.type_node().is_some() {
                    self.parts.push(part(": "));
                    self.visit_for_display_parts(node.type_node());
                }
            }
            Kind::MethodSignature => {
                if !node.modifier_nodes().is_empty() {
                    self.visit_display_part_list(node.modifier_nodes(), " ");
                    self.parts.push(part(" "));
                }
                self.visit_for_display_parts(node.name());
                if let Some(postfix_token) = node.postfix_token() {
                    self.parts.push(part(scanner::token_to_string(postfix_token.kind())));
                }
                self.visit_parameters_and_type_parameters(node);
                if node.type_node().is_some() {
                    self.parts.push(part(": "));
                    self.visit_for_display_parts(node.type_node());
                }
            }
            Kind::CallSignature => {
                self.visit_parameters_and_type_parameters(node);
                if node.type_node().is_some() {
                    self.parts.push(part(": "));
                    self.visit_for_display_parts(node.type_node());
                }
            }
            Kind::ConstructSignature => {
                self.parts.push(part("new "));
                self.visit_parameters_and_type_parameters(node);
                if node.type_node().is_some() {
                    self.parts.push(part(": "));
                    self.visit_for_display_parts(node.type_node());
                }
            }
            Kind::ObjectBindingPattern => {
                self.parts.push(part("{"));
                if !node.elements().is_empty() {
                    self.parts.push(part(" "));
                    self.visit_display_part_list(node.elements(), ", ");
                    self.parts.push(part(" "));
                }
                self.parts.push(part("}"));
            }
            Kind::BindingElement => {
                self.visit_for_display_parts(node.name());
            }
            Kind::PrefixUnaryExpression => {
                self.parts.push(part(scanner::token_to_string(
                    node.as_prefix_unary_expression().operator,
                )));
                self.visit_for_display_parts(Some(node.as_prefix_unary_expression().operand));
            }
            Kind::TemplateLiteralType => {
                let template = node.as_template_literal_type_node();
                self.visit_for_display_parts(Some(template.head));
                for &span in template.template_spans.nodes() {
                    self.visit_for_display_parts(Some(span));
                }
            }
            Kind::TemplateHead | Kind::TemplateMiddle | Kind::TemplateTail => {
                self.parts.push(part(self.state.get_literal_text(node)));
            }
            Kind::TemplateLiteralTypeSpan => {
                self.visit_for_display_parts(node.type_node());
                self.visit_for_display_parts(Some(node.as_template_literal_type_span().literal));
            }
            Kind::ThisType => {
                self.parts.push(part("this"));
            }
            Kind::ComputedPropertyName => {
                self.parts.push(part("["));
                self.visit_for_display_parts(node.expression());
                self.parts.push(part("]"));
            }
            Kind::PropertyAccessExpression => {
                self.visit_for_display_parts(node.expression());
                self.parts.push(part("."));
                self.visit_for_display_parts(node.name());
            }
            Kind::ElementAccessExpression => {
                self.visit_for_display_parts(node.expression());
                self.parts.push(part("["));
                self.visit_for_display_parts(Some(
                    node.as_element_access_expression().argument_expression,
                ));
                self.parts.push(part("]"));
            }
            k => panic!("Debug Failure. Unexpected node: {k:?}"),
        }
    }

    fn visit_display_part_list(&mut self, nodes: &[P<Node>], separator: &str) {
        for (i, &n) in nodes.iter().enumerate() {
            if i > 0 {
                self.parts.push(part(separator));
            }
            self.visit_for_display_parts(Some(n));
        }
    }

    fn visit_parameters_and_type_parameters(&mut self, node: P<Node>) {
        if !node.type_parameters().is_empty() {
            self.parts.push(part("<"));
            self.visit_display_part_list(node.type_parameters(), ", ");
            self.parts.push(part(">"));
        }
        self.parts.push(part("("));
        self.visit_display_part_list(node.parameters(), ", ");
        self.parts.push(part(")"));
    }
}

impl InlayHintState<'_> {
    // inlay_hints.go:443
    fn get_inlay_hint_label_parts(
        &self,
        node: P<Node>,
        id_to_symbol: &FxHashMap<P<Node>, P<Symbol>>,
    ) -> Vec<lsproto::InlayHintLabelPart> {
        let mut b = LabelPartsBuilder { state: self, id_to_symbol, parts: Vec::new() };
        b.visit_for_display_parts(Some(node));
        b.parts
    }

    // inlay_hints.go:789
    fn get_node_display_part(&self, text: &str, node: P<Node>) -> lsproto::InlayHintLabelPart {
        let file = ast::get_source_file_of_node(node).unwrap();
        let pos = astnav::get_start_of_node(node, file, false /*includeJSDoc*/);
        let end = node.end().expect_source("inlay-hint targets have source positions");
        let mut part = part(text);
        // The location is an optional go-to target for the name. Only attach it when the name maps back to a
        // single concrete span in the original text; an approximate or synthesized mapping would point the
        // user somewhere wrong, so it is better to omit the target than to fabricate one.
        let (lsp_range, fidelity) = self.converters.to_lsp_range_for_feature(
            &file,
            TextRange::new(pos, end),
            Feature::InlayHints,
        );
        if fidelity.is_single_segment() {
            part.location = Some(lsproto::Location {
                uri: lsconv::file_name_to_document_uri(file.original_file_name()),
                range: lsp_range,
            });
        }
        part
    }

    // inlay_hints.go:806
    fn get_literal_text(&self, node: P<Node>) -> String {
        match node.kind() {
            Kind::StringLiteral => {
                if self.quote_preference == QuotePreference::Single {
                    return format!(
                        "'{}'",
                        printer::escape_string(node.text(), printer::QuoteChar::SingleQuote)
                    );
                }
                return format!(
                    "\"{}\"",
                    printer::escape_string(node.text(), printer::QuoteChar::DoubleQuote)
                );
            }
            Kind::TemplateHead | Kind::TemplateMiddle | Kind::TemplateTail => {
                let mut raw_text = node.raw_text().to_string();
                if raw_text.is_empty() {
                    raw_text = printer::escape_string(node.text(), printer::QuoteChar::Backtick);
                }
                match node.kind() {
                    Kind::TemplateHead => return format!("`{raw_text}${{"),
                    Kind::TemplateMiddle => return format!("}}{raw_text}${{"),
                    Kind::TemplateTail => return format!("}}{raw_text}`"),
                    _ => {}
                }
            }
            _ => {}
        }
        node.text().to_string()
    }
}

// inlay_hints.go:830
struct ParameterInfo {
    parameter: P<Node>,
    name: String,
    is_rest_parameter: bool,
}

impl InlayHintState<'_> {
    // inlay_hints.go:836
    fn get_parameter_identifier_info_at_position(
        &mut self,
        signature: P<Signature>,
        pos: usize,
    ) -> Option<ParameterInfo> {
        let parameters = signature.parameters();
        let param_count = parameters.len() - usize::from(signature.has_rest_parameter());
        if pos < param_count {
            let param = parameters[pos];
            let param_id = get_parameter_declaration_identifier(param)?;
            return Some(ParameterInfo {
                parameter: param_id,
                name: param_id.text().to_string(),
                is_rest_parameter: false,
            });
        }

        let mut rest_parameter: Option<P<Symbol>> = None;
        let mut rest_id: Option<P<Node>> = None;
        if param_count < parameters.len() {
            rest_parameter = Some(parameters[param_count]);
            rest_id = get_parameter_declaration_identifier(parameters[param_count]);
        }
        let rest_id = rest_id?;
        let rest_parameter = rest_parameter.unwrap();

        let rest_type = self.checker.get_type_of_symbol(rest_parameter);
        if rest_type.is_tuple_type() {
            let element_infos = rest_type.target().unwrap().as_tuple_type().element_infos();
            let mut associated_names: Vec<Option<P<Node>>> =
                Vec::with_capacity(element_infos.len());
            associated_names.extend(
                element_infos.iter().map(|element_info| element_info.labeled_declaration()),
            );
            let index = pos - param_count;
            if index < associated_names.len()
                && let Some(associated_name) = associated_names[index]
            {
                let name = associated_name.name().unwrap();
                assert!(ast::is_identifier(name));
                let is_rest_tuple_element = if ast::is_named_tuple_member(associated_name) {
                    associated_name.as_named_tuple_member().dot_dot_dot_token.is_some()
                } else {
                    associated_name.as_parameter_declaration().dot_dot_dot_token().is_some()
                };
                return Some(ParameterInfo {
                    parameter: name,
                    name: name.text().to_string(),
                    is_rest_parameter: is_rest_tuple_element,
                });
            }

            return None;
        }

        if pos == param_count {
            return Some(ParameterInfo {
                parameter: rest_id,
                name: rest_parameter.name().to_string(),
                is_rest_parameter: true,
            });
        }
        None
    }
}

// inlay_hints.go:901
fn get_parameter_declaration_identifier(symbol: P<Symbol>) -> Option<P<Node>> {
    let value_declaration = symbol.value_declaration()?;
    if ast::is_parameter_declaration(value_declaration)
        && ast::is_identifier(value_declaration.name().unwrap())
    {
        return value_declaration.name();
    }
    None
}

// inlay_hints.go:908
fn identifier_or_access_expression_postfix_matches_parameter_name(
    expr: P<Node>,
    parameter_name: &str,
) -> bool {
    if ast::is_identifier(expr) {
        return expr.text() == parameter_name;
    }
    if ast::is_property_access_expression(expr) {
        return expr.name().unwrap().text() == parameter_name;
    }
    false
}

impl InlayHintState<'_> {
    // inlay_hints.go:918
    fn leading_comments_contains_parameter_name(&self, node: P<Node>, name: &str) -> bool {
        if !scanner::is_identifier_text(name, self.file.language_variant.get()) {
            return false;
        }

        // Go ranges over the (possibly nil) sequence; arguments are never JsxText.
        let ranges = get_leading_comment_ranges_of_node(node, self.file).unwrap();
        let file_text = self.file.text();
        if ranges.into_iter().any(|r| {
            let comment_text =
                file_text[r.text_range.pos().expect_source("comment ranges have source positions")
                    as usize
                    ..r.text_range.end().expect_source("comment ranges have source positions")
                        as usize]
                    .trim_matches(|r: char| r.is_whitespace() || r == '/' || r == '*');
            comment_text == name
        }) {
            return true;
        }

        false
    }

    // inlay_hints.go:937
    fn get_type_annotation_position(&self, decl: P<Node>) -> u32 {
        if let Some(close_paren_token) =
            astnav::find_child_of_kind(decl, Kind::CloseParenToken, self.file)
        {
            return close_paren_token.end().expect_source("parameter lists have source positions");
        }
        decl.parameter_list().unwrap().end().expect_source("parameter lists have source positions")
    }
}

// inlay_hints.go:945
fn is_any_inlay_hint_enabled(preferences: &InlayHintsPreferences) -> bool {
    preferences.include_inlay_parameter_name_hints != IncludeInlayParameterNameHints::None
        || preferences.include_inlay_function_parameter_type_hints.is_true()
        || preferences.include_inlay_variable_type_hints.is_true()
        || preferences.include_inlay_property_declaration_type_hints.is_true()
        || preferences.include_inlay_function_like_return_type_hints.is_true()
        || preferences.include_inlay_enum_member_value_hints.is_true()
}
