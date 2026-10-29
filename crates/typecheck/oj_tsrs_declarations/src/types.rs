#![expect(
    clippy::redundant_pub_crate,
    reason = "Only the public API is re-exported; implementation helpers remain crate-private."
)]

// Hand-ported data model of package `declarations` (types, interfaces and constants of transform.go, tracker.go,
// diagnostics.go and supplementalreferences.go). Function bodies live in the per-file modules.
//
// Conventions (see also docs/CHECKER.md "Node builder" for the checker side):
// - Transformers, the tracker and its shared state are arena handles (`P<…>`, `&self` methods, `Cell`/`RefCell`
//   fields), because node visitor callbacks (`Rc<dyn Fn>`) capture them, like Go closures capture `tx`.
// - The checker is reached through `Resolver` (tsrs_transformers/src/resolver.rs); only the symbol tracker's
//   `track_symbol` / `report_inference_fallback` receive it directly (they run inside node-builder calls, where Go
//   holds the lock).
// - Go func-typed fields become `Rc<dyn Fn>` (cloned to save/restore, as Go copies the func value).

use crate::*;

// transform.go:23
#[derive(Clone, Copy)]
pub struct ReferencedFilePair {
    pub(crate) file: P<SourceFile>,
    pub(crate) ref_: P<FileReference>,
}

// transform.go:28: Go `OutputPaths` interface (`DeclarationFilePath()`, `JsFilePath()`), implemented only by
// `outputpaths.OutputPaths`; Rust uses that type directly (tsrs_tsoptions::outputpaths::OutputPaths).

// transform.go:33
// Used to be passed in the TransformationContext, which is now just an EmitContext
pub trait DeclarationEmitHost: ModuleSpecifierGenerationHost {
    // GetCurrentDirectory() / UseCaseSensitiveFileNames() come from OutputPathsHost (via ModuleSpecifierGenerationHost).
    fn get_source_file_from_reference(
        &self,
        origin: P<SourceFile>,
        ref_: P<FileReference>,
    ) -> Option<P<SourceFile>>;

    fn get_output_paths_for(&self, file: P<SourceFile>, force_dts_paths: bool) -> OutputPaths;
    fn source_file_may_be_emitted(&self, file: P<SourceFile>, force_dts_emit: bool) -> bool;
}

// transform.go:46
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ThisPropertyAssignmentKey {
    pub(crate) name: String,
    pub(crate) node: Option<P<Node>>,
    pub(crate) is_static: bool,
    pub(crate) is_private: bool,
}

// transform.go:63
pub struct DeclarationTransformer {
    pub base: Transformer, // Go embeds transformers.Transformer
    pub(crate) host: &'static dyn DeclarationEmitHost,
    pub(crate) compiler_options: P<CompilerOptions>,
    pub(crate) tracker: P<SymbolTrackerImpl>,
    pub(crate) state: P<SymbolTrackerSharedState>,
    pub(crate) resolver: Resolver,
    pub(crate) declaration_file_path: String,

    pub(crate) needs_declare: Cell<bool>,
    pub(crate) needs_scope_fix_marker: Cell<bool>,
    pub(crate) result_has_scope_marker: Cell<bool>,
    pub(crate) enclosing_declaration: Cell<Option<P<Node>>>,
    pub(crate) result_has_external_module_indicator: Cell<bool>,
    pub(crate) suppress_new_diagnostic_contexts: Cell<bool>,
    pub(crate) witnessed_cjs_exports: RefCell<Set<String>>,
    /// Go `map[ast.NodeId]*ast.Node`: a present key with a nil value (`None`) means "deleted", which differs from a
    /// missing key.
    pub(crate) late_statement_replacement_map: RefCell<FxHashMap<NodeId, Option<P<Node>>>>,
    pub(crate) expando_hosts: RefCell<FxHashMap<NodeId, Option<P<Node>>>>, // store the result of transforming expando hosts so they can be inserted later if the host is actually referenced
    pub(crate) expando_members: RefCell<FxHashMap<NodeId, Vec<P<Node>>>>, // store any found expando _members_ after transforming them so *if* the host is referenced, they can be emitted alongside it
    pub(crate) deferred_expando_assignments: RefCell<FxHashMap<NodeId, Vec<P<Node>>>>, // expando assignments whose host wasn't visible when collected, processed if the host is late-marked visible
    pub(crate) seen_properties: RefCell<Set<ThisPropertyAssignmentKey>>,
    pub(crate) this_property_assignments_collected: RefCell<Vec<P<Node>>>,
    pub(crate) raw_referenced_files: RefCell<Vec<ReferencedFilePair>>,
    pub(crate) raw_type_reference_directives: RefCell<Vec<P<FileReference>>>,
    pub(crate) raw_lib_reference_directives: RefCell<Vec<P<FileReference>>>,
    pub(crate) binding_name_visitor: OnceCell<ast::NodeVisitor>,
    pub(crate) expression_visitor: OnceCell<ast::NodeVisitor>,
    pub(crate) cjs_export_assignment_visitor: OnceCell<ast::NodeVisitor>,
    pub(crate) export_stripping_visitor: OnceCell<ast::NodeVisitor>,
    pub(crate) this_property_visitor: OnceCell<ast::NodeVisitor>,

    pub(crate) cjs_export_assignment: Cell<Option<P<Node>>>,
    pub(crate) cjs_export_members: RefCell<Vec<P<Node>>>,
    pub(crate) cjs_export_assignment_name: Cell<Option<P<Node>>>, // tracks the name node used for `export =` in CJS module.exports assignments
    pub(crate) declare_stripping_visitor: OnceCell<ast::NodeVisitor>,
    pub(crate) in_class_expression_declaration: Cell<bool>, // true when serializing members of a class expression kept as a class declaration
}

impl DeclarationTransformer {
    // Go's promoted `Transformer` methods and visitor fields. Visitors are handed out as copies (they are stateless).

    pub(crate) fn emit_context(&self) -> P<EmitContext> {
        self.base.emit_context()
    }

    pub(crate) fn visitor(&self) -> ast::NodeVisitor {
        self.base.visitor()
    }

    pub(crate) fn factory(&self) -> &'static printer::NodeFactory {
        self.base.factory()
    }

    /// Go `tx.Visitor().Visit(node)`: calls the visit callback directly (no `SyntaxList` lifting); Go's
    /// `visit` returns nil for a nil node.
    pub(crate) fn visit_fn(&self, node: Option<P<Node>>) -> Option<P<Node>> {
        let node = node?;
        let mut v = self.visitor();
        let visit = v.visit.clone().unwrap();
        visit(&mut v, node)
    }

    pub(crate) fn binding_name_visitor(&self) -> ast::NodeVisitor {
        self.binding_name_visitor.get().unwrap().clone()
    }

    pub(crate) fn expression_visitor(&self) -> ast::NodeVisitor {
        self.expression_visitor.get().unwrap().clone()
    }

    pub(crate) fn cjs_export_assignment_visitor(&self) -> ast::NodeVisitor {
        self.cjs_export_assignment_visitor.get().unwrap().clone()
    }

    pub(crate) fn export_stripping_visitor(&self) -> ast::NodeVisitor {
        self.export_stripping_visitor.get().unwrap().clone()
    }

    pub(crate) fn this_property_visitor(&self) -> ast::NodeVisitor {
        self.this_property_visitor.get().unwrap().clone()
    }

    pub(crate) fn declare_stripping_visitor(&self) -> ast::NodeVisitor {
        self.declare_stripping_visitor.get().unwrap().clone()
    }
}

// transform.go:215
pub(crate) const DECLARATION_EMIT_NODE_BUILDER_FLAGS: nodebuilder::Flags =
    nodebuilder::Flags::MultilineObjectLiterals
        .union(nodebuilder::Flags::WriteClassExpressionAsTypeLiteral)
        .union(nodebuilder::Flags::UseTypeOfFunction)
        .union(nodebuilder::Flags::UseStructuralFallback)
        .union(nodebuilder::Flags::AllowEmptyTuple)
        .union(nodebuilder::Flags::GenerateNamesForShadowedTypeParams)
        .union(nodebuilder::Flags::NoTruncation);

// transform.go:223
pub(crate) const DECLARATION_EMIT_INTERNAL_NODE_BUILDER_FLAGS: nodebuilder::InternalFlags =
    nodebuilder::InternalFlags::AllowUnresolvedNames
        .union(nodebuilder::InternalFlags::RewriteModuleSpecifiers);

// tracker.go:12
pub struct SymbolTrackerImpl {
    pub(crate) resolver: Resolver,
    pub(crate) state: P<SymbolTrackerSharedState>,
    pub(crate) fallback_stack: RefCell<Vec<Option<P<Node>>>>,

    // For detecting class expression self-references during member serialization.
    // When set, TrackSymbol will record usage without reporting accessibility errors.
    pub(crate) watched_class_symbol: Cell<Option<P<Symbol>>>,
    pub(crate) class_symbol_tracked: Cell<bool>,

    pub(crate) get_isolated_declaration_error: GetIsolatedDeclarationError,
}

/// Go `func(node *ast.Node) *ast.Diagnostic` built by `createGetIsolatedDeclarationErrors`; it runs inside
/// node-builder calls (from `ReportInferenceFallback`), so it receives the checker.
pub type GetIsolatedDeclarationError = Rc<dyn Fn(&mut Checker, P<Node>) -> P<Diagnostic>>;

// tracker.go:229
pub struct SymbolTrackerSharedState {
    pub(crate) late_marked_statements: RefCell<Vec<P<Node>>>,
    pub(crate) diagnostics: RefCell<Vec<P<Diagnostic>>>,
    pub(crate) get_symbol_accessibility_diagnostic: RefCell<GetSymbolAccessibilityDiagnostic>,
    pub(crate) error_name_node: Cell<Option<P<Node>>>,
    pub(crate) isolated_declarations: bool,
    pub(crate) strip_internal: bool,
    pub(crate) current_source_file: Cell<Option<P<SourceFile>>>,
    pub(crate) resolver: Resolver,
    /// Go `reportExpandoFunctionErrors func(node *ast.Node)`, assigned by `NewDeclarationTransformer`. It calls the
    /// non-locking `GetPropertiesOfContainerFunction`, so it receives the checker (callers outside a node-builder call
    /// pass `resolver.lock(..)`'s).
    pub(crate) report_expando_function_errors: OnceCell<Rc<dyn Fn(&mut Checker, P<Node>)>>,
}

// diagnostics.go:10
pub type GetSymbolAccessibilityDiagnostic =
    Rc<dyn Fn(&SymbolAccessibilityResult) -> Option<P<SymbolAccessibilityDiagnostic>>>;

// diagnostics.go:12
pub struct SymbolAccessibilityDiagnostic {
    pub(crate) error_node: Option<P<Node>>,
    pub(crate) diagnostic_message: &'static Message,
    pub(crate) type_name: Option<P<Node>>,
}

// supplementalreferences.go:12
// SupplementalReferencesTransformer adds triple-slash path references from a content mapper's
// canonical declaration output to the declaration files emitted for its supplemental files.
// This ensures that consumers loading the canonical declaration also include the supplemental types.
pub struct SupplementalReferencesTransformer {
    pub(crate) host: &'static dyn DeclarationEmitHost,
    pub(crate) supplemental_files: &'static [P<SourceFile>],
    pub(crate) declaration_file_path: String,
    pub(crate) force_declaration_paths: bool,
}

// Go: `*SymbolTrackerImpl` implements `nodebuilder.SymbolTracker` (checker.SymbolTracker); the methods are in
// tracker.rs.
impl SymbolTracker for SymbolTrackerImpl {
    fn track_symbol(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
        enclosing_declaration: Option<P<Node>>,
        meaning: SymbolFlags,
    ) -> bool {
        SymbolTrackerImpl::track_symbol(self, c, symbol, enclosing_declaration, meaning)
    }
    fn report_inaccessible_this_error(&self) {
        SymbolTrackerImpl::report_inaccessible_this_error(self);
    }
    fn report_private_in_base_of_class_expression(&self, property_name: &str) {
        SymbolTrackerImpl::report_private_in_base_of_class_expression(self, property_name);
    }
    fn report_inaccessible_unique_symbol_error(&self) {
        SymbolTrackerImpl::report_inaccessible_unique_symbol_error(self);
    }
    fn report_cyclic_structure_error(&self) {
        SymbolTrackerImpl::report_cyclic_structure_error(self);
    }
    fn report_likely_unsafe_import_required_error(&self, specifier: &str, symbol_name: &str) {
        SymbolTrackerImpl::report_likely_unsafe_import_required_error(self, specifier, symbol_name);
    }
    fn report_truncation_error(&self) {
        SymbolTrackerImpl::report_truncation_error(self);
    }
    fn report_nonlocal_augmentation(
        &self,
        containing_file: P<SourceFile>,
        parent_symbol: P<Symbol>,
        augmenting_symbol: P<Symbol>,
    ) {
        SymbolTrackerImpl::report_nonlocal_augmentation(
            self,
            containing_file,
            parent_symbol,
            augmenting_symbol,
        );
    }
    fn report_non_serializable_property(&self, property_name: &str) {
        SymbolTrackerImpl::report_non_serializable_property(self, property_name);
    }
    fn report_inference_fallback(&self, c: &mut Checker, node: P<Node>) {
        SymbolTrackerImpl::report_inference_fallback(self, c, node);
    }
    fn push_error_fallback_node(&self, node: Option<P<Node>>) {
        SymbolTrackerImpl::push_error_fallback_node(self, node);
    }
    fn pop_error_fallback_node(&self) {
        SymbolTrackerImpl::pop_error_fallback_node(self);
    }
}
