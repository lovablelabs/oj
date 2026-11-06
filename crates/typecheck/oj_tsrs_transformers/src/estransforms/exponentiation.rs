use crate::*;

// exponentiation.go:8
struct ExponentiationTransformer {
    base: Transformer,
}

impl ExponentiationTransformer {
    fn emit_context(&self) -> P<EmitContext> {
        self.base.emit_context()
    }

    fn factory(&self) -> &'static printer::NodeFactory {
        self.base.factory()
    }

    fn visitor(&self) -> NodeVisitor {
        self.base.visitor()
    }

    // exponentiation.go:12
    fn visit(&self, node: P<Node>) -> Option<P<Node>> {
        if !node.subtree_facts().intersects(SubtreeFacts::ContainsExponentiationOperator) {
            return Some(node);
        }
        match node.kind() {
            Kind::BinaryExpression => Some(self.visit_binary_expression(node)),
            _ => self.visitor().visit_each_child(Some(node)),
        }
    }

    // exponentiation.go:24
    fn visit_binary_expression(&self, node: P<Node>) -> P<Node> {
        match node.as_binary_expression().operator_token.kind() {
            Kind::AsteriskAsteriskEqualsToken => {
                return self.visit_exponentiation_assignment_expression(node);
            }
            Kind::AsteriskAsteriskToken => return self.visit_exponentiation_expression(node),
            _ => {}
        }
        self.visitor().visit_each_child(Some(node)).unwrap()
    }

    // exponentiation.go:34
    fn visit_exponentiation_assignment_expression(&self, node: P<Node>) -> P<Node> {
        let f = self.factory();
        let target: P<Node>;
        let value: P<Node>;
        let b = node.as_binary_expression();
        let left = self.visitor().visit_node(Some(b.left)).unwrap();
        let right = self.visitor().visit_node(Some(b.right.get())).unwrap();
        if ast::is_element_access_expression(left) {
            // Transforms `a[x] **= b` into `(_a = a)[_x = x] = Math.pow(_a[_x], b)`
            let expression_temp = f.new_temp_variable();
            self.emit_context().add_variable_declaration(expression_temp);
            let argument_expression_temp = f.new_temp_variable();
            self.emit_context().add_variable_declaration(argument_expression_temp);

            let obj_expr = f.new_assignment_expression(expression_temp, left.expression().unwrap());
            obj_expr.set_loc(left.expression().unwrap().loc());
            let access_expr = f.new_assignment_expression(
                argument_expression_temp,
                left.as_element_access_expression().argument_expression,
            );
            access_expr.set_loc(left.as_element_access_expression().argument_expression.loc());

            target = f.new_element_access_expression(obj_expr, None, access_expr, NodeFlags::None);

            value = f.new_element_access_expression(
                expression_temp,
                None,
                argument_expression_temp,
                NodeFlags::None,
            );
            value.set_loc(left.loc());
        } else if ast::is_property_access_expression(left) {
            // Transforms `a.x **= b` into `(_a = a).x = Math.pow(_a.x, b)`
            let expression_temp = f.new_temp_variable();
            self.emit_context().add_variable_declaration(expression_temp);
            let assignment =
                f.new_assignment_expression(expression_temp, left.expression().unwrap());
            assignment.set_loc(left.expression().unwrap().loc());
            target = f.new_property_access_expression(
                assignment,
                None,
                left.name().unwrap(),
                NodeFlags::None,
            );
            target.set_loc(left.loc());

            value = f.new_property_access_expression(
                expression_temp,
                None,
                left.name().unwrap(),
                NodeFlags::None,
            );
            value.set_loc(left.loc());
        } else {
            // Transforms `a **= b` into `a = Math.pow(a, b)`
            target = left;
            value = left;
        }

        let rhs = f.new_global_method_call("Math", "pow", vec![value, right]);
        rhs.set_loc(node.loc());
        let result = f.new_assignment_expression(target, rhs);
        result.set_loc(node.loc());
        result
    }

    // exponentiation.go:79
    fn visit_exponentiation_expression(&self, node: P<Node>) -> P<Node> {
        let b = node.as_binary_expression();
        let left = self.visitor().visit_node(Some(b.left)).unwrap();
        let right = self.visitor().visit_node(Some(b.right.get())).unwrap();
        let result = self.factory().new_global_method_call("Math", "pow", vec![left, right]);
        result.set_loc(node.loc());
        result
    }
}

// exponentiation.go:87
#[expect(
    clippy::unnecessary_wraps,
    reason = "Transformer factories share an optional result so disabled transforms can return None."
)]
pub fn new_exponentiation_transformer(opt: &TransformOptions) -> Option<P<Transformer>> {
    let tx = P::new(ExponentiationTransformer { base: Transformer::default() });
    Some(tx.get().base.new_transformer(
        Rc::new(move |_: &mut NodeVisitor, n: P<Node>| tx.visit(n)),
        Some(opt.context),
    ))
}
