// Port of estransforms/classthis.go.

use crate::*;

// classthis.go:10
// Gets whether a node is a `static {}` block containing only a single assignment of the static `this` to the `_classThis`
// (or similar) variable stored in the `classthis` property of the block's `EmitNode`.
pub fn is_class_this_assignment_block(emit_context: P<EmitContext>, node: P<Node>) -> bool {
    if ast::is_class_static_block_declaration(node) {
        let n = node.as_class_static_block_declaration();
        let body = n.body.as_block();
        if body.statements.nodes().len() == 1 {
            let statement = body.statements.nodes()[0];
            if ast::is_expression_statement(statement) {
                let expression = statement.expression().unwrap();
                if ast::is_assignment_expression(
                    expression, true, /*excludeCompoundAssignment*/
                ) {
                    let binary = expression.as_binary_expression();
                    return ast::is_identifier(binary.left)
                        && emit_context.class_this(node) == Some(binary.left)
                        && binary.right().kind() == Kind::ThisKeyword;
                }
            }
        }
    }
    false
}
