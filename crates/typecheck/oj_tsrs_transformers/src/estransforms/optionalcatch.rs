use crate::*;

// optionalcatch.go:8
struct OptionalCatchTransformer {
    base: Transformer,
}

impl OptionalCatchTransformer {
    fn factory(&self) -> &'static printer::NodeFactory {
        self.base.factory()
    }

    fn visitor(&self) -> NodeVisitor {
        self.base.visitor()
    }

    // optionalcatch.go:12
    fn visit(&self, node: P<Node>) -> Option<P<Node>> {
        if !node.subtree_facts().intersects(SubtreeFacts::ContainsMissingCatchClauseVariable) {
            return Some(node);
        }
        match node.kind() {
            Kind::CatchClause => Some(self.visit_catch_clause(node)),
            _ => self.visitor().visit_each_child(Some(node)),
        }
    }

    // optionalcatch.go:24
    fn visit_catch_clause(&self, node: P<Node>) -> P<Node> {
        let c = node.as_catch_clause();
        if c.variable_declaration.is_none() {
            let f = self.factory();
            // Go: ch.Visitor().Visit(node.Block), the visitor's raw visit callback.
            return f.new_catch_clause(
                Some(f.new_variable_declaration(f.new_temp_variable(), None, None, None)),
                self.visit(c.block).unwrap(),
            );
        }
        self.visitor().visit_each_child(Some(node)).unwrap()
    }
}

// optionalcatch.go:34
#[expect(
    clippy::unnecessary_wraps,
    reason = "Transformer factories share an optional result so disabled transforms can return None."
)]
pub fn new_optional_catch_transformer(opt: &TransformOptions) -> Option<P<Transformer>> {
    let tx = P::new(OptionalCatchTransformer { base: Transformer::default() });
    Some(tx.get().base.new_transformer(
        Rc::new(move |_: &mut NodeVisitor, n: P<Node>| tx.visit(n)),
        Some(opt.context),
    ))
}
