use std::fmt::Display;

use rustc_hash::FxHashMap;
use tsrs_ast as ast;
use tsrs_ast::{
    DiagnosticExt, Kind, ModifierFlags, ModifierList, Node, NodeFlags, NodeList,
    OperatorPrecedence, TokenFlags,
};
use tsrs_core::{LanguageVariant, P, TextRange, Tristate};
use tsrs_diagnostics as diagnostics;
use tsrs_diagnostics::Message;
use tsrs_scanner as scanner;

use crate::parser_1::{
    JsdocScannerInfo, Parser, ParsingContext, is_export_modifier, is_missing_node_list,
    modifier_list_has_async,
};
use crate::types::ParseFlags;
use crate::utilities::{is_keyword_or_punctuation, token_is_identifier_or_keyword};

impl Parser {
    pub(crate) fn parse_assignment_expression_or_higher(&mut self) -> P<Node> {
        self.parse_assignment_expression_or_higher_worker(
            true, /*allowReturnTypeInArrowFunction*/
        )
    }

    fn parse_assignment_expression_or_higher_worker(
        &mut self,
        allow_return_type_in_arrow_function: bool,
    ) -> P<Node> {
        //  AssignmentExpression[in,yield]:
        //      1) ConditionalExpression[?in,?yield]
        //      2) LeftHandSideExpression = AssignmentExpression[?in,?yield]
        //      3) LeftHandSideExpression AssignmentOperator AssignmentExpression[?in,?yield]
        //      4) ArrowFunctionExpression[?in,?yield]
        //      5) AsyncArrowFunctionExpression[in,yield,await]
        //      6) [+Yield] YieldExpression[?In]
        //
        // Note: for ease of implementation we treat productions '2' and '3' as the same thing.
        // (i.e. they're both BinaryExpressions with an assignment operator in it).
        // First, do the simple check if we have a YieldExpression (production '6').
        if self.is_yield_expression() {
            return self.parse_yield_expression();
        }
        // Then, check if we have an arrow function (production '4' and '5') that starts with a parenthesized
        // parameter list or is an async arrow function.
        // AsyncArrowFunctionExpression:
        //      1) async[no LineTerminator here]AsyncArrowBindingIdentifier[?Yield][no LineTerminator here]=>AsyncConciseBody[?In]
        //      2) CoverCallExpressionAndAsyncArrowHead[?Yield, ?Await][no LineTerminator here]=>AsyncConciseBody[?In]
        // Production (1) of AsyncArrowFunctionExpression is parsed in "tryParseAsyncSimpleArrowFunctionExpression".
        // And production (2) is parsed in "tryParseParenthesizedArrowFunctionExpression".
        //
        // If we do successfully parse arrow-function, we must *not* recurse for productions 1, 2 or 3. An ArrowFunction is
        // not a LeftHandSideExpression, nor does it start a ConditionalExpression.  So we are done
        // with AssignmentExpression if we see one.
        if let Some(arrow_expression) = self
            .try_parse_parenthesized_arrow_function_expression(allow_return_type_in_arrow_function)
        {
            return arrow_expression;
        }
        if let Some(arrow_expression) = self
            .try_parse_async_simple_arrow_function_expression(allow_return_type_in_arrow_function)
        {
            return arrow_expression;
        }
        // Now try to see if we're in production '1', '2' or '3'.  A conditional expression can
        // start with a LogicalOrExpression, while the assignment productions can only start with
        // LeftHandSideExpressions.
        //
        // So, first, we try to just parse out a BinaryExpression.  If we get something that is a
        // LeftHandSide or higher, then we can try to parse out the assignment expression part.
        // Otherwise, we try to parse out the conditional expression bit.  We want to allow any
        // binary expression here, so we pass in the 'lowest' precedence here so that it matches
        // and consumes anything.
        let pos = self.node_pos();
        let jsdoc = self.jsdoc_scanner_info();
        let expr = self.parse_binary_expression_or_higher(OperatorPrecedence::LOWEST);
        // To avoid a look-ahead, we did not handle the case of an arrow function with a single un-parenthesized
        // parameter ('x => ...') above. We handle it here by checking if the parsed expression was a single
        // identifier and the current token is an arrow.
        if expr.kind() == Kind::Identifier && self.token == Kind::EqualsGreaterThanToken {
            return self.parse_simple_arrow_function_expression(
                pos,
                expr,
                allow_return_type_in_arrow_function,
                jsdoc,
                None, /*asyncModifier*/
            );
        }
        // Now see if we might be in cases '2' or '3'.
        // If the expression was a LHS expression, and we have an assignment operator, then
        // we're in '2' or '3'. Consume the assignment and return.
        //
        // Note: we call reScanGreaterToken so that we get an appropriately merged token
        // for cases like `> > =` becoming `>>=`
        if ast::is_left_hand_side_expression(expr)
            && ast::is_assignment_operator(self.re_scan_greater_than_token())
        {
            let operator_token = self.parse_token_node();
            let right = self
                .parse_assignment_expression_or_higher_worker(allow_return_type_in_arrow_function);
            return self.make_binary_expression(expr, operator_token, right, pos);
        }
        // It wasn't an assignment or a lambda.  This is a conditional expression:
        self.parse_conditional_expression_rest(expr, pos, allow_return_type_in_arrow_function)
    }

    fn is_yield_expression(&mut self) -> bool {
        if self.token == Kind::YieldKeyword {
            // If we have a 'yield' keyword, and this is a context where yield expressions are
            // allowed, then definitely parse out a yield expression.
            if self.in_yield_context() {
                return true;
            }

            // We're in a context where 'yield expr' is not allowed.  However, if we can
            // definitely tell that the user was trying to parse a 'yield expr' and not
            // just a normal expr that start with a 'yield' identifier, then parse out
            // a 'yield expr'.  We can then report an error later that they are only
            // allowed in generator expressions.
            //
            // for example, if we see 'yield(foo)', then we'll have to treat that as an
            // invocation expression of something called 'yield'.  However, if we have
            // 'yield foo' then that is not legal as a normal expression, so we can
            // definitely recognize this as a yield expression.
            //
            // for now we just check if the next token is an identifier.  More heuristics
            // can be added here later as necessary.  We just need to make sure that we
            // don't accidentally consume something legal.
            return self
                .look_ahead(Parser::next_token_is_identifier_or_keyword_or_literal_on_same_line);
        }
        false
    }

    fn parse_yield_expression(&mut self) -> P<Node> {
        let pos = self.node_pos();
        // YieldExpression[In] :
        //      yield
        //      yield [no LineTerminator here] [Lexical goal InputElementRegExp]AssignmentExpression[?In, Yield]
        //      yield [no LineTerminator here] * [Lexical goal InputElementRegExp]AssignmentExpression[?In, Yield]
        self.next_token();

        let result = if !self.has_preceding_line_break()
            && (self.token == Kind::AsteriskToken || self.is_start_of_expression())
        {
            let asterisk_token = self.parse_optional_token(Kind::AsteriskToken);
            let expression = self.parse_assignment_expression_or_higher();
            self.factory.new_yield_expression(asterisk_token, Some(expression))
        } else {
            // if the next token is not on the same line as yield.  or we don't have an '*' or
            // the start of an expression, then this is just a simple "yield" expression.
            self.factory.new_yield_expression(None /*asteriskToken*/, None /*expression*/)
        };
        self.finish_node(result, pos)
    }

    fn is_parenthesized_arrow_function_expression(&mut self) -> Tristate {
        if self.token == Kind::OpenParenToken
            || self.token == Kind::LessThanToken
            || self.token == Kind::AsyncKeyword
        {
            let state = self.mark();
            let result = self.next_is_parenthesized_arrow_function_expression();
            self.rewind(state);
            return result;
        }
        if self.token == Kind::EqualsGreaterThanToken {
            // ERROR RECOVERY TWEAK:
            // If we see a standalone => try to parse it as an arrow function expression as that's
            // likely what the user intended to write.
            return Tristate::True;
        }
        // Definitely not a parenthesized arrow function.
        Tristate::False
    }

    fn next_is_parenthesized_arrow_function_expression(&mut self) -> Tristate {
        if self.token == Kind::AsyncKeyword {
            self.next_token();
            if self.has_preceding_line_break() {
                return Tristate::False;
            }
            if self.token != Kind::OpenParenToken && self.token != Kind::LessThanToken {
                return Tristate::False;
            }
        }
        let first = self.token;
        let second = self.next_token();
        if first == Kind::OpenParenToken {
            if second == Kind::CloseParenToken {
                // Simple cases: "() =>", "(): ", and "() {".
                // This is an arrow function with no parameters.
                // The last one is not actually an arrow function,
                // but this is probably what the user intended.
                let third = self.next_token();
                match third {
                    Kind::EqualsGreaterThanToken | Kind::ColonToken | Kind::OpenBraceToken => {
                        return Tristate::True;
                    }
                    _ => {}
                }
                return Tristate::False;
            }
            // If encounter "([" or "({", this could be the start of a binding pattern.
            // Examples:
            //      ([ x ]) => { }
            //      ({ x }) => { }
            //      ([ x ])
            //      ({ x })
            if second == Kind::OpenBracketToken || second == Kind::OpenBraceToken {
                return Tristate::Unknown;
            }
            // Simple case: "(..."
            // This is an arrow function with a rest parameter.
            if second == Kind::DotDotDotToken {
                return Tristate::True;
            }
            // Check for "(xxx yyy", where xxx is a modifier and yyy is an identifier. This
            // isn't actually allowed, but we want to treat it as a lambda so we can provide
            // a good error message.
            if ast::is_modifier_kind(second)
                && second != Kind::AsyncKeyword
                && self.look_ahead(Parser::next_token_is_identifier)
            {
                if self.next_token() == Kind::AsKeyword {
                    // https://github.com/microsoft/TypeScript/issues/44466
                    return Tristate::False;
                }
                return Tristate::True;
            }
            // If we had "(" followed by something that's not an identifier,
            // then this definitely doesn't look like a lambda.  "this" is not
            // valid, but we want to parse it and then give a semantic error.
            if !self.is_identifier() && second != Kind::ThisKeyword {
                return Tristate::False;
            }
            match self.next_token() {
                Kind::ColonToken => {
                    // If we have something like "(a:", then we must have a
                    // type-annotated parameter in an arrow function expression.
                    return Tristate::True;
                }
                Kind::QuestionToken => {
                    self.next_token();
                    // If we have "(a?:" or "(a?," or "(a?=" or "(a?)" then it is definitely a lambda.
                    if self.token == Kind::ColonToken
                        || self.token == Kind::CommaToken
                        || self.token == Kind::EqualsToken
                        || self.token == Kind::CloseParenToken
                    {
                        return Tristate::True;
                    }
                    // Otherwise it is definitely not a lambda.
                    return Tristate::False;
                }
                Kind::CommaToken | Kind::EqualsToken | Kind::CloseParenToken => {
                    // If we have "(a," or "(a=" or "(a)" this *could* be an arrow function
                    return Tristate::Unknown;
                }
                _ => {}
            }
            // It is definitely not an arrow function
            Tristate::False
        } else {
            assert_eq!(first, Kind::LessThanToken);
            // If we have "<" not followed by an identifier,
            // then this definitely is not an arrow function.
            if !self.is_identifier() && self.token != Kind::ConstKeyword {
                return Tristate::False;
            }
            // JSX overrides
            if self.language_variant == LanguageVariant::JSX {
                let is_arrow_function_in_jsx = self.look_ahead(|p| {
                    p.parse_optional(Kind::ConstKeyword);
                    let third = p.next_token();
                    if third == Kind::ExtendsKeyword {
                        let fourth = p.next_token();
                        match fourth {
                            Kind::EqualsToken | Kind::GreaterThanToken | Kind::SlashToken => {
                                return false;
                            }
                            _ => {}
                        }
                        return true;
                    } else if third == Kind::CommaToken || third == Kind::EqualsToken {
                        return true;
                    }
                    false
                });
                if is_arrow_function_in_jsx {
                    return Tristate::True;
                }
                return Tristate::False;
            }
            // This *could* be a parenthesized arrow function.
            Tristate::Unknown
        }
    }

    #[inline]
    fn try_parse_parenthesized_arrow_function_expression(
        &mut self,
        allow_return_type_in_arrow_function: bool,
    ) -> Option<P<Node>> {
        // Keep the speculative snapshot in the worker, including standalone `=>` for error recovery.
        if !matches!(
            self.token,
            Kind::OpenParenToken
                | Kind::LessThanToken
                | Kind::AsyncKeyword
                | Kind::EqualsGreaterThanToken
        ) {
            return None;
        }
        self.try_parse_parenthesized_arrow_function_expression_worker(
            allow_return_type_in_arrow_function,
        )
    }

    #[inline(never)]
    fn try_parse_parenthesized_arrow_function_expression_worker(
        &mut self,
        allow_return_type_in_arrow_function: bool,
    ) -> Option<P<Node>> {
        let tristate = self.is_parenthesized_arrow_function_expression();
        if tristate == Tristate::False {
            // It's definitely not a parenthesized arrow function expression.
            return None;
        }
        // If we definitely have an arrow function, then we can just parse one, not requiring a
        // following => or { token. Otherwise, we *might* have an arrow function.  Try to parse
        // it out, but don't allow any ambiguity, and return 'undefined' if this could be an
        // expression instead.
        if tristate == Tristate::True {
            return self.parse_parenthesized_arrow_function_expression(
                true, /*allowAmbiguity*/
                true, /*allowReturnTypeInArrowFunction*/
            );
        }
        let state = self.mark();
        let result = self.parse_possible_parenthesized_arrow_function_expression(
            allow_return_type_in_arrow_function,
        );
        if result.is_none() {
            self.rewind(state);
        }
        result
    }

    fn parse_parenthesized_arrow_function_expression(
        &mut self,
        allow_ambiguity: bool,
        allow_return_type_in_arrow_function: bool,
    ) -> Option<P<Node>> {
        let pos = self.node_pos();
        let jsdoc = self.jsdoc_scanner_info();
        let modifiers = self.parse_modifiers_for_arrow_function();
        let is_async = modifier_list_has_async(modifiers);
        let signature_flags = if is_async { ParseFlags::Await } else { ParseFlags::None };
        // Arrow functions are never generators.
        //
        // If we're speculatively parsing a signature for a parenthesized arrow function, then
        // we have to have a complete parameter list.  Otherwise we might see something like
        // a => (b => c)
        // And think that "(b =>" was actually a parenthesized arrow function with a missing
        // close paren.
        let type_parameters = self.parse_type_parameters();
        let parameters: P<NodeList>;
        if self.parse_expected(Kind::OpenParenToken) {
            if allow_ambiguity {
                parameters =
                    self.parse_parameters_worker(signature_flags, allow_ambiguity).unwrap();
            } else {
                let maybe_parameters =
                    self.parse_parameters_worker(signature_flags, allow_ambiguity);
                {
                    let maybe_parameters = maybe_parameters?;
                    parameters = maybe_parameters;
                }
            }
            if !self.parse_expected(Kind::CloseParenToken) && !allow_ambiguity {
                return None;
            }
        } else {
            if !allow_ambiguity {
                return None;
            }
            parameters = self.create_missing_list();
        }
        let has_return_colon = self.token == Kind::ColonToken;
        let return_type = self.parse_return_type(Kind::ColonToken, false /*isType*/);
        if let Some(return_type) = return_type
            && !allow_ambiguity
            && type_has_arrow_function_blocking_parse_error(return_type)
        {
            return None;
        }
        // Parsing a signature isn't enough.
        // Parenthesized arrow signatures often look like other valid expressions.
        // For instance:
        //  - "(x = 10)" is an assignment expression parsed as a signature with a default parameter value.
        //  - "(x,y)" is a comma expression parsed as a signature with two parameters.
        //  - "a ? (b): c" will have "(b):" parsed as a signature with a return type annotation.
        //  - "a ? (b): function() {}" will too, since function() is a valid JSDoc function type.
        //  - "a ? (b): (function() {})" as well, but inside of a parenthesized type with an arbitrary amount of nesting.
        //
        // So we need just a bit of lookahead to ensure that it can only be a signature.
        let mut unwrapped_type = return_type;
        while let Some(t) = unwrapped_type {
            if t.kind() != Kind::ParenthesizedType {
                break;
            }
            unwrapped_type = t.type_node(); // Skip parens if need be
        }
        if !allow_ambiguity
            && self.token != Kind::EqualsGreaterThanToken
            && self.token != Kind::OpenBraceToken
        {
            // Returning undefined here will cause our caller to rewind to where we started from.
            return None;
        }
        // If we have an arrow, then try to parse the body. Even if not, try to parse if we
        // have an opening brace, just in case we're in an error state.
        let last_token = self.token;
        let equals_greater_than_token = self.parse_expected_token(Kind::EqualsGreaterThanToken);
        let body = if last_token == Kind::EqualsGreaterThanToken
            || last_token == Kind::OpenBraceToken
        {
            self.parse_arrow_function_expression_body(is_async, allow_return_type_in_arrow_function)
        } else {
            self.parse_identifier()
        };
        // Given:
        //     x ? y => ({ y }) : z => ({ z })
        // We try to parse the body of the first arrow function by looking at:
        //     ({ y }) : z => ({ z })
        // This is a valid arrow function with "z" as the return type.
        //
        // But, if we're in the true side of a conditional expression, this colon
        // terminates the expression, so we cannot allow a return type if we aren't
        // certain whether or not the preceding text was parsed as a parameter list.
        //
        // For example,
        //     a() ? (b: number, c?: string): void => d() : e
        // is determined by isParenthesizedArrowFunctionExpression to unambiguously
        // be an arrow expression, so we allow a return type.
        if !allow_return_type_in_arrow_function && has_return_colon {
            // However, if the arrow function we were able to parse is followed by another colon
            // as in:
            //     a ? (x): string => x : null
            // Then allow the arrow function, and treat the second colon as terminating
            // the conditional expression. It's okay to do this because this code would
            // be a syntax error in JavaScript (as the second colon shouldn't be there).
            if self.token != Kind::ColonToken {
                return None;
            }
        }
        let node = self.factory.new_arrow_function(
            modifiers,
            type_parameters,
            Some(parameters),
            return_type,
            None, /*fullSignature*/
            Some(equals_greater_than_token),
            Some(body),
        );
        let result = self.finish_node(node, pos);
        self.with_jsdoc(result, jsdoc);
        self.check_js_syntax(result);
        Some(result)
    }

    fn parse_modifiers_for_arrow_function(&mut self) -> Option<P<ModifierList>> {
        if self.token == Kind::AsyncKeyword {
            let pos = self.node_pos();
            self.next_token();
            let node = self.factory.new_modifier(Kind::AsyncKeyword);
            let modifier = self.finish_node(node, pos);
            return Some(self.new_modifier_list(modifier.loc(), &[modifier]));
        }
        None
    }

    fn parse_arrow_function_expression_body(
        &mut self,
        is_async: bool,
        allow_return_type_in_arrow_function: bool,
    ) -> P<Node> {
        if self.token == Kind::OpenBraceToken {
            return self.parse_function_block(
                if is_async { ParseFlags::Await } else { ParseFlags::None },
                None, /*diagnosticMessage*/
            );
        }
        if self.token != Kind::SemicolonToken
            && self.token != Kind::FunctionKeyword
            && self.token != Kind::ClassKeyword
            && self.is_start_of_statement()
            && !self.is_start_of_expression_statement()
        {
            // Check if we got a plain statement (i.e. no expression-statements, no function/class expressions/declarations)
            //
            // Here we try to recover from a potential error situation in the case where the
            // user meant to supply a block. For example, if the user wrote:
            //
            //  a =>
            //      let v = 0;
            //  }
            //
            // they may be missing an open brace.  Check to see if that's the case so we can
            // try to recover better.  If we don't do this, then the next close curly we see may end
            // up preemptively closing the containing construct.
            //
            // Note: even when 'IgnoreMissingOpenBrace' is passed, parseBody will still error.
            return self.parse_function_block(
                ParseFlags::IgnoreMissingOpenBrace
                    | if is_async { ParseFlags::Await } else { ParseFlags::None },
                None, /*diagnosticMessage*/
            );
        }
        let save_context_flags = self.context_flags;
        self.set_context_flags(NodeFlags::AwaitContext, is_async);
        self.set_context_flags(NodeFlags::YieldContext, false);
        let node =
            self.parse_assignment_expression_or_higher_worker(allow_return_type_in_arrow_function);
        self.context_flags = save_context_flags;
        node
    }

    fn is_start_of_expression_statement(&mut self) -> bool {
        // As per the grammar, none of '{' or 'function' or 'class' can start an expression statement.
        self.token != Kind::OpenBraceToken
            && self.token != Kind::FunctionKeyword
            && self.token != Kind::ClassKeyword
            && self.token != Kind::AtToken
            && self.is_start_of_expression()
    }

    fn parse_possible_parenthesized_arrow_function_expression(
        &mut self,
        allow_return_type_in_arrow_function: bool,
    ) -> Option<P<Node>> {
        let token_pos = self.scanner.token_start();
        if self.not_parenthesized_arrow.contains(&token_pos) {
            return None;
        }
        let result = self.parse_parenthesized_arrow_function_expression(
            false, /*allowAmbiguity*/
            allow_return_type_in_arrow_function,
        );
        if result.is_none() {
            self.not_parenthesized_arrow.insert(token_pos);
        }
        result
    }

    fn try_parse_async_simple_arrow_function_expression(
        &mut self,
        allow_return_type_in_arrow_function: bool,
    ) -> Option<P<Node>> {
        // We do a check here so that we won't be doing unnecessarily call to "lookAhead"
        if self.token == Kind::AsyncKeyword
            && self.look_ahead(Parser::next_is_un_parenthesized_async_arrow_function)
        {
            let pos = self.node_pos();
            let jsdoc = self.jsdoc_scanner_info();
            let async_modifier = self.parse_modifiers_for_arrow_function();
            let expr = self.parse_binary_expression_or_higher(OperatorPrecedence::LOWEST);
            return Some(self.parse_simple_arrow_function_expression(
                pos,
                expr,
                allow_return_type_in_arrow_function,
                jsdoc,
                async_modifier,
            ));
        }
        None
    }

    fn next_is_un_parenthesized_async_arrow_function(&mut self) -> bool {
        // AsyncArrowFunctionExpression:
        //      1) async[no LineTerminator here]AsyncArrowBindingIdentifier[?Yield][no LineTerminator here]=>AsyncConciseBody[?In]
        //      2) CoverCallExpressionAndAsyncArrowHead[?Yield, ?Await][no LineTerminator here]=>AsyncConciseBody[?In]
        if self.token == Kind::AsyncKeyword {
            self.next_token();
            // If the "async" is followed by "=>" token then it is not a beginning of an async arrow-function
            // but instead a simple arrow-function which will be parsed inside "parseAssignmentExpressionOrHigher"
            if self.has_preceding_line_break() || self.token == Kind::EqualsGreaterThanToken {
                return false;
            }
            // Check for un-parenthesized AsyncArrowFunction
            if !self.is_identifier() {
                return false;
            }
            self.next_token_without_check();
            return !self.has_preceding_line_break() && self.token == Kind::EqualsGreaterThanToken;
        }
        false
    }

    fn parse_simple_arrow_function_expression(
        &mut self,
        pos: u32,
        identifier: P<Node>,
        allow_return_type_in_arrow_function: bool,
        jsdoc: JsdocScannerInfo,
        async_modifier: Option<P<ModifierList>>,
    ) -> P<Node> {
        assert!(
            self.token == Kind::EqualsGreaterThanToken,
            "parseSimpleArrowFunctionExpression should only have been called if we had a =>"
        );
        let node = self.factory.new_parameter_declaration(
            None, /*modifiers*/
            None, /*dotDotDotToken*/
            identifier, None, /*questionToken*/
            None, /*typeNode*/
            None, /*initializer*/
        );
        let parameter = self.finish_node(
            node,
            identifier.pos().expect_source("parsed nodes have source positions"),
        );
        let parameters = self.new_node_list(parameter.loc(), &[parameter]);
        let equals_greater_than_token = self.parse_expected_token(Kind::EqualsGreaterThanToken);
        let body = self.parse_arrow_function_expression_body(
            async_modifier.is_some(), /*isAsync*/
            allow_return_type_in_arrow_function,
        );
        let node = self.factory.new_arrow_function(
            async_modifier,
            None, /*typeParameters*/
            Some(parameters),
            None, /*returnType*/
            None, /*fullSignature*/
            Some(equals_greater_than_token),
            Some(body),
        );
        let result = self.finish_node(node, pos);
        self.with_jsdoc(result, jsdoc);
        result
    }

    fn parse_conditional_expression_rest(
        &mut self,
        left_operand: P<Node>,
        pos: u32,
        allow_return_type_in_arrow_function: bool,
    ) -> P<Node> {
        // Note: we are passed in an expression which was produced from parseBinaryExpressionOrHigher.
        let Some(question_token) = self.parse_optional_token(Kind::QuestionToken) else {
            return left_operand;
        };
        // Note: we explicitly 'allowIn' in the whenTrue part of the condition expression, and
        // we do not that for the 'whenFalse' part.
        let save_context_flags = self.context_flags;
        self.set_context_flags(NodeFlags::DisallowInContext, false);
        let true_expression = self.parse_assignment_expression_or_higher_worker(
            false, /*allowReturnTypeInArrowFunction*/
        );
        self.context_flags = save_context_flags;
        let colon_token = self.parse_expected_token(Kind::ColonToken);
        let false_expression = if ast::node_is_present(Some(colon_token)) {
            self.parse_assignment_expression_or_higher_worker(allow_return_type_in_arrow_function)
        } else {
            self.create_missing_identifier()
        };
        let node = self.factory.new_conditional_expression(
            left_operand,
            question_token,
            true_expression,
            colon_token,
            false_expression,
        );
        self.finish_node(node, pos)
    }

    fn parse_binary_expression_or_higher(&mut self, precedence: OperatorPrecedence) -> P<Node> {
        let pos = self.node_pos();
        let left_operand = self.parse_unary_expression_or_higher();
        self.parse_binary_expression_rest(precedence, left_operand, pos)
    }

    fn parse_binary_expression_rest(
        &mut self,
        precedence: OperatorPrecedence,
        left_operand: P<Node>,
        pos: u32,
    ) -> P<Node> {
        let mut left_operand = left_operand;
        let mut last_operand = left_operand;
        loop {
            // We either have a binary operator here, or we're finished.  We call
            // reScanGreaterToken so that we merge token sequences like > and = into >=
            let operator = self.re_scan_greater_than_token();
            let new_precedence = ast::get_binary_operator_precedence(operator);
            // Check the precedence to see if we should "take" this operator
            // - For left associative operator (all operator but **), consume the operator,
            //   recursively call the function below, and parse binaryExpression as a rightOperand
            //   of the caller if the new precedence of the operator is greater then or equal to the current precedence.
            //   For example:
            //      a - b - c;
            //            ^token; leftOperand = b. Return b to the caller as a rightOperand
            //      a * b - c
            //            ^token; leftOperand = b. Return b to the caller as a rightOperand
            //      a - b * c;
            //            ^token; leftOperand = b. Return b * c to the caller as a rightOperand
            // - For right associative operator (**), consume the operator, recursively call the function
            //   and parse binaryExpression as a rightOperand of the caller if the new precedence of
            //   the operator is strictly grater than the current precedence
            //   For example:
            //      a ** b ** c;
            //             ^^token; leftOperand = b. Return b ** c to the caller as a rightOperand
            //      a - b ** c;
            //            ^^token; leftOperand = b. Return b ** c to the caller as a rightOperand
            //      a ** b - c
            //             ^token; leftOperand = b. Return b to the caller as a rightOperand
            if !should_consume_binary_operator(operator, new_precedence, precedence) {
                break;
            }
            if operator == Kind::InKeyword && self.in_disallow_in_context() {
                break;
            }
            if operator == Kind::AsKeyword || operator == Kind::SatisfiesKeyword {
                // Make sure we *do* perform ASI for constructs like this:
                //    var x = foo
                //    as (Bar)
                // This should be parsed as an initialized variable, followed
                // by a function call to 'as' with the argument 'Bar'
                if self.has_preceding_line_break() {
                    break;
                }
                self.next_token();
                // When we have 'a ## b as SomeType $$ c' or 'a ## b satisfies SomeType $$ c', where ## and $$
                // are binary operators, we want to stop parsing when $$ would bind before ## after erasing the
                // assertion. See https://github.com/microsoft/TypeScript/issues/63527.
                let mut last_precedence = OperatorPrecedence::HIGHEST;
                if ast::is_binary_expression(last_operand) {
                    last_precedence = ast::get_binary_operator_precedence(
                        last_operand.as_binary_expression().operator_token.kind(),
                    );
                }
                if operator == Kind::SatisfiesKeyword {
                    let type_node = self.parse_type();
                    left_operand = self.make_satisfies_expression(left_operand, type_node);
                } else {
                    let type_node = self.parse_type();
                    left_operand = self.make_as_expression(left_operand, type_node);
                }
                // Stop if the next operator would bind before the last operator when the assertion is erased.
                let next_operator = self.re_scan_greater_than_token();
                let next_precedence = ast::get_binary_operator_precedence(next_operator);
                if should_consume_binary_operator(next_operator, next_precedence, last_precedence) {
                    break;
                }
            } else {
                let operator_token = self.parse_token_node();
                let right = self.parse_binary_expression_or_higher(new_precedence);
                left_operand =
                    self.make_binary_expression(left_operand, operator_token, right, pos);
                last_operand = left_operand;
            }
        }
        left_operand
    }

    fn make_satisfies_expression(&mut self, expression: P<Node>, type_node: P<Node>) -> P<Node> {
        let node = self.factory.new_satisfies_expression(expression, type_node);
        let node = self.finish_node(
            node,
            expression.pos().expect_source("parsed nodes have source positions"),
        );
        self.check_js_syntax(node)
    }

    fn make_as_expression(&mut self, left: P<Node>, right: P<Node>) -> P<Node> {
        let node = self.factory.new_as_expression(left, right);
        let node =
            self.finish_node(node, left.pos().expect_source("parsed nodes have source positions"));
        self.check_js_syntax(node)
    }

    pub(crate) fn make_binary_expression(
        &mut self,
        left: P<Node>,
        operator_token: P<Node>,
        right: P<Node>,
        pos: u32,
    ) -> P<Node> {
        let node = self.factory.new_binary_expression(
            None, /*modifiers*/
            left,
            None, /*typeNode*/
            operator_token,
            right,
        );
        self.finish_node(node, pos)
    }

    #[inline]
    pub(crate) fn parse_unary_expression_or_higher(&mut self) -> P<Node> {
        // ES7 UpdateExpression:
        //      1) LeftHandSideExpression[?Yield]
        //      2) LeftHandSideExpression[?Yield][no LineTerminator here]++
        //      3) LeftHandSideExpression[?Yield][no LineTerminator here]--
        //      4) ++UnaryExpression[?Yield]
        //      5) --UnaryExpression[?Yield]
        if self.is_update_expression() {
            let pos = self.node_pos();
            let update_expression = self.parse_update_expression();
            if self.token == Kind::AsteriskAsteriskToken {
                return self.parse_binary_expression_rest(
                    ast::get_binary_operator_precedence(self.token),
                    update_expression,
                    pos,
                );
            }
            return update_expression;
        }
        // ES7 UnaryExpression:
        //      1) UpdateExpression[?yield]
        //      2) delete UpdateExpression[?yield]
        //      3) void UpdateExpression[?yield]
        //      4) typeof UpdateExpression[?yield]
        //      5) + UpdateExpression[?yield]
        //      6) - UpdateExpression[?yield]
        //      7) ~ UpdateExpression[?yield]
        //      8) ! UpdateExpression[?yield]
        let unary_operator = self.token;
        let simple_unary_expression = self.parse_simple_unary_expression();
        if self.token == Kind::AsteriskAsteriskToken {
            self.report_invalid_exponentiation_operand(simple_unary_expression, unary_operator);
        }
        simple_unary_expression
    }

    #[cold]
    fn report_invalid_exponentiation_operand(
        &mut self,
        simple_unary_expression: P<Node>,
        unary_operator: Kind,
    ) {
        let pos = scanner::skip_trivia(
            self.source_text,
            simple_unary_expression.pos().expect_source("parsed nodes have source positions"),
        );
        let end = simple_unary_expression.end().expect_source("parsed nodes have source positions");
        if simple_unary_expression.kind() == Kind::TypeAssertionExpression {
            self.parse_error_at(pos, end, &diagnostics::A_TYPE_ASSERTION_EXPRESSION_IS_NOT_ALLOWED_IN_THE_LEFT_HAND_SIDE_OF_AN_EXPONENTIATION_EXPRESSION_CONSIDER_ENCLOSING_THE_EXPRESSION_IN_PARENTHESES, &[]);
        } else {
            assert!(is_keyword_or_punctuation(unary_operator));
            self.parse_error_at(pos, end, &diagnostics::AN_UNARY_EXPRESSION_WITH_THE_0_OPERATOR_IS_NOT_ALLOWED_IN_THE_LEFT_HAND_SIDE_OF_AN_EXPONENTIATION_EXPRESSION_CONSIDER_ENCLOSING_THE_EXPRESSION_IN_PARENTHESES, &[&scanner::token_to_string(unary_operator)]);
        }
    }

    fn is_update_expression(&self) -> bool {
        match self.token {
            Kind::PlusToken
            | Kind::MinusToken
            | Kind::TildeToken
            | Kind::ExclamationToken
            | Kind::DeleteKeyword
            | Kind::TypeOfKeyword
            | Kind::VoidKeyword
            | Kind::AwaitKeyword => false,
            Kind::LessThanToken => self.language_variant == LanguageVariant::JSX,
            _ => true,
        }
    }

    // Outlining ++/-- leaves a small common path; forcing it inline saves instructions in the parser corpora.
    #[expect(
        clippy::inline_always,
        reason = "Measured on the Compiler, VS Code, and t3code parser corpora after outlining update operators"
    )]
    #[inline(always)]
    fn parse_update_expression(&mut self) -> P<Node> {
        let pos = self.node_pos();
        if self.token == Kind::PlusPlusToken || self.token == Kind::MinusMinusToken {
            return self.parse_prefix_update_expression(pos);
        } else if self.language_variant == LanguageVariant::JSX
            && self.token == Kind::LessThanToken
            && self.look_ahead(Parser::next_token_is_identifier_or_keyword_or_greater_than)
        {
            // JSXElement is part of primaryExpression
            return self.parse_jsx_element_or_self_closing_element_or_fragment(
                true,  /* inExpressionContext */
                None,  /* topInvalidNodePosition */
                None,  /* openingTag */
                false, /* mustBeUnary */
            );
        }
        let expression = self.parse_left_hand_side_expression_or_higher();
        if (self.token == Kind::PlusPlusToken || self.token == Kind::MinusMinusToken)
            && !self.has_preceding_line_break()
        {
            return self.parse_postfix_update_expression(pos, expression);
        }
        expression
    }

    #[inline(never)]
    fn parse_prefix_update_expression(&mut self, pos: u32) -> P<Node> {
        let operator = self.token;
        self.next_token();
        let operand = self.parse_left_hand_side_expression_or_higher();
        let node = self.factory.new_prefix_unary_expression(operator, operand);
        self.finish_node(node, pos)
    }

    #[inline(never)]
    fn parse_postfix_update_expression(&mut self, pos: u32, expression: P<Node>) -> P<Node> {
        let operator = self.token;
        self.next_token();
        let node = self.factory.new_postfix_unary_expression(expression, operator);
        self.finish_node(node, pos)
    }

    fn parse_jsx_element_or_self_closing_element_or_fragment(
        &mut self,
        in_expression_context: bool,
        top_invalid_node_position: Option<u32>,
        opening_tag: Option<P<Node>>,
        must_be_unary: bool,
    ) -> P<Node> {
        let pos = self.node_pos();
        let opening = self
            .parse_jsx_opening_or_self_closing_element_or_opening_fragment(in_expression_context);
        let mut result: P<Node>;
        match opening.kind() {
            Kind::JsxOpeningElement => {
                let mut children = self.parse_jsx_children(opening);
                let closing_element: P<Node>;
                let last_child = children.nodes().last().copied();
                if let Some(last_child) = last_child.filter(|last_child| {
                    last_child.kind() == Kind::JsxElement
                        && !ast::tag_names_are_equivalent(
                            last_child.as_jsx_element().opening_element.tag_name(),
                            last_child.as_jsx_element().closing_element.tag_name(),
                        )
                        && ast::tag_names_are_equivalent(
                            opening.tag_name(),
                            last_child.as_jsx_element().closing_element.tag_name(),
                        )
                }) {
                    // when an unclosed JsxOpeningElement incorrectly parses its parent's JsxClosingElement,
                    // restructure (<div>(...<span>...</div>)) --> (<div>(...<span>...</>)</div>)
                    // (no need to error; the parent will error)
                    let last_child_data = last_child.as_jsx_element();
                    let end = last_child_data
                        .children
                        .end()
                        .expect_source("parsed lists have source positions");
                    let missing = self.new_identifier("");
                    let missing_identifier = self.finish_node_with_end(missing, end, end);
                    let closing = self.factory.new_jsx_closing_element(missing_identifier);
                    let new_closing_element = self.finish_node_with_end(closing, end, end);
                    let element = self.factory.new_jsx_element(
                        last_child_data.opening_element,
                        last_child_data.children,
                        new_closing_element,
                    );
                    let new_last = self.finish_node_with_end(
                        element,
                        last_child_data
                            .opening_element
                            .pos()
                            .expect_source("parsed nodes have source positions"),
                        end,
                    );
                    // force reset parent pointers from discarded parse result
                    last_child_data.opening_element.set_parent(Some(new_last));
                    for c in last_child_data.children.nodes() {
                        c.set_parent(Some(new_last));
                    }
                    new_closing_element.set_parent(Some(new_last));
                    let mut nodes = children.nodes()[0..children.nodes().len() - 1].to_vec();
                    nodes.push(new_last);
                    children = self.new_node_list(
                        TextRange::new(
                            children.pos().expect_source("parsed lists have source positions"),
                            new_last.end().expect_source("parsed nodes have source positions"),
                        ),
                        &nodes,
                    );
                    closing_element = last_child_data.closing_element;
                } else {
                    closing_element =
                        self.parse_jsx_closing_element(opening, in_expression_context);
                    if !ast::tag_names_are_equivalent(
                        opening.tag_name(),
                        closing_element.tag_name(),
                    ) {
                        if opening_tag.is_some_and(|opening_tag| {
                            ast::is_jsx_opening_element(opening_tag)
                                && ast::tag_names_are_equivalent(
                                    closing_element.tag_name(),
                                    opening_tag.tag_name(),
                                )
                        }) {
                            // opening incorrectly matched with its parent's closing -- put error on opening
                            let text = scanner::get_text_of_node_from_source_text(
                                self.source_text,
                                opening.tag_name(),
                                false, /*includeTrivia*/
                            );
                            self.parse_error_at_range(
                                opening.tag_name().loc(),
                                &diagnostics::JSX_ELEMENT_0_HAS_NO_CORRESPONDING_CLOSING_TAG,
                                &[&text],
                            );
                        } else {
                            // other opening/closing mismatches -- put error on closing
                            let text = scanner::get_text_of_node_from_source_text(
                                self.source_text,
                                opening.tag_name(),
                                false, /*includeTrivia*/
                            );
                            self.parse_error_at_range(
                                closing_element.tag_name().loc(),
                                &diagnostics::EXPECTED_CORRESPONDING_JSX_CLOSING_TAG_FOR_0,
                                &[&text],
                            );
                        }
                    }
                }
                let element = self.factory.new_jsx_element(opening, children, closing_element);
                result = self.finish_node(element, pos);
                closing_element.set_parent(Some(result)); // force reset parent pointers from possibly discarded parse result
            }
            Kind::JsxOpeningFragment => {
                let children = self.parse_jsx_children(opening);
                let closing_fragment = self.parse_jsx_closing_fragment(in_expression_context);
                let fragment = self.factory.new_jsx_fragment(opening, children, closing_fragment);
                result = self.finish_node(fragment, pos);
            }
            Kind::JsxSelfClosingElement => {
                // Nothing else to do for self-closing elements
                result = opening;
            }
            _ => panic!("Unhandled case in parseJsxElementOrSelfClosingElementOrFragment"),
        }
        // If the user writes the invalid code '<div></div><div></div>' in an expression context (i.e. not wrapped in
        // an enclosing tag), we'll naively try to parse   ^ this as a 'less than' operator and the remainder of the tag
        // as garbage, which will cause the formatter to badly mangle the JSX. Perform a speculative parse of a JSX
        // element if we see a < token so that we can wrap it in a synthetic binary expression so the formatter
        // does less damage and we can report a better error.
        // Since JSX elements are invalid < operands anyway, this lookahead parse will only occur in error scenarios
        // of one sort or another.
        // If we are in a unary context, we can't do this recovery; the binary expression we return here is not
        // a valid UnaryExpression and will cause problems later.
        if !must_be_unary && in_expression_context && self.token == Kind::LessThanToken {
            let top_bad_pos = top_invalid_node_position.unwrap_or_else(|| {
                result.pos().expect_source("parsed nodes have source positions")
            });
            let invalid_element = self.parse_jsx_element_or_self_closing_element_or_fragment(
                true,
                Some(top_bad_pos),
                None,
                false,
            );
            let operator_token = self.factory.new_token(Kind::CommaToken);
            let invalid_pos =
                invalid_element.pos().expect_source("parsed nodes have source positions");
            operator_token.set_loc(TextRange::new(invalid_pos, invalid_pos));
            self.parse_error_at(
                scanner::skip_trivia(self.source_text, top_bad_pos),
                invalid_element.end().expect_source("parsed nodes have source positions"),
                &diagnostics::JSX_EXPRESSIONS_MUST_HAVE_ONE_PARENT_ELEMENT,
                &[],
            );
            let binary = self.factory.new_binary_expression(
                None, /*modifiers*/
                result,
                None, /*typeNode*/
                operator_token,
                invalid_element,
            );
            result = self.finish_node(binary, pos);
        }
        result
    }

    fn parse_jsx_children(&mut self, opening_tag: P<Node>) -> P<NodeList> {
        let pos = self.node_pos();
        let save_parsing_contexts = self.parsing_contexts;
        self.parsing_contexts |= 1 << (ParsingContext::JsxChildren as u32);
        let start = self.node_stack.len();
        loop {
            let current_token = self.scanner.re_scan_jsx_token(true /*allowMultilineJsxText*/);
            self.report_scan_errors();
            let Some(child) = self.parse_jsx_child(opening_tag, current_token) else {
                break;
            };
            self.node_stack.push(child);
            if ast::is_jsx_opening_element(opening_tag)
                && child.kind() == Kind::JsxElement
                && !ast::tag_names_are_equivalent(
                    child.as_jsx_element().opening_element.tag_name(),
                    child.as_jsx_element().closing_element.tag_name(),
                )
                && ast::tag_names_are_equivalent(
                    opening_tag.tag_name(),
                    child.as_jsx_element().closing_element.tag_name(),
                )
            {
                // stop after parsing a mismatched child like <div>...(<span></div>) in order to reattach the </div> higher
                break;
            }
        }
        self.parsing_contexts = save_parsing_contexts;
        let end = self.node_pos();
        self.finish_node_list(start, TextRange::new(pos, end))
    }

    fn parse_jsx_child(&mut self, opening_tag: P<Node>, token: Kind) -> Option<P<Node>> {
        match token {
            Kind::EndOfFile => {
                // If we hit EOF, issue the error at the tag that lacks the closing element
                // rather than at the end of the file (which is useless)
                if ast::is_jsx_opening_fragment(opening_tag) {
                    self.parse_error_at_range(
                        opening_tag.loc(),
                        &diagnostics::JSX_FRAGMENT_HAS_NO_CORRESPONDING_CLOSING_TAG,
                        &[],
                    );
                } else {
                    // We want the error span to cover only 'Foo.Bar' in < Foo.Bar >
                    // or to cover only 'Foo' in < Foo >
                    let tag = opening_tag.tag_name();
                    let tag_end = tag.end().expect_source("parsed nodes have source positions");
                    let start = std::cmp::min(
                        scanner::skip_trivia(
                            self.source_text,
                            tag.pos().expect_source("parsed nodes have source positions"),
                        ),
                        tag_end,
                    );
                    let text = scanner::get_text_of_node_from_source_text(
                        self.source_text,
                        opening_tag.tag_name(),
                        false, /*includeTrivia*/
                    );
                    self.parse_error_at(
                        start,
                        tag_end,
                        &diagnostics::JSX_ELEMENT_0_HAS_NO_CORRESPONDING_CLOSING_TAG,
                        &[&text],
                    );
                }
                None
            }
            Kind::LessThanSlashToken | Kind::ConflictMarkerTrivia => None,
            Kind::JsxText | Kind::JsxTextAllWhiteSpaces => Some(self.parse_jsx_text()),
            Kind::OpenBraceToken => self.parse_jsx_expression(false /*inExpressionContext*/),
            Kind::LessThanToken => {
                Some(self.parse_jsx_element_or_self_closing_element_or_fragment(
                    false,
                    None,
                    Some(opening_tag),
                    false,
                ))
            }
            _ => panic!("Unhandled case in parseJsxChild"),
        }
    }

    fn parse_jsx_text(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let text = self.scanner.token_value();
        let result = self.factory.new_jsx_text(text, self.token == Kind::JsxTextAllWhiteSpaces);
        self.scan_jsx_text();
        self.finish_node(result, pos)
    }

    fn parse_jsx_expression(&mut self, in_expression_context: bool) -> Option<P<Node>> {
        let pos = self.node_pos();
        if !self.parse_expected(Kind::OpenBraceToken) {
            return None;
        }
        let mut dot_dot_dot_token: Option<P<Node>> = None;
        let mut expression: Option<P<Node>> = None;
        if self.token != Kind::CloseBraceToken {
            if !in_expression_context {
                dot_dot_dot_token = self.parse_optional_token(Kind::DotDotDotToken);
            }
            // Only an AssignmentExpression is valid here per the JSX spec,
            // but we can unambiguously parse a comma sequence and provide
            // a better error message in grammar checking.
            expression = Some(self.parse_expression());
        }
        if in_expression_context {
            self.parse_expected(Kind::CloseBraceToken);
        } else if self.parse_expected_without_advancing(Kind::CloseBraceToken) {
            self.scan_jsx_text();
        }
        let node = self.factory.new_jsx_expression(dot_dot_dot_token, expression);
        Some(self.finish_node(node, pos))
    }

    fn scan_jsx_text(&mut self) -> Kind {
        self.token = self.scanner.scan_jsx_token();
        self.report_scan_errors();
        self.token
    }

    fn scan_jsx_identifier(&mut self) -> Kind {
        self.token = self.scanner.scan_jsx_identifier();
        self.report_scan_errors();
        self.token
    }

    fn scan_jsx_attribute_value(&mut self) -> Kind {
        self.token = self.scanner.scan_jsx_attribute_value();
        self.report_scan_errors();
        self.token
    }

    fn parse_jsx_closing_element(&mut self, open: P<Node>, in_expression_context: bool) -> P<Node> {
        let pos = self.node_pos();
        self.parse_expected(Kind::LessThanSlashToken);
        let tag_name = self.parse_jsx_element_name();
        if self.parse_expected_with_diagnostic(
            Kind::GreaterThanToken,
            None,  /*diagnosticMessage*/
            false, /*shouldAdvance*/
        ) {
            // manually advance the scanner in order to look for jsx text inside jsx
            if in_expression_context || !ast::tag_names_are_equivalent(open.tag_name(), tag_name) {
                self.next_token();
            } else {
                self.scan_jsx_text();
            }
        }
        let node = self.factory.new_jsx_closing_element(tag_name);
        self.finish_node(node, pos)
    }

    fn parse_jsx_opening_or_self_closing_element_or_opening_fragment(
        &mut self,
        in_expression_context: bool,
    ) -> P<Node> {
        let pos = self.node_pos();
        self.parse_expected(Kind::LessThanToken);
        if self.token == Kind::GreaterThanToken {
            // See below for explanation of scanJsxText
            self.scan_jsx_text();
            let node = self.factory.new_jsx_opening_fragment();
            return self.finish_node(node, pos);
        }
        let tag_name = self.parse_jsx_element_name();
        let mut type_arguments: Option<P<NodeList>> = None;
        if !self.context_flags.intersects(NodeFlags::JavaScriptFile) {
            type_arguments = self.parse_type_arguments();
        }
        let attributes = self.parse_jsx_attributes();

        let result = if self.token == Kind::GreaterThanToken {
            // Closing tag, so scan the immediately-following text with the JSX scanning instead
            // of regular scanning to avoid treating illegal characters (e.g. '#') as immediate
            // scanning errors
            self.scan_jsx_text();
            self.factory.new_jsx_opening_element(tag_name, type_arguments, attributes)
        } else {
            self.parse_expected(Kind::SlashToken);
            if self.parse_expected_without_advancing(Kind::GreaterThanToken) {
                if in_expression_context {
                    self.next_token();
                } else {
                    self.scan_jsx_text();
                }
            }
            self.factory.new_jsx_self_closing_element(tag_name, type_arguments, attributes)
        };
        self.finish_node(result, pos)
    }

    fn parse_jsx_element_name(&mut self) -> P<Node> {
        let pos = self.node_pos();
        // JsxElement can have name in the form of
        //      propertyAccessExpression
        //      primaryExpression in the form of an identifier and "this" keyword
        // We can't just simply use parseLeftHandSideExpressionOrHigher because then we will start consider class,function etc as a keyword
        // We only want to consider "this" as a primaryExpression
        let initial_expression = self.parse_jsx_tag_name();
        if ast::is_jsx_namespaced_name(initial_expression) {
            return initial_expression; // `a:b.c` is invalid syntax, don't even look for the `.` if we parse `a:b`, and let `parseAttribute` report "unexpected :" instead.
        }
        let mut expression = initial_expression;
        while self.parse_optional(Kind::DotToken) {
            let name = self.parse_right_side_of_dot(
                true,  /*allowIdentifierNames*/
                false, /*allowPrivateIdentifiers*/
                false, /*allowUnicodeEscapeSequenceInIdentifierName*/
            );
            let node = self.factory.new_property_access_expression(
                expression,
                None,
                name,
                NodeFlags::None,
            );
            expression = self.finish_node(node, pos);
        }
        expression
    }

    fn parse_jsx_tag_name(&mut self) -> P<Node> {
        let pos = self.node_pos();
        self.scan_jsx_identifier();
        let is_this = self.token == Kind::ThisKeyword;
        let tag_name = self.parse_identifier_name_error_on_unicode_escape_sequence();
        if self.parse_optional(Kind::ColonToken) {
            self.scan_jsx_identifier();
            let name = self.parse_identifier_name_error_on_unicode_escape_sequence();
            let node = self.factory.new_jsx_namespaced_name(tag_name, name);
            return self.finish_node(node, pos);
        }
        if is_this {
            let result = self.factory.new_keyword_expression(Kind::ThisKeyword);
            return self.finish_node(result, pos);
        }
        tag_name
    }

    fn parse_jsx_attributes(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let properties =
            self.parse_list(ParsingContext::JsxAttributes, Parser::parse_jsx_attribute);
        let node = self.factory.new_jsx_attributes(properties);
        self.finish_node(node, pos)
    }

    fn parse_jsx_attribute(&mut self) -> P<Node> {
        if self.token == Kind::OpenBraceToken {
            return self.parse_jsx_spread_attribute();
        }
        let pos = self.node_pos();
        let name = self.parse_jsx_attribute_name();
        let initializer = self.parse_jsx_attribute_value();
        let node = self.factory.new_jsx_attribute(name, initializer);
        self.finish_node(node, pos)
    }

    fn parse_jsx_spread_attribute(&mut self) -> P<Node> {
        let pos = self.node_pos();
        self.parse_expected(Kind::OpenBraceToken);
        self.parse_expected(Kind::DotDotDotToken);
        let expression = self.parse_expression();
        self.parse_expected(Kind::CloseBraceToken);
        let node = self.factory.new_jsx_spread_attribute(expression);
        self.finish_node(node, pos)
    }

    fn parse_jsx_attribute_name(&mut self) -> P<Node> {
        let pos = self.node_pos();
        self.scan_jsx_identifier();
        let attr_name = self.parse_identifier_name_error_on_unicode_escape_sequence();
        if self.parse_optional(Kind::ColonToken) {
            self.scan_jsx_identifier();
            let name = self.parse_identifier_name_error_on_unicode_escape_sequence();
            let node = self.factory.new_jsx_namespaced_name(attr_name, name);
            return self.finish_node(node, pos);
        }
        attr_name
    }

    fn parse_jsx_attribute_value(&mut self) -> Option<P<Node>> {
        if self.token == Kind::EqualsToken {
            if self.scan_jsx_attribute_value() == Kind::StringLiteral {
                return Some(self.parse_literal_expression());
            }
            if self.token == Kind::OpenBraceToken {
                return self.parse_jsx_expression(true /*inExpressionContext*/);
            }
            if self.token == Kind::LessThanToken {
                // An attribute value must be a single JsxAttributeValue, so don't allow the sibling-element
                // recovery to wrap it in a synthetic binary expression.
                return Some(self.parse_jsx_element_or_self_closing_element_or_fragment(
                    true, None, None, true,
                ));
            }
            self.parse_error_at_current_token(&diagnostics::X_OR_JSX_ELEMENT_EXPECTED, &[]);
        }
        None
    }

    fn parse_jsx_closing_fragment(&mut self, in_expression_context: bool) -> P<Node> {
        let pos = self.node_pos();
        self.parse_expected(Kind::LessThanSlashToken);
        if self.parse_expected_with_diagnostic(
            Kind::GreaterThanToken,
            Some(&diagnostics::EXPECTED_CORRESPONDING_CLOSING_TAG_FOR_JSX_FRAGMENT),
            false, /*shouldAdvance*/
        ) {
            // manually advance the scanner in order to look for jsx text inside jsx
            if in_expression_context {
                self.next_token();
            } else {
                self.scan_jsx_text();
            }
        }
        let node = self.factory.new_jsx_closing_fragment();
        self.finish_node(node, pos)
    }

    fn parse_simple_unary_expression(&mut self) -> P<Node> {
        let token = self.token;
        match token {
            Kind::PlusToken | Kind::MinusToken | Kind::TildeToken | Kind::ExclamationToken => {
                self.parse_prefix_unary_expression()
            }
            Kind::DeleteKeyword => self.parse_delete_expression(),
            Kind::TypeOfKeyword => self.parse_type_of_expression(),
            Kind::VoidKeyword => self.parse_void_expression(),
            Kind::LessThanToken => {
                // Just like in parseUpdateExpression, we need to avoid parsing type assertions when
                // in JSX and we see an expression like "+ <foo> bar".
                if self.language_variant == LanguageVariant::JSX {
                    return self.parse_jsx_element_or_self_closing_element_or_fragment(
                        true, None, None, true,
                    );
                }
                // // This is modified UnaryExpression grammar in TypeScript
                // //  UnaryExpression (modified):
                // //      < type > UnaryExpression
                self.parse_type_assertion()
            }
            Kind::AwaitKeyword if self.is_await_expression() => self.parse_await_expression(),
            _ => self.parse_update_expression(),
        }
    }

    pub(crate) fn parse_prefix_unary_expression(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let operator = self.token;
        self.next_token();
        let operand = self.parse_simple_unary_expression();
        let node = self.factory.new_prefix_unary_expression(operator, operand);
        self.finish_node(node, pos)
    }

    fn parse_delete_expression(&mut self) -> P<Node> {
        let pos = self.node_pos();
        self.next_token();
        let expression = self.parse_simple_unary_expression();
        let node = self.factory.new_delete_expression(expression);
        self.finish_node(node, pos)
    }

    fn parse_type_of_expression(&mut self) -> P<Node> {
        let pos = self.node_pos();
        self.next_token();
        let expression = self.parse_simple_unary_expression();
        let node = self.factory.new_type_of_expression(expression);
        self.finish_node(node, pos)
    }

    fn parse_void_expression(&mut self) -> P<Node> {
        let pos = self.node_pos();
        self.next_token();
        let expression = self.parse_simple_unary_expression();
        let node = self.factory.new_void_expression(expression);
        self.finish_node(node, pos)
    }

    fn is_await_expression(&mut self) -> bool {
        if self.token == Kind::AwaitKeyword {
            if self.in_await_context() {
                return true;
            }
            // here we are using similar heuristics as 'isYieldExpression'
            return self
                .look_ahead(Parser::next_token_is_identifier_or_keyword_or_literal_on_same_line);
        }
        false
    }

    fn parse_await_expression(&mut self) -> P<Node> {
        let pos = self.node_pos();
        self.next_token();
        let expression = self.parse_simple_unary_expression();
        let node = self.factory.new_await_expression(expression);
        self.finish_node(node, pos)
    }

    fn parse_type_assertion(&mut self) -> P<Node> {
        assert!(
            self.language_variant != LanguageVariant::JSX,
            "Type assertions should never be parsed in JSX; they should be parsed as comparisons or JSX elements/fragments."
        );
        let pos = self.node_pos();
        self.parse_expected(Kind::LessThanToken);
        let type_node = self.parse_type();
        self.parse_expected(Kind::GreaterThanToken);
        let expression = self.parse_simple_unary_expression();
        let node = self.factory.new_type_assertion(type_node, expression);
        self.finish_node(node, pos)
    }

    #[inline]
    pub(crate) fn parse_left_hand_side_expression_or_higher(&mut self) -> P<Node> {
        // Original Ecma:
        // LeftHandSideExpression: See 11.2
        //      NewExpression
        //      CallExpression
        //
        // Our simplification:
        //
        // LeftHandSideExpression: See 11.2
        //      MemberExpression
        //      CallExpression
        //
        // See comment in parseMemberExpressionOrHigher on how we replaced NewExpression with
        // MemberExpression to make our lives easier.
        //
        // to best understand the below code, it's important to see how CallExpression expands
        // out into its own productions:
        //
        // CallExpression:
        //      MemberExpression Arguments
        //      CallExpression Arguments
        //      CallExpression[Expression]
        //      CallExpression.IdentifierName
        //      import (AssignmentExpression)
        //      super Arguments
        //      super.IdentifierName
        //
        // Because of the recursion in these calls, we need to bottom out first. There are three
        // bottom out states we can run into: 1) We see 'super' which must start either of
        // the last two CallExpression productions. 2) We see 'import' which must start import call.
        // 3)we have a MemberExpression which either completes the LeftHandSideExpression,
        // or starts the beginning of the first four CallExpression productions.
        let pos = self.node_pos();
        let expression = if self.token == Kind::ImportKeyword {
            self.parse_import_expression(pos)
        } else if self.token == Kind::SuperKeyword {
            self.parse_super_expression()
        } else {
            self.parse_member_expression_or_higher()
        };
        // Now, we *may* be complete.  However, we might have consumed the start of a
        // CallExpression or OptionalExpression.  As such, we need to consume the rest
        // of it here to be complete.
        self.parse_call_expression_rest(pos, expression)
    }

    #[inline(never)]
    fn parse_import_expression(&mut self, pos: u32) -> P<Node> {
        let expression: P<Node>;
        if self.look_ahead(Parser::next_token_is_open_paren_or_less_than) {
            // We don't want to eagerly consume all import keyword as import call expression so we look ahead to find "("
            // For example:
            //      var foo3 = require("subfolder
            //      import * as foo1 from "module-from-node
            // We want this import to be a statement rather than import call expression
            self.source_flags |= NodeFlags::PossiblyContainsDynamicImport;
            expression = self.parse_keyword_expression();
        } else if self.look_ahead(Parser::next_token_is_dot) {
            // This is an 'import.*' metaproperty (i.e. 'import.meta')
            self.next_token(); // advance past the 'import'
            self.next_token(); // advance past the dot
            let name = self.parse_import_meta_property_name();
            let node = self.factory.new_meta_property(Kind::ImportKeyword, name);
            expression = self.finish_node(node, pos);
            if ast::is_import_phase_meta_property(expression) {
                if self.token == Kind::OpenParenToken || self.token == Kind::LessThanToken {
                    self.source_flags |= NodeFlags::PossiblyContainsDynamicImport;
                }
            } else {
                self.source_flags |= NodeFlags::PossiblyContainsImportMeta;
            }
        } else {
            expression = self.parse_member_expression_or_higher();
        }
        expression
    }

    fn next_token_is_dot(&mut self) -> bool {
        self.next_token() == Kind::DotToken
    }

    fn parse_import_meta_property_name(&mut self) -> P<Node> {
        if matches!(self.token, Kind::DeferKeyword | Kind::SourceKeyword)
            && self.current_import_phase_modifier() == Kind::Unknown
        {
            self.parse_error_at_current_token(
                &diagnostics::KEYWORDS_CANNOT_CONTAIN_ESCAPE_CHARACTERS,
                &[],
            );
        }
        self.parse_identifier_name()
    }

    fn parse_super_expression(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let mut expression = self.parse_keyword_expression();
        if self.token == Kind::LessThanToken {
            let start_pos = self.node_pos();
            let type_arguments = self.try_parse_type_arguments_in_expression();
            if type_arguments.is_some() {
                let end = self.node_pos();
                self.parse_error_at(
                    start_pos,
                    end,
                    &diagnostics::X_SUPER_MAY_NOT_USE_TYPE_ARGUMENTS,
                    &[],
                );
                if !self.is_template_start_of_tagged_template() {
                    let node =
                        self.factory.new_expression_with_type_arguments(expression, type_arguments);
                    expression = self.finish_node(node, pos);
                }
            }
        }
        if self.token == Kind::OpenParenToken
            || self.token == Kind::DotToken
            || self.token == Kind::OpenBracketToken
        {
            return expression;
        }
        // If we have seen "super" it must be followed by '(' or '.'.
        // If it wasn't then just try to parse out a '.' and report an error.
        self.parse_error_at_current_token(
            &diagnostics::X_SUPER_MUST_BE_FOLLOWED_BY_AN_ARGUMENT_LIST_OR_MEMBER_ACCESS,
            &[],
        );
        // private names will never work with `super` (`super.#foo`), but that's a semantic error, not syntactic
        let name = self.parse_right_side_of_dot(
            true, /*allowIdentifierNames*/
            true, /*allowPrivateIdentifiers*/
            true, /*allowUnicodeEscapeSequenceInIdentifierName*/
        );
        let node = self.factory.new_property_access_expression(
            expression,
            None, /*questionDotToken*/
            name,
            NodeFlags::None,
        );
        self.finish_node(node, pos)
    }

    fn is_template_start_of_tagged_template(&self) -> bool {
        self.token == Kind::NoSubstitutionTemplateLiteral || self.token == Kind::TemplateHead
    }

    #[inline]
    fn try_parse_type_arguments_in_expression(&mut self) -> Option<P<NodeList>> {
        // TypeArguments must not be parsed in JavaScript files to avoid ambiguity with binary operators.
        // Check the cheap preconditions before saving the parser state: unless the current token is `<`
        // (or `<<`, which reScanLessThanToken would split), there is nothing to speculatively parse and
        // the mark/rewind would be a no-op.
        if self.context_flags.intersects(NodeFlags::JavaScriptFile)
            || (self.token != Kind::LessThanToken && self.token != Kind::LessThanLessThanToken)
        {
            return None;
        }
        self.try_parse_type_arguments_in_expression_worker()
    }

    fn try_parse_type_arguments_in_expression_worker(&mut self) -> Option<P<NodeList>> {
        let state = self.mark();
        if self.re_scan_less_than_token() == Kind::LessThanToken {
            self.next_token();
            let type_arguments =
                self.parse_delimited_list(ParsingContext::TypeArguments, Parser::parse_type);
            // If it doesn't have the closing `>` then it's definitely not an type argument list.
            if self.re_scan_greater_than_token() == Kind::GreaterThanToken {
                self.next_token();
                // We successfully parsed a type argument list. The next token determines whether we want to
                // treat it as such. If the type argument list is followed by `(` or a template literal, as in
                // `f<number>(42)`, we favor the type argument interpretation even though JavaScript would view
                // it as a relational expression.
                if self.can_follow_type_arguments_in_expression() {
                    return type_arguments;
                }
            }
        }
        self.rewind(state);
        None
    }

    fn can_follow_type_arguments_in_expression(&mut self) -> bool {
        match self.token {
            // These tokens can follow a type argument list in a call expression:
            // foo<x>(
            // foo<T> `...`
            // foo<T> `...${100}...`
            Kind::OpenParenToken | Kind::NoSubstitutionTemplateLiteral | Kind::TemplateHead => {
                return true;
            }
            // A type argument list followed by `<` never makes sense, and a type argument list followed
            // by `>` is ambiguous with a (re-scanned) `>>` operator, so we disqualify both. Also, in
            // this context, `+` and `-` are unary operators, not binary operators.
            Kind::LessThanToken | Kind::GreaterThanToken | Kind::PlusToken | Kind::MinusToken => {
                return false;
            }
            _ => {}
        }
        // We favor the type argument list interpretation when it is immediately followed by
        // a line break, a binary operator, or something that can't start an expression.
        self.has_preceding_line_break()
            || self.is_binary_operator()
            || !self.is_start_of_expression()
    }

    fn parse_member_expression_or_higher(&mut self) -> P<Node> {
        // Note: to make our lives simpler, we decompose the NewExpression productions and
        // place ObjectCreationExpression and FunctionExpression into PrimaryExpression.
        // like so:
        //
        //   PrimaryExpression : See 11.1
        //      this
        //      Identifier
        //      Literal
        //      ArrayLiteral
        //      ObjectLiteral
        //      (Expression)
        //      FunctionExpression
        //      new MemberExpression Arguments?
        //
        //   MemberExpression : See 11.2
        //      PrimaryExpression
        //      MemberExpression[Expression]
        //      MemberExpression.IdentifierName
        //
        //   CallExpression : See 11.2
        //      MemberExpression
        //      CallExpression Arguments
        //      CallExpression[Expression]
        //      CallExpression.IdentifierName
        //
        // Technically this is ambiguous.  i.e. CallExpression defines:
        //
        //   CallExpression:
        //      CallExpression Arguments
        //
        // If you see: "new Foo()"
        //
        // Then that could be treated as a single ObjectCreationExpression, or it could be
        // treated as the invocation of "new Foo".  We disambiguate that in code (to match
        // the original grammar) by making sure that if we see an ObjectCreationExpression
        // we always consume arguments if they are there. So we treat "new Foo()" as an
        // object creation only, and not at all as an invocation.  Another way to think
        // about this is that for every "new" that we see, we will consume an argument list if
        // it is there as part of the *associated* object creation node.  Any additional
        // argument lists we see, will become invocation expressions.
        //
        // Because there are no other places in the grammar now that refer to FunctionExpression
        // or ObjectCreationExpression, it is safe to push down into the PrimaryExpression
        // production.
        //
        // Because CallExpression and MemberExpression are left recursive, we need to bottom out
        // of the recursion immediately.  So we parse out a primary expression to start with.
        let pos = self.node_pos();
        let expression = self.parse_primary_expression();
        self.parse_member_expression_rest(pos, expression, true /*allowOptionalChain*/)
    }

    #[inline]
    pub(crate) fn parse_member_expression_rest(
        &mut self,
        pos: u32,
        expression: P<Node>,
        allow_optional_chain: bool,
    ) -> P<Node> {
        // Every other token leaves the expression unchanged without scanning or allocating. Keep that
        // return outside the worker's frame, which also contains node construction and error recovery.
        if !self.is_start_of_member_expression_rest() {
            return expression;
        }
        self.parse_member_expression_rest_worker(pos, expression, allow_optional_chain)
    }

    #[inline]
    fn is_start_of_member_expression_rest(&self) -> bool {
        matches!(
            self.token,
            Kind::DotToken
                | Kind::QuestionDotToken
                | Kind::OpenBracketToken
                | Kind::NoSubstitutionTemplateLiteral
                | Kind::TemplateHead
                | Kind::ExclamationToken
                | Kind::LessThanToken
                | Kind::LessThanLessThanToken
        )
    }

    #[inline(never)]
    fn parse_member_expression_rest_worker(
        &mut self,
        pos: u32,
        expression: P<Node>,
        allow_optional_chain: bool,
    ) -> P<Node> {
        let mut expression = expression;
        loop {
            let mut question_dot_token: Option<P<Node>> = None;
            let is_property_access;
            if allow_optional_chain && self.is_start_of_optional_property_or_element_access_chain()
            {
                question_dot_token = Some(self.parse_expected_token(Kind::QuestionDotToken));
                is_property_access = token_is_identifier_or_keyword(self.token);
            } else {
                is_property_access = self.parse_optional(Kind::DotToken);
            }
            if is_property_access {
                expression =
                    self.parse_property_access_expression_rest(pos, expression, question_dot_token);
                continue;
            }
            // when in the [Decorator] context, we do not parse ElementAccess as it could be part of a ComputedPropertyName
            if (question_dot_token.is_some() || !self.in_decorator_context())
                && self.parse_optional(Kind::OpenBracketToken)
            {
                expression =
                    self.parse_element_access_expression_rest(pos, expression, question_dot_token);
                continue;
            }
            if self.is_template_start_of_tagged_template() {
                // Absorb type arguments into TemplateExpression when preceding expression is ExpressionWithTypeArguments
                if question_dot_token.is_none()
                    && ast::is_expression_with_type_arguments(expression)
                {
                    let original = expression.as_expression_with_type_arguments();
                    expression = self.parse_tagged_template_rest(
                        pos,
                        original.expression,
                        question_dot_token,
                        original.type_arguments.get(),
                    );
                    Self::unparse_expression_with_type_arguments(
                        Some(original.expression),
                        original.type_arguments.get(),
                        expression,
                    );
                } else {
                    expression = self.parse_tagged_template_rest(
                        pos,
                        expression,
                        question_dot_token,
                        None, /*typeArguments*/
                    );
                }
                continue;
            }
            if question_dot_token.is_none() {
                if self.token == Kind::ExclamationToken && !self.has_preceding_line_break() {
                    self.next_token();
                    let node = self.factory.new_non_null_expression(expression, NodeFlags::None);
                    let node = self.finish_node(node, pos);
                    expression = self.check_js_syntax(node);
                    continue;
                }
                let type_arguments = self.try_parse_type_arguments_in_expression();
                if type_arguments.is_some() {
                    let node =
                        self.factory.new_expression_with_type_arguments(expression, type_arguments);
                    expression = self.finish_node(node, pos);
                    continue;
                }
            }
            return expression;
        }
    }

    fn is_start_of_optional_property_or_element_access_chain(&mut self) -> bool {
        self.token == Kind::QuestionDotToken
            && self
                .look_ahead(Parser::next_token_is_identifier_or_keyword_or_open_bracket_or_template)
    }

    fn next_token_is_identifier_or_keyword_or_open_bracket_or_template(&mut self) -> bool {
        self.next_token();
        token_is_identifier_or_keyword(self.token)
            || self.token == Kind::OpenBracketToken
            || self.is_template_start_of_tagged_template()
    }

    fn parse_property_access_expression_rest(
        &mut self,
        pos: u32,
        expression: P<Node>,
        question_dot_token: Option<P<Node>>,
    ) -> P<Node> {
        let name = self.parse_right_side_of_dot(
            true, /*allowIdentifierNames*/
            true, /*allowPrivateIdentifiers*/
            true, /*allowUnicodeEscapeSequenceInIdentifierName*/
        );
        let is_optional_chain =
            question_dot_token.is_some() || Self::try_reparse_optional_chain(expression);
        let property_access = self.factory.new_property_access_expression(
            expression,
            question_dot_token,
            name,
            if is_optional_chain { NodeFlags::OptionalChain } else { NodeFlags::None },
        );
        if is_optional_chain && ast::is_private_identifier(name) {
            let loc = self.skip_range_trivia(name.loc());
            self.parse_error_at_range(
                loc,
                &diagnostics::AN_OPTIONAL_CHAIN_CANNOT_CONTAIN_PRIVATE_IDENTIFIERS,
                &[],
            );
        }
        if ast::is_expression_with_type_arguments(expression)
            && let Some(type_arguments) = expression.type_argument_list()
        {
            let loc = TextRange::new(
                type_arguments.pos().expect_source("parsed lists have source positions") - 1,
                scanner::skip_trivia(
                    self.source_text,
                    type_arguments.end().expect_source("parsed lists have source positions"),
                ) + 1,
            );
            self.parse_error_at_range(
                loc,
                &diagnostics::AN_INSTANTIATION_EXPRESSION_CANNOT_BE_FOLLOWED_BY_A_PROPERTY_ACCESS,
                &[],
            );
        }
        self.finish_node(property_access, pos)
    }

    #[inline]
    fn try_reparse_optional_chain(node: P<Node>) -> bool {
        if node.flags().intersects(NodeFlags::OptionalChain) {
            return true;
        }
        // check for an optional chain in a non-null expression
        ast::is_non_null_expression(node)
            && Self::try_reparse_optional_chain_in_non_null_expression(node)
    }

    fn try_reparse_optional_chain_in_non_null_expression(node: P<Node>) -> bool {
        {
            let mut expr = node.expression().unwrap();
            while ast::is_non_null_expression(expr)
                && !expr.flags().intersects(NodeFlags::OptionalChain)
            {
                expr = expr.expression().unwrap();
            }
            if expr.flags().intersects(NodeFlags::OptionalChain) {
                // this is part of an optional chain. Walk down from `node` to `expression` and set the flag.
                let mut node = node;
                while ast::is_non_null_expression(node) {
                    node.set_flags(node.flags() | NodeFlags::OptionalChain);
                    node = node.expression().unwrap();
                }
                return true;
            }
        }
        false
    }

    fn parse_element_access_expression_rest(
        &mut self,
        pos: u32,
        expression: P<Node>,
        question_dot_token: Option<P<Node>>,
    ) -> P<Node> {
        let mut argument_expression = self.create_missing_identifier();
        if self.token == Kind::CloseBracketToken {
            let node_pos = self.node_pos();
            self.parse_error_at(
                node_pos,
                node_pos,
                &diagnostics::AN_ELEMENT_ACCESS_EXPRESSION_SHOULD_TAKE_AN_ARGUMENT,
                &[],
            );
        } else {
            argument_expression = self.parse_expression_allow_in();
        }
        self.parse_expected(Kind::CloseBracketToken);
        let is_optional_chain =
            question_dot_token.is_some() || Self::try_reparse_optional_chain(expression);
        let node = self.factory.new_element_access_expression(
            expression,
            question_dot_token,
            argument_expression,
            if is_optional_chain { NodeFlags::OptionalChain } else { NodeFlags::None },
        );
        self.finish_node(node, pos)
    }

    #[inline]
    pub(crate) fn parse_call_expression_rest(&mut self, pos: u32, expression: P<Node>) -> P<Node> {
        if self.token != Kind::OpenParenToken && !self.is_start_of_member_expression_rest() {
            return expression;
        }
        self.parse_call_expression_rest_worker(pos, expression)
    }

    #[inline(never)]
    fn parse_call_expression_rest_worker(&mut self, pos: u32, expression: P<Node>) -> P<Node> {
        let mut expression = expression;
        loop {
            expression = self
                .parse_member_expression_rest(pos, expression, true /*allowOptionalChain*/);
            let mut type_arguments: Option<P<NodeList>> = None;
            let question_dot_token = self.parse_optional_token(Kind::QuestionDotToken);
            if question_dot_token.is_some() {
                type_arguments = self.try_parse_type_arguments_in_expression();
                if self.is_template_start_of_tagged_template() {
                    expression = self.parse_tagged_template_rest(
                        pos,
                        expression,
                        question_dot_token,
                        type_arguments,
                    );
                    continue;
                }
            }
            if type_arguments.is_some() || self.token == Kind::OpenParenToken {
                // Absorb type arguments into CallExpression when preceding expression is ExpressionWithTypeArguments
                if question_dot_token.is_none()
                    && expression.kind() == Kind::ExpressionWithTypeArguments
                {
                    type_arguments = expression.type_argument_list();
                    expression = expression.as_expression_with_type_arguments().expression;
                }
                let inner = expression;
                let argument_list = self.parse_argument_list();
                let is_optional_chain =
                    question_dot_token.is_some() || Self::try_reparse_optional_chain(expression);
                let node = self.factory.new_call_expression(
                    expression,
                    question_dot_token,
                    type_arguments,
                    argument_list,
                    if is_optional_chain { NodeFlags::OptionalChain } else { NodeFlags::None },
                );
                let node = self.finish_node(node, pos);
                expression = self.check_js_syntax(node);
                Self::unparse_expression_with_type_arguments(
                    Some(inner),
                    type_arguments,
                    expression,
                );
                continue;
            }
            if question_dot_token.is_some() {
                // We parsed `?.` but then failed to parse anything, so report a missing identifier here.
                self.parse_error_at_current_token(&diagnostics::IDENTIFIER_EXPECTED, &[]);
                let name = self.create_missing_identifier();
                let node = self.factory.new_property_access_expression(
                    expression,
                    question_dot_token,
                    name,
                    NodeFlags::OptionalChain,
                );
                expression = self.finish_node(node, pos);
            }
            break;
        }
        expression
    }

    fn parse_argument_list(&mut self) -> P<NodeList> {
        self.parse_expected(Kind::OpenParenToken);
        let result = self
            .parse_delimited_list(
                ParsingContext::ArgumentExpressions,
                Parser::parse_argument_expression,
            )
            .unwrap();
        self.parse_expected(Kind::CloseParenToken);
        result
    }

    fn parse_argument_expression(&mut self) -> P<Node> {
        self.do_in_context(
            NodeFlags::DisallowInContext | NodeFlags::DecoratorContext,
            false,
            Parser::parse_argument_or_array_literal_element,
        )
    }

    fn parse_argument_or_array_literal_element(&mut self) -> P<Node> {
        match self.token {
            Kind::DotDotDotToken => return self.parse_spread_element(),
            Kind::CommaToken => {
                let node = self.factory.new_omitted_expression();
                let pos = self.node_pos();
                return self.finish_node(node, pos);
            }
            _ => {}
        }
        self.parse_assignment_expression_or_higher()
    }

    fn parse_spread_element(&mut self) -> P<Node> {
        let pos = self.node_pos();
        self.parse_expected(Kind::DotDotDotToken);
        let expression = self.parse_assignment_expression_or_higher();
        let node = self.factory.new_spread_element(expression);
        self.finish_node(node, pos)
    }

    fn parse_tagged_template_rest(
        &mut self,
        pos: u32,
        tag: P<Node>,
        question_dot_token: Option<P<Node>>,
        type_arguments: Option<P<NodeList>>,
    ) -> P<Node> {
        let template: P<Node> = if self.token == Kind::NoSubstitutionTemplateLiteral {
            self.re_scan_template_token(true /*isTaggedTemplate*/);
            self.parse_literal_expression()
        } else {
            self.parse_template_expression(true /*isTaggedTemplate*/)
        };
        let is_optional_chain =
            question_dot_token.is_some() || tag.flags().intersects(NodeFlags::OptionalChain);
        let node = self.factory.new_tagged_template_expression(
            tag,
            question_dot_token,
            type_arguments,
            template,
            if is_optional_chain { NodeFlags::OptionalChain } else { NodeFlags::None },
        );
        let node = self.finish_node(node, pos);
        self.check_js_syntax(node)
    }

    fn parse_template_expression(&mut self, is_tagged_template: bool) -> P<Node> {
        let pos = self.node_pos();
        let head = self.parse_template_head(is_tagged_template);
        let spans = self.parse_template_spans(is_tagged_template);
        let node = self.factory.new_template_expression(head, spans);
        self.finish_node(node, pos)
    }

    fn parse_template_spans(&mut self, is_tagged_template: bool) -> P<NodeList> {
        let pos = self.node_pos();
        let start = self.node_stack.len();
        loop {
            let span = self.parse_template_span(is_tagged_template);
            self.node_stack.push(span);
            if span.as_template_span().literal.kind() != Kind::TemplateMiddle {
                break;
            }
        }
        let end = self.node_pos();
        self.finish_node_list(start, TextRange::new(pos, end))
    }

    fn parse_template_span(&mut self, is_tagged_template: bool) -> P<Node> {
        let pos = self.node_pos();
        let expression = self.parse_expression_allow_in();
        let literal = self.parse_literal_of_template_span(is_tagged_template);
        let node = self.factory.new_template_span(expression, literal);
        self.finish_node(node, pos)
    }

    // Identifiers avoid the larger frame needed by the other primary expressions.
    #[inline]
    fn parse_primary_expression(&mut self) -> P<Node> {
        if self.token == Kind::Identifier {
            return self
                .parse_identifier_with_diagnostic(Some(&diagnostics::EXPRESSION_EXPECTED), None);
        }
        self.parse_primary_expression_worker()
    }

    #[inline(never)]
    fn parse_primary_expression_worker(&mut self) -> P<Node> {
        let token = self.token;
        match token {
            Kind::NoSubstitutionTemplateLiteral
            | Kind::NumericLiteral
            | Kind::BigIntLiteral
            | Kind::StringLiteral => {
                if token == Kind::NoSubstitutionTemplateLiteral
                    && self.scanner.token_flags().intersects(TokenFlags::IsInvalid)
                {
                    self.re_scan_template_token(false /*isTaggedTemplate*/);
                }
                return self.parse_literal_expression();
            }
            Kind::ThisKeyword
            | Kind::SuperKeyword
            | Kind::NullKeyword
            | Kind::TrueKeyword
            | Kind::FalseKeyword => {
                return self.parse_keyword_expression();
            }
            Kind::OpenParenToken => return self.parse_parenthesized_expression(),
            Kind::OpenBracketToken => return self.parse_array_literal_expression(),
            Kind::OpenBraceToken => return self.parse_object_literal_expression(),
            Kind::AsyncKeyword => {
                // Async arrow functions are parsed earlier in parseAssignmentExpressionOrHigher.
                // If we encounter `async [no LineTerminator here] function` then this is an async
                // function; otherwise, its an identifier.
                if self.look_ahead(Parser::next_token_is_function_keyword_on_same_line) {
                    return self.parse_function_expression();
                }
            }
            Kind::AtToken => return self.parse_decorated_expression(),
            Kind::ClassKeyword => return self.parse_class_expression(),
            Kind::FunctionKeyword => return self.parse_function_expression(),
            Kind::NewKeyword => return self.parse_new_expression_or_new_dot_target(),
            Kind::SlashToken | Kind::SlashEqualsToken => {
                if self.re_scan_slash_token() == Kind::RegularExpressionLiteral {
                    return self.parse_literal_expression();
                }
            }
            Kind::TemplateHead => {
                return self.parse_template_expression(false /*isTaggedTemplate*/);
            }
            Kind::PrivateIdentifier => return self.parse_private_identifier(),
            _ => {}
        }
        self.parse_identifier_with_diagnostic(Some(&diagnostics::EXPRESSION_EXPECTED), None)
    }

    fn parse_parenthesized_expression(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let jsdoc = self.jsdoc_scanner_info();
        self.parse_expected(Kind::OpenParenToken);
        let expression = self.parse_expression_allow_in();
        self.parse_expected(Kind::CloseParenToken);
        let node = self.factory.new_parenthesized_expression(expression);
        let result = self.finish_node(node, pos);
        self.with_jsdoc(result, jsdoc);
        result
    }

    pub(crate) fn parse_array_literal_expression(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let open_bracket_position = self.scanner.token_start();
        let open_bracket_parsed = self.parse_expected(Kind::OpenBracketToken);
        let multi_line = self.has_preceding_line_break();
        let elements = self
            .parse_delimited_list(
                ParsingContext::ArrayLiteralMembers,
                Parser::parse_argument_or_array_literal_element,
            )
            .unwrap();
        self.parse_expected_matching_brackets(
            Kind::OpenBracketToken,
            Kind::CloseBracketToken,
            open_bracket_parsed,
            open_bracket_position,
        );
        let node = self.factory.new_array_literal_expression(elements, multi_line);
        self.finish_node(node, pos)
    }

    pub(crate) fn parse_object_literal_expression(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let open_brace_position = self.scanner.token_start();
        let open_brace_parsed = self.parse_expected(Kind::OpenBraceToken);
        let multi_line = self.has_preceding_line_break();
        let properties = self
            .parse_delimited_list(
                ParsingContext::ObjectLiteralMembers,
                Parser::parse_object_literal_element,
            )
            .unwrap();
        self.parse_expected_matching_brackets(
            Kind::OpenBraceToken,
            Kind::CloseBraceToken,
            open_brace_parsed,
            open_brace_position,
        );
        let node = self.factory.new_object_literal_expression(properties, multi_line);
        self.finish_node(node, pos)
    }

    fn parse_object_literal_element(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let jsdoc = self.jsdoc_scanner_info();
        if self.parse_optional(Kind::DotDotDotToken) {
            let expression = self.parse_assignment_expression_or_higher();
            let node = self.factory.new_spread_assignment(expression);
            let result = self.finish_node(node, pos);
            self.with_jsdoc(result, jsdoc);
            return result;
        }
        let modifiers = self.parse_modifiers_ex(
            true,  /*allowDecorators*/
            false, /*permitConstAsModifier*/
            false, /*stopOnStartOfClassStaticBlock*/
        );
        if self.parse_contextual_modifier(Kind::GetKeyword) {
            return self.parse_accessor_declaration(
                pos,
                jsdoc,
                modifiers,
                Kind::GetAccessor,
                ParseFlags::None,
            );
        }
        if self.parse_contextual_modifier(Kind::SetKeyword) {
            return self.parse_accessor_declaration(
                pos,
                jsdoc,
                modifiers,
                Kind::SetAccessor,
                ParseFlags::None,
            );
        }
        let asterisk_token = self.parse_optional_token(Kind::AsteriskToken);
        let token_is_identifier = self.is_identifier();
        let name = self.parse_property_name();
        // Disallowing of optional property assignments and definite assignment assertion happens in the grammar checker.
        let mut postfix_token = self.parse_optional_token(Kind::QuestionToken);
        // Decorators, Modifiers, questionToken, and exclamationToken are not supported by property assignments and are reported in the grammar checker
        if postfix_token.is_none() {
            postfix_token = self.parse_optional_token(Kind::ExclamationToken);
        }
        if asterisk_token.is_some()
            || self.token == Kind::OpenParenToken
            || self.token == Kind::LessThanToken
        {
            return self.parse_method_declaration(
                pos,
                jsdoc,
                modifiers,
                asterisk_token,
                name,
                postfix_token,
                None, /*diagnosticMessage*/
            );
        }
        // check if it is short-hand property assignment or normal property assignment
        // NOTE: if token is EqualsToken it is interpreted as CoverInitializedName production
        // CoverInitializedName[Yield] :
        //     IdentifierReference[?Yield] Initializer[In, ?Yield]
        // this is necessary because ObjectLiteral productions are also used to cover grammar for ObjectAssignmentPattern

        let is_shorthand_property_assignment =
            token_is_identifier && self.token != Kind::ColonToken;
        let node: P<Node> = if is_shorthand_property_assignment {
            let equals_token = self.parse_optional_token(Kind::EqualsToken);
            let mut initializer: Option<P<Node>> = None;
            if equals_token.is_some() {
                initializer = Some(self.do_in_context(
                    NodeFlags::DisallowInContext,
                    false,
                    Parser::parse_assignment_expression_or_higher,
                ));
            }
            self.factory.new_shorthand_property_assignment(
                modifiers,
                name,
                postfix_token,
                None, /*typeNode*/
                equals_token,
                initializer,
            )
        } else {
            self.parse_expected(Kind::ColonToken);
            let initializer = self.do_in_context(
                NodeFlags::DisallowInContext,
                false,
                Parser::parse_assignment_expression_or_higher,
            );
            self.factory.new_property_assignment(
                modifiers,
                name,
                postfix_token,
                None, /*typeNode*/
                initializer,
            )
        };
        self.finish_node(node, pos);
        self.with_jsdoc(node, jsdoc);
        node
    }

    fn parse_function_expression(&mut self) -> P<Node> {
        // GeneratorExpression:
        //      function* BindingIdentifier [Yield][opt](FormalParameters[Yield]){ GeneratorBody }
        //
        // FunctionExpression:
        //      function BindingIdentifier[opt](FormalParameters){ FunctionBody }
        let save_contex_flags = self.context_flags;
        self.set_context_flags(NodeFlags::DecoratorContext, false);
        let pos = self.node_pos();
        let jsdoc = self.jsdoc_scanner_info();
        let modifiers = self.parse_modifiers();
        self.parse_expected(Kind::FunctionKeyword);
        let asterisk_token = self.parse_optional_token(Kind::AsteriskToken);
        let is_generator = asterisk_token.is_some();
        let is_async = modifier_list_has_async(modifiers);
        let signature_flags = (if is_generator { ParseFlags::Yield } else { ParseFlags::None })
            | (if is_async { ParseFlags::Await } else { ParseFlags::None });
        let name = if is_generator && is_async {
            self.do_in_context(
                NodeFlags::YieldContext | NodeFlags::AwaitContext,
                true,
                Parser::parse_optional_binding_identifier,
            )
        } else if is_generator {
            self.do_in_context(
                NodeFlags::YieldContext,
                true,
                Parser::parse_optional_binding_identifier,
            )
        } else if is_async {
            self.do_in_context(
                NodeFlags::AwaitContext,
                true,
                Parser::parse_optional_binding_identifier,
            )
        } else {
            self.parse_optional_binding_identifier()
        };
        let type_parameters = self.parse_type_parameters();
        let parameters = self.parse_parameters(signature_flags);
        let return_type = self.parse_return_type(Kind::ColonToken, false /*isType*/);
        let body = self.parse_function_block(signature_flags, None /*diagnosticMessage*/);
        self.context_flags = save_contex_flags;
        let result = self.factory.new_function_expression(
            modifiers,
            asterisk_token,
            name,
            type_parameters,
            Some(parameters),
            return_type,
            None, /*fullSignature*/
            Some(body),
        );
        self.finish_node(result, pos);
        self.with_jsdoc(result, jsdoc);
        self.check_js_syntax(result);
        result
    }

    fn parse_optional_binding_identifier(&mut self) -> Option<P<Node>> {
        if self.is_binding_identifier() {
            return Some(self.parse_binding_identifier());
        }
        None
    }

    fn parse_decorated_expression(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let jsdoc = self.jsdoc_scanner_info();
        let modifiers = self.parse_modifiers_ex(
            true,  /*allowDecorators*/
            false, /*permitConstAsModifier*/
            false, /*stopOnStartOfClassStaticBlock*/
        );
        if self.token == Kind::ClassKeyword {
            return self.parse_class_declaration_or_expression(
                pos,
                jsdoc,
                modifiers,
                Kind::ClassExpression,
            );
        }
        let node_pos = self.node_pos();
        self.parse_error_at(node_pos, node_pos, &diagnostics::EXPRESSION_EXPECTED, &[]);
        let node = self.factory.new_missing_declaration(modifiers);
        self.finish_node(node, pos)
    }

    fn unparse_expression_with_type_arguments(
        expression: Option<P<Node>>,
        type_arguments: Option<P<NodeList>>,
        result: P<Node>,
    ) {
        // force overwrite the `.Parent` of the expression and type arguments to erase the fact that they may have originally been parsed as an ExpressionWithTypeArguments and be parented to such
        if let Some(expression) = expression {
            expression.set_parent(Some(result));
        }
        if let Some(type_arguments) = type_arguments {
            for a in type_arguments.nodes() {
                a.set_parent(Some(result));
            }
        }
    }

    fn parse_new_expression_or_new_dot_target(&mut self) -> P<Node> {
        let pos = self.node_pos();
        self.parse_expected(Kind::NewKeyword);
        if self.parse_optional(Kind::DotToken) {
            let name = self.parse_identifier_name();
            let node = self.factory.new_meta_property(Kind::NewKeyword, name);
            return self.finish_node(node, pos);
        }
        let expression_pos = self.node_pos();
        let primary = self.parse_primary_expression();
        let mut expression = self.parse_member_expression_rest(
            expression_pos,
            primary,
            false, /*allowOptionalChain*/
        );
        let mut type_arguments: Option<P<NodeList>> = None;
        // Absorb type arguments into NewExpression when preceding expression is ExpressionWithTypeArguments
        if expression.kind() == Kind::ExpressionWithTypeArguments {
            type_arguments = expression.type_argument_list();
            expression = expression.as_expression_with_type_arguments().expression;
        }
        if self.token == Kind::QuestionDotToken {
            let text = scanner::get_text_of_node_from_source_text(
                self.source_text,
                expression,
                false, /*includeTrivia*/
            );
            self.parse_error_at_current_token(
                &diagnostics::INVALID_OPTIONAL_CHAIN_FROM_NEW_EXPRESSION_DID_YOU_MEAN_TO_CALL_0,
                &[&text],
            );
        }
        let mut argument_list: Option<P<NodeList>> = None;
        if self.token == Kind::OpenParenToken {
            argument_list = Some(self.parse_argument_list());
        }
        let node = self.factory.new_new_expression(expression, type_arguments, argument_list);
        let node = self.finish_node(node, pos);
        let result = self.check_js_syntax(node);
        Self::unparse_expression_with_type_arguments(Some(expression), type_arguments, result);
        result
    }

    pub(crate) fn parse_keyword_expression(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let result = self.factory.new_keyword_expression(self.token);
        self.next_token();
        self.finish_node(result, pos)
    }

    pub(crate) fn parse_literal_expression(&mut self) -> P<Node> {
        let pos = self.node_pos();
        let text = self.scanner.token_value();
        let token_flags = self.scanner.token_flags();
        let result = match self.token {
            Kind::StringLiteral => self.factory.new_string_literal(text, token_flags),
            Kind::NumericLiteral => self.factory.new_numeric_literal(text, token_flags),
            Kind::BigIntLiteral => self.factory.new_big_int_literal(text, token_flags),
            Kind::RegularExpressionLiteral => {
                self.factory.new_regular_expression_literal(text, token_flags)
            }
            Kind::NoSubstitutionTemplateLiteral => {
                self.factory.new_no_substitution_template_literal(text, token_flags)
            }
            _ => panic!("Unhandled case in parseLiteralExpression"),
        };
        self.next_token();
        self.finish_node(result, pos)
    }

    pub(crate) fn parse_identifier_name_error_on_unicode_escape_sequence(&mut self) -> P<Node> {
        if self.scanner.has_unicode_escape() || self.scanner.has_extended_unicode_escape() {
            self.parse_error_at_current_token(
                &diagnostics::UNICODE_ESCAPE_SEQUENCE_CANNOT_APPEAR_HERE,
                &[],
            );
        }
        self.create_identifier(token_is_identifier_or_keyword(self.token))
    }

    pub(crate) fn parse_binding_identifier(&mut self) -> P<Node> {
        self.parse_binding_identifier_with_diagnostic(None)
    }

    pub(crate) fn parse_binding_identifier_with_diagnostic(
        &mut self,
        private_identifier_diagnostic_message: Option<&'static Message>,
    ) -> P<Node> {
        let save_has_await_identifier = self.statement_has_await_identifier;
        let is_binding_identifier = self.is_binding_identifier();
        let id = self.create_identifier_with_diagnostic(
            is_binding_identifier,
            None, /*diagnosticMessage*/
            private_identifier_diagnostic_message,
        );
        self.statement_has_await_identifier = save_has_await_identifier;
        id
    }

    pub(crate) fn parse_identifier_name(&mut self) -> P<Node> {
        self.parse_identifier_name_with_diagnostic(None)
    }

    pub(crate) fn parse_identifier_name_with_diagnostic(
        &mut self,
        diagnostic_message: Option<&'static Message>,
    ) -> P<Node> {
        self.create_identifier_with_diagnostic(
            token_is_identifier_or_keyword(self.token),
            diagnostic_message,
            None,
        )
    }

    pub(crate) fn parse_identifier(&mut self) -> P<Node> {
        self.parse_identifier_with_diagnostic(None, None)
    }

    pub(crate) fn parse_identifier_with_diagnostic(
        &mut self,
        diagnostic_message: Option<&'static Message>,
        private_identifier_diagnostic_message: Option<&'static Message>,
    ) -> P<Node> {
        let is_identifier = self.is_identifier();
        self.create_identifier_with_diagnostic(
            is_identifier,
            diagnostic_message,
            private_identifier_diagnostic_message,
        )
    }

    pub(crate) fn create_identifier(&mut self, is_identifier: bool) -> P<Node> {
        self.create_identifier_with_diagnostic(is_identifier, None, None)
    }

    fn create_identifier_with_diagnostic(
        &mut self,
        is_identifier: bool,
        diagnostic_message: Option<&'static Message>,
        private_identifier_diagnostic_message: Option<&'static Message>,
    ) -> P<Node> {
        if is_identifier {
            let pos = if self.scanner.has_preceding_jsdoc_leading_asterisks() {
                self.scanner.token_start()
            } else {
                self.node_pos()
            };
            let text = self.scanner.token_value();
            let end = self.scanner.token_end();
            self.next_token_without_check();
            let id = self.new_source_identifier(text, TextRange::new(pos, end));
            return self.finish_node(id, pos);
        }
        self.create_missing_identifier_with_diagnostic(
            diagnostic_message,
            private_identifier_diagnostic_message,
        )
    }

    #[cold]
    fn create_missing_identifier_with_diagnostic(
        &mut self,
        diagnostic_message: Option<&'static Message>,
        private_identifier_diagnostic_message: Option<&'static Message>,
    ) -> P<Node> {
        if self.token == Kind::PrivateIdentifier {
            if let Some(private_identifier_diagnostic_message) =
                private_identifier_diagnostic_message
            {
                self.parse_error_at_current_token(private_identifier_diagnostic_message, &[]);
            } else {
                self.parse_error_at_current_token(
                    &diagnostics::PRIVATE_IDENTIFIERS_ARE_NOT_ALLOWED_OUTSIDE_CLASS_BODIES,
                    &[],
                );
            }
            return self.create_identifier(true /*isIdentifier*/);
        }
        // Only for end of file because the error gets reported incorrectly on embedded script tags.
        let report_at_current_position = self.token == Kind::EndOfFile;
        if let Some(diagnostic_message) = diagnostic_message {
            if report_at_current_position {
                let pos = self.scanner.token_full_start();
                self.parse_error_at(pos, pos, diagnostic_message, &[]);
            } else {
                self.parse_error_at_current_token(diagnostic_message, &[]);
            }
        } else if is_reserved_word(self.token) {
            let token_text = self.scanner.token_text();
            if report_at_current_position {
                let pos = self.scanner.token_full_start();
                self.parse_error_at(
                    pos,
                    pos,
                    &diagnostics::IDENTIFIER_EXPECTED_0_IS_A_RESERVED_WORD_THAT_CANNOT_BE_USED_HERE,
                    &[&token_text],
                );
            } else {
                self.parse_error_at_current_token(
                    &diagnostics::IDENTIFIER_EXPECTED_0_IS_A_RESERVED_WORD_THAT_CANNOT_BE_USED_HERE,
                    &[&token_text],
                );
            }
        } else if report_at_current_position {
            let pos = self.scanner.token_full_start();
            self.parse_error_at(pos, pos, &diagnostics::IDENTIFIER_EXPECTED, &[]);
        } else {
            self.parse_error_at_current_token(&diagnostics::IDENTIFIER_EXPECTED, &[]);
        }
        self.create_missing_identifier()
    }

    pub(crate) fn new_node_list(&self, loc: TextRange, nodes: &[P<Node>]) -> P<NodeList> {
        let list = self.factory.new_node_list_from_slice(nodes);
        list.loc.set(loc);
        list
    }

    pub(crate) fn new_modifier_list(&self, loc: TextRange, nodes: &[P<Node>]) -> P<ModifierList> {
        let list = self.factory.new_modifier_list_from_slice(nodes);
        list.list.loc.set(loc);
        list
    }

    pub(crate) fn finish_node(&mut self, node: P<Node>, pos: u32) -> P<Node> {
        let end = self.node_pos();
        self.finish_node_with_end(node, pos, end)
    }

    pub(crate) fn finish_node_with_end(&mut self, node: P<Node>, pos: u32, end: u32) -> P<Node> {
        node.set_loc(TextRange::new(pos, end));
        node.set_flags(node.flags() | self.context_flags);
        if self.has_parse_error {
            node.set_flags(node.flags() | NodeFlags::ThisNodeHasError);
            self.has_parse_error = false;
        }
        self.override_parent_in_immediate_children(node);
        node
    }

    pub(crate) fn override_parent_in_immediate_children(&mut self, node: P<Node>) {
        self.current_parent = Some(node);
        let current_parent = self.current_parent;
        node.for_each_child_static(&mut |n: P<Node>| {
            n.set_parent(current_parent);
            false
        });
        self.current_parent = None;
    }

    pub(crate) fn next_token_is_slash(&mut self) -> bool {
        self.next_token() == Kind::SlashToken
    }

    pub(crate) fn scan_type_member_start(&mut self) -> bool {
        // Return true if we have the start of a signature member
        if self.token == Kind::OpenParenToken
            || self.token == Kind::LessThanToken
            || self.token == Kind::GetKeyword
            || self.token == Kind::SetKeyword
        {
            return true;
        }
        let mut id_token = false;
        // Eat up all modifiers, but hold on to the last one in case it is actually an identifier
        while ast::is_modifier_kind(self.token) {
            id_token = true;
            self.next_token();
        }
        // Index signatures and computed property names are type members
        if self.token == Kind::OpenBracketToken {
            return true;
        }
        // Try to get the first property-like token following all modifiers
        if self.is_literal_property_name() {
            id_token = true;
            self.next_token();
        }
        // If we were able to get any potential identifier, check that it is
        // the start of a member declaration
        if id_token {
            return self.token == Kind::OpenParenToken
                || self.token == Kind::LessThanToken
                || self.token == Kind::QuestionToken
                || self.token == Kind::ColonToken
                || self.token == Kind::CommaToken
                || self.can_parse_semicolon();
        }
        false
    }

    pub(crate) fn scan_class_member_start(&mut self) -> bool {
        let mut id_token = Kind::Unknown;
        if self.token == Kind::AtToken {
            return true;
        }
        // Eat up all modifiers, but hold on to the last one in case it is actually an identifier.
        while ast::is_modifier_kind(self.token) {
            id_token = self.token;
            // If the idToken is a class modifier (protected, private, public, and static), it is
            // certain that we are starting to parse class member. This allows better error recovery
            // Example:
            //      public foo() ...     // true
            //      public @dec blah ... // true; we will then report an error later
            //      export public ...    // true; we will then report an error later
            if ast::is_class_member_modifier(id_token) {
                return true;
            }
            self.next_token();
        }
        if self.token == Kind::AsteriskToken {
            return true;
        }
        // Try to get the first property-like token following all modifiers.
        // This can either be an identifier or the 'get' or 'set' keywords.
        if self.is_literal_property_name() {
            id_token = self.token;
            self.next_token();
        }
        // Index signatures and computed properties are class members; we can parse.
        if self.token == Kind::OpenBracketToken {
            return true;
        }
        // If we were able to get any potential identifier...
        if id_token != Kind::Unknown {
            // If we have a non-keyword identifier, or if we have an accessor, then it's safe to parse.
            if !ast::is_keyword(id_token)
                || id_token == Kind::SetKeyword
                || id_token == Kind::GetKeyword
            {
                return true;
            }
            // If it *is* a keyword, but not an accessor, check a little farther along
            // to see if it should actually be parsed as a class member.
            match self.token {
                Kind::OpenParenToken // Method declaration
                | Kind::LessThanToken // Generic Method declaration
                | Kind::ExclamationToken // Non-null assertion on property name
                | Kind::ColonToken // Type Annotation for declaration
                | Kind::EqualsToken // Initializer for declaration
                | Kind::QuestionToken => {
                    // Not valid, but permitted so that it gets caught later on.
                    return true;
                }
                _ => {}
            }
            // Covers
            //  - Semicolons     (declaration termination)
            //  - Closing braces (end-of-class, must be declaration)
            //  - End-of-files   (not valid, but permitted so that it gets caught later on)
            //  - Line-breaks    (enabling *automatic semicolon insertion*)
            return self.can_parse_semicolon();
        }
        false
    }

    pub(crate) fn can_parse_semicolon(&self) -> bool {
        // If there's a real semicolon, then we can always parse it out.
        // We can parse out an optional semicolon in ASI cases in the following cases.
        self.token == Kind::SemicolonToken
            || self.token == Kind::CloseBraceToken
            || self.token == Kind::EndOfFile
            || self.has_preceding_line_break()
    }

    pub(crate) fn try_parse_semicolon(&mut self) -> bool {
        if !self.can_parse_semicolon() {
            return false;
        }
        if self.token == Kind::SemicolonToken {
            // consume the semicolon if it was explicitly provided.
            self.next_token();
        }
        true
    }

    pub(crate) fn parse_semicolon(&mut self) -> bool {
        self.try_parse_semicolon() || self.parse_expected(Kind::SemicolonToken)
    }

    pub(crate) fn is_literal_property_name(&self) -> bool {
        token_is_identifier_or_keyword(self.token)
            || self.token == Kind::StringLiteral
            || self.token == Kind::NumericLiteral
            || self.token == Kind::BigIntLiteral
    }

    pub(crate) fn is_start_of_statement(&mut self) -> bool {
        match self.token {
            Kind::ImportKeyword => {
                self.is_start_of_declaration()
                    || self.is_next_token_open_paren_or_less_than_or_dot()
            }
            Kind::ConstKeyword | Kind::ExportKeyword => self.is_start_of_declaration(),
            Kind::AtToken
            | Kind::SemicolonToken
            | Kind::OpenBraceToken
            | Kind::VarKeyword
            | Kind::LetKeyword
            | Kind::UsingKeyword
            | Kind::FunctionKeyword
            | Kind::ClassKeyword
            | Kind::EnumKeyword
            | Kind::IfKeyword
            | Kind::DoKeyword
            | Kind::WhileKeyword
            | Kind::ForKeyword
            | Kind::ContinueKeyword
            | Kind::BreakKeyword
            | Kind::ReturnKeyword
            | Kind::WithKeyword
            | Kind::SwitchKeyword
            | Kind::ThrowKeyword
            | Kind::TryKeyword
            | Kind::DebuggerKeyword
            // 'catch' and 'finally' do not actually indicate that the code is part of a statement,
            // however, we say they are here so that we may gracefully parse them and error later.
            | Kind::CatchKeyword
            | Kind::FinallyKeyword
            // When these don't start a declaration, they're an identifier in an expression statement
            | Kind::AsyncKeyword
            | Kind::DeclareKeyword
            | Kind::InterfaceKeyword
            | Kind::ModuleKeyword
            | Kind::NamespaceKeyword
            | Kind::TypeKeyword
            | Kind::GlobalKeyword
            | Kind::DeferKeyword
            | Kind::SourceKeyword => true,
            Kind::AccessorKeyword
            | Kind::PublicKeyword
            | Kind::PrivateKeyword
            | Kind::ProtectedKeyword
            | Kind::StaticKeyword
            | Kind::ReadonlyKeyword => {
                // When these don't start a declaration, they may be the start of a class member if an identifier
                // immediately follows. Otherwise they're an identifier in an expression statement.
                self.is_start_of_declaration()
                    || !self.look_ahead(Parser::next_token_is_identifier_or_keyword_on_same_line)
            }
            _ => self.is_start_of_expression(),
        }
    }

    pub(crate) fn is_start_of_declaration(&mut self) -> bool {
        self.look_ahead(Parser::scan_start_of_declaration)
    }

    fn scan_start_of_declaration(&mut self) -> bool {
        loop {
            match self.token {
                Kind::VarKeyword
                | Kind::LetKeyword
                | Kind::ConstKeyword
                | Kind::FunctionKeyword
                | Kind::ClassKeyword
                | Kind::EnumKeyword => {
                    return true;
                }
                Kind::UsingKeyword => return self.is_using_declaration(),
                Kind::AwaitKeyword => return self.is_await_using_declaration(),
                // 'declare', 'module', 'namespace', 'interface'* and 'type' are all legal JavaScript identifiers;
                // however, an identifier cannot be followed by another identifier on the same line. This is what we
                // count on to parse out the respective declarations. For instance, we exploit this to say that
                //
                //    namespace n
                //
                // can be none other than the beginning of a namespace declaration, but need to respect that JavaScript sees
                //
                //    namespace
                //    n
                //
                // as the identifier 'namespace' on one line followed by the identifier 'n' on another.
                // We need to look one token ahead to see if it permissible to try parsing a declaration.
                //
                // *Note*: 'interface' is actually a strict mode reserved word. So while
                //
                //   "use strict"
                //   interface
                //   I {}
                //
                // could be legal, it would add complexity for very little gain.
                Kind::InterfaceKeyword
                | Kind::TypeKeyword
                | Kind::DeferKeyword
                | Kind::SourceKeyword => {
                    return self.next_token_is_identifier_on_same_line();
                }
                Kind::ModuleKeyword | Kind::NamespaceKeyword => {
                    return self.next_token_is_identifier_or_string_literal_on_same_line();
                }
                Kind::AbstractKeyword
                | Kind::AccessorKeyword
                | Kind::AsyncKeyword
                | Kind::DeclareKeyword
                | Kind::PrivateKeyword
                | Kind::ProtectedKeyword
                | Kind::PublicKeyword
                | Kind::ReadonlyKeyword => {
                    let previous_token = self.token;
                    self.next_token();
                    // ASI takes effect for this modifier.
                    if self.has_preceding_line_break() {
                        return false;
                    }
                    if previous_token == Kind::DeclareKeyword && self.token == Kind::TypeKeyword {
                        // If we see 'declare type', then commit to parsing a type alias. parseTypeAliasDeclaration will
                        // report Line_break_not_permitted_here if needed.
                        return true;
                    }
                    continue;
                }
                Kind::GlobalKeyword => {
                    self.next_token();
                    return self.token == Kind::OpenBraceToken
                        || self.token == Kind::Identifier
                        || self.token == Kind::ExportKeyword;
                }
                Kind::ImportKeyword => {
                    self.next_token();
                    return self.token == Kind::DeferKeyword
                        || self.token == Kind::SourceKeyword
                        || self.token == Kind::StringLiteral
                        || self.token == Kind::AsteriskToken
                        || self.token == Kind::OpenBraceToken
                        || token_is_identifier_or_keyword(self.token);
                }
                Kind::ExportKeyword => {
                    self.next_token();
                    if self.token == Kind::EqualsToken
                        || self.token == Kind::AsteriskToken
                        || self.token == Kind::OpenBraceToken
                        || self.token == Kind::DefaultKeyword
                        || self.token == Kind::AsKeyword
                        || self.token == Kind::AtToken
                    {
                        return true;
                    }
                    if self.token == Kind::TypeKeyword {
                        self.next_token();
                        return self.token == Kind::AsteriskToken
                            || self.token == Kind::OpenBraceToken
                            || self.is_identifier() && !self.has_preceding_line_break();
                    }
                    continue;
                }
                Kind::StaticKeyword => {
                    self.next_token();
                    continue;
                }
                _ => {}
            }
            return false;
        }
    }

    pub(crate) fn is_start_of_expression(&mut self) -> bool {
        if self.is_start_of_left_hand_side_expression() {
            return true;
        }
        match self.token {
            Kind::PlusToken
            | Kind::MinusToken
            | Kind::TildeToken
            | Kind::ExclamationToken
            | Kind::DeleteKeyword
            | Kind::TypeOfKeyword
            | Kind::VoidKeyword
            | Kind::PlusPlusToken
            | Kind::MinusMinusToken
            | Kind::LessThanToken
            | Kind::AwaitKeyword
            | Kind::YieldKeyword
            | Kind::PrivateIdentifier
            | Kind::AtToken => {
                // Yield/await always starts an expression.  Either it is an identifier (in which case
                // it is definitely an expression).  Or it's a keyword (either because we're in
                // a generator or async function, or in strict mode (or both)) and it started a yield or await expression.
                return true;
            }
            _ => {}
        }
        // Error tolerance.  If we see the start of some binary operator, we consider
        // that the start of an expression.  That way we'll parse out a missing identifier,
        // give a good message about an identifier being missing, and then consume the
        // rest of the binary expression.
        if self.is_binary_operator() {
            return true;
        }
        self.is_identifier()
    }

    pub(crate) fn is_start_of_left_hand_side_expression(&mut self) -> bool {
        match self.token {
            Kind::ThisKeyword
            | Kind::SuperKeyword
            | Kind::NullKeyword
            | Kind::TrueKeyword
            | Kind::FalseKeyword
            | Kind::NumericLiteral
            | Kind::BigIntLiteral
            | Kind::StringLiteral
            | Kind::NoSubstitutionTemplateLiteral
            | Kind::TemplateHead
            | Kind::OpenParenToken
            | Kind::OpenBracketToken
            | Kind::OpenBraceToken
            | Kind::FunctionKeyword
            | Kind::ClassKeyword
            | Kind::NewKeyword
            | Kind::SlashToken
            | Kind::SlashEqualsToken
            | Kind::Identifier => return true,
            Kind::ImportKeyword => return self.is_next_token_open_paren_or_less_than_or_dot(),
            _ => {}
        }
        self.is_identifier()
    }

    pub(crate) fn is_start_of_type(&mut self, in_start_of_parameter: bool) -> bool {
        match self.token {
            Kind::AnyKeyword
            | Kind::UnknownKeyword
            | Kind::StringKeyword
            | Kind::NumberKeyword
            | Kind::BigIntKeyword
            | Kind::BooleanKeyword
            | Kind::ReadonlyKeyword
            | Kind::SymbolKeyword
            | Kind::UniqueKeyword
            | Kind::VoidKeyword
            | Kind::UndefinedKeyword
            | Kind::NullKeyword
            | Kind::ThisKeyword
            | Kind::TypeOfKeyword
            | Kind::NeverKeyword
            | Kind::OpenBraceToken
            | Kind::OpenBracketToken
            | Kind::LessThanToken
            | Kind::BarToken
            | Kind::AmpersandToken
            | Kind::NewKeyword
            | Kind::StringLiteral
            | Kind::NumericLiteral
            | Kind::BigIntLiteral
            | Kind::TrueKeyword
            | Kind::FalseKeyword
            | Kind::ObjectKeyword
            | Kind::AsteriskToken
            | Kind::QuestionToken
            | Kind::ExclamationToken
            | Kind::DotDotDotToken
            | Kind::InferKeyword
            | Kind::ImportKeyword
            | Kind::AssertsKeyword
            | Kind::NoSubstitutionTemplateLiteral
            | Kind::TemplateHead => return true,
            Kind::FunctionKeyword => return !in_start_of_parameter,
            Kind::MinusToken => {
                return !in_start_of_parameter
                    && self.look_ahead(Parser::next_token_is_numeric_or_big_int_literal);
            }
            Kind::OpenParenToken => {
                // Only consider '(' the start of a type if followed by ')', '...', an identifier, a modifier,
                // or something that starts a type. We don't want to consider things like '(1)' a type.
                return !in_start_of_parameter
                    && self.look_ahead(Parser::next_is_parenthesized_or_function_type);
            }
            _ => {}
        }
        self.is_identifier()
    }

    pub(crate) fn next_token_is_numeric_or_big_int_literal(&mut self) -> bool {
        self.next_token();
        self.token == Kind::NumericLiteral || self.token == Kind::BigIntLiteral
    }

    fn next_is_parenthesized_or_function_type(&mut self) -> bool {
        self.next_token();
        self.token == Kind::CloseParenToken
            || self.is_start_of_parameter(false /*isJSDocParameter*/)
            || self.is_start_of_type(false /*inStartOfParameter*/)
    }

    pub(crate) fn is_start_of_parameter(&mut self, is_jsdoc_parameter: bool) -> bool {
        self.token == Kind::DotDotDotToken
            || self.is_binding_identifier_or_private_identifier_or_pattern()
            || ast::is_modifier_kind(self.token)
            || self.token == Kind::AtToken
            || self.is_start_of_type(!is_jsdoc_parameter /*inStartOfParameter*/)
    }

    pub(crate) fn is_binding_identifier_or_private_identifier_or_pattern(&self) -> bool {
        self.token == Kind::OpenBraceToken
            || self.token == Kind::OpenBracketToken
            || self.token == Kind::PrivateIdentifier
            || self.is_binding_identifier()
    }

    fn is_next_token_open_paren_or_less_than_or_dot(&mut self) -> bool {
        self.look_ahead(Parser::next_token_is_open_paren_or_less_than_or_dot)
    }

    fn next_token_is_open_paren_or_less_than_or_dot(&mut self) -> bool {
        matches!(self.next_token(), Kind::OpenParenToken | Kind::LessThanToken | Kind::DotToken)
    }

    fn next_token_is_identifier_on_same_line(&mut self) -> bool {
        self.next_token();
        self.is_identifier() && !self.has_preceding_line_break()
    }

    fn next_token_is_identifier_or_string_literal_on_same_line(&mut self) -> bool {
        self.next_token();
        (self.is_identifier() || self.token == Kind::StringLiteral)
            && !self.has_preceding_line_break()
    }

    // Ignore strict mode flag because we will report an error in type checker instead.
    pub(crate) fn is_identifier(&self) -> bool {
        if self.token == Kind::Identifier {
            return true;
        }
        // If we have a 'yield' keyword, and we're in the [yield] context, then 'yield' is
        // considered a keyword and is not an identifier.
        // If we have a 'await' keyword, and we're in the [Await] context, then 'await' is
        // considered a keyword and is not an identifier.
        if self.token == Kind::YieldKeyword && self.in_yield_context()
            || self.token == Kind::AwaitKeyword && self.in_await_context()
        {
            return false;
        }
        self.token > Kind::LAST_RESERVED_WORD
    }

    pub(crate) fn is_binding_identifier(&self) -> bool {
        // `let await`/`let yield` in [Yield] or [Await] are allowed here and disallowed in the binder.
        self.token == Kind::Identifier || self.token > Kind::LAST_RESERVED_WORD
    }

    pub(crate) fn is_import_attribute_name(&self) -> bool {
        token_is_identifier_or_keyword(self.token) || self.token == Kind::StringLiteral
    }

    fn is_binary_operator(&self) -> bool {
        if self.in_disallow_in_context() && self.token == Kind::InKeyword {
            return false;
        }
        ast::get_binary_operator_precedence(self.token) != OperatorPrecedence::Invalid
    }

    pub(crate) fn is_valid_heritage_clause_object_literal(&mut self) -> bool {
        self.look_ahead(Parser::next_is_valid_heritage_clause_object_literal)
    }

    fn next_is_valid_heritage_clause_object_literal(&mut self) -> bool {
        if self.next_token() == Kind::CloseBraceToken {
            // if we see "extends {}" then only treat the {} as what we're extending (and not
            // the class body) if we have:
            //
            //      extends {} {
            //      extends {},
            //      extends {} extends
            //      extends {} implements
            let next = self.next_token();
            return next == Kind::CommaToken
                || next == Kind::OpenBraceToken
                || next == Kind::ExtendsKeyword
                || next == Kind::ImplementsKeyword;
        }
        true
    }

    pub(crate) fn is_heritage_clause(&self) -> bool {
        self.token == Kind::ExtendsKeyword || self.token == Kind::ImplementsKeyword
    }

    pub(crate) fn is_heritage_clause_extends_or_implements_keyword(&mut self) -> bool {
        self.is_heritage_clause() && self.look_ahead(Parser::next_is_start_of_expression)
    }

    fn next_is_start_of_expression(&mut self) -> bool {
        self.next_token();
        self.is_start_of_expression()
    }

    pub(crate) fn is_using_declaration(&mut self) -> bool {
        // 'using' always starts a lexical declaration if followed by an identifier. We also eagerly parse
        // |ObjectBindingPattern| so that we can report a grammar error during check. We don't parse out
        // |ArrayBindingPattern| since it potentially conflicts with element access (i.e., `using[x]`).
        self.look_ahead(|p| {
            p.next_token_is_binding_identifier_or_start_of_destructuring_on_same_line(
                false, /*disallowOf*/
            )
        })
    }

    fn next_token_is_equals_or_semicolon_or_colon_token(&mut self) -> bool {
        self.next_token();
        self.token == Kind::EqualsToken
            || self.token == Kind::SemicolonToken
            || self.token == Kind::ColonToken
    }

    fn next_token_is_binding_identifier_or_start_of_destructuring_on_same_line(
        &mut self,
        disallow_of: bool,
    ) -> bool {
        self.next_token();
        if disallow_of && self.token == Kind::OfKeyword {
            return self.look_ahead(Parser::next_token_is_equals_or_semicolon_or_colon_token);
        }
        (self.is_binding_identifier() || self.token == Kind::OpenBraceToken)
            && !self.has_preceding_line_break()
    }

    pub(crate) fn next_token_is_binding_identifier_or_start_of_destructuring_on_same_line_disallow_of(
        &mut self,
    ) -> bool {
        self.next_token_is_binding_identifier_or_start_of_destructuring_on_same_line(
            true, /*disallowOf*/
        )
    }

    pub(crate) fn is_await_using_declaration(&mut self) -> bool {
        self.look_ahead(Parser::next_is_using_keyword_then_binding_identifier_or_start_of_object_destructuring_on_same_line)
    }

    pub(crate) fn next_is_using_keyword_then_binding_identifier_or_start_of_object_destructuring_on_same_line(
        &mut self,
    ) -> bool {
        self.next_token() == Kind::UsingKeyword
            && self.next_token_is_binding_identifier_or_start_of_destructuring_on_same_line(
                false, /*disallowOf*/
            )
    }

    pub(crate) fn next_token_is_token_string_literal(&mut self) -> bool {
        self.next_token() == Kind::StringLiteral
    }

    pub(crate) fn set_context_flags(&mut self, flags: NodeFlags, value: bool) {
        if value {
            self.context_flags |= flags;
        } else {
            self.context_flags &= !flags;
        }
    }

    pub(crate) fn do_in_context<T>(
        &mut self,
        flags: NodeFlags,
        value: bool,
        f: impl FnOnce(&mut Parser) -> T,
    ) -> T {
        let save_context_flags = self.context_flags;
        self.set_context_flags(flags, value);
        let result = f(self);
        self.context_flags = save_context_flags;
        result
    }

    fn in_yield_context(&self) -> bool {
        self.context_flags.intersects(NodeFlags::YieldContext)
    }

    fn in_disallow_in_context(&self) -> bool {
        self.context_flags.intersects(NodeFlags::DisallowInContext)
    }

    pub(crate) fn in_disallow_conditional_types_context(&self) -> bool {
        self.context_flags.intersects(NodeFlags::DisallowConditionalTypesContext)
    }

    fn in_decorator_context(&self) -> bool {
        self.context_flags.intersects(NodeFlags::DecoratorContext)
    }

    pub(crate) fn in_await_context(&self) -> bool {
        self.context_flags.intersects(NodeFlags::AwaitContext)
    }

    pub(crate) fn skip_range_trivia(&self, text_range: TextRange) -> TextRange {
        TextRange::new(
            scanner::skip_trivia(
                self.source_text,
                text_range.pos().expect_source("parsed ranges have source positions"),
            ),
            text_range.end().expect_source("parsed ranges have source positions"),
        )
    }

    pub(crate) fn process_pragmas_into_fields(&mut self, context: P<ast::SourceFile>) {
        let mut check_js_directive: Option<P<ast::CheckJsDirective>> = None;
        let mut referenced_files: Vec<P<ast::FileReference>> = Vec::new();
        let mut type_reference_directives: Vec<P<ast::FileReference>> = Vec::new();
        let mut lib_reference_directives: Vec<P<ast::FileReference>> = Vec::new();
        // context.AmdDependencies = nil
        for pragma in context.pragmas() {
            match &*pragma.name {
                "reference" => {
                    let types = pragma.args.get("types");
                    let lib = pragma.args.get("lib");
                    let path = pragma.args.get("path");
                    let resolution_mode = pragma.args.get("resolution-mode");
                    let preserve = pragma.args.get("preserve");
                    let no_default_lib = pragma.args.get("no-default-lib");
                    if no_default_lib.is_some_and(|no_default_lib| no_default_lib.value == "true") {
                        // Ignored.
                    } else if let Some(types) = types {
                        let mut parsed = tsrs_core::ResolutionMode::default();
                        if let Some(resolution_mode) = resolution_mode {
                            parsed = self.parse_resolution_mode(
                                &resolution_mode.value,
                                resolution_mode
                                    .text_range
                                    .pos()
                                    .expect_source("pragma ranges have source positions"),
                                resolution_mode
                                    .text_range
                                    .end()
                                    .expect_source("pragma ranges have source positions"),
                            );
                        }
                        type_reference_directives.push(P::new(ast::FileReference {
                            text_range: types.text_range,
                            file_name: types.value.clone(),
                            resolution_mode: parsed,
                            preserve: preserve.is_some_and(|preserve| preserve.value == "true"),
                        }));
                    } else if let Some(lib) = lib {
                        lib_reference_directives.push(P::new(ast::FileReference {
                            text_range: lib.text_range,
                            file_name: lib.value.clone(),
                            resolution_mode: tsrs_core::ResolutionMode::default(),
                            preserve: preserve.is_some_and(|preserve| preserve.value == "true"),
                        }));
                    } else if let Some(path) = path {
                        referenced_files.push(P::new(ast::FileReference {
                            text_range: path.text_range,
                            file_name: path.value.clone(),
                            resolution_mode: tsrs_core::ResolutionMode::default(),
                            preserve: preserve.is_some_and(|preserve| preserve.value == "true"),
                        }));
                    } else {
                        self.parse_error_at_range(
                            pragma.comment_range.text_range,
                            &diagnostics::INVALID_REFERENCE_DIRECTIVE_SYNTAX,
                            &[],
                        );
                    }
                }
                "ts-check" | "ts-nocheck" => {
                    // _last_ of either nocheck or check in a file is the "winner"
                    if check_js_directive.is_none_or(|directive| {
                        pragma.comment_range.text_range.pos() > directive.range.text_range.pos()
                    }) {
                        check_js_directive = Some(P::new(ast::CheckJsDirective {
                            enabled: &*pragma.name == "ts-check",
                            range: pragma.comment_range,
                        }));
                    }
                }
                "jsx" | "jsxfrag" | "jsximportsource" | "jsxruntime" => {
                    // Nothing to do here
                }
                _ => panic!("Unhandled pragma kind: {}", pragma.name),
            }
        }
        context.check_js_directive.set(check_js_directive);
        context.referenced_files.set(tsrs_core::alloc_vec(referenced_files));
        context.type_reference_directives.set(tsrs_core::alloc_vec(type_reference_directives));
        context.lib_reference_directives.set(tsrs_core::alloc_vec(lib_reference_directives));
    }

    fn parse_resolution_mode(
        &mut self,
        mode: &str,
        pos: u32,
        end: u32,
    ) -> tsrs_core::ResolutionMode {
        let mut resolution_kind = tsrs_core::ResolutionMode::default();
        if mode == "import" {
            resolution_kind = tsrs_core::ModuleKind::ES_NEXT;
            return resolution_kind;
        }
        if mode == "require" {
            resolution_kind = tsrs_core::ModuleKind::COMMON_JS;
            return resolution_kind;
        }
        self.parse_error_at(
            pos,
            end,
            &diagnostics::X_RESOLUTION_MODE_SHOULD_BE_EITHER_REQUIRE_OR_IMPORT,
            &[],
        );
        resolution_kind
    }

    fn js_error_at_range(
        &mut self,
        loc: TextRange,
        message: &'static Message,
        args: &[&dyn Display],
    ) {
        let range = TextRange::new(
            scanner::skip_trivia(
                self.source_text,
                loc.pos().expect_source("parsed ranges have source positions"),
            ),
            loc.end().expect_source("parsed ranges have source positions"),
        );
        self.js_diagnostics.push(ast::new_diagnostic(None, range, message, args));
    }

    fn check_js_decorator_syntax(&mut self, node: P<Node>) {
        let modifiers = node.modifier_nodes();
        if modifiers.is_empty() {
            return;
        }

        if ast::can_have_illegal_decorators(node) {
            if let Some(modifier) = modifiers.iter().find_map(|&modifier| {
                if ast::is_decorator(modifier) { Some(modifier) } else { None }
            }) {
                self.js_error_at_range(
                    modifier.loc(),
                    &diagnostics::DECORATORS_ARE_NOT_VALID_HERE,
                    &[],
                );
            }
        } else if ast::can_have_decorators(node)
            && let Some(decorator_index) = find_index(modifiers, ast::is_decorator)
            && ast::is_class_declaration(node)
            && let Some(export_index) = find_index(modifiers, is_export_modifier)
        {
            let default_index = find_index(modifiers, |m| m.kind() == Kind::DefaultKeyword);
            if decorator_index > export_index
                && default_index.is_some_and(|default_index| decorator_index < default_index)
            {
                // Decorator between `export` and `default`
                self.js_error_at_range(
                    modifiers[decorator_index].loc(),
                    &diagnostics::DECORATORS_ARE_NOT_VALID_HERE,
                    &[],
                );
            } else if decorator_index < export_index {
                // Find a trailing decorator after the export keyword
                let trailing_decorator_index = modifiers[export_index..]
                    .iter()
                    .position(|&modifier| ast::is_decorator(modifier))
                    .map(|index| index + export_index);
                if let Some(trailing_decorator_index) = trailing_decorator_index {
                    let trailing = modifiers[trailing_decorator_index];
                    let diag = ast::new_diagnostic(
                                    None,
                                    TextRange::new(
                                        scanner::skip_trivia(
                                            self.source_text,
                                            trailing.loc().pos().expect_source("parsed nodes have source positions"),
                                        ),
                                        trailing.loc().end().expect_source("parsed nodes have source positions"),
                                    ),
                                    &diagnostics::DECORATORS_MAY_NOT_APPEAR_AFTER_EXPORT_OR_EXPORT_DEFAULT_IF_THEY_ALSO_APPEAR_BEFORE_EXPORT,
                                    &[],
                                );
                    let decorator = modifiers[decorator_index];
                    diag.add_related_info(ast::new_diagnostic(
                        None,
                        TextRange::new(
                            scanner::skip_trivia(
                                self.source_text,
                                decorator
                                    .loc()
                                    .pos()
                                    .expect_source("parsed nodes have source positions"),
                            ),
                            decorator
                                .loc()
                                .end()
                                .expect_source("parsed nodes have source positions"),
                        ),
                        &diagnostics::DECORATOR_USED_BEFORE_EXPORT_HERE,
                        &[],
                    ));
                    self.js_diagnostics.push(diag);
                }
            }
        }
    }

    #[inline]
    pub(crate) fn check_js_syntax(&mut self, node: P<Node>) -> P<Node> {
        if !node.flags().intersects(NodeFlags::JavaScriptFile)
            || node.flags().intersects(NodeFlags::JSDoc | NodeFlags::Reparsed)
        {
            return node;
        }
        self.check_js_syntax_worker(node)
    }

    fn check_js_syntax_worker(&mut self, node: P<Node>) -> P<Node> {
        match node.kind() {
            Kind::Parameter
            | Kind::PropertyDeclaration
            | Kind::MethodDeclaration
            | Kind::MethodSignature
            | Kind::Constructor
            | Kind::GetAccessor
            | Kind::SetAccessor
            | Kind::FunctionExpression
            | Kind::FunctionDeclaration
            | Kind::ArrowFunction
            | Kind::VariableDeclaration
            | Kind::IndexSignature => {
                if matches!(
                    node.kind(),
                    Kind::Parameter | Kind::PropertyDeclaration | Kind::MethodDeclaration
                ) && let Some(token) = node.question_token()
                    && !token.flags().intersects(NodeFlags::Reparsed)
                    && ast::is_question_token(token)
                {
                    self.js_error_at_range(
                        token.loc(),
                        &diagnostics::THE_0_MODIFIER_CAN_ONLY_BE_USED_IN_TYPE_SCRIPT_FILES,
                        &[&"?"],
                    );
                }
                // fallthrough
                if ast::is_function_like(Some(node)) && node.body().is_none() {
                    self.js_error_at_range(
                        node.loc(),
                        &diagnostics::SIGNATURE_DECLARATIONS_CAN_ONLY_BE_USED_IN_TYPE_SCRIPT_FILES,
                        &[],
                    );
                } else if let Some(t) =
                    node.type_node().filter(|t| !t.flags().intersects(NodeFlags::Reparsed))
                {
                    self.js_error_at_range(
                        t.loc(),
                        &diagnostics::TYPE_ANNOTATIONS_CAN_ONLY_BE_USED_IN_TYPE_SCRIPT_FILES,
                        &[],
                    );
                }
            }
            Kind::ImportDeclaration => {
                if node.import_clause().is_some_and(|clause| clause.is_type_only()) {
                    self.js_error_at_range(
                        node.loc(),
                        &diagnostics::X_0_DECLARATIONS_CAN_ONLY_BE_USED_IN_TYPE_SCRIPT_FILES,
                        &[&"import type"],
                    );
                }
            }
            Kind::ExportDeclaration => {
                if node.is_type_only() {
                    self.js_error_at_range(
                        node.loc(),
                        &diagnostics::X_0_DECLARATIONS_CAN_ONLY_BE_USED_IN_TYPE_SCRIPT_FILES,
                        &[&"export type"],
                    );
                }
            }
            Kind::ImportSpecifier => {
                if node.is_type_only() {
                    self.js_error_at_range(
                        node.loc(),
                        &diagnostics::X_0_DECLARATIONS_CAN_ONLY_BE_USED_IN_TYPE_SCRIPT_FILES,
                        &[&"import...type"],
                    );
                }
            }
            Kind::ExportSpecifier => {
                if node.is_type_only() {
                    self.js_error_at_range(
                        node.loc(),
                        &diagnostics::X_0_DECLARATIONS_CAN_ONLY_BE_USED_IN_TYPE_SCRIPT_FILES,
                        &[&"export...type"],
                    );
                }
            }
            Kind::ImportEqualsDeclaration => {
                self.js_error_at_range(
                    node.loc(),
                    &diagnostics::X_IMPORT_CAN_ONLY_BE_USED_IN_TYPE_SCRIPT_FILES,
                    &[],
                );
            }
            Kind::ExportAssignment => {
                if node.as_export_assignment().is_export_equals {
                    self.js_error_at_range(
                        node.loc(),
                        &diagnostics::X_EXPORT_CAN_ONLY_BE_USED_IN_TYPE_SCRIPT_FILES,
                        &[],
                    );
                }
            }
            Kind::HeritageClause => {
                if node.as_heritage_clause().token == Kind::ImplementsKeyword {
                    self.js_error_at_range(
                        node.loc(),
                        &diagnostics::X_IMPLEMENTS_CLAUSES_CAN_ONLY_BE_USED_IN_TYPE_SCRIPT_FILES,
                        &[],
                    );
                }
            }
            Kind::InterfaceDeclaration => {
                self.js_error_at_range(
                    node.name().unwrap().loc(),
                    &diagnostics::X_0_DECLARATIONS_CAN_ONLY_BE_USED_IN_TYPE_SCRIPT_FILES,
                    &[&"interface"],
                );
            }
            Kind::ModuleDeclaration => {
                let keyword = scanner::token_to_string(node.as_module_declaration().keyword);
                self.js_error_at_range(
                    node.name().unwrap().loc(),
                    &diagnostics::X_0_DECLARATIONS_CAN_ONLY_BE_USED_IN_TYPE_SCRIPT_FILES,
                    &[&keyword],
                );
            }
            Kind::TypeAliasDeclaration => {
                self.js_error_at_range(
                    node.name().unwrap().loc(),
                    &diagnostics::TYPE_ALIASES_CAN_ONLY_BE_USED_IN_TYPE_SCRIPT_FILES,
                    &[],
                );
            }
            Kind::EnumDeclaration => {
                self.js_error_at_range(
                    node.name().unwrap().loc(),
                    &diagnostics::X_0_DECLARATIONS_CAN_ONLY_BE_USED_IN_TYPE_SCRIPT_FILES,
                    &[&"enum"],
                );
            }
            Kind::NonNullExpression => {
                self.js_error_at_range(
                    node.loc(),
                    &diagnostics::NON_NULL_ASSERTIONS_CAN_ONLY_BE_USED_IN_TYPE_SCRIPT_FILES,
                    &[],
                );
            }
            Kind::AsExpression => {
                self.js_error_at_range(
                    node.type_node().unwrap().loc(),
                    &diagnostics::TYPE_ASSERTION_EXPRESSIONS_CAN_ONLY_BE_USED_IN_TYPE_SCRIPT_FILES,
                    &[],
                );
            }
            Kind::SatisfiesExpression => {
                self.js_error_at_range(node.type_node().unwrap().loc(), &diagnostics::TYPE_SATISFACTION_EXPRESSIONS_CAN_ONLY_BE_USED_IN_TYPE_SCRIPT_FILES, &[]);
            }
            _ => {}
        }
        // Check decorator placement in JS files
        self.check_js_decorator_syntax(node);
        // Check absence of type parameters, type arguments and non-JavaScript modifiers
        match node.kind() {
            Kind::ClassDeclaration
            | Kind::ClassExpression
            | Kind::MethodDeclaration
            | Kind::Constructor
            | Kind::GetAccessor
            | Kind::SetAccessor
            | Kind::FunctionExpression
            | Kind::FunctionDeclaration
            | Kind::ArrowFunction
            | Kind::VariableStatement
            | Kind::PropertyDeclaration => {
                if !matches!(node.kind(), Kind::VariableStatement | Kind::PropertyDeclaration)
                    && let Some(list) = node.type_parameter_list()
                    && list.nodes().iter().any(|n| !n.flags().intersects(NodeFlags::Reparsed))
                {
                    self.js_error_at_range(list.loc.get(), &diagnostics::TYPE_PARAMETER_DECLARATIONS_CAN_ONLY_BE_USED_IN_TYPE_SCRIPT_FILES, &[]);
                }
                // fallthrough
                for &modifier in node.modifier_nodes() {
                    if !modifier.flags().intersects(NodeFlags::Reparsed)
                        && modifier.kind() != Kind::Decorator
                        && !ast::modifier_to_flag(modifier.kind())
                            .intersects(ModifierFlags::JavaScript)
                    {
                        let text = scanner::token_to_string(modifier.kind());
                        self.js_error_at_range(
                            modifier.loc(),
                            &diagnostics::THE_0_MODIFIER_CAN_ONLY_BE_USED_IN_TYPE_SCRIPT_FILES,
                            &[&text],
                        );
                    }
                }
            }
            Kind::Parameter => {
                if node.modifier_nodes().iter().any(|&m| ast::is_modifier(m)) {
                    self.js_error_at_range(
                        node.modifiers().unwrap().list.loc.get(),
                        &diagnostics::PARAMETER_MODIFIERS_CAN_ONLY_BE_USED_IN_TYPE_SCRIPT_FILES,
                        &[],
                    );
                }
            }
            Kind::CallExpression
            | Kind::NewExpression
            | Kind::ExpressionWithTypeArguments
            | Kind::JsxSelfClosingElement
            | Kind::JsxOpeningElement
            | Kind::TaggedTemplateExpression => {
                if let Some(list) = node.type_argument_list()
                    && list.nodes().iter().any(|n| !n.flags().intersects(NodeFlags::Reparsed))
                {
                    self.js_error_at_range(
                        list.loc.get(),
                        &diagnostics::TYPE_ARGUMENTS_CAN_ONLY_BE_USED_IN_TYPE_SCRIPT_FILES,
                        &[],
                    );
                }
            }
            _ => {}
        }
        node
    }
}

// If true, we should abort parsing an error function.
fn type_has_arrow_function_blocking_parse_error(node: P<Node>) -> bool {
    match node.kind() {
        Kind::TypeReference => ast::node_is_missing(Some(node.as_type_reference_node().type_name)),
        Kind::FunctionType | Kind::ConstructorType => {
            is_missing_node_list(node.function_like_data().unwrap().parameters.get())
                || type_has_arrow_function_blocking_parse_error(node.type_node().unwrap())
        }
        Kind::ParenthesizedType => {
            type_has_arrow_function_blocking_parse_error(node.type_node().unwrap())
        }
        _ => false,
    }
}

// shouldConsumeBinaryOperator reports whether an operator binds before the operator represented by currentPrecedence.
// At equal precedence, only the right-associative exponentiation operator binds first.
fn should_consume_binary_operator(
    operator: Kind,
    operator_precedence: OperatorPrecedence,
    current_precedence: OperatorPrecedence,
) -> bool {
    if operator_precedence > current_precedence {
        return true;
    }
    operator_precedence == current_precedence && operator == Kind::AsteriskAsteriskToken
}

pub fn is_reserved_word(token: Kind) -> bool {
    Kind::FIRST_RESERVED_WORD <= token && token <= Kind::LAST_RESERVED_WORD
}

pub fn attach_file_to_diagnostics(
    diagnostics: &[P<ast::Diagnostic>],
    file: P<ast::SourceFile>,
) -> Vec<P<ast::Diagnostic>> {
    for d in diagnostics {
        d.set_file(Some(file));
        for r in d.related_information() {
            r.set_file(Some(file));
        }
    }
    diagnostics.to_vec()
}

pub fn get_comment_pragmas(
    _f: &mut ast::NodeFactory,
    source_text: &'static str,
) -> Vec<ast::Pragma> {
    let mut pragmas = Vec::new();
    pragmas.extend(scanner::get_leading_comment_ranges(source_text, 0).flat_map(|comment_range| {
        let comment = &source_text[comment_range.pos() as usize..comment_range.end() as usize];
        extract_pragmas(comment_range, comment)
    }));
    pragmas
}

fn extract_pragmas(comment_range: ast::CommentRange, text: &str) -> Vec<ast::Pragma> {
    let mut text = text;
    if comment_range.kind == Kind::SingleLineCommentTrivia {
        let mut pos: usize = 2;
        let triple_slash = match_(text, pos, "/");
        if triple_slash {
            pos += 1;
        }
        pos = skip_blanks(text, pos);
        if triple_slash && match_(text, pos, "<") {
            let tag_name = extract_name(text, pos + 1);
            if tag_name != "reference" {
                return Vec::new();
            }
            pos += 10;
            let mut args: FxHashMap<String, ast::PragmaArgument> = FxHashMap::default();
            loop {
                pos = skip_blanks(text, pos);
                if match_(text, pos, "/>") {
                    break;
                }
                let arg_name = extract_name(text, pos);
                if arg_name.is_empty() {
                    break;
                }
                pos = skip_blanks(text, pos + arg_name.len());
                if !match_(text, pos, "=") {
                    break;
                }
                pos = skip_blanks(text, pos + 1);
                let Some(value) = extract_quoted_string(text, pos) else {
                    break;
                };
                args.insert(
                    arg_name.clone(),
                    ast::PragmaArgument {
                        text_range: TextRange::new(
                            comment_range.pos() + pos as u32 + 1,
                            comment_range.pos() + pos as u32 + 1 + value.len() as u32,
                        ),
                        name: arg_name.clone(),
                        value: value.to_string(),
                    },
                );
                pos += value.len() + 2;
            }
            return vec![ast::Pragma { comment_range, name: "reference".to_string(), args }];
        }
        if match_(text, pos, "@") {
            pos += 1;
            let pragma_name = extract_name(text, pos);
            if !(pragma_name == "ts-check" || pragma_name == "ts-nocheck") {
                return Vec::new();
            }
            return vec![ast::Pragma {
                comment_range,
                name: pragma_name,
                args: FxHashMap::default(),
            }];
        }
    }
    if comment_range.kind == Kind::MultiLineCommentTrivia {
        text = text.strip_suffix("*/").unwrap_or(text);
        let mut pos: usize = 2;
        let mut pragmas = Vec::new();
        while let Some(found) = skip_to(text, pos, "@") {
            pos = found;
            // Mirrors the /@(\S+)(\s+(?:\S.*)?)?$/gm pragma regex used by TypeScript: the '@'
            // must be immediately followed by a non-whitespace pragma name, and the remainder
            // of the line is consumed as that pragma's arguments. As a consequence, only the
            // first '@'-token on a line is considered, so an unrelated '@token' earlier on the
            // line (e.g. an email address) prevents a later '@jsx' on the same line from being
            // treated as a pragma.
            let name_pos = pos + 1;
            let name_end = skip_non_blanks(text, name_pos);
            if name_end == name_pos {
                pos += 1;
                continue;
            }
            let line_end = line_end_pos(text, pos);
            let pragma_name = text[name_pos..name_end].to_lowercase();
            if pragma_name == "jsx"
                || pragma_name == "jsxfrag"
                || pragma_name == "jsximportsource"
                || pragma_name == "jsxruntime"
            {
                let start = skip_blanks(text, name_end);
                let arg_end = skip_non_blanks(text, start);
                if arg_end != start {
                    let mut args: FxHashMap<String, ast::PragmaArgument> =
                        FxHashMap::with_capacity_and_hasher(1, rustc_hash::FxBuildHasher);
                    args.insert(
                        "factory".to_string(),
                        ast::PragmaArgument {
                            text_range: TextRange::new(
                                comment_range.pos() + start as u32,
                                comment_range.pos() + arg_end as u32,
                            ),
                            name: "factory".to_string(),
                            value: text[start..arg_end].to_string(),
                        },
                    );
                    pragmas.push(ast::Pragma { comment_range, name: pragma_name.clone(), args });
                }
            }
            pos = line_end;
        }
        return pragmas;
    }
    Vec::new()
}

fn match_(text: &str, pos: usize, s: &str) -> bool {
    text.as_bytes()[pos..].starts_with(s.as_bytes())
}

fn skip_blanks(text: &str, pos: usize) -> usize {
    let bytes = text.as_bytes();
    let mut pos = pos;
    pos += (pos..bytes.len())
        .take_while(|&index| bytes[index] == b' ' || bytes[index] == b'\t')
        .count();
    pos
}

fn skip_non_blanks(text: &str, pos: usize) -> usize {
    let bytes = text.as_bytes();
    let mut pos = pos;
    pos += (pos..bytes.len())
        .take_while(|&index| {
            bytes[index] != b' '
                && bytes[index] != b'\t'
                && bytes[index] != b'\r'
                && bytes[index] != b'\n'
        })
        .count();
    pos
}

fn skip_to(text: &str, pos: usize, s: &str) -> Option<usize> {
    if pos >= text.len() {
        return None;
    }
    let haystack = &text.as_bytes()[pos..];
    let needle = s.as_bytes();
    haystack.windows(needle.len()).position(|w| w == needle).map(|i| pos + i)
}

fn line_end_pos(text: &str, pos: usize) -> usize {
    if let Some((offset, _)) =
        text[pos..].char_indices().find(|&(_, ch)| tsrs_core::stringutil::is_line_break(ch as u32))
    {
        return pos + offset;
    }
    text.len()
}

fn extract_name(text: &str, pos: usize) -> String {
    let bytes = text.as_bytes();
    let start = pos;
    let mut pos = pos;
    pos += (pos..bytes.len())
        .take_while(|&index| {
            bytes[index] >= b'A' && bytes[index] <= b'Z'
                || bytes[index] >= b'a' && bytes[index] <= b'z'
                || bytes[index] == b'-'
        })
        .count();
    text[start..pos].to_lowercase()
}

fn extract_quoted_string(text: &str, pos: usize) -> Option<&str> {
    let bytes = text.as_bytes();
    if pos == bytes.len() {
        return None;
    }
    let quote = bytes[pos];
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    let mut pos = pos + 1;
    let start = pos;
    pos += (pos..bytes.len()).take_while(|&index| bytes[index] != quote).count();
    if pos == bytes.len() {
        return None;
    }
    Some(&text[start..pos])
}

// core.FindIndex
fn find_index(nodes: &[P<Node>], mut f: impl FnMut(P<Node>) -> bool) -> Option<usize> {
    nodes.iter().position(|&n| f(n))
}
