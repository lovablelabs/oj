use tsrs_ast::*;
use tsrs_core::*;
use tsrs_scanner as scanner;

use crate::*;

//
// Expressions
//

impl Printer {
    fn emit_keyword_expression(&mut self, node: P<Node>) {
        self.emit_keyword_node(Some(node));
    }

    fn emit_array_literal_expression_element(&mut self, node: P<Node>) {
        self.emit_expression(node, OperatorPrecedence::Spread);
    }

    fn emit_array_literal_expression(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_array_literal_expression();
        self.emit_list(
            Printer::emit_array_literal_expression_element,
            node,
            Some(n.elements()),
            ListFormat::ArrayLiteralExpressionElements
                | if n.multi_line { ListFormat::PreferNewLine } else { ListFormat::None },
        );
        self.exit_node(node, state);
    }

    fn emit_object_literal_expression(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_object_literal_expression();
        let indented = self.should_emit_indented(node);
        self.increase_indent_if(indented);
        self.push_name_generation_scope(Some(node));
        self.generate_all_member_names(Some(n.properties()));
        let allow_trailing_comma = self.should_allow_trailing_comma(node, Some(n.properties()));
        self.emit_list(
            Printer::emit_object_literal_element,
            node,
            Some(n.properties()),
            ListFormat::ObjectLiteralExpressionProperties
                | if n.multi_line { ListFormat::PreferNewLine } else { ListFormat::None }
                | if allow_trailing_comma {
                    ListFormat::AllowTrailingComma
                } else {
                    ListFormat::None
                },
        );
        self.pop_name_generation_scope(Some(node));
        self.decrease_indent_if(indented);
        self.exit_node(node, state);
    }

    // 1..toString is a valid property access, emit a dot after the literal
    // Also emit a dot if expression is a integer const enum value - it will appear in generated code as numeric literal
    fn may_need_dot_dot_for_property_access(&mut self, expression: P<Node>) -> bool {
        let expression = skip_partially_emitted_expressions(expression);
        if is_numeric_literal(expression) {
            // check if numeric literal is a decimal literal that was originally written with a dot
            let text = self.get_literal_text_of_node(
                expression,
                None, /*sourceFile*/
                getLiteralTextFlags::NeverAsciiEscape,
            );
            // If the number will be printed verbatim and it doesn't already contain a dot or an exponent indicator, add one
            // if the expression doesn't have any comments that will be emitted.
            return !expression
                .as_numeric_literal()
                .token_flags()
                .intersects(TokenFlags::WithSpecifier)
                && !text.contains(scanner::token_to_string(Kind::DotToken))
                && !text.contains('E')
                && !text.contains('e');
        }
        false
    }

    pub(crate) fn emit_property_access_expression(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_property_access_expression();
        self.emit_expression(
            n.expression(),
            if is_optional_chain(node) {
                OperatorPrecedence::OptionalChain
            } else {
                OperatorPrecedence::Member
            },
        );
        let token = if let Some(token) = n.question_dot_token() {
            token
        } else {
            let token = self.emit_context.factory.new_token(Kind::DotToken);
            token.set_loc(TextRange::from_text_pos(n.expression().end(), n.name().pos()));
            self.emit_context.add_emit_flags(token, EmitFlags::NoSourceMap);
            token
        };
        let lines_before_dot = self.get_lines_between_nodes(node, n.expression(), token);
        self.write_line_repeat(lines_before_dot);
        self.increase_indent_if(lines_before_dot > 0);
        let should_emit_dot_dot = token.kind() != Kind::QuestionDotToken
            && self.may_need_dot_dot_for_property_access(n.expression())
            && !self.writer().has_trailing_comment()
            && !self.writer().has_trailing_whitespace();
        if should_emit_dot_dot {
            self.write_punctuation(".");
        }
        if n.question_dot_token().is_some() {
            self.emit_token_node(Some(token));
        } else {
            self.emit_token(Kind::DotToken, n.expression().end(), WriteKind::Punctuation, node);
        }
        let lines_after_dot = self.get_lines_between_nodes(node, token, n.name());
        self.write_line_repeat(lines_after_dot);
        self.increase_indent_if(lines_after_dot > 0);
        self.emit_member_name(Some(n.name()));
        self.decrease_indent_if(lines_after_dot > 0);
        self.decrease_indent_if(lines_before_dot > 0);
        self.exit_node(node, state);
    }

    fn emit_element_access_expression(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_element_access_expression();
        self.emit_expression(
            n.expression(),
            if is_optional_chain(node) {
                OperatorPrecedence::OptionalChain
            } else {
                OperatorPrecedence::Member
            },
        );
        self.emit_token_node(n.question_dot_token());
        self.emit_token(
            Kind::OpenBracketToken,
            greatest_end(TextPos::Synthetic, &[&n.expression(), &n.question_dot_token()]),
            WriteKind::Punctuation,
            node,
        );
        self.emit_expression(n.argument_expression(), OperatorPrecedence::Comma);
        self.emit_token(
            Kind::CloseBracketToken,
            n.argument_expression().end(),
            WriteKind::Punctuation,
            node,
        );
        self.exit_node(node, state);
    }

    fn emit_argument(&mut self, node: P<Node>) {
        self.emit_expression(node, OperatorPrecedence::Spread);
    }

    fn emit_callee(&mut self, callee: P<Node>, parent_node: P<Node>) {
        if self.should_emit_indirect_call(parent_node) {
            self.write_punctuation("(");
            self.write_literal("0");
            self.write_punctuation(",");
            self.write_space();
            self.emit_expression(callee, OperatorPrecedence::Comma);
            self.write_punctuation(")");
        } else if parent_node.kind() == Kind::CallExpression
            && is_new_expression_without_arguments(skip_partially_emitted_expressions(callee))
        {
            // Parenthesize `new C` inside of a CallExpression so it is treated as `(new C)()` and not `new C()`
            self.emit_expression(callee, OperatorPrecedence::Parentheses);
        } else {
            self.emit_expression(
                callee,
                if is_optional_chain(parent_node) {
                    OperatorPrecedence::OptionalChain
                } else {
                    OperatorPrecedence::Member
                },
            );
        }
    }

    fn emit_call_expression(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_call_expression();
        self.emit_callee(n.expression(), node);
        self.emit_token_node(n.question_dot_token());
        self.emit_type_arguments(node, n.type_arguments());
        self.emit_list(
            Printer::emit_argument,
            node,
            Some(n.arguments()),
            ListFormat::CallExpressionArguments,
        );
        self.exit_node(node, state);
    }

    fn emit_new_expression(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_new_expression();
        self.emit_token(Kind::NewKeyword, node.pos(), WriteKind::Keyword, node);
        self.write_space();
        if skip_partially_emitted_expressions(n.expression()).kind() == Kind::CallExpression {
            // Parenthesize `C()` inside of a NewExpression so it is treated as `new (C())` and not `new C()`
            self.emit_expression(n.expression(), OperatorPrecedence::Parentheses);
        } else {
            self.emit_expression(n.expression(), OperatorPrecedence::Member);
        }
        self.emit_type_arguments(node, n.type_arguments());
        self.emit_list(
            Printer::emit_argument,
            node,
            n.arguments(),
            ListFormat::NewExpressionArguments,
        );
        self.exit_node(node, state);
    }

    fn emit_template_literal(&mut self, node: P<Node>) {
        match node.kind() {
            Kind::NoSubstitutionTemplateLiteral => self.emit_no_substitution_template_literal(node),
            Kind::TemplateExpression => self.emit_template_expression(node),
            _ => panic!("unhandled TemplateLiteral: {:?}", node.kind()),
        }
    }

    fn emit_tagged_template_expression(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_tagged_template_expression();
        self.emit_callee(n.tag(), node);
        self.emit_type_arguments(node, n.type_arguments());
        self.write_space();
        self.emit_template_literal(n.template());
        self.exit_node(node, state);
    }

    fn emit_type_assertion_expression(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_type_assertion();
        self.write_punctuation("<");
        self.emit_type_node_outside_extends(n.type_());
        self.write_punctuation(">");
        self.emit_expression(n.expression(), OperatorPrecedence::Update);
        self.exit_node(node, state);
    }

    fn emit_parenthesized_expression(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let expression = node.as_parenthesized_expression().expression();
        let _open_paren_pos =
            self.emit_token(Kind::OpenParenToken, node.pos(), WriteKind::Punctuation, node);
        let indented = self.write_line_separators_and_indent_before(expression, node);
        self.emit_expression(expression, OperatorPrecedence::Comma);
        self.write_line_separators_after(expression, node);
        self.decrease_indent_if(indented);
        // Go falls back to openParenPos when Expression is nil; it never is here.
        let close_paren_pos = expression.end();
        self.emit_token(Kind::CloseParenToken, close_paren_pos, WriteKind::Punctuation, node);
        self.exit_node(node, state);
    }

    fn emit_function_expression(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_function_expression();
        self.generate_name_if_needed(n.name());
        self.emit_modifier_list(node, node.modifiers(), false /*allowDecorators*/);
        self.write_keyword("function");
        self.emit_token_node(n.asterisk_token());
        self.write_space();
        self.emit_identifier_name_node(n.name());
        let indented = self.should_emit_indented(node);
        self.increase_indent_if(indented);
        self.push_name_generation_scope(Some(node));
        self.emit_signature(node);
        self.emit_function_body_node(n.body());
        self.pop_name_generation_scope(Some(node));
        self.decrease_indent_if(indented);
        self.exit_node(node, state);
    }

    fn emit_concise_body(&mut self, node: P<Node>) {
        if is_block(node) {
            self.emit_function_body(node);
        } else if is_object_literal_expression(get_leftmost_expression(
            node, false, /*stopAtCallExpressions*/
        )) {
            // Wrap in ParenthesizedExpression to ensure parens are emitted after any leading
            // PartiallyEmittedExpression comments, matching TypeScript's factory-time wrapping
            // via parenthesizeConciseBodyOfArrowFunction.
            let paren = self.emit_context.factory.new_parenthesized_expression(node);
            paren.set_loc(node.loc());
            self.emit_expression(paren, OperatorPrecedence::LOWEST);
        } else if is_expression(node) {
            self.emit_expression(node, OperatorPrecedence::Yield);
        } else {
            panic!("unexpected ConciseBody: {:?}", node.kind());
        }
    }

    fn emit_arrow_function(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_arrow_function();
        self.emit_modifier_list(node, node.modifiers(), false /*allowDecorators*/);
        let indented = self.should_emit_indented(node);
        self.increase_indent_if(indented);
        self.push_name_generation_scope(Some(node));
        self.emit_type_parameters(node, n.type_parameters());
        self.emit_parameters_for_arrow(node, n.parameters());
        self.emit_type_annotation(n.type_());
        self.write_space();
        self.emit_token_node(n.equals_greater_than_token());
        self.write_space();
        self.emit_concise_body(n.body().unwrap());
        self.pop_name_generation_scope(Some(node));
        self.decrease_indent_if(indented);
        self.exit_node(node, state);
    }

    fn emit_delete_expression(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_token(Kind::DeleteKeyword, node.pos(), WriteKind::Keyword, node);
        self.write_space();
        self.emit_expression(node.as_delete_expression().expression(), OperatorPrecedence::Unary);
        self.exit_node(node, state);
    }

    fn emit_type_of_expression(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_token(Kind::TypeOfKeyword, node.pos(), WriteKind::Keyword, node);
        self.write_space();
        self.emit_expression(node.as_type_of_expression().expression(), OperatorPrecedence::Unary);
        self.exit_node(node, state);
    }

    fn emit_void_expression(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_token(Kind::VoidKeyword, node.pos(), WriteKind::Keyword, node);
        self.write_space();
        self.emit_expression(node.as_void_expression().expression(), OperatorPrecedence::Unary);
        self.exit_node(node, state);
    }

    fn emit_await_expression(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_token(Kind::AwaitKeyword, node.pos(), WriteKind::Keyword, node);
        self.write_space();
        self.emit_expression(node.as_await_expression().expression(), OperatorPrecedence::Unary);
        self.exit_node(node, state);
    }

    fn emit_prefix_unary_expression(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_prefix_unary_expression();
        let operator = n.operator;
        let operand = n.operand();
        self.emit_token(operator, node.pos(), WriteKind::Operator, node);

        // In some cases, we need to emit a space between the operator and the operand. One obvious case
        // is when the operator is an identifier, like delete or typeof. We also need to do this for plus
        // and minus expressions in certain cases. Specifically, consider the following two cases (parens
        // are just for clarity of exposition, and not part of the source code):
        //
        //  (+(+1))
        //  (+(++1))
        //
        // We need to emit a space in both cases. In the first case, the absence of a space will make
        // the resulting expression a prefix increment operation. And in the second, it will make the resulting
        // expression a prefix increment whose operand is a plus expression - (++(+x))
        // The same is true of minus of course.
        if operand.kind() == Kind::PrefixUnaryExpression {
            let inner = operand.as_prefix_unary_expression().operator;
            if (operator == Kind::PlusToken
                && (inner == Kind::PlusToken || inner == Kind::PlusPlusToken))
                || (operator == Kind::MinusToken
                    && (inner == Kind::MinusToken || inner == Kind::MinusMinusToken))
            {
                self.write_space();
            }
        }

        self.emit_expression(operand, OperatorPrecedence::Unary);
        self.exit_node(node, state);
    }

    fn emit_postfix_unary_expression(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_postfix_unary_expression();
        self.emit_expression(n.operand(), OperatorPrecedence::LeftHandSide);
        self.emit_token(n.operator, n.operand().end(), WriteKind::Operator, node);
        self.exit_node(node, state);
    }

    // This function determines whether an expression consists of a homogeneous set of
    // literal expressions or binary plus expressions that all share the same literal kind.
    // It is used to determine whether the right-hand operand of a binary plus expression can be
    // emitted without parentheses.
    fn get_literal_kind_of_binary_plus_operand(node: P<Node>) -> Kind {
        let node = skip_partially_emitted_expressions(node);

        if is_literal_kind(node.kind()) {
            return node.kind();
        }

        if node.kind() == Kind::BinaryExpression {
            let n = node.as_binary_expression();
            if n.operator_token().kind() == Kind::PlusToken {
                // !!! Determine if caching this is worthwhile over recomputing
                ////if n.cachedLiteralKind != KindUnknown {
                ////	return n.cachedLiteralKind;
                ////}

                let left_kind = Self::get_literal_kind_of_binary_plus_operand(n.left());
                let mut literal_kind = Kind::Unknown;
                if is_literal_kind(left_kind)
                    && left_kind == Self::get_literal_kind_of_binary_plus_operand(n.right())
                {
                    literal_kind = left_kind;
                }

                ////n.cachedLiteralKind = literalKind;
                return literal_kind;
            }
        }

        Kind::Unknown
    }

    fn get_binary_expression_precedence(node: P<Node>) -> (OperatorPrecedence, OperatorPrecedence) {
        let n = node.as_binary_expression();
        let precedence = get_expression_precedence(node);
        let mut left_prec = precedence;
        let mut right_prec = precedence;
        match precedence {
            OperatorPrecedence::Assignment => {
                // assignment is right-associative
                left_prec = OperatorPrecedence::Conditional;
                right_prec = OperatorPrecedence::Yield;
            }
            OperatorPrecedence::LogicalOR => {
                right_prec = OperatorPrecedence::LogicalAND;
            }
            OperatorPrecedence::LogicalAND => {
                right_prec = OperatorPrecedence::BitwiseOR;
            }
            OperatorPrecedence::Comma
            | OperatorPrecedence::BitwiseOR
            | OperatorPrecedence::BitwiseXOR
            | OperatorPrecedence::BitwiseAND => {
                // No need to parenthesize the right operand when the binary operator and
                // operand are both comma, |, ^, or & due to associativity:
                //  x,(a,b)     => x,a,b
                //  x|(a|b)     => x|a|b
                //  x^(a^b)     => x^a^b
                //  x&(a&b)     => x&a&b
            }
            OperatorPrecedence::Equality => {
                right_prec = OperatorPrecedence::Relational;
            }
            OperatorPrecedence::Relational => {
                right_prec = OperatorPrecedence::Shift;
            }
            OperatorPrecedence::Shift => {
                right_prec = OperatorPrecedence::Additive;
            }
            OperatorPrecedence::Additive => 'case: {
                if n.operator_token().kind() == Kind::PlusToken
                    && is_binary_operation(n.right(), Kind::PlusToken)
                {
                    let left_kind = Self::get_literal_kind_of_binary_plus_operand(n.left());
                    if is_literal_kind(left_kind)
                        && left_kind == Self::get_literal_kind_of_binary_plus_operand(n.right())
                    {
                        // No need to parenthesize the right operand when the binary operator
                        // is plus (+) if both the left and right operands consist solely of either
                        // literals of the same kind or binary plus (+) expressions for literals of
                        // the same kind (recursively).
                        //  "a"+(1+2)       => "a"+(1+2)
                        //  "a"+("b"+"c")   => "a"+"b"+"c"
                        break 'case;
                    }
                }
                right_prec = OperatorPrecedence::Multiplicative;
            }
            OperatorPrecedence::Multiplicative => 'case: {
                if n.operator_token().kind() == Kind::AsteriskToken
                    && is_binary_operation(n.right(), Kind::AsteriskToken)
                {
                    // No need to parenthesize the right operand when the binary operator and
                    // operand are both * due to the associative property of mathematics:
                    //  x*(a*b)     => x*a*b
                    break 'case;
                }
                right_prec = OperatorPrecedence::Exponentiation;
            }
            OperatorPrecedence::Exponentiation => {
                // exponentiation is right-associative
                left_prec = OperatorPrecedence::Update;
            }
            _ => panic!("unhandled precedence: {precedence:?}"),
        }
        (left_prec, right_prec)
    }

    fn emit_binary_expression(&mut self, node: P<Node>) {
        let n = node.as_binary_expression();
        let (mut left_prec, mut right_prec) = Self::get_binary_expression_precedence(node);
        let emitted_left = skip_partially_emitted_expressions(n.left());
        if node_is_synthesized(emitted_left)
            && emitted_left.kind() == Kind::BinaryExpression
            && mixing_binary_operators_requires_parentheses(
                n.operator_token().kind(),
                emitted_left.as_binary_expression().operator_token().kind(),
            )
        {
            left_prec = OperatorPrecedence::HIGHEST;
        }
        let emitted_right = skip_partially_emitted_expressions(n.right());
        if node_is_synthesized(emitted_right)
            && emitted_right.kind() == Kind::BinaryExpression
            && mixing_binary_operators_requires_parentheses(
                n.operator_token().kind(),
                emitted_right.as_binary_expression().operator_token().kind(),
            )
        {
            right_prec = OperatorPrecedence::HIGHEST;
        }
        let state = self.enter_node(node);
        self.emit_expression(n.left(), left_prec);
        let lines_before_operator =
            self.get_lines_between_nodes(node, n.left(), n.operator_token());
        let lines_after_operator =
            self.get_lines_between_nodes(node, n.operator_token(), n.right());
        self.write_lines_and_indent(
            lines_before_operator,
            n.operator_token().kind() != Kind::CommaToken, /*writeSpaceIfNotIndenting*/
        );
        self.emit_token_node_ex(Some(n.operator_token()), tokenEmitFlags::NoSourceMaps);
        self.write_lines_and_indent(lines_after_operator, true /*writeSpaceIfNotIndenting*/); // Binary operators should have a space before the comment starts
        self.emit_expression(n.right(), right_prec);
        self.decrease_indent_if(lines_after_operator > 0);
        self.decrease_indent_if(lines_before_operator > 0);
        self.exit_node(node, state);
    }

    fn emit_short_circuit_expression(&mut self, node: P<Node>) {
        if is_binary_operation(
            skip_partially_emitted_expressions(node),
            Kind::QuestionQuestionToken,
        ) {
            self.emit_expression(node, OperatorPrecedence::COALESCE);
        } else {
            self.emit_expression(node, OperatorPrecedence::LogicalOR);
        }
    }

    fn emit_conditional_expression(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_conditional_expression();
        let lines_before_question =
            self.get_lines_between_nodes(node, n.condition(), n.question_token());
        let lines_after_question =
            self.get_lines_between_nodes(node, n.question_token(), n.when_true());
        let lines_before_colon = self.get_lines_between_nodes(node, n.when_true(), n.colon_token());
        let lines_after_colon = self.get_lines_between_nodes(node, n.colon_token(), n.when_false());
        self.emit_short_circuit_expression(n.condition());
        self.write_lines_and_indent(lines_before_question, true /*writeSpaceIfNotIndenting*/);
        self.emit_punctuation_node(Some(n.question_token()));
        self.write_lines_and_indent(lines_after_question, true /*writeSpaceIfNotIndenting*/);
        self.emit_expression(n.when_true(), OperatorPrecedence::Yield);
        self.decrease_indent_if(lines_after_question > 0);
        self.decrease_indent_if(lines_before_question > 0);
        self.write_lines_and_indent(lines_before_colon, true /*writeSpaceIfNotIndenting*/);
        self.emit_punctuation_node(Some(n.colon_token()));
        self.write_lines_and_indent(lines_after_colon, true /*writeSpaceIfNotIndenting*/);
        self.emit_expression(n.when_false(), OperatorPrecedence::Yield);
        self.decrease_indent_if(lines_after_colon > 0);
        self.decrease_indent_if(lines_before_colon > 0);
        self.exit_node(node, state);
    }

    fn emit_template_expression(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_template_expression();
        self.emit_template_head(n.head());
        self.emit_list(
            Printer::emit_template_span_node,
            node,
            Some(n.template_spans()),
            ListFormat::TemplateExpressionSpans,
        );
        self.exit_node(node, state);
    }

    fn emit_yield_expression(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_yield_expression();
        self.emit_token(Kind::YieldKeyword, node.pos(), WriteKind::Keyword, node);
        self.emit_punctuation_node(n.asterisk_token());
        if let Some(expression) = n.expression() {
            self.write_space();
            self.emit_expression_no_asi(expression, OperatorPrecedence::DISALLOW_COMMA);
        }
        self.exit_node(node, state);
    }

    fn emit_spread_element(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_token(Kind::DotDotDotToken, node.pos(), WriteKind::Punctuation, node);
        self.emit_expression(
            node.as_spread_element().expression(),
            OperatorPrecedence::DISALLOW_COMMA,
        );
        self.exit_node(node, state);
    }

    fn emit_class_expression(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_class_expression();
        self.generate_name_if_needed(n.name());

        let pos = self.emit_modifier_list(node, node.modifiers(), true /*allowDecorators*/);
        self.emit_token(Kind::ClassKeyword, pos, WriteKind::Keyword, node);

        if let Some(name) = n.name() {
            self.write_space();
            self.emit_identifier_name(name);
        }

        let indented = self.should_emit_indented(node);
        self.increase_indent_if(indented);

        self.emit_type_parameters(node, n.type_parameters());
        self.emit_list(
            Printer::emit_heritage_clause_node,
            node,
            n.heritage_clauses(),
            ListFormat::ClassHeritageClauses,
        );
        self.write_space();
        self.write_punctuation("{");
        self.push_name_generation_scope(Some(node));
        self.generate_all_member_names(Some(n.members()));
        self.emit_list(
            Printer::emit_class_element,
            node,
            Some(n.members()),
            ListFormat::ClassMembers,
        );
        self.pop_name_generation_scope(Some(node));
        self.write_punctuation("}");

        self.decrease_indent_if(indented);
        self.exit_node(node, state);
    }

    fn emit_omitted_expression(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.exit_node(node, state);
    }

    pub(crate) fn emit_expression_with_type_arguments(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_expression_with_type_arguments();
        self.emit_expression(n.expression(), OperatorPrecedence::Member);
        self.emit_type_arguments(node, n.type_arguments());
        self.exit_node(node, state);
    }

    fn emit_as_expression(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_as_expression();
        self.emit_expression(n.expression(), OperatorPrecedence::Relational);
        self.write_space();
        self.write_keyword("as");
        self.write_space();
        self.emit_type_node_outside_extends(n.type_());
        self.exit_node(node, state);
    }

    fn emit_satisfies_expression(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_satisfies_expression();
        self.emit_expression(n.expression(), OperatorPrecedence::Relational);
        self.write_space();
        self.write_keyword("satisfies");
        self.write_space();
        self.emit_type_node_outside_extends(n.type_());
        self.exit_node(node, state);
    }

    fn emit_non_null_expression(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_expression(
            node.as_non_null_expression().expression(),
            OperatorPrecedence::Member,
        );
        self.write_operator("!");
        self.exit_node(node, state);
    }

    fn emit_meta_property(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_meta_property();
        let pos = self.emit_token(n.keyword_token, node.pos(), WriteKind::Punctuation, node);
        self.emit_token(Kind::DotToken, pos, WriteKind::Punctuation, node);
        self.emit_identifier_name(n.name());
        self.exit_node(node, state);
    }

    fn emit_partially_emitted_expression(&mut self, node: P<Node>) {
        // avoid reprinting parens for nested partially emitted expressions
        struct Entry {
            node: P<Node>,
            state: PrinterState,
        }
        let mut node = node;
        let mut stack: Vec<Entry> = Vec::new();
        loop {
            let state = self.enter_node(node);
            let emit_flags = self.emit_context.emit_flags(node);
            let expression = node.as_partially_emitted_expression().expression();
            if !emit_flags.intersects(EmitFlags::NoLeadingComments)
                && node.pos() != expression.pos()
            {
                self.emit_trailing_comments_of_position(
                    expression.pos(),
                    false, /*prefixSpace*/
                    false, /*forceNoNewline*/
                );
            }
            stack.push(Entry { node, state });
            if !is_partially_emitted_expression(expression) {
                break;
            }
            node = expression;
        }

        self.emit_expression(
            node.as_partially_emitted_expression().expression(),
            OperatorPrecedence::LOWEST,
        );

        // unwind stack
        while let Some(entry) = stack.pop() {
            let emit_flags = self.emit_context.emit_flags(node);
            let expression = node.as_partially_emitted_expression().expression();
            if !emit_flags.intersects(EmitFlags::NoTrailingComments)
                && node.end() != expression.end()
            {
                self.emit_leading_comments_of_position(expression.end());
            }
            self.exit_node(node, entry.state);
            node = entry.node;
        }
    }

    fn comment_will_emit_new_line(comment: CommentRange) -> bool {
        comment.kind == Kind::SingleLineCommentTrivia || comment.has_trailing_new_line
    }

    fn synthetic_comment_will_emit_new_line(comment: &SynthesizedComment) -> bool {
        comment.kind == Kind::SingleLineCommentTrivia || comment.has_trailing_new_line
    }

    fn will_emit_leading_new_line(&self, node: P<Node>) -> bool {
        let Some(current_source_file) = self.current_source_file() else {
            return false;
        };
        let mut has_leading_comment_ranges = false;
        let mut has_new_line_comment = false;
        // A synthesized node has no comment ranges: Go's scanner yields none for its position (-1).
        let text = current_source_file.text();
        for comment in node
            .pos()
            .source_offset()
            .into_iter()
            .flat_map(|pos| scanner::get_leading_comment_ranges(text, pos))
        {
            has_leading_comment_ranges = true;
            if Self::comment_will_emit_new_line(comment) {
                has_new_line_comment = true;
            }
        }
        if has_leading_comment_ranges {
            let parse_node = self.emit_context.parse_node(Some(node));
            if let Some(parse_node) = parse_node
                && parse_node.parent().is_some_and(is_parenthesized_expression)
            {
                return true;
            }
        }
        if has_new_line_comment {
            return true;
        }
        if self
            .emit_context
            .get_synthetic_leading_comments(node)
            .iter()
            .any(Self::synthetic_comment_will_emit_new_line)
        {
            return true;
        }
        if is_partially_emitted_expression(node) {
            let pee = node.as_partially_emitted_expression();
            if node.pos() != pee.expression().pos()
                && pee
                    .expression()
                    .pos()
                    .source_offset()
                    .into_iter()
                    .flat_map(|pos| scanner::get_trailing_comment_ranges(text, pos))
                    .any(Self::comment_will_emit_new_line)
            {
                return true;
            }
            return self.will_emit_leading_new_line(pee.expression());
        }
        false
    }

    // parenthesizeExpressionForNoAsi wraps an expression in parens if we would emit a leading comment
    // that would introduce a line separator between the node and its parent.
    fn parenthesize_expression_for_no_asi(&mut self, node: P<Node>) -> P<Node> {
        if !self.comments_disabled {
            match node.kind() {
                Kind::PartiallyEmittedExpression => {
                    if self.will_emit_leading_new_line(node) {
                        let pee = node.as_partially_emitted_expression();
                        let parse_node = self.emit_context.parse_node(Some(node));
                        if let Some(parse_node) = parse_node
                            && is_parenthesized_expression(parse_node)
                        {
                            // If the original node was a parenthesized expression, restore it to preserve comment and source map emit
                            let parens = self
                                .emit_context
                                .factory
                                .new_parenthesized_expression(pee.expression());
                            self.emit_context.set_original(parens, node);
                            parens.set_loc(parse_node.loc());
                            return parens;
                        }
                        return self.emit_context.factory.new_parenthesized_expression(node);
                    }
                    let pee = node.as_partially_emitted_expression();
                    let expression = self.parenthesize_expression_for_no_asi(pee.expression());
                    return self
                        .emit_context
                        .factory
                        .update_partially_emitted_expression(node, expression);
                }
                Kind::PropertyAccessExpression => {
                    let pae = node.as_property_access_expression();
                    let expression = self.parenthesize_expression_for_no_asi(pae.expression());
                    return self.emit_context.factory.update_property_access_expression(
                        node,
                        expression,
                        pae.question_dot_token(),
                        pae.name(),
                        node.flags(),
                    );
                }
                Kind::ElementAccessExpression => {
                    let eae = node.as_element_access_expression();
                    let expression = self.parenthesize_expression_for_no_asi(eae.expression());
                    return self.emit_context.factory.update_element_access_expression(
                        node,
                        expression,
                        eae.question_dot_token(),
                        eae.argument_expression(),
                        node.flags(),
                    );
                }
                Kind::CallExpression => {
                    let ce = node.as_call_expression();
                    let expression = self.parenthesize_expression_for_no_asi(ce.expression());
                    return self.emit_context.factory.update_call_expression(
                        node,
                        expression,
                        ce.question_dot_token(),
                        ce.type_arguments(),
                        ce.arguments(),
                        node.flags(),
                    );
                }
                Kind::TaggedTemplateExpression => {
                    let tte = node.as_tagged_template_expression();
                    let tag = self.parenthesize_expression_for_no_asi(tte.tag());
                    return self.emit_context.factory.update_tagged_template_expression(
                        node,
                        tag,
                        tte.question_dot_token(),
                        tte.type_arguments(),
                        tte.template(),
                        node.flags(),
                    );
                }
                Kind::PostfixUnaryExpression => {
                    let pue = node.as_postfix_unary_expression();
                    let operand = self.parenthesize_expression_for_no_asi(pue.operand());
                    return self.emit_context.factory.update_postfix_unary_expression(
                        node,
                        operand,
                        pue.operator,
                    );
                }
                Kind::BinaryExpression => {
                    let be = node.as_binary_expression();
                    let left = self.parenthesize_expression_for_no_asi(be.left());
                    return self.emit_context.factory.update_binary_expression(
                        node,
                        node.modifiers(),
                        left,
                        be.type_(),
                        be.operator_token(),
                        be.right(),
                    );
                }
                Kind::ConditionalExpression => {
                    let ce = node.as_conditional_expression();
                    let condition = self.parenthesize_expression_for_no_asi(ce.condition());
                    return self.emit_context.factory.update_conditional_expression(
                        node,
                        condition,
                        ce.question_token(),
                        ce.when_true(),
                        ce.colon_token(),
                        ce.when_false(),
                    );
                }
                Kind::AsExpression => {
                    let ae = node.as_as_expression();
                    let expression = self.parenthesize_expression_for_no_asi(ae.expression());
                    return self.emit_context.factory.update_as_expression(
                        node,
                        expression,
                        ae.type_(),
                    );
                }
                Kind::SatisfiesExpression => {
                    let se = node.as_satisfies_expression();
                    let expression = self.parenthesize_expression_for_no_asi(se.expression());
                    return self.emit_context.factory.update_satisfies_expression(
                        node,
                        expression,
                        se.type_(),
                    );
                }
                Kind::NonNullExpression => {
                    let nne = node.as_non_null_expression();
                    let expression = self.parenthesize_expression_for_no_asi(nne.expression());
                    return self.emit_context.factory.update_non_null_expression(
                        node,
                        expression,
                        node.flags(),
                    );
                }
                _ => {}
            }
        }
        node
    }

    fn emit_expression_no_asi(&mut self, node: P<Node>, precedence: OperatorPrecedence) {
        let node = self.parenthesize_expression_for_no_asi(node);
        self.emit_expression(node, precedence);
    }

    pub(crate) fn emit_expression(&mut self, node: P<Node>, precedence: OperatorPrecedence) {
        let parens =
            get_expression_precedence(skip_partially_emitted_expressions(node)) < precedence;
        if parens {
            self.write_punctuation("(");
        }

        match node.kind() {
            // Keywords
            Kind::TrueKeyword | Kind::FalseKeyword | Kind::NullKeyword => {
                self.emit_token_node(Some(node));
            }
            Kind::ThisKeyword | Kind::SuperKeyword | Kind::ImportKeyword => {
                self.emit_keyword_expression(node);
            }

            // Literals
            Kind::NumericLiteral => self.emit_numeric_literal(node),
            Kind::BigIntLiteral => self.emit_big_int_literal(node),
            Kind::StringLiteral => self.emit_string_literal(node),
            Kind::RegularExpressionLiteral => self.emit_regular_expression_literal(node),
            Kind::NoSubstitutionTemplateLiteral => self.emit_no_substitution_template_literal(node),

            // Identifiers
            Kind::Identifier => self.emit_identifier_reference(node),
            Kind::PrivateIdentifier => self.emit_private_identifier(node),

            // Expressions
            Kind::ArrayLiteralExpression => self.emit_array_literal_expression(node),
            Kind::ObjectLiteralExpression => self.emit_object_literal_expression(node),
            Kind::PropertyAccessExpression => self.emit_property_access_expression(node),
            Kind::ElementAccessExpression => self.emit_element_access_expression(node),
            Kind::CallExpression => self.emit_call_expression(node),
            Kind::NewExpression => self.emit_new_expression(node),
            Kind::TaggedTemplateExpression => self.emit_tagged_template_expression(node),
            Kind::TypeAssertionExpression => self.emit_type_assertion_expression(node),
            Kind::ParenthesizedExpression => self.emit_parenthesized_expression(node),
            Kind::FunctionExpression => self.emit_function_expression(node),
            Kind::ArrowFunction => self.emit_arrow_function(node),
            Kind::DeleteExpression => self.emit_delete_expression(node),
            Kind::TypeOfExpression => self.emit_type_of_expression(node),
            Kind::VoidExpression => self.emit_void_expression(node),
            Kind::AwaitExpression => self.emit_await_expression(node),
            Kind::PrefixUnaryExpression => self.emit_prefix_unary_expression(node),
            Kind::PostfixUnaryExpression => self.emit_postfix_unary_expression(node),
            Kind::BinaryExpression => self.emit_binary_expression(node),
            Kind::ConditionalExpression => self.emit_conditional_expression(node),
            Kind::TemplateExpression => self.emit_template_expression(node),
            Kind::YieldExpression => self.emit_yield_expression(node),
            Kind::SpreadElement => self.emit_spread_element(node),
            Kind::ClassExpression => self.emit_class_expression(node),
            Kind::OmittedExpression => self.emit_omitted_expression(node),
            Kind::AsExpression => self.emit_as_expression(node),
            Kind::NonNullExpression => self.emit_non_null_expression(node),
            Kind::ExpressionWithTypeArguments => self.emit_expression_with_type_arguments(node),
            Kind::SatisfiesExpression => self.emit_satisfies_expression(node),
            Kind::MetaProperty => self.emit_meta_property(node),
            Kind::SyntheticExpression => panic!("SyntheticExpression should never be printed."),
            Kind::MissingDeclaration => {
                // Missing declarations do not emit an expression.
            }

            // JSX
            Kind::JsxElement => self.emit_jsx_element(node),
            Kind::JsxSelfClosingElement => self.emit_jsx_self_closing_element(node),
            Kind::JsxFragment => self.emit_jsx_fragment(node),

            // Synthesized list
            Kind::SyntaxList => panic!("SyntaxList should not be printed"),

            // Transformation nodes
            Kind::NotEmittedStatement => return,
            Kind::PartiallyEmittedExpression => self.emit_partially_emitted_expression(node),
            Kind::SyntheticReferenceExpression => {
                panic!("SyntheticReferenceExpression should not be printed")
            }

            _ => panic!("unexpected Expression: {:?}", node.kind()),
        }

        if parens {
            self.write_punctuation(")");
        }
    }
}

//
// Misc
//

impl Printer {
    pub(crate) fn emit_template_span(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_template_span();
        self.emit_expression(n.expression(), OperatorPrecedence::Comma);
        self.emit_template_middle_tail(n.literal());
        self.exit_node(node, state);
    }

    fn emit_template_span_node(&mut self, node: P<Node>) {
        self.emit_template_span(node);
    }

    pub(crate) fn emit_semicolon_class_element(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.write_trailing_semicolon();
        self.exit_node(node, state);
    }
}

//
// Statements
//

impl Printer {
    fn is_empty_block(&self, block: P<Node>, statements: P<NodeList>) -> bool {
        statements.nodes().is_empty()
            && (self.current_source_file().is_none()
                || range_end_is_on_same_line_as_range_start(
                    block.loc(),
                    block.loc(),
                    self.current_source_file().unwrap(),
                ))
    }

    fn emit_block(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_block();
        self.generate_names(Some(node));
        self.emit_token(Kind::OpenBraceToken, node.pos(), WriteKind::Punctuation, node);

        let format = if !n.multi_line && self.is_empty_block(node, n.statements)
            || self.should_emit_on_single_line(node)
        {
            ListFormat::SingleLineBlockStatements
        } else {
            ListFormat::MultiLineBlockStatements
        };
        self.emit_list(Printer::emit_statement, node, Some(n.statements), format);

        self.emit_token_ex(
            Kind::CloseBraceToken,
            n.statements.end(),
            WriteKind::Punctuation,
            node,
            if format.intersects(ListFormat::MultiLine) {
                tokenEmitFlags::IndentLeadingComments
            } else {
                tokenEmitFlags::None
            },
        );
        self.exit_node(node, state);
    }

    fn emit_variable_statement(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_modifier_list(node, node.modifiers(), false /*allowDecorators*/);
        self.emit_variable_declaration_list(node.as_variable_statement().declaration_list());
        self.write_trailing_semicolon();
        self.exit_node(node, state);
    }

    fn emit_empty_statement(&mut self, node: P<Node>, is_embedded_statement: bool) {
        let state = self.enter_node(node);

        // While most trailing semicolons are possibly insignificant, an embedded "empty"
        // statement is significant and cannot be elided by a trailing-semicolon-omitting writer.
        if is_embedded_statement {
            self.write_punctuation(";");
        } else {
            self.write_trailing_semicolon();
        }
        self.exit_node(node, state);
    }

    fn emit_expression_statement(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let expression = node.as_expression_statement().expression();

        if self.current_source_file().is_some_and(|f| f.script_kind() == ScriptKind::JSON) {
            // !!! In strada, this was handled by an undefined parenthesizerRule, so this is a hack.
            self.emit_expression(expression, OperatorPrecedence::Comma);
        } else if is_immediately_invoked_function_expression_or_arrow_function(expression) {
            // For IIFEs, parenthesize just the callee (not the whole call), matching TypeScript's
            // parenthesizeExpressionOfExpressionStatement which wraps the function/arrow in parens:
            //   (function() { })()  -- not (function() { }())
            self.emit_iife_with_parenthesized_callee(expression);
        } else {
            match get_leftmost_expression(expression, false /*stopAtCallExpression*/).kind() {
                Kind::FunctionExpression | Kind::ObjectLiteralExpression => {
                    self.emit_expression(expression, OperatorPrecedence::Parentheses);
                }
                _ => self.emit_expression(expression, OperatorPrecedence::Comma),
            }
        }

        // Emit semicolon in non json files
        // or if json file that created synthesized expression(eg.define expression statement when --out and amd code generation)
        if self.current_source_file().is_none()
            || self.current_source_file().unwrap().script_kind() != ScriptKind::JSON
            || node_is_synthesized(expression)
        {
            self.write_trailing_semicolon();
        }

        self.exit_node(node, state);
    }

    // emitIIFEWithParenthesizedCallee emits a call expression that is an IIFE,
    // wrapping just the callee in parens rather than the entire call expression.
    // This matches TypeScript's parenthesizeExpressionOfExpressionStatement behavior:
    //
    //	(function() { })()   -- parens around callee only
    //
    // instead of:
    //
    //	(function() { }())   -- parens around entire call
    fn emit_iife_with_parenthesized_callee(&mut self, node: P<Node>) {
        // Walk through PartiallyEmittedExpression wrappers to find the call
        let call_node = skip_partially_emitted_expressions(node);
        let call = call_node.as_call_expression();
        let state = self.enter_node(call_node);
        // Emit the callee wrapped in parens
        self.write_punctuation("(");
        self.emit_expression(call.expression(), OperatorPrecedence::LOWEST);
        self.write_punctuation(")");
        self.emit_token_node(call.question_dot_token());
        self.emit_type_arguments(call_node, call.type_arguments());
        self.emit_list(
            Printer::emit_argument,
            call_node,
            Some(call.arguments()),
            ListFormat::CallExpressionArguments,
        );
        self.exit_node(call_node, state);
    }

    fn emit_if_statement(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_if_statement();
        let pos = self.emit_token(Kind::IfKeyword, node.pos(), WriteKind::Keyword, node);
        self.write_space();
        self.emit_token(Kind::OpenParenToken, pos, WriteKind::Punctuation, node);
        self.emit_expression(n.expression(), OperatorPrecedence::LOWEST);
        self.emit_token(Kind::CloseParenToken, n.expression().end(), WriteKind::Punctuation, node);
        self.emit_embedded_statement(node, n.then_statement());
        if let Some(else_statement) = n.else_statement() {
            self.write_line_or_space(node, n.then_statement(), else_statement);
            self.emit_token(Kind::ElseKeyword, n.then_statement().end(), WriteKind::Keyword, node);
            if else_statement.kind() == Kind::IfStatement {
                self.write_space();
                self.emit_if_statement(else_statement);
            } else {
                self.emit_embedded_statement(node, else_statement);
            }
        }
        self.exit_node(node, state);
    }

    fn emit_while_clause(&mut self, node: P<Node>, expression: P<Node>, start_pos: TextPos) {
        let pos = self.emit_token(Kind::WhileKeyword, start_pos, WriteKind::Keyword, node);
        self.write_space();
        self.emit_token(Kind::OpenParenToken, pos, WriteKind::Punctuation, node);
        self.emit_expression(expression, OperatorPrecedence::LOWEST);
        self.emit_token(Kind::CloseParenToken, expression.end(), WriteKind::Punctuation, node);
    }

    fn emit_do_statement(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_do_statement();
        self.emit_token(Kind::DoKeyword, node.pos(), WriteKind::Keyword, node);
        self.emit_embedded_statement(node, n.statement());
        if is_block(n.statement()) && !self.options.preserve_source_newlines {
            self.write_space();
        } else {
            self.write_line_or_space(node, n.statement(), n.expression());
        }

        self.emit_while_clause(node, n.expression(), n.statement().end());
        self.write_trailing_semicolon();
        self.exit_node(node, state);
    }

    fn emit_while_statement(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_while_statement();
        self.emit_while_clause(node, n.expression(), node.pos());
        self.emit_embedded_statement(node, n.statement());
        self.exit_node(node, state);
    }

    fn emit_for_initializer(&mut self, node: P<Node>) {
        if node.kind() == Kind::VariableDeclarationList {
            self.emit_variable_declaration_list(node);
        } else {
            self.emit_expression(node, OperatorPrecedence::LOWEST);
        }
    }

    fn emit_for_statement(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_for_statement();
        let mut pos = self.emit_token(Kind::ForKeyword, node.pos(), WriteKind::Keyword, node);
        self.write_space();
        pos = self.emit_token(Kind::OpenParenToken, pos, WriteKind::Punctuation, node);
        if let Some(initializer) = n.initializer() {
            self.emit_for_initializer(initializer);
            pos = initializer.end();
        }
        pos = self.emit_token(Kind::SemicolonToken, pos, WriteKind::Punctuation, node);
        if let Some(condition) = n.condition() {
            self.write_space();
            self.emit_expression(condition, OperatorPrecedence::LOWEST);
            pos = condition.end();
        }
        pos = self.emit_token(Kind::SemicolonToken, pos, WriteKind::Punctuation, node);
        if let Some(incrementor) = n.incrementor() {
            self.write_space();
            self.emit_expression(incrementor, OperatorPrecedence::LOWEST);
            pos = incrementor.end();
        }
        self.emit_token(Kind::CloseParenToken, pos, WriteKind::Punctuation, node);
        self.emit_embedded_statement(node, n.statement());
        self.exit_node(node, state);
    }

    fn emit_for_in_statement(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_for_in_or_of_statement();
        let pos = self.emit_token(Kind::ForKeyword, node.pos(), WriteKind::Keyword, node);
        self.write_space();
        self.emit_token(Kind::OpenParenToken, pos, WriteKind::Punctuation, node);
        self.emit_for_initializer(n.initializer());
        self.write_space();
        self.emit_token(Kind::InKeyword, n.initializer().end(), WriteKind::Keyword, node);
        self.write_space();
        self.emit_expression(n.expression(), OperatorPrecedence::LOWEST);
        self.emit_token(Kind::CloseParenToken, n.expression().end(), WriteKind::Punctuation, node);
        self.emit_embedded_statement(node, n.statement());
        self.exit_node(node, state);
    }

    fn emit_for_of_statement(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_for_in_or_of_statement();
        let open_paren_pos =
            self.emit_token(Kind::ForKeyword, node.pos(), WriteKind::Keyword, node);
        self.write_space();
        if n.await_modifier().is_some() {
            self.emit_keyword_node(n.await_modifier());
            self.write_space();
        }
        self.emit_token(Kind::OpenParenToken, open_paren_pos, WriteKind::Punctuation, node);
        self.emit_for_initializer(n.initializer());
        self.write_space();
        self.emit_token(Kind::OfKeyword, n.initializer().end(), WriteKind::Keyword, node);
        self.write_space();
        self.emit_expression(n.expression(), OperatorPrecedence::LOWEST);
        self.emit_token(Kind::CloseParenToken, n.expression().end(), WriteKind::Punctuation, node);
        self.emit_embedded_statement(node, n.statement());
        self.exit_node(node, state);
    }

    fn emit_continue_statement(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_token(Kind::ContinueKeyword, node.pos(), WriteKind::Keyword, node);
        if let Some(label) = node.as_continue_statement().label() {
            self.write_space();
            self.emit_label_identifier(label);
        }
        self.write_trailing_semicolon();
        self.exit_node(node, state);
    }

    fn emit_break_statement(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_token(Kind::BreakKeyword, node.pos(), WriteKind::Keyword, node);
        if let Some(label) = node.as_break_statement().label() {
            self.write_space();
            self.emit_label_identifier(label);
        }
        self.write_trailing_semicolon();
        self.exit_node(node, state);
    }

    fn emit_return_statement(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_token(Kind::ReturnKeyword, node.pos(), WriteKind::Keyword, node);
        if let Some(expression) = node.as_return_statement().expression() {
            self.write_space();
            self.emit_expression_no_asi(expression, OperatorPrecedence::LOWEST);
        }
        self.write_trailing_semicolon();
        self.exit_node(node, state);
    }

    fn emit_with_statement(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_with_statement();
        let pos = self.emit_token(Kind::WithKeyword, node.pos(), WriteKind::Keyword, node);
        self.write_space();
        self.emit_token(Kind::OpenParenToken, pos, WriteKind::Punctuation, node);
        self.emit_expression(n.expression(), OperatorPrecedence::LOWEST);
        self.emit_token(Kind::CloseParenToken, n.expression().end(), WriteKind::Punctuation, node);
        self.emit_embedded_statement(node, n.statement());
        self.exit_node(node, state);
    }

    fn emit_switch_statement(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_switch_statement();
        let pos = self.emit_token(Kind::SwitchKeyword, node.pos(), WriteKind::Keyword, node);
        self.write_space();
        self.emit_token(Kind::OpenParenToken, pos, WriteKind::Punctuation, node);
        self.emit_expression(n.expression(), OperatorPrecedence::LOWEST);
        self.emit_token(Kind::CloseParenToken, n.expression().end(), WriteKind::Punctuation, node);
        self.write_space();
        self.emit_case_block(n.case_block());
        self.exit_node(node, state);
    }

    fn emit_labeled_statement(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_labeled_statement();
        self.emit_label_identifier(n.label());
        self.emit_token(Kind::ColonToken, n.label().end(), WriteKind::Punctuation, node);

        // TODO: use emitEmbeddedStatement rather than writeSpace/emitStatement here after Strada migration as it is
        //       more consistent with similar emit elsewhere. writeSpace/emitStatement is used here to reduce spurious
        //       diffs when testing the Strada migration.
        ////p.emitEmbeddedStatement(node.AsNode(), node.Statement)

        self.write_space();
        self.emit_statement(n.statement());

        self.exit_node(node, state);
    }

    fn emit_throw_statement(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_token(Kind::ThrowKeyword, node.pos(), WriteKind::Keyword, node);
        self.write_space();
        self.emit_expression_no_asi(
            node.as_throw_statement().expression(),
            OperatorPrecedence::LOWEST,
        );
        self.write_trailing_semicolon();
        self.exit_node(node, state);
    }

    fn emit_try_statement(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_try_statement();
        self.emit_token(Kind::TryKeyword, node.pos(), WriteKind::Keyword, node);
        self.write_space();
        self.emit_block(n.try_block());
        if let Some(catch_clause) = n.catch_clause() {
            self.write_line_or_space(node, n.try_block(), catch_clause);
            self.emit_catch_clause(catch_clause);
        }
        if let Some(finally_block) = n.finally_block() {
            let previous = n.catch_clause().unwrap_or(n.try_block());
            self.write_line_or_space(node, previous, finally_block);
            self.emit_token(Kind::FinallyKeyword, previous.end(), WriteKind::Keyword, node);
            self.write_space();
            self.emit_block(finally_block);
        }
        self.exit_node(node, state);
    }

    fn emit_debugger_statement(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_token(Kind::DebuggerKeyword, node.pos(), WriteKind::Keyword, node);
        self.write_trailing_semicolon();
        self.exit_node(node, state);
    }

    pub(crate) fn emit_not_emitted_statement(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.exit_node(node, state);
    }

    pub(crate) fn emit_not_emitted_type_element(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.exit_node(node, state);
    }
}

//
// Declarations
//

impl Printer {
    pub(crate) fn emit_variable_declaration(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_variable_declaration();
        self.emit_binding_name(Some(n.name()));
        self.emit_punctuation_node(n.exclamation_token());
        self.emit_type_annotation(n.type_());
        let type_node = self.emit_context.get_type_node(n.name());
        self.emit_initializer(
            n.initializer(),
            greatest_end(n.name().end(), &[&n.type_(), &type_node]),
            node,
        );
        self.exit_node(node, state);
    }

    fn emit_variable_declaration_node(&mut self, node: P<Node>) {
        self.emit_variable_declaration(node);
    }

    pub(crate) fn emit_variable_declaration_list(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        if is_var_let(node) {
            self.write_keyword("let");
        } else if is_var_const(node) {
            self.write_keyword("const");
        } else if is_var_using(node) {
            self.write_keyword("using");
        } else if is_var_await_using(node) {
            self.write_keyword("await");
            self.write_space();
            self.write_keyword("using");
        } else {
            self.write_keyword("var");
        }
        self.write_space();
        self.emit_list(
            Printer::emit_variable_declaration_node,
            node,
            Some(node.as_variable_declaration_list().declarations()),
            ListFormat::VariableDeclarationList,
        );
        self.exit_node(node, state);
    }

    fn emit_function_declaration(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_function_declaration();
        self.generate_name_if_needed(n.name());
        self.emit_modifier_list(node, node.modifiers(), false /*allowDecorators*/);
        self.write_keyword("function");
        self.emit_token_node(n.asterisk_token());
        self.write_space();
        if let Some(name) = n.name() {
            self.emit_identifier_name(name);
        }
        let indented = self.should_emit_indented(node);
        self.increase_indent_if(indented);
        self.push_name_generation_scope(Some(node));
        self.emit_signature(node);
        self.emit_function_body_node(n.body());
        self.pop_name_generation_scope(Some(node));
        self.decrease_indent_if(indented);
        self.exit_node(node, state);
    }

    fn emit_class_declaration(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_class_declaration();
        self.generate_name_if_needed(n.name());
        let pos = self.emit_modifier_list(node, node.modifiers(), true /*allowDecorators*/);
        self.emit_token(Kind::ClassKeyword, pos, WriteKind::Keyword, node);
        if let Some(name) = n.name() {
            self.write_space();
            self.emit_identifier_name(name);
        }
        let indented = self.should_emit_indented(node);
        self.increase_indent_if(indented);
        self.emit_type_parameters(node, n.type_parameters());
        self.emit_list(
            Printer::emit_heritage_clause_node,
            node,
            n.heritage_clauses(),
            ListFormat::ClassHeritageClauses,
        );
        self.write_space();
        self.write_punctuation("{");
        self.push_name_generation_scope(Some(node));
        self.generate_all_member_names(Some(n.members()));
        self.emit_list(
            Printer::emit_class_element,
            node,
            Some(n.members()),
            ListFormat::ClassMembers,
        );
        self.pop_name_generation_scope(Some(node));
        self.write_punctuation("}");
        self.decrease_indent_if(indented);
        self.exit_node(node, state);
    }

    fn emit_interface_declaration(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_interface_declaration();
        self.emit_modifier_list(node, node.modifiers(), false /*allowDecorators*/);
        self.write_keyword("interface");
        self.write_space();
        self.emit_binding_identifier(n.name());
        self.emit_type_parameters(node, n.type_parameters());
        self.emit_list(
            Printer::emit_heritage_clause_node,
            node,
            n.heritage_clauses(),
            ListFormat::HeritageClauses,
        );
        self.write_space();
        self.write_punctuation("{");
        self.push_name_generation_scope(Some(node));
        self.generate_all_member_names(Some(n.members()));
        self.emit_list(
            Printer::emit_type_element,
            node,
            Some(n.members()),
            ListFormat::InterfaceMembers,
        );
        self.pop_name_generation_scope(Some(node));
        self.write_punctuation("}");
        self.exit_node(node, state);
    }

    pub(crate) fn emit_type_alias_declaration(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_type_alias_declaration();
        self.emit_modifier_list(node, node.modifiers(), false /*allowDecorators*/);
        self.write_keyword("type");
        self.write_space();
        self.emit_binding_identifier(n.name());
        self.emit_type_parameters(node, n.type_parameters());
        self.write_space();
        self.write_punctuation("=");
        self.write_space();
        self.emit_type_node_outside_extends(n.type_().unwrap());
        self.write_trailing_semicolon();
        self.exit_node(node, state);
    }

    fn emit_enum_declaration(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_enum_declaration();
        self.emit_modifier_list(node, node.modifiers(), false /*allowDecorators*/);
        self.write_keyword("enum");
        self.write_space();
        self.emit_binding_identifier(n.name());
        self.write_space();
        self.write_punctuation("{");
        self.emit_list(
            Printer::emit_enum_member_node,
            node,
            Some(n.members()),
            ListFormat::EnumMembers,
        );
        self.write_punctuation("}");
        self.exit_node(node, state);
    }

    fn emit_module_declaration(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_module_declaration();
        self.emit_modifier_list(node, node.modifiers(), false /*allowDecorators*/);
        if n.keyword != Kind::GlobalKeyword {
            self.write_keyword(if n.keyword == Kind::NamespaceKeyword {
                "namespace"
            } else {
                "module"
            });
            self.write_space();
        }
        self.emit_module_name(Some(n.name()));
        let mut body = n.body();
        while let Some(b) = body {
            if !is_module_declaration(b) {
                break;
            }
            let module = b.as_module_declaration();
            self.write_punctuation(".");
            self.emit_nested_module_name(Some(module.name()));
            body = module.body();
        }
        if let Some(attributes) = n.attributes() {
            self.write_space();
            self.write_keyword("with");
            self.write_space();
            self.emit_type_node(attributes, TypePrecedence::NonArray);
        }
        match body {
            None => self.write_trailing_semicolon(),
            Some(body) => {
                self.write_space();
                self.emit_module_block(body);
            }
        }
        self.exit_node(node, state);
    }

    pub(crate) fn emit_module_block(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let statements = node.as_module_block().statements();
        self.generate_names(Some(node));
        self.emit_token(Kind::OpenBraceToken, node.pos(), WriteKind::Punctuation, node);
        let format =
            if self.is_empty_block(node, statements) || self.should_emit_on_single_line(node) {
                ListFormat::SingleLineBlockStatements
            } else {
                ListFormat::MultiLineBlockStatements
            };
        self.emit_list(Printer::emit_statement, node, Some(statements), format);
        self.emit_token_ex(
            Kind::CloseBraceToken,
            statements.end(),
            WriteKind::Punctuation,
            node,
            if format.intersects(ListFormat::MultiLine) {
                tokenEmitFlags::IndentLeadingComments
            } else {
                tokenEmitFlags::None
            },
        );
        self.exit_node(node, state);
    }

    pub(crate) fn emit_case_block(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let clauses = node.as_case_block().clauses();
        self.emit_token(Kind::OpenBraceToken, node.pos(), WriteKind::Punctuation, node);
        self.emit_list(
            Printer::emit_case_or_default_clause_node,
            node,
            Some(clauses),
            ListFormat::CaseBlockClauses,
        );
        self.emit_token_ex(
            Kind::CloseBraceToken,
            clauses.end(),
            WriteKind::Punctuation,
            node,
            tokenEmitFlags::IndentLeadingComments,
        );
        self.exit_node(node, state);
    }

    fn emit_import_equals_declaration(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_import_equals_declaration();
        self.emit_modifier_list(node, node.modifiers(), false /*allowDecorators*/);
        let pos = self.emit_token(
            Kind::ImportKeyword,
            greatest_end(node.pos(), &[&node.modifiers()]),
            WriteKind::Keyword,
            node,
        );
        self.write_space();
        if n.is_type_only {
            self.emit_token(Kind::TypeKeyword, pos, WriteKind::Keyword, node);
            self.write_space();
        }
        self.emit_binding_identifier(n.name());
        self.write_space();
        self.emit_token(Kind::EqualsToken, n.name().end(), WriteKind::Punctuation, node);
        self.write_space();
        self.emit_module_reference(n.module_reference());
        self.write_trailing_semicolon();
        self.exit_node(node, state);
    }

    fn emit_module_reference(&mut self, node: P<Node>) {
        match node.kind() {
            Kind::Identifier => self.emit_identifier_reference(node),
            Kind::QualifiedName => self.emit_qualified_name(node),
            Kind::ExternalModuleReference => self.emit_external_module_reference(node),
            _ => panic!("unhandled ModuleReference: {:?}", node.kind()),
        }
    }

    fn emit_import_declaration(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_import_declaration();
        self.emit_modifier_list(node, node.modifiers(), false /*allowDecorators*/);
        self.emit_token(
            Kind::ImportKeyword,
            greatest_end(node.pos(), &[&node.modifiers()]),
            WriteKind::Keyword,
            node,
        );
        self.write_space();
        if let Some(import_clause) = n.import_clause() {
            self.emit_import_clause(import_clause);
            self.write_space();
            self.emit_token(Kind::FromKeyword, import_clause.end(), WriteKind::Keyword, node);
            self.write_space();
        }
        self.emit_expression(n.module_specifier(), OperatorPrecedence::LOWEST);
        if let Some(attributes) = n.attributes() {
            self.write_space();
            self.emit_import_attributes(attributes);
        }
        self.write_trailing_semicolon();
        self.exit_node(node, state);
    }

    pub(crate) fn emit_import_clause(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_import_clause();
        if n.phase_modifier() != Kind::Unknown {
            self.emit_token(n.phase_modifier(), node.pos(), WriteKind::Keyword, node);
            self.write_space();
        }
        if let Some(name) = n.name() {
            self.emit_binding_identifier(name);
            if n.named_bindings().is_some() {
                self.emit_token(Kind::CommaToken, name.end(), WriteKind::Punctuation, node);
                self.write_space();
            }
        }
        self.emit_named_import_bindings(n.named_bindings());
        self.exit_node(node, state);
    }

    pub(crate) fn emit_namespace_import(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let pos = self.emit_token(Kind::AsteriskToken, node.pos(), WriteKind::Punctuation, node);
        self.write_space();
        self.emit_token(Kind::AsKeyword, pos, WriteKind::Keyword, node);
        self.write_space();
        self.emit_binding_identifier(node.as_namespace_import().name());
        self.exit_node(node, state);
    }

    pub(crate) fn emit_named_imports(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.write_punctuation("{");
        self.emit_list(
            Printer::emit_import_specifier_node,
            node,
            Some(node.as_named_imports().elements()),
            ListFormat::NamedImportsOrExportsElements,
        );
        self.write_punctuation("}");
        self.exit_node(node, state);
    }

    fn emit_named_import_bindings(&mut self, node: Option<P<Node>>) {
        let Some(node) = node else {
            return;
        };
        match node.kind() {
            Kind::NamespaceImport => self.emit_namespace_import(node),
            Kind::NamedImports => self.emit_named_imports(node),
            _ => panic!("unhandled NamedImportBindings: {:?}", node.kind()),
        }
    }

    pub(crate) fn emit_import_specifier(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_import_specifier();
        if n.is_type_only {
            self.write_keyword("type");
            self.write_space();
        }
        if let Some(property_name) = n.property_name() {
            self.emit_module_export_name(Some(property_name));
            self.write_space();
            self.emit_token(Kind::AsKeyword, property_name.end(), WriteKind::Keyword, node);
            self.write_space();
        }
        self.emit_binding_identifier(n.name());
        self.exit_node(node, state);
    }

    fn emit_import_specifier_node(&mut self, node: P<Node>) {
        self.emit_import_specifier(node);
    }

    fn emit_export_assignment(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_export_assignment();
        let next_pos = self.emit_token(Kind::ExportKeyword, node.pos(), WriteKind::Keyword, node);
        self.write_space();
        if n.is_export_equals {
            self.emit_token(Kind::EqualsToken, next_pos, WriteKind::Operator, node);
        } else {
            self.emit_token(Kind::DefaultKeyword, next_pos, WriteKind::Keyword, node);
        }
        self.write_space();
        if n.is_export_equals {
            self.emit_expression(n.expression(), OperatorPrecedence::Assignment);
        } else {
            // parenthesize `class` and `function` expressions so as not to conflict with exported `class` and `function` declarations
            let expr =
                get_leftmost_expression(n.expression(), false /*stopAtCallExpressions*/);
            if is_class_expression(expr) || is_function_expression(expr) {
                self.emit_expression(n.expression(), OperatorPrecedence::Parentheses);
            } else {
                self.emit_expression(n.expression(), OperatorPrecedence::Assignment);
            }
        }
        self.write_trailing_semicolon();
        self.exit_node(node, state);
    }

    fn emit_export_declaration(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_export_declaration();
        self.emit_modifier_list(node, node.modifiers(), false /*allowDecorators*/);
        let mut pos = self.emit_token(Kind::ExportKeyword, node.pos(), WriteKind::Keyword, node);
        self.write_space();
        if n.is_type_only {
            pos = self.emit_token(Kind::TypeKeyword, pos, WriteKind::Keyword, node);
            self.write_space();
        }
        if let Some(export_clause) = n.export_clause() {
            self.emit_named_export_bindings(export_clause);
        } else {
            pos = self.emit_token(Kind::AsteriskToken, pos, WriteKind::Punctuation, node);
        }
        if let Some(module_specifier) = n.module_specifier() {
            self.write_space();
            self.emit_token(
                Kind::FromKeyword,
                greatest_end(pos, &[&n.export_clause()]),
                WriteKind::Keyword,
                node,
            );
            self.write_space();
            self.emit_expression(module_specifier, OperatorPrecedence::LOWEST);
        }
        if let Some(attributes) = n.attributes() {
            self.write_space();
            self.emit_import_attributes(attributes);
        }
        self.write_trailing_semicolon();
        self.exit_node(node, state);
    }

    pub(crate) fn emit_import_attributes(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_import_attributes();
        self.emit_token(n.token, node.pos(), WriteKind::Keyword, node);
        self.write_space();
        self.emit_list(
            Printer::emit_import_attribute_node,
            node,
            Some(n.attributes()),
            ListFormat::ImportAttributes,
        );
        self.exit_node(node, state);
    }

    pub(crate) fn emit_import_attribute(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_import_attribute();
        self.emit_import_attribute_name(n.name().unwrap());
        self.write_punctuation(":");
        self.write_space();
        let value = n.value();
        if !self.emit_context.emit_flags(value).intersects(EmitFlags::NoLeadingComments) {
            let comment_range = self.emit_context.comment_range(value);
            self.emit_trailing_comments(comment_range.pos(), CommentSeparator::After);
        }
        self.emit_expression(value, OperatorPrecedence::DISALLOW_COMMA);
        self.exit_node(node, state);
    }

    pub(crate) fn emit_import_attribute_node(&mut self, node: P<Node>) {
        self.emit_import_attribute(node);
    }

    fn emit_namespace_export_declaration(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let mut pos = self.emit_token(Kind::ExportKeyword, node.pos(), WriteKind::Keyword, node);
        self.write_space();
        pos = self.emit_token(Kind::AsKeyword, pos, WriteKind::Keyword, node);
        self.write_space();
        self.emit_token(Kind::NamespaceKeyword, pos, WriteKind::Keyword, node);
        self.write_space();
        self.emit_binding_identifier(node.as_namespace_export_declaration().name());
        self.write_trailing_semicolon();
        self.exit_node(node, state);
    }

    pub(crate) fn emit_namespace_export(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let pos = self.emit_token(Kind::AsteriskToken, node.pos(), WriteKind::Punctuation, node);
        self.write_space();
        self.emit_token(Kind::AsKeyword, pos, WriteKind::Keyword, node);
        self.write_space();
        self.emit_module_export_name(Some(node.as_namespace_export().name()));
        self.exit_node(node, state);
    }

    pub(crate) fn emit_named_exports(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.write_punctuation("{");
        self.emit_list(
            Printer::emit_export_specifier_node,
            node,
            Some(node.as_named_exports().elements()),
            ListFormat::NamedImportsOrExportsElements,
        );
        self.write_punctuation("}");
        self.exit_node(node, state);
    }

    fn emit_named_export_bindings(&mut self, node: P<Node>) {
        match node.kind() {
            Kind::NamespaceExport => self.emit_namespace_export(node),
            Kind::NamedExports => self.emit_named_exports(node),
            _ => panic!("unhandled NamedExportBindings: {:?}", node.kind()),
        }
    }

    pub(crate) fn emit_export_specifier(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_export_specifier();
        if n.is_type_only {
            self.write_keyword("type");
            self.write_space();
        }
        if let Some(property_name) = n.property_name() {
            self.emit_module_export_name(Some(property_name));
            self.write_space();
            self.emit_token(Kind::AsKeyword, property_name.end(), WriteKind::Keyword, node);
            self.write_space();
        }
        self.emit_module_export_name(Some(n.name()));
        self.exit_node(node, state);
    }

    fn emit_export_specifier_node(&mut self, node: P<Node>) {
        self.emit_export_specifier(node);
    }

    fn emit_embedded_statement(&mut self, parent_node: P<Node>, node: P<Node>) {
        if is_block(node)
            || self.should_emit_on_single_line(parent_node)
            || self.options.preserve_source_newlines
                && self.get_leading_line_terminator_count(
                    Some(parent_node),
                    Some(node),
                    ListFormat::None,
                ) == 0
        {
            self.write_space();
            self.emit_statement(node);
        } else {
            self.write_line();
            self.increase_indent();
            if node.kind() == Kind::EmptyStatement {
                self.emit_empty_statement(node, true /*isEmbeddedStatement*/);
            } else {
                self.emit_statement(node);
            }
            self.decrease_indent();
        }
    }

    pub(crate) fn emit_statement(&mut self, node: P<Node>) {
        if let Some(snippet_element) = self.emit_context.snippet_element(node) {
            self.emit_snippet_node(node, snippet_element);
            return;
        }

        match node.kind() {
            // Statements
            Kind::Block => self.emit_block(node),
            Kind::EmptyStatement => {
                self.emit_empty_statement(node, false /*isEmbeddedStatement*/);
            }
            Kind::VariableStatement => self.emit_variable_statement(node),
            Kind::ExpressionStatement => self.emit_expression_statement(node),
            Kind::IfStatement => self.emit_if_statement(node),
            Kind::DoStatement => self.emit_do_statement(node),
            Kind::WhileStatement => self.emit_while_statement(node),
            Kind::ForStatement => self.emit_for_statement(node),
            Kind::ForInStatement => self.emit_for_in_statement(node),
            Kind::ForOfStatement => self.emit_for_of_statement(node),
            Kind::ContinueStatement => self.emit_continue_statement(node),
            Kind::BreakStatement => self.emit_break_statement(node),
            Kind::ReturnStatement => self.emit_return_statement(node),
            Kind::WithStatement => self.emit_with_statement(node),
            Kind::SwitchStatement => self.emit_switch_statement(node),
            Kind::LabeledStatement => self.emit_labeled_statement(node),
            Kind::ThrowStatement => self.emit_throw_statement(node),
            Kind::TryStatement => self.emit_try_statement(node),
            Kind::DebuggerStatement => self.emit_debugger_statement(node),
            Kind::NotEmittedStatement => self.emit_not_emitted_statement(node),

            // Declaration Statements
            Kind::FunctionDeclaration => self.emit_function_declaration(node),
            Kind::ClassDeclaration => self.emit_class_declaration(node),
            Kind::InterfaceDeclaration => self.emit_interface_declaration(node),
            Kind::TypeAliasDeclaration | Kind::JSTypeAliasDeclaration => {
                self.emit_type_alias_declaration(node);
            }
            Kind::EnumDeclaration => self.emit_enum_declaration(node),
            Kind::ModuleDeclaration => self.emit_module_declaration(node),
            Kind::MissingDeclaration => {
                // Missing declarations do not emit a statement.
            }

            // Import/Export Statements
            Kind::NamespaceExportDeclaration => self.emit_namespace_export_declaration(node),
            Kind::ImportEqualsDeclaration => self.emit_import_equals_declaration(node),
            Kind::ImportDeclaration => self.emit_import_declaration(node),
            Kind::ExportAssignment => self.emit_export_assignment(node),
            Kind::ExportDeclaration => self.emit_export_declaration(node),

            _ => panic!("unhandled statement: {:?}", node.kind()),
        }
    }
}

//
// Module references
//

impl Printer {
    pub(crate) fn emit_external_module_reference(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.write_keyword("require");
        self.write_punctuation("(");
        self.emit_expression(
            node.as_external_module_reference().expression(),
            OperatorPrecedence::DISALLOW_COMMA,
        );
        self.write_punctuation(")");
        self.exit_node(node, state);
    }
}

//
// JSX
//

impl Printer {
    fn emit_jsx_element(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_jsx_element();
        self.emit_jsx_opening_element(n.opening_element());
        self.emit_list(
            Printer::emit_jsx_child,
            node,
            Some(n.children()),
            ListFormat::JsxElementOrFragmentChildren,
        );
        self.emit_jsx_closing_element(n.closing_element());
        self.exit_node(node, state);
    }

    fn emit_jsx_self_closing_element(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_jsx_self_closing_element();
        self.write_punctuation("<");
        self.emit_jsx_tag_name(n.tag_name());
        self.emit_type_arguments(node, n.type_arguments());
        self.write_space();
        self.emit_jsx_attributes(n.attributes());
        self.write_punctuation("/>");
        self.exit_node(node, state);
    }

    fn emit_jsx_fragment(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_jsx_fragment();
        self.emit_jsx_opening_fragment(n.opening_fragment());
        self.emit_list(
            Printer::emit_jsx_child,
            node,
            Some(n.children()),
            ListFormat::JsxElementOrFragmentChildren,
        );
        self.emit_jsx_closing_fragment(n.closing_fragment());
        self.exit_node(node, state);
    }

    pub(crate) fn emit_jsx_opening_element(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_jsx_opening_element();
        self.write_punctuation("<");
        let indented = self.write_line_separators_and_indent_before(n.tag_name(), node);
        self.emit_jsx_tag_name(n.tag_name());
        self.emit_type_arguments(node, n.type_arguments());
        if !n.attributes().properties().is_empty() {
            self.write_space();
        }
        self.emit_jsx_attributes(n.attributes());
        self.write_line_separators_after(n.attributes(), node);
        self.decrease_indent_if(indented);
        self.write_punctuation(">");
        self.exit_node(node, state);
    }

    pub(crate) fn emit_jsx_closing_element(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.write_punctuation("</");
        self.emit_jsx_tag_name(node.as_jsx_closing_element().tag_name());
        self.write_punctuation(">");
        self.exit_node(node, state);
    }

    pub(crate) fn emit_jsx_opening_fragment(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.write_punctuation("<");
        self.write_punctuation(">");
        self.exit_node(node, state);
    }

    pub(crate) fn emit_jsx_closing_fragment(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.write_punctuation("</");
        self.write_punctuation(">");
        self.exit_node(node, state);
    }

    pub(crate) fn emit_jsx_text(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        // TODO(rbuckton): Should this be using `getLiteralTextOfNode` instead?
        self.write_literal(node.as_jsx_text().text());
        self.exit_node(node, state);
    }

    pub(crate) fn emit_jsx_attributes(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_list(
            Printer::emit_jsx_attribute_like,
            node,
            Some(node.as_jsx_attributes().properties()),
            ListFormat::JsxElementAttributes,
        );
        self.exit_node(node, state);
    }

    pub(crate) fn emit_jsx_attribute(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_jsx_attribute();
        self.emit_jsx_attribute_name(n.name());
        if let Some(initializer) = n.initializer() {
            self.write_punctuation("=");
            self.emit_jsx_attribute_value(initializer);
        }
        self.exit_node(node, state);
    }

    pub(crate) fn emit_jsx_spread_attribute(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.write_punctuation("{...");
        self.emit_expression(
            node.as_jsx_spread_attribute().expression(),
            OperatorPrecedence::LOWEST,
        );
        self.write_punctuation("}");
        self.exit_node(node, state);
    }

    fn emit_jsx_attribute_like(&mut self, node: P<Node>) {
        match node.kind() {
            Kind::JsxAttribute => self.emit_jsx_attribute(node),
            Kind::JsxSpreadAttribute => self.emit_jsx_spread_attribute(node),
            _ => panic!("unhandled JsxAttributeLike: {:?}", node.kind()),
        }
    }

    pub(crate) fn emit_jsx_expression(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_jsx_expression();
        if n.expression().is_some()
            || !self.comments_disabled
                && !node_is_synthesized(node)
                && self.has_comments_at_position(node.pos())
        {
            // preserve empty expressions if they contain comments!
            let indented = self.current_source_file().is_some()
                && !node_is_synthesized(node)
                && !get_lines_between_positions(
                    self.current_source_file().unwrap(),
                    node.pos(),
                    node.end(),
                )
                .is_same();
            self.increase_indent_if(indented);
            let end =
                self.emit_token(Kind::OpenBraceToken, node.pos(), WriteKind::Punctuation, node);
            self.emit_token_node(n.dot_dot_dot_token());
            if let Some(expression) = n.expression() {
                self.emit_expression(expression, OperatorPrecedence::DISALLOW_COMMA);
            }
            self.emit_token(
                Kind::CloseBraceToken,
                greatest_end(end, &[&n.expression(), &n.dot_dot_dot_token()]),
                WriteKind::Punctuation,
                node,
            );
            self.decrease_indent_if(indented);
        }
        self.exit_node(node, state);
    }

    pub(crate) fn emit_jsx_namespaced_name(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_jsx_namespaced_name();
        self.emit_identifier_name(n.namespace());
        self.write_punctuation(":");
        self.emit_identifier_name(n.name());
        self.exit_node(node, state);
    }

    fn emit_jsx_child(&mut self, node: P<Node>) {
        match node.kind() {
            Kind::JsxText => self.emit_jsx_text(node),
            Kind::JsxExpression => self.emit_jsx_expression(node),
            Kind::JsxElement => self.emit_jsx_element(node),
            Kind::JsxSelfClosingElement => self.emit_jsx_self_closing_element(node),
            Kind::JsxFragment => self.emit_jsx_fragment(node),
            _ => panic!("unhandled JsxChild: {:?}", node.kind()),
        }
    }

    fn emit_jsx_tag_name(&mut self, node: P<Node>) {
        match node.kind() {
            Kind::Identifier => self.emit_identifier_reference(node),
            Kind::ThisKeyword => self.emit_keyword_expression(node),
            Kind::JsxNamespacedName => self.emit_jsx_namespaced_name(node),
            Kind::PropertyAccessExpression => self.emit_property_access_expression(node),
            _ => panic!("unhandled JsxTagName: {:?}", node.kind()),
        }
    }

    fn emit_jsx_attribute_name(&mut self, node: P<Node>) {
        match node.kind() {
            Kind::Identifier => self.emit_identifier_name(node),
            Kind::JsxNamespacedName => self.emit_jsx_namespaced_name(node),
            _ => panic!("unhandled JsxAttributeName: {:?}", node.kind()),
        }
    }

    fn emit_jsx_attribute_value(&mut self, node: P<Node>) {
        match node.kind() {
            Kind::StringLiteral => self.emit_string_literal(node),
            Kind::JsxExpression => self.emit_jsx_expression(node),
            Kind::JsxElement => self.emit_jsx_element(node),
            Kind::JsxSelfClosingElement => self.emit_jsx_self_closing_element(node),
            Kind::JsxFragment => self.emit_jsx_fragment(node),
            _ => self.emit_expression(node, OperatorPrecedence::LOWEST),
        }
    }
}

//
// Clauses
//

impl Printer {
    fn emit_case_or_default_clause_statements(&mut self, node: P<Node>, colon_pos: TextPos) {
        let statements = node.as_case_or_default_clause().statements();
        let emit_as_single_statement = statements.nodes().len() == 1
            // treat synthesized nodes as located on the same line for emit purposes
            && (self.current_source_file().is_none()
                || node_is_synthesized(node)
                || node_is_synthesized(statements.nodes()[0])
                || range_start_positions_are_on_same_line(node.loc(), statements.nodes()[0].loc(), self.current_source_file().unwrap()));

        let mut format = ListFormat::CaseOrDefaultClauseStatements;
        if emit_as_single_statement {
            // When emitting as a single statement, use writeToken (no comments) for the colon
            // to avoid duplicating trailing comments that will be picked up by the statement list.
            self.write_token_text(Kind::ColonToken, WriteKind::Punctuation, colon_pos);
            self.write_space();
            format &= !(ListFormat::MultiLine | ListFormat::Indented);
        } else {
            self.emit_token(Kind::ColonToken, colon_pos, WriteKind::Punctuation, node);
        }

        self.emit_list(Printer::emit_statement, node, Some(statements), format);
    }

    pub(crate) fn emit_case_clause(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let expression = node.as_case_or_default_clause().expression().unwrap();
        self.emit_token(Kind::CaseKeyword, node.pos(), WriteKind::Keyword, node);
        self.write_space();
        self.emit_expression(expression, OperatorPrecedence::LOWEST);
        self.emit_case_or_default_clause_statements(node, expression.end());
        self.exit_node(node, state);
    }

    pub(crate) fn emit_default_clause(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let pos = self.emit_token(Kind::DefaultKeyword, node.pos(), WriteKind::Keyword, node);
        self.emit_case_or_default_clause_statements(node, pos);
        self.exit_node(node, state);
    }

    fn emit_case_or_default_clause_node(&mut self, node: P<Node>) {
        match node.kind() {
            Kind::CaseClause => self.emit_case_clause(node),
            Kind::DefaultClause => self.emit_default_clause(node),
            _ => panic!("unhandled CaseOrDefaultClause: {:?}", node.kind()),
        }
    }

    pub(crate) fn emit_heritage_clause(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_heritage_clause();
        self.write_space();
        self.emit_token(n.token, node.pos(), WriteKind::Keyword, node);
        self.write_space();
        self.emit_list(
            Printer::emit_heritage_clause_element,
            node,
            Some(n.types()),
            ListFormat::HeritageClauseTypes,
        );
        self.exit_node(node, state);
    }

    fn emit_heritage_clause_element(&mut self, node: P<Node>) {
        match node.kind() {
            Kind::ExpressionWithTypeArguments => self.emit_expression_with_type_arguments(node),
            Kind::TypeReference => self.emit_type_reference(node),
            _ => panic!("unhandled HeritageClauseElement: {:?}", node.kind()),
        }
    }

    fn emit_heritage_clause_node(&mut self, node: P<Node>) {
        self.emit_heritage_clause(node);
    }

    pub(crate) fn emit_catch_clause(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_catch_clause();
        let open_paren_pos =
            self.emit_token(Kind::CatchKeyword, node.pos(), WriteKind::Keyword, node);
        self.write_space();

        if let Some(variable_declaration) = n.variable_declaration() {
            self.emit_token(Kind::OpenParenToken, open_paren_pos, WriteKind::Punctuation, node);
            self.emit_variable_declaration(variable_declaration);
            self.emit_token(
                Kind::CloseParenToken,
                variable_declaration.end(),
                WriteKind::Punctuation,
                node,
            );
            self.write_space();
        }

        self.emit_block(n.block());
        self.exit_node(node, state);
    }
}

//
// Property assignments
//

impl Printer {
    pub(crate) fn emit_property_assignment(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_property_assignment();
        self.emit_property_name(Some(n.name()));
        self.write_punctuation(":");
        self.write_space();
        // This is to ensure that we emit comment in the following case:
        //      For example:
        //          obj = {
        //              id: /*comment1*/ ()=>void
        //          }
        // "comment1" is not considered to be leading comment for node.initializer
        // but rather a trailing comment on the previous node.
        let initializer = n.initializer();
        if !self.emit_context.emit_flags(initializer).intersects(EmitFlags::NoLeadingComments) {
            let comment_range = self.emit_context.comment_range(initializer);
            self.emit_trailing_comments(comment_range.pos(), CommentSeparator::After);
        }
        self.emit_expression(initializer, OperatorPrecedence::DISALLOW_COMMA);
        self.exit_node(node, state);
    }

    pub(crate) fn emit_shorthand_property_assignment(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_shorthand_property_assignment();
        self.emit_property_name(Some(n.name()));
        if let Some(object_assignment_initializer) = n.object_assignment_initializer() {
            self.write_space();
            self.write_punctuation("=");
            self.write_space();
            self.emit_expression(object_assignment_initializer, OperatorPrecedence::DISALLOW_COMMA);
        }
        self.exit_node(node, state);
    }

    pub(crate) fn emit_spread_assignment(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        // Go checks `node.Expression != nil`; the field is never nil here.
        let expression = node.as_spread_assignment().expression();
        self.emit_token(Kind::DotDotDotToken, node.pos(), WriteKind::Punctuation, node);
        self.emit_expression(expression, OperatorPrecedence::DISALLOW_COMMA);
        self.exit_node(node, state);
    }
}

//
// Enum
//

impl Printer {
    pub(crate) fn emit_enum_member(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_enum_member();
        self.emit_property_name(Some(n.name()));
        self.emit_initializer(n.initializer(), n.name().end(), node);
        self.exit_node(node, state);
    }

    fn emit_enum_member_node(&mut self, node: P<Node>) {
        self.emit_enum_member(node);
    }
}

//
// JSDoc
//

impl Printer {
    pub(crate) fn emit_jsdoc_node(_node: P<Node>) {
        // !!!
        panic!("not implemented");
    }
}

//
// Top-level nodes
//

impl Printer {
    fn emit_shebang_if_needed(&mut self, node: P<SourceFile>) {
        if node_is_synthesized(node.as_node()) {
            return;
        }
        let shebang = scanner::get_shebang(node.text());
        if !shebang.is_empty() {
            self.write_comment(shebang);
            self.write_line();
        }
    }

    pub(crate) fn emit_prologue_directives(&mut self, statements: P<NodeList>) -> usize {
        for (i, statement) in statements.nodes().iter().enumerate() {
            if is_prologue_directive(*statement) {
                self.write_line();
                self.emit_statement(*statement);
            } else {
                return i;
            }
        }
        statements.nodes().len()
    }

    pub(crate) fn emit_helpers(&mut self, node: P<Node>) -> bool {
        let mut helpers_emitted = false;
        let source_file = self.current_source_file();
        let should_skip = self.options.no_emit_helpers
            || source_file.is_some_and(|f| self.emit_context.has_recorded_external_helpers(f));
        let mut helpers = self.emit_context.get_emit_helpers(node);
        if !helpers.is_empty() {
            helpers.sort_by(|a, b| compare_emit_helpers(*a, *b));
            for helper in helpers {
                if !helper.scoped {
                    // Skip the helper if it can be skipped and the noEmitHelpers compiler
                    // option is set, or if it can be imported and the importHelpers compiler
                    // option is set.
                    if should_skip {
                        continue;
                    }
                }
                if let Some(text_callback) = helper.text_callback {
                    let text = text_callback(&mut |name: &str| {
                        self.make_file_level_optimistic_unique_name(name)
                    });
                    self.write_lines(&text);
                } else {
                    self.write_lines(helper.text);
                }
                helpers_emitted = true;
            }
        }

        helpers_emitted
    }

    fn make_file_level_optimistic_unique_name(&mut self, name: &str) -> String {
        self.name_generator.make_file_level_optimistic_unique_name(name)
    }

    // Go `(*Printer).emitSourceFile`; suffixed because the exported `EmitSourceFile` takes the plain snake_case name.
    pub(crate) fn emit_source_file_(&mut self, node: P<Node>) {
        let file = node.as_source_file_p();
        let saved_current_source_file = self.current_source_file();
        let saved_comments_disabled = self.comments_disabled;
        self.set_current_source_file(Some(file));

        self.write_line();

        self.push_name_generation_scope(Some(node));
        self.generate_all_names(Some(file.statements));

        let mut index = 0;
        let state;
        if file.script_kind() == ScriptKind::JSON {
            state = self.emit_detached_comments_before_statement_list(node, file.statements.loc());
        } else {
            self.emit_shebang_if_needed(file);
            index = self.emit_prologue_directives(file.statements);
            if !self.writer().is_at_start_of_line() {
                self.write_line();
            }
            state = self.emit_detached_comments_before_statement_list(node, file.statements.loc());
            self.emit_helpers(node);
            if file.is_declaration_file() {
                self.emit_triple_slash_directives(file);
            }
        }

        // !!! Emit triple-slash directives
        self.emit_list_range(
            Printer::emit_statement,
            Some(node),
            Some(file.statements),
            ListFormat::MultiLine,
            index,
            None,
        );
        self.pop_name_generation_scope(Some(node));
        self.emit_detached_comments_after_statement_list(node, file.statements.loc(), state);
        self.set_current_source_file(saved_current_source_file);
        self.comments_disabled = saved_comments_disabled;
    }

    fn emit_triple_slash_directives(&mut self, node: P<SourceFile>) {
        self.emit_directive("path", node.referenced_files());
        self.emit_directive("types", node.type_reference_directives());
        self.emit_directive("lib", node.lib_reference_directives());
    }

    fn emit_directive(&mut self, kind: &str, refs: &[P<FileReference>]) {
        for r in refs {
            let mut resolution_mode = String::new();
            if r.resolution_mode != RESOLUTION_MODE_NONE {
                resolution_mode = format!(
                    "resolution-mode=\"{}\" ",
                    if r.resolution_mode == RESOLUTION_MODE_ESM { "import" } else { "require" }
                );
            }
            self.write_comment(&format!(
                "/// <reference {}=\"{}\" {}{}/>",
                kind,
                r.file_name,
                resolution_mode,
                if r.preserve { "preserve=\"true\" " } else { "" }
            ));
            self.write_line();
        }
    }
}
