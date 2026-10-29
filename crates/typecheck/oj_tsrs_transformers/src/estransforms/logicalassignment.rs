use crate::*;

// logicalassignment.go:8
struct LogicalAssignmentTransformer {
    base: Transformer,
}

impl LogicalAssignmentTransformer {
    fn emit_context(&self) -> P<EmitContext> {
        self.base.emit_context()
    }

    fn factory(&self) -> &'static printer::NodeFactory {
        self.base.factory()
    }

    fn visitor(&self) -> NodeVisitor {
        self.base.visitor()
    }

    // logicalassignment.go:12
    fn visit(&self, node: P<Node>) -> Option<P<Node>> {
        if !node.subtree_facts().intersects(SubtreeFacts::ContainsLogicalAssignments) {
            return Some(node);
        }
        match node.kind() {
            Kind::BinaryExpression => Some(self.visit_binary_expression(node)),
            _ => self.visitor().visit_each_child(Some(node)),
        }
    }

    // logicalassignment.go:24
    fn visit_binary_expression(&self, node: P<Node>) -> P<Node> {
        let f = self.factory();
        let b = node.as_binary_expression();
        let non_assignment_operator = match b.operator_token.kind() {
            Kind::BarBarEqualsToken => Kind::BarBarToken,
            Kind::AmpersandAmpersandEqualsToken => Kind::AmpersandAmpersandToken,
            Kind::QuestionQuestionEqualsToken => Kind::QuestionQuestionToken,
            _ => return self.visitor().visit_each_child(Some(node)).unwrap(),
        };

        let mut left = ast::skip_parentheses(self.visitor().visit_node(Some(b.left)).unwrap());
        let mut assignment_target = left;
        let right = ast::skip_parentheses(self.visitor().visit_node(Some(b.right.get())).unwrap());

        if ast::is_access_expression(left) {
            let property_access_target_simple_copiable =
                is_simple_copiable_expression(left.expression().unwrap());
            let mut property_access_target = left.expression().unwrap();
            let mut property_access_target_assignment = left.expression().unwrap();
            if !property_access_target_simple_copiable {
                property_access_target = f.new_temp_variable();
                self.emit_context().add_variable_declaration(property_access_target);
                property_access_target_assignment =
                    f.new_assignment_expression(property_access_target, left.expression().unwrap());
            }

            if ast::is_property_access_expression(left) {
                assignment_target = f.new_property_access_expression(
                    property_access_target,
                    None,
                    left.name().unwrap(),
                    NodeFlags::None,
                );
                left = f.new_property_access_expression(
                    property_access_target_assignment,
                    None,
                    left.name().unwrap(),
                    NodeFlags::None,
                );
            } else {
                let element_access_argument_simple_copiable = is_simple_copiable_expression(
                    left.as_element_access_expression().argument_expression,
                );
                let mut element_access_argument =
                    left.as_element_access_expression().argument_expression;
                let mut argument_expr = element_access_argument;
                if !element_access_argument_simple_copiable {
                    element_access_argument = f.new_temp_variable();
                    self.emit_context().add_variable_declaration(element_access_argument);
                    argument_expr = f.new_assignment_expression(
                        element_access_argument,
                        left.as_element_access_expression().argument_expression,
                    );
                }

                assignment_target = f.new_element_access_expression(
                    property_access_target,
                    None,
                    element_access_argument,
                    NodeFlags::None,
                );
                left = f.new_element_access_expression(
                    property_access_target_assignment,
                    None,
                    argument_expr,
                    NodeFlags::None,
                );
            }
        }

        f.new_binary_expression(
            None,
            left,
            None,
            f.new_token(non_assignment_operator),
            f.new_parenthesized_expression(f.new_assignment_expression(assignment_target, right)),
        )
    }
}

// logicalassignment.go:110
#[expect(
    clippy::unnecessary_wraps,
    reason = "Transformer factories share an optional result so disabled transforms can return None."
)]
pub fn new_logical_assignment_transformer(opt: &TransformOptions) -> Option<P<Transformer>> {
    let tx = P::new(LogicalAssignmentTransformer { base: Transformer::default() });
    Some(tx.get().base.new_transformer(
        Rc::new(move |_: &mut NodeVisitor, n: P<Node>| tx.visit(n)),
        Some(opt.context),
    ))
}
