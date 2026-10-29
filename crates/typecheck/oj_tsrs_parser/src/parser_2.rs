use tsrs_ast::{
    self as ast, DiagnosticExt, Kind, ModifierList, Node, NodeFlags, NodeList, TokenFlags,
};
use tsrs_core::{P, TextRange};
use tsrs_diagnostics::{self as diagnostics, Message};
use tsrs_scanner as scanner;

use crate::parser_1::JsdocScannerInfo;
use crate::utilities::{
    token_is_identifier_or_keyword, token_is_identifier_or_keyword_or_greater_than,
};
use crate::*;

impl Parser {
    pub(crate) fn parse_namespace_import(&mut self) -> P<Node> {
        // NameSpaceImport:
        //  * as ImportedBinding
        let pos = self.node_pos();
        self.parse_expected(Kind::AsteriskToken);
        self.parse_expected(Kind::AsKeyword);
        let name = self.parse_identifier();
        let node = self.factory.new_namespace_import(name);
        self.finish_node(node, pos)
    }

    pub(crate) fn parse_named_imports(&mut self) -> P<Node> {
        let pos = self.node_pos();
        // NamedImports:
        //  { }
        //  { ImportsList }
        //  { ImportsList, }
        let imports = self
            .parse_bracketed_list(
                ParsingContext::ImportOrExportSpecifiers,
                Parser::parse_import_specifier,
                Kind::OpenBraceToken,
                Kind::CloseBraceToken,
            )
            .unwrap();
        let node = self.factory.new_named_imports(imports);
        self.finish_node(node, pos)
    }

    fn parse_import_specifier(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let (is_type_only, property_name, name) =
            self.parse_import_or_export_specifier(Kind::ImportSpecifier);
        let identifier_name;
        if name.kind() == Kind::Identifier {
            identifier_name = name;
        } else {
            let loc = self.skip_range_trivia(name.loc());
            self.parse_error_at_range(loc, &diagnostics::IDENTIFIER_EXPECTED, &[]);
            identifier_name = self.new_identifier("");
            self.finish_node(
                identifier_name,
                name.pos().expect_source("parsed nodes have source positions"),
            );
        }
        let node = self.factory.new_import_specifier(is_type_only, property_name, identifier_name);
        let node = self.finish_node(node, pos);
        self.check_js_syntax(node)
    }

    fn parse_import_or_export_specifier(&mut self, kind: Kind) -> (bool, Option<P<Node>>, P<Node>) {
        // ImportSpecifier:
        //   BindingIdentifier
        //   ModuleExportName as BindingIdentifier
        // ExportSpecifier:
        //   ModuleExportName
        //   ModuleExportName as ModuleExportName
        // let checkIdentifierIsKeyword = isKeyword(token()) && !isIdentifier();
        // let checkIdentifierStart = scanner.getTokenStart();
        // let checkIdentifierEnd = scanner.getTokenEnd();
        let mut is_type_only = false;
        let mut property_name: Option<P<Node>> = None;
        let mut can_parse_as_keyword = true;
        let disallow_keywords = kind == Kind::ImportSpecifier;
        let (mut name, mut name_ok) = self.parse_module_export_name(disallow_keywords);
        if name.kind() == Kind::Identifier && name.text() == "type" {
            // If the first token of an import specifier is 'type', there are a lot of possibilities,
            // especially if we see 'as' afterwards:
            //
            // import { type } from "mod";          - isTypeOnly: false,   name: type
            // import { type as } from "mod";       - isTypeOnly: true,    name: as
            // import { type as as } from "mod";    - isTypeOnly: false,   name: as,    propertyName: type
            // import { type as as as } from "mod"; - isTypeOnly: true,    name: as,    propertyName: as
            if self.token == Kind::AsKeyword {
                // { type as ...? }
                let first_as = self.parse_identifier_name();
                if self.token == Kind::AsKeyword {
                    // { type as as ...? }
                    let second_as = self.parse_identifier_name();
                    if self.can_parse_module_export_name() {
                        // { type as as something }
                        // { type as as "something" }
                        is_type_only = true;
                        property_name = Some(first_as);
                        (name, name_ok) = self.parse_module_export_name(disallow_keywords);
                        can_parse_as_keyword = false;
                    } else {
                        // { type as as }
                        property_name = Some(name);
                        name = second_as;
                        can_parse_as_keyword = false;
                    }
                } else if self.can_parse_module_export_name() {
                    // { type as something }
                    // { type as "something" }
                    property_name = Some(name);
                    can_parse_as_keyword = false;
                    (name, name_ok) = self.parse_module_export_name(disallow_keywords);
                } else {
                    // { type as }
                    is_type_only = true;
                    name = first_as;
                }
            } else if self.can_parse_module_export_name() {
                // { type something ...? }
                // { type "something" ...? }
                is_type_only = true;
                (name, name_ok) = self.parse_module_export_name(disallow_keywords);
            }
        }
        if can_parse_as_keyword && self.token == Kind::AsKeyword {
            property_name = Some(name);
            self.parse_expected(Kind::AsKeyword);
            (name, name_ok) = self.parse_module_export_name(disallow_keywords);
        }

        if !name_ok {
            let loc = self.skip_range_trivia(name.loc());
            self.parse_error_at_range(loc, &diagnostics::IDENTIFIER_EXPECTED, &[]);
        }

        (is_type_only, property_name, name)
    }

    fn can_parse_module_export_name(&self) -> bool {
        token_is_identifier_or_keyword(self.token) || self.token == Kind::StringLiteral
    }

    fn parse_module_export_name(&mut self, disallow_keywords: bool) -> (P<Node>, bool) {
        let mut name_ok = true;

        if self.token == Kind::StringLiteral {
            return (self.parse_literal_expression(), name_ok);
        }
        if disallow_keywords && ast::is_keyword(self.token) && !self.is_identifier() {
            name_ok = false;
        }
        (self.parse_identifier_name(), name_ok)
    }

    pub(crate) fn try_parse_import_attributes(&mut self) -> Option<P<Node>> {
        if self.token == Kind::WithKeyword
            || (self.token == Kind::AssertKeyword && !self.has_preceding_line_break())
        {
            if self.token == Kind::AssertKeyword {
                self.parse_error_at_current_token(
                    &diagnostics::IMPORT_ASSERTIONS_HAVE_BEEN_REPLACED_BY_IMPORT_ATTRIBUTES_USE_WITH_INSTEAD_OF_ASSERT,
                    &[],
                );
            }
            return Some(self.parse_import_attributes(self.token, false /*skipKeyword*/));
        }
        None
    }

    pub(crate) fn parse_export_assignment(
        &mut self,
        pos: u32,
        jsdoc: JsdocScannerInfo,
        modifiers: Option<P<ModifierList>>,
    ) -> P<Node> {
        let save_context_flags = self.context_flags;
        let save_has_await_identifier = self.statement_has_await_identifier;
        self.set_context_flags(NodeFlags::AwaitContext, true);
        let mut is_export_equals = false;
        if self.parse_optional(Kind::EqualsToken) {
            is_export_equals = true;
        } else {
            self.parse_expected(Kind::DefaultKeyword);
        }
        let expression = self.parse_assignment_expression_or_higher();
        self.parse_semicolon();
        self.context_flags = save_context_flags;
        self.statement_has_await_identifier = save_has_await_identifier;
        let node = self.factory.new_export_assignment(
            modifiers,
            is_export_equals,
            None, /*typeNode*/
            expression,
        );
        let result = self.finish_node(node, pos);
        self.with_jsdoc(result, jsdoc);
        self.check_js_syntax(result);
        result
    }

    pub(crate) fn parse_namespace_export_declaration(
        &mut self,
        pos: u32,
        jsdoc: JsdocScannerInfo,
        modifiers: Option<P<ModifierList>>,
    ) -> P<Node> {
        self.parse_expected(Kind::AsKeyword);
        self.parse_expected(Kind::NamespaceKeyword);
        let save_has_await_identifier = self.statement_has_await_identifier;
        let name = self.parse_identifier();
        self.statement_has_await_identifier = save_has_await_identifier;
        self.parse_semicolon();
        // NamespaceExportDeclaration nodes cannot have decorators or modifiers, we attach them here so we can report them in the grammar checker
        let node = self.factory.new_namespace_export_declaration(modifiers, name);
        let result = self.finish_node(node, pos);
        self.with_jsdoc(result, jsdoc);
        result
    }

    pub(crate) fn parse_export_declaration(
        &mut self,
        pos: u32,
        jsdoc: JsdocScannerInfo,
        modifiers: Option<P<ModifierList>>,
    ) -> P<Node> {
        let save_context_flags = self.context_flags;
        let save_has_await_identifier = self.statement_has_await_identifier;
        self.set_context_flags(NodeFlags::AwaitContext, true);
        let mut export_clause: Option<P<Node>> = None;
        let mut module_specifier: Option<P<Node>> = None;
        let mut attributes: Option<P<Node>> = None;
        let is_type_only = self.parse_optional(Kind::TypeKeyword);
        let namespace_export_pos = self.node_pos();
        if self.parse_optional(Kind::AsteriskToken) {
            if self.parse_optional(Kind::AsKeyword) {
                export_clause = Some(self.parse_namespace_export(namespace_export_pos));
            }
            self.parse_expected(Kind::FromKeyword);
            module_specifier = Some(self.parse_module_specifier());
        } else {
            export_clause = Some(self.parse_named_exports());
            // It is not uncommon to accidentally omit the 'from' keyword. Additionally, in editing scenarios,
            // the 'from' keyword can be parsed as a named export when the export clause is unterminated (i.e. `export { from "moduleName";`)
            // If we don't have a 'from' keyword, see if we have a string literal such that ASI won't take effect.
            if self.token == Kind::FromKeyword
                || (self.token == Kind::StringLiteral && !self.has_preceding_line_break())
            {
                self.parse_expected(Kind::FromKeyword);
                module_specifier = Some(self.parse_module_specifier());
            }
        }
        if module_specifier.is_some()
            && (self.token == Kind::WithKeyword || self.token == Kind::AssertKeyword)
            && !self.has_preceding_line_break()
        {
            if self.token == Kind::AssertKeyword {
                self.parse_error_at_current_token(
                    &diagnostics::IMPORT_ASSERTIONS_HAVE_BEEN_REPLACED_BY_IMPORT_ATTRIBUTES_USE_WITH_INSTEAD_OF_ASSERT,
                    &[],
                );
            }
            attributes = Some(self.parse_import_attributes(self.token, false /*skipKeyword*/));
        }
        self.parse_semicolon();
        self.context_flags = save_context_flags;
        self.statement_has_await_identifier = save_has_await_identifier;
        let node = self.factory.new_export_declaration(
            modifiers,
            is_type_only,
            export_clause,
            module_specifier,
            attributes,
        );
        let result = self.finish_node(node, pos);
        self.with_jsdoc(result, jsdoc);
        self.check_js_syntax(result);
        result
    }

    fn parse_namespace_export(&mut self, pos: u32) -> P<Node> {
        let (export_name, _) = self.parse_module_export_name(false /*disallowKeywords*/);
        let node = self.factory.new_namespace_export(export_name);
        self.finish_node(node, pos)
    }

    fn parse_named_exports(&mut self) -> P<Node> {
        let pos = self.node_pos();
        // NamedImports:
        //  { }
        //  { ImportsList }
        //  { ImportsList, }
        let exports = self
            .parse_bracketed_list(
                ParsingContext::ImportOrExportSpecifiers,
                Parser::parse_export_specifier,
                Kind::OpenBraceToken,
                Kind::CloseBraceToken,
            )
            .unwrap();
        let node = self.factory.new_named_exports(exports);
        self.finish_node(node, pos)
    }

    fn parse_export_specifier(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let jsdoc = self.jsdoc_scanner_info();
        let (is_type_only, property_name, name) =
            self.parse_import_or_export_specifier(Kind::ExportSpecifier);
        let node = self.factory.new_export_specifier(is_type_only, property_name, name);
        let result = self.finish_node(node, pos);
        self.with_jsdoc(result, jsdoc);
        self.check_js_syntax(result);
        result
    }

    // TYPES

    pub(crate) fn parse_type(&mut self) -> P<Node> {
        let save_context_flags = self.context_flags;
        self.set_context_flags(NodeFlags::TypeExcludesFlags, false);
        let mut type_node;
        if self.is_start_of_function_type_or_constructor_type() {
            type_node = self.parse_function_or_constructor_type();
        } else {
            let pos = self.node_pos();
            type_node = self.parse_union_type_or_higher();
            if !self.in_disallow_conditional_types_context()
                && !self.has_preceding_line_break()
                && self.parse_optional(Kind::ExtendsKeyword)
            {
                // The type following 'extends' is not permitted to be another conditional type
                let extends_type = self.do_in_context(
                    NodeFlags::DisallowConditionalTypesContext,
                    true,
                    Parser::parse_type,
                );
                self.parse_expected(Kind::QuestionToken);
                let true_type = self.do_in_context(
                    NodeFlags::DisallowConditionalTypesContext,
                    false,
                    Parser::parse_type,
                );
                self.parse_expected(Kind::ColonToken);
                let false_type = self.do_in_context(
                    NodeFlags::DisallowConditionalTypesContext,
                    false,
                    Parser::parse_type,
                );
                let conditional_type = self.factory.new_conditional_type_node(
                    type_node,
                    extends_type,
                    true_type,
                    false_type,
                );
                self.finish_node(conditional_type, pos);
                type_node = conditional_type;
            }
        }
        self.context_flags = save_context_flags;
        type_node
    }

    fn parse_union_type_or_higher(&mut self) -> P<Node> {
        self.parse_union_or_intersection_type(
            Kind::BarToken,
            Parser::parse_intersection_type_or_higher,
        )
    }

    fn parse_intersection_type_or_higher(&mut self) -> P<Node> {
        self.parse_union_or_intersection_type(
            Kind::AmpersandToken,
            Parser::parse_type_operator_or_higher,
        )
    }

    // Specialize the two constituent parsers so type parsing does not dispatch through a function pointer.
    fn parse_union_or_intersection_type(
        &mut self,
        operator: Kind,
        parse_constituent_type: impl Fn(&mut Parser) -> P<Node> + Copy,
    ) -> P<Node> {
        let pos = self.node_pos();
        let is_union_type = operator == Kind::BarToken;
        let has_leading_operator = self.parse_optional(operator);
        let mut type_node;
        if has_leading_operator {
            type_node = self
                .parse_function_or_constructor_type_to_error(is_union_type, parse_constituent_type);
        } else {
            type_node = parse_constituent_type(self);
        }
        if self.token == operator || has_leading_operator {
            let start = self.node_stack.len();
            self.node_stack.push(type_node);
            while self.parse_optional(operator) {
                let t = self.parse_function_or_constructor_type_to_error(
                    is_union_type,
                    parse_constituent_type,
                );
                self.node_stack.push(t);
            }
            let end = self.node_pos();
            let list = self.finish_node_list(start, TextRange::new(pos, end));
            type_node = self.create_union_or_intersection_type_node(operator, list);
            self.finish_node(type_node, pos);
        }
        type_node
    }

    fn create_union_or_intersection_type_node(
        &self,
        operator: Kind,
        types: P<NodeList>,
    ) -> P<Node> {
        match operator {
            Kind::BarToken => self.factory.new_union_type_node(types),
            Kind::AmpersandToken => self.factory.new_intersection_type_node(types),
            _ => panic!("Unhandled case in createUnionOrIntersectionType"),
        }
    }

    fn parse_type_operator_or_higher(&mut self) -> P<Node> {
        let operator = self.token;
        match operator {
            Kind::KeyOfKeyword | Kind::UniqueKeyword | Kind::ReadonlyKeyword => {
                return self.parse_type_operator(operator);
            }
            Kind::InferKeyword => {
                return self.parse_infer_type();
            }
            _ => {}
        }
        self.do_in_context(
            NodeFlags::DisallowConditionalTypesContext,
            false,
            Parser::parse_postfix_type_or_higher,
        )
    }

    fn parse_type_operator(&mut self, operator: Kind) -> P<Node> {
        let pos = self.node_pos();
        self.parse_expected(operator);
        let type_ = self.parse_type_operator_or_higher();
        let node = self.factory.new_type_operator_node(operator, type_);
        self.finish_node(node, pos)
    }

    fn parse_infer_type(&mut self) -> P<Node> {
        let pos = self.node_pos();
        self.parse_expected(Kind::InferKeyword);
        let type_parameter = self.parse_type_parameter_of_infer_type();
        let node = self.factory.new_infer_type_node(type_parameter);
        self.finish_node(node, pos)
    }

    fn parse_type_parameter_of_infer_type(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let name = self.parse_identifier();
        let constraint = self.try_parse_constraint_of_infer_type();
        let node = self.factory.new_type_parameter_declaration(
            None, /*modifiers*/
            name, constraint, None, /*expression*/
            None, /*defaultType*/
        );
        self.finish_node(node, pos)
    }

    fn try_parse_constraint_of_infer_type(&mut self) -> Option<P<Node>> {
        let state = self.mark();
        if self.parse_optional(Kind::ExtendsKeyword) {
            let constraint = self.do_in_context(
                NodeFlags::DisallowConditionalTypesContext,
                true,
                Parser::parse_type,
            );
            if self.in_disallow_conditional_types_context() || self.token != Kind::QuestionToken {
                return Some(constraint);
            }
        }
        self.rewind(state);
        None
    }

    fn parse_postfix_type_or_higher(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let mut type_node = self.parse_non_array_type();
        while !self.has_preceding_line_break() {
            match self.token {
                Kind::ExclamationToken => {
                    self.next_token();
                    let node = self.factory.new_jsdoc_non_nullable_type(type_node);
                    type_node = self.finish_node(node, pos);
                }
                Kind::QuestionToken => {
                    // If next token is start of a type we have a conditional type
                    if self.look_ahead(Parser::next_is_start_of_type) {
                        return type_node;
                    }
                    self.next_token();
                    let node = self.factory.new_jsdoc_nullable_type(type_node);
                    type_node = self.finish_node(node, pos);
                }
                Kind::OpenBracketToken => {
                    self.parse_expected(Kind::OpenBracketToken);
                    if self.is_start_of_type(false /*isStartOfParameter*/) {
                        let index_type = self.parse_type();
                        self.parse_expected(Kind::CloseBracketToken);
                        let node = self.factory.new_indexed_access_type_node(type_node, index_type);
                        type_node = self.finish_node(node, pos);
                    } else {
                        self.parse_expected(Kind::CloseBracketToken);
                        let node = self.factory.new_array_type_node(type_node);
                        type_node = self.finish_node(node, pos);
                    }
                }
                _ => return type_node,
            }
        }
        type_node
    }

    fn next_is_start_of_type(&mut self) -> bool {
        self.next_token();
        self.is_start_of_type(false /*inStartOfParameter*/)
    }

    fn parse_non_array_type(&mut self) -> P<Node> {
        match self.token {
            Kind::AnyKeyword
            | Kind::UnknownKeyword
            | Kind::StringKeyword
            | Kind::NumberKeyword
            | Kind::BigIntKeyword
            | Kind::SymbolKeyword
            | Kind::BooleanKeyword
            | Kind::UndefinedKeyword
            | Kind::NeverKeyword
            | Kind::ObjectKeyword => {
                let state = self.mark();
                let keyword_type_node = self.parse_keyword_type_node();
                // If these are followed by a dot then parse these out as a dotted type reference instead
                if self.token != Kind::DotToken {
                    return keyword_type_node;
                }
                self.rewind(state);
                self.parse_type_reference()
            }
            Kind::AsteriskEqualsToken => {
                // If there is '*=', treat it as * followed by postfix =
                self.scanner.re_scan_asterisk_equals_token();
                self.report_scan_errors();
                self.parse_jsdoc_all_type()
            }
            Kind::AsteriskToken => self.parse_jsdoc_all_type(),
            Kind::QuestionQuestionToken => {
                // If there is '??', treat it as prefix-'?' in JSDoc type.
                self.scanner.re_scan_question_token();
                self.report_scan_errors();
                self.parse_jsdoc_nullable_type()
            }
            Kind::QuestionToken => self.parse_jsdoc_nullable_type(),
            Kind::ExclamationToken => self.parse_jsdoc_non_nullable_type(),
            Kind::NoSubstitutionTemplateLiteral
            | Kind::StringLiteral
            | Kind::NumericLiteral
            | Kind::BigIntLiteral
            | Kind::TrueKeyword
            | Kind::FalseKeyword
            | Kind::NullKeyword => self.parse_literal_type_node(false /*negative*/),
            Kind::MinusToken => {
                if self.look_ahead(Parser::next_token_is_numeric_or_big_int_literal) {
                    return self.parse_literal_type_node(true /*negative*/);
                }
                self.parse_type_reference()
            }
            Kind::VoidKeyword => self.parse_keyword_type_node(),
            Kind::ThisKeyword => {
                let this_keyword = self.parse_this_type_node();
                if self.token == Kind::IsKeyword && !self.has_preceding_line_break() {
                    return self.parse_this_type_predicate(this_keyword);
                }
                this_keyword
            }
            Kind::TypeOfKeyword => {
                if self.look_ahead(Parser::next_is_start_of_type_of_import_type) {
                    return self.parse_import_type();
                }
                self.parse_type_query()
            }
            Kind::OpenBraceToken => {
                if self.look_ahead(Parser::next_is_start_of_mapped_type) {
                    return self.parse_mapped_type();
                }
                self.parse_type_literal()
            }
            Kind::OpenBracketToken => self.parse_tuple_type(),
            Kind::OpenParenToken => self.parse_parenthesized_type(),
            Kind::ImportKeyword => self.parse_import_type(),
            Kind::AssertsKeyword => {
                if self.look_ahead(Parser::next_token_is_identifier_or_keyword_on_same_line) {
                    return self.parse_asserts_type_predicate();
                }
                self.parse_type_reference()
            }
            Kind::TemplateHead => self.parse_template_type(),
            _ => self.parse_type_reference(),
        }
    }

    pub(crate) fn parse_keyword_type_node(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let result = self.factory.new_keyword_type_node(self.token);
        self.next_token();
        self.finish_node(result, pos)
    }

    fn parse_this_type_node(&mut self) -> P<Node> {
        let pos = self.node_pos();
        self.next_token();
        let node = self.factory.new_this_type_node();
        self.finish_node(node, pos)
    }

    fn parse_this_type_predicate(&mut self, lhs: P<Node>) -> P<Node> {
        self.next_token();
        let type_ = self.parse_type();
        let node =
            self.factory.new_type_predicate_node(None /*assertsModifier*/, lhs, Some(type_));
        self.finish_node(node, lhs.pos().expect_source("parsed nodes have source positions"))
    }

    fn parse_jsdoc_all_type(&mut self) -> P<Node> {
        let pos = self.node_pos();
        self.next_token();
        let node = self.factory.new_jsdoc_all_type();
        self.finish_node(node, pos)
    }

    fn parse_jsdoc_non_nullable_type(&mut self) -> P<Node> {
        let pos = self.node_pos();
        self.next_token();
        let type_ = self.parse_type_operator_or_higher();
        let node = self.factory.new_jsdoc_non_nullable_type(type_);
        self.finish_node(node, pos)
    }

    fn parse_jsdoc_nullable_type(&mut self) -> P<Node> {
        let pos = self.node_pos();
        // skip the ?
        self.next_token();
        let type_ = self.parse_type_operator_or_higher();
        let node = self.factory.new_jsdoc_nullable_type(type_);
        self.finish_node(node, pos)
    }

    pub(crate) fn parse_jsdoc_type(&mut self) -> P<Node> {
        self.scanner.set_skip_jsdoc_leading_asterisks(true);
        let pos = self.node_pos();

        let has_dot_dot_dot = self.parse_optional(Kind::DotDotDotToken);
        let mut t = self.parse_type_or_type_predicate();
        self.scanner.set_skip_jsdoc_leading_asterisks(false);
        if has_dot_dot_dot {
            let node = self.factory.new_jsdoc_variadic_type(t);
            t = self.finish_node(node, pos);
        }
        if self.token == Kind::EqualsToken {
            self.next_token();
            let node = self.factory.new_jsdoc_optional_type(t);
            return self.finish_node(node, pos);
        }
        t
    }

    fn parse_literal_type_node(&mut self, negative: bool) -> P<Node> {
        let pos = self.node_pos();
        if negative {
            self.next_token();
        }
        let mut expression;
        if self.token == Kind::TrueKeyword
            || self.token == Kind::FalseKeyword
            || self.token == Kind::NullKeyword
        {
            expression = self.parse_keyword_expression();
        } else {
            expression = self.parse_literal_expression();
        }
        if negative {
            let node = self.factory.new_prefix_unary_expression(Kind::MinusToken, expression);
            expression = self.finish_node(node, pos);
        }
        let node = self.factory.new_literal_type_node(expression);
        self.finish_node(node, pos)
    }

    fn parse_type_reference(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let type_name = self.parse_entity_name_of_type_reference();
        let type_arguments = self.parse_type_arguments_of_type_reference();
        let node = self.factory.new_type_reference_node(type_name, type_arguments);
        self.finish_node(node, pos)
    }

    fn parse_entity_name_of_type_reference(&mut self) -> P<Node> {
        self.parse_entity_name(
            true,  /*allowReservedWords*/
            false, /*allowPrivateName*/
            Some(&diagnostics::TYPE_EXPECTED),
        )
    }

    pub(crate) fn parse_entity_name(
        &mut self,
        allow_reserved_words: bool,
        allow_private_name: bool,
        diagnostic_message: Option<&'static Message>,
    ) -> P<Node> {
        let pos = self.node_pos();
        let mut entity;
        if allow_reserved_words {
            entity = self.parse_identifier_name_with_diagnostic(diagnostic_message);
        } else {
            entity = self.parse_identifier_with_diagnostic(diagnostic_message, None);
        }
        while self.parse_optional(Kind::DotToken) {
            if self.token == Kind::LessThanToken {
                // The entity is part of a JSDoc-style generic. We will use the gap between `typeName` and
                // `typeArguments` to report it as a grammar error in the checker.
                break;
            }
            let right = self.parse_right_side_of_dot(
                allow_reserved_words,
                allow_private_name,
                true, /*allowUnicodeEscapeSequenceInIdentifierName*/
            );
            let node = self.factory.new_qualified_name(entity, right);
            entity = self.finish_node(node, pos);
        }
        entity
    }

    pub(crate) fn parse_right_side_of_dot(
        &mut self,
        allow_identifier_names: bool,
        allow_private_identifiers: bool,
        allow_unicode_escape_sequence_in_identifier_name: bool,
    ) -> P<Node> {
        // Technically a keyword is valid here as all identifiers and keywords are identifier names.
        // However, often we'll encounter this in error situations when the identifier or keyword
        // is actually starting another valid construct.
        //
        // So, we check for the following specific case:
        //
        //      name.
        //      identifierOrKeyword identifierNameOrKeyword
        //
        // Note: the newlines are important here.  For example, if that above code
        // were rewritten into:
        //
        //      name.identifierOrKeyword
        //      identifierNameOrKeyword
        //
        // Then we would consider it valid.  That's because ASI would take effect and
        // the code would be implicitly: "name.identifierOrKeyword; identifierNameOrKeyword".
        // In the first case though, ASI will not take effect because there is not a
        // line terminator after the identifier or keyword.
        if self.has_preceding_line_break()
            && token_is_identifier_or_keyword(self.token)
            && self.look_ahead(Parser::next_token_is_identifier_or_keyword_on_same_line)
        {
            // Report that we need an identifier.  However, report it right after the dot,
            // and not on the next token.  This is because the next token might actually
            // be an identifier and the error would be quite confusing.
            let pos = self.node_pos();
            self.parse_error_at(pos, pos, &diagnostics::IDENTIFIER_EXPECTED, &[]);
            return self.create_missing_identifier();
        }
        if self.token == Kind::PrivateIdentifier {
            let node = self.parse_private_identifier();
            if allow_private_identifiers {
                return node;
            }
            let pos = self.node_pos();
            self.parse_error_at(pos, pos, &diagnostics::IDENTIFIER_EXPECTED, &[]);
            return self.create_missing_identifier();
        }
        if allow_identifier_names {
            if allow_unicode_escape_sequence_in_identifier_name {
                return self.parse_identifier_name();
            }
            return self.parse_identifier_name_error_on_unicode_escape_sequence();
        }
        let save_has_await_identifier = self.statement_has_await_identifier;
        let id = self.parse_identifier();
        self.statement_has_await_identifier = save_has_await_identifier;
        id
    }

    pub(crate) fn new_identifier(&mut self, text: &'static str) -> P<Node> {
        self.identifier_count += 1;
        let id = self.factory.new_identifier(text);
        if text == "await" {
            self.statement_has_await_identifier = true;
        }
        id
    }

    /// `new_identifier` for the current token's text, which ends at `loc.end()` (the factory keeps only its length
    /// when it is a slice of the source text).
    pub(crate) fn new_source_identifier(&mut self, text: &'static str, loc: TextRange) -> P<Node> {
        self.identifier_count += 1;
        let id = self.factory.new_source_identifier(text, self.source_text_index, loc);
        if text == "await" {
            self.statement_has_await_identifier = true;
        }
        id
    }

    pub(crate) fn create_missing_identifier(&mut self) -> P<Node> {
        let node = self.new_identifier("");
        let pos = self.node_pos();
        self.finish_node(node, pos)
    }

    pub(crate) fn parse_private_identifier(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let text = self.scanner.token_value();
        self.next_token();
        let node = self.factory.new_private_identifier(text);
        self.finish_node(node, pos)
    }

    pub(crate) fn re_scan_less_than_token(&mut self) -> Kind {
        self.token = self.scanner.re_scan_less_than_token();
        self.report_scan_errors();
        self.token
    }

    pub(crate) fn re_scan_greater_than_token(&mut self) -> Kind {
        self.token = self.scanner.re_scan_greater_than_token();
        self.report_scan_errors();
        self.token
    }

    pub(crate) fn re_scan_slash_token(&mut self) -> Kind {
        self.token = self.scanner.re_scan_slash_token(false);
        self.report_scan_errors();
        self.token
    }

    pub(crate) fn re_scan_template_token(&mut self, is_tagged_template: bool) -> Kind {
        self.token = self.scanner.re_scan_template_token(is_tagged_template);
        self.report_scan_errors();
        self.token
    }

    fn parse_type_arguments_of_type_reference(&mut self) -> Option<P<NodeList>> {
        if !self.has_preceding_line_break() && self.re_scan_less_than_token() == Kind::LessThanToken
        {
            return self.parse_type_arguments();
        }
        None
    }

    pub(crate) fn parse_type_arguments(&mut self) -> Option<P<NodeList>> {
        if self.token == Kind::LessThanToken {
            return self.parse_bracketed_list(
                ParsingContext::TypeArguments,
                Parser::parse_type,
                Kind::LessThanToken,
                Kind::GreaterThanToken,
            );
        }
        None
    }

    fn next_is_start_of_type_of_import_type(&mut self) -> bool {
        self.next_token();
        self.token == Kind::ImportKeyword
    }

    fn parse_import_type(&mut self) -> P<Node> {
        self.source_flags |= NodeFlags::PossiblyContainsDynamicImport;
        let pos = self.node_pos();
        let is_type_of = self.parse_optional(Kind::TypeOfKeyword);
        self.parse_expected(Kind::ImportKeyword);
        self.parse_expected(Kind::OpenParenToken);
        let type_node = self.parse_type();
        let mut attributes: Option<P<Node>> = None;
        if self.parse_optional(Kind::CommaToken) {
            let open_brace_position = self.scanner.token_start();
            self.parse_expected(Kind::OpenBraceToken);
            let current_token = self.token;
            if current_token == Kind::WithKeyword || current_token == Kind::AssertKeyword {
                if current_token == Kind::AssertKeyword {
                    self.parse_error_at_current_token(
                        &diagnostics::IMPORT_ASSERTIONS_HAVE_BEEN_REPLACED_BY_IMPORT_ATTRIBUTES_USE_WITH_INSTEAD_OF_ASSERT,
                        &[],
                    );
                }
                self.next_token();
            } else {
                self.parse_error_at_current_token(
                    &diagnostics::X_0_EXPECTED,
                    &[&scanner::token_to_string(Kind::WithKeyword)],
                );
            }
            self.parse_expected(Kind::ColonToken);
            attributes = Some(self.parse_import_attributes(current_token, true /*skipKeyword*/));
            self.parse_optional(Kind::CommaToken);
            if !self.parse_expected(Kind::CloseBraceToken)
                && let Some(&last_diagnostic) = self.diagnostics.last()
                && last_diagnostic.code() == diagnostics::X_0_EXPECTED.code()
            {
                let related = ast::new_diagnostic(
                    None,
                    TextRange::new(open_brace_position, open_brace_position),
                    &diagnostics::THE_PARSER_EXPECTED_TO_FIND_A_1_TO_MATCH_THE_0_TOKEN_HERE,
                    &[&"{", &"}"],
                );
                last_diagnostic.add_related_info(related);
            }
        }
        self.parse_expected(Kind::CloseParenToken);
        let mut qualifier: Option<P<Node>> = None;
        if self.parse_optional(Kind::DotToken) {
            qualifier = Some(self.parse_entity_name_of_type_reference());
        }
        let type_arguments = self.parse_type_arguments_of_type_reference();
        let node = self.factory.new_import_type_node(
            is_type_of,
            type_node,
            attributes,
            qualifier,
            type_arguments,
        );
        self.finish_node(node, pos)
    }

    fn parse_import_attribute(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let mut name: Option<P<Node>> = None;
        if token_is_identifier_or_keyword(self.token) {
            name = Some(self.parse_identifier_name());
        } else if self.token == Kind::StringLiteral {
            name = Some(self.parse_literal_expression());
        }
        if name.is_some() {
            self.parse_expected(Kind::ColonToken);
        } else {
            self.parse_error_at_current_token(
                &diagnostics::IDENTIFIER_OR_STRING_LITERAL_EXPECTED,
                &[],
            );
        }
        let value = self.parse_assignment_expression_or_higher();
        let node = self.factory.new_import_attribute(name, value);
        self.finish_node(node, pos)
    }

    fn parse_import_attributes(&mut self, token: Kind, skip_keyword: bool) -> P<Node> {
        let pos = self.node_pos();
        if !skip_keyword {
            self.parse_expected(token);
        }
        let elements;
        let mut multi_line = false;
        let open_brace_position = self.scanner.token_start();
        if self.parse_expected(Kind::OpenBraceToken) {
            multi_line = self.has_preceding_line_break();
            elements = self.parse_delimited_list(
                ParsingContext::ImportAttributes,
                Parser::parse_import_attribute,
            );
            if !self.parse_expected(Kind::CloseBraceToken)
                && let Some(&last_diagnostic) = self.diagnostics.last()
                && last_diagnostic.code() == diagnostics::X_0_EXPECTED.code()
            {
                let related = ast::new_diagnostic(
                    None,
                    TextRange::new(open_brace_position, open_brace_position),
                    &diagnostics::THE_PARSER_EXPECTED_TO_FIND_A_1_TO_MATCH_THE_0_TOKEN_HERE,
                    &[&"{", &"}"],
                );
                last_diagnostic.add_related_info(related);
            }
        } else {
            elements = Some(self.parse_empty_node_list());
        }
        let node = self.factory.new_import_attributes(token, elements.unwrap(), multi_line);
        self.finish_node(node, pos)
    }

    fn parse_type_query(&mut self) -> P<Node> {
        let pos = self.node_pos();
        self.parse_expected(Kind::TypeOfKeyword);
        let entity_name = self.parse_entity_name(
            true, /*allowReservedWords*/
            true, /*allowPrivateName*/
            None,
        );
        // Make sure we perform ASI to prevent parsing the next line's type arguments as part of an instantiation expression
        let mut type_arguments: Option<P<NodeList>> = None;
        if !self.has_preceding_line_break() {
            type_arguments = self.parse_type_arguments();
        }
        let node = self.factory.new_type_query_node(entity_name, type_arguments);
        self.finish_node(node, pos)
    }

    fn next_is_start_of_mapped_type(&mut self) -> bool {
        self.next_token();
        if self.token == Kind::PlusToken || self.token == Kind::MinusToken {
            return self.next_token() == Kind::ReadonlyKeyword;
        }
        if self.token == Kind::ReadonlyKeyword {
            self.next_token();
        }
        self.token == Kind::OpenBracketToken
            && self.next_token_is_identifier()
            && self.next_token() == Kind::InKeyword
    }

    fn parse_mapped_type(&mut self) -> P<Node> {
        let pos = self.node_pos();
        self.parse_expected(Kind::OpenBraceToken);
        let mut readonly_token: Option<P<Node>> = None; // ReadonlyKeyword | PlusToken | MinusToken
        if self.token == Kind::ReadonlyKeyword
            || self.token == Kind::PlusToken
            || self.token == Kind::MinusToken
        {
            let token = self.parse_token_node();
            readonly_token = Some(token);
            if token.kind() != Kind::ReadonlyKeyword {
                self.parse_expected(Kind::ReadonlyKeyword);
            }
        }
        self.parse_expected(Kind::OpenBracketToken);
        let type_parameter = self.parse_mapped_type_parameter();
        let mut name_type: Option<P<Node>> = None;
        if self.parse_optional(Kind::AsKeyword) {
            name_type = Some(self.parse_type());
        }
        self.parse_expected(Kind::CloseBracketToken);
        let mut question_token: Option<P<Node>> = None; // QuestionToken | PlusToken | MinusToken
        if self.token == Kind::QuestionToken
            || self.token == Kind::PlusToken
            || self.token == Kind::MinusToken
        {
            let token = self.parse_token_node();
            question_token = Some(token);
            if token.kind() != Kind::QuestionToken {
                self.parse_expected(Kind::QuestionToken);
            }
        }
        let type_node = self.parse_type_annotation();
        self.parse_semicolon();
        let members = self.parse_list(ParsingContext::TypeMembers, Parser::parse_type_member);
        self.parse_expected(Kind::CloseBraceToken);
        let node = self.factory.new_mapped_type_node(
            readonly_token,
            type_parameter,
            name_type,
            question_token,
            type_node,
            Some(members),
        );
        self.finish_node(node, pos)
    }

    fn parse_mapped_type_parameter(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let name = self.parse_identifier_name();
        self.parse_expected(Kind::InKeyword);
        let type_node = self.parse_type();
        let node = self.factory.new_type_parameter_declaration(
            None, /*modifiers*/
            name,
            Some(type_node),
            None, /*expression*/
            None, /*defaultType*/
        );
        self.finish_node(node, pos)
    }

    pub(crate) fn parse_type_member(&mut self) -> P<Node> {
        if self.token == Kind::OpenParenToken || self.token == Kind::LessThanToken {
            return self.parse_signature_member(Kind::CallSignature);
        }
        if self.token == Kind::NewKeyword
            && self.look_ahead(Parser::next_token_is_open_paren_or_less_than)
        {
            return self.parse_signature_member(Kind::ConstructSignature);
        }
        let pos = self.node_pos();
        let jsdoc = self.jsdoc_scanner_info();
        let modifiers = self.parse_modifiers();
        if self.parse_contextual_modifier(Kind::GetKeyword) {
            return self.parse_accessor_declaration(
                pos,
                jsdoc,
                modifiers,
                Kind::GetAccessor,
                ParseFlags::Type,
            );
        }
        if self.parse_contextual_modifier(Kind::SetKeyword) {
            return self.parse_accessor_declaration(
                pos,
                jsdoc,
                modifiers,
                Kind::SetAccessor,
                ParseFlags::Type,
            );
        }
        if self.is_index_signature() {
            return self.parse_index_signature_declaration(pos, jsdoc, modifiers);
        }
        self.parse_property_or_method_signature(pos, jsdoc, modifiers)
    }

    pub(crate) fn next_token_is_open_paren_or_less_than(&mut self) -> bool {
        self.next_token();
        self.token == Kind::OpenParenToken || self.token == Kind::LessThanToken
    }

    fn parse_signature_member(&mut self, kind: Kind) -> P<Node> {
        let pos = self.node_pos();
        let jsdoc = self.jsdoc_scanner_info();
        if kind == Kind::ConstructSignature {
            self.parse_expected(Kind::NewKeyword);
        }
        let type_parameters = self.parse_type_parameters();
        let parameters = self.parse_parameters(ParseFlags::Type);
        let type_node = self.parse_return_type(Kind::ColonToken, true /*isType*/);
        self.parse_type_member_semicolon();

        let result = if kind == Kind::CallSignature {
            self.factory.new_call_signature_declaration(
                type_parameters,
                Some(parameters),
                type_node,
            )
        } else {
            self.factory.new_construct_signature_declaration(
                type_parameters,
                Some(parameters),
                type_node,
            )
        };
        self.finish_node(result, pos);
        self.with_jsdoc(result, jsdoc);
        result
    }

    pub(crate) fn parse_type_parameters(&mut self) -> Option<P<NodeList>> {
        if self.token == Kind::LessThanToken {
            return self.parse_bracketed_list(
                ParsingContext::TypeParameters,
                Parser::parse_type_parameter,
                Kind::LessThanToken,
                Kind::GreaterThanToken,
            );
        }
        None
    }

    fn parse_type_parameter(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let modifiers = self.parse_modifiers_ex(
            false, /*allowDecorators*/
            true,  /*permitConstAsModifier*/
            false, /*stopOnStartOfClassStaticBlock*/
        );
        let name = self.parse_identifier();
        let mut constraint: Option<P<Node>> = None;
        let mut expression: Option<P<Node>> = None;
        if self.parse_optional(Kind::ExtendsKeyword) {
            // It's not uncommon for people to write improper constraints to a generic.  If the
            // user writes a constraint that is an expression and not an actual type, then parse
            // it out as an expression (so we can recover well), but report that a type is needed
            // instead.
            if self.is_start_of_type(false /*inStartOfParameter*/) || !self.is_start_of_expression()
            {
                constraint = Some(self.parse_type());
            } else {
                // It was not a type, and it looked like an expression.  Parse out an expression
                // here so we recover well.  Note: it is important that we call parseUnaryExpression
                // and not parseExpression here.  If the user has:
                //
                //      <T extends "">
                //
                // We do *not* want to consume the `>` as we're consuming the expression for "".
                expression = Some(self.parse_unary_expression_or_higher());
            }
        }
        let mut default_type: Option<P<Node>> = None;
        if self.parse_optional(Kind::EqualsToken) {
            default_type = Some(self.parse_type());
        }
        let result = self.factory.new_type_parameter_declaration(
            modifiers,
            name,
            constraint,
            expression,
            default_type,
        );
        self.finish_node(result, pos)
    }

    pub(crate) fn parse_parameters(&mut self, flags: ParseFlags) -> P<NodeList> {
        // FormalParameters [Yield,Await]: (modified)
        //      [empty]
        //      FormalParameterList[?Yield,Await]
        //
        // FormalParameter[Yield,Await]: (modified)
        //      BindingElement[?Yield,Await]
        //
        // BindingElement [Yield,Await]: (modified)
        //      SingleNameBinding[?Yield,?Await]
        //      BindingPattern[?Yield,?Await]Initializer [In, ?Yield,?Await] opt
        //
        // SingleNameBinding [Yield,Await]:
        //      BindingIdentifier[?Yield,?Await]Initializer [In, ?Yield,?Await] opt
        if self.parse_expected(Kind::OpenParenToken) {
            // With allowAmbiguity, parseParameterEx never returns nil, so neither does the list.
            let parameters = self.parse_parameters_worker(flags, true /*allowAmbiguity*/).unwrap();
            self.parse_expected(Kind::CloseParenToken);
            return parameters;
        }
        self.create_missing_list()
    }

    pub(crate) fn parse_parameters_worker(
        &mut self,
        flags: ParseFlags,
        allow_ambiguity: bool,
    ) -> Option<P<NodeList>> {
        // FormalParameters [Yield,Await]: (modified)
        //      [empty]
        //      FormalParameterList[?Yield,Await]
        //
        // FormalParameter[Yield,Await]: (modified)
        //      BindingElement[?Yield,Await]
        //
        // BindingElement [Yield,Await]: (modified)
        //      SingleNameBinding[?Yield,?Await]
        //      BindingPattern[?Yield,?Await]Initializer [In, ?Yield,?Await] opt
        //
        // SingleNameBinding [Yield,Await]:
        //      BindingIdentifier[?Yield,?Await]Initializer [In, ?Yield,?Await] opt
        let in_await_context = self.context_flags.intersects(NodeFlags::AwaitContext);
        let save_context_flags = self.context_flags;
        self.set_context_flags(NodeFlags::YieldContext, flags.intersects(ParseFlags::Yield));
        self.set_context_flags(NodeFlags::AwaitContext, flags.intersects(ParseFlags::Await));
        let parameters =
            self.parse_delimited_list(ParsingContext::Parameters, move |p: &mut Parser| {
                let parameter = p.parse_parameter_ex(in_await_context, allow_ambiguity);
                if let Some(parameter) = parameter
                    && !flags.intersects(ParseFlags::Type)
                {
                    p.check_js_syntax(parameter);
                }
                parameter
            });
        self.context_flags = save_context_flags;
        parameters
    }

    fn parse_parameter(&mut self) -> P<Node> {
        self.parse_parameter_ex(false /*inOuterAwaitContext*/, true /*allowAmbiguity*/).unwrap()
    }

    fn parse_parameter_ex(
        &mut self,
        in_outer_await_context: bool,
        allow_ambiguity: bool,
    ) -> Option<P<Node>> {
        let pos = self.node_pos();
        let jsdoc = self.jsdoc_scanner_info();
        // FormalParameter [Yield,Await]:
        //      BindingElement[?Yield,?Await]
        // Decorators are parsed in the outer [Await] context, the rest of the parameter is parsed in the function's [Await] context.
        let save_context_flags = self.context_flags;
        self.set_context_flags(NodeFlags::AwaitContext, in_outer_await_context);
        let modifiers = self.parse_modifiers_ex(
            true,  /*allowDecorators*/
            false, /*permitConstAsModifier*/
            false, /*stopOnStartOfClassStaticBlock*/
        );
        self.context_flags = save_context_flags;
        if self.token == Kind::ThisKeyword {
            let name = self.create_identifier(true /*isIdentifier*/);
            let type_annotation = self.parse_type_annotation();
            let result = self.factory.new_parameter_declaration(
                modifiers,
                None, /*dotDotDotToken*/
                name,
                None, /*questionToken*/
                type_annotation,
                None, /*initializer*/
            );
            if let Some(modifiers) = modifiers {
                self.parse_error_at_range(
                    modifiers.nodes()[0].loc(),
                    &diagnostics::NEITHER_DECORATORS_NOR_MODIFIERS_MAY_BE_APPLIED_TO_THIS_PARAMETERS,
                    &[],
                );
            }
            let result = self.finish_node(result, pos);
            self.with_jsdoc(result, jsdoc);
            return Some(result);
        }
        let dot_dot_dot_token = self.parse_optional_token(Kind::DotDotDotToken);
        if !allow_ambiguity && !self.is_parameter_name_start() {
            return None;
        }
        let name = self.parse_name_of_parameter(modifiers);
        let question_token = self.parse_optional_token(Kind::QuestionToken);
        let type_annotation = self.parse_type_annotation();
        let initializer = self.parse_initializer();
        let result = self.factory.new_parameter_declaration(
            modifiers,
            dot_dot_dot_token,
            name,
            question_token,
            type_annotation,
            initializer,
        );
        let result = self.finish_node(result, pos);
        self.with_jsdoc(result, jsdoc);
        Some(result)
    }

    fn is_parameter_name_start(&self) -> bool {
        // Be permissive about await and yield by calling isBindingIdentifier instead of isIdentifier; disallowing
        // them during a speculative parse leads to many more follow-on errors than allowing the function to parse then later
        // complaining about the use of the keywords.
        self.is_binding_identifier()
            || self.token == Kind::OpenBracketToken
            || self.token == Kind::OpenBraceToken
    }

    fn parse_name_of_parameter(&mut self, modifiers: Option<P<ModifierList>>) -> P<Node> {
        // FormalParameter [Yield,Await]:
        //      BindingElement[?Yield,?Await]
        let name = self.parse_identifier_or_pattern_with_diagnostic(Some(
            &diagnostics::PRIVATE_IDENTIFIERS_CANNOT_BE_USED_AS_PARAMETERS,
        ));
        if name.loc().len() == Some(0) && modifiers.is_none() && ast::is_modifier_kind(self.token) {
            // in cases like
            // 'use strict'
            // function foo(static)
            // isParameter('static') == true, because of isModifier('static')
            // however 'static' is not a legal identifier in a strict mode.
            // so result of this function will be Parameter (flags = 0, name = missing, type = undefined, initializer = undefined)
            // and current token will not change => parsing of the enclosing parameter list will last till the end of time (or OOM)
            // to avoid this we'll advance cursor to the next token.
            self.next_token();
        }
        name
    }

    pub(crate) fn parse_return_type(
        &mut self,
        return_token: Kind,
        is_type: bool,
    ) -> Option<P<Node>> {
        if self.should_parse_return_type(return_token, is_type) {
            return Some(self.do_in_context(
                NodeFlags::DisallowConditionalTypesContext,
                false,
                Parser::parse_type_or_type_predicate,
            ));
        }
        None
    }

    fn should_parse_return_type(&mut self, return_token: Kind, is_type: bool) -> bool {
        if return_token == Kind::EqualsGreaterThanToken {
            self.parse_expected(return_token);
            return true;
        } else if self.parse_optional(Kind::ColonToken) {
            return true;
        } else if is_type && self.token == Kind::EqualsGreaterThanToken {
            // This is easy to get backward, especially in type contexts, so parse the type anyway
            self.parse_error_at_current_token(
                &diagnostics::X_0_EXPECTED,
                &[&scanner::token_to_string(Kind::ColonToken)],
            );
            self.next_token();
            return true;
        }
        false
    }

    fn parse_type_or_type_predicate(&mut self) -> P<Node> {
        if self.is_identifier() {
            let state = self.mark();
            let pos = self.node_pos();
            let id = self.parse_identifier();
            if self.token == Kind::IsKeyword && !self.has_preceding_line_break() {
                self.next_token();
                let type_ = self.parse_type();
                let node = self.factory.new_type_predicate_node(
                    None, /*assertsModifier*/
                    id,
                    Some(type_),
                );
                return self.finish_node(node, pos);
            }
            self.rewind(state);
        }
        self.parse_type()
    }

    fn parse_type_member_semicolon(&mut self) {
        // We allow type members to be separated by commas or (possibly ASI) semicolons.
        // First check if it was a comma.  If so, we're done with the member.
        if self.parse_optional(Kind::CommaToken) {
            return;
        }
        // Didn't have a comma.  We must have a (possible ASI) semicolon.
        self.parse_semicolon();
    }

    pub(crate) fn parse_accessor_declaration(
        &mut self,
        pos: u32,
        jsdoc: JsdocScannerInfo,
        modifiers: Option<P<ModifierList>>,
        kind: Kind,
        flags: ParseFlags,
    ) -> P<Node> {
        let name = self.parse_property_name();
        let type_parameters = self.parse_type_parameters();
        let parameters = self.parse_parameters(ParseFlags::None);
        let return_type = self.parse_return_type(Kind::ColonToken, false /*isType*/);
        let body = self.parse_function_block_or_semicolon(flags, None /*diagnosticMessage*/);

        // Keep track of `typeParameters` (for both) and `type` (for setters) if they were parsed those indicate grammar errors
        let result = if kind == Kind::GetAccessor {
            self.factory.new_get_accessor_declaration(
                modifiers,
                name,
                type_parameters,
                Some(parameters),
                return_type,
                None, /*fullSignature*/
                body,
            )
        } else {
            self.factory.new_set_accessor_declaration(
                modifiers,
                name,
                type_parameters,
                Some(parameters),
                return_type,
                None, /*fullSignature*/
                body,
            )
        };
        let result = self.finish_node(result, pos);
        self.with_jsdoc(result, jsdoc);
        if !flags.intersects(ParseFlags::Type) {
            self.check_js_syntax(result);
        }
        result
    }

    pub(crate) fn parse_property_name(&mut self) -> P<Node> {
        let save_has_await_identifier = self.statement_has_await_identifier;
        let prop = self.parse_property_name_worker(true /*allowComputedPropertyNames*/);
        self.statement_has_await_identifier = save_has_await_identifier;
        prop
    }

    fn parse_property_name_worker(&mut self, allow_computed_property_names: bool) -> P<Node> {
        if self.token == Kind::StringLiteral
            || self.token == Kind::NumericLiteral
            || self.token == Kind::BigIntLiteral
        {
            return self.parse_literal_expression();
        }
        if allow_computed_property_names && self.token == Kind::OpenBracketToken {
            return self.parse_computed_property_name();
        }
        if self.token == Kind::PrivateIdentifier {
            return self.parse_private_identifier();
        }
        self.parse_identifier_name()
    }

    fn parse_computed_property_name(&mut self) -> P<Node> {
        // PropertyName [Yield]:
        //      LiteralPropertyName
        //      ComputedPropertyName[?Yield]
        let pos = self.node_pos();
        self.parse_expected(Kind::OpenBracketToken);
        // We parse any expression (including a comma expression). But the grammar
        // says that only an assignment expression is allowed, so the grammar checker
        // will error if it sees a comma expression.
        let expression = self.parse_expression_allow_in();
        self.parse_expected(Kind::CloseBracketToken);
        let node = self.factory.new_computed_property_name(expression);
        self.finish_node(node, pos)
    }

    pub(crate) fn parse_function_block_or_semicolon(
        &mut self,
        flags: ParseFlags,
        diagnostic_message: Option<&'static Message>,
    ) -> Option<P<Node>> {
        if self.token != Kind::OpenBraceToken {
            if flags.intersects(ParseFlags::Type) {
                self.parse_type_member_semicolon();
                return None;
            }
            if self.can_parse_semicolon() {
                self.parse_semicolon();
                return None;
            }
        }
        Some(self.parse_function_block(flags, diagnostic_message))
    }

    pub(crate) fn parse_function_block(
        &mut self,
        flags: ParseFlags,
        diagnostic_message: Option<&'static Message>,
    ) -> P<Node> {
        let save_context_flags = self.context_flags;
        let save_has_await_identifier = self.statement_has_await_identifier;
        self.set_context_flags(NodeFlags::YieldContext, flags.intersects(ParseFlags::Yield));
        self.set_context_flags(NodeFlags::AwaitContext, flags.intersects(ParseFlags::Await));
        // We may be in a [Decorator] context when parsing a function expression or
        // arrow function. The body of the function is not in [Decorator] context.
        self.set_context_flags(NodeFlags::DecoratorContext, false);
        let block = self
            .parse_block(flags.intersects(ParseFlags::IgnoreMissingOpenBrace), diagnostic_message);
        self.context_flags = save_context_flags;
        self.statement_has_await_identifier = save_has_await_identifier;
        block
    }

    pub(crate) fn is_index_signature(&mut self) -> bool {
        self.token == Kind::OpenBracketToken
            && self.look_ahead(Parser::next_is_unambiguously_index_signature)
    }

    fn next_is_unambiguously_index_signature(&mut self) -> bool {
        // The only allowed sequence is:
        //
        //   [id:
        //
        // However, for error recovery, we also check the following cases:
        //
        //   [...
        //   [id,
        //   [id?,
        //   [id?:
        //   [id?]
        //   [public id
        //   [private id
        //   [protected id
        //   []
        //
        self.next_token();
        if self.token == Kind::DotDotDotToken || self.token == Kind::CloseBracketToken {
            return true;
        }
        if ast::is_modifier_kind(self.token) {
            self.next_token();
            if self.is_identifier() {
                return true;
            }
        } else if !self.is_identifier() {
            return false;
        } else {
            // Skip the identifier
            self.next_token();
        }
        // A colon signifies a well formed indexer
        // A comma should be a badly formed indexer because comma expressions are not allowed
        // in computed properties.
        if self.token == Kind::ColonToken || self.token == Kind::CommaToken {
            return true;
        }
        // Question mark could be an indexer with an optional property,
        // or it could be a conditional expression in a computed property.
        if self.token != Kind::QuestionToken {
            return false;
        }
        // If any of the following tokens are after the question mark, it cannot
        // be a conditional expression, so treat it as an indexer.
        self.next_token();
        self.token == Kind::ColonToken
            || self.token == Kind::CommaToken
            || self.token == Kind::CloseBracketToken
    }

    pub(crate) fn parse_index_signature_declaration(
        &mut self,
        pos: u32,
        jsdoc: JsdocScannerInfo,
        modifiers: Option<P<ModifierList>>,
    ) -> P<Node> {
        let parameters = self
            .parse_bracketed_list(
                ParsingContext::Parameters,
                Parser::parse_parameter,
                Kind::OpenBracketToken,
                Kind::CloseBracketToken,
            )
            .unwrap();
        let type_node = self.parse_type_annotation();
        self.parse_type_member_semicolon();
        let node =
            self.factory.new_index_signature_declaration(modifiers, Some(parameters), type_node);
        let result = self.finish_node(node, pos);
        self.with_jsdoc(result, jsdoc);
        result
    }

    fn parse_property_or_method_signature(
        &mut self,
        pos: u32,
        jsdoc: JsdocScannerInfo,
        modifiers: Option<P<ModifierList>>,
    ) -> P<Node> {
        let name = self.parse_property_name();
        let question_token = self.parse_optional_token(Kind::QuestionToken);

        let result = if self.token == Kind::OpenParenToken || self.token == Kind::LessThanToken {
            // Method signatures don't exist in expression contexts.  So they have neither
            // [Yield] nor [Await]
            let type_parameters = self.parse_type_parameters();
            let parameters = self.parse_parameters(ParseFlags::Type);
            let return_type = self.parse_return_type(Kind::ColonToken, true /*isType*/);
            self.factory.new_method_signature_declaration(
                modifiers,
                name,
                question_token,
                type_parameters,
                Some(parameters),
                return_type,
            )
        } else {
            let type_node = self.parse_type_annotation();
            // Although type literal properties cannot not have initializers, we attempt
            // to parse an initializer so we can report in the checker that an interface
            // property or type literal property cannot have an initializer.
            let mut initializer: Option<P<Node>> = None;
            if self.token == Kind::EqualsToken {
                initializer = self.parse_initializer();
            }
            self.factory.new_property_signature_declaration(
                modifiers,
                name,
                question_token,
                type_node,
                initializer,
            )
        };
        self.parse_type_member_semicolon();
        let result = self.finish_node(result, pos);
        self.with_jsdoc(result, jsdoc);
        result
    }

    pub(crate) fn parse_type_literal(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let members = self.parse_object_type_members();
        let node = self.factory.new_type_literal_node(members);
        Self::set_lazy_owner(members, node);
        self.finish_node(node, pos)
    }

    pub(crate) fn parse_object_type_members(&mut self) -> P<NodeList> {
        if self.parse_expected(Kind::OpenBraceToken) {
            let members =
                self.parse_member_list(ParsingContext::TypeMembers, Parser::parse_type_member);
            self.parse_expected(Kind::CloseBraceToken);
            return members;
        }
        self.create_missing_list()
    }

    fn parse_tuple_type(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let elements = self
            .parse_bracketed_list(
                ParsingContext::TupleElementTypes,
                Parser::parse_tuple_element_name_or_tuple_element_type,
                Kind::OpenBracketToken,
                Kind::CloseBracketToken,
            )
            .unwrap();
        let node = self.factory.new_tuple_type_node(elements);
        self.finish_node(node, pos)
    }

    fn parse_tuple_element_name_or_tuple_element_type(&mut self) -> P<Node> {
        if self.look_ahead(Parser::scan_start_of_named_tuple_element) {
            let pos = self.node_pos();
            let jsdoc = self.jsdoc_scanner_info();
            let dot_dot_dot_token = self.parse_optional_token(Kind::DotDotDotToken);
            let name = self.parse_identifier_name();
            let question_token = self.parse_optional_token(Kind::QuestionToken);
            self.parse_expected(Kind::ColonToken);
            let type_node = self.parse_tuple_element_type();
            let node = self.factory.new_named_tuple_member(
                dot_dot_dot_token,
                name,
                question_token,
                type_node,
            );
            let result = self.finish_node(node, pos);
            self.with_jsdoc(result, jsdoc);
            return result;
        }
        self.parse_tuple_element_type()
    }

    fn scan_start_of_named_tuple_element(&mut self) -> bool {
        if self.token == Kind::DotDotDotToken {
            return token_is_identifier_or_keyword(self.next_token())
                && self.next_token_is_colon_or_question_colon();
        }
        token_is_identifier_or_keyword(self.token) && self.next_token_is_colon_or_question_colon()
    }

    fn next_token_is_colon_or_question_colon(&mut self) -> bool {
        self.next_token() == Kind::ColonToken
            || self.token == Kind::QuestionToken && self.next_token() == Kind::ColonToken
    }

    fn parse_tuple_element_type(&mut self) -> P<Node> {
        let pos = self.node_pos();
        if self.parse_optional(Kind::DotDotDotToken) {
            let type_ = self.parse_type();
            let node = self.factory.new_rest_type_node(type_);
            return self.finish_node(node, pos);
        }
        let type_node = self.parse_type();
        if ast::is_jsdoc_nullable_type(type_node)
            && type_node.pos() == type_node.type_node().unwrap().pos()
        {
            let inner = type_node.type_node().unwrap();
            let node = self.factory.new_optional_type_node(inner);
            node.set_flags(type_node.flags());
            node.set_loc(type_node.loc());
            inner.set_parent(Some(node));
            return node;
        }
        type_node
    }

    fn parse_parenthesized_type(&mut self) -> P<Node> {
        let pos = self.node_pos();
        self.parse_expected(Kind::OpenParenToken);
        let type_node = self.parse_type();
        self.parse_expected(Kind::CloseParenToken);
        let node = self.factory.new_parenthesized_type_node(type_node);
        self.finish_node(node, pos)
    }

    fn parse_asserts_type_predicate(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let asserts_modifier = self.parse_expected_token(Kind::AssertsKeyword);

        let parameter_name = if self.token == Kind::ThisKeyword {
            self.parse_this_type_node()
        } else {
            self.parse_identifier()
        };
        let mut type_node: Option<P<Node>> = None;
        if self.parse_optional(Kind::IsKeyword) {
            type_node = Some(self.parse_type());
        }
        let node =
            self.factory.new_type_predicate_node(Some(asserts_modifier), parameter_name, type_node);
        self.finish_node(node, pos)
    }

    fn parse_template_type(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let head = self.parse_template_head(false /*isTaggedTemplate*/);
        let spans = self.parse_template_type_spans();
        let node = self.factory.new_template_literal_type_node(head, spans);
        self.finish_node(node, pos)
    }

    pub(crate) fn parse_template_head(&mut self, is_tagged_template: bool) -> P<Node> {
        if !is_tagged_template && self.scanner.token_flags().intersects(TokenFlags::IsInvalid) {
            self.re_scan_template_token(false /*isTaggedTemplate*/);
        }
        let pos = self.node_pos();
        let text = self.scanner.token_value();
        let raw_text = self.get_template_literal_raw_text(2 /*endLength*/);
        let result = self.factory.new_template_head(text, raw_text, self.scanner.token_flags());
        self.next_token();
        self.finish_node(result, pos)
    }

    fn get_template_literal_raw_text(&self, end_length: usize) -> &'static str {
        let mut end_length = end_length;
        let token_text = self.scanner.token_text();
        if self.scanner.token_flags().intersects(TokenFlags::Unterminated) {
            end_length = 0;
        }
        &token_text[1..token_text.len() - end_length]
    }

    fn parse_template_type_spans(&mut self) -> P<NodeList> {
        let pos = self.node_pos();
        let start = self.node_stack.len();
        loop {
            let span = self.parse_template_type_span();
            self.node_stack.push(span);
            if span.as_template_literal_type_span().literal.kind() != Kind::TemplateMiddle {
                break;
            }
        }
        let end = self.node_pos();
        self.finish_node_list(start, TextRange::new(pos, end))
    }

    fn parse_template_type_span(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let type_ = self.parse_type();
        let literal = self.parse_literal_of_template_span(false /*isTaggedTemplate*/);
        let node = self.factory.new_template_literal_type_span(type_, literal);
        self.finish_node(node, pos)
    }

    pub(crate) fn parse_literal_of_template_span(&mut self, is_tagged_template: bool) -> P<Node> {
        if self.token == Kind::CloseBraceToken {
            self.re_scan_template_token(is_tagged_template);
            return self.parse_template_middle_or_tail();
        }
        self.parse_error_at_current_token(
            &diagnostics::X_0_EXPECTED,
            &[&scanner::token_to_string(Kind::CloseBraceToken)],
        );
        let node = self.factory.new_template_tail("", "", TokenFlags::None);
        let pos = self.node_pos();
        self.finish_node(node, pos)
    }

    fn parse_template_middle_or_tail(&mut self) -> P<Node> {
        let pos = self.node_pos();

        let result = if self.token == Kind::TemplateMiddle {
            let text = self.scanner.token_value();
            let raw_text = self.get_template_literal_raw_text(2 /*endLength*/);
            self.factory.new_template_middle(text, raw_text, self.scanner.token_flags())
        } else {
            let text = self.scanner.token_value();
            let raw_text = self.get_template_literal_raw_text(1 /*endLength*/);
            self.factory.new_template_tail(text, raw_text, self.scanner.token_flags())
        };
        self.next_token();
        self.finish_node(result, pos)
    }

    fn parse_function_or_constructor_type_to_error(
        &mut self,
        is_in_union_type: bool,
        parse_constituent_type: impl Fn(&mut Parser) -> P<Node> + Copy,
    ) -> P<Node> {
        // the function type and constructor type shorthand notation
        // are not allowed directly in unions and intersections, but we'll
        // try to parse them gracefully and issue a helpful message.
        if self.is_start_of_function_type_or_constructor_type() {
            let type_node = self.parse_function_or_constructor_type();

            let diagnostic: &'static Message = if type_node.kind() == Kind::FunctionType {
                if is_in_union_type {
                    &diagnostics::FUNCTION_TYPE_NOTATION_MUST_BE_PARENTHESIZED_WHEN_USED_IN_A_UNION_TYPE
                } else {
                    &diagnostics::FUNCTION_TYPE_NOTATION_MUST_BE_PARENTHESIZED_WHEN_USED_IN_AN_INTERSECTION_TYPE
                }
            } else {
                if is_in_union_type {
                    &diagnostics::CONSTRUCTOR_TYPE_NOTATION_MUST_BE_PARENTHESIZED_WHEN_USED_IN_A_UNION_TYPE
                } else {
                    &diagnostics::CONSTRUCTOR_TYPE_NOTATION_MUST_BE_PARENTHESIZED_WHEN_USED_IN_AN_INTERSECTION_TYPE
                }
            };
            self.parse_error_at_range(type_node.loc(), diagnostic, &[]);
            return type_node;
        }
        parse_constituent_type(self)
    }

    fn is_start_of_function_type_or_constructor_type(&mut self) -> bool {
        self.token == Kind::LessThanToken
            || self.token == Kind::OpenParenToken
                && self.look_ahead(Parser::next_is_unambiguously_start_of_function_type)
            || self.token == Kind::NewKeyword
            || self.token == Kind::AbstractKeyword
                && self.look_ahead(Parser::next_token_is_new_keyword)
    }

    fn parse_function_or_constructor_type(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let jsdoc = self.jsdoc_scanner_info();
        let modifiers = self.parse_modifiers_for_constructor_type();
        let is_constructor_type = self.parse_optional(Kind::NewKeyword);
        assert!(
            modifiers.is_none() || is_constructor_type,
            "Per isStartOfFunctionOrConstructorType, a function type cannot have modifiers."
        );
        let type_parameters = self.parse_type_parameters();
        let parameters = self.parse_parameters(ParseFlags::Type);
        let return_type =
            self.parse_return_type(Kind::EqualsGreaterThanToken, false /*isType*/);

        let result = if is_constructor_type {
            self.factory.new_constructor_type_node(
                modifiers,
                type_parameters,
                Some(parameters),
                return_type,
            )
        } else {
            self.factory.new_function_type_node(type_parameters, Some(parameters), return_type)
        };
        self.finish_node(result, pos);
        self.with_jsdoc(result, jsdoc);
        result
    }

    fn parse_modifiers_for_constructor_type(&mut self) -> Option<P<ModifierList>> {
        if self.token == Kind::AbstractKeyword {
            let pos = self.node_pos();
            let modifier = self.factory.new_modifier(self.token);
            self.next_token();
            self.finish_node(modifier, pos);
            return Some(self.new_modifier_list(modifier.loc(), &[modifier]));
        }
        None
    }

    fn next_token_is_new_keyword(&mut self) -> bool {
        self.next_token() == Kind::NewKeyword
    }

    fn next_is_unambiguously_start_of_function_type(&mut self) -> bool {
        self.next_token();
        if self.token == Kind::CloseParenToken || self.token == Kind::DotDotDotToken {
            // ( )
            // ( ...
            return true;
        }
        if self.skip_parameter_start() {
            // We successfully skipped modifiers (if any) and an identifier or binding pattern,
            // now see if we have something that indicates a parameter declaration
            if self.token == Kind::ColonToken
                || self.token == Kind::CommaToken
                || self.token == Kind::QuestionToken
                || self.token == Kind::EqualsToken
            {
                // ( xxx :
                // ( xxx ,
                // ( xxx ?
                // ( xxx =
                return true;
            }
            if self.token == Kind::CloseParenToken
                && self.next_token() == Kind::EqualsGreaterThanToken
            {
                // ( xxx ) =>
                return true;
            }
        }
        false
    }

    fn skip_parameter_start(&mut self) -> bool {
        if ast::is_modifier_kind(self.token) {
            // Skip modifiers
            self.parse_modifiers();
        }
        self.parse_optional(Kind::DotDotDotToken);
        if self.is_identifier() || self.token == Kind::ThisKeyword {
            self.next_token();
            return true;
        }
        if self.token == Kind::OpenBracketToken || self.token == Kind::OpenBraceToken {
            // Return true if we can parse an array or object binding pattern with no errors
            let previous_error_count = self.diagnostics.len();
            self.parse_identifier_or_pattern();
            return previous_error_count == self.diagnostics.len();
        }
        false
    }

    pub(crate) fn parse_modifiers(&mut self) -> Option<P<ModifierList>> {
        self.parse_modifiers_ex(false, false, false)
    }

    // Keep speculative modifier parsing and list construction out of the empty-list path.
    #[inline]
    pub(crate) fn parse_modifiers_ex(
        &mut self,
        allow_decorators: bool,
        permit_const_as_modifier: bool,
        stop_on_start_of_class_static_block: bool,
    ) -> Option<P<ModifierList>> {
        if !(ast::is_modifier_kind(self.token) || allow_decorators && self.token == Kind::AtToken) {
            return None;
        }
        self.parse_modifiers_worker(
            allow_decorators,
            permit_const_as_modifier,
            stop_on_start_of_class_static_block,
        )
    }

    #[inline(never)]
    fn parse_modifiers_worker(
        &mut self,
        allow_decorators: bool,
        permit_const_as_modifier: bool,
        stop_on_start_of_class_static_block: bool,
    ) -> Option<P<ModifierList>> {
        let mut has_leading_modifier = false;
        let mut has_trailing_decorator = false;
        let mut has_trailing_modifier = false;
        let mut has_static_modifier = false;
        // Decorators should be contiguous in a list of modifiers but can potentially appear in two places (i.e., `[...leadingDecorators, ...leadingModifiers, ...trailingDecorators, ...trailingModifiers]`).
        // The leading modifiers *should* only contain `export` and `default` when trailingDecorators are present, but we'll handle errors for any other leading modifiers in the checker.
        // It is illegal to have both leadingDecorators and trailingDecorators, but we will report that as a grammar check in the checker.
        // parse leading decorators
        let pos = self.node_pos();
        let start = self.node_stack.len();
        loop {
            if allow_decorators && self.token == Kind::AtToken && !has_trailing_modifier {
                let decorator = self.parse_decorator();
                self.node_stack.push(decorator);
                if has_leading_modifier {
                    has_trailing_decorator = true;
                }
            } else {
                let Some(modifier) = self.try_parse_modifier(
                    has_static_modifier,
                    permit_const_as_modifier,
                    stop_on_start_of_class_static_block,
                ) else {
                    break;
                };
                if modifier.kind() == Kind::StaticKeyword {
                    has_static_modifier = true;
                }
                self.node_stack.push(modifier);
                if has_trailing_decorator {
                    has_trailing_modifier = true;
                } else {
                    has_leading_modifier = true;
                }
            }
        }
        if self.node_stack.len() > start {
            let end = self.node_pos();
            let list = self.new_modifier_list(TextRange::new(pos, end), &self.node_stack[start..]);
            self.node_stack.truncate(start);
            return Some(list);
        }
        None
    }

    fn parse_decorator(&mut self) -> P<Node> {
        let pos = self.node_pos();
        self.parse_expected(Kind::AtToken);
        let expression = self.do_in_context(
            NodeFlags::DecoratorContext,
            true,
            Parser::parse_decorator_expression,
        );
        let node = self.factory.new_decorator(expression);
        self.finish_node(node, pos)
    }

    fn parse_decorator_expression(&mut self) -> P<Node> {
        if self.in_await_context() && self.token == Kind::AwaitKeyword {
            // `@await` is disallowed in an [Await] context, but can cause parsing to go off the rails
            // This simply parses the missing identifier and moves on.
            let pos = self.node_pos();
            let await_expression = self
                .parse_identifier_with_diagnostic(Some(&diagnostics::EXPRESSION_EXPECTED), None);
            self.next_token();
            let member_expression = self.parse_member_expression_rest(
                pos,
                await_expression,
                true, /*allowOptionalChain*/
            );
            return self.parse_call_expression_rest(pos, member_expression);
        }
        self.parse_left_hand_side_expression_or_higher()
    }

    fn try_parse_modifier(
        &mut self,
        has_seen_static_modifier: bool,
        permit_const_as_modifier: bool,
        stop_on_start_of_class_static_block: bool,
    ) -> Option<P<Node>> {
        let pos = self.node_pos();
        let kind = self.token;
        if self.token == Kind::ConstKeyword && permit_const_as_modifier {
            // We need to ensure that any subsequent modifiers appear on the same line
            // so that when 'const' is a standalone declaration, we don't issue an error.
            if !self.look_ahead(Parser::next_token_is_on_same_line_and_can_follow_modifier) {
                return None;
            }
            self.next_token();
        } else if (stop_on_start_of_class_static_block
            && self.token == Kind::StaticKeyword
            && self.look_ahead(Parser::next_token_is_open_brace))
            || (has_seen_static_modifier && self.token == Kind::StaticKeyword)
            || (!self.parse_any_contextual_modifier())
        {
            return None;
        }
        let node = self.factory.new_modifier(kind);
        Some(self.finish_node(node, pos))
    }

    #[inline]
    pub(crate) fn parse_contextual_modifier(&mut self, t: Kind) -> bool {
        // A nonmatching token would only mark and rewind unchanged parser state.
        if self.token != t {
            return false;
        }
        self.parse_contextual_modifier_worker()
    }

    #[inline(never)]
    fn parse_contextual_modifier_worker(&mut self) -> bool {
        let state = self.mark();
        if self.next_token_can_follow_modifier() {
            return true;
        }
        self.rewind(state);
        false
    }

    fn parse_any_contextual_modifier(&mut self) -> bool {
        // Not a modifier: Go marks and rewinds an unchanged state (a no-op) before returning false.
        if !ast::is_modifier_kind(self.token) {
            return false;
        }
        let state = self.mark();
        if self.next_token_can_follow_modifier() {
            return true;
        }
        self.rewind(state);
        false
    }

    fn next_token_can_follow_modifier(&mut self) -> bool {
        match self.token {
            Kind::ConstKeyword => {
                // 'const' is only a modifier if followed by 'enum'.
                self.next_token() == Kind::EnumKeyword
            }
            Kind::ExportKeyword => {
                self.next_token();
                if self.token == Kind::DefaultKeyword {
                    return self.look_ahead(Parser::next_token_can_follow_default_keyword);
                }
                if self.token == Kind::TypeKeyword {
                    return self.look_ahead(Parser::next_token_can_follow_export_modifier);
                }
                self.can_follow_export_modifier()
            }
            Kind::DefaultKeyword => self.next_token_can_follow_default_keyword(),
            Kind::StaticKeyword => {
                self.next_token();
                self.can_follow_modifier()
            }
            Kind::GetKeyword | Kind::SetKeyword => {
                self.next_token();
                self.can_follow_get_or_set_keyword()
            }
            _ => self.next_token_is_on_same_line_and_can_follow_modifier(),
        }
    }

    fn next_token_can_follow_default_keyword(&mut self) -> bool {
        match self.next_token() {
            Kind::ClassKeyword | Kind::FunctionKeyword | Kind::InterfaceKeyword | Kind::AtToken => {
                true
            }
            Kind::AbstractKeyword => {
                self.look_ahead(Parser::next_token_is_class_keyword_on_same_line)
            }
            Kind::AsyncKeyword => {
                self.look_ahead(Parser::next_token_is_function_keyword_on_same_line)
            }
            _ => false,
        }
    }

    pub(crate) fn next_token_is_identifier_or_keyword(&mut self) -> bool {
        token_is_identifier_or_keyword(self.next_token())
    }

    pub(crate) fn next_token_is_identifier_or_keyword_or_greater_than(&mut self) -> bool {
        token_is_identifier_or_keyword_or_greater_than(self.next_token())
    }

    pub(crate) fn next_token_is_identifier_or_keyword_on_same_line(&mut self) -> bool {
        self.next_token_is_identifier_or_keyword() && !self.has_preceding_line_break()
    }

    pub(crate) fn next_token_is_identifier_or_keyword_or_literal_on_same_line(&mut self) -> bool {
        (self.next_token_is_identifier_or_keyword()
            || self.token == Kind::NumericLiteral
            || self.token == Kind::BigIntLiteral
            || self.token == Kind::StringLiteral)
            && !self.has_preceding_line_break()
    }

    fn next_token_is_class_keyword_on_same_line(&mut self) -> bool {
        self.next_token() == Kind::ClassKeyword && !self.has_preceding_line_break()
    }

    pub(crate) fn next_token_is_function_keyword_on_same_line(&mut self) -> bool {
        self.next_token() == Kind::FunctionKeyword && !self.has_preceding_line_break()
    }

    fn next_token_can_follow_export_modifier(&mut self) -> bool {
        self.next_token();
        self.can_follow_export_modifier()
    }

    fn can_follow_export_modifier(&self) -> bool {
        self.token == Kind::AtToken
            || self.token != Kind::AsteriskToken
                && self.token != Kind::AsKeyword
                && self.token != Kind::OpenBraceToken
                && self.can_follow_modifier()
    }

    fn can_follow_modifier(&self) -> bool {
        self.token == Kind::OpenBracketToken
            || self.token == Kind::OpenBraceToken
            || self.token == Kind::AsteriskToken
            || self.token == Kind::DotDotDotToken
            || self.is_literal_property_name()
    }

    fn can_follow_get_or_set_keyword(&self) -> bool {
        self.token == Kind::OpenBracketToken || self.is_literal_property_name()
    }

    fn next_token_is_on_same_line_and_can_follow_modifier(&mut self) -> bool {
        self.next_token();
        if self.has_preceding_line_break() {
            return false;
        }
        self.can_follow_modifier()
    }

    pub(crate) fn next_token_is_open_brace(&mut self) -> bool {
        self.next_token() == Kind::OpenBraceToken
    }

    pub(crate) fn parse_expression(&mut self) -> P<Node> {
        // Expression[in]:
        //      AssignmentExpression[in]
        //      Expression[in] , AssignmentExpression[in]

        // clear the decorator context when parsing Expression, as it should be unambiguous when parsing a decorator
        let save_context_flags = self.context_flags;
        self.context_flags &= !NodeFlags::DecoratorContext;
        let pos = self.node_pos();
        let mut expr = self.parse_assignment_expression_or_higher();
        while let Some(operator_token) = self.parse_optional_token(Kind::CommaToken) {
            let right = self.parse_assignment_expression_or_higher();
            expr = self.make_binary_expression(expr, operator_token, right, pos);
        }
        self.context_flags = save_context_flags;
        expr
    }

    pub(crate) fn parse_expression_allow_in(&mut self) -> P<Node> {
        self.do_in_context(NodeFlags::DisallowInContext, false, Parser::parse_expression)
    }
}
