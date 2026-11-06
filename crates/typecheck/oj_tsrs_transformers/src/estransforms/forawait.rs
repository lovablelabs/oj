use crate::*;
use printer::{EmitFlags, GeneratedIdentifierFlags};
use tsrs_core::TextRange;
use tsrs_core::collections::OrderedSet;

use super::SuperAccessState;
use super::async_::is_simple_parameter_list;

// Facts we track as we traverse the tree
// forawait.go:12
type ForAwaitHierarchyFacts = u32;

const FOR_AWAIT_HIERARCHY_FACTS_NONE: ForAwaitHierarchyFacts = 0;

//
// Ancestor facts
//

const FOR_AWAIT_HIERARCHY_FACTS_HAS_LEXICAL_THIS: ForAwaitHierarchyFacts = 1 << 0;
const FOR_AWAIT_HIERARCHY_FACTS_ITERATION_CONTAINER: ForAwaitHierarchyFacts = 1 << 1;

//
// Ancestor masks
//

const FOR_AWAIT_HIERARCHY_FACTS_ANCESTOR_FACTS_MASK: ForAwaitHierarchyFacts = (1 << 2) - 1;

const FOR_AWAIT_HIERARCHY_FACTS_SOURCE_FILE_EXCLUDES: ForAwaitHierarchyFacts =
    FOR_AWAIT_HIERARCHY_FACTS_ITERATION_CONTAINER;
const FOR_AWAIT_HIERARCHY_FACTS_STRICT_MODE_SOURCE_FILE_INCLUDES: ForAwaitHierarchyFacts =
    FOR_AWAIT_HIERARCHY_FACTS_NONE;

const FOR_AWAIT_HIERARCHY_FACTS_CLASS_OR_FUNCTION_INCLUDES: ForAwaitHierarchyFacts =
    FOR_AWAIT_HIERARCHY_FACTS_HAS_LEXICAL_THIS;
const FOR_AWAIT_HIERARCHY_FACTS_CLASS_OR_FUNCTION_EXCLUDES: ForAwaitHierarchyFacts =
    FOR_AWAIT_HIERARCHY_FACTS_ITERATION_CONTAINER;

const FOR_AWAIT_HIERARCHY_FACTS_ARROW_FUNCTION_INCLUDES: ForAwaitHierarchyFacts =
    FOR_AWAIT_HIERARCHY_FACTS_NONE;
const FOR_AWAIT_HIERARCHY_FACTS_ARROW_FUNCTION_EXCLUDES: ForAwaitHierarchyFacts =
    FOR_AWAIT_HIERARCHY_FACTS_CLASS_OR_FUNCTION_EXCLUDES;

const FOR_AWAIT_HIERARCHY_FACTS_ITERATION_STATEMENT_INCLUDES: ForAwaitHierarchyFacts =
    FOR_AWAIT_HIERARCHY_FACTS_ITERATION_CONTAINER;
const FOR_AWAIT_HIERARCHY_FACTS_ITERATION_STATEMENT_EXCLUDES: ForAwaitHierarchyFacts =
    FOR_AWAIT_HIERARCHY_FACTS_NONE;

// forawait.go:43
struct ForawaitTransformer {
    base: Transformer,
    super_access_state: SuperAccessState,

    enclosing_function_flags: Cell<FunctionFlags>,
    for_await_hierarchy_facts: Cell<ForAwaitHierarchyFacts>,
    exported_variable_statement: Cell<bool>,

    fallback_node_visitor: OnceCell<NodeVisitor>,
    no_async_modifier_visitor: OnceCell<NodeVisitor>,
}

// forawait.go:56
#[expect(
    clippy::unnecessary_wraps,
    reason = "Transformer factories share an optional result so disabled transforms can return None."
)]
#[expect(
    clippy::redundant_pub_crate,
    reason = "Only the public API is re-exported; implementation helpers remain crate-private."
)]
pub(crate) fn newforawait_transformer(opt: &TransformOptions) -> Option<P<Transformer>> {
    let tx = P::new(ForawaitTransformer {
        base: Transformer::default(),
        super_access_state: SuperAccessState::default(),
        enclosing_function_flags: Cell::new(FunctionFlags::Normal),
        for_await_hierarchy_facts: Cell::new(FOR_AWAIT_HIERARCHY_FACTS_NONE),
        exported_variable_statement: Cell::new(false),
        fallback_node_visitor: OnceCell::new(),
        no_async_modifier_visitor: OnceCell::new(),
    });
    let result = tx.get().base.new_transformer(
        Rc::new(move |_: &mut NodeVisitor, n: P<Node>| tx.visit(n)),
        Some(opt.context),
    );
    tx.get().super_access_state.init_super_access_visitor(tx.emit_context(), tx.factory());
    let _ =
        tx.fallback_node_visitor.set(tx.emit_context().new_node_visitor(Rc::new(
            move |_: &mut NodeVisitor, n: P<Node>| tx.visit_fallback(n),
        )));
    let _ = tx.no_async_modifier_visitor.set(tx.emit_context().new_node_visitor(Rc::new(
        |_: &mut NodeVisitor, node: P<Node>| {
            if node.kind() == Kind::AsyncKeyword {
                return None;
            }
            Some(node)
        },
    )));
    Some(result)
}

impl ForawaitTransformer {
    fn emit_context(&self) -> P<EmitContext> {
        self.base.emit_context()
    }

    fn factory(&self) -> &'static printer::NodeFactory {
        self.base.factory()
    }

    fn visitor(&self) -> NodeVisitor {
        self.base.visitor()
    }

    fn sas(&self) -> &SuperAccessState {
        &self.super_access_state
    }

    fn is_async_generator(&self) -> bool {
        let flags = self.enclosing_function_flags.get();
        flags.intersects(FunctionFlags::Async) && flags.intersects(FunctionFlags::Generator)
    }

    // forawait.go:73
    fn affects_subtree(
        &self,
        exclude_facts: ForAwaitHierarchyFacts,
        include_facts: ForAwaitHierarchyFacts,
    ) -> bool {
        self.for_await_hierarchy_facts.get()
            != ((self.for_await_hierarchy_facts.get() & !exclude_facts) | include_facts)
    }

    // enterSubtree sets the HierarchyFacts for this node prior to visiting this node's subtree,
    // returning the facts set prior to modification.
    // forawait.go:79
    fn enter_subtree(
        &self,
        exclude_facts: ForAwaitHierarchyFacts,
        include_facts: ForAwaitHierarchyFacts,
    ) -> ForAwaitHierarchyFacts {
        let ancestor_facts = self.for_await_hierarchy_facts.get();
        self.for_await_hierarchy_facts.set(
            ((self.for_await_hierarchy_facts.get() & !exclude_facts) | include_facts)
                & FOR_AWAIT_HIERARCHY_FACTS_ANCESTOR_FACTS_MASK,
        );
        ancestor_facts
    }

    // exitSubtree restores the HierarchyFacts for this node's ancestor after visiting this node's
    // subtree.
    // forawait.go:87
    fn exit_subtree(&self, ancestor_facts: ForAwaitHierarchyFacts) {
        self.for_await_hierarchy_facts.set(ancestor_facts);
    }

    // forawait.go:91
    fn visit_modifiers_no_async(
        &self,
        modifiers: Option<P<ModifierList>>,
    ) -> Option<P<ModifierList>> {
        self.no_async_modifier_visitor.get().unwrap().clone().visit_modifiers(modifiers)
    }

    // forawait.go:95
    fn do_with_hierarchy_facts(
        &self,
        cb: fn(&ForawaitTransformer, P<Node>) -> Option<P<Node>>,
        node: P<Node>,
        exclude_facts: ForAwaitHierarchyFacts,
        include_facts: ForAwaitHierarchyFacts,
    ) -> Option<P<Node>> {
        if self.affects_subtree(exclude_facts, include_facts) {
            let ancestor_facts = self.enter_subtree(exclude_facts, include_facts);
            let result = cb(self, node);
            self.exit_subtree(ancestor_facts);
            return result;
        }
        cb(self, node)
    }

    // forawait.go:105
    fn visit_default(&self, node: P<Node>) -> Option<P<Node>> {
        self.visitor().visit_each_child(Some(node))
    }

    // forawait.go:109
    fn fallback_visitor(&self, node: P<Node>) -> Option<P<Node>> {
        if self.sas().captured_super_properties.borrow().is_none() {
            return Some(node);
        }
        match node.kind() {
            Kind::FunctionExpression
            | Kind::FunctionDeclaration
            | Kind::MethodDeclaration
            | Kind::GetAccessor
            | Kind::SetAccessor
            | Kind::Constructor => {
                return Some(node);
            }
            _ => {}
        }
        self.sas().track_super_access(node);
        self.fallback_node_visitor.get().unwrap().clone().visit_each_child(Some(node))
    }

    // forawait.go:122
    fn visit_fallback(&self, node: P<Node>) -> Option<P<Node>> {
        self.fallback_visitor(node)
    }

    // forawait.go:126
    fn visit(&self, node: P<Node>) -> Option<P<Node>> {
        if !node.subtree_facts().intersects(SubtreeFacts::ContainsForAwaitOrAsyncGenerator) {
            return self.fallback_visitor(node);
        }
        self.sas().track_super_access(node);
        match node.kind() {
            Kind::SourceFile => Some(self.visit_source_file(node)),
            Kind::AwaitExpression => Some(self.visit_await_expression(node)),
            Kind::YieldExpression => Some(self.visit_yield_expression(node)),
            Kind::ReturnStatement => Some(self.visit_return_statement(node)),
            Kind::LabeledStatement => Some(self.visit_labeled_statement(node)),
            Kind::DoStatement
            | Kind::WhileStatement
            | Kind::ForInStatement
            | Kind::ForStatement => self.do_with_hierarchy_facts(
                ForawaitTransformer::visit_default,
                node,
                FOR_AWAIT_HIERARCHY_FACTS_ITERATION_STATEMENT_EXCLUDES,
                FOR_AWAIT_HIERARCHY_FACTS_ITERATION_STATEMENT_INCLUDES,
            ),
            Kind::ForOfStatement => Some(self.visit_for_of_statement(node, None)),
            Kind::Constructor => self.do_with_hierarchy_facts(
                ForawaitTransformer::visit_constructor_declaration,
                node,
                FOR_AWAIT_HIERARCHY_FACTS_CLASS_OR_FUNCTION_EXCLUDES,
                FOR_AWAIT_HIERARCHY_FACTS_CLASS_OR_FUNCTION_INCLUDES,
            ),
            Kind::MethodDeclaration => self.do_with_hierarchy_facts(
                ForawaitTransformer::visit_method_declaration,
                node,
                FOR_AWAIT_HIERARCHY_FACTS_CLASS_OR_FUNCTION_EXCLUDES,
                FOR_AWAIT_HIERARCHY_FACTS_CLASS_OR_FUNCTION_INCLUDES,
            ),
            Kind::GetAccessor => self.do_with_hierarchy_facts(
                ForawaitTransformer::visit_get_accessor_declaration,
                node,
                FOR_AWAIT_HIERARCHY_FACTS_CLASS_OR_FUNCTION_EXCLUDES,
                FOR_AWAIT_HIERARCHY_FACTS_CLASS_OR_FUNCTION_INCLUDES,
            ),
            Kind::SetAccessor => self.do_with_hierarchy_facts(
                ForawaitTransformer::visit_set_accessor_declaration,
                node,
                FOR_AWAIT_HIERARCHY_FACTS_CLASS_OR_FUNCTION_EXCLUDES,
                FOR_AWAIT_HIERARCHY_FACTS_CLASS_OR_FUNCTION_INCLUDES,
            ),
            Kind::FunctionDeclaration => self.do_with_hierarchy_facts(
                ForawaitTransformer::visit_function_declaration,
                node,
                FOR_AWAIT_HIERARCHY_FACTS_CLASS_OR_FUNCTION_EXCLUDES,
                FOR_AWAIT_HIERARCHY_FACTS_CLASS_OR_FUNCTION_INCLUDES,
            ),
            Kind::FunctionExpression => self.do_with_hierarchy_facts(
                ForawaitTransformer::visit_function_expression,
                node,
                FOR_AWAIT_HIERARCHY_FACTS_CLASS_OR_FUNCTION_EXCLUDES,
                FOR_AWAIT_HIERARCHY_FACTS_CLASS_OR_FUNCTION_INCLUDES,
            ),
            Kind::ArrowFunction => self.do_with_hierarchy_facts(
                ForawaitTransformer::visit_arrow_function,
                node,
                FOR_AWAIT_HIERARCHY_FACTS_ARROW_FUNCTION_EXCLUDES,
                FOR_AWAIT_HIERARCHY_FACTS_ARROW_FUNCTION_INCLUDES,
            ),
            Kind::ClassDeclaration | Kind::ClassExpression => self.do_with_hierarchy_facts(
                ForawaitTransformer::visit_default,
                node,
                FOR_AWAIT_HIERARCHY_FACTS_CLASS_OR_FUNCTION_EXCLUDES,
                FOR_AWAIT_HIERARCHY_FACTS_CLASS_OR_FUNCTION_INCLUDES,
            ),
            _ => self.visitor().visit_each_child(Some(node)),
        }
    }

    // forawait.go:221
    fn visit_await_expression(&self, node: P<Node>) -> P<Node> {
        if self.is_async_generator() {
            let f = self.factory();
            let result = f.new_yield_expression(
                None, /*asteriskToken*/
                Some(f.new_await_helper(
                    self.visitor().visit_node(Some(node.as_await_expression().expression)).unwrap(),
                )),
            );
            result.set_loc(node.loc());
            self.emit_context().set_original(result, node);
            return result;
        }
        self.visitor().visit_each_child(Some(node)).unwrap()
    }

    // forawait.go:234
    fn visit_yield_expression(&self, node: P<Node>) -> P<Node> {
        if self.is_async_generator() {
            let f = self.factory();
            let y = node.as_yield_expression();
            if y.asterisk_token.is_some() {
                let expression = self.visitor().visit_node(y.expression).unwrap();

                let async_values_result = f.new_async_values_helper(expression);
                async_values_result.set_loc(expression.loc());

                let async_delegator_result = f.new_async_delegator_helper(async_values_result);
                async_delegator_result.set_loc(expression.loc());

                let inner_yield =
                    f.update_yield_expression(node, y.asterisk_token, Some(async_delegator_result));

                let awaited_yield = f.new_await_helper(inner_yield);

                let result =
                    f.new_yield_expression(None /*asteriskToken*/, Some(awaited_yield));
                result.set_loc(node.loc());
                self.emit_context().set_original(result, node);
                return result;
            }

            let inner_expression = if y.expression.is_some() {
                self.visitor().visit_node(y.expression).unwrap()
            } else {
                f.new_void_zero_expression()
            };

            let result = f.new_yield_expression(
                None, /*asteriskToken*/
                Some(self.create_downlevel_await(inner_expression)),
            );
            result.set_loc(node.loc());
            self.emit_context().set_original(result, node);
            return result;
        }

        self.visitor().visit_each_child(Some(node)).unwrap()
    }

    // forawait.go:281
    fn visit_return_statement(&self, node: P<Node>) -> P<Node> {
        if self.is_async_generator() {
            let r = node.as_return_statement();
            let expression = if r.expression().is_some() {
                self.visitor().visit_node(r.expression()).unwrap()
            } else {
                self.factory().new_void_zero_expression()
            };
            return self
                .factory()
                .update_return_statement(node, Some(self.create_downlevel_await(expression)));
        }

        self.visitor().visit_each_child(Some(node)).unwrap()
    }

    // forawait.go:298
    fn visit_labeled_statement(&self, node: P<Node>) -> P<Node> {
        if self.enclosing_function_flags.get().intersects(FunctionFlags::Async) {
            let statement = unwrap_innermost_statement_of_label(node);
            if statement.kind() == Kind::ForOfStatement
                && statement.as_for_in_or_of_statement().await_modifier.is_some()
            {
                return self.visit_for_of_statement(statement, Some(node));
            }
            return self.factory().restore_enclosing_label(
                self.visitor().visit_node(Some(statement)).unwrap(),
                Some(node),
            );
        }
        self.visitor().visit_each_child(Some(node)).unwrap()
    }

    // forawait.go:319
    fn visit_source_file(&self, node: P<Node>) -> P<Node> {
        let ancestor_facts = self.enter_subtree(
            FOR_AWAIT_HIERARCHY_FACTS_SOURCE_FILE_EXCLUDES,
            FOR_AWAIT_HIERARCHY_FACTS_STRICT_MODE_SOURCE_FILE_INCLUDES,
        );
        self.exported_variable_statement.set(false);
        let visited = self.visitor().visit_each_child(Some(node)).unwrap();
        self.emit_context().add_emit_helper(visited, &self.emit_context().read_emit_helpers());
        self.exit_subtree(ancestor_facts);
        visited
    }

    // visitForOfStatement visits a ForOfStatement and converts it into a ES2015-compatible ForOfStatement.
    // forawait.go:332
    fn visit_for_of_statement(
        &self,
        node: P<Node>,
        outermost_labeled_statement: Option<P<Node>>,
    ) -> P<Node> {
        let ancestor_facts = self.enter_subtree(
            FOR_AWAIT_HIERARCHY_FACTS_ITERATION_STATEMENT_EXCLUDES,
            FOR_AWAIT_HIERARCHY_FACTS_ITERATION_STATEMENT_INCLUDES,
        );
        let result = if node.as_for_in_or_of_statement().await_modifier.is_some() {
            self.transform_for_await_of_statement(node, outermost_labeled_statement, ancestor_facts)
        } else {
            self.factory().restore_enclosing_label(
                self.visitor().visit_each_child(Some(node)).unwrap(),
                outermost_labeled_statement,
            )
        };
        self.exit_subtree(ancestor_facts);
        result
    }

    // forawait.go:344
    fn convert_for_of_statement_head(
        &self,
        node: P<Node>,
        bound_value: P<Node>,
        non_user_code: P<Node>,
    ) -> P<Node> {
        let f = self.factory();
        let n = node.as_for_in_or_of_statement();
        let value = f.new_temp_variable();
        self.emit_context().add_variable_declaration(value);
        let iterator_value_expression = f.new_assignment_expression(value, bound_value);
        let iterator_value_statement = f.new_expression_statement(iterator_value_expression);
        self.emit_context().set_source_map_range(iterator_value_statement, n.expression.loc());

        let exit_non_user_code_expression = f
            .new_assignment_expression(non_user_code, f.new_keyword_expression(Kind::FalseKeyword));
        let exit_non_user_code_statement =
            f.new_expression_statement(exit_non_user_code_expression);
        self.emit_context().set_source_map_range(exit_non_user_code_statement, n.expression.loc());

        let mut statements = vec![iterator_value_statement, exit_non_user_code_statement];
        let binding = f.create_for_of_binding_statement(n.initializer, value);
        statements.push(self.visitor().visit_node(Some(binding)).unwrap());

        // Go: zero-valued core.TextRange (0, 0) when the statement is not a block.
        let mut body_location = TextRange::new(0, 0);
        let mut statements_location = TextRange::new(0, 0);
        let statement = self.visitor().visit_embedded_statement(Some(n.statement)).unwrap();
        if ast::is_block(statement) {
            statements.extend_from_slice(statement.statements());
            body_location = statement.loc();
            statements_location = statement.statement_list().unwrap().loc.get();
        } else {
            statements.push(statement);
        }

        let stmt_list = f.new_node_list(statements);
        stmt_list.loc.set(statements_location);
        let block = f.new_block(stmt_list, true);
        block.set_loc(body_location);
        block
    }

    // forawait.go:381
    fn create_downlevel_await(&self, expression: P<Node>) -> P<Node> {
        if self.enclosing_function_flags.get().intersects(FunctionFlags::Generator) {
            return self.factory().new_yield_expression(
                None, /*asteriskToken*/
                Some(self.factory().new_await_helper(expression)),
            );
        }
        self.factory().new_await_expression(expression)
    }

    // forawait.go:391
    fn transform_for_await_of_statement(
        &self,
        node: P<Node>,
        outermost_labeled_statement: Option<P<Node>>,
        ancestor_facts: ForAwaitHierarchyFacts,
    ) -> P<Node> {
        let f = self.factory();
        let ec = self.emit_context();
        let n = node.as_for_in_or_of_statement();
        let expression = self.visitor().visit_node(Some(n.expression)).unwrap();

        let iterator = if ast::is_identifier(expression) {
            f.new_generated_name_for_node(expression)
        } else {
            f.new_temp_variable()
        };

        let result = if ast::is_identifier(expression) {
            f.new_generated_name_for_node(iterator)
        } else {
            f.new_temp_variable()
        };

        let non_user_code = f.new_temp_variable();
        let done = f.new_temp_variable();
        ec.add_variable_declaration(done);
        let error_record = f.new_unique_name("e");
        let catch_variable = f.new_generated_name_for_node(error_record);
        let return_method = f.new_temp_variable();
        let call_values = f.new_async_values_helper(expression);
        call_values.set_loc(n.expression.loc());
        let call_next = f.new_call_expression(
            f.new_property_access_expression(
                iterator,
                None,
                f.new_identifier("next"),
                NodeFlags::None,
            ),
            None,
            None,
            f.new_node_list(vec![]),
            NodeFlags::None,
        );
        let get_done = f.new_property_access_expression(
            result,
            None,
            f.new_identifier("done"),
            NodeFlags::None,
        );
        let get_value = f.new_property_access_expression(
            result,
            None,
            f.new_identifier("value"),
            NodeFlags::None,
        );
        let call_return = f.new_function_call_call(return_method, Some(iterator), &[]);

        ec.add_variable_declaration(error_record);
        ec.add_variable_declaration(return_method);

        // if we are enclosed in an outer loop ensure we reset 'errorRecord' per each iteration
        let initializer = if ancestor_facts & FOR_AWAIT_HIERARCHY_FACTS_ITERATION_CONTAINER != 0 {
            f.inline_expressions(&[
                f.new_assignment_expression(error_record, f.new_void_zero_expression()),
                call_values,
            ])
            .unwrap()
        } else {
            call_values
        };

        // Build the for statement
        let iterator_decl = f.new_variable_declaration(iterator, None, None, Some(initializer));
        iterator_decl.set_loc(n.expression.loc());
        let var_decl_list = f.new_variable_declaration_list(
            f.new_node_list(vec![
                f.new_variable_declaration(
                    non_user_code,
                    None,
                    None,
                    Some(f.new_keyword_expression(Kind::TrueKeyword)),
                ),
                iterator_decl,
                f.new_variable_declaration(result, None, None, None),
            ]),
            NodeFlags::None,
        );
        var_decl_list.set_loc(n.expression.loc());

        let condition = f
            .inline_expressions(&[
                f.new_assignment_expression(result, self.create_downlevel_await(call_next)),
                f.new_assignment_expression(done, get_done),
                f.new_prefix_unary_expression(Kind::ExclamationToken, done),
            ])
            .unwrap();

        let incrementor =
            f.new_assignment_expression(non_user_code, f.new_keyword_expression(Kind::TrueKeyword));

        let for_statement = f.new_for_statement(
            Some(var_decl_list),
            Some(condition),
            Some(incrementor),
            self.convert_for_of_statement_head(node, get_value, non_user_code),
        );
        for_statement.set_loc(node.loc());
        ec.add_emit_flags(for_statement, EmitFlags::NoTokenTrailingSourceMaps);
        ec.set_original(for_statement, node);

        // Build the try/catch/finally
        let try_block = f.new_block(
            f.new_node_list(vec![
                f.restore_enclosing_label(for_statement, outermost_labeled_statement),
            ]),
            true,
        );

        // catch clause: { e_1 = { error: e_2 }; }
        let catch_body = f.new_block(
            f.new_node_list(vec![f.new_expression_statement(f.new_assignment_expression(
                error_record,
                f.new_object_literal_expression(
                    f.new_node_list(vec![f.new_property_assignment(
                        None,
                        f.new_identifier("error"),
                        None,
                        None,
                        catch_variable,
                    )]),
                    false,
                ),
            ))]),
            false,
        );
        ec.add_emit_flags(catch_body, EmitFlags::SingleLine);
        let catch_clause = f.new_catch_clause(
            Some(f.new_variable_declaration(catch_variable, None, None, None)),
            catch_body,
        );

        // finally block
        // inner try: if (!nonUserCode && !done && (returnMethod = iterator.return)) await returnMethod.call(iterator);
        let inner_if_condition = f.new_binary_expression(
            None,
            f.new_binary_expression(
                None,
                f.new_prefix_unary_expression(Kind::ExclamationToken, non_user_code),
                None,
                f.new_token(Kind::AmpersandAmpersandToken),
                f.new_prefix_unary_expression(Kind::ExclamationToken, done),
            ),
            None,
            f.new_token(Kind::AmpersandAmpersandToken),
            f.new_assignment_expression(
                return_method,
                f.new_property_access_expression(
                    iterator,
                    None,
                    f.new_identifier("return"),
                    NodeFlags::None,
                ),
            ),
        );
        let inner_if_statement = f.new_if_statement(
            inner_if_condition,
            f.new_expression_statement(self.create_downlevel_await(call_return)),
            None,
        );
        ec.add_emit_flags(inner_if_statement, EmitFlags::SingleLine);

        let inner_try_block = f.new_block(f.new_node_list(vec![inner_if_statement]), false);

        // inner finally: if (errorRecord) throw errorRecord.error;
        let inner_finally_if = f.new_if_statement(
            error_record,
            f.new_throw_statement(f.new_property_access_expression(
                error_record,
                None,
                f.new_identifier("error"),
                NodeFlags::None,
            )),
            None,
        );
        ec.add_emit_flags(inner_finally_if, EmitFlags::SingleLine);
        let inner_finally_block = f.new_block(f.new_node_list(vec![inner_finally_if]), false);
        ec.add_emit_flags(inner_finally_block, EmitFlags::SingleLine);

        let inner_try_statement =
            f.new_try_statement(inner_try_block, None, Some(inner_finally_block));
        let finally_block = f.new_block(f.new_node_list(vec![inner_try_statement]), true);

        f.new_try_statement(try_block, Some(catch_clause), Some(finally_block))
    }

    // forawait.go:537
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Visitor branches share an optional node result so sibling branches can remove a node."
    )]
    fn visit_constructor_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        let saved_enclosing_function_flags =
            self.enclosing_function_flags.replace(ast::get_function_flags(Some(node)));
        let parameters =
            self.emit_context().visit_parameters(node.parameter_list(), &mut self.visitor());
        let body = self.emit_context().visit_function_body(node.body(), &mut self.visitor());
        let updated = self.factory().update_constructor_declaration(
            node,
            node.modifiers(),
            None, /*typeParameters*/
            parameters,
            None, /*returnType*/
            None, /*fullSignature*/
            body,
        );
        self.enclosing_function_flags.set(saved_enclosing_function_flags);
        Some(updated)
    }

    // forawait.go:554
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Visitor branches share an optional node result so sibling branches can remove a node."
    )]
    fn visit_get_accessor_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        let saved_enclosing_function_flags =
            self.enclosing_function_flags.replace(ast::get_function_flags(Some(node)));
        let name = self.visitor().visit_node(node.name()).unwrap();
        let parameters =
            self.emit_context().visit_parameters(node.parameter_list(), &mut self.visitor());
        let body = self.emit_context().visit_function_body(node.body(), &mut self.visitor());
        let updated = self.factory().update_get_accessor_declaration(
            node,
            node.modifiers(),
            name,
            None, /*typeParameters*/
            parameters,
            None, /*returnType*/
            None, /*fullSignature*/
            body,
        );
        self.enclosing_function_flags.set(saved_enclosing_function_flags);
        Some(updated)
    }

    // forawait.go:572
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Visitor branches share an optional node result so sibling branches can remove a node."
    )]
    fn visit_set_accessor_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        let saved_enclosing_function_flags =
            self.enclosing_function_flags.replace(ast::get_function_flags(Some(node)));
        let name = self.visitor().visit_node(node.name()).unwrap();
        let parameters =
            self.emit_context().visit_parameters(node.parameter_list(), &mut self.visitor());
        let body = self.emit_context().visit_function_body(node.body(), &mut self.visitor());
        let updated = self.factory().update_set_accessor_declaration(
            node,
            node.modifiers(),
            name,
            None, /*typeParameters*/
            parameters,
            None, /*returnType*/
            None, /*fullSignature*/
            body,
        );
        self.enclosing_function_flags.set(saved_enclosing_function_flags);
        Some(updated)
    }

    // forawait.go:590
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Visitor branches share an optional node result so sibling branches can remove a node."
    )]
    fn visit_method_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        let saved_enclosing_function_flags =
            self.enclosing_function_flags.replace(ast::get_function_flags(Some(node)));
        let flags = self.enclosing_function_flags.get();

        let modifiers = if flags.intersects(FunctionFlags::Generator) {
            self.visit_modifiers_no_async(node.modifiers())
        } else {
            node.modifiers()
        };

        let asterisk_token = if flags.intersects(FunctionFlags::Async) {
            None
        } else {
            node.as_method_declaration().asterisk_token()
        };

        let (parameters, body) = if flags.intersects(FunctionFlags::Async)
            && flags.intersects(FunctionFlags::Generator)
        {
            (
                Some(self.transform_async_generator_function_parameter_list(node)),
                Some(self.transform_async_generator_function_body(node)),
            )
        } else {
            (
                self.emit_context().visit_parameters(node.parameter_list(), &mut self.visitor()),
                self.emit_context().visit_function_body(node.body(), &mut self.visitor()),
            )
        };

        let name = self.visitor().visit_node(node.name()).unwrap();
        let updated = self.factory().update_method_declaration(
            node,
            modifiers,
            asterisk_token,
            name,
            None, /*postfixToken*/
            None, /*typeParameters*/
            parameters,
            None, /*returnType*/
            None, /*fullSignature*/
            body,
        );
        self.enclosing_function_flags.set(saved_enclosing_function_flags);
        Some(updated)
    }

    // forawait.go:635
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Visitor branches share an optional node result so sibling branches can remove a node."
    )]
    fn visit_function_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        let saved_enclosing_function_flags =
            self.enclosing_function_flags.replace(ast::get_function_flags(Some(node)));
        let flags = self.enclosing_function_flags.get();

        let modifiers = if flags.intersects(FunctionFlags::Generator) {
            self.visit_modifiers_no_async(node.modifiers())
        } else {
            node.modifiers()
        };

        let asterisk_token = if flags.intersects(FunctionFlags::Async) {
            None
        } else {
            node.as_function_declaration().asterisk_token()
        };

        let (parameters, body) = if flags.intersects(FunctionFlags::Async)
            && flags.intersects(FunctionFlags::Generator)
        {
            (
                Some(self.transform_async_generator_function_parameter_list(node)),
                Some(self.transform_async_generator_function_body(node)),
            )
        } else {
            (
                self.emit_context().visit_parameters(node.parameter_list(), &mut self.visitor()),
                self.emit_context().visit_function_body(node.body(), &mut self.visitor()),
            )
        };

        let updated = self.factory().update_function_declaration(
            node,
            modifiers,
            asterisk_token,
            node.name(),
            None, /*typeParameters*/
            parameters,
            None, /*returnType*/
            None, /*fullSignature*/
            body,
        );
        self.enclosing_function_flags.set(saved_enclosing_function_flags);
        Some(updated)
    }

    // forawait.go:680
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Visitor branches share an optional node result so sibling branches can remove a node."
    )]
    fn visit_arrow_function(&self, node: P<Node>) -> Option<P<Node>> {
        let saved_enclosing_function_flags =
            self.enclosing_function_flags.replace(ast::get_function_flags(Some(node)));
        let parameters =
            self.emit_context().visit_parameters(node.parameter_list(), &mut self.visitor());
        let body = self.emit_context().visit_function_body(node.body(), &mut self.visitor());
        let updated = self.factory().update_arrow_function(
            node,
            node.modifiers(),
            None, /*typeParameters*/
            parameters,
            None, /*returnType*/
            None, /*fullSignature*/
            node.as_arrow_function().equals_greater_than_token,
            body,
        );
        self.enclosing_function_flags.set(saved_enclosing_function_flags);
        Some(updated)
    }

    // forawait.go:698
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Visitor branches share an optional node result so sibling branches can remove a node."
    )]
    fn visit_function_expression(&self, node: P<Node>) -> Option<P<Node>> {
        let saved_enclosing_function_flags =
            self.enclosing_function_flags.replace(ast::get_function_flags(Some(node)));
        let flags = self.enclosing_function_flags.get();

        let modifiers = if flags.intersects(FunctionFlags::Generator) {
            self.visit_modifiers_no_async(node.modifiers())
        } else {
            node.modifiers()
        };

        let asterisk_token = if flags.intersects(FunctionFlags::Async) {
            None
        } else {
            node.as_function_expression().asterisk_token()
        };

        let (parameters, body) = if flags.intersects(FunctionFlags::Async)
            && flags.intersects(FunctionFlags::Generator)
        {
            (
                Some(self.transform_async_generator_function_parameter_list(node)),
                Some(self.transform_async_generator_function_body(node)),
            )
        } else {
            (
                self.emit_context().visit_parameters(node.parameter_list(), &mut self.visitor()),
                self.emit_context().visit_function_body(node.body(), &mut self.visitor()),
            )
        };

        let updated = self.factory().update_function_expression(
            node,
            modifiers,
            asterisk_token,
            node.name(),
            None, /*typeParameters*/
            parameters,
            None, /*returnType*/
            None, /*fullSignature*/
            body,
        );
        self.enclosing_function_flags.set(saved_enclosing_function_flags);
        Some(updated)
    }

    // forawait.go:743
    fn transform_async_generator_function_parameter_list(&self, node: P<Node>) -> P<NodeList> {
        if is_simple_parameter_list(node.parameters()) {
            return self
                .emit_context()
                .visit_parameters(node.parameter_list(), &mut self.visitor())
                .unwrap();
        }
        // Add fixed parameters to preserve the function's `length` property.
        let f = self.factory();
        let mut new_parameters: Vec<P<Node>> = Vec::new();
        new_parameters.extend(node.parameters().iter().map_while(|&parameter| {
            let param = parameter.as_parameter_declaration();
            if param.initializer().is_some() || param.dot_dot_dot_token().is_some() {
                return None;
            }
            let new_parameter = f.new_parameter_declaration(
                None,
                None,
                f.new_generated_name_for_node_ex(
                    param.name(),
                    printer::AutoGenerateOptions {
                        flags: GeneratedIdentifierFlags::ReservedInNestedScopes,
                        ..Default::default()
                    },
                ),
                None,
                None,
                None,
            );
            Some(new_parameter)
        }));
        let new_parameters_array = f.new_node_list(new_parameters);
        new_parameters_array.loc.set(node.parameter_list().unwrap().loc.get());
        new_parameters_array
    }

    // forawait.go:770
    fn transform_async_generator_function_body(&self, node: P<Node>) -> P<Node> {
        let f = self.factory();
        let ec = self.emit_context();
        let s = self.sas();
        let mut inner_parameters: Option<P<NodeList>> = None;
        if !is_simple_parameter_list(node.parameters()) {
            inner_parameters = ec.visit_parameters(node.parameter_list(), &mut self.visitor());
        }

        let auto_generate_options = printer::AutoGenerateOptions {
            flags: GeneratedIdentifierFlags::Optimistic | GeneratedIdentifierFlags::FileLevel,
            ..Default::default()
        };
        let saved_captured_super_properties =
            s.captured_super_properties.replace(Some(OrderedSet::default()));
        let saved_has_super_element_access = s.has_super_element_access.replace(false);
        let saved_has_super_property_assignment = s.has_super_property_assignment.replace(false);
        let saved_super_binding =
            s.super_binding.replace(Some(f.new_unique_name_ex("_super", auto_generate_options)));
        let saved_super_index_binding = s
            .super_index_binding
            .replace(Some(f.new_unique_name_ex("_superIndex", auto_generate_options)));

        let body = node.body().unwrap();
        let mut async_body = f.update_block(
            body,
            self.visitor().visit_nodes(body.statement_list()).unwrap(),
            body.as_block().multi_line,
        );
        async_body = f.update_block(
            async_body,
            ec.end_and_merge_variable_environment_list(async_body.statement_list()).unwrap(),
            async_body.as_block().multi_line,
        );

        // Substitute super property accesses with _super/_superIndex helpers
        let emit_super_helpers = !s.captured_super_properties.borrow().as_ref().unwrap().is_empty()
            || s.has_super_element_access.get();
        if emit_super_helpers {
            async_body = s.substitute_super_accesses_in_body(async_body);
        }

        let inner_params = match inner_parameters {
            Some(inner_parameters) => inner_parameters,
            None => f.new_node_list(vec![]),
        };

        let name = node.name().map(|name| f.new_generated_name_for_node(name));

        let generator_func = f.new_function_expression(
            None, /*modifiers*/
            Some(f.new_token(Kind::AsteriskToken)),
            name,
            None, /*typeParameters*/
            Some(inner_params),
            None, /*returnType*/
            None, /*fullSignature*/
            Some(async_body),
        );

        let return_statement = f.new_return_statement(Some(f.new_async_generator_helper(
            generator_func,
            self.for_await_hierarchy_facts.get() & FOR_AWAIT_HIERARCHY_FACTS_HAS_LEXICAL_THIS != 0,
        )));

        ec.start_variable_environment();
        if emit_super_helpers && !s.captured_super_properties.borrow().as_ref().unwrap().is_empty()
        {
            ec.add_initialization_statement(s.create_super_access_variable_statement());
        }

        let outer_statements = vec![return_statement];

        let block = f.update_block(
            body,
            ec.end_and_merge_variable_environment_list(Some(f.new_node_list(outer_statements)))
                .unwrap(),
            body.as_block().multi_line,
        );

        if emit_super_helpers && s.has_super_element_access.get() {
            if s.has_super_property_assignment.get() {
                ec.add_emit_helper(
                    block,
                    &[SP::from_static(&printer::ADVANCED_ASYNC_SUPER_HELPER)],
                );
            } else {
                ec.add_emit_helper(block, &[SP::from_static(&printer::ASYNC_SUPER_HELPER)]);
            }
        }

        *s.captured_super_properties.borrow_mut() = saved_captured_super_properties;
        s.has_super_element_access.set(saved_has_super_element_access);
        s.has_super_property_assignment.set(saved_has_super_property_assignment);
        s.super_binding.set(saved_super_binding);
        s.super_index_binding.set(saved_super_index_binding);

        block
    }
}

// unwrapInnermostStatementOfLabel follows LabeledStatement chains to find the innermost statement.
// forawait.go:309
fn unwrap_innermost_statement_of_label(mut node: P<Node>) -> P<Node> {
    loop {
        let statement = node.as_labeled_statement().statement;
        if statement.kind() != Kind::LabeledStatement {
            return statement;
        }
        node = statement;
    }
}
