use crate::*;
use printer::{EmitFlags, GeneratedIdentifierFlags};
use tsrs_core::collections::OrderedSet;

use super::SuperAccessState;

// async.go:13
type AsyncContextFlags = u32;

const ASYNC_CONTEXT_NON_TOP_LEVEL: AsyncContextFlags = 1 << 0;
const ASYNC_CONTEXT_HAS_LEXICAL_THIS: AsyncContextFlags = 1 << 1;

// async.go:20
#[derive(Clone, Copy, Default)]
struct LexicalArgumentsInfo {
    binding: Option<P<Node>>,
    used: bool,
}

// async.go:25
struct AsyncTransformer {
    base: Transformer,
    super_access_state: SuperAccessState,

    context_flags: Cell<AsyncContextFlags>,

    enclosing_function_parameter_names: RefCell<Option<FxHashSet<String>>>,
    lexical_arguments: Cell<LexicalArgumentsInfo>,

    async_body_visitor: OnceCell<NodeVisitor>,
    fallback_node_visitor: OnceCell<NodeVisitor>,
}

// async.go:37
#[expect(
    clippy::unnecessary_wraps,
    reason = "Transformer factories share an optional result so disabled transforms can return None."
)]
pub fn new_async_transformer(opt: &TransformOptions) -> Option<P<Transformer>> {
    let tx = P::new(AsyncTransformer {
        base: Transformer::default(),
        super_access_state: SuperAccessState::default(),
        context_flags: Cell::new(0),
        enclosing_function_parameter_names: RefCell::new(None),
        lexical_arguments: Cell::new(LexicalArgumentsInfo::default()),
        async_body_visitor: OnceCell::new(),
        fallback_node_visitor: OnceCell::new(),
    });
    let result = tx.get().base.new_transformer(
        Rc::new(move |_: &mut NodeVisitor, n: P<Node>| tx.visit(n)),
        Some(opt.context),
    );
    tx.get().super_access_state.init_super_access_visitor(tx.emit_context(), tx.factory());
    let _ = tx.async_body_visitor.set(tx.emit_context().new_node_visitor(Rc::new(
        move |_: &mut NodeVisitor, n: P<Node>| tx.visit_async_body_node(n),
    )));
    let _ =
        tx.fallback_node_visitor.set(tx.emit_context().new_node_visitor(Rc::new(
            move |_: &mut NodeVisitor, n: P<Node>| tx.visit_fallback(n),
        )));
    Some(result)
}

impl AsyncTransformer {
    fn emit_context(&self) -> P<EmitContext> {
        self.base.emit_context()
    }

    fn factory(&self) -> &'static printer::NodeFactory {
        self.base.factory()
    }

    fn visitor(&self) -> NodeVisitor {
        self.base.visitor()
    }

    fn async_body_visitor(&self) -> NodeVisitor {
        self.async_body_visitor.get().unwrap().clone()
    }

    fn fallback_node_visitor(&self) -> NodeVisitor {
        self.fallback_node_visitor.get().unwrap().clone()
    }

    fn sas(&self) -> &SuperAccessState {
        &self.super_access_state
    }

    fn new_super_binding_name(&self, text: &str) -> P<Node> {
        self.factory().new_unique_name_ex(
            text,
            printer::AutoGenerateOptions {
                flags: GeneratedIdentifierFlags::Optimistic | GeneratedIdentifierFlags::FileLevel,
                ..Default::default()
            },
        )
    }

    // async.go:47
    fn visit_source_file(&self, node: P<Node>) -> P<Node> {
        if node.as_source_file().is_declaration_file.get() {
            return node;
        }

        self.set_context_flag(ASYNC_CONTEXT_NON_TOP_LEVEL, false);
        self.set_context_flag(ASYNC_CONTEXT_HAS_LEXICAL_THIS, false);
        let visited = self.visitor().visit_each_child(Some(node)).unwrap();
        self.emit_context().add_emit_helper(visited, &self.emit_context().read_emit_helpers());
        visited
    }

    // async.go:59
    fn set_context_flag(&self, flag: AsyncContextFlags, val: bool) {
        if val {
            self.context_flags.set(self.context_flags.get() | flag);
        } else {
            self.context_flags.set(self.context_flags.get() & !flag);
        }
    }

    // async.go:67
    fn in_context(&self, flags: AsyncContextFlags) -> bool {
        self.context_flags.get() & flags != 0
    }

    // async.go:71
    fn in_top_level_context(&self) -> bool {
        !self.in_context(ASYNC_CONTEXT_NON_TOP_LEVEL)
    }

    // async.go:75
    fn in_has_lexical_this_context(&self) -> bool {
        self.in_context(ASYNC_CONTEXT_HAS_LEXICAL_THIS)
    }

    // async.go:79
    fn do_with_context(
        &self,
        flags: AsyncContextFlags,
        cb: fn(&AsyncTransformer, P<Node>) -> Option<P<Node>>,
        node: P<Node>,
    ) -> Option<P<Node>> {
        let flags_to_set = flags & !self.context_flags.get();
        if flags_to_set != 0 {
            self.set_context_flag(flags_to_set, true);
            let result = cb(self, node);
            self.set_context_flag(flags_to_set, false);
            return result;
        }
        cb(self, node)
    }

    // async.go:90
    fn visit_default(&self, node: P<Node>) -> Option<P<Node>> {
        self.visitor().visit_each_child(Some(node))
    }

    // async.go:94
    fn fallback_visitor(&self, node: P<Node>) -> Option<P<Node>> {
        if self.sas().captured_super_properties.borrow().is_none()
            && self.lexical_arguments.get().binding.is_none()
        {
            return Some(node);
        }
        self.sas().track_super_access(node);
        match node.kind() {
            Kind::FunctionExpression
            | Kind::FunctionDeclaration
            | Kind::MethodDeclaration
            | Kind::GetAccessor
            | Kind::SetAccessor
            | Kind::Constructor => {
                return Some(node);
            }
            Kind::Identifier => {
                let la = self.lexical_arguments.get();
                if la.binding.is_some()
                    && node.text() == "arguments"
                    && !ast::is_identifier_name(node)
                    && !ast::is_label_name(node)
                {
                    self.lexical_arguments
                        .set(LexicalArgumentsInfo { binding: la.binding, used: true });
                    return la.binding;
                }
            }
            _ => {}
        }
        // Parameter, BindingElement and VariableDeclaration fall through to visitEachChild.
        self.fallback_node_visitor().visit_each_child(Some(node))
    }

    // async.go:122
    fn visit_fallback(&self, node: P<Node>) -> Option<P<Node>> {
        self.fallback_visitor(node)
    }

    // async.go:126
    fn visit(&self, node: P<Node>) -> Option<P<Node>> {
        // Go: `defer tx.setContextFlag(asyncContextHasLexicalThis, true)`
        if self.emit_context().emit_flags(node).intersects(EmitFlags::NoLexicalThis)
            && self.in_has_lexical_this_context()
        {
            self.set_context_flag(ASYNC_CONTEXT_HAS_LEXICAL_THIS, false);
            let result = self.visit_worker(node);
            self.set_context_flag(ASYNC_CONTEXT_HAS_LEXICAL_THIS, true);
            return result;
        }
        self.visit_worker(node)
    }

    // async.go:126 (body of visit after the deferred restore is set up)
    fn visit_worker(&self, node: P<Node>) -> Option<P<Node>> {
        if !node
            .subtree_facts()
            .intersects(SubtreeFacts::ContainsAnyAwait | SubtreeFacts::ContainsAwait)
        {
            return self.fallback_visitor(node);
        }
        self.sas().track_super_access(node);
        match node.kind() {
            Kind::AsyncKeyword => {
                // ES2017 async modifier should be elided for targets < ES2017
                None
            }
            Kind::SourceFile => Some(self.visit_source_file(node)),
            Kind::AwaitExpression => Some(self.visit_await_expression(node)),
            Kind::MethodDeclaration => self.do_with_context(
                ASYNC_CONTEXT_NON_TOP_LEVEL | ASYNC_CONTEXT_HAS_LEXICAL_THIS,
                AsyncTransformer::visit_method_declaration,
                node,
            ),
            Kind::FunctionDeclaration => self.do_with_context(
                ASYNC_CONTEXT_NON_TOP_LEVEL | ASYNC_CONTEXT_HAS_LEXICAL_THIS,
                AsyncTransformer::visit_function_declaration,
                node,
            ),
            Kind::FunctionExpression => self.do_with_context(
                ASYNC_CONTEXT_NON_TOP_LEVEL | ASYNC_CONTEXT_HAS_LEXICAL_THIS,
                AsyncTransformer::visit_function_expression,
                node,
            ),
            Kind::ArrowFunction => self.do_with_context(
                ASYNC_CONTEXT_NON_TOP_LEVEL,
                AsyncTransformer::visit_arrow_function,
                node,
            ),
            Kind::GetAccessor => self.do_with_context(
                ASYNC_CONTEXT_NON_TOP_LEVEL | ASYNC_CONTEXT_HAS_LEXICAL_THIS,
                AsyncTransformer::visit_get_accessor_declaration,
                node,
            ),
            Kind::SetAccessor => self.do_with_context(
                ASYNC_CONTEXT_NON_TOP_LEVEL | ASYNC_CONTEXT_HAS_LEXICAL_THIS,
                AsyncTransformer::visit_set_accessor_declaration,
                node,
            ),
            Kind::Constructor => self.do_with_context(
                ASYNC_CONTEXT_NON_TOP_LEVEL | ASYNC_CONTEXT_HAS_LEXICAL_THIS,
                AsyncTransformer::visit_constructor_declaration,
                node,
            ),
            Kind::ClassDeclaration | Kind::ClassExpression => self.do_with_context(
                ASYNC_CONTEXT_NON_TOP_LEVEL | ASYNC_CONTEXT_HAS_LEXICAL_THIS,
                AsyncTransformer::visit_default,
                node,
            ),
            _ => self.visitor().visit_each_child(Some(node)),
        }
    }

    // async.go:167
    fn visit_async_body_node(&self, node: P<Node>) -> Option<P<Node>> {
        if is_node_with_possible_hoisted_declaration(node) {
            match node.kind() {
                Kind::VariableStatement => {
                    return self.visit_variable_statement_in_async_body(node);
                }
                Kind::ForStatement => return Some(self.visit_for_statement_in_async_body(node)),
                Kind::ForInStatement => {
                    return Some(self.visit_for_in_statement_in_async_body(node));
                }
                Kind::ForOfStatement => {
                    return Some(self.visit_for_of_statement_in_async_body(node));
                }
                Kind::CatchClause => return self.visit_catch_clause_in_async_body(node),
                Kind::Block
                | Kind::SwitchStatement
                | Kind::CaseBlock
                | Kind::CaseClause
                | Kind::DefaultClause
                | Kind::TryStatement
                | Kind::DoStatement
                | Kind::WhileStatement
                | Kind::IfStatement
                | Kind::WithStatement
                | Kind::LabeledStatement => {
                    return self.async_body_visitor().visit_each_child(Some(node));
                }
                _ => {}
            }
        }
        self.visit(node)
    }

    // async.go:197
    fn visit_catch_clause_in_async_body(&self, node: P<Node>) -> Option<P<Node>> {
        let mut catch_clause_names: FxHashSet<String> = FxHashSet::default();
        if let Some(variable_declaration) = node.as_catch_clause().variable_declaration {
            Self::record_declaration_name(variable_declaration, &mut catch_clause_names);
        }

        // names declared in a catch variable are block scoped
        let mut catch_clause_unshadowed_names: Option<FxHashSet<String>> = None;
        #[expect(
            clippy::iter_over_hash_type,
            reason = "removes names from a set; the set it ends with does not depend on the order"
        )]
        for escaped_name in &catch_clause_names {
            let enclosing = self.enclosing_function_parameter_names.borrow();
            if let Some(enclosing) = enclosing.as_ref()
                && enclosing.contains(escaped_name)
            {
                if catch_clause_unshadowed_names.is_none() {
                    catch_clause_unshadowed_names = Some(enclosing.clone());
                }
                catch_clause_unshadowed_names.as_mut().unwrap().remove(escaped_name);
            }
        }

        if let Some(catch_clause_unshadowed_names) = catch_clause_unshadowed_names {
            let saved_enclosing_function_parameter_names = self
                .enclosing_function_parameter_names
                .replace(Some(catch_clause_unshadowed_names));
            let result = self.async_body_visitor().visit_each_child(Some(node));
            *self.enclosing_function_parameter_names.borrow_mut() =
                saved_enclosing_function_parameter_names;
            return result;
        }
        self.async_body_visitor().visit_each_child(Some(node))
    }

    // async.go:225
    fn visit_variable_statement_in_async_body(&self, node: P<Node>) -> Option<P<Node>> {
        let decl_list = node.as_variable_statement().declaration_list;
        if self.is_variable_declaration_list_with_colliding_name(Some(decl_list)) {
            let expression =
                self.visit_variable_declaration_list_with_colliding_names(decl_list, false);
            if let Some(expression) = expression {
                return Some(self.factory().new_expression_statement(expression));
            }
            return None;
        }
        self.visitor().visit_each_child(Some(node))
    }

    // async.go:237
    fn visit_for_in_statement_in_async_body(&self, node: P<Node>) -> P<Node> {
        let n = node.as_for_in_or_of_statement();
        let visited_initializer =
            if self.is_variable_declaration_list_with_colliding_name(Some(n.initializer)) {
                self.visit_variable_declaration_list_with_colliding_names(n.initializer, true)
            } else {
                self.visitor().visit_node(Some(n.initializer))
            };

        let expression = self.visitor().visit_node(Some(n.expression));
        let statement = self.async_body_visitor().visit_embedded_statement(Some(n.statement()));
        self.factory().update_for_in_or_of_statement(
            node,
            None, /*awaitModifier*/
            visited_initializer.unwrap(),
            expression.unwrap(),
            statement.unwrap(),
        )
    }

    // async.go:253
    fn visit_for_of_statement_in_async_body(&self, node: P<Node>) -> P<Node> {
        let n = node.as_for_in_or_of_statement();
        let visited_initializer =
            if self.is_variable_declaration_list_with_colliding_name(Some(n.initializer)) {
                self.visit_variable_declaration_list_with_colliding_names(n.initializer, true)
            } else {
                self.visitor().visit_node(Some(n.initializer))
            };

        let await_modifier = self.visitor().visit_node(n.await_modifier);
        let expression = self.visitor().visit_node(Some(n.expression));
        let statement = self.async_body_visitor().visit_embedded_statement(Some(n.statement()));
        self.factory().update_for_in_or_of_statement(
            node,
            await_modifier,
            visited_initializer.unwrap(),
            expression.unwrap(),
            statement.unwrap(),
        )
    }

    // async.go:269
    fn visit_for_statement_in_async_body(&self, node: P<Node>) -> P<Node> {
        let n = node.as_for_statement();
        let initializer = n.initializer;
        let visited_initializer = if let Some(initializer) = initializer
            && self.is_variable_declaration_list_with_colliding_name(Some(initializer))
        {
            self.visit_variable_declaration_list_with_colliding_names(initializer, false)
        } else {
            self.visitor().visit_node(n.initializer)
        };

        let condition = self.visitor().visit_node(n.condition);
        let incrementor = self.visitor().visit_node(n.incrementor);
        let statement = self.async_body_visitor().visit_embedded_statement(Some(n.statement()));
        self.factory().update_for_statement(
            node,
            visited_initializer,
            condition,
            incrementor,
            statement.unwrap(),
        )
    }

    // visitAwaitExpression visits an AwaitExpression node.
    //
    // This function will be called any time a ES2017 await expression is encountered.
    // async.go:290
    fn visit_await_expression(&self, node: P<Node>) -> P<Node> {
        // do not downlevel a top-level await as it is module syntax...
        if self.in_top_level_context() {
            return self.visitor().visit_each_child(Some(node)).unwrap();
        }
        let yield_expr = self.factory().new_yield_expression(
            None, /*asteriskToken*/
            self.visitor().visit_node(Some(node.as_await_expression().expression)),
        );
        yield_expr.set_loc(node.loc());
        self.emit_context().set_original(yield_expr, node);
        yield_expr
    }

    // async.go:304
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Visitor branches share an optional node result so sibling branches can remove a node."
    )]
    fn visit_constructor_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        let saved_lexical_arguments =
            self.lexical_arguments.replace(LexicalArgumentsInfo::default());
        let modifiers = self.visitor().visit_modifiers(node.modifiers());
        let parameters =
            self.emit_context().visit_parameters(node.parameter_list(), &mut self.visitor());
        let body = self.transform_method_body(node);
        let updated = self.factory().update_constructor_declaration(
            node,
            modifiers,
            None, /*typeParameters*/
            parameters,
            None, /*returnType*/
            None, /*fullSignature*/
            Some(body),
        );
        self.lexical_arguments.set(saved_lexical_arguments);
        Some(updated)
    }

    // visitMethodDeclaration visits a MethodDeclaration node.
    //
    // This function will be called when one of the following conditions are met:
    // - The node is marked as async
    // async.go:325
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Visitor branches share an optional node result so sibling branches can remove a node."
    )]
    fn visit_method_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        let function_flags = ast::get_function_flags(Some(node));
        let saved_lexical_arguments =
            self.lexical_arguments.replace(LexicalArgumentsInfo::default());

        let parameters;
        let body;
        if function_flags.intersects(FunctionFlags::Async) {
            parameters = Some(self.transform_async_function_parameter_list(node));
            body = self.transform_async_function_body(node, parameters.unwrap());
        } else {
            parameters =
                self.emit_context().visit_parameters(node.parameter_list(), &mut self.visitor());
            body = self.transform_method_body(node);
        }

        let modifiers = self.visitor().visit_modifiers(node.modifiers());
        let updated = self.factory().update_method_declaration(
            node,
            modifiers,
            node.as_method_declaration().asterisk_token(),
            node.name().unwrap(),
            None, /*postfixToken*/
            None, /*typeParameters*/
            parameters,
            None, /*returnType*/
            None, /*fullSignature*/
            Some(body),
        );
        self.lexical_arguments.set(saved_lexical_arguments);
        Some(updated)
    }

    // async.go:358
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Visitor branches share an optional node result so sibling branches can remove a node."
    )]
    fn visit_get_accessor_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        let saved_lexical_arguments =
            self.lexical_arguments.replace(LexicalArgumentsInfo::default());
        let modifiers = self.visitor().visit_modifiers(node.modifiers());
        let parameters =
            self.emit_context().visit_parameters(node.parameter_list(), &mut self.visitor());
        let body = self.transform_method_body(node);
        let updated = self.factory().update_get_accessor_declaration(
            node,
            modifiers,
            node.name().unwrap(),
            None, /*typeParameters*/
            parameters,
            None, /*returnType*/
            None, /*fullSignature*/
            Some(body),
        );
        self.lexical_arguments.set(saved_lexical_arguments);
        Some(updated)
    }

    // async.go:376
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Visitor branches share an optional node result so sibling branches can remove a node."
    )]
    fn visit_set_accessor_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        let saved_lexical_arguments =
            self.lexical_arguments.replace(LexicalArgumentsInfo::default());
        let modifiers = self.visitor().visit_modifiers(node.modifiers());
        let parameters =
            self.emit_context().visit_parameters(node.parameter_list(), &mut self.visitor());
        let body = self.transform_method_body(node);
        let updated = self.factory().update_set_accessor_declaration(
            node,
            modifiers,
            node.name().unwrap(),
            None, /*typeParameters*/
            parameters,
            None, /*returnType*/
            None, /*fullSignature*/
            Some(body),
        );
        self.lexical_arguments.set(saved_lexical_arguments);
        Some(updated)
    }

    // visitFunctionDeclaration visits a FunctionDeclaration node.
    //
    // This function will be called when one of the following conditions are met:
    // - The node is marked async
    // async.go:398
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Visitor branches share an optional node result so sibling branches can remove a node."
    )]
    fn visit_function_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        let function_flags = ast::get_function_flags(Some(node));
        let saved_lexical_arguments =
            self.lexical_arguments.replace(LexicalArgumentsInfo::default());

        let parameters;
        let body;
        if function_flags.intersects(FunctionFlags::Async) {
            parameters = Some(self.transform_async_function_parameter_list(node));
            body = Some(self.transform_async_function_body(node, parameters.unwrap()));
        } else {
            parameters =
                self.emit_context().visit_parameters(node.parameter_list(), &mut self.visitor());
            body = self.emit_context().visit_function_body(node.body(), &mut self.visitor());
        }

        let modifiers = self.visitor().visit_modifiers(node.modifiers());
        let name = self.visitor().visit_node(node.name());
        let updated = self.factory().update_function_declaration(
            node,
            modifiers,
            node.as_function_declaration().asterisk_token(),
            name,
            None, /*typeParameters*/
            parameters,
            None, /*returnType*/
            None, /*fullSignature*/
            body,
        );
        self.lexical_arguments.set(saved_lexical_arguments);
        Some(updated)
    }

    // visitFunctionExpression visits a FunctionExpression node.
    //
    // This function will be called when one of the following conditions are met:
    // - The node is marked async
    // async.go:435
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Visitor branches share an optional node result so sibling branches can remove a node."
    )]
    fn visit_function_expression(&self, node: P<Node>) -> Option<P<Node>> {
        let function_flags = ast::get_function_flags(Some(node));
        let saved_lexical_arguments =
            self.lexical_arguments.replace(LexicalArgumentsInfo::default());

        let parameters;
        let body;
        if function_flags.intersects(FunctionFlags::Async) {
            parameters = Some(self.transform_async_function_parameter_list(node));
            body = Some(self.transform_async_function_body(node, parameters.unwrap()));
        } else {
            parameters =
                self.emit_context().visit_parameters(node.parameter_list(), &mut self.visitor());
            body = self.emit_context().visit_function_body(node.body(), &mut self.visitor());
        }

        let modifiers = self.visitor().visit_modifiers(node.modifiers());
        let name = self.visitor().visit_node(node.name());
        let updated = self.factory().update_function_expression(
            node,
            modifiers,
            node.as_function_expression().asterisk_token(),
            name,
            None, /*typeParameters*/
            parameters,
            None, /*returnType*/
            None, /*fullSignature*/
            body,
        );
        self.lexical_arguments.set(saved_lexical_arguments);
        Some(updated)
    }

    // visitArrowFunction visits an ArrowFunction.
    //
    // This function will be called when one of the following conditions are met:
    // - The node is marked async
    // async.go:472
    fn visit_arrow_function(&self, node: P<Node>) -> Option<P<Node>> {
        // `arguments` in class static blocks is always an error, but we preserve Strada's emit
        // behavior for baseline compatibility. In Strada, checker-based `isArgumentsLocalBinding`
        // returns false for `arguments` in static blocks (since the binding doesn't exist due to
        // the error), so the async transform leaves them untouched.
        if self.emit_context().emit_flags(node).intersects(EmitFlags::NoLexicalArguments) {
            let saved_lexical_arguments =
                self.lexical_arguments.replace(LexicalArgumentsInfo::default());
            // Go: `defer func() { tx.lexicalArguments = savedLexicalArguments }()`
            let result = self.visit_arrow_function_worker(node);
            self.lexical_arguments.set(saved_lexical_arguments);
            return result;
        }
        self.visit_arrow_function_worker(node)
    }

    // async.go:472 (body of visitArrowFunction after the deferred restore is set up)
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Visitor branches share an optional node result so sibling branches can remove a node."
    )]
    fn visit_arrow_function_worker(&self, node: P<Node>) -> Option<P<Node>> {
        let function_flags = ast::get_function_flags(Some(node));

        let parameters;
        let body;
        if function_flags.intersects(FunctionFlags::Async) {
            parameters = Some(self.transform_async_function_parameter_list(node));
            body = Some(self.transform_async_function_body(node, parameters.unwrap()));
        } else {
            parameters =
                self.emit_context().visit_parameters(node.parameter_list(), &mut self.visitor());
            body = self.emit_context().visit_function_body(node.body(), &mut self.visitor());
        }

        let modifiers = self.visitor().visit_modifiers(node.modifiers());
        Some(self.factory().update_arrow_function(
            node,
            modifiers,
            None, /*typeParameters*/
            parameters,
            None, /*returnType*/
            None, /*fullSignature*/
            node.as_arrow_function().equals_greater_than_token,
            body,
        ))
    }

    // async.go:508
    fn record_declaration_name(node: P<Node>, names: &mut FxHashSet<String>) {
        let Some(name) = node.name() else {
            return;
        };
        if ast::is_identifier(name) {
            names.insert(name.text().to_string());
        } else if ast::is_binding_pattern(name) {
            for &element in name.as_binding_pattern().elements.nodes() {
                if !ast::is_omitted_expression(element) {
                    Self::record_declaration_name(element, names);
                }
            }
        }
    }

    // async.go:524
    fn is_variable_declaration_list_with_colliding_name(&self, node: Option<P<Node>>) -> bool {
        let Some(node) = node else {
            return false;
        };
        ast::is_variable_declaration_list(node)
            && !node.flags().intersects(NodeFlags::BlockScoped)
            && node
                .as_variable_declaration_list()
                .declarations
                .nodes()
                .iter()
                .any(|&d| self.collides_with_parameter_name(d))
    }

    // async.go:531
    fn visit_variable_declaration_list_with_colliding_names(
        &self,
        node: P<Node>,
        has_receiver: bool,
    ) -> Option<P<Node>> {
        self.hoist_variable_declaration_list(node);

        let mut variables: Vec<P<Node>> = Vec::new();
        variables.extend(
            node.as_variable_declaration_list().declarations.nodes().iter().filter_map(|&decl| {
                if decl.as_variable_declaration().initializer().is_some() {
                    Some(decl)
                } else {
                    None
                }
            }),
        );

        if variables.is_empty() {
            if has_receiver {
                let name =
                    node.as_variable_declaration_list().declarations.nodes()[0].name().unwrap();
                let target = if ast::is_binding_pattern(name) {
                    convert_binding_pattern_to_assignment_pattern(self.emit_context(), name)
                } else {
                    name
                };
                return self.visitor().visit_node(Some(target));
            }
            return None;
        }

        let mut expressions: Vec<P<Node>> = Vec::new();
        expressions.extend(
            variables.into_iter().map(|variable| self.transform_initialized_variable(variable)),
        );
        self.factory().inline_expressions(&expressions)
    }

    // async.go:563
    fn hoist_variable_declaration_list(&self, node: P<Node>) {
        for &decl in node.as_variable_declaration_list().declarations.nodes() {
            self.hoist_variable(decl);
        }
    }

    // async.go:569
    fn hoist_variable(&self, node: P<Node>) {
        let Some(name) = node.name() else {
            return;
        };
        if ast::is_identifier(name) {
            self.emit_context().add_variable_declaration(name);
        } else if ast::is_binding_pattern(name) {
            for &element in name.as_binding_pattern().elements.nodes() {
                if !ast::is_omitted_expression(element) {
                    self.hoist_variable(element);
                }
            }
        }
    }

    // async.go:585
    fn transform_initialized_variable(&self, node: P<Node>) -> P<Node> {
        let name = node.name().unwrap();
        let target = if ast::is_binding_pattern(name) {
            convert_binding_pattern_to_assignment_pattern(self.emit_context(), name)
        } else {
            name
        };
        let converted = self.factory().new_assignment_expression(
            target,
            node.as_variable_declaration().initializer().unwrap(),
        );
        self.emit_context().set_source_map_range(converted, node.loc());
        self.visitor().visit_node(Some(converted)).unwrap()
    }

    // async.go:597
    fn collides_with_parameter_name(&self, node: P<Node>) -> bool {
        let Some(name) = node.name() else {
            return false;
        };
        if ast::is_identifier(name) {
            return self
                .enclosing_function_parameter_names
                .borrow()
                .as_ref()
                .is_some_and(|names| names.contains(name.text()));
        }
        if ast::is_binding_pattern(name)
            && name.as_binding_pattern().elements.nodes().iter().any(|&element| {
                !ast::is_omitted_expression(element) && self.collides_with_parameter_name(element)
            })
        {
            return true;
        }
        false
    }

    // async.go:615
    fn transform_method_body(&self, node: P<Node>) -> P<Node> {
        let s = self.sas();
        let saved_captured_super_properties =
            s.captured_super_properties.replace(Some(OrderedSet::default()));
        let saved_has_super_element_access = s.has_super_element_access.replace(false);
        let saved_has_super_property_assignment = s.has_super_property_assignment.replace(false);
        let saved_super_binding =
            s.super_binding.replace(Some(self.new_super_binding_name("_super")));
        let saved_super_index_binding =
            s.super_index_binding.replace(Some(self.new_super_binding_name("_superIndex")));

        self.emit_context().start_variable_environment();
        let mut updated =
            self.emit_context().visit_function_body(node.body(), &mut self.visitor()).unwrap();

        // Minor optimization, emit `_super` helper to capture `super` access in an arrow.
        let captured_size = s.captured_super_properties.borrow().as_ref().unwrap().len();
        let emit_super_helpers = (captured_size > 0 || s.has_super_element_access.get())
            && !ast::get_function_flags(Some(self.get_original_if_function_like(node)))
                .contains(FunctionFlags::AsyncGenerator);

        if emit_super_helpers && captured_size > 0 {
            self.emit_context()
                .add_initialization_statement(s.create_super_access_variable_statement());
        }

        let merged_statements = self
            .emit_context()
            .end_and_merge_variable_environment_list(updated.statement_list())
            .unwrap();
        if emit_super_helpers && s.has_super_element_access.get() && !updated.as_block().multi_line
        {
            let new_block = self.factory().new_block(merged_statements, true);
            new_block.set_loc(updated.loc());
            updated = new_block;
        } else {
            updated = self.factory().update_block(
                updated,
                merged_statements,
                updated.as_block().multi_line,
            );
        }

        if emit_super_helpers && s.has_super_element_access.get() {
            if s.has_super_property_assignment.get() {
                self.emit_context().add_emit_helper(
                    updated,
                    &[SP::from_static(&printer::ADVANCED_ASYNC_SUPER_HELPER)],
                );
            } else {
                self.emit_context()
                    .add_emit_helper(updated, &[SP::from_static(&printer::ASYNC_SUPER_HELPER)]);
            }
        }

        *s.captured_super_properties.borrow_mut() = saved_captured_super_properties;
        s.has_super_element_access.set(saved_has_super_element_access);
        s.has_super_property_assignment.set(saved_has_super_property_assignment);
        s.super_binding.set(saved_super_binding);
        s.super_index_binding.set(saved_super_index_binding);
        updated
    }

    // async.go:667
    fn create_capture_arguments_statement(&self) -> P<Node> {
        let f = self.factory();
        let variable = f.new_variable_declaration(
            self.lexical_arguments.get().binding.unwrap(),
            None,
            None,
            Some(f.new_identifier("arguments")),
        );
        let decl_list =
            f.new_variable_declaration_list(f.new_node_list(vec![variable]), NodeFlags::None);
        let statement = f.new_variable_statement(None, decl_list);
        self.emit_context()
            .add_emit_flags(statement, EmitFlags::StartOnNewLine | EmitFlags::CustomPrologue);
        statement
    }

    // async.go:680
    fn transform_async_function_parameter_list(&self, node: P<Node>) -> P<NodeList> {
        if is_simple_parameter_list(node.parameters()) {
            return self
                .emit_context()
                .visit_parameters(node.parameter_list(), &mut self.visitor())
                .unwrap();
        }

        let f = self.factory();
        let mut new_parameters: Vec<P<Node>> = Vec::new();
        for &parameter in node.parameters() {
            let param = parameter.as_parameter_declaration();
            if param.initializer().is_some() || param.dot_dot_dot_token().is_some() {
                // for an arrow function, capture the remaining arguments in a rest parameter.
                // for any other function/method this isn't necessary as we can just use `arguments`.
                if node.kind() == Kind::ArrowFunction {
                    let rest_parameter = f.new_parameter_declaration(
                        None,
                        Some(f.new_token(Kind::DotDotDotToken)),
                        f.new_unique_name_ex(
                            "args",
                            printer::AutoGenerateOptions {
                                flags: GeneratedIdentifierFlags::ReservedInNestedScopes,
                                ..Default::default()
                            },
                        ),
                        None,
                        None,
                        None,
                    );
                    new_parameters.push(rest_parameter);
                }
                break;
            }
            // for arrow functions we capture fixed parameters to forward to `__awaiter`. For all other functions
            // we add fixed parameters to preserve the function's `length` property.
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
            new_parameters.push(new_parameter);
        }
        let new_parameters_array = f.new_node_list(new_parameters);
        new_parameters_array.loc.set(node.parameter_list().unwrap().loc.get());
        new_parameters_array
    }

    // async.go:725
    fn transform_async_function_body(
        &self,
        node: P<Node>,
        outer_parameters: P<NodeList>,
    ) -> P<Node> {
        let f = self.factory();
        let s = self.sas();
        let is_arrow = node.kind() == Kind::ArrowFunction;
        let mut saved_captured_super_properties: Option<OrderedSet<String>> = None;
        let saved_has_super_element_access = s.has_super_element_access.get();
        let saved_has_super_property_assignment = s.has_super_property_assignment.get();
        let saved_super_binding = s.super_binding.get();
        let saved_super_index_binding = s.super_index_binding.get();
        if !is_arrow {
            saved_captured_super_properties =
                s.captured_super_properties.replace(Some(OrderedSet::default()));
            s.has_super_element_access.set(false);
            s.has_super_property_assignment.set(false);
            s.super_binding.set(Some(self.new_super_binding_name("_super")));
            s.super_index_binding.set(Some(self.new_super_binding_name("_superIndex")));
        }

        let mut inner_parameters: Option<P<NodeList>> = None;
        if !is_simple_parameter_list(node.parameters()) {
            inner_parameters =
                self.emit_context().visit_parameters(node.parameter_list(), &mut self.visitor());
        }

        let saved_lexical_arguments = self.lexical_arguments.get();
        let capture_lexical_arguments = self.lexical_arguments.get().binding.is_none();
        if capture_lexical_arguments {
            self.lexical_arguments.set(LexicalArgumentsInfo {
                binding: Some(f.new_unique_name("arguments")),
                used: false,
            });
        }

        let mut arguments_expression: Option<P<Node>> = None;
        if inner_parameters.is_some() {
            if is_arrow {
                // `node` does not have a simple parameter list, so `outerParameters` refers to placeholders that are
                // forwarded to `innerParameters`, matching how they are introduced in `transformAsyncFunctionParameterList`.
                let mut parameter_bindings: Vec<P<Node>> = Vec::new();
                let outer_len = outer_parameters.nodes().len();
                for (i, &param) in node.parameters().iter().enumerate() {
                    if i >= outer_len {
                        break;
                    }
                    let original_parameter = param.as_parameter_declaration();
                    let outer_parameter = outer_parameters.nodes()[i].as_parameter_declaration();
                    if original_parameter.initializer().is_some()
                        || original_parameter.dot_dot_dot_token().is_some()
                    {
                        parameter_bindings.push(f.new_spread_element(outer_parameter.name()));
                        break;
                    }
                    parameter_bindings.push(outer_parameter.name());
                }
                arguments_expression = Some(
                    f.new_array_literal_expression(f.new_node_list(parameter_bindings), false),
                );
            } else {
                arguments_expression = Some(f.new_identifier("arguments"));
            }
        }

        // An async function is emit as an outer function that calls an inner
        // generator function. To preserve lexical bindings, we pass the current
        // `this` and `arguments` objects to `__awaiter`. The generator function
        // passed to `__awaiter` is executed inside of the callback to the
        // promise constructor.

        let mut enclosing_function_parameter_names: FxHashSet<String> = FxHashSet::default();
        for &parameter in node.parameters() {
            Self::record_declaration_name(parameter, &mut enclosing_function_parameter_names);
        }
        let saved_enclosing_function_parameter_names = self
            .enclosing_function_parameter_names
            .replace(Some(enclosing_function_parameter_names));

        let has_lexical_this = self.in_has_lexical_this_context();

        let mut async_body = self.transform_async_function_body_worker(node.body().unwrap());
        async_body = f.update_block(
            async_body,
            self.emit_context()
                .end_and_merge_variable_environment_list(async_body.statement_list())
                .unwrap(),
            async_body.as_block().multi_line,
        );

        // Substitute super property accesses with _super/_superIndex helpers
        let emit_super_helpers = s
            .captured_super_properties
            .borrow()
            .as_ref()
            .is_some_and(|c| !c.is_empty() || s.has_super_element_access.get());
        if emit_super_helpers {
            inner_parameters =
                s.super_access_visitor.get().unwrap().clone().visit_nodes(inner_parameters);
            async_body = s.substitute_super_accesses_in_body(async_body);
        }

        let result = if is_arrow {
            let mut r = f.new_awaiter_helper(
                has_lexical_this,
                arguments_expression,
                inner_parameters,
                async_body,
            );

            if capture_lexical_arguments && self.lexical_arguments.get().used {
                let block =
                    self.emit_context().convert_to_function_block(r, true /*multiLine*/);
                if !ast::is_block(r) {
                    self.emit_context().set_original(block.statement_list().unwrap().nodes()[0], r);
                }
                r = f.update_block(
                    block,
                    self.emit_context().merge_environment_list(
                        block.statement_list().unwrap(),
                        &[self.create_capture_arguments_statement()],
                    ),
                    block.as_block().multi_line,
                );
            }
            r
        } else {
            self.emit_context().start_variable_environment();

            // Minor optimization, emit `_super` helper to capture `super` access in an arrow.
            if emit_super_helpers
                && !s.captured_super_properties.borrow().as_ref().unwrap().is_empty()
            {
                self.emit_context()
                    .add_initialization_statement(s.create_super_access_variable_statement());
            }

            if capture_lexical_arguments && self.lexical_arguments.get().used {
                self.emit_context()
                    .add_initialization_statement(self.create_capture_arguments_statement());
            }

            let statements = vec![f.new_return_statement(Some(f.new_awaiter_helper(
                has_lexical_this,
                arguments_expression,
                inner_parameters,
                async_body,
            )))];

            let block = f.new_block(
                self.emit_context()
                    .end_and_merge_variable_environment_list(Some(f.new_node_list(statements)))
                    .unwrap(),
                true,
            );
            block.set_loc(node.body().unwrap().loc());

            if emit_super_helpers && s.has_super_element_access.get() {
                if s.has_super_property_assignment.get() {
                    self.emit_context().add_emit_helper(
                        block,
                        &[SP::from_static(&printer::ADVANCED_ASYNC_SUPER_HELPER)],
                    );
                } else {
                    self.emit_context()
                        .add_emit_helper(block, &[SP::from_static(&printer::ASYNC_SUPER_HELPER)]);
                }
            }

            block
        };

        *self.enclosing_function_parameter_names.borrow_mut() =
            saved_enclosing_function_parameter_names;
        if !is_arrow {
            *s.captured_super_properties.borrow_mut() = saved_captured_super_properties;
            s.has_super_element_access.set(saved_has_super_element_access);
            s.has_super_property_assignment.set(saved_has_super_property_assignment);
            s.super_binding.set(saved_super_binding);
            s.super_index_binding.set(saved_super_index_binding);
            self.lexical_arguments.set(saved_lexical_arguments);
        } else if capture_lexical_arguments && !self.lexical_arguments.get().used {
            // If we created a new binding but it wasn't used, restore the previous state.
            // If it was used, keep the binding alive so sibling arrows can reuse it
            // (the `var` declaration hoists to the enclosing function scope).
            self.lexical_arguments.set(saved_lexical_arguments);
        } else if capture_lexical_arguments {
            // Keep the binding but clear the used flag so siblings don't re-emit the capture statement.
            let la = self.lexical_arguments.get();
            self.lexical_arguments.set(LexicalArgumentsInfo { binding: la.binding, used: false });
        }
        result
    }

    // async.go:888
    fn transform_async_function_body_worker(&self, body: P<Node>) -> P<Node> {
        let f = self.factory();
        if ast::is_block(body) {
            return f.update_block(
                body,
                self.async_body_visitor().visit_nodes(body.statement_list()).unwrap(),
                body.as_block().multi_line,
            );
        }
        // Convert expression body to block body with return statement
        let visited = self.async_body_visitor().visit_node(Some(body));
        let ret = f.new_return_statement(visited);
        ret.set_loc(body.loc());
        let list = f.new_node_list(vec![ret]);
        list.loc.set(body.loc());
        let block = f.new_block(list, false /*multiLine*/);
        block.set_loc(body.loc());
        block
    }

    // async.go:955
    fn get_original_if_function_like(&self, node: P<Node>) -> P<Node> {
        let original = self.emit_context().most_original(Some(node));
        if let Some(original) = original
            && ast::is_function_like_declaration(original)
        {
            return original;
        }
        node
    }
}

// assignmentTargetContainsSuperProperty checks top-down whether an assignment target
// expression contains a super property or element access (super.x or super[x]).
// This avoids relying on parent pointers (IsAssignmentTarget) which may not be set
// on synthesized AST nodes from prior transforms.
// async.go:909
pub fn assignment_target_contains_super_property(node: P<Node>) -> bool {
    match node.kind() {
        Kind::PropertyAccessExpression | Kind::ElementAccessExpression => {
            return node.expression().unwrap().kind() == Kind::SuperKeyword;
        }
        Kind::ParenthesizedExpression => {
            return assignment_target_contains_super_property(
                node.as_parenthesized_expression().expression(),
            );
        }
        Kind::ArrayLiteralExpression => {
            return node
                .as_array_literal_expression()
                .elements
                .nodes()
                .iter()
                .any(|&e| assignment_target_contains_super_property(e));
        }
        Kind::ObjectLiteralExpression => {
            if node.as_object_literal_expression().properties.nodes().iter().any(|&prop| {
                match prop.kind() {
                    Kind::PropertyAssignment => {
                        if assignment_target_contains_super_property(
                            prop.as_property_assignment().initializer(),
                        ) {
                            return true;
                        }
                    }
                    Kind::ShorthandPropertyAssignment => {
                        if assignment_target_contains_super_property(prop.name().unwrap()) {
                            return true;
                        }
                    }
                    Kind::SpreadAssignment
                        if assignment_target_contains_super_property(
                            prop.as_spread_assignment().expression,
                        ) =>
                    {
                        return true;
                    }
                    _ => {}
                }

                false
            }) {
                return true;
            }
        }
        Kind::SpreadElement => {
            return assignment_target_contains_super_property(node.as_spread_element().expression);
        }
        _ => {}
    }
    false
}

// isUpdateExpression checks if a prefix/postfix unary expression is ++ or --.
// async.go:942
pub fn is_update_expression(node: P<Node>) -> bool {
    if ast::is_prefix_unary_expression(node) {
        let op = node.as_prefix_unary_expression().operator;
        return op == Kind::PlusPlusToken || op == Kind::MinusMinusToken;
    }
    if ast::is_postfix_unary_expression(node) {
        let op = node.as_postfix_unary_expression().operator;
        return op == Kind::PlusPlusToken || op == Kind::MinusMinusToken;
    }
    false
}

// isSimpleParameterList checks if every parameter has no initializer and an Identifier name.
// async.go:963
pub fn is_simple_parameter_list(params: &[P<Node>]) -> bool {
    if params.iter().any(|&param| {
        let p = param.as_parameter_declaration();
        p.initializer().is_some() || !ast::is_identifier(p.name())
    }) {
        return false;
    }
    true
}

// isNodeWithPossibleHoistedDeclaration checks if a node could contain hoisted declarations.
// async.go:975
fn is_node_with_possible_hoisted_declaration(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::Block
            | Kind::VariableStatement
            | Kind::WithStatement
            | Kind::IfStatement
            | Kind::SwitchStatement
            | Kind::CaseBlock
            | Kind::CaseClause
            | Kind::DefaultClause
            | Kind::LabeledStatement
            | Kind::ForStatement
            | Kind::ForInStatement
            | Kind::ForOfStatement
            | Kind::DoStatement
            | Kind::WhileStatement
            | Kind::TryStatement
            | Kind::CatchClause
    )
}
