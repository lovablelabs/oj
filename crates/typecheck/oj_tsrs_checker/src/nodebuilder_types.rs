//! Non-function declarations of nodebuilder.go, nodebuilderimpl.go and nodebuilderscopes.go (the node builder data
//! model). See docs/CHECKER.md, "Node builder".

use std::cell::OnceCell;

use tsrs_ast::{NodeFactory, VisitFn};
use tsrs_core::collections::{CopyOnWriteMap, CopyOnWriteSet};
use tsrs_module::ModeAwareCacheKey;

use crate::*;

// nodebuilder.go

/// Go `NodeBuilder`: the public entry points (`TypeToTypeNode`, `SymbolToEntityName`, …) that push/pop a
/// `NodeBuilderContext` around a `NodeBuilderImpl` call.
///
/// An arena handle (`P<NodeBuilder>`, `&self` methods), because the checker caches one
/// (`Checker::type_to_string_nodebuilder`) and serialization re-enters it (a diagnostic reported during lazy
/// resolution formats a type with the same builder, pushing a new context).
pub struct NodeBuilder {
    /// Go pushes `b.impl.ctx`, which is nil for the outermost call.
    pub(crate) ctx_stack: RefCell<Vec<Option<P<NodeBuilderContext>>>>,
    pub(crate) host: &'static dyn ModuleSpecifierGenerationHost, // Go `Host`
    pub(crate) impl_: P<NodeBuilderImpl>,
    pub(crate) verbosity: Cell<Option<P<VerbosityContext>>>, // nil for non-hover callers
}

// nodebuilderimpl.go

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CompositeSymbolIdentity {
    pub(crate) is_constructor_node: bool,
    pub(crate) symbol_id: SymbolId,
    pub(crate) node_id: NodeId,
}

/// Go `TrackedSymbolArgs` (`*TrackedSymbolArgs`, built once by a composite literal and never compared by identity):
/// a value here.
#[derive(Clone, Copy)]
pub struct TrackedSymbolArgs {
    pub(crate) symbol: P<Symbol>,
    pub(crate) enclosing_declaration: Option<P<Node>>,
    pub(crate) meaning: SymbolFlags,
}

/// Go `SerializedTypeEntry`, handled as `P<SerializedTypeEntry>` (built once by a composite literal).
pub struct SerializedTypeEntry {
    pub(crate) node: Option<P<Node>>, // Go stores the (possibly nil) result of the transform
    pub(crate) truncating: bool,
    pub(crate) added_length: usize,
    pub(crate) tracked_symbols: Vec<TrackedSymbolArgs>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CompositeTypeCacheIdentity {
    pub(crate) type_id: TypeId,
    pub(crate) flags: Flags,
    pub(crate) internal_flags: InternalFlags,
    pub(crate) infer_type_parameters: CacheHashKey,
}

#[derive(Default)]
pub struct NodeBuilderLinks {
    pub(crate) serialized_types: GoMap<CompositeTypeCacheIdentity, P<SerializedTypeEntry>>, // Collection of types serialized at this location
    pub(crate) fake_scope_for_signature_declaration: Cell<Option<&'static str>>, // If present, this is a fake scope injected into an enclosing declaration chain.
}

/// Go `module.ModeAwareCache[moduleSpecifierResult]` is `map[ModeAwareCacheKey]moduleSpecifierResult`.
#[derive(Default)]
pub struct NodeBuilderSymbolLinks {
    pub(crate) specifier_cache: GoMap<ModeAwareCacheKey, ModuleSpecifierResult>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ModuleSpecifierResult {
    pub(crate) specifier: &'static str,
    pub(crate) import_attributes_type: Option<P<Type>>,
}

/// Go `NodeBuilderContext`: the per-call state of a node builder invocation.
///
/// Handled as `P<NodeBuilderContext>` (Go swaps `b.impl.ctx` pointers, `SymbolTrackerImpl` and the
/// `cloneNodeBuilderContext` restore closure hold it); every field Go assigns is a `Cell`/`RefCell`. Build
/// with `NodeBuilderContext { …, ..NodeBuilderContext::new(host) }`.
pub struct NodeBuilderContext {
    pub(crate) host: &'static dyn ModuleSpecifierGenerationHost, // Go `Host`
    pub(crate) tracker: Cell<Option<&'static dyn SymbolTracker>>, // never None after NodeBuilder::enter_context
    pub(crate) approximate_length: Cell<usize>,
    pub(crate) max_truncation_length: Cell<usize>,
    pub(crate) encountered_error: Cell<bool>,
    pub(crate) truncating: Cell<bool>,
    pub(crate) reported_diagnostic: Cell<bool>,
    pub(crate) flags: Cell<Flags>,
    pub(crate) internal_flags: Cell<InternalFlags>,
    pub(crate) depth: Cell<usize>,
    pub(crate) max_expansion_depth: Cell<Option<usize>>,
    /// `ExpandSymbolForHover` pushes a nil sentinel, hence `Option`.
    pub(crate) type_stack: RefCell<Vec<Option<P<Type>>>>,
    pub(crate) can_increase_expansion_depth: Cell<bool>,
    pub(crate) expansion_truncated: Cell<bool>,
    pub(crate) enclosing_declaration: Cell<Option<P<Node>>>,
    pub(crate) enclosing_file: Cell<Option<P<SourceFile>>>,
    /// Go assigns whole slices (`t.root.inferTypeParameters`) and restores the saved one.
    pub(crate) infer_type_parameters: Cell<&'static [P<Type>]>,
    pub(crate) visited_types: RefCell<Set<TypeId>>,
    pub(crate) symbol_depth: RefCell<FxHashMap<CompositeSymbolIdentity, usize>>,
    /// Go saves the slice, sets it to nil, and stores/restores it around `visitAndTransformType`.
    pub(crate) tracked_symbols: RefCell<Vec<TrackedSymbolArgs>>,
    pub(crate) mapper: MapperCell,
    pub(crate) reverse_mapped_stack: RefCell<Vec<P<Symbol>>>,
    pub(crate) enclosing_symbol_types: RefCell<FxHashMap<SymbolId, P<Type>>>,
    pub(crate) suppress_report_inference_fallback: Cell<bool>,
    pub(crate) remapped_symbol_references: RefCell<FxHashMap<SymbolId, P<Symbol>>>,

    // per signature scope state
    pub(crate) type_parameter_names: RefCell<CopyOnWriteMap<TypeId, P<Node>>>, // values are Identifier nodes
    pub(crate) type_parameter_names_by_text: RefCell<CopyOnWriteSet<String>>,
    pub(crate) type_parameter_names_by_text_next_name_count: RefCell<CopyOnWriteMap<String, usize>>,
    pub(crate) type_parameter_symbol_list: RefCell<CopyOnWriteSet<SymbolId>>,
}

impl NodeBuilderContext {
    /// The Go zero value (with the one field that has no zero value).
    pub(crate) fn new(host: &'static dyn ModuleSpecifierGenerationHost) -> NodeBuilderContext {
        NodeBuilderContext {
            host,
            tracker: Cell::new(None),
            approximate_length: Cell::new(0),
            max_truncation_length: Cell::new(0),
            encountered_error: Cell::new(false),
            truncating: Cell::new(false),
            reported_diagnostic: Cell::new(false),
            flags: Cell::new(Flags::None),
            internal_flags: Cell::new(InternalFlags::None),
            depth: Cell::new(0),
            max_expansion_depth: Cell::new(Some(0)),
            type_stack: RefCell::default(),
            can_increase_expansion_depth: Cell::new(false),
            expansion_truncated: Cell::new(false),
            enclosing_declaration: Cell::new(None),
            enclosing_file: Cell::new(None),
            infer_type_parameters: Cell::new(&[]),
            visited_types: RefCell::default(),
            symbol_depth: RefCell::default(),
            tracked_symbols: RefCell::default(),
            mapper: MapperCell::new(None),
            reverse_mapped_stack: RefCell::default(),
            enclosing_symbol_types: RefCell::default(),
            suppress_report_inference_fallback: Cell::new(false),
            remapped_symbol_references: RefCell::default(),
            type_parameter_names: RefCell::default(),
            type_parameter_names_by_text: RefCell::default(),
            type_parameter_names_by_text_next_name_count: RefCell::default(),
            type_parameter_symbol_list: RefCell::default(),
        }
    }
}

/// Go `NodeBuilderImpl`. An arena handle (`P<NodeBuilderImpl>`) whose methods take `&self` plus `c: &mut Checker`
/// (Go's `ch` field is dropped: `b.ch.foo()` -> `c.foo()`).
pub struct NodeBuilderImpl {
    // host members
    /// Go `f = e.Factory.AsNodeFactory()`: a handle to the emit context's factory (same hooks, so created nodes are
    /// marked synthesized). All factory methods take `&self`, so calls nest: `b.f.new_x(b.f.new_y())`.
    pub(crate) f: NodeFactory,
    pub(crate) e: P<EmitContext>,
    pub(crate) pc: P<PseudoChecker>,

    // cache
    pub(crate) links: tsrs_core::LinkStore<Node, NodeBuilderLinks>,
    pub(crate) symbol_links: tsrs_core::LinkStore<Symbol, NodeBuilderSymbolLinks>,

    // state
    pub(crate) ctx: Cell<Option<P<NodeBuilderContext>>>,

    // reusable visitor
    /// Go's `cloneBindingNameVisitor` (visit callback `b.cloneBindingName`), kept as its callback: a `NodeVisitor`
    /// holds nothing but the callback, the factory and hooks, and a visit re-enters it, so `clone_binding_name` builds
    /// `new_node_visitor(Some(visit.clone()), Some(b.f.clone()), NodeVisitorHooks::default())` per call and runs it
    /// under `b.checker_slot.lend(c, ..)`. Set by `new_node_builder_impl`.
    pub(crate) clone_binding_name_visitor: OnceCell<VisitFn>,

    // symbols for synthesized identifiers, needed for e.g. inlay hints
    pub(crate) id_to_symbol: P<RefCell<FxHashMap<P<Node>, P<Symbol>>>>,

    /// Rust only: lends the checker to visitor callbacks (`cloneBindingName`, `getExistingNodeTreeVisitor`), which
    /// Go writes as closures over `b.ch`. See `CheckerSlot`.
    pub(crate) checker_slot: P<CheckerSlot>,
    /// Rust only: this builder's own handle, for closures that capture `b` (`b.as_p()`). Set by `new_node_builder_impl`.
    pub(crate) this: OnceCell<P<NodeBuilderImpl>>,
}

impl NodeBuilderImpl {
    /// Go `b.ctx` (always set while a `NodeBuilder` entry point runs).
    #[inline]
    pub(crate) fn ctx(&self) -> P<NodeBuilderContext> {
        self.ctx.get().unwrap()
    }

    /// `b` as a copyable handle, for closures (Go captures the `*NodeBuilderImpl`).
    #[inline]
    pub(crate) fn as_p(&self) -> P<NodeBuilderImpl> {
        *self.this.get().unwrap()
    }
}

/// Rust only: gives `ast::NodeVisitor` callbacks access to the checker.
///
/// Go's visitor callbacks are closures over `b.ch`; a Rust `VisitFn` is a `'static` `Rc<dyn Fn(&mut
/// NodeVisitor, P<Node>)>` and cannot capture `&mut Checker`. The code that starts a visit lends its checker
/// for the duration (`slot.lend(c, || v.visit_node(n))`); a callback borrows it back with `slot.with(|c|
/// ..)`, and must `lend` its own `c` again before re-entering the visitor (`slot.with(|c| { ..; slot.lend(c,
/// || v.visit_each_child(Some(n))) })`), so every checker reference is derived from the one live above it.
/// Like a `RefCell`, a second `with` without an intervening `lend` panics, and so does `with` outside any
/// `lend`.
#[derive(Default)]
pub struct CheckerSlot {
    ptr: Cell<Option<std::ptr::NonNull<Checker>>>,
    in_use: Cell<bool>,
}

impl CheckerSlot {
    /// Makes `c` available to `with` while `f` runs (nests; the previous state is restored afterwards).
    pub fn lend<R>(&self, c: &mut Checker, f: impl FnOnce() -> R) -> R {
        let saved =
            (self.ptr.replace(Some(std::ptr::NonNull::from(c))), self.in_use.replace(false));
        let result = f();
        self.ptr.set(saved.0);
        self.in_use.set(saved.1);
        result
    }

    /// Calls `f` with the checker lent by the innermost `lend`.
    pub fn with<R>(&self, f: impl FnOnce(&mut Checker) -> R) -> R {
        // The checker's allocations (types, symbols, caches) outlive a scratch region (one file's emit) the caller
        // may have entered (notes/mem-emit-regions.md), and stay in the checker's own region if it has one.
        let _outer = tsrs_core::arena::escape_scratch_one_level();
        let mut ptr = self.ptr.get().expect("CheckerSlot::with outside CheckerSlot::lend");
        assert!(
            !self.in_use.replace(true),
            "CheckerSlot::with re-entered without CheckerSlot::lend"
        );
        // SAFETY: `ptr` comes from the `&mut Checker` passed to the innermost active `lend`, which is borrowed for
        // the whole `lend` call and not otherwise usable while `f` runs; `in_use` guarantees that at most one
        // reference derived from it is live (a nested `with` needs a nested `lend`, which derives from this one).
        let result = f(unsafe { ptr.as_mut() });
        self.in_use.set(false);
        result
    }
}

pub(crate) const DEFAULT_MAXIMUM_TRUNCATION_LENGTH: usize = 160;
pub(crate) const NO_TRUNCATION_MAXIMUM_TRUNCATION_LENGTH: usize = 1_000_000;

#[derive(Clone, Debug)]
pub struct SortedSymbolNamePair {
    pub(crate) sym: P<Symbol>,
    pub(crate) name: String,
}

/// Go `SignatureToSignatureDeclarationOptions`, passed as `Option<P<…>>` (built once by a composite literal).
#[derive(Default)]
pub struct SignatureToSignatureDeclarationOptions {
    pub(crate) modifiers: &'static [P<Node>],
    pub(crate) name: Option<P<Node>>, // a PropertyName node
    pub(crate) question_token: Option<P<Node>>,
}

pub(crate) const MAX_REVERSE_MAPPED_NESTING_INSPECTION_DEPTH: usize = 3;

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum PropertyNameNodeKind {
    Identifier,
    NumericLiteral,
    StringLiteral,
}

// nodebuilderscopes.go

#[derive(Clone, Debug)]
pub struct LocalsRecord {
    pub(crate) name: String,
    pub(crate) old_symbol: P<Symbol>,
}

/// `modulespecifiers.CheckerShape` is implemented by `*Checker` in Go (exported `GetSymbolAtLocation` /
/// `GetAliasedSymbol`).
impl modulespecifiers::CheckerShape for Checker {
    fn get_symbol_at_location(&mut self, node: P<Node>) -> Option<P<Symbol>> {
        self.get_symbol_at_location_exported(node)
    }
    fn get_aliased_symbol(&mut self, symbol: P<Symbol>) -> P<Symbol> {
        Checker::get_aliased_symbol(self, symbol)
    }
}

// nodecopy.go

/// Go `recoveryBoundary`, handled as `P<recoveryBoundary>` (the wrapping tracker holds it). Fields Go sets at
/// construction and only reads are plain; the rest are `Cell`/`RefCell`.
pub struct RecoveryBoundary {
    pub(crate) ctx: P<NodeBuilderContext>,
    pub(crate) had_error: Cell<bool>,
    pub(crate) deferred_reports: RefCell<Vec<Box<dyn FnOnce()>>>,
    pub(crate) old_tracker: Option<&'static dyn SymbolTracker>,
    /// `finalizeBoundary` moves it back into the context (`take()`).
    pub(crate) old_tracked_symbols: RefCell<Vec<TrackedSymbolArgs>>,
    pub(crate) tracked_symbols: RefCell<Vec<TrackedSymbolArgs>>,
    pub(crate) old_encountered_error: bool,
    pub(crate) old_approximate_length: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct OriginalRecoveryScopeState {
    pub(crate) tracked_symbols_top: usize,
    pub(crate) unreported_errors_top: usize,
    pub(crate) had_error: bool,
}

/// Go `wrappingTracker`, handled as `P<wrappingTracker>` and used as `&'static dyn SymbolTracker`; the
/// inherent methods are in nodecopy.rs.
///
/// `markError(w.wrapped.ReportX)` passes `Some(Box::new(move || wrapped.report_x()))` with `let wrapped =
/// self.wrapped;`.
pub struct WrappingTracker {
    pub(crate) wrapped: &'static dyn SymbolTracker,
    pub(crate) bound: P<RecoveryBoundary>,
}

impl SymbolTracker for WrappingTracker {
    fn track_symbol(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
        enclosing_declaration: Option<P<Node>>,
        meaning: SymbolFlags,
    ) -> bool {
        WrappingTracker::track_symbol(self, c, symbol, enclosing_declaration, meaning)
    }
    fn report_inaccessible_this_error(&self) {
        WrappingTracker::report_inaccessible_this_error(self);
    }
    fn report_private_in_base_of_class_expression(&self, property_name: &str) {
        WrappingTracker::report_private_in_base_of_class_expression(self, property_name);
    }
    fn report_inaccessible_unique_symbol_error(&self) {
        WrappingTracker::report_inaccessible_unique_symbol_error(self);
    }
    fn report_cyclic_structure_error(&self) {
        WrappingTracker::report_cyclic_structure_error(self);
    }
    fn report_likely_unsafe_import_required_error(&self, specifier: &str, symbol_name: &str) {
        WrappingTracker::report_likely_unsafe_import_required_error(self, specifier, symbol_name);
    }
    fn report_truncation_error(&self) {
        WrappingTracker::report_truncation_error(self);
    }
    fn report_nonlocal_augmentation(
        &self,
        containing_file: P<SourceFile>,
        parent_symbol: P<Symbol>,
        augmenting_symbol: P<Symbol>,
    ) {
        WrappingTracker::report_nonlocal_augmentation(
            self,
            containing_file,
            parent_symbol,
            augmenting_symbol,
        );
    }
    fn report_non_serializable_property(&self, property_name: &str) {
        WrappingTracker::report_non_serializable_property(self, property_name);
    }
    fn report_inference_fallback(&self, c: &mut Checker, node: P<Node>) {
        WrappingTracker::report_inference_fallback(self, c, node);
    }
    fn push_error_fallback_node(&self, node: Option<P<Node>>) {
        WrappingTracker::push_error_fallback_node(self, node);
    }
    fn pop_error_fallback_node(&self) {
        WrappingTracker::pop_error_fallback_node(self);
    }
}

// emitresolver.go (the data; the functions are in emitresolver.rs)

// Links for jsx
#[derive(Default)]
pub struct JSXLinks {
    pub(crate) import_ref: Cell<Option<P<Node>>>,
}

// Links for declarations

#[derive(Default)]
pub struct DeclarationLinks {
    pub(crate) is_visible: Cell<Tristate>, // if declaration is depended upon by exported declarations
}

#[derive(Default)]
pub struct DeclarationFileLinks {
    pub(crate) aliases_marked: Cell<bool>, // if file has had alias visibility marked
}

/// Go `EmitResolverLinks`: visibility and JSX links outlive a single file's emit resolver.
#[derive(Default)]
pub struct EmitResolverLinks {
    pub(crate) jsx_links: tsrs_core::LinkStore<Node, JSXLinks>,
    pub(crate) declaration_links: tsrs_core::LinkStore<Node, DeclarationLinks>,
    pub(crate) declaration_file_links: tsrs_core::LinkStore<Node, DeclarationFileLinks>,
}

/// Go `EmitResolver`, handled as `P<EmitResolver>`, a checker holder (`&self, c: &mut Checker`).
///
/// Go's `checker` field is dropped and so is `checkerMu`: the exported methods that lock it in Go
/// (`IsDeclarationVisible`, ...) are the `_exported`/unsuffixed `pub` methods, and the declaration
/// transformer serializes access to the checker itself (`tsrs_declarations` `Resolver::lock`).
/// `isValueAliasDeclaration` / `aliasMarkingVisitor` are Go method values of `isValueAliasDeclarationWorker`
/// / `aliasMarkingVisitorWorker`; Rust calls those methods directly.
pub struct EmitResolver {
    pub(crate) reference_resolver: std::cell::OnceCell<P<tsrs_binder::ReferenceResolver<Checker>>>,
    pub(crate) emit_context: P<EmitContext>,
    pub(crate) request_node_builder: std::cell::OnceCell<P<NodeBuilder>>,
    pub(crate) links: P<EmitResolverLinks>,
}
