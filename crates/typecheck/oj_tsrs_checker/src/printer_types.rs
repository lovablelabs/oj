//! Shares node-builder and printer declarations without introducing import cycles.
//!
//! Declarations that printer.rs / symbolaccessibility / symboltracker signatures reference: the node-builder and printer types from other Go packages (`nodebuilder`, `printer`), the non-function declarations of `symbolaccessibility.go` and `symboltracker.go`, and the seam to the `tsrs_printer` crate.
//!
//! The node builder's own data model is in `nodebuilder_types.rs`.

use crate::direct_bitflags as bitflags;

use crate::*;

// Seam to `tsrs_printer` (Go `internal/printer`): every use of the printer crate by the checker goes through these
// names. `printer::NodeFactory` is deliberately not re-exported (it would clash with `tsrs_ast::NodeFactory` in the stub
// files' glob imports); reach it as `e.factory`.
pub use tsrs_printer::{
    EmitContext, EmitFlags, EmitTextWriter, PrintHandlers, Printer, PrinterOptions, QuoteChar,
    escape_string, get_single_line_string_writer, new_emit_context, new_printer, new_text_writer,
};

// nodebuilder/types.go

/// Go `nodebuilder.SymbolTracker`.
pub trait SymbolTracker {
    fn track_symbol(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
        enclosing_declaration: Option<P<Node>>,
        meaning: SymbolFlags,
    ) -> bool;
    fn report_inaccessible_this_error(&self);
    fn report_private_in_base_of_class_expression(&self, property_name: &str);
    fn report_inaccessible_unique_symbol_error(&self);
    fn report_cyclic_structure_error(&self);
    fn report_likely_unsafe_import_required_error(&self, specifier: &str, symbol_name: &str);
    fn report_truncation_error(&self);
    fn report_nonlocal_augmentation(
        &self,
        containing_file: P<SourceFile>,
        parent_symbol: P<Symbol>,
        augmenting_symbol: P<Symbol>,
    );
    fn report_non_serializable_property(&self, property_name: &str);
    fn report_inference_fallback(&self, c: &mut Checker, node: P<Node>);
    fn push_error_fallback_node(&self, node: Option<P<Node>>);
    fn pop_error_fallback_node(&self);

    /// Go's type assertion `tracker.(*SymbolTrackerImpl)` (in `NewSymbolTrackerImpl`).
    fn as_symbol_tracker_impl(&self) -> Option<&SymbolTrackerImpl> {
        None
    }
}

bitflags! {
    /// Go `nodebuilder.Flags`. NOTE: If modifying this enum, must modify `TypeFormatFlags` too!
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct Flags: u32 {
        const None = 0;
        // Options
        const NoTruncation = 1 << 0;
        const WriteArrayAsGenericType = 1 << 1;
        const GenerateNamesForShadowedTypeParams = 1 << 2;
        const UseStructuralFallback = 1 << 3;
        const ForbidIndexedAccessSymbolReferences = 1 << 4;
        const WriteTypeArgumentsOfSignature = 1 << 5;
        const UseFullyQualifiedType = 1 << 6;
        const UseOnlyExternalAliasing = 1 << 7;
        const SuppressAnyReturnType = 1 << 8;
        const WriteTypeParametersInQualifiedName = 1 << 9;
        const MultilineObjectLiterals = 1 << 10;
        const WriteClassExpressionAsTypeLiteral = 1 << 11;
        const UseTypeOfFunction = 1 << 12;
        const OmitParameterModifiers = 1 << 13;
        const UseAliasDefinedOutsideCurrentScope = 1 << 14;
        const UseSingleQuotesForStringLiteralType = 1 << 28;
        const NoTypeReduction = 1 << 29;
        const UseInstantiationExpressions = 1 << 30;
        const OmitThisParameter = 1 << 25;
        const WriteCallStyleSignature = 1 << 27;
        // Error handling
        const AllowThisInObjectLiteral = 1 << 15;
        const AllowQualifiedNameInPlaceOfIdentifier = 1 << 16;
        const AllowAnonymousIdentifier = 1 << 17;
        const AllowEmptyUnionOrIntersection = 1 << 18;
        const AllowEmptyTuple = 1 << 19;
        const AllowUniqueESSymbolType = 1 << 20;
        const AllowEmptyIndexInfoType = 1 << 21;
        // Errors (cont.)
        const AllowNodeModulesRelativePaths = 1 << 26;
        const IgnoreErrors = Self::AllowThisInObjectLiteral.bits() | Self::AllowQualifiedNameInPlaceOfIdentifier.bits() | Self::AllowAnonymousIdentifier.bits() | Self::AllowEmptyUnionOrIntersection.bits() | Self::AllowEmptyTuple.bits() | Self::AllowEmptyIndexInfoType.bits() | Self::AllowNodeModulesRelativePaths.bits();
        // State
        const InObjectTypeLiteral = 1 << 22;
        const InTypeAlias = 1 << 23;
        const InInitialEntityName = 1 << 24;
    }
}

bitflags! {
    /// Go `nodebuilder.InternalFlags`.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct InternalFlags: u32 {
        const None = 0;
        const WriteComputedProps = 1 << 0;
        const NoSyntacticPrinter = 1 << 1;
        const DoNotIncludeSymbolChain = 1 << 2;
        const AllowUnresolvedNames = 1 << 3;
        const RewriteModuleSpecifiers = 1 << 4;
    }
}

// printer/emitresolver.go

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum SymbolAccessibility {
    #[default]
    Accessible,
    NotAccessible,
    CannotBeNamed,
    NotResolved,
}

/// Go `printer.TypeReferenceSerializationKind` (printer/emitresolver.go).
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum TypeReferenceSerializationKind {
    // The TypeReferenceNode could not be resolved.
    // The type name should be emitted using a safe fallback.
    Unknown,
    // The TypeReferenceNode resolves to a type with a constructor
    // function that can be reached at runtime (e.g. a `class`
    // declaration or a `var` declaration for the static side
    // of a type, such as the global `Promise` type in lib.d.ts).
    TypeWithConstructSignatureAndValue,
    // The TypeReferenceNode resolves to a Void-like, Nullable, or Never type.
    VoidNullableOrNeverType,
    // The TypeReferenceNode resolves to a Number-like type.
    NumberLikeType,
    // The TypeReferenceNode resolves to a BigInt-like type.
    BigIntLikeType,
    // The TypeReferenceNode resolves to a String-like type.
    StringLikeType,
    // The TypeReferenceNode resolves to a Boolean-like type.
    BooleanType,
    // The TypeReferenceNode resolves to an Array-like type.
    ArrayLikeType,
    // The TypeReferenceNode resolves to the ESSymbol type.
    ESSymbolType,
    // The TypeReferenceNode resolved to the global Promise constructor symbol.
    Promise,
    // The TypeReferenceNode resolves to a Function type or a type with call signatures.
    TypeWithCallSignature,
    // The TypeReferenceNode resolves to any other type.
    ObjectType,
}

#[derive(Clone, Debug, Default)]
pub struct SymbolAccessibilityResult {
    pub accessibility: SymbolAccessibility,
    pub aliases_to_make_visible: Vec<P<Node>>, // aliases that need to have this symbol visible
    pub error_symbol_name: String,             // Optional - symbol name that results in error
    pub error_node: Option<P<Node>>,           // Optional - node that results in error
    pub error_module_name: String, // Optional - If the symbol is not visible from module, module's name
}

// checker/nodebuilder.go

/// Go `VerbosityContext`, passed as `Option<P<VerbosityContext>>`; the node builder writes the output fields.
#[derive(Debug, Default)]
pub struct VerbosityContext {
    pub level: Cell<usize>, // 0 = default (no expansion), 1+ = expansion depth
    pub max_truncation_length: Cell<usize>, // 0 = use default
    pub can_increase_verbosity: Cell<bool>, // output: whether increasing Level would reveal more
    pub truncated: Cell<bool>, // output: whether output was truncated
}

// checker/symbolaccessibility.go

/// Passed as `&accessibleSymbolChainContext`. Go copies the struct but shares the map (a reference type), and
/// `canQualifySymbol` builds a new context around the same map, hence the `Rc<RefCell<…>>`.
#[derive(Clone)]
pub struct AccessibleSymbolChainContext {
    pub(crate) symbol: Option<P<Symbol>>, // nil when canQualifySymbol recurses on a symbol without a parent
    pub(crate) enclosing_declaration: Option<P<Node>>,
    pub(crate) meaning: SymbolFlags,
    pub(crate) use_only_external_aliasing: bool,
    pub(crate) visited_symbol_tables_map:
        std::rc::Rc<RefCell<FxHashMap<SymbolId, FxHashSet<SymbolTableID>>>>,
}

// checker/symboltracker.go

/// Go `SymbolTrackerImpl`, handled as `P<SymbolTrackerImpl>` and passed on as `&'static dyn SymbolTracker`
/// (`p.get()`); the inherent methods (printer.rs) take `&self`.
pub struct SymbolTrackerImpl {
    pub(crate) context: P<NodeBuilderContext>,
    pub(crate) inner: Option<&'static dyn SymbolTracker>,
    pub(crate) disable_track_symbol: Cell<bool>,
}

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
    fn as_symbol_tracker_impl(&self) -> Option<&SymbolTrackerImpl> {
        Some(self)
    }
}

// Go `context.Context`.
pub use tsrs_core::context::Context;
