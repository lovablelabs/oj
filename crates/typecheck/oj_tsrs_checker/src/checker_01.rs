use crate::*;
use rustc_hash::FxHashMap;
use tsrs_ast as ast;
use tsrs_ast::*;
use tsrs_diagnostics as diagnostics;
use tsrs_diagnostics::Message;

// Non-function declarations in checker.go:1-2172 (hand-ported by checker-foundation):
//   type CheckMode (checker.go:36)
//   const (checker.go:38): CheckModeNormal, CheckModeContextual, CheckModeInferential,
//     CheckModeSkipContextSensitive, CheckModeSkipGenericFunctions, CheckModeIsForSignatureHelp,
//     CheckModeRestBindingElement, CheckModeTypeOnly, CheckModeForceTuple
//   type TypeSystemEntity (checker.go:52)
//   type TypeSystemPropertyName (checker.go:54)
//   const (checker.go:56): TypeSystemPropertyNameType, TypeSystemPropertyNameResolvedBaseConstructorType,
//     TypeSystemPropertyNameDeclaredType, TypeSystemPropertyNameResolvedReturnType,
//     TypeSystemPropertyNameResolvedBaseConstraint, TypeSystemPropertyNameResolvedTypeArguments,
//     TypeSystemPropertyNameResolvedBaseTypes, TypeSystemPropertyNameWriteType,
//     TypeSystemPropertyNameInitializerIsUndefined, TypeSystemPropertyNameAliasTarget
//   type TypeResolution (checker.go:69)
//   type ContextualInfo (checker.go:77)
//   type InferenceContextInfo (checker.go:85)
//   type WideningKind (checker.go:92)
//   const (checker.go:94): WideningKindNormal, WideningKindFunctionReturn, WideningKindGeneratorNext,
//     WideningKindGeneratorYield
//   type EnumLiteralKey (checker.go:103)
//   type EnumRelationKey (checker.go:110)
//   type CachedTypeKind (checker.go:117)
//   const (checker.go:119): CachedTypeKindLiteralUnionBaseType, CachedTypeKindIndexType,
//     CachedTypeKindStringIndexType, CachedTypeKindEquivalentBaseType, CachedTypeKindApparentType,
//     CachedTypeKindAwaitedType, CachedTypeKindEvolvingArrayType, CachedTypeKindArrayLiteralType,
//     CachedTypeKindPermissiveInstantiation, CachedTypeKindRestrictiveInstantiation,
//     CachedTypeKindRestrictiveTypeParameter, CachedTypeKindIndexedAccessForReading,
//     CachedTypeKindIndexedAccessForWriting, CachedTypeKindWidened, CachedTypeKindRegularObjectLiteral,
//     CachedTypeKindPromisedTypeOfPromise, CachedTypeKindDefaultOnlyType, CachedTypeKindSyntheticType,
//     CachedTypeKindDecoratorContext, CachedTypeKindDecoratorContextStatic,
//     CachedTypeKindDecoratorContextPrivate, CachedTypeKindDecoratorContextPrivateStatic
//   type CachedTypeKey (checker.go:146)
//   type NarrowedTypeKey (checker.go:153)
//   type UnionOfUnionKey (checker.go:162)
//   type CachedSignatureKey (checker.go:171)
//   var (checker.go:176): SignatureKeyErased, SignatureKeyCanonical, SignatureKeyBase, SignatureKeyInner,
//     SignatureKeyOuter
//   type StringMappingKey (checker.go:186)
//   type AssignmentReducedKey (checker.go:193)
//   type DiscriminatedContextualTypeKey (checker.go:200)
//   type InstantiationExpressionKey (checker.go:207)
//   type SubstitutionTypeKey (checker.go:214)
//   type ReverseMappedTypeKey (checker.go:221)
//   type IterationTypesKey (checker.go:229)
//   type PropertiesTypesKey (checker.go:236)
//   type NonExistentPropertyKey (checker.go:244)
//   type FlowLoopKey (checker.go:252)
//   type FlowLoopInfo (checker.go:257)
//   type InferenceFlags (checker.go:264)
//   const (checker.go:266): InferenceFlagsNone, InferenceFlagsNoDefault, InferenceFlagsAnyDefault,
//     InferenceFlagsSkippedGenericFunction, InferenceFlagsNoConstraintChecks
//   type InferenceContext (checker.go:276)
//   type InferenceInfo (checker.go:289)
//   type InferencePriority (checker.go:300)
//   const (checker.go:302): InferencePriorityNone, InferencePriorityNakedTypeVariable,
//     InferencePrioritySpeculativeTuple, InferencePrioritySubstituteSource,
//     InferencePriorityHomomorphicMappedType, InferencePriorityPartialHomomorphicMappedType,
//     InferencePriorityMappedTypeConstraint, InferencePriorityContravariantConditional,
//     InferencePriorityReturnType, InferencePriorityLiteralKeyof, InferencePriorityNoConstraints,
//     InferencePriorityAlwaysStrict, InferencePriorityMaxValue, InferencePriorityCircularity,
//     InferencePriorityPriorityImpliesCombination
//   type IntraExpressionInferenceSite (checker.go:321)
//   type DeclarationMeaning (checker.go:326)
//   const (checker.go:328): DeclarationMeaningGetAccessor, DeclarationMeaningSetAccessor,
//     DeclarationMeaningPropertyAssignment, DeclarationMeaningMethod, DeclarationMeaningPrivateStatic,
//     DeclarationMeaningGetOrSetAccessor, DeclarationMeaningPropertyAssignmentOrMethod
//   type DeclarationSpaces (checker.go:338)
//   const (checker.go:340): DeclarationSpacesNone, DeclarationSpacesExportValue, DeclarationSpacesExportType,
//     DeclarationSpacesExportNamespace
//   type IntrinsicTypeKind (checker.go:349)
//   const (checker.go:351): IntrinsicTypeKindUnknown, IntrinsicTypeKindUppercase, IntrinsicTypeKindLowercase,
//     IntrinsicTypeKindCapitalize, IntrinsicTypeKindUncapitalize, IntrinsicTypeKindNoInfer
//   var intrinsicTypeKinds (checker.go:360)
//   type MappedTypeModifiers (checker.go:368)
//   const (checker.go:370): MappedTypeModifiersIncludeReadonly, MappedTypeModifiersExcludeReadonly,
//     MappedTypeModifiersIncludeOptional, MappedTypeModifiersExcludeOptional
//   type MappedTypeNameTypeKind (checker.go:377)
//   const (checker.go:379): MappedTypeNameTypeKindNone, MappedTypeNameTypeKindFiltering,
//     MappedTypeNameTypeKindRemapping
//   type ReferenceHint (checker.go:385)
//   const (checker.go:387): ReferenceHintUnspecified, ReferenceHintIdentifier, ReferenceHintProperty,
//     ReferenceHintExportAssignment, ReferenceHintJsx, ReferenceHintExportImportEquals,
//     ReferenceHintExportSpecifier, ReferenceHintDecorator
//   type TypeFacts (checker.go:398)
//   const (checker.go:400): TypeFactsNone, TypeFactsTypeofEQString, TypeFactsTypeofEQNumber,
//     TypeFactsTypeofEQBigInt, TypeFactsTypeofEQBoolean, TypeFactsTypeofEQSymbol, TypeFactsTypeofEQObject,
//     TypeFactsTypeofEQFunction, TypeFactsTypeofEQHostObject, TypeFactsTypeofNEString,
//     TypeFactsTypeofNENumber, TypeFactsTypeofNEBigInt, TypeFactsTypeofNEBoolean, TypeFactsTypeofNESymbol,
//     TypeFactsTypeofNEObject, TypeFactsTypeofNEFunction, TypeFactsTypeofNEHostObject, TypeFactsEQUndefined,
//     TypeFactsEQNull, TypeFactsEQUndefinedOrNull, TypeFactsNEUndefined, TypeFactsNENull,
//     TypeFactsNEUndefinedOrNull, TypeFactsTruthy, TypeFactsFalsy, TypeFactsIsUndefined, TypeFactsIsNull,
//     TypeFactsIsUndefinedOrNull, TypeFactsAll, TypeFactsBaseStringStrictFacts, TypeFactsBaseStringFacts,
//     TypeFactsStringStrictFacts, TypeFactsStringFacts, TypeFactsEmptyStringStrictFacts,
//     TypeFactsEmptyStringFacts, TypeFactsNonEmptyStringStrictFacts, TypeFactsNonEmptyStringFacts,
//     TypeFactsBaseNumberStrictFacts, TypeFactsBaseNumberFacts, TypeFactsNumberStrictFacts,
//     TypeFactsNumberFacts, TypeFactsZeroNumberStrictFacts, TypeFactsZeroNumberFacts,
//     TypeFactsNonZeroNumberStrictFacts, TypeFactsNonZeroNumberFacts, TypeFactsBaseBigIntStrictFacts,
//     TypeFactsBaseBigIntFacts, TypeFactsBigIntStrictFacts, TypeFactsBigIntFacts,
//     TypeFactsZeroBigIntStrictFacts, TypeFactsZeroBigIntFacts, TypeFactsNonZeroBigIntStrictFacts,
//     TypeFactsNonZeroBigIntFacts, TypeFactsBaseBooleanStrictFacts, TypeFactsBaseBooleanFacts,
//     TypeFactsBooleanStrictFacts, TypeFactsBooleanFacts, TypeFactsFalseStrictFacts, TypeFactsFalseFacts,
//     TypeFactsTrueStrictFacts, TypeFactsTrueFacts, TypeFactsSymbolStrictFacts, TypeFactsSymbolFacts,
//     TypeFactsObjectStrictFacts, TypeFactsObjectFacts, TypeFactsFunctionStrictFacts, TypeFactsFunctionFacts,
//     TypeFactsVoidFacts, TypeFactsUndefinedFacts, TypeFactsNullFacts, TypeFactsEmptyObjectStrictFacts,
//     TypeFactsEmptyObjectFacts, TypeFactsUnknownFacts, TypeFactsAllTypeofNE, TypeFactsOrFactsMask,
//     TypeFactsAndFactsMask
//   type IterationUse (checker.go:483)
//   const (checker.go:485): IterationUseAllowsSyncIterablesFlag, IterationUseAllowsAsyncIterablesFlag,
//     IterationUseAllowsStringInputFlag, IterationUseForOfFlag, IterationUseYieldStarFlag,
//     IterationUseSpreadFlag, IterationUseDestructuringFlag, IterationUsePossiblyOutOfBounds,
//     IterationUseElement, IterationUseSpread, IterationUseDestructuring, IterationUseForOf,
//     IterationUseForAwaitOf, IterationUseYieldStar, IterationUseAsyncYieldStar,
//     IterationUseGeneratorReturnType, IterationUseAsyncGeneratorReturnType, IterationUseCacheFlags
//   type IterationTypes (checker.go:507)
//   type IterationTypeKind (checker.go:513)
//   const (checker.go:515): IterationTypeKindYield, IterationTypeKindReturn, IterationTypeKindNext
//   type IterationTypesResolver (checker.go:521)
//   type WideningContext (checker.go:537)
//   type VarianceStackEntry (checker.go:546)
//   const maxSerializationLevel (checker.go:551)
//   type Program (checker.go:553)
//   type Host (checker.go:577)
//   var nextCheckerID (checker.go:583)
//   type Checker (checker.go:585)
//   func NewChecker checker.go:911 (not generated)
//   var primitiveTypeAliasSuggestions (checker.go:1786)

// checker.go:1128
pub(crate) fn create_file_index_map(files: &[P<SourceFile>]) -> FxHashMap<P<SourceFile>, usize> {
    let mut result = FxHashMap::with_capacity_and_hasher(files.len(), rustc_hash::FxBuildHasher);
    result.extend(files.iter().enumerate().map(|(i, file)| (*file, i)));
    result
}

// checker.go:1136
pub(crate) fn count_global_symbols(files: &[P<SourceFile>]) -> usize {
    let mut count = 0;
    count = files.iter().fold(count, |mut count, file| {
        if !ast::is_external_or_common_js_module(*file) {
            count += file.locals().map_or(0, |locals| locals.len());
        }

        count
    });
    count
}

impl Checker {
    // checker.go:1146
    pub(crate) fn report_unreliable_worker(&mut self, t: P<Type>) -> P<Type> {
        if t == self.marker_super_type || t == self.marker_sub_type || t == self.marker_other_type {
            self.reliability_flags |= RelationComparisonResult::ReportsUnreliable;
        }
        t
    }

    // checker.go:1153
    pub(crate) fn report_unmeasurable_worker(&mut self, t: P<Type>) -> P<Type> {
        if t == self.marker_super_type || t == self.marker_sub_type || t == self.marker_other_type {
            self.reliability_flags |= RelationComparisonResult::ReportsUnmeasurable;
        }
        t
    }

    // checker.go:1161-1192: getGlobalTypeResolver, getGlobalTypeAliasResolver, getGlobalValueSymbolResolver,
    // getGlobalTypeSymbolResolver and getGlobalTypesResolver build the lazily resolved function-valued fields that
    // NewChecker assigns; in Rust those fields are the cached `get_global_*` methods in checker.rs.

    // checker.go:1195
    pub(crate) fn get_global_type_alias_symbol(
        &mut self,
        name: &str,
        arity: usize,
        report_errors: bool,
    ) -> Option<P<Symbol>> {
        let symbol = self.get_global_symbol(
            name,
            SymbolFlags::TypeAlias,
            if report_errors { Some(&diagnostics::CANNOT_FIND_GLOBAL_TYPE_0) } else { None },
        )?;
        // Resolve the declared type of the symbol. This resolves type parameters for the type alias so that we can check arity.
        self.get_declared_type_of_symbol(symbol);
        if self.type_alias_links.get(symbol).type_parameters.get().len() != arity {
            if report_errors {
                let decl = symbol
                    .declarations()
                    .iter()
                    .copied()
                    .find(|d| ast::is_type_alias_declaration(*d));
                self.error(
                    decl,
                    &diagnostics::GLOBAL_TYPE_0_MUST_HAVE_1_TYPE_PARAMETER_S,
                    &[&ast::symbol_name(symbol), &arity],
                );
            }
            return None;
        }
        Some(symbol)
    }

    // checker.go:1212
    pub fn get_type_alias_type_parameters(&mut self, symbol: P<Symbol>) -> Vec<P<Type>> {
        assert!(
            symbol.flags().intersects(SymbolFlags::TypeAlias),
            "Attempted to fetch type alias parameters for non-type-alias symbol"
        );
        self.get_declared_type_of_symbol(symbol);
        self.type_alias_links.get(symbol).type_parameters.get().to_vec()
    }

    // checker.go:1220
    pub(crate) fn get_global_type(
        &mut self,
        name: &str,
        arity: usize,
        report_errors: bool,
    ) -> P<Type> {
        let symbol = self.get_global_symbol(
            name,
            SymbolFlags::Type,
            if report_errors { Some(&diagnostics::CANNOT_FIND_GLOBAL_TYPE_0) } else { None },
        );
        if let Some(symbol) = symbol {
            if symbol.flags().intersects(SymbolFlags::Class | SymbolFlags::Interface) {
                let t = self.get_declared_type_of_symbol(symbol);
                if t.as_interface_type().type_parameters().len() == arity {
                    return t;
                }
                if report_errors {
                    self.error(
                        get_global_type_declaration(symbol),
                        &diagnostics::GLOBAL_TYPE_0_MUST_HAVE_1_TYPE_PARAMETER_S,
                        &[&ast::symbol_name(symbol), &arity],
                    );
                }
            } else if report_errors {
                self.error(
                    get_global_type_declaration(symbol),
                    &diagnostics::GLOBAL_TYPE_0_MUST_BE_A_CLASS_OR_INTERFACE_TYPE,
                    &[&ast::symbol_name(symbol)],
                );
            }
        }
        if arity != 0 {
            return self.empty_generic_type;
        }
        self.empty_object_type
    }
}

// checker.go:1241
fn get_global_type_declaration(symbol: P<Symbol>) -> Option<P<Node>> {
    if let Some(result) = symbol.declarations().iter().find_map(|declaration| {
        match declaration.kind() {
            Kind::ClassDeclaration
            | Kind::InterfaceDeclaration
            | Kind::EnumDeclaration
            | Kind::TypeAliasDeclaration => {
                return Some(Some(*declaration));
            }
            _ => {}
        }

        None
    }) {
        return result;
    }
    None
}

impl Checker {
    // checker.go:1251
    pub(crate) fn get_global_symbol(
        &mut self,
        name: &str,
        meaning: SymbolFlags,
        diagnostic: Option<&'static Message>,
    ) -> Option<P<Symbol>> {
        // Don't track references for global symbols anyway, so value if `isReference` is arbitrary
        self.resolve_name(
            None, name, meaning, diagnostic, false, /*isUse*/
            false, /*excludeGlobals*/
        )
    }

    // checker.go:1256
    #[expect(
        clippy::unused_self,
        reason = "The Go initialization or tracing hook is retained in the same method call sequence."
    )]
    pub(crate) fn initialize_closures(&self) {
        // Go assigns the function-valued fields isPrimitiveOrObjectOrEmptyType, containsMissingType,
        // couldContainTypeVariables, isStringIndexSignatureOnlyType, markNodeAssignments and
        // compareTypesAssignable here. In Rust they are the methods of the same names in checker.rs.
    }

    // checker.go:1269
    pub(crate) fn initialize_iteration_resolvers(&mut self) {
        // The resolvers' function-valued fields are methods of IterationTypesResolver (checker.rs) that dispatch on
        // `is_async` to the matching lazily resolved globals.
        self.sync_iteration_types_resolver = P::new(IterationTypesResolver {
            is_async: false,
            iterator_symbol_name: "iterator",
            must_have_a_next_method_diagnostic: &diagnostics::AN_ITERATOR_MUST_HAVE_A_NEXT_METHOD,
            must_be_a_method_diagnostic: &diagnostics::THE_0_PROPERTY_OF_AN_ITERATOR_MUST_BE_A_METHOD,
            must_have_a_value_diagnostic: &diagnostics::THE_TYPE_RETURNED_BY_THE_0_METHOD_OF_AN_ITERATOR_MUST_HAVE_A_VALUE_PROPERTY,
        });
        self.async_iteration_types_resolver = P::new(IterationTypesResolver {
            is_async: true,
            iterator_symbol_name: "asyncIterator",
            must_have_a_next_method_diagnostic: &diagnostics::AN_ASYNC_ITERATOR_MUST_HAVE_A_NEXT_METHOD,
            must_be_a_method_diagnostic: &diagnostics::THE_0_PROPERTY_OF_AN_ASYNC_ITERATOR_MUST_BE_A_METHOD,
            must_have_a_value_diagnostic: &diagnostics::THE_TYPE_RETURNED_BY_THE_0_METHOD_OF_AN_ASYNC_ITERATOR_MUST_BE_A_PROMISE_FOR_A_TYPE_WITH_A_VALUE_PROPERTY,
        });
    }

    // checker.go:1306
    pub(crate) fn initialize_checker(&mut self) {
        // Initialize global symbol table
        let mut ambient_module_symbols: Vec<P<Symbol>> = Vec::new();
        let mut augmentations: Vec<&'static [P<Node>]> = Vec::with_capacity(self.files.len());
        let files = self.files;
        for &file in files {
            if !ast::is_external_or_common_js_module(file) {
                // It is an error for a non-external-module (i.e. script) to declare its own `globalThis`.
                if let Some(file_global_this_symbol) =
                    file.locals().and_then(|locals| locals.lookup("globalThis"))
                {
                    let declarations = file_global_this_symbol.declarations();
                    for &d in declarations {
                        self.add_diagnostic(new_diagnostic_for_node(
                            Some(d),
                            Some(&diagnostics::DECLARATION_NAME_CONFLICTS_WITH_BUILT_IN_GLOBAL_IDENTIFIER_0),
                            &[&"globalThis"],
                        ));
                    }
                }
                if let Some(locals) = file.locals() {
                    for symbol in locals.values() {
                        // We defer merging of global ambient module declarations since they may require other global symbols
                        // and types to be resolved. See https://github.com/microsoft/TypeScript/tsc/issues/2953.
                        if symbol.flags().intersects(SymbolFlags::Module)
                            && ast::is_ambient_module_symbol_name(symbol.name())
                        {
                            ambient_module_symbols.push(symbol);
                        } else {
                            self.merge_global_symbol(symbol);
                        }
                    }
                }
            }
            self.pattern_ambient_modules.extend_from_slice(file.pattern_ambient_modules());
            augmentations.push(file.module_augmentations());
            if file.symbol().is_some() {
                // Merge in UMD exports with first-in-wins semantics (see #9771)
                if let Some(global_exports) = file.global_exports() {
                    for (name, symbol) in global_exports.entries() {
                        if !self.globals.has(name) {
                            self.globals.set(name, symbol);
                        }
                    }
                }
            }
        }
        // We do global augmentations separately from module augmentations (and before creating global types) because they
        //  1. Affect global types. We won't have the correct global types until global augmentations are merged. Also,
        //  2. Module augmentation instantiation requires creating the type of a module, which, in turn, can require
        //       checking for an export or property on the module (if export=) which, in turn, can fall back to the
        //       apparent type of the module - either globalObjectType or globalFunctionType - which wouldn't exist if we
        //       did module augmentations prior to finalizing the global types.
        for list in &augmentations {
            for &augmentation in *list {
                // Merge 'global' module augmentations. This needs to be done after global symbol table is initialized to
                // make sure that all ambient modules are indexed
                if ast::is_global_scope_augmentation(augmentation.parent().unwrap()) {
                    self.merge_module_augmentation(augmentation);
                }
            }
        }
        self.add_undefined_to_globals_or_error_on_redeclaration();
        self.value_symbol_links
            .get(self.undefined_symbol)
            .resolved_type
            .set(Some(self.undefined_widening_type));
        let links = self.value_symbol_links.get(self.arguments_symbol);
        links.resolved_type.set(Some(self.get_global_type(
            "IArguments",
            0,    /*arity*/
            true, /*reportErrors*/
        )));
        self.value_symbol_links.get(self.unknown_symbol).resolved_type.set(Some(self.error_type));
        let links = self.value_symbol_links.get(self.global_this_symbol);
        links
            .resolved_type
            .set(Some(self.new_object_type(ObjectFlags::Anonymous, Some(self.global_this_symbol))));
        // Initialize special types
        self.global_array_type =
            self.get_global_type("Array", 1 /*arity*/, true /*reportErrors*/);
        self.global_object_type =
            self.get_global_type("Object", 0 /*arity*/, true /*reportErrors*/);
        self.global_function_type =
            self.get_global_type("Function", 0 /*arity*/, true /*reportErrors*/);
        self.global_callable_function_type =
            self.get_global_strict_function_type("CallableFunction");
        self.global_newable_function_type = self.get_global_strict_function_type("NewableFunction");
        self.global_string_type =
            self.get_global_type("String", 0 /*arity*/, true /*reportErrors*/);
        self.global_number_type =
            self.get_global_type("Number", 0 /*arity*/, true /*reportErrors*/);
        self.global_boolean_type =
            self.get_global_type("Boolean", 0 /*arity*/, true /*reportErrors*/);
        self.global_reg_exp_type =
            self.get_global_type("RegExp", 0 /*arity*/, true /*reportErrors*/);
        self.any_array_type = self.create_array_type(self.any_type);
        self.auto_array_type = self.create_array_type(self.auto_type);
        if self.auto_array_type == self.empty_object_type {
            // autoArrayType is used as a marker, so even if global Array type is not defined, it needs to be a unique type
            self.auto_array_type = self.new_anonymous_type(None, None, &[], &[], &[]);
        }
        self.global_readonly_array_type =
            self.get_global_type("ReadonlyArray", 1 /*arity*/, false /*reportErrors*/);
        if self.global_readonly_array_type == self.empty_generic_type {
            self.global_readonly_array_type = self.global_array_type;
        }
        self.any_readonly_array_type = self.create_type_from_generic_global_type(
            self.global_readonly_array_type,
            &[self.any_type],
        );
        self.global_this_type =
            self.get_global_type("ThisType", 1 /*arity*/, false /*reportErrors*/);
        // Now merge global ambient module declarations
        for symbol in ambient_module_symbols {
            self.merge_global_symbol(symbol);
        }
        self.merge_pattern_ambient_modules();
        // merge _nonglobal_ module augmentations.
        // this needs to be done after global symbol table is initialized to make sure that all ambient modules are indexed
        for list in &augmentations {
            for &augmentation in *list {
                if !ast::is_global_scope_augmentation(augmentation.parent().unwrap()) {
                    self.merge_module_augmentation(augmentation);
                }
            }
        }
    }

    // checker.go:1397
    fn merge_global_symbol(&mut self, symbol: P<Symbol>) {
        let global_symbol = self.globals.lookup(symbol.name());
        let merged = if let Some(global_symbol) = global_symbol {
            self.merge_symbol(global_symbol, symbol, false /*unidirectional*/)
        } else {
            self.get_merged_symbol(symbol)
        };
        self.globals.set(symbol.name(), merged);
    }

    // Pattern ambient modules are merged together if they have the same pattern and identical import attributes type.
    // checker.go:1409
    fn merge_pattern_ambient_modules(&mut self) {
        let mut groups_by_pattern: FxHashMap<String, Vec<usize>> = FxHashMap::default();
        // Go builds fresh `&ast.PatternAmbientModule{...}` values and mutates their Symbol; they are only published at the end.
        let mut grouped: Vec<(tsrs_core::Pattern, P<Symbol>)> =
            Vec::with_capacity(self.pattern_ambient_modules.len());
        let modules = self.pattern_ambient_modules.clone();
        for module in &modules {
            let attributes_type = self.get_type_of_module_import_attributes(module.symbol);
            let mut group_index = None;
            let indexes = groups_by_pattern.get(&module.pattern.text).cloned().unwrap_or_default();
            for index in indexes {
                let other = self.get_type_of_module_import_attributes(grouped[index].1);
                if self.is_type_identical_to(attributes_type, other) {
                    group_index = Some(index);
                    break;
                }
            }
            if group_index.is_none() {
                groups_by_pattern
                    .entry(module.pattern.text.clone())
                    .or_default()
                    .push(grouped.len());
                grouped.push((module.pattern.clone(), module.symbol));
            } else if let Some(group_index) = group_index {
                grouped[group_index].1 = self.merge_symbol(
                    grouped[group_index].1,
                    module.symbol,
                    false, /*unidirectional*/
                );
            }
        }
        for module in &modules {
            if self.globals.has(module.symbol.name()) {
                let merged = self.get_merged_symbol(module.symbol);
                self.globals.set(module.symbol.name(), merged);
            }
        }
        self.pattern_ambient_modules = grouped
            .into_iter()
            .map(|(pattern, symbol)| P::new(ast::PatternAmbientModule { pattern, symbol }))
            .collect();
    }

    // checker.go:1436
    fn merge_module_augmentation(&mut self, module_name: P<Node>) {
        let module_node = module_name.parent().unwrap();
        let module_augmentation_symbol = module_node.symbol().unwrap();
        if module_augmentation_symbol.declarations()[0] != module_node {
            // this is a combined symbol for multiple augmentations within the same file.
            // its symbol already has accumulated information for all declarations
            // so we need to add it just once - do the work only for first declaration
            return;
        }
        if ast::is_global_scope_augmentation(module_node) {
            let globals = self.globals;
            self.merge_symbol_table(
                globals,
                module_augmentation_symbol.exports(),
                false, /*unidirectional*/
                None,  /*parent*/
            );
        } else {
            // find a module that about to be augmented
            // do not validate names of augmentations that are defined in ambient context
            let mut module_not_found_error: Option<&'static Message> = None;
            if !module_name
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .flags()
                .intersects(NodeFlags::Ambient)
            {
                module_not_found_error = Some(
                    &diagnostics::INVALID_MODULE_NAME_IN_AUGMENTATION_MODULE_0_CANNOT_BE_FOUND,
                );
            }
            // We ban import attributes on module augmentation declarations.
            let Some(main_module) = self.resolve_external_module_name_worker(
                Some(module_name),
                Some(module_name),
                module_not_found_error,
                false, /*ignoreErrors*/
                true,  /*isForAugmentation*/
                None,  /*importAttributesType*/
            ) else {
                return;
            };
            // obtain item referenced by 'export='
            let main_module =
                self.resolve_external_module_symbol(main_module, false /*dontResolveAlias*/);
            if main_module.flags().intersects(SymbolFlags::Namespace) {
                // If we're merging an augmentation to a pattern ambient module, we want to
                // perform the merge unidirectionally from the augmentation ('a.foo') to
                // the pattern ('*.foo'), so that 'getMergedSymbol()' on a.foo gives you
                // all the exports both from the pattern and from the augmentation, but
                // 'getMergedSymbol()' on *.foo only gives you exports from *.foo.
                let pattern_ambient_modules = self.pattern_ambient_modules.clone();
                let some = pattern_ambient_modules
                    .iter()
                    .any(|module| main_module == self.get_merged_symbol(module.symbol));
                if some {
                    let merged = self.merge_symbol(
                        module_augmentation_symbol,
                        main_module,
                        true, /*unidirectional*/
                    );
                    // moduleName will be a StringLiteral since this is not `declare global`.
                    let augmentations_table = *self
                        .pattern_ambient_module_augmentations
                        .get_or_insert_with(SymbolTable::new);
                    augmentations_table.set(module_name.text(), merged);
                    let targets_table = *self
                        .pattern_ambient_module_augmentation_targets
                        .get_or_insert_with(SymbolTable::new);
                    targets_table.set(module_name.text(), main_module);
                } else {
                    let augmentation_exports = module_augmentation_symbol.exports();
                    if main_module
                        .exports()
                        .and_then(|exports| exports.lookup(ast::INTERNAL_SYMBOL_NAME_EXPORT_STAR))
                        .is_some()
                        && augmentation_exports.map_or(0, |exports| exports.len()) != 0
                    {
                        // We may need to merge the module augmentation's exports into the target symbols of the resolved exports
                        let resolved_exports = self.get_resolved_members_or_exports_of_symbol(
                            main_module,
                            MembersOrExportsResolutionKind::ResolvedExports,
                        );
                        for (key, value) in augmentation_exports.unwrap().entries() {
                            if let Some(resolved) = resolved_exports
                                .and_then(|resolved_exports| resolved_exports.lookup(key))
                                && main_module
                                    .exports()
                                    .and_then(|exports| exports.lookup(key))
                                    .is_none()
                            {
                                self.merge_symbol(resolved, value, false /*unidirectional*/);
                            }
                        }
                    }
                    self.merge_symbol(
                        main_module,
                        module_augmentation_symbol,
                        false, /*unidirectional*/
                    );
                }
            } else {
                // moduleName will be a StringLiteral since this is not `declare global`.
                self.error(Some(module_name), &diagnostics::CANNOT_AUGMENT_MODULE_0_BECAUSE_IT_RESOLVES_TO_A_NON_MODULE_ENTITY, &[&module_name.text()]);
            }
        }
    }

    // checker.go:1493
    fn add_undefined_to_globals_or_error_on_redeclaration(&mut self) {
        let name = self.undefined_symbol.name();
        let target_symbol = self.globals.lookup(name);
        if let Some(target_symbol) = target_symbol {
            let declarations = target_symbol.declarations();
            for &declaration in declarations {
                if !ast::is_type_declaration(declaration) {
                    self.add_diagnostic(create_diagnostic_for_node(
                        Some(declaration),
                        &diagnostics::DECLARATION_NAME_CONFLICTS_WITH_BUILT_IN_GLOBAL_IDENTIFIER_0,
                        &[&name],
                    ));
                }
            }
        } else {
            self.globals.set(name, self.undefined_symbol);
        }
    }

    // checker.go:1507
    pub(crate) fn create_name_resolver(&self) -> P<NameResolver<Checker>> {
        P::new(NameResolver::<Checker> {
            compiler_options: self.compiler_options,
            get_symbol_of_declaration: Some(super::checker::Checker::get_symbol_of_declaration),
            error: Some(|c, location, message, args| c.error(location, message, args)),
            globals: Some(self.globals),
            arguments_symbol: Cell::new(Some(self.arguments_symbol)),
            require_symbol: Some(self.require_symbol),
            lookup: Some(super::checker::Checker::get_symbol),
            symbol_referenced: Some(super::checker::Checker::symbol_referenced),
            set_requires_scope_change_cache: Some(
                super::checker::Checker::set_requires_scope_change_cache,
            ),
            get_requires_scope_change_cache: Some(
                super::checker::Checker::get_requires_scope_change_cache,
            ),
            on_property_with_invalid_initializer: Some(
                |c, error_location, name, property_with_invalid_initializer, result| {
                    c.check_and_report_error_for_invalid_initializer(
                        error_location,
                        name,
                        property_with_invalid_initializer,
                        result,
                    )
                },
            ),
            on_failed_to_resolve_symbol: Some(
                |c, error_location, name, meaning, name_not_found_message| {
                    c.on_failed_to_resolve_symbol(
                        error_location,
                        name,
                        meaning,
                        name_not_found_message,
                    );
                },
            ),
            on_successfully_resolved_symbol: Some(
                |c,
                 error_location,
                 result,
                 meaning,
                 last_location,
                 associated_declaration,
                 within_deferred_context| {
                    c.on_successfully_resolved_symbol(
                        error_location,
                        result,
                        meaning,
                        last_location,
                        associated_declaration,
                        within_deferred_context,
                    );
                },
            ),
        })
    }

    // checker.go:1525
    pub(crate) fn create_name_resolver_for_suggestion(&self) -> P<NameResolver<Checker>> {
        P::new(NameResolver::<Checker> {
            compiler_options: self.compiler_options,
            get_symbol_of_declaration: Some(super::checker::Checker::get_symbol_of_declaration),
            error: Some(|c, location, message, args| c.error(location, message, args)),
            globals: Some(self.globals),
            arguments_symbol: Cell::new(Some(self.arguments_symbol)),
            require_symbol: Some(self.require_symbol),
            lookup: Some(super::checker::Checker::get_suggestion_for_symbol_name_lookup),
            symbol_referenced: Some(super::checker::Checker::symbol_referenced),
            set_requires_scope_change_cache: Some(
                super::checker::Checker::set_requires_scope_change_cache,
            ),
            get_requires_scope_change_cache: Some(
                super::checker::Checker::get_requires_scope_change_cache,
            ),
            ..NameResolver::new(self.compiler_options, Some(self.globals))
        })
    }

    // checker.go:1540
    pub(crate) fn symbol_referenced(&mut self, symbol: P<Symbol>, meaning: SymbolFlags) {
        self.symbol_reference_links.add_reference_kinds(symbol, meaning);
    }

    /// The meanings `symbol` was referenced with: Go reads `c.symbolReferenceLinks.Get(symbol).referenceKinds`, which
    /// creates empty links for a symbol never referenced; the links hold nothing but the flags, so a read makes none
    /// (the unused-locals checks ask about every local).
    pub(crate) fn reference_kinds(&self, symbol: P<Symbol>) -> SymbolFlags {
        self.symbol_reference_links.reference_kinds(symbol)
    }

    // checker.go:1544
    fn get_requires_scope_change_cache(&mut self, node: P<Node>) -> Tristate {
        self.node_links.get(node).declaration_requires_scope_change.get()
    }

    // checker.go:1548
    fn set_requires_scope_change_cache(&mut self, node: P<Node>, value: Tristate) {
        self.node_links.get(node).declaration_requires_scope_change.set(value);
    }

    // The invalid initializer error is needed in two situation:
    // 1. When result is undefined, after checking for a missing "this."
    // 2. When result is defined
    // checker.go:1555
    fn check_and_report_error_for_invalid_initializer(
        &mut self,
        error_location: Option<P<Node>>,
        name: &str,
        property_with_invalid_initializer: P<Node>,
        result: Option<P<Symbol>>,
    ) -> bool {
        if !self.compiler_options.get_emit_standard_class_fields() {
            if let Some(error_location) = error_location
                && result.is_none()
                && self.check_and_report_error_for_missing_prefix(error_location, name)
            {
                return true;
            }
            // We have a match, but the reference occurred within a property initializer and the identifier also binds
            // to a local variable in the constructor where the code will be emitted. Note that this is actually allowed
            // with emitStandardClassFields because the scope semantics are different.
            let prop = property_with_invalid_initializer;
            let message = if error_location.is_some()
                && prop
                    .type_node()
                    .is_some_and(|t| t.loc().contains_inclusive(error_location.unwrap().pos()))
            {
                &diagnostics::TYPE_OF_INSTANCE_MEMBER_VARIABLE_0_CANNOT_REFERENCE_IDENTIFIER_1_DECLARED_IN_THE_CONSTRUCTOR
            } else {
                &diagnostics::INITIALIZER_OF_INSTANCE_MEMBER_VARIABLE_0_CANNOT_REFERENCE_IDENTIFIER_1_DECLARED_IN_THE_CONSTRUCTOR
            };
            self.error(
                error_location,
                message,
                &[&tsrs_scanner::declaration_name_to_string(prop.name()), &name],
            );
            return true;
        }
        false
    }

    // checker.go:1573
    fn check_and_report_error_for_missing_prefix(
        &mut self,
        error_location: P<Node>,
        name: &str,
    ) -> bool {
        if !ast::is_identifier(error_location)
            || error_location.text() != name
            || is_type_reference_identifier(error_location)
            || is_in_type_query(error_location)
        {
            return false;
        }
        let container = Self::get_this_container(
            error_location,
            false, /*includeArrowFunctions*/
            false, /*includeClassComputedPropertyName*/
        )
        .unwrap();
        let mut location = container;
        while let Some(location_parent) = location.parent() {
            if ast::is_class_like(location_parent) {
                let Some(class_symbol) = self.get_symbol_of_declaration(location_parent) else {
                    break;
                };
                // Check to see if a static member exists.
                let constructor_type = self.get_type_of_symbol(class_symbol);
                if self.get_property_of_type(constructor_type, name).is_some() {
                    let class_name = self.symbol_to_string(class_symbol);
                    self.error(
                        Some(error_location),
                        &diagnostics::CANNOT_FIND_NAME_0_DID_YOU_MEAN_THE_STATIC_MEMBER_1_0,
                        &[&name, &class_name],
                    );
                    return true;
                }
                // No static member is present.
                // Check if we're in an instance method and look for a relevant instance member.
                if location == container && !ast::is_static(location) {
                    let instance_type = self
                        .get_declared_type_of_symbol(class_symbol)
                        .as_interface_type()
                        .this_type
                        .get()
                        .unwrap();
                    // TODO: GH#18217
                    if self.get_property_of_type(instance_type, name).is_some() {
                        self.error(Some(error_location), &diagnostics::CANNOT_FIND_NAME_0_DID_YOU_MEAN_THE_INSTANCE_MEMBER_THIS_0, &[&name]);
                        return true;
                    }
                }
            }
            location = location_parent;
        }
        false
    }

    // checker.go:1605
    fn on_failed_to_resolve_symbol(
        &mut self,
        error_location: Option<P<Node>>,
        name: &str,
        meaning: SymbolFlags,
        name_not_found_message: &'static Message,
    ) {
        // The `const` in a `const` assertion (`x as const`) is a syntactic marker, not a real
        // type reference, and must never be resolved or reported as an unresolvable name.
        if is_const_type_reference_name(error_location) {
            return;
        }
        if let Some(loc) = error_location
            && (loc.parent().unwrap().kind() == Kind::JSDocLink
                || self.check_and_report_error_for_missing_prefix(loc, name)
                || self.check_and_report_error_for_extending_interface(loc)
                || self.check_and_report_error_for_using_type_as_namespace(loc, name, meaning)
                || self.check_and_report_error_for_exporting_primitive_type(loc, name)
                || self.check_and_report_error_for_using_namespace_as_type_or_value(
                    loc, name, meaning,
                )
                || self.check_and_report_error_for_using_type_as_value(loc, name, meaning)
                || self.check_and_report_error_for_using_value_as_type(loc, name, meaning))
        {
            return;
        }
        let mut declaration_name = name.to_string();
        if let Some(loc) = error_location
            && ast::is_identifier(loc)
            && loc.text() == name
        {
            declaration_name = tsrs_scanner::declaration_name_to_string(Some(loc)); // use escape sequences from original file
        }
        // Report missing lib first
        let suggested_lib = Self::get_suggested_lib_for_non_existent_name(name);
        if !suggested_lib.is_empty() {
            self.error(
                error_location,
                name_not_found_message,
                &[&declaration_name, &suggested_lib],
            );
            return;
        }
        // Then spelling suggestions
        let suggestion =
            self.get_suggested_symbol_for_nonexistent_symbol(error_location, name, meaning);
        if let Some(suggestion) = suggestion
            && !suggestion.value_declaration().is_some_and(|value_declaration| {
                ast::is_ambient_module(value_declaration)
                    && ast::is_global_scope_augmentation(value_declaration)
            })
        {
            let suggestion_name = self.symbol_to_string(suggestion);
            let is_unchecked_js = self.is_unchecked_js_suggestion(
                error_location,
                Some(suggestion),
                false, /*excludeClasses*/
            );
            let message = if meaning == SymbolFlags::Namespace {
                &diagnostics::CANNOT_FIND_NAMESPACE_0_DID_YOU_MEAN_1
            } else if is_unchecked_js {
                &diagnostics::COULD_NOT_FIND_NAME_0_DID_YOU_MEAN_1
            } else {
                &diagnostics::CANNOT_FIND_NAME_0_DID_YOU_MEAN_1
            };
            let diagnostic = new_diagnostic_for_node(
                error_location,
                Some(message),
                &[&declaration_name, &suggestion_name],
            );
            if let Some(value_declaration) = suggestion.value_declaration() {
                diagnostic.add_related_info(new_diagnostic_for_node(
                    Some(value_declaration),
                    Some(&diagnostics::X_0_IS_DECLARED_HERE),
                    &[&suggestion_name],
                ));
            }
            self.add_error_or_suggestion(!is_unchecked_js, diagnostic);
            return;
        }
        // And then fall back to unspecified "not found"
        self.error(error_location, name_not_found_message, &[&declaration_name]);
    }

    // checker.go:1649
    fn check_and_report_error_for_using_type_as_namespace(
        &mut self,
        error_location: P<Node>,
        name: &str,
        meaning: SymbolFlags,
    ) -> bool {
        if meaning == SymbolFlags::Namespace {
            let symbol = self
                .resolve_name(
                    Some(error_location),
                    name,
                    SymbolFlags::Type & !SymbolFlags::Namespace,
                    None,  /*nameNotFoundMessage*/
                    false, /*isUse*/
                    false, /*excludeGlobals*/
                )
                .map(|s| self.resolve_symbol(s));
            if let Some(symbol) = symbol {
                let parent = error_location.parent().unwrap();
                if ast::is_qualified_name(parent) {
                    assert!(
                        parent.as_qualified_name().left == error_location,
                        "Should only be resolving left side of qualified name as a namespace"
                    );
                    let prop_name = parent.as_qualified_name().right.text();
                    let declared_type = self.get_declared_type_of_symbol(symbol);
                    let prop_type = self.get_property_of_type(declared_type, prop_name);
                    if prop_type.is_some() {
                        self.error(
                            Some(parent),
                            &diagnostics::CANNOT_ACCESS_0_1_BECAUSE_0_IS_A_TYPE_BUT_NOT_A_NAMESPACE_DID_YOU_MEAN_TO_RETRIEVE_THE_TYPE_OF_THE_PROPERTY_1_IN_0_WITH_0_1,
                            &[&name, &prop_name],
                        );
                        return true;
                    }
                }
                self.error(
                    Some(error_location),
                    &diagnostics::X_0_ONLY_REFERS_TO_A_TYPE_BUT_IS_BEING_USED_AS_A_NAMESPACE_HERE,
                    &[&name],
                );
                return true;
            }
        }
        false
    }

    // checker.go:1670
    fn check_and_report_error_for_exporting_primitive_type(
        &mut self,
        error_location: P<Node>,
        name: &str,
    ) -> bool {
        if is_primitive_type_name(name)
            && error_location.parent().unwrap().kind() == Kind::ExportSpecifier
        {
            self.error(
                Some(error_location),
                &diagnostics::CANNOT_EXPORT_0_ONLY_LOCAL_DECLARATIONS_CAN_BE_EXPORTED_FROM_A_MODULE,
                &[&name],
            );
            return true;
        }
        false
    }
}

// checker.go:1678
fn is_primitive_type_name(s: &str) -> bool {
    s == "any" || s == "string" || s == "number" || s == "boolean" || s == "never" || s == "unknown"
}

impl Checker {
    // checker.go:1682
    fn check_and_report_error_for_using_namespace_as_type_or_value(
        &mut self,
        error_location: P<Node>,
        name: &str,
        meaning: SymbolFlags,
    ) -> bool {
        if meaning.intersects(SymbolFlags::Value & !SymbolFlags::Type) {
            let symbol = self
                .resolve_name(
                    Some(error_location),
                    name,
                    SymbolFlags::NamespaceModule,
                    None,  /*nameNotFoundMessage*/
                    false, /*isUse*/
                    false, /*excludeGlobals*/
                )
                .map(|s| self.resolve_symbol(s));
            if symbol.is_some() {
                // `export = ns` may legitimately reference a namespace; checkExportAssignment decides
                // whether that is an error, so don't report "cannot use namespace as a value" here.
                if !is_export_assignment_expression_name(Some(error_location)) {
                    self.error(
                        Some(error_location),
                        &diagnostics::CANNOT_USE_NAMESPACE_0_AS_A_VALUE,
                        &[&name],
                    );
                }
                return true;
            }
        } else if meaning.intersects(SymbolFlags::Type & !SymbolFlags::Value) {
            let symbol = self
                .resolve_name(
                    Some(error_location),
                    name,
                    SymbolFlags::Module,
                    None,  /*nameNotFoundMessage*/
                    false, /*isUse*/
                    false, /*excludeGlobals*/
                )
                .map(|s| self.resolve_symbol(s));
            if symbol.is_some() {
                self.error(
                    Some(error_location),
                    &diagnostics::CANNOT_USE_NAMESPACE_0_AS_A_TYPE,
                    &[&name],
                );
                return true;
            }
        }
        false
    }

    // checker.go:1703
    fn check_and_report_error_for_using_type_as_value(
        &mut self,
        error_location: P<Node>,
        name: &str,
        meaning: SymbolFlags,
    ) -> bool {
        if meaning.intersects(SymbolFlags::Value) {
            if is_primitive_type_name(name) {
                let grandparent = error_location.parent().unwrap().parent();
                if let Some(grandparent) =
                    grandparent.filter(|g| g.parent().is_some() && ast::is_heritage_clause(*g))
                {
                    let heritage_kind = grandparent.as_heritage_clause().token;
                    let container = grandparent.parent().unwrap();
                    let container_kind = container.kind();
                    if container_kind == Kind::InterfaceDeclaration
                        && heritage_kind == Kind::ExtendsKeyword
                    {
                        self.error(
                            Some(error_location),
                            &diagnostics::AN_INTERFACE_CANNOT_EXTEND_A_PRIMITIVE_TYPE_LIKE_0_IT_CAN_ONLY_EXTEND_OTHER_NAMED_OBJECT_TYPES,
                            &[&name],
                        );
                    } else if ast::is_class_like(container) && heritage_kind == Kind::ExtendsKeyword
                    {
                        self.error(
                            Some(error_location),
                            &diagnostics::A_CLASS_CANNOT_EXTEND_A_PRIMITIVE_TYPE_LIKE_0_CLASSES_CAN_ONLY_EXTEND_CONSTRUCTABLE_VALUES,
                            &[&name],
                        );
                    } else if ast::is_class_like(container)
                        && heritage_kind == Kind::ImplementsKeyword
                    {
                        self.error(
                            Some(error_location),
                            &diagnostics::A_CLASS_CANNOT_IMPLEMENT_A_PRIMITIVE_TYPE_LIKE_0_IT_CAN_ONLY_IMPLEMENT_OTHER_NAMED_OBJECT_TYPES,
                            &[&name],
                        );
                    }
                } else {
                    self.error(
                        Some(error_location),
                        &diagnostics::X_0_ONLY_REFERS_TO_A_TYPE_BUT_IS_BEING_USED_AS_A_VALUE_HERE,
                        &[&name],
                    );
                }
                return true;
            }
            let symbol = self
                .resolve_name(
                    Some(error_location),
                    name,
                    SymbolFlags::Type & !SymbolFlags::Value,
                    None,  /*nameNotFoundMessage*/
                    false, /*isUse*/
                    false, /*excludeGlobals*/
                )
                .map(|s| self.resolve_symbol(s));
            if let Some(symbol) = symbol {
                let all_flags = self.get_symbol_flags(symbol);
                if !all_flags.intersects(SymbolFlags::Value) {
                    // `export = SomeType` may legitimately reference a type-only name; checkExportAssignment
                    // decides whether that is an error, so don't report "used as a value" here.
                    if is_export_assignment_expression_name(Some(error_location)) {
                        return true;
                    }
                    if is_es2015_or_later_constructor_name(name) {
                        self.error(
                            Some(error_location),
                            &diagnostics::X_0_ONLY_REFERS_TO_A_TYPE_BUT_IS_BEING_USED_AS_A_VALUE_HERE_DO_YOU_NEED_TO_CHANGE_YOUR_TARGET_LIBRARY_TRY_CHANGING_THE_LIB_COMPILER_OPTION_TO_ES2015_OR_LATER,
                            &[&name],
                        );
                    } else if self.maybe_mapped_type(error_location, symbol) {
                        self.error(
                            Some(error_location),
                            &diagnostics::X_0_ONLY_REFERS_TO_A_TYPE_BUT_IS_BEING_USED_AS_A_VALUE_HERE_DID_YOU_MEAN_TO_USE_1_IN_0,
                            &[&name, &if name == "K" { "P" } else { "K" }],
                        );
                    } else {
                        self.error(Some(error_location), &diagnostics::X_0_ONLY_REFERS_TO_A_TYPE_BUT_IS_BEING_USED_AS_A_VALUE_HERE, &[&name]);
                    }
                    return true;
                }
            }
        }
        false
    }
}

// checker.go:1745
fn is_es2015_or_later_constructor_name(s: &str) -> bool {
    s == "Promise" || s == "Symbol" || s == "Map" || s == "WeakMap" || s == "Set" || s == "WeakSet"
}

impl Checker {
    // checker.go:1749
    fn maybe_mapped_type(&mut self, node: P<Node>, symbol: P<Symbol>) -> bool {
        let mut node = node;
        loop {
            node = node.parent().unwrap();
            if !(ast::is_computed_property_name(node)
                || ast::is_property_signature_declaration(node))
            {
                break;
            }
        }
        if ast::is_type_literal_node(node) && node.members().len() == 1 {
            let t = self.get_declared_type_of_symbol(symbol);
            return t.flags().intersects(TypeFlags::Union)
                && self.all_types_assignable_to_kind_ex(
                    t,
                    TypeFlags::StringOrNumberLiteral,
                    true, /*strict*/
                );
        }
        false
    }

    // checker.go:1763
    fn check_and_report_error_for_using_value_as_type(
        &mut self,
        error_location: P<Node>,
        name: &str,
        meaning: SymbolFlags,
    ) -> bool {
        if meaning.intersects(SymbolFlags::Type & !SymbolFlags::Namespace) {
            let symbol = self
                .resolve_name(
                    Some(error_location),
                    name,
                    !SymbolFlags::Type & SymbolFlags::Value,
                    None,  /*nameNotFoundMessage*/
                    false, /*isUse*/
                    false, /*excludeGlobals*/
                )
                .map(|s| self.resolve_symbol(s));
            if let Some(symbol) = symbol
                && !symbol.flags().intersects(SymbolFlags::Namespace)
            {
                self.error(Some(error_location), &diagnostics::X_0_REFERS_TO_A_VALUE_BUT_IS_BEING_USED_AS_A_TYPE_HERE_DID_YOU_MEAN_TYPEOF_0, &[&name]);
                return true;
            }
        }
        false
    }

    // checker.go:1774
    fn get_suggested_lib_for_non_existent_name(name: &str) -> String {
        let feature_map = get_feature_map();
        if let Some(type_features) = feature_map.get(name) {
            return type_features[0].lib.to_string();
        }
        String::new()
    }

    // location Option<P<Node>>: onFailedToResolveSymbol passes a nil errorLocation (getGlobalSymbol).
    // checker.go:1782
    fn get_suggested_symbol_for_nonexistent_symbol(
        &mut self,
        location: Option<P<Node>>,
        outer_name: &str,
        meaning: SymbolFlags,
    ) -> Option<P<Symbol>> {
        self.resolve_name_for_symbol_suggestion(
            location, outer_name, meaning, None,  /*nameNotFoundMessage*/
            false, /*isUse*/
            false, /*excludeGlobals*/
        )
    }
}

// checker.go:1804
fn get_primitive_type_alias_suggestions(symbols: Option<P<SymbolTable>>) -> Vec<P<Symbol>> {
    let mut result = Vec::new();
    result.extend(primitive_type_alias_suggestions().iter().filter_map(
        |(builtin_name, suggestion)| {
            if symbols.is_some_and(|symbols| symbols.has(builtin_name)) {
                Some(*suggestion)
            } else {
                None
            }
        },
    ));
    result
}

impl Checker {
    // checker.go:1816
    fn get_suggestion_for_symbol_name_lookup(
        &mut self,
        symbols: Option<P<SymbolTable>>,
        name: &str,
        meaning: SymbolFlags,
    ) -> Option<P<Symbol>> {
        let symbol = self.get_symbol(symbols, name, meaning);
        if symbol.is_some() {
            return symbol;
        }
        let mut extras: Vec<P<Symbol>> = Vec::new();
        if meaning.intersects(SymbolFlags::GlobalLookup) {
            extras = get_primitive_type_alias_suggestions(symbols);
        }
        let mut candidates = symbols.map_or_else(Vec::new, |symbols| symbols.values());
        candidates.extend(extras);
        self.get_spelling_suggestion_for_name(name, &candidates, meaning)
    }

    // Given a name and a list of symbols whose names are *not* equal to the name, return a spelling suggestion if there is
    // one that is close enough. Names less than length 3 only check for case-insensitive equality, not levenshtein distance.
    //
    // If there is a candidate that's the same except for case, return that.
    // If there is a candidate that's within one edit of the name, return that.
    // Otherwise, return the candidate with the smallest Levenshtein distance,
    //
    // Except for candidates:
    //   - With no name
    //   - Whose meaning doesn't match the `meaning` parameter.
    //   - Whose length differs from the target name by more than 0.34 of the length of the name.
    //   - Whose levenshtein distance is more than 0.4 of the length of the name (0.4 allows 1 substitution/transposition
    //     for every 5 characters, and 1 insertion/deletion at 3 characters)
    // checker.go:1841
    pub(crate) fn get_spelling_suggestion_for_name(
        &mut self,
        name: &str,
        symbols: &[P<Symbol>],
        meaning: SymbolFlags,
    ) -> Option<P<Symbol>> {
        // Both callbacks need the checker; the suggestion worker never calls them re-entrantly.
        let c = RefCell::new(self);
        let get_candidate_name = |candidate: &P<Symbol>| -> &'static str {
            let candidate = *candidate;
            let candidate_name = ast::symbol_name(candidate);
            if candidate_name.is_empty()
                || candidate_name.as_bytes()[0] == b'"'
                || candidate_name.as_bytes()[0] == ast::INTERNAL_SYMBOL_NAME_PREFIX_BYTE
            {
                return "";
            }
            if candidate.flags().intersects(meaning) {
                return candidate_name;
            }
            if candidate.flags().intersects(SymbolFlags::Alias) {
                let alias = c.borrow_mut().try_resolve_alias(candidate);
                if alias.is_some_and(|alias| alias.flags().intersects(meaning)) {
                    return candidate_name;
                }
            }
            ""
        };
        tsrs_core::get_spelling_suggestion(
            name,
            symbols.iter().copied(),
            get_candidate_name,
            |a: &P<Symbol>, b: &P<Symbol>| c.borrow_mut().compare_symbols(Some(*a), Some(*b)),
        )
    }

    // checker.go:1861
    fn on_successfully_resolved_symbol(
        &mut self,
        error_location: Option<P<Node>>,
        result: P<Symbol>,
        meaning: SymbolFlags,
        last_location: Option<P<Node>>,
        associated_declaration_for_containing_initializer_or_binding_name: Option<P<Node>>,
        within_deferred_context: bool,
    ) {
        let name = result.name();
        let is_in_external_module = last_location.is_some_and(|last_location| {
            ast::is_source_file(last_location)
                && ast::is_external_or_common_js_module(last_location.as_source_file_p())
        });
        // Only check for block-scoped variable if we have an error location and are looking for the
        // name with variable meaning
        //      For example,
        //          declare module foo {
        //              interface bar {}
        //          }
        //      const foo/*1*/: foo/*2*/.bar;
        // The foo at /*1*/ and /*2*/ will share same symbol with two meanings:
        // block-scoped variable and namespace module. However, only when we
        // try to resolve name in /*1*/ which is used in variable position,
        // we want to check for block-scoped
        if let Some(error_location) = error_location
            && (meaning.intersects(SymbolFlags::BlockScopedVariable)
                || meaning.intersects(SymbolFlags::Class | SymbolFlags::Enum)
                    && meaning & SymbolFlags::Value == SymbolFlags::Value)
        {
            let export_or_local_symbol =
                self.get_export_symbol_of_value_symbol_if_exported(Some(result)).unwrap();
            if export_or_local_symbol.flags().intersects(
                SymbolFlags::BlockScopedVariable | SymbolFlags::Class | SymbolFlags::Enum,
            ) {
                self.check_resolved_block_scoped_variable(export_or_local_symbol, error_location);
            }
        }
        // If we're in an external module, we can't reference value symbols created from UMD export declarations
        if is_in_external_module
            && (meaning & SymbolFlags::Value) == SymbolFlags::Value
            && !error_location.unwrap().flags().intersects(NodeFlags::JSDoc)
        {
            let merged = self.get_merged_symbol(result);
            let declarations = merged.declarations();
            if !declarations.is_empty()
                && declarations.iter().all(|d| {
                    ast::is_namespace_export_declaration(*d)
                        || ast::is_source_file(*d) && d.as_source_file().global_exports().is_some()
                })
            {
                self.error_or_suggestion(
                    self.compiler_options.allow_umd_global_access != Tristate::True,
                    error_location,
                    &diagnostics::X_0_REFERS_TO_A_UMD_GLOBAL_BUT_THE_CURRENT_FILE_IS_A_MODULE_CONSIDER_ADDING_AN_IMPORT_INSTEAD,
                    &[&name],
                );
            }
        }
        // If we're in a parameter initializer or binding name, we can't reference the values of the parameter whose initializer we're within or parameters to the right
        if let Some(associated) = associated_declaration_for_containing_initializer_or_binding_name
            && !within_deferred_context
            && (meaning & SymbolFlags::Value) == SymbolFlags::Value
        {
            let late_bound = self.get_late_bound_symbol(result);
            let candidate = self.get_merged_symbol(late_bound);
            let root = ast::get_root_declaration(associated);
            // A parameter initializer or binding pattern initializer within a parameter cannot refer to itself
            if Some(candidate) == self.get_symbol_of_declaration(associated) {
                self.error(
                    error_location,
                    &diagnostics::PARAMETER_0_CANNOT_REFERENCE_ITSELF,
                    &[&tsrs_scanner::declaration_name_to_string(associated.name())],
                );
            } else if candidate
                .value_declaration()
                .is_some_and(|value_declaration| value_declaration.pos() > associated.pos())
                && root.parent().unwrap().locals().is_some()
                && self.get_symbol(root.parent().unwrap().locals(), candidate.name(), meaning)
                    == Some(candidate)
            {
                self.error(
                    error_location,
                    &diagnostics::PARAMETER_0_CANNOT_REFERENCE_IDENTIFIER_1_DECLARED_AFTER_IT,
                    &[
                        &tsrs_scanner::declaration_name_to_string(associated.name()),
                        &tsrs_scanner::declaration_name_to_string(error_location),
                    ],
                );
            }
        }
        if let Some(loc) = error_location
            && meaning.intersects(SymbolFlags::Value)
            && result.flags().intersects(SymbolFlags::Alias)
            && !result.flags().intersects(SymbolFlags::Value)
            && !ast::is_valid_type_only_alias_use_site(loc)
        {
            let type_only_declaration =
                self.get_type_only_alias_declaration_ex(result, SymbolFlags::Value);
            if let Some(type_only_declaration) = type_only_declaration {
                let message = if ast::node_kind_is(
                    type_only_declaration,
                    &[Kind::ExportSpecifier, Kind::ExportDeclaration, Kind::NamespaceExport],
                ) {
                    &diagnostics::X_0_CANNOT_BE_USED_AS_A_VALUE_BECAUSE_IT_WAS_EXPORTED_USING_EXPORT_TYPE
                } else {
                    &diagnostics::X_0_CANNOT_BE_USED_AS_A_VALUE_BECAUSE_IT_WAS_IMPORTED_USING_IMPORT_TYPE
                };
                let diagnostic = self.error(error_location, message, &[&name]);
                Self::add_type_only_declaration_related_info(
                    diagnostic,
                    Some(type_only_declaration),
                    name,
                );
            }
        }
        // Look at 'compilerOptions.isolatedModules' and not 'getIsolatedModules(...)' (which considers 'verbatimModuleSyntax')
        // here because 'verbatimModuleSyntax' will already have an error for importing a type without 'import type'.
        if self.compiler_options.isolated_modules == Tristate::True
            && is_in_external_module
            && (meaning & SymbolFlags::Value) == SymbolFlags::Value
        {
            let globals = self.globals;
            let is_global = self.get_symbol(Some(globals), name, meaning) == Some(result);
            let mut non_value_symbol: Option<P<Symbol>> = None;
            let last_location = last_location.unwrap();
            if is_global && ast::is_source_file(last_location) {
                non_value_symbol =
                    self.get_symbol(last_location.locals(), name, !SymbolFlags::Value);
            }
            if let Some(non_value_symbol) = non_value_symbol {
                let import_decl = non_value_symbol.declarations().iter().copied().find(|d| {
                    ast::node_kind_is(
                        *d,
                        &[
                            Kind::ImportSpecifier,
                            Kind::ImportClause,
                            Kind::NamespaceImport,
                            Kind::ImportEqualsDeclaration,
                        ],
                    )
                });
                if let Some(import_decl) = import_decl
                    && !ast::is_type_only_import_declaration(import_decl)
                {
                    self.error(
                            Some(import_decl),
                            &diagnostics::IMPORT_0_CONFLICTS_WITH_GLOBAL_VALUE_USED_IN_THIS_FILE_SO_MUST_BE_DECLARED_WITH_A_TYPE_ONLY_IMPORT_WHEN_ISOLATED_MODULES_IS_ENABLED,
                            &[&name],
                        );
                }
            }
        }
    }

    // checker.go:1929
    fn check_resolved_block_scoped_variable(&mut self, result: P<Symbol>, error_location: P<Node>) {
        assert!(
            result.flags().intersects(SymbolFlags::BlockScopedVariable)
                || result.flags().intersects(SymbolFlags::Class)
                || result.flags().intersects(SymbolFlags::Enum)
        );
        if result.flags().intersects(
            SymbolFlags::Function | SymbolFlags::FunctionScopedVariable | SymbolFlags::Assignment,
        ) && result.flags().intersects(SymbolFlags::Class)
        {
            // constructor functions aren't block scoped
            return;
        }
        // Block-scoped variables cannot be used before their definition
        let declaration = result.declarations().iter().copied().find(|d| {
            ast::is_block_or_catch_scoped(*d)
                || ast::is_class_like(*d)
                || ast::is_enum_declaration(*d)
        });
        let Some(declaration) = declaration else {
            panic!("checkResolvedBlockScopedVariable could not find block-scoped declaration");
        };
        if !declaration.flags().intersects(NodeFlags::Ambient)
            && !self.is_block_scoped_name_declared_before_use(declaration, error_location)
        {
            let mut diagnostic: Option<P<Diagnostic>> = None;
            let declaration_name =
                tsrs_scanner::declaration_name_to_string(ast::get_name_of_declaration(declaration));
            if result.flags().intersects(SymbolFlags::BlockScopedVariable) {
                diagnostic = Some(self.error(
                    Some(error_location),
                    &diagnostics::BLOCK_SCOPED_VARIABLE_0_USED_BEFORE_ITS_DECLARATION,
                    &[&declaration_name],
                ));
            } else if result.flags().intersects(SymbolFlags::Class) {
                diagnostic = Some(self.error(
                    Some(error_location),
                    &diagnostics::CLASS_0_USED_BEFORE_ITS_DECLARATION,
                    &[&declaration_name],
                ));
            } else if result.flags().intersects(SymbolFlags::RegularEnum) {
                diagnostic = Some(self.error(
                    Some(error_location),
                    &diagnostics::ENUM_0_USED_BEFORE_ITS_DECLARATION,
                    &[&declaration_name],
                ));
            } else {
                assert!(result.flags().intersects(SymbolFlags::ConstEnum));
                if self.compiler_options.get_isolated_modules() {
                    diagnostic = Some(self.error(
                        Some(error_location),
                        &diagnostics::ENUM_0_USED_BEFORE_ITS_DECLARATION,
                        &[&declaration_name],
                    ));
                }
            }
            if let Some(diagnostic) = diagnostic {
                diagnostic.add_related_info(create_diagnostic_for_node(
                    Some(declaration),
                    &diagnostics::X_0_IS_DECLARED_HERE,
                    &[&declaration_name],
                ));
            }
        }
    }

    // checker.go:1963
    pub(crate) fn is_block_scoped_name_declared_before_use(
        &mut self,
        declaration: P<Node>,
        usage: P<Node>,
    ) -> bool {
        let declaration_file = ast::get_source_file_of_node(declaration);
        let use_file = ast::get_source_file_of_node(usage);
        let decl_container = ast::get_enclosing_block_scope_container(declaration);
        if declaration_file != use_file {
            // nodes are in different files and order cannot be determined
            return true;
        }
        // deferred usage in a type context is always OK regardless of the usage position:
        if usage.flags().intersects(NodeFlags::JSDoc)
            || is_in_type_query(usage)
            || Self::is_in_ambient_or_type_node(usage)
        {
            return true;
        }
        if declaration.pos() <= usage.pos()
            && !(ast::is_property_declaration(declaration)
                && is_this_property(usage.parent().unwrap())
                && declaration.initializer().is_none()
                && !is_exclamation_token(declaration.postfix_token()))
        {
            // declaration is before usage
            if declaration.kind() == Kind::BindingElement {
                // still might be illegal if declaration and usage are both binding elements (eg var [a = b, b = b] = [1, 2])
                let error_binding_element = ast::find_ancestor_kind(usage, Kind::BindingElement);
                if let Some(error_binding_element) = error_binding_element {
                    return ast::find_ancestor(error_binding_element, ast::is_binding_element)
                        != ast::find_ancestor(declaration, ast::is_binding_element)
                        || declaration.pos() < error_binding_element.pos();
                }
                // or it might be illegal if usage happens before parent variable is declared (eg var [a] = a)
                return self.is_block_scoped_name_declared_before_use(
                    ast::find_ancestor_kind(declaration, Kind::VariableDeclaration).unwrap(),
                    usage,
                );
            } else if declaration.kind() == Kind::VariableDeclaration {
                // still might be illegal if usage is in the initializer of the variable declaration (eg var a = a)
                return !is_immediately_used_in_initializer_of_block_scoped_variable(
                    declaration,
                    usage,
                    decl_container.unwrap(),
                );
            } else if ast::is_class_like(declaration) {
                // still might be illegal if the usage is within a computed property name in the class (eg class A { static p = "a"; [A.p]() {} })
                // or when used within a decorator in the class (e.g. `@dec(A.x) class A { static x = "x" }`),
                // except when used in a function that is not an IIFE (e.g., `@dec(() => A.x) class A { ... }`)
                let mut container = Some(usage);
                while let Some(cur) = container {
                    if cur == declaration {
                        break;
                    }
                    let cur_parent = cur.parent();
                    let parent_parent = |n: Option<P<Node>>| n?.parent();
                    if ast::is_computed_property_name(cur)
                        && parent_parent(cur_parent) == Some(declaration)
                        || !self.legacy_decorators
                            && ast::is_decorator(cur)
                            && (cur_parent == Some(declaration)
                                || ast::is_method_declaration(cur_parent.unwrap())
                                    && parent_parent(cur_parent) == Some(declaration)
                                || ast::is_accessor(cur_parent.unwrap())
                                    && parent_parent(cur_parent) == Some(declaration)
                                || ast::is_property_declaration(cur_parent.unwrap())
                                    && parent_parent(cur_parent) == Some(declaration)
                                || ast::is_parameter_declaration(cur_parent.unwrap())
                                    && parent_parent(parent_parent(cur_parent))
                                        == Some(declaration))
                    {
                        break;
                    }
                    container = cur_parent;
                }
                let Some(container) = container.filter(|c| *c != declaration) else {
                    return true;
                };
                if !self.legacy_decorators && ast::is_decorator(container) {
                    let mut n = Some(usage);
                    while let Some(cur) = n {
                        if cur == container {
                            break;
                        }
                        if ast::is_function_like(cur)
                            && ast::get_immediately_invoked_function_expression(cur).is_none()
                        {
                            break;
                        }
                        n = cur.parent();
                    }
                    return n.is_some_and(|n| n != container);
                }
                return false;
            } else if ast::is_property_declaration(declaration) {
                // still might be illegal if a self-referencing property initializer (eg private x = this.x)
                return !is_property_immediately_referenced_within_declaration(
                    declaration,
                    usage,
                    false, /*stopAtAnyPropertyDeclaration*/
                );
            } else if ast::is_parameter_property_declaration(
                declaration,
                declaration.parent().unwrap(),
            ) {
                // foo = this.bar is illegal in emitStandardClassFields when bar is a parameter property
                return !(self.emit_standard_class_fields
                    && ast::get_containing_class(declaration) == ast::get_containing_class(usage)
                    && self.is_used_in_function_or_instance_property(
                        usage,
                        declaration,
                        decl_container.unwrap(),
                    ));
            }
            return true;
        }
        // declaration is after usage, but it can still be legal if usage is deferred:
        // 1. inside an export specifier
        // 2. inside a function
        // 3. inside an instance property initializer, a reference to a non-instance property
        //    (except when emitStandardClassFields: true and the reference is to a parameter property)
        // 4. inside a static property initializer, a reference to a static method in the same class
        // 5. inside a TS export= declaration (since we will move the export statement during emit to avoid TDZ)
        let usage_parent = usage.parent().unwrap();
        if ast::is_export_specifier(usage_parent)
            || ast::is_export_assignment(usage_parent)
                && usage_parent.as_export_assignment().is_export_equals
        {
            // export specifiers do not use the variable, they only make it available for use
            return true;
        }
        // When resolving symbols for exports, the `usage` location passed in can be the export site directly
        if ast::is_export_assignment(usage) && usage.as_export_assignment().is_export_equals {
            return true;
        }
        if self.is_used_in_function_or_instance_property(
            usage,
            declaration,
            decl_container.unwrap(),
        ) {
            if self.emit_standard_class_fields
                && ast::get_containing_class(declaration).is_some()
                && (ast::is_property_declaration(declaration)
                    || ast::is_parameter_property_declaration(
                        declaration,
                        declaration.parent().unwrap(),
                    ))
            {
                return !is_property_immediately_referenced_within_declaration(
                    declaration,
                    usage,
                    true, /*stopAtAnyPropertyDeclaration*/
                );
            }
            return true;
        }
        false
    }

    // checker.go:2052
    fn is_used_in_function_or_instance_property(
        &mut self,
        usage: P<Node>,
        declaration: P<Node>,
        decl_container: P<Node>,
    ) -> bool {
        // ast.FindAncestorOrQuit(usage, callback) != nil, with the walk inlined because the callback needs the checker.
        let mut node = Some(usage);
        while let Some(current) = node {
            match self.is_used_in_function_or_instance_property_callback(
                current,
                usage,
                declaration,
                decl_container,
            ) {
                ast::FindAncestorResult::Quit => return false,
                ast::FindAncestorResult::True => return true,
                ast::FindAncestorResult::False => {}
            }
            node = current.parent();
        }
        false
    }

    fn is_used_in_function_or_instance_property_callback(
        &mut self,
        current: P<Node>,
        usage: P<Node>,
        declaration: P<Node>,
        decl_container: P<Node>,
    ) -> ast::FindAncestorResult {
        if current == decl_container {
            return ast::FindAncestorResult::Quit;
        }
        if ast::is_function_like(current) {
            return ast::to_find_ancestor_result(
                ast::get_immediately_invoked_function_expression(current).is_none(),
            );
        }
        if ast::is_class_static_block_declaration(current) {
            return ast::to_find_ancestor_result(declaration.pos() < usage.pos());
        }

        if let Some(property_declaration) =
            current.parent().filter(|p| ast::is_property_declaration(*p))
        {
            let initializer_of_property = property_declaration.initializer() == Some(current);
            if initializer_of_property {
                if ast::is_static(property_declaration) {
                    if ast::is_method_declaration(declaration) {
                        return ast::FindAncestorResult::True;
                    }
                    if ast::is_property_declaration(declaration)
                        && ast::get_containing_class(usage)
                            == ast::get_containing_class(declaration)
                    {
                        let prop_name = declaration.name().unwrap();
                        if ast::is_identifier(prop_name) || ast::is_private_identifier(prop_name) {
                            let symbol = self.get_symbol_of_declaration(declaration).unwrap();
                            let t = self.get_type_of_symbol(symbol);
                            let declaration_parent = declaration.parent().unwrap();
                            let static_blocks: Vec<P<Node>> = declaration_parent
                                .members()
                                .iter()
                                .copied()
                                .filter(|m| ast::is_class_static_block_declaration(*m))
                                .collect();
                            if self.is_property_initialized_in_static_blocks(
                                prop_name,
                                t,
                                &static_blocks,
                                declaration_parent.pos(),
                                current.pos(),
                            ) {
                                return ast::FindAncestorResult::True;
                            }
                        }
                    }
                } else {
                    let is_declaration_instance_property =
                        ast::is_property_declaration(declaration) && !ast::is_static(declaration);
                    if !is_declaration_instance_property
                        || ast::get_containing_class(usage)
                            != ast::get_containing_class(declaration)
                    {
                        return ast::FindAncestorResult::True;
                    }
                }
            }
        }

        if let Some(decorator) = current.parent().filter(|p| ast::is_decorator(*p))
            && decorator.expression() == Some(current)
        {
            let decorator_parent = decorator.parent().unwrap();
            if ast::is_parameter_declaration(decorator_parent) {
                if self.is_used_in_function_or_instance_property(
                    decorator_parent.parent().unwrap().parent().unwrap(),
                    declaration,
                    decl_container,
                ) {
                    return ast::FindAncestorResult::True;
                }
                return ast::FindAncestorResult::Quit;
            }
            if ast::is_method_declaration(decorator_parent) {
                if self.is_used_in_function_or_instance_property(
                    decorator_parent.parent().unwrap(),
                    declaration,
                    decl_container,
                ) {
                    return ast::FindAncestorResult::True;
                }
                return ast::FindAncestorResult::Quit;
            }
        }

        ast::FindAncestorResult::False
    }
}

// checker.go:2113
fn is_immediately_used_in_initializer_of_block_scoped_variable(
    declaration: P<Node>,
    usage: P<Node>,
    decl_container: P<Node>,
) -> bool {
    let grandparent = declaration.parent().unwrap().parent().unwrap();
    match grandparent.kind() {
        Kind::VariableStatement | Kind::ForStatement | Kind::ForOfStatement
            // variable statement/for/for-of statement case,
            // use site should not be inside variable declaration (initializer of declaration or binding element)
            if is_same_scope_descendent_of(usage, Some(declaration), decl_container) => {
                return true;
            }
        _ => {}
    }
    // ForIn/ForOf case - use site should not be used in expression part
    ast::is_for_in_or_of_statement(grandparent)
        && is_same_scope_descendent_of(usage, grandparent.expression(), decl_container)
}

// Starting from 'initial' node walk up the parent chain until 'stopAt' node is reached.
// If at any point current node is equal to 'parent' node - return true.
// If current node is an IIFE, continue walking up.
// Return false if 'stopAt' node is reached or isFunctionLike(current) === true.
// checker.go:2131
fn is_same_scope_descendent_of(
    initial: P<Node>,
    parent: Option<P<Node>>,
    stop_at: P<Node>,
) -> bool {
    let Some(parent) = parent else {
        return false;
    };
    let mut n = Some(initial);
    while let Some(cur) = n {
        if cur == parent {
            return true;
        }
        if cur == stop_at
            || ast::is_function_like(cur)
                && (ast::get_immediately_invoked_function_expression(cur).is_none()
                    || ast::get_function_flags(Some(cur))
                        .intersects(ast::FunctionFlags::AsyncGenerator))
        {
            return false;
        }
        n = cur.parent();
    }
    false
}

// isPropertyImmediatelyReferencedWithinDeclaration is used for detecting ES-standard class field use-before-def errors
// checker.go:2147
fn is_property_immediately_referenced_within_declaration(
    declaration: P<Node>,
    usage: P<Node>,
    stop_at_any_property_declaration: bool,
) -> bool {
    // always legal if usage is after declaration
    if usage.end() > declaration.end() {
        return false;
    }
    // still might be legal if usage is deferred (e.g. x: any = () => this.x)
    // otherwise illegal if immediately referenced within the declaration (e.g. x: any = this.x)
    let mut node = Some(usage);
    while let Some(cur) = node {
        if cur == declaration {
            break;
        }
        match cur.kind() {
            Kind::ArrowFunction => return false,
            Kind::PropertyDeclaration => {
                // even when stopping at any property declaration, they need to come from the same class
                return stop_at_any_property_declaration
                    && ((ast::is_property_declaration(declaration)
                        && cur.parent() == declaration.parent())
                        || (ast::is_parameter_property_declaration(
                            declaration,
                            declaration.parent().unwrap(),
                        ) && cur.parent() == declaration.parent().unwrap().parent()));
            }
            Kind::Block => match cur.parent().unwrap().kind() {
                Kind::MethodDeclaration | Kind::GetAccessor | Kind::SetAccessor => return false,
                _ => {}
            },
            _ => {}
        }
        node = cur.parent();
    }
    true
}
