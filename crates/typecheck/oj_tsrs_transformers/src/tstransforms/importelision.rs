use super::*;

struct ImportElisionTransformer {
    base: Transformer,
    compiler_options: P<CompilerOptions>,
    current_source_file: Cell<Option<P<SourceFile>>>,
    emit_resolver: Option<Resolver>,
}

// importelision.go:17
#[expect(
    clippy::unnecessary_wraps,
    reason = "Transformer factories share an optional result so disabled transforms can return None."
)]
pub fn new_import_elision_transformer(opt: &TransformOptions) -> Option<P<Transformer>> {
    let compiler_options = opt.compiler_options;
    let emit_context = opt.context;
    assert!(
        !compiler_options.verbatim_module_syntax.is_true(),
        "ImportElisionTransformer should not be used with VerbatimModuleSyntax"
    );
    let tx = P::new(ImportElisionTransformer {
        base: Transformer::default(),
        compiler_options,
        current_source_file: Cell::new(None),
        emit_resolver: opt.emit_resolver,
    });
    Some(tx.get().base.new_transformer(
        Rc::new(move |_: &mut NodeVisitor, n: P<Node>| tx.visit(n)),
        Some(emit_context),
    ))
}

impl ImportElisionTransformer {
    fn emit_context(&self) -> P<EmitContext> {
        self.base.emit_context()
    }

    // importelision.go:27
    fn visit(&self, node: P<Node>) -> Option<P<Node>> {
        if ast::is_source_file(node)
            && let Some(emit_resolver) = self.emit_resolver
        {
            emit_resolver.mark_linked_references_recursively(
                self.emit_context().most_original(Some(node)).unwrap().as_source_file_p(),
            );
        }

        let f = self.base.factory();
        let mut v = self.base.visitor();
        match node.kind() {
            Kind::ImportEqualsDeclaration => {
                if ast::is_external_module_import_equals_declaration(node) {
                    if !self.should_emit_alias_declaration(node) {
                        return None;
                    }
                } else if !self.should_emit_import_equals_declaration(node) {
                    return None;
                }
                v.visit_each_child(Some(node))
            }
            Kind::ImportDeclaration => {
                let n = node.as_import_declaration();
                // Do not elide a side-effect only import declaration.
                //  import "foo";
                if let Some(import_clause) = n.import_clause {
                    let import_clause = v.visit_node(Some(import_clause))?;
                    return Some(f.update_import_declaration(
                        node,
                        node.modifiers(),
                        Some(import_clause),
                        n.module_specifier,
                        v.visit_node(n.attributes()),
                    ));
                }
                v.visit_each_child(Some(node))
            }
            Kind::ImportClause => {
                let n = node.as_import_clause();
                let name = if self.should_emit_alias_declaration(node) { n.name() } else { None };
                let named_bindings = v.visit_node(n.named_bindings);
                if name.is_none() && named_bindings.is_none() {
                    // all import bindings were elided
                    return None;
                }
                Some(f.update_import_clause(node, n.phase_modifier(), name, named_bindings))
            }
            Kind::NamespaceImport | Kind::ImportSpecifier => {
                if !self.should_emit_alias_declaration(node) {
                    // elide type-only or unused imports
                    return None;
                }
                Some(node)
            }
            Kind::NamedImports => {
                let n = node.as_named_imports();
                let elements = v.visit_nodes(Some(n.elements)).unwrap();
                if elements.nodes().is_empty() {
                    // all import specifiers were elided
                    return None;
                }
                Some(f.update_named_imports(node, elements))
            }
            Kind::ExportAssignment => {
                if !self.compiler_options.verbatim_module_syntax.is_true()
                    && !self.is_value_alias_declaration(node)
                {
                    // elide unused import
                    return None;
                }
                v.visit_each_child(Some(node))
            }
            Kind::ExportDeclaration => {
                let n = node.as_export_declaration();
                let mut export_clause: Option<P<Node>> = None;
                if let Some(c) = n.export_clause {
                    export_clause = v.visit_node(Some(c));
                    export_clause?;
                }
                Some(f.update_export_declaration(
                    node,
                    None,  /*modifiers*/
                    false, /*isTypeOnly*/
                    export_clause,
                    v.visit_node(n.module_specifier),
                    v.visit_node(n.attributes()),
                ))
            }
            Kind::NamedExports => {
                let n = node.as_named_exports();
                let elements = v.visit_nodes(Some(n.elements)).unwrap();
                if elements.nodes().is_empty() {
                    // all export specifiers were elided
                    return None;
                }
                Some(f.update_named_exports(node, elements))
            }
            Kind::ExportSpecifier => {
                if !self.is_value_alias_declaration(node) {
                    // elide unused export
                    return None;
                }
                Some(node)
            }
            Kind::SourceFile => {
                let saved_current_source_file = self.current_source_file.get();
                self.current_source_file.set(Some(node.as_source_file_p()));
                let node = v.visit_each_child(Some(node));
                self.current_source_file.set(saved_current_source_file);
                node
            }
            Kind::ModuleDeclaration | Kind::ModuleBlock => v.visit_each_child(Some(node)),
            _ => Some(node),
        }
    }

    // importelision.go:124
    fn should_emit_alias_declaration(&self, node: P<Node>) -> bool {
        ast::is_in_js_file(node) || self.is_referenced_alias_declaration(node)
    }

    // importelision.go:128
    fn should_emit_import_equals_declaration(&self, node: P<Node>) -> bool {
        // preserve old compiler's behavior: emit import declaration (even if we do not consider them referenced) when
        // - current file is not external module
        // - import declaration is top level and target is value imported by entity name
        self.should_emit_alias_declaration(node)
            || (!ast::is_external_module(self.current_source_file.get().unwrap())
                && self.is_top_level_value_import_equals_with_entity_name(node))
    }

    // importelision.go:135
    fn is_referenced_alias_declaration(&self, node: P<Node>) -> bool {
        let Some(node) = self.emit_context().parse_node(Some(node)) else {
            return true;
        };
        self.emit_resolver.unwrap().is_referenced_alias_declaration(node)
    }

    // importelision.go:140
    fn is_value_alias_declaration(&self, node: P<Node>) -> bool {
        let Some(node) = self.emit_context().parse_node(Some(node)) else {
            return true;
        };
        self.emit_resolver.unwrap().is_value_alias_declaration(node)
    }

    // importelision.go:145
    fn is_top_level_value_import_equals_with_entity_name(&self, node: P<Node>) -> bool {
        let Some(node) = self.emit_context().parse_node(Some(node)) else {
            return false;
        };
        self.emit_resolver.unwrap().is_top_level_value_import_equals_with_entity_name(node)
    }
}
