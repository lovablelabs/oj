// Type-, signature- and display-centric handlers (Go tsc/internal/api/session.go).

use tsrs_ast::Kind;
use tsrs_checker::{
    ContextFlags, Flags as NodeBuilderFlags, IndexKind, SignatureKind, Type, TypeDataTag,
    TypeFlags, TypeFormatFlags,
};
use tsrs_core::P;
use tsrs_core::json::Value;

use super::host::{CheckerError, CheckerErrorKind, CheckerHost, CheckerResponse, CheckerResult};
use super::json::obj;
use super::params::Params;
use super::setup::{Setup, SnapshotCtx, touching_property_name_at_utf16};

fn setup<'h>(host: &'h dyn CheckerHost, p: &Params) -> CheckerResult<Setup<'h>> {
    Setup::new(host, p.u64("snapshot")?, p.project()?)
}

// Wrong-kind requests: pinned Go does not validate these; the handler panics and the ipc connection
// (or batchRequests) recovers it as `panic: <value>\n<stack>`. The checks below reproduce the same
// failure class and first line (recorded in testdata/go_probe/go_shapes_b85298b6.jsonl) without
// actually panicking (no stack is attached).
fn go_panic(message: impl std::fmt::Display) -> CheckerError {
    CheckerError { kind: CheckerErrorKind::Internal, message: format!("panic: {message}") }
}

const GO_NIL_DEREF: &str = "runtime error: invalid memory address or nil pointer dereference";

/// Go's name for the `checker.TypeData` implementation behind a type.
fn go_type_data_name(tag: TypeDataTag) -> &'static str {
    match tag {
        TypeDataTag::Intrinsic => "IntrinsicType",
        TypeDataTag::Literal => "LiteralType",
        TypeDataTag::UniqueESSymbol => "UniqueESSymbolType",
        TypeDataTag::Object => "ObjectType",
        TypeDataTag::TypeReference => "TypeReference",
        TypeDataTag::Interface => "InterfaceType",
        TypeDataTag::Tuple => "TupleType",
        TypeDataTag::InstantiationExpression => "InstantiationExpressionType",
        TypeDataTag::Mapped => "MappedType",
        TypeDataTag::ReverseMapped => "ReverseMappedType",
        TypeDataTag::EvolvingArray => "EvolvingArrayType",
        TypeDataTag::Union => "UnionType",
        TypeDataTag::Intersection => "IntersectionType",
        TypeDataTag::TypeParameter => "TypeParameter",
        TypeDataTag::Index => "IndexType",
        TypeDataTag::IndexedAccess => "IndexedAccessType",
        TypeDataTag::TemplateLiteral => "TemplateLiteralType",
        TypeDataTag::StringMapping => "StringMappingType",
        TypeDataTag::Substitution => "SubstitutionType",
        TypeDataTag::Conditional => "ConditionalType",
    }
}

/// Go `t.AsXxx()` (a `t.data.(*Xxx)` type assertion).
fn tagged(t: P<Type>, tag: TypeDataTag) -> CheckerResult<P<Type>> {
    if t.data_tag() == tag {
        Ok(t)
    } else {
        Err(go_panic(format!(
            "interface conversion: checker.TypeData is *checker.{}, not *checker.{}",
            go_type_data_name(t.data_tag()),
            go_type_data_name(tag)
        )))
    }
}

/// Go `t.AsInterfaceType()` (nil for non-interfaces) followed by a field read.
fn interface_of(t: P<Type>) -> CheckerResult<&'static tsrs_checker::InterfaceType> {
    t.try_as_interface_type().ok_or_else(|| go_panic(GO_NIL_DEREF))
}

// --- Location / position types ---

pub fn get_type_at_location(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let node = s.resolve_node(p.string("location")?)?;
    let t = s.c().get_type_at_location(node);
    s.type_response(t)
}

pub fn get_type_at_locations(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let locations = p.string_array("locations")?;
    let mut out = Vec::with_capacity(locations.len());
    for loc in locations {
        let node = s.resolve_node(loc)?;
        let t = s.c().get_type_at_location(node);
        out.push(s.type_response(t)?);
    }
    Ok(Value::Array(out))
}

pub fn get_type_at_position(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let file = s.source_file(&p.document("file")?)?;
    let node = touching_property_name_at_utf16(file, p.u32("position")?);
    let t = s.c().get_type_at_location(node);
    s.type_response(t)
}

pub fn get_types_at_positions(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let file = s.source_file(&p.document("file")?)?;
    let positions = p.u32_array("positions")?;
    let mut out = Vec::with_capacity(positions.len());
    for pos in positions {
        let node = touching_property_name_at_utf16(file, pos);
        let t = s.c().get_type_at_location(node);
        out.push(s.type_response(t)?);
    }
    Ok(Value::Array(out))
}

// --- Type sub-properties (Go GetTypePropertyParams, field "objectId") ---

#[derive(Clone, Copy)]
pub enum TypeProperty {
    Target,
    FreshType,
    RegularType,
    ThisType,
    ObjectType,
    IndexType,
    CheckType,
    ExtendsType,
    BaseType,
    SubstConstraint,
    MappedTypeParameter,
    MappedConstraintType,
    MappedNameType,
    MappedTemplateType,
}

fn type_property(t: P<Type>, property: TypeProperty) -> CheckerResult<Option<P<Type>>> {
    Ok(match property {
        TypeProperty::Target => {
            if !t.flags().intersects(
                TypeFlags::Object
                    | TypeFlags::TypeParameter
                    | TypeFlags::Index
                    | TypeFlags::StringMapping,
            ) {
                return Err(go_panic("Unhandled case in Type.Target"));
            }
            t.target()
        }
        TypeProperty::FreshType => tagged(t, TypeDataTag::Literal)?.as_literal_type().fresh_type(),
        TypeProperty::RegularType => {
            tagged(t, TypeDataTag::Literal)?.as_literal_type().regular_type()
        }
        TypeProperty::ThisType => interface_of(t)?.this_type(),
        TypeProperty::ObjectType => {
            tagged(t, TypeDataTag::IndexedAccess)?.as_indexed_access_type().object_type()
        }
        TypeProperty::IndexType => {
            tagged(t, TypeDataTag::IndexedAccess)?.as_indexed_access_type().index_type()
        }
        TypeProperty::CheckType => {
            tagged(t, TypeDataTag::Conditional)?.as_conditional_type().check_type()
        }
        TypeProperty::ExtendsType => {
            tagged(t, TypeDataTag::Conditional)?.as_conditional_type().extends_type()
        }
        TypeProperty::BaseType => {
            tagged(t, TypeDataTag::Substitution)?.as_substitution_type().base_type()
        }
        TypeProperty::SubstConstraint => {
            tagged(t, TypeDataTag::Substitution)?.as_substitution_type().subst_constraint()
        }
        TypeProperty::MappedTypeParameter => {
            tagged(t, TypeDataTag::Mapped)?.as_mapped_type().type_parameter()
        }
        TypeProperty::MappedConstraintType => {
            tagged(t, TypeDataTag::Mapped)?.as_mapped_type().constraint_type()
        }
        TypeProperty::MappedNameType => {
            tagged(t, TypeDataTag::Mapped)?.as_mapped_type().name_type()
        }
        TypeProperty::MappedTemplateType => {
            tagged(t, TypeDataTag::Mapped)?.as_mapped_type().template_type()
        }
    })
}

/// Go `resolveTypePropertyOfType`.
pub fn resolve_type_property(
    host: &dyn CheckerHost,
    p: &Params,
    property: TypeProperty,
) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let t = s.resolve_type(p.u32("objectId")?)?;
    let result = type_property(t, property)?;
    s.opt_type_response(result)
}

#[derive(Clone, Copy)]
pub enum TypeArrayProperty {
    Types,
    TypeParameters,
    OuterTypeParameters,
    LocalTypeParameters,
    AliasTypeArguments,
}

/// Go `resolveTypeArrayPropertyOfType` (nil/empty → `[]`, json/v2's encoding of a nil slice).
pub fn resolve_type_array_property(
    host: &dyn CheckerHost,
    p: &Params,
    property: TypeArrayProperty,
) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let t = s.resolve_type(p.u32("objectId")?)?;
    let types: &[P<Type>] = match property {
        TypeArrayProperty::Types => {
            if !t.flags().intersects(TypeFlags::UnionOrIntersection | TypeFlags::TemplateLiteral) {
                return Err(go_panic("Unhandled case in Type.Types"));
            }
            t.types()
        }
        TypeArrayProperty::TypeParameters => interface_of(t)?.type_parameters(),
        TypeArrayProperty::OuterTypeParameters => interface_of(t)?.outer_type_parameters(),
        TypeArrayProperty::LocalTypeParameters => interface_of(t)?.local_type_parameters(),
        TypeArrayProperty::AliasTypeArguments => t.alias().map_or(&[], |a| a.type_arguments()),
    };
    if types.is_empty() {
        return Ok(Value::Array(Vec::new())); // Go nil slice: json/v2 encodes []
    }
    s.types_response(types)
}

#[derive(Clone, Copy)]
pub enum TypeSymbolProperty {
    Symbol,
    AliasSymbol,
}

/// Go `resolveSymbolPropertyOfType` (no checker).
pub fn resolve_type_symbol_property(
    host: &dyn CheckerHost,
    p: &Params,
    property: TypeSymbolProperty,
) -> CheckerResult<Value> {
    let sd = SnapshotCtx::new(host, p.u64("snapshot")?, p.project()?)?;
    let t = sd.resolve_type(p.u32("objectId")?, None)?;
    let result = match property {
        TypeSymbolProperty::Symbol => t.symbol(),
        TypeSymbolProperty::AliasSymbol => t.alias().and_then(|a| a.symbol()),
    };
    match result {
        Some(symbol) => sd.symbol_response(symbol, &sd.project),
        None => Ok(Value::Null),
    }
}

// --- Checker operations on a type (Go CheckerTypeParams / GetTypePropertyParams etc.) ---

#[derive(Clone, Copy)]
pub enum TypeOp {
    Awaited,
    BaseTypeOfLiteral,
    NonNullable,
    Widened,
    Apparent,
    Reduced,
    ConstraintOfTypeParameter,
    DefaultFromTypeParameter,
    BaseConstraint,
    TrueTypeOfConditional,
    FalseTypeOfConditional,
}

/// `field` is the param name holding the type id ("type" or "objectId", per the pinned params struct).
pub fn type_op(
    host: &dyn CheckerHost,
    p: &Params,
    field: &str,
    op: TypeOp,
) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let t = s.resolve_type(p.u32(field)?)?;
    let c = s.c();
    let result = match op {
        TypeOp::Awaited => c.get_awaited_type_exported(t),
        TypeOp::BaseTypeOfLiteral => Some(c.get_base_type_of_literal_type_exported(t)),
        TypeOp::NonNullable => Some(c.get_non_nullable_type(t)),
        TypeOp::Widened => Some(c.get_widened_type_exported(t)),
        TypeOp::Apparent => Some(c.get_apparent_type_exported(t)),
        TypeOp::Reduced => Some(c.get_reduced_type_exported(t)),
        // Pinned Go answers null for non-type-parameters (no panic).
        TypeOp::ConstraintOfTypeParameter => c.get_constraint_of_type_parameter_exported(t),
        TypeOp::DefaultFromTypeParameter => c.get_default_from_type_parameter_exported(t),
        TypeOp::BaseConstraint => c.get_base_constraint_of_type_exported(t),
        TypeOp::TrueTypeOfConditional => {
            Some(c.get_true_type_of_conditional_type(tagged(t, TypeDataTag::Conditional)?))
        }
        TypeOp::FalseTypeOfConditional => {
            Some(c.get_false_type_of_conditional_type(tagged(t, TypeDataTag::Conditional)?))
        }
    };
    s.opt_type_response(result)
}

#[derive(Clone, Copy)]
pub enum TypeListOp {
    BaseTypes,
    TypeArguments,
}

pub fn type_list_op(host: &dyn CheckerHost, p: &Params, op: TypeListOp) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let t = s.resolve_type(p.u32("type")?)?;
    let types = match op {
        // Pinned Go answers [] for non-class/interface types (getBaseTypes checks the object flags first).
        TypeListOp::BaseTypes => s.c().get_base_types_exported(t),
        // Pinned Go dereferences a nil `AsTypeReference()` here.
        TypeListOp::TypeArguments => {
            if t.try_as_type_reference().is_none() {
                return Err(go_panic(GO_NIL_DEREF));
            }
            s.c().get_type_arguments_exported(t)
        }
    };
    if types.is_empty() {
        return Ok(Value::Array(Vec::new())); // Go nil slice: json/v2 encodes []
    }
    s.types_response(types)
}

#[derive(Clone, Copy)]
pub enum TypePredicateOp {
    IsArray,
    IsArrayLike,
}

pub fn type_predicate_op(
    host: &dyn CheckerHost,
    p: &Params,
    op: TypePredicateOp,
) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let t = s.resolve_type(p.u32("type")?)?;
    Ok(Value::Bool(match op {
        TypePredicateOp::IsArray => s.c().is_array_type_exported(t),
        TypePredicateOp::IsArrayLike => s.c().is_array_like_type_exported(t),
    }))
}

pub fn is_type_assignable_to(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let source = s.resolve_type(p.u32("source")?)?;
    let target = s.resolve_type(p.u32("target")?)?;
    Ok(Value::Bool(s.c().is_type_assignable_to_exported(source, target)))
}

pub fn get_properties_of_type(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let t = s.resolve_type(p.u32("type")?)?;
    let props = s.c().get_properties_of_type_exported(t);
    if props.is_empty() {
        return Ok(Value::Array(Vec::new())); // Go nil slice: json/v2 encodes []
    }
    s.symbols_response(props)
}

pub fn get_apparent_properties_of_type(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let t = s.resolve_type(p.u32("objectId")?)?;
    let props = s.c().get_apparent_properties(t);
    s.symbols_response(&props)
}

pub fn get_property_of_type(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let t = s.resolve_type(p.u32("type")?)?;
    let prop = s.c().get_property_of_type_exported(t, p.string("name")?);
    s.opt_symbol_response(prop)
}

pub fn get_type_of_property_of_type(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let t = s.resolve_type(p.u32("type")?)?;
    let prop_type = s.c().get_type_of_property_of_type_exported(t, p.string("name")?);
    s.opt_type_response(prop_type)
}

pub fn get_index_infos_of_type(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let t = s.resolve_type(p.u32("type")?)?;
    let infos = s.c().get_index_infos_of_type_exported(t);
    if infos.is_empty() {
        return Ok(Value::Array(Vec::new())); // Go nil slice: json/v2 encodes []
    }
    let mut out = Vec::with_capacity(infos.len());
    for info in infos {
        out.push(s.index_info_response(*info)?);
    }
    Ok(Value::Array(out))
}

pub fn get_index_info_of_type(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let t = s.resolve_type(p.u32("type")?)?;
    let kind = p.i32("kind")?;
    let key_type = if kind == IndexKind::String as i32 {
        s.c().get_string_type()
    } else if kind == IndexKind::Number as i32 {
        s.c().get_number_type()
    } else {
        return Err(CheckerError::client(format!("invalid index kind {kind}")));
    };
    match s.c().get_index_info_of_type_exported(t, key_type) {
        Some(info) => s.index_info_response(info),
        None => Ok(Value::Null),
    }
}

pub fn get_signatures_of_type(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let t = s.resolve_type(p.u32("type")?)?;
    let kind = match p.i32("kind")? {
        0 => SignatureKind::Call,
        1 => SignatureKind::Construct,
        // Pinned Go passes any value through as `checker.SignatureKind` and finds no signatures.
        _ => return Ok(Value::Array(Vec::new())),
    };
    let sigs = s.c().get_signatures_of_type_exported(t, kind);
    Ok(Value::Array(
        sigs.iter().map(|sig| s.signature_response(*sig)).collect::<CheckerResult<_>>()?,
    ))
}

// --- Node-based checker queries ---

#[derive(Clone, Copy)]
pub enum NodeTypeOp {
    Contextual,
    FromTypeNode,
}

pub fn node_type_op(host: &dyn CheckerHost, p: &Params, op: NodeTypeOp) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let node = s.resolve_node(p.string("location")?)?;
    let t = match op {
        NodeTypeOp::Contextual => s.c().get_contextual_type_exported(node, ContextFlags::None),
        NodeTypeOp::FromTypeNode => Some(s.c().get_type_from_type_node_exported(node)),
    };
    s.opt_type_response(t)
}

pub fn get_contextual_type_for_argument(
    host: &dyn CheckerHost,
    p: &Params,
) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let node = s.resolve_node(p.string("location")?)?;
    let index = usize::try_from(p.i32("index")?)
        .map_err(|_| CheckerError::client("invalid parameter index"))?;
    let t = s.c().get_contextual_type_for_argument_at_index_exported(node, index);
    s.opt_type_response(Some(t))
}

pub fn is_context_sensitive(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let node = s.resolve_node(p.string("location")?)?;
    Ok(Value::Bool(s.c().is_context_sensitive_exported(node)))
}

pub fn get_type_of_symbol_at_location(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let symbol = s.resolve_symbol(&p.symbol_ref("symbol")?)?;
    let node = s.resolve_node(p.string("location")?)?;
    let t = s.c().get_type_of_symbol_at_location(symbol, Some(node));
    s.opt_type_response(t)
}

/// Go `handleGetConstantValue` → `ConstantValueResponse { isNumber, value }`.
pub fn get_constant_value(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let node = s.resolve_node(p.string("location")?)?;
    let value = s.c().get_constant_value(node);
    let mut o = obj();
    o.set("isNumber", Value::Bool(matches!(value, Some(tsrs_checker::LiteralValue::Number(_)))));
    o.set("value", super::json::literal_value_to_json(value));
    Ok(o.build())
}

pub fn get_resolved_signature(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let node = s.resolve_node(p.string("location")?)?;
    let sig = s.c().get_resolved_signature_exported(node);
    s.signature_response(sig)
}

pub fn get_signature_from_declaration(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let node = s.resolve_node(p.string("location")?)?;
    let sig = s.c().get_signature_from_declaration_exported(node);
    s.signature_response(sig)
}

// --- Signatures ---

/// Go `resolveTypeArrayPropertyOfSignature` (typeParameters; nil/empty → `[]`).
pub fn get_type_parameters_of_signature(
    host: &dyn CheckerHost,
    p: &Params,
) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let sig = s.resolve_signature(p.u64("objectId")?)?;
    let types = sig.type_parameters();
    if types.is_empty() {
        return Ok(Value::Array(Vec::new())); // Go nil slice: json/v2 encodes []
    }
    s.types_response(types)
}

/// Go `resolveSymbolArrayPropertyOfSignature` / `resolveSymbolPropertyOfSignature` /
/// `resolveSignaturePropertyOfSignature` (no checker).
#[derive(Clone, Copy)]
pub enum SignatureProperty {
    Parameters,
    ThisParameter,
    Target,
}

pub fn signature_property(
    host: &dyn CheckerHost,
    p: &Params,
    property: SignatureProperty,
) -> CheckerResult<Value> {
    let sd = SnapshotCtx::new(host, p.u64("snapshot")?, p.project()?)?;
    let sig = sd.resolve_signature(p.u64("objectId")?, None)?;
    match property {
        SignatureProperty::Parameters => {
            let params = sig.parameters();
            if params.is_empty() {
                return Ok(Value::Array(Vec::new())); // Go nil slice: json/v2 encodes []
            }
            Ok(Value::Array(
                params
                    .iter()
                    .map(|sym| sd.symbol_response(*sym, &sd.project))
                    .collect::<CheckerResult<_>>()?,
            ))
        }
        SignatureProperty::ThisParameter => match sig.this_parameter() {
            Some(sym) => sd.symbol_response(sym, &sd.project),
            None => Ok(Value::Null),
        },
        SignatureProperty::Target => match sig.target() {
            Some(target) => sd.signature_response(sd.registered_checker_id(), target),
            None => Ok(Value::Null),
        },
    }
}

#[derive(Clone, Copy)]
pub enum SignatureTypeOp {
    ReturnType,
    RestType,
}

/// `field`: "objectId" for getReturnTypeOfSignature (`GetSignaturePropertyParams`), "signature" for
/// getRestTypeOfSignature (`CheckerSignatureParams`).
pub fn signature_type_op(
    host: &dyn CheckerHost,
    p: &Params,
    field: &str,
    op: SignatureTypeOp,
) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let sig = s.resolve_signature(p.u64(field)?)?;
    let t = match op {
        SignatureTypeOp::ReturnType => s.c().get_return_type_of_signature_exported(sig),
        SignatureTypeOp::RestType => s.c().get_rest_type_of_signature_exported(sig),
    };
    s.type_response(t)
}

#[derive(Clone, Copy)]
pub enum ParameterAt {
    ParameterType,
    TypeParameter,
}

pub fn signature_parameter_at(
    host: &dyn CheckerHost,
    p: &Params,
    op: ParameterAt,
) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let sig = s.resolve_signature(p.u64("signature")?)?;
    let index = usize::try_from(p.i32("index")?)
        .map_err(|_| CheckerError::client("invalid parameter index"))?;
    let t = match op {
        ParameterAt::ParameterType => s.c().get_type_at_position_exported(sig, index),
        ParameterAt::TypeParameter => s.c().get_type_parameter_at_position(sig, index),
    };
    s.type_response(t)
}

pub fn get_type_predicate_of_signature(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let sig = s.resolve_signature(p.u64("signature")?)?;
    let Some(pred) = s.c().get_type_predicate_of_signature_exported(sig) else {
        return Ok(Value::Null);
    };
    let mut o = obj();
    o.num("kind", pred.kind() as i32 as f64);
    o.num("parameterIndex", pred.parameter_index().map_or(-1.0, |index| index as f64));
    o.str_nonempty("parameterName", pred.parameter_name());
    if let Some(t) = pred.type_() {
        o.set("type", s.type_response(t)?);
    }
    Ok(o.build())
}

// --- Intrinsics and well-known singletons ---

#[derive(Clone, Copy)]
pub enum Intrinsic {
    Any,
    String,
    Number,
    Boolean,
    Void,
    Undefined,
    Null,
    Never,
    Unknown,
    BigInt,
    ESSymbol,
    NonPrimitive,
}

pub fn get_intrinsic_type(
    host: &dyn CheckerHost,
    p: &Params,
    which: Intrinsic,
) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let c = s.c();
    let t = match which {
        Intrinsic::Any => c.get_any_type(),
        Intrinsic::String => c.get_string_type(),
        Intrinsic::Number => c.get_number_type(),
        Intrinsic::Boolean => c.get_boolean_type(),
        Intrinsic::Void => c.get_void_type(),
        Intrinsic::Undefined => c.get_undefined_type(),
        Intrinsic::Null => c.get_null_type(),
        Intrinsic::Never => c.get_never_type(),
        Intrinsic::Unknown => c.get_unknown_type(),
        Intrinsic::BigInt => c.get_big_int_type(),
        Intrinsic::ESSymbol => c.get_es_symbol_type(),
        Intrinsic::NonPrimitive => c.get_non_primitive_type(),
    };
    s.type_response(t)
}

pub fn get_well_known_symbols(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let unknown = s.c().get_unknown_symbol();
    let undefined = s.c().get_undefined_symbol();
    let arguments = s.c().get_arguments_symbol();
    let registry = &s.sd.scope.registry;
    let mut o = obj();
    for (name, symbol) in [("unknown", unknown), ("undefined", undefined), ("arguments", arguments)]
    {
        let (id, _) = registry.register_symbol(symbol, &s.sd.project)?;
        o.num(name, id as f64);
    }
    Ok(o.build())
}

pub fn get_well_known_signatures(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let unknown = s.c().get_unknown_signature();
    let id = s.sd.scope.registry.register_signature(&s.sd.project, s.checker_id, unknown)?;
    let mut o = obj();
    o.num("unknown", id as f64);
    Ok(o.build())
}

// --- Display ---

fn enclosing(s: &Setup, p: &Params) -> CheckerResult<Option<P<tsrs_ast::Node>>> {
    let location = p.string("location")?;
    if location.is_empty() {
        return Ok(None);
    }
    s.resolve_node(location).map(Some)
}

pub fn type_to_string(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let mut s = setup(host, p)?;
    let t = s.resolve_type(p.u32("type")?)?;
    let enclosing = enclosing(&s, p)?;
    let flags = p.i32("flags")?;
    let flags = if flags != 0 {
        TypeFormatFlags::from_bits_retain(flags as u32)
    } else {
        TypeFormatFlags::AllowUniqueESSymbolType
            | TypeFormatFlags::UseAliasDefinedOutsideCurrentScope
    };
    Ok(Value::String(s.c().type_to_string_ex_exported(t, enclosing, flags, None)))
}

pub fn type_to_type_node(host: &dyn CheckerHost, p: &Params) -> CheckerResult<CheckerResponse> {
    let mut s = setup(host, p)?;
    let t = s.resolve_type(p.u32("type")?)?;
    let enclosing = enclosing(&s, p)?;
    let flags = NodeBuilderFlags::from_bits_retain(p.i32("flags")? as u32);
    match s.c().type_to_type_node(t, enclosing, flags, None) {
        Some(node) => Ok(CheckerResponse::EncodedNode(host.encode_node(node)?)),
        None => Ok(CheckerResponse::Json(Value::Null)),
    }
}

pub fn signature_to_signature_declaration(
    host: &dyn CheckerHost,
    p: &Params,
) -> CheckerResult<CheckerResponse> {
    let mut s = setup(host, p)?;
    let sig = s.resolve_signature(p.u64("signature")?)?;
    let enclosing = enclosing(&s, p)?;
    let kind = p.i32("kind")?;
    if kind < 0 || kind > Kind::Count as i32 {
        // Out of the AST kind range Go hits the same default case as any unhandled kind.
        return Err(go_panic("Unhandled kind in signatureToSignatureDeclarationHelper"));
    }
    let kind = Kind::from_i16(kind as i16);
    let flags = NodeBuilderFlags::from_bits_retain(p.i32("flags")? as u32);
    match s.c().signature_to_signature_declaration(sig, kind, enclosing, flags) {
        Some(node) => Ok(CheckerResponse::EncodedNode(host.encode_node(node)?)),
        None => Ok(CheckerResponse::Json(Value::Null)),
    }
}
