// Port of transformer.go: the `Transformer` base embedded by every transformer.

use crate::*;

/// Go `transformers.Transformer`, embedded by value in each transformer.
///
/// Transformers are arena handles (`&self` methods), so the fields Go assigns in `NewTransformer` are
/// `OnceCell`s. The visitor carries no state besides its callback, factory and hooks, so `visitor()` hands
/// out a copy (Go shares one `*NodeVisitor`, which is re-entrant).
#[derive(Default)]
pub struct Transformer {
    emit_context: OnceCell<P<EmitContext>>,
    visitor: OnceCell<ast::NodeVisitor>,
}

impl Transformer {
    // transformer.go:14
    pub fn new_transformer(
        &'static self,
        visit: ast::VisitFn,
        emit_context: Option<P<EmitContext>>,
    ) -> P<Transformer> {
        assert!(self.emit_context.get().is_none(), "Transformer already initialized");
        let emit_context = emit_context.unwrap_or_else(printer::new_emit_context);
        let _ = self.emit_context.set(emit_context);
        let _ = self.visitor.set(emit_context.new_node_visitor(visit));
        P::from_static(self)
    }

    // transformer.go:28
    pub fn emit_context(&self) -> P<EmitContext> {
        *self.emit_context.get().unwrap()
    }

    // transformer.go:32
    pub fn visitor(&self) -> ast::NodeVisitor {
        self.visitor.get().unwrap().clone()
    }

    // transformer.go:36 (Go keeps `emitContext.Factory` in a field; it is the same handle)
    pub fn factory(&self) -> &'static printer::NodeFactory {
        &self.emit_context().get().factory
    }

    // transformer.go:40
    pub fn transform_source_file(&self, file: P<SourceFile>) -> P<SourceFile> {
        let visited = self.visitor().visit_source_file(file.as_node());
        visited.as_source_file_p()
    }
}
