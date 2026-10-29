// Method → handler table for the checker lane (Go `Session.HandleRequest` cases for these methods).
// The param field names follow each method's pinned params struct (`unmarshalers` in proto.go).

use std::sync::LazyLock;

use rustc_hash::FxHashSet;
use tsrs_core::json::Value;

use super::handlers_ls as ls;
use super::handlers_symbols::{
    self as sym, AliasQuery, SymbolProperty, SymbolTableProperty, SymbolTypeQuery,
};
use super::handlers_types::{
    self as ty, Intrinsic, NodeTypeOp, ParameterAt, SignatureProperty, SignatureTypeOp,
    TypeArrayProperty, TypeListOp, TypeOp, TypePredicateOp, TypeProperty, TypeSymbolProperty,
};
use super::host::{CheckerHost, CheckerResponse, CheckerResult};
use super::params::Params;

/// Handles a checker-lane method. Returns `None` for methods this lane does not implement, so the core
/// dispatcher can fall through to other lanes.
pub fn handle(
    host: &dyn CheckerHost,
    method: &str,
    params: &Value,
) -> Option<CheckerResult<CheckerResponse>> {
    if !is_checker_method(method) {
        return None;
    }
    let p = match Params::new(params, method) {
        Ok(p) => p,
        Err(e) => return Some(Err(e)),
    };
    Some(dispatch(host, method, &p))
}

fn json(r: CheckerResult<Value>) -> CheckerResult<CheckerResponse> {
    r.map(CheckerResponse::Json)
}

fn dispatch(h: &dyn CheckerHost, method: &str, p: &Params) -> CheckerResult<CheckerResponse> {
    match method {
        "typeToTypeNode" => return ty::type_to_type_node(h, p),
        "signatureToSignatureDeclaration" => return ty::signature_to_signature_declaration(h, p),
        _ => {}
    }
    json(match method {
        "getSymbolAtPosition" => sym::get_symbol_at_position(h, p),
        "getSymbolsAtPositions" => sym::get_symbols_at_positions(h, p),
        "getSymbolAtLocation" => sym::get_symbol_at_location(h, p),
        "getSymbolsAtLocations" => sym::get_symbols_at_locations(h, p),
        "getSymbolOfSourceFile" => sym::get_symbol_of_source_file(h, p),
        "getSymbolsOfSourceFiles" => sym::get_symbols_of_source_files(h, p),
        "getTypeOfSymbol" => sym::get_type_of_symbol(h, p, SymbolTypeQuery::TypeOf),
        "getTypesOfSymbols" => sym::get_types_of_symbols(h, p),
        "getDeclaredTypeOfSymbol" => sym::get_type_of_symbol(h, p, SymbolTypeQuery::DeclaredType),
        "getNonMissingTypeOfSymbol" => {
            sym::get_type_of_symbol(h, p, SymbolTypeQuery::NonMissingType)
        }
        "resolveName" => sym::resolve_name(h, p),
        "getSymbolsInScope" => sym::get_symbols_in_scope(h, p),
        "getSignaturesOfType" => ty::get_signatures_of_type(h, p),
        "getResolvedSignature" => ty::get_resolved_signature(h, p),
        "getTypeAtLocation" => ty::get_type_at_location(h, p),
        "getTypeAtLocations" => ty::get_type_at_locations(h, p),
        "getTypeAtPosition" => ty::get_type_at_position(h, p),
        "getTypesAtPositions" => ty::get_types_at_positions(h, p),

        "getParentOfSymbol" => sym::symbol_property(h, p, SymbolProperty::Parent),
        "getMembersOfSymbol" => sym::symbol_table_property(h, p, SymbolTableProperty::Members),
        "getExportsOfSymbol" => sym::symbol_table_property(h, p, SymbolTableProperty::Exports),
        "getExportSymbolOfSymbol" => sym::symbol_property(h, p, SymbolProperty::ExportSymbol),

        "getSymbolOfType" => ty::resolve_type_symbol_property(h, p, TypeSymbolProperty::Symbol),
        "getTargetOfType" => ty::resolve_type_property(h, p, TypeProperty::Target),
        "getFreshTypeOfType" => ty::resolve_type_property(h, p, TypeProperty::FreshType),
        "getRegularTypeOfType" => ty::resolve_type_property(h, p, TypeProperty::RegularType),
        "getTypesOfType" => ty::resolve_type_array_property(h, p, TypeArrayProperty::Types),
        "getTypeParametersOfType" => {
            ty::resolve_type_array_property(h, p, TypeArrayProperty::TypeParameters)
        }
        "getOuterTypeParametersOfType" => {
            ty::resolve_type_array_property(h, p, TypeArrayProperty::OuterTypeParameters)
        }
        "getLocalTypeParametersOfType" => {
            ty::resolve_type_array_property(h, p, TypeArrayProperty::LocalTypeParameters)
        }
        "getThisTypeOfType" => ty::resolve_type_property(h, p, TypeProperty::ThisType),
        "getAliasTypeArgumentsOfType" => {
            ty::resolve_type_array_property(h, p, TypeArrayProperty::AliasTypeArguments)
        }
        "getAliasSymbolOfType" => {
            ty::resolve_type_symbol_property(h, p, TypeSymbolProperty::AliasSymbol)
        }
        "getObjectTypeOfType" => ty::resolve_type_property(h, p, TypeProperty::ObjectType),
        "getIndexTypeOfType" => ty::resolve_type_property(h, p, TypeProperty::IndexType),
        "getCheckTypeOfType" => ty::resolve_type_property(h, p, TypeProperty::CheckType),
        "getExtendsTypeOfType" => ty::resolve_type_property(h, p, TypeProperty::ExtendsType),
        "getBaseTypeOfType" => ty::resolve_type_property(h, p, TypeProperty::BaseType),
        "getConstraintOfType" => ty::resolve_type_property(h, p, TypeProperty::SubstConstraint),
        "getTypeParameterOfMappedType" => {
            ty::resolve_type_property(h, p, TypeProperty::MappedTypeParameter)
        }
        "getConstraintTypeOfMappedType" => {
            ty::resolve_type_property(h, p, TypeProperty::MappedConstraintType)
        }
        "getNameTypeOfMappedType" => ty::resolve_type_property(h, p, TypeProperty::MappedNameType),
        "getTemplateTypeOfMappedType" => {
            ty::resolve_type_property(h, p, TypeProperty::MappedTemplateType)
        }

        "getTypeParametersOfSignature" => ty::get_type_parameters_of_signature(h, p),
        "getParametersOfSignature" => ty::signature_property(h, p, SignatureProperty::Parameters),
        "getThisParameterOfSignature" => {
            ty::signature_property(h, p, SignatureProperty::ThisParameter)
        }
        "getTargetOfSignature" => ty::signature_property(h, p, SignatureProperty::Target),

        "getContextualType" => ty::node_type_op(h, p, NodeTypeOp::Contextual),
        "getContextualTypeForArgument" => ty::get_contextual_type_for_argument(h, p),
        "getAwaitedType" => ty::type_op(h, p, "type", TypeOp::Awaited),
        "getBaseTypeOfLiteralType" => ty::type_op(h, p, "type", TypeOp::BaseTypeOfLiteral),
        "getNonNullableType" => ty::type_op(h, p, "objectId", TypeOp::NonNullable),
        "getTypeFromTypeNode" => ty::node_type_op(h, p, NodeTypeOp::FromTypeNode),
        "getWidenedType" => ty::type_op(h, p, "type", TypeOp::Widened),
        "getParameterType" => ty::signature_parameter_at(h, p, ParameterAt::ParameterType),
        "getTypeParameterAtPosition" => {
            ty::signature_parameter_at(h, p, ParameterAt::TypeParameter)
        }
        "isArrayLikeType" => ty::type_predicate_op(h, p, TypePredicateOp::IsArrayLike),
        "isTypeAssignableTo" => ty::is_type_assignable_to(h, p),
        "getShorthandAssignmentValueSymbol" => sym::get_shorthand_assignment_value_symbol(h, p),
        "getTypeOfSymbolAtLocation" => ty::get_type_of_symbol_at_location(h, p),
        "typeToString" => ty::type_to_string(h, p),
        "isContextSensitive" => ty::is_context_sensitive(h, p),
        "getReturnTypeOfSignature" => {
            ty::signature_type_op(h, p, "objectId", SignatureTypeOp::ReturnType)
        }
        "getRestTypeOfSignature" => {
            ty::signature_type_op(h, p, "signature", SignatureTypeOp::RestType)
        }
        "getTypePredicateOfSignature" => ty::get_type_predicate_of_signature(h, p),
        "getBaseTypes" => ty::type_list_op(h, p, TypeListOp::BaseTypes),
        "getPropertiesOfType" => ty::get_properties_of_type(h, p),
        "getApparentPropertiesOfType" => ty::get_apparent_properties_of_type(h, p),
        "getApparentType" => ty::type_op(h, p, "objectId", TypeOp::Apparent),
        "getReducedType" => ty::type_op(h, p, "objectId", TypeOp::Reduced),
        "getPropertyOfType" => ty::get_property_of_type(h, p),
        "getTypeOfPropertyOfType" => ty::get_type_of_property_of_type(h, p),
        "getIndexInfoOfType" => ty::get_index_info_of_type(h, p),
        "getIndexInfosOfType" => ty::get_index_infos_of_type(h, p),
        "getConstraintOfTypeParameter" => {
            ty::type_op(h, p, "objectId", TypeOp::ConstraintOfTypeParameter)
        }
        "getDefaultFromTypeParameter" => {
            ty::type_op(h, p, "objectId", TypeOp::DefaultFromTypeParameter)
        }
        "getBaseConstraintOfType" => ty::type_op(h, p, "type", TypeOp::BaseConstraint),
        "getTypeArguments" => ty::type_list_op(h, p, TypeListOp::TypeArguments),
        "getTrueTypeOfConditionalType" => {
            ty::type_op(h, p, "objectId", TypeOp::TrueTypeOfConditional)
        }
        "getFalseTypeOfConditionalType" => {
            ty::type_op(h, p, "objectId", TypeOp::FalseTypeOfConditional)
        }
        "getConstantValue" => ty::get_constant_value(h, p),
        "getSignatureFromDeclaration" => ty::get_signature_from_declaration(h, p),
        "getExportSpecifierLocalTargetSymbol" => {
            sym::get_export_specifier_local_target_symbol(h, p)
        }
        "getAliasedSymbol" => sym::alias_query(h, p, AliasQuery::Aliased),
        "getImmediateAliasedSymbol" => sym::alias_query(h, p, AliasQuery::ImmediateAliased),
        "getTargetSymbol" => sym::alias_query(h, p, AliasQuery::Target),
        "getMergedSymbol" => sym::alias_query(h, p, AliasQuery::Merged),
        "getSymbolOfNode" => sym::get_symbol_of_node(h, p, false),
        "getSymbolOfDeclarationForChecker" => sym::get_symbol_of_node(h, p, true),
        "getParentOfSymbolForChecker" => sym::alias_query(h, p, AliasQuery::Parent),
        "getExportSymbolOfSymbolForChecker" => sym::alias_query(h, p, AliasQuery::ExportSymbol),
        "getFullyQualifiedName" => sym::get_fully_qualified_name(h, p),
        "getExportsOfModule" => sym::get_exports_of_module(h, p),
        "getMemberInModuleExports" => sym::get_member_in_module_exports(h, p),
        "getJsDocTags" => sym::get_jsdoc_tags(h, p),
        "getDocumentationComment" => sym::get_documentation_comment(h, p),
        "isArrayType" => ty::type_predicate_op(h, p, TypePredicateOp::IsArray),
        "isReadonlySymbol" => sym::is_readonly_symbol(h, p),
        "getReferencesToSymbolInFile" => sym::get_references_to_symbol_in_file(h, p),
        "getReferencedSymbolsForNode" => ls::get_referenced_symbols_for_node(h, p),
        "getSignatureUsages" => ls::get_signature_usages(h, p),
        "getCompletionsAtPosition" => ls::get_completions_at_position(h, p),
        "getImportAdderEdits" => ls::get_import_adder_edits(h, p),

        "getAnyType" => ty::get_intrinsic_type(h, p, Intrinsic::Any),
        "getStringType" => ty::get_intrinsic_type(h, p, Intrinsic::String),
        "getNumberType" => ty::get_intrinsic_type(h, p, Intrinsic::Number),
        "getBooleanType" => ty::get_intrinsic_type(h, p, Intrinsic::Boolean),
        "getVoidType" => ty::get_intrinsic_type(h, p, Intrinsic::Void),
        "getUndefinedType" => ty::get_intrinsic_type(h, p, Intrinsic::Undefined),
        "getNullType" => ty::get_intrinsic_type(h, p, Intrinsic::Null),
        "getNeverType" => ty::get_intrinsic_type(h, p, Intrinsic::Never),
        "getUnknownType" => ty::get_intrinsic_type(h, p, Intrinsic::Unknown),
        "getBigIntType" => ty::get_intrinsic_type(h, p, Intrinsic::BigInt),
        "getESSymbolType" => ty::get_intrinsic_type(h, p, Intrinsic::ESSymbol),
        "getNonPrimitiveType" => ty::get_intrinsic_type(h, p, Intrinsic::NonPrimitive),
        "getWellKnownSymbols" => ty::get_well_known_symbols(h, p),
        "getWellKnownSignatures" => ty::get_well_known_signatures(h, p),
        _ => unreachable!("is_checker_method and dispatch disagree on {method}"),
    })
}

/// Every method this lane dispatches.
const CHECKER_METHODS: &[&str] = &[
    "getSymbolAtPosition",
    "getSymbolsAtPositions",
    "getSymbolAtLocation",
    "getSymbolsAtLocations",
    "getSymbolOfSourceFile",
    "getSymbolsOfSourceFiles",
    "getTypeOfSymbol",
    "getTypesOfSymbols",
    "getDeclaredTypeOfSymbol",
    "getNonMissingTypeOfSymbol",
    "resolveName",
    "getSymbolsInScope",
    "getSignaturesOfType",
    "getResolvedSignature",
    "getTypeAtLocation",
    "getTypeAtLocations",
    "getTypeAtPosition",
    "getTypesAtPositions",
    "getParentOfSymbol",
    "getMembersOfSymbol",
    "getExportsOfSymbol",
    "getExportSymbolOfSymbol",
    "getSymbolOfType",
    "getTargetOfType",
    "getFreshTypeOfType",
    "getRegularTypeOfType",
    "getTypesOfType",
    "getTypeParametersOfType",
    "getOuterTypeParametersOfType",
    "getLocalTypeParametersOfType",
    "getThisTypeOfType",
    "getAliasTypeArgumentsOfType",
    "getAliasSymbolOfType",
    "getObjectTypeOfType",
    "getIndexTypeOfType",
    "getCheckTypeOfType",
    "getExtendsTypeOfType",
    "getBaseTypeOfType",
    "getConstraintOfType",
    "getTypeParameterOfMappedType",
    "getConstraintTypeOfMappedType",
    "getNameTypeOfMappedType",
    "getTemplateTypeOfMappedType",
    "getTypeParametersOfSignature",
    "getParametersOfSignature",
    "getThisParameterOfSignature",
    "getTargetOfSignature",
    "getContextualType",
    "getContextualTypeForArgument",
    "getAwaitedType",
    "getBaseTypeOfLiteralType",
    "getNonNullableType",
    "getTypeFromTypeNode",
    "getWidenedType",
    "getParameterType",
    "getTypeParameterAtPosition",
    "isArrayLikeType",
    "isTypeAssignableTo",
    "getShorthandAssignmentValueSymbol",
    "getTypeOfSymbolAtLocation",
    "typeToTypeNode",
    "signatureToSignatureDeclaration",
    "typeToString",
    "isContextSensitive",
    "getReturnTypeOfSignature",
    "getRestTypeOfSignature",
    "getTypePredicateOfSignature",
    "getBaseTypes",
    "getPropertiesOfType",
    "getApparentPropertiesOfType",
    "getApparentType",
    "getReducedType",
    "getPropertyOfType",
    "getTypeOfPropertyOfType",
    "getIndexInfoOfType",
    "getIndexInfosOfType",
    "getConstraintOfTypeParameter",
    "getDefaultFromTypeParameter",
    "getBaseConstraintOfType",
    "getTypeArguments",
    "getTrueTypeOfConditionalType",
    "getFalseTypeOfConditionalType",
    "getConstantValue",
    "getSignatureFromDeclaration",
    "getExportSpecifierLocalTargetSymbol",
    "getAliasedSymbol",
    "getImmediateAliasedSymbol",
    "getTargetSymbol",
    "getMergedSymbol",
    "getSymbolOfNode",
    "getSymbolOfDeclarationForChecker",
    "getParentOfSymbolForChecker",
    "getExportSymbolOfSymbolForChecker",
    "getFullyQualifiedName",
    "getExportsOfModule",
    "getMemberInModuleExports",
    "getJsDocTags",
    "getDocumentationComment",
    "isArrayType",
    "isReadonlySymbol",
    "getReferencesToSymbolInFile",
    "getReferencedSymbolsForNode",
    "getSignatureUsages",
    "getCompletionsAtPosition",
    "getImportAdderEdits",
    "getAnyType",
    "getStringType",
    "getNumberType",
    "getBooleanType",
    "getVoidType",
    "getUndefinedType",
    "getNullType",
    "getNeverType",
    "getUnknownType",
    "getBigIntType",
    "getESSymbolType",
    "getNonPrimitiveType",
    "getWellKnownSymbols",
    "getWellKnownSignatures",
];

pub fn is_checker_method(method: &str) -> bool {
    // Every request asks: hash the method instead of comparing it with each name in turn.
    static METHODS: LazyLock<FxHashSet<&'static str>> =
        LazyLock::new(|| CHECKER_METHODS.iter().copied().collect());
    METHODS.contains(method)
}
