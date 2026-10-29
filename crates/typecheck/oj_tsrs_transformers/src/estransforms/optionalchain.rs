use super::*;

// optionalchain.go:10
struct OptionalChainTransformer {
    base: Transformer,
}

impl OptionalChainTransformer {
    fn emit_context(&self) -> P<EmitContext> {
        self.base.emit_context()
    }

    fn factory(&self) -> &'static printer::NodeFactory {
        self.base.factory()
    }

    fn visitor(&self) -> NodeVisitor {
        self.base.visitor()
    }

    // optionalchain.go:14
    fn visit(&self, node: P<Node>) -> Option<P<Node>> {
        if !node.subtree_facts().intersects(SubtreeFacts::ContainsOptionalChaining) {
            return Some(node);
        }
        match node.kind() {
            Kind::CallExpression => Some(self.visit_call_expression(node, false)),
            Kind::PropertyAccessExpression | Kind::ElementAccessExpression => {
                if node.flags().intersects(NodeFlags::OptionalChain) {
                    return Some(self.visit_optional_expression(node, false, false));
                }
                self.visitor().visit_each_child(Some(node))
            }
            Kind::DeleteExpression => self.visit_delete_expression(node),
            _ => self.visitor().visit_each_child(Some(node)),
        }
    }

    // optionalchain.go:34
    fn visit_call_expression(&self, node: P<Node>, capture_this_arg: bool) -> P<Node> {
        if node.flags().intersects(NodeFlags::OptionalChain) {
            // If `node` is an optional chain, then it is the outermost chain of an optional expression.
            return self.visit_optional_expression(node, capture_this_arg, false);
        }
        let c = node.as_call_expression();
        if ast::is_parenthesized_expression(c.expression) {
            let unwrapped = ast::skip_parentheses(c.expression);
            if unwrapped.flags().intersects(NodeFlags::OptionalChain) {
                // capture thisArg for calls of parenthesized optional chains like `(foo?.bar)()`
                let expression = self.visit_parenthesized_expression(c.expression, true, false);
                let args = self.visitor().visit_nodes(Some(c.arguments)).unwrap();
                if ast::is_synthetic_reference_expression(expression) {
                    let s = expression.as_synthetic_reference_expression();
                    let res = self.factory().new_function_call_call(
                        s.expression,
                        Some(s.this_arg),
                        args.nodes(),
                    );
                    res.set_loc(node.loc());
                    self.emit_context().set_original(res, node);
                    return res;
                }
                return self.factory().update_call_expression(
                    node,
                    expression,
                    None, /*questionDotToken*/
                    None, /*typeArguments*/
                    args,
                    node.flags(),
                );
            }
        }
        self.visitor().visit_each_child(Some(node)).unwrap()
    }

    // optionalchain.go:57
    fn visit_parenthesized_expression(
        &self,
        node: P<Node>,
        capture_this_arg: bool,
        is_delete: bool,
    ) -> P<Node> {
        let expr = self.visit_non_optional_expression(
            node.expression().unwrap(),
            capture_this_arg,
            is_delete,
        );
        if ast::is_synthetic_reference_expression(expr) {
            // `(a.b)` -> { expression `((_a = a).b)`, thisArg: `_a` }
            // `(a[b])` -> { expression `((_a = a)[b])`, thisArg: `_a` }
            let synth = expr.as_synthetic_reference_expression();
            let res = self.factory().new_synthetic_reference_expression(
                self.factory().update_parenthesized_expression(node, synth.expression),
                synth.this_arg,
            );
            self.emit_context().set_original(res, node);
            return res;
        }
        self.factory().update_parenthesized_expression(node, expr)
    }

    // optionalchain.go:70
    fn visit_property_or_element_access_expression(
        &self,
        node: P<Node>,
        capture_this_arg: bool,
        is_delete: bool,
    ) -> P<Node> {
        if node.flags().intersects(NodeFlags::OptionalChain) {
            // If `node` is an optional chain, then it is the outermost chain of an optional expression.
            return self.visit_optional_expression(node, capture_this_arg, is_delete);
        }
        let mut expression = self.visitor().visit_node(node.expression());
        assert!(
            expression.is_none() || !ast::is_synthetic_reference_expression(expression.unwrap())
        );
        let f = self.factory();

        let mut this_arg: Option<P<Node>> = None;
        if capture_this_arg {
            if is_simple_copiable_expression(expression.unwrap()) {
                this_arg = expression;
            } else {
                let t = f.new_temp_variable();
                this_arg = Some(t);
                self.emit_context().add_variable_declaration(t);
                expression = Some(f.new_assignment_expression(t, expression.unwrap()));
            }
        }

        let expression = if node.kind() == Kind::PropertyAccessExpression {
            f.update_property_access_expression(
                node,
                expression.unwrap(),
                None, /*questionDotToken*/
                self.visitor().visit_node(node.name()).unwrap(),
                node.flags(),
            )
        } else {
            let p = node.as_element_access_expression();
            f.update_element_access_expression(
                node,
                expression.unwrap(),
                None,
                self.visitor().visit_node(Some(p.argument_expression)).unwrap(),
                node.flags(),
            )
        };

        if let Some(this_arg) = this_arg {
            let res = f.new_synthetic_reference_expression(expression, this_arg);
            self.emit_context().set_original(res, node);
            return res;
        }
        expression
    }

    // optionalchain.go:105
    fn visit_delete_expression(&self, node: P<Node>) -> Option<P<Node>> {
        let unwrapped = ast::skip_parentheses(node.expression().unwrap());
        if unwrapped.flags().intersects(NodeFlags::OptionalChain) {
            return Some(self.visit_non_optional_expression(
                node.expression().unwrap(),
                false,
                true,
            ));
        }
        self.visitor().visit_each_child(Some(node))
    }

    // optionalchain.go:113
    fn visit_non_optional_expression(
        &self,
        node: P<Node>,
        capture_this_arg: bool,
        is_delete: bool,
    ) -> P<Node> {
        match node.kind() {
            Kind::ParenthesizedExpression => {
                self.visit_parenthesized_expression(node, capture_this_arg, is_delete)
            }
            Kind::ElementAccessExpression | Kind::PropertyAccessExpression => {
                self.visit_property_or_element_access_expression(node, capture_this_arg, is_delete)
            }
            Kind::CallExpression => self.visit_call_expression(node, capture_this_arg),
            _ => self.visitor().visit_node(Some(node)).unwrap(),
        }
    }
}

// optionalchain.go:126
struct FlattenResult {
    expression: P<Node>,
    chain: Vec<P<Node>>,
}

// optionalchain.go:131
fn is_non_null_chain(node: P<Node>) -> bool {
    ast::is_non_null_expression(node) && node.flags().intersects(NodeFlags::OptionalChain)
}

// optionalchain.go:135
fn flatten_chain(mut chain: P<Node>) -> FlattenResult {
    assert!(!is_non_null_chain(chain));
    let mut links: Vec<P<Node>> = vec![chain];
    while !ast::is_tagged_template_expression(chain) && chain.question_dot_token().is_none() {
        chain = ast::skip_partially_emitted_expressions(chain.expression().unwrap());
        assert!(!is_non_null_chain(chain));
        links.insert(0, chain);
    }
    FlattenResult { expression: chain.expression().unwrap(), chain: links }
}

// optionalchain.go:146
fn is_call_chain(node: P<Node>) -> bool {
    ast::is_call_expression(node) && node.flags().intersects(NodeFlags::OptionalChain)
}

impl OptionalChainTransformer {
    // optionalchain.go:150
    fn visit_optional_expression(
        &self,
        node: P<Node>,
        capture_this_arg: bool,
        is_delete: bool,
    ) -> P<Node> {
        let f = self.factory();
        let r = flatten_chain(node);
        let expression = r.expression;
        let chain = r.chain;
        let left = self.visit_non_optional_expression(
            ast::skip_partially_emitted_expressions(expression),
            is_call_chain(chain[0]),
            false,
        );
        let mut left_this_arg: Option<P<Node>> = None;
        let mut captured_left = left;
        if ast::is_synthetic_reference_expression(left) {
            left_this_arg = Some(left.as_synthetic_reference_expression().this_arg);
            captured_left = left.as_synthetic_reference_expression().expression;
        }
        let mut left_expression = f.restore_outer_expressions(
            Some(expression),
            captured_left,
            OuterExpressionKinds::PartiallyEmittedExpressions,
        );
        if !is_simple_copiable_expression(captured_left) {
            captured_left = f.new_temp_variable();
            self.emit_context().add_variable_declaration(captured_left);
            left_expression = f.new_assignment_expression(captured_left, left_expression);
        }
        let mut right_expression = captured_left;
        let mut this_arg: Option<P<Node>> = None;

        for (i, &segment) in chain.iter().enumerate() {
            match segment.kind() {
                Kind::ElementAccessExpression | Kind::PropertyAccessExpression => {
                    if i == chain.len() - 1 && capture_this_arg {
                        if is_simple_copiable_expression(right_expression) {
                            this_arg = Some(right_expression);
                        } else {
                            let t = f.new_temp_variable();
                            this_arg = Some(t);
                            self.emit_context().add_variable_declaration(t);
                            right_expression = f.new_assignment_expression(t, right_expression);
                        }
                    }
                    if segment.kind() == Kind::ElementAccessExpression {
                        right_expression = f.new_element_access_expression(
                            right_expression,
                            None,
                            self.visitor()
                                .visit_node(Some(
                                    segment.as_element_access_expression().argument_expression,
                                ))
                                .unwrap(),
                            NodeFlags::None,
                        );
                    } else {
                        right_expression = f.new_property_access_expression(
                            right_expression,
                            None,
                            self.visitor().visit_node(segment.name()).unwrap(),
                            NodeFlags::None,
                        );
                    }
                }
                Kind::CallExpression => {
                    if i == 0 && left_this_arg.is_some() {
                        let mut lta = left_this_arg.unwrap();
                        if !self.emit_context().has_auto_generate_info(Some(lta)) {
                            lta = lta.clone_node(f);
                            self.emit_context().add_emit_flags(lta, EmitFlags::NoComments);
                            left_this_arg = Some(lta);
                        }
                        let mut call_this_arg = lta;
                        if lta.kind() == Kind::SuperKeyword {
                            call_this_arg = f.new_this_expression();
                        }
                        right_expression = f.new_function_call_call(
                            right_expression,
                            Some(call_this_arg),
                            self.visitor().visit_nodes(segment.argument_list()).unwrap().nodes(),
                        );
                    } else {
                        right_expression = f.new_call_expression(
                            right_expression,
                            None,
                            None,
                            self.visitor().visit_nodes(segment.argument_list()).unwrap(),
                            NodeFlags::None,
                        );
                    }
                }
                _ => {}
            }
            self.emit_context().set_original(right_expression, segment);
        }

        let mut target = if is_delete {
            f.new_conditional_expression(
                create_not_null_condition(
                    self.emit_context(),
                    left_expression,
                    captured_left,
                    true,
                ),
                f.new_token(Kind::QuestionToken),
                f.new_true_expression(),
                f.new_token(Kind::ColonToken),
                f.new_delete_expression(right_expression),
            )
        } else {
            f.new_conditional_expression(
                create_not_null_condition(
                    self.emit_context(),
                    left_expression,
                    captured_left,
                    true,
                ),
                f.new_token(Kind::QuestionToken),
                f.new_void_zero_expression(),
                f.new_token(Kind::ColonToken),
                right_expression,
            )
        };
        target.set_loc(node.loc());
        if let Some(this_arg) = this_arg {
            target = f.new_synthetic_reference_expression(target, this_arg);
        }
        self.emit_context().set_original(target, node);
        target
    }
}

// optionalchain.go:237
#[expect(
    clippy::unnecessary_wraps,
    reason = "Transformer factories share an optional result so disabled transforms can return None."
)]
pub fn new_optional_chain_transformer(opt: &TransformOptions) -> Option<P<Transformer>> {
    let tx = P::new(OptionalChainTransformer { base: Transformer::default() });
    Some(tx.get().base.new_transformer(
        Rc::new(move |_: &mut NodeVisitor, n: P<Node>| tx.visit(n)),
        Some(opt.context),
    ))
}
