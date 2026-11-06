use super::*;

// nullishcoalescing.go:8
struct NullishCoalescingTransformer {
    base: Transformer,
}

impl NullishCoalescingTransformer {
    fn emit_context(&self) -> P<EmitContext> {
        self.base.emit_context()
    }

    fn factory(&self) -> &'static printer::NodeFactory {
        self.base.factory()
    }

    fn visitor(&self) -> NodeVisitor {
        self.base.visitor()
    }

    // nullishcoalescing.go:12
    fn visit(&self, node: P<Node>) -> Option<P<Node>> {
        if !node.subtree_facts().intersects(SubtreeFacts::ContainsNullishCoalescing) {
            return Some(node);
        }
        match node.kind() {
            Kind::BinaryExpression => Some(self.visit_binary_expression(node)),
            _ => self.visitor().visit_each_child(Some(node)),
        }
    }

    // nullishcoalescing.go:24
    fn visit_binary_expression(&self, node: P<Node>) -> P<Node> {
        let b = node.as_binary_expression();
        match b.operator_token.kind() {
            Kind::QuestionQuestionToken => {
                let f = self.factory();
                let mut left = self.visitor().visit_node(Some(b.left)).unwrap();
                let mut right = left;
                if !is_simple_copiable_expression(left) {
                    right = f.new_temp_variable();
                    self.emit_context().add_variable_declaration(right);
                    left = f.new_assignment_expression(right, left);
                }
                f.new_conditional_expression(
                    create_not_null_condition(self.emit_context(), left, right, false),
                    f.new_token(Kind::QuestionToken),
                    right,
                    f.new_token(Kind::ColonToken),
                    self.visitor().visit_node(Some(b.right.get())).unwrap(),
                )
            }
            _ => self.visitor().visit_each_child(Some(node)).unwrap(),
        }
    }
}

// nullishcoalescing.go:46
#[expect(
    clippy::unnecessary_wraps,
    reason = "Transformer factories share an optional result so disabled transforms can return None."
)]
pub fn new_nullish_coalescing_transformer(opt: &TransformOptions) -> Option<P<Transformer>> {
    let tx = P::new(NullishCoalescingTransformer { base: Transformer::default() });
    Some(tx.get().base.new_transformer(
        Rc::new(move |_: &mut NodeVisitor, n: P<Node>| tx.visit(n)),
        Some(opt.context),
    ))
}
