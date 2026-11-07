use std::sync::LazyLock;

use tsrs_ast::Node;
use tsrs_core::{P, alloc_slice};

// `PseudoType`s are skeletons of types - partially interpreted expressions and type nodes
// composed to represent how you *should* construct a type out of them. They can be trivially
// mapped into actual types by a real `Checker`, or into a tree of `Node`s directly, without
// needing to make any intermediate types, by a `NodeBuilder`. Unlike checker `Type`s, these are
// never normalized, and multiple pseudo-types may refer to the same underlying `Type`.

// In strada, these were implicit in the AST nodes constructed in `expressionToTypeNode.ts`, which
// repurposed AST nodes for this purpose, but in so doing, often confused weather or not it had validated
// nested nodes for use at a given use-site. By keeping the mapping deferred like this, we can know we haven't
// done any use-site checks until we're ready to map the `PseudoType` into a `Node`, and can cache
// `PseudoType`s across multiple target positions.

#[repr(i16)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum PseudoTypeKind {
    Direct,
    Inferred,
    NoResult,
    MaybeConstLocation,
    Union,
    Undefined,
    Null,
    Any,
    String,
    Number,
    BigInt,
    Boolean,
    False,
    True,
    SingleCallSignature,
    Tuple,
    ObjectLiteral,
    StringLiteral,
    NumericLiteral,
    BigIntLiteral,
}

/// Go `PseudoType` (handled as `P<PseudoType>`, immutable).
///
/// Go's `data pseudoTypeData` interface, whose implementations embed the `PseudoType` header, is the `data`
/// enum; `as_pseudo_type_x()` are Go's `AsPseudoTypeX()` casts (panic on a shape mismatch like the Go type
/// assertion).
pub struct PseudoType {
    pub kind: PseudoTypeKind,
    pub data: PseudoTypeData,
}

/// Go's `pseudoTypeData` implementations. `Base` is `PseudoTypeBase` (the keyword-like kinds); `Literal` serves the
/// three literal kinds.
pub enum PseudoTypeData {
    Base,
    Direct(PseudoTypeDirect),
    Inferred(PseudoTypeInferred),
    NoResult(PseudoTypeNoResult),
    MaybeConstLocation(PseudoTypeMaybeConstLocation),
    Union(PseudoTypeUnion),
    SingleCallSignature(PseudoTypeSingleCallSignature),
    Tuple(PseudoTypeTuple),
    ObjectLiteral(PseudoTypeObjectLiteral),
    Literal(PseudoTypeLiteral),
}

// Pseudo types are built per node builder request and never cached: in the scratch region during emit
// (notes/mem-emit-regions.md).
fn new_pseudo_type(kind: PseudoTypeKind, data: PseudoTypeData) -> P<PseudoType> {
    P::new_scratch(PseudoType { kind, data })
}

// The shared pseudo types below are process-wide: never in a freeable region (language server).
fn new_static_pseudo_type(kind: PseudoTypeKind) -> P<PseudoType> {
    let _arena = tsrs_core::arena::enter_thread_arena();
    P::new(PseudoType { kind, data: PseudoTypeData::Base })
}

pub static PSEUDO_TYPE_UNDEFINED: LazyLock<P<PseudoType>> =
    LazyLock::new(|| new_static_pseudo_type(PseudoTypeKind::Undefined));
pub(crate) static PSEUDO_TYPE_NULL: LazyLock<P<PseudoType>> =
    LazyLock::new(|| new_static_pseudo_type(PseudoTypeKind::Null));

pub(crate) static PSEUDO_TYPE_STRING: LazyLock<P<PseudoType>> =
    LazyLock::new(|| new_static_pseudo_type(PseudoTypeKind::String));
pub(crate) static PSEUDO_TYPE_NUMBER: LazyLock<P<PseudoType>> =
    LazyLock::new(|| new_static_pseudo_type(PseudoTypeKind::Number));
pub(crate) static PSEUDO_TYPE_BIG_INT: LazyLock<P<PseudoType>> =
    LazyLock::new(|| new_static_pseudo_type(PseudoTypeKind::BigInt));
pub(crate) static PSEUDO_TYPE_BOOLEAN: LazyLock<P<PseudoType>> =
    LazyLock::new(|| new_static_pseudo_type(PseudoTypeKind::Boolean));
pub(crate) static PSEUDO_TYPE_FALSE: LazyLock<P<PseudoType>> =
    LazyLock::new(|| new_static_pseudo_type(PseudoTypeKind::False));
pub(crate) static PSEUDO_TYPE_TRUE: LazyLock<P<PseudoType>> =
    LazyLock::new(|| new_static_pseudo_type(PseudoTypeKind::True));

macro_rules! pseudo_type_cast {
    ($name:ident, $variant:ident, $ty:ty) => {
        pub fn $name(&self) -> &$ty {
            match &self.data {
                PseudoTypeData::$variant(d) => d,
                _ => panic!(
                    concat!(stringify!($name), ": unexpected pseudo type kind {:?}"),
                    self.kind
                ),
            }
        }
    };
}

impl PseudoType {
    pseudo_type_cast!(as_pseudo_type_direct, Direct, PseudoTypeDirect);
    pseudo_type_cast!(as_pseudo_type_inferred, Inferred, PseudoTypeInferred);
    pseudo_type_cast!(as_pseudo_type_no_result, NoResult, PseudoTypeNoResult);
    pseudo_type_cast!(
        as_pseudo_type_maybe_const_location,
        MaybeConstLocation,
        PseudoTypeMaybeConstLocation
    );
    pseudo_type_cast!(as_pseudo_type_union, Union, PseudoTypeUnion);
    pseudo_type_cast!(
        as_pseudo_type_single_call_signature,
        SingleCallSignature,
        PseudoTypeSingleCallSignature
    );
    pseudo_type_cast!(as_pseudo_type_tuple, Tuple, PseudoTypeTuple);
    pseudo_type_cast!(as_pseudo_type_object_literal, ObjectLiteral, PseudoTypeObjectLiteral);
    pseudo_type_cast!(as_pseudo_type_literal, Literal, PseudoTypeLiteral);
}

// PseudoTypeDirect directly encodes the type referred to by a given TypeNode
pub struct PseudoTypeDirect {
    pub type_node: P<Node>,
}

pub(crate) fn new_pseudo_type_direct(type_node: P<Node>) -> P<PseudoType> {
    new_pseudo_type(PseudoTypeKind::Direct, PseudoTypeData::Direct(PseudoTypeDirect { type_node }))
}

// PseudoTypeInferred directly encodes the type referred to by a given Expression
// These represent cases where the expression was too complex for the pseudochecker.
// Most of the time, these locations will produce an error under ID.
// Specific error nodes (shorthand properties, spread assignments, etc.) are stored on the
// ErrorNodes field, collected during pseudochecker construction.
pub struct PseudoTypeInferred {
    pub expression: P<Node>,
    pub error_nodes: &'static [P<Node>],
    pub is_signature_return: bool,
}

pub(crate) fn new_pseudo_type_inferred(expr: P<Node>, is_signature_return: bool) -> P<PseudoType> {
    new_pseudo_type(
        PseudoTypeKind::Inferred,
        PseudoTypeData::Inferred(PseudoTypeInferred {
            expression: expr,
            error_nodes: &[],
            is_signature_return,
        }),
    )
}

pub(crate) fn new_pseudo_type_inferred_with_errors(
    expr: P<Node>,
    is_signature_return: bool,
    error_nodes: &[P<Node>],
) -> P<PseudoType> {
    new_pseudo_type(
        PseudoTypeKind::Inferred,
        PseudoTypeData::Inferred(PseudoTypeInferred {
            expression: expr,
            error_nodes: alloc_slice(error_nodes),
            is_signature_return,
        }),
    )
}

// PseudoTypeNoResult is analogous to PseudoTypeInferred in that it references a case
// where the type was too complex for the pseudochecker. Rather than an expression, however,
// it is referring to the return type of a signature or declaration.
pub struct PseudoTypeNoResult {
    pub declaration: P<Node>,
}

pub(crate) fn new_pseudo_type_no_result(decl: P<Node>) -> P<PseudoType> {
    new_pseudo_type(
        PseudoTypeKind::NoResult,
        PseudoTypeData::NoResult(PseudoTypeNoResult { declaration: decl }),
    )
}

// PseudoTypeMaybeConstLocation encodes the const/regular types of a location so the builder
// can later select the appropriate pseudotype based on the location's context. This is used
// to ensure accuracy in nested expressions without exposing type-based functionality to the pseudochecker.
// A nodebuilder that doesn't do contextual typing would need to, as policy, reject these types if they
// are in a contextually typed position! (Otherwise they could pick one, but either type could be wrong, depending on context!)
// At the top-level, which is generally what ID is concerned with, nothing is contextually typed, so these cases don't generally
// cause problems. Once you get into reused nodes in nested expressions, however, this becomes important.
// In strada, checker `isConstContext` functionality exposed to the pseudochecker + type comparison sanity checking
// on nested results masks the need for this abstraction, but with it present it clearly highlights a shortcoming
// of the ID infernce model and how "standalone" it can(n't) truly be without substantial restrictions on expression inference.
pub struct PseudoTypeMaybeConstLocation {
    pub node: P<Node>,
    pub const_type: P<PseudoType>,
    pub regular_type: P<PseudoType>,
}

pub(crate) fn new_pseudo_type_maybe_const_location(
    loc: P<Node>,
    ct: P<PseudoType>,
    reg: P<PseudoType>,
) -> P<PseudoType> {
    new_pseudo_type(
        PseudoTypeKind::MaybeConstLocation,
        PseudoTypeData::MaybeConstLocation(PseudoTypeMaybeConstLocation {
            node: loc,
            const_type: ct,
            regular_type: reg,
        }),
    )
}

// PseudoTypeUnion is a collection of psudotypes joined into a union
pub struct PseudoTypeUnion {
    pub types: &'static [P<PseudoType>],
}

pub fn new_pseudo_type_union(types: &[P<PseudoType>]) -> P<PseudoType> {
    new_pseudo_type(
        PseudoTypeKind::Union,
        PseudoTypeData::Union(PseudoTypeUnion { types: alloc_slice(types) }),
    )
}

/// Go `PseudoParameter`, handled as `P<PseudoParameter>`.
pub struct PseudoParameter {
    pub rest: bool,
    pub name: P<Node>,
    pub optional: bool,
    pub type_: P<PseudoType>,
}

pub(crate) fn new_pseudo_parameter(
    is_rest: bool,
    name: P<Node>,
    is_optional: bool,
    t: P<PseudoType>,
) -> P<PseudoParameter> {
    P::new(PseudoParameter { rest: is_rest, name, optional: is_optional, type_: t })
}

// PseudoTypeSingleCallSignature represents an object type with a single call signature, like an arrow or function expression
pub struct PseudoTypeSingleCallSignature {
    pub signature: P<Node>,
    pub parameters: &'static [P<PseudoParameter>],
    pub type_parameters: &'static [P<Node>], // TypeParameterDeclaration nodes
    pub return_type: P<PseudoType>,
}

pub(crate) fn new_pseudo_type_single_call_signature(
    signature: P<Node>,
    parameters: &[P<PseudoParameter>],
    type_parameters: &[P<Node>],
    return_type: P<PseudoType>,
) -> P<PseudoType> {
    new_pseudo_type(
        PseudoTypeKind::SingleCallSignature,
        PseudoTypeData::SingleCallSignature(PseudoTypeSingleCallSignature {
            signature,
            parameters: alloc_slice(parameters),
            type_parameters: alloc_slice(type_parameters),
            return_type,
        }),
    )
}

// PseudoTypeTuple represents a tuple originaing from an `as const` array literal
pub struct PseudoTypeTuple {
    pub elements: &'static [P<PseudoType>],
}

pub(crate) fn new_pseudo_type_tuple(elements: &[P<PseudoType>]) -> P<PseudoType> {
    new_pseudo_type(
        PseudoTypeKind::Tuple,
        PseudoTypeData::Tuple(PseudoTypeTuple { elements: alloc_slice(elements) }),
    )
}

/// Go `PseudoObjectElement` (handled as `P<PseudoObjectElement>`); Go's `data pseudoObjectElementData` is the
/// `data` enum, `as_pseudo_object_method()` & co. are the casts.
pub struct PseudoObjectElement {
    pub name: P<Node>,
    pub optional: bool,
    pub kind: PseudoObjectElementKind,
    data: PseudoObjectElementData,
}

pub enum PseudoObjectElementData {
    Method(PseudoObjectMethod),
    PropertyAssignment(PseudoPropertyAssignment),
    SetAccessor(PseudoSetAccessor),
    GetAccessor(PseudoGetAccessor),
}

impl PseudoObjectElement {
    pub fn signature(&self) -> Option<P<Node>> {
        match self.kind {
            PseudoObjectElementKind::Method => Some(self.as_pseudo_object_method().signature),
            PseudoObjectElementKind::SetAccessor => Some(self.as_pseudo_set_accessor().signature),
            PseudoObjectElementKind::GetAccessor => Some(self.as_pseudo_get_accessor().signature),
            PseudoObjectElementKind::PropertyAssignment => None,
        }
    }

    pub fn as_pseudo_object_method(&self) -> &PseudoObjectMethod {
        match &self.data {
            PseudoObjectElementData::Method(d) => d,
            _ => panic!("as_pseudo_object_method: unexpected element kind {:?}", self.kind),
        }
    }

    pub fn as_pseudo_property_assignment(&self) -> &PseudoPropertyAssignment {
        match &self.data {
            PseudoObjectElementData::PropertyAssignment(d) => d,
            _ => panic!("as_pseudo_property_assignment: unexpected element kind {:?}", self.kind),
        }
    }

    pub fn as_pseudo_set_accessor(&self) -> &PseudoSetAccessor {
        match &self.data {
            PseudoObjectElementData::SetAccessor(d) => d,
            _ => panic!("as_pseudo_set_accessor: unexpected element kind {:?}", self.kind),
        }
    }

    pub fn as_pseudo_get_accessor(&self) -> &PseudoGetAccessor {
        match &self.data {
            PseudoObjectElementData::GetAccessor(d) => d,
            _ => panic!("as_pseudo_get_accessor: unexpected element kind {:?}", self.kind),
        }
    }
}

#[repr(i8)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum PseudoObjectElementKind {
    Method,
    PropertyAssignment,
    SetAccessor,
    GetAccessor,
}

fn new_pseudo_object_element(
    kind: PseudoObjectElementKind,
    name: P<Node>,
    optional: bool,
    data: PseudoObjectElementData,
) -> P<PseudoObjectElement> {
    P::new(PseudoObjectElement { name, optional, kind, data })
}

pub struct PseudoObjectMethod {
    signature: P<Node>,
    pub type_parameters: &'static [P<Node>], // TypeParameterDeclaration nodes
    pub parameters: &'static [P<PseudoParameter>],
    pub return_type: P<PseudoType>,
}

pub(crate) fn new_pseudo_object_method(
    signature: P<Node>,
    name: P<Node>,
    optional: bool,
    type_parameters: &[P<Node>],
    parameters: &[P<PseudoParameter>],
    return_type: P<PseudoType>,
) -> P<PseudoObjectElement> {
    new_pseudo_object_element(
        PseudoObjectElementKind::Method,
        name,
        optional,
        PseudoObjectElementData::Method(PseudoObjectMethod {
            signature,
            type_parameters: alloc_slice(type_parameters),
            parameters: alloc_slice(parameters),
            return_type,
        }),
    )
}

pub struct PseudoPropertyAssignment {
    pub readonly: bool,
    pub type_: P<PseudoType>,
}

pub(crate) fn new_pseudo_property_assignment(
    readonly: bool,
    name: P<Node>,
    optional: bool,
    t: P<PseudoType>,
) -> P<PseudoObjectElement> {
    new_pseudo_object_element(
        PseudoObjectElementKind::PropertyAssignment,
        name,
        optional,
        PseudoObjectElementData::PropertyAssignment(PseudoPropertyAssignment {
            readonly,
            type_: t,
        }),
    )
}

pub struct PseudoSetAccessor {
    signature: P<Node>,
    pub parameter: P<PseudoParameter>,
}

pub(crate) fn new_pseudo_set_accessor(
    signature: P<Node>,
    name: P<Node>,
    optional: bool,
    p: P<PseudoParameter>,
) -> P<PseudoObjectElement> {
    new_pseudo_object_element(
        PseudoObjectElementKind::SetAccessor,
        name,
        optional,
        PseudoObjectElementData::SetAccessor(PseudoSetAccessor { signature, parameter: p }),
    )
}

pub struct PseudoGetAccessor {
    signature: P<Node>,
    pub type_: P<PseudoType>,
}

pub(crate) fn new_pseudo_get_accessor(
    signature: P<Node>,
    name: P<Node>,
    optional: bool,
    t: P<PseudoType>,
) -> P<PseudoObjectElement> {
    new_pseudo_object_element(
        PseudoObjectElementKind::GetAccessor,
        name,
        optional,
        PseudoObjectElementData::GetAccessor(PseudoGetAccessor { signature, type_: t }),
    )
}

// PseudoTypeObjectLiteral represents an object type originaing from an object literal
pub struct PseudoTypeObjectLiteral {
    pub elements: &'static [P<PseudoObjectElement>],
}

pub(crate) fn new_pseudo_type_object_literal(elements: &[P<PseudoObjectElement>]) -> P<PseudoType> {
    new_pseudo_type(
        PseudoTypeKind::ObjectLiteral,
        PseudoTypeData::ObjectLiteral(PseudoTypeObjectLiteral { elements: alloc_slice(elements) }),
    )
}

// PseudoTypeLiteral represents a literal type
pub struct PseudoTypeLiteral {
    pub node: P<Node>,
}

pub(crate) fn new_pseudo_type_string_literal(node: P<Node>) -> P<PseudoType> {
    new_pseudo_type(
        PseudoTypeKind::StringLiteral,
        PseudoTypeData::Literal(PseudoTypeLiteral { node }),
    )
}

pub(crate) fn new_pseudo_type_numeric_literal(node: P<Node>) -> P<PseudoType> {
    new_pseudo_type(
        PseudoTypeKind::NumericLiteral,
        PseudoTypeData::Literal(PseudoTypeLiteral { node }),
    )
}

pub(crate) fn new_pseudo_type_big_int_literal(node: P<Node>) -> P<PseudoType> {
    new_pseudo_type(
        PseudoTypeKind::BigIntLiteral,
        PseudoTypeData::Literal(PseudoTypeLiteral { node }),
    )
}
