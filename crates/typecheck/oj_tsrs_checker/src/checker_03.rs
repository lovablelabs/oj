#![expect(
    clippy::redundant_pub_crate,
    reason = "Only the public API is re-exported; implementation helpers remain crate-private."
)]

use std::collections::hash_map::Entry;

use rustc_hash::FxHashMap;
use tsrs_ast as ast;
use tsrs_ast::*;
use tsrs_core::*;
use tsrs_diagnostics as diagnostics;
use tsrs_diagnostics::Message;

use crate::*;

// Non-function declarations in checker.go:4321-6526 (hand-ported by checker-foundation):
//   type InheritanceInfo (checker.go:5122)

impl Checker {
    // checker.go:4321
    pub(crate) fn check_class_like_declaration(&mut self, node: P<Node>) {
        self.check_grammar_class_like_declaration(node);
        self.check_decorators(node);
        self.check_collisions_for_declaration_name(node, node.name());
        self.check_type_parameters(node.type_parameters());
        self.check_exports_on_merged_declarations(node);
        let symbol = self.get_symbol_of_declaration(node).unwrap();
        let class_type = self.get_declared_type_of_symbol(symbol);
        let class_type_data = class_type.as_interface_type();
        let type_with_this = self.get_type_with_this_argument(class_type, None, false);
        let static_type = self.get_type_of_symbol(symbol);
        self.check_type_parameter_lists_identical(symbol);
        self.check_function_or_constructor_symbol(symbol);
        self.check_object_type_for_duplicate_declarations(node, true /*checkPrivateNames*/);

        // Only check for reserved static identifiers on non-ambient context.
        let node_in_ambient_context = node.flags().intersects(NodeFlags::Ambient);
        if !node_in_ambient_context {
            self.check_class_for_static_property_name_conflicts(node);
        }

        let base_type_node = get_class_extends_heritage_element(node);
        if let Some(base_type_node) = base_type_node {
            self.check_source_elements(base_type_node.type_arguments());
            let base_types = self.get_base_types(class_type);
            if !base_types.is_empty() {
                let base_type = base_types[0];
                let base_constructor_type = self.get_base_constructor_type_of_class(class_type);
                self.check_jsdoc_augments_tag_matches_extends(node, base_type_node, base_type);
                let static_base_type = self.get_apparent_type(base_constructor_type);
                self.check_base_type_accessibility(static_base_type, base_type_node);
                self.check_source_element(base_type_node.expression());
                if !base_type_node.type_arguments().is_empty() {
                    self.check_source_elements(base_type_node.type_arguments());
                    self.get_constructors_for_type_arguments(
                        static_base_type,
                        base_type_node.type_arguments(),
                        base_type_node,
                    )
                    .into_iter()
                    .all(|constructor| {
                        self.check_type_argument_constraints(
                            base_type_node,
                            constructor.type_parameters(),
                        )
                    });
                }
                let base_with_this = self.get_type_with_this_argument(
                    base_type,
                    class_type_data.this_type.get(),
                    false,
                );
                if self.check_type_assignable_to(type_with_this, base_with_this, None, None) {
                    // Report static side error only when instance type is assignable
                    let t = self.get_type_without_signatures(static_base_type);
                    self.check_type_assignable_to(static_type, t, Some(node.name().unwrap_or(node)), Some(&diagnostics::CLASS_STATIC_SIDE_0_INCORRECTLY_EXTENDS_BASE_CLASS_STATIC_SIDE_1));
                } else {
                    self.issue_member_specific_error(
                        node,
                        type_with_this,
                        base_with_this,
                        &diagnostics::CLASS_0_INCORRECTLY_EXTENDS_BASE_CLASS_1,
                    );
                }
                if base_constructor_type.flags().intersects(TypeFlags::TypeVariable) {
                    if self.is_mixin_constructor_type(static_type) {
                        let construct_signatures = self.get_signatures_of_type(
                            base_constructor_type,
                            SignatureKind::Construct,
                        );
                        if construct_signatures
                            .iter()
                            .any(|signature| signature.flags().intersects(SignatureFlags::Abstract))
                            && !has_syntactic_modifier(node, ModifierFlags::Abstract)
                        {
                            self.error(Some(node.name().unwrap_or(node)), &diagnostics::A_MIXIN_CLASS_THAT_EXTENDS_FROM_A_TYPE_VARIABLE_CONTAINING_AN_ABSTRACT_CONSTRUCT_SIGNATURE_MUST_ALSO_BE_DECLARED_ABSTRACT, &[]);
                        }
                    } else {
                        self.error(Some(node.name().unwrap_or(node)), &diagnostics::A_MIXIN_CLASS_MUST_HAVE_A_CONSTRUCTOR_WITH_A_SINGLE_REST_PARAMETER_OF_TYPE_ANY, &[]);
                    }
                }
                if !(static_base_type
                    .symbol()
                    .is_some_and(|s| s.flags().intersects(SymbolFlags::Class)))
                    && !base_constructor_type.flags().intersects(TypeFlags::TypeVariable)
                {
                    // When the static base type is a "class-like" constructor function (but not actually a class), we verify
                    // that all instantiated base constructor signatures return the same type.
                    let constructors = self.get_instantiated_constructors_for_type_arguments(
                        static_base_type,
                        base_type_node.type_arguments(),
                        base_type_node,
                    );
                    let mut every = true;
                    for sig in constructors {
                        let return_type = self.get_return_type_of_signature(sig);
                        if !self.is_type_identical_to(return_type, base_type) {
                            every = false;
                            break;
                        }
                    }
                    if !every {
                        self.error(
                            base_type_node.expression(),
                            &diagnostics::BASE_CONSTRUCTORS_MUST_ALL_HAVE_THE_SAME_RETURN_TYPE,
                            &[],
                        );
                    }
                }
                self.check_kinds_of_property_member_overrides(class_type, base_type);
            }
        }
        self.check_members_for_override_modifier(node, class_type, type_with_this, static_type);
        let implemented_type_nodes = get_implements_heritage_clause_elements(node);
        for &type_ref_node in implemented_type_nodes {
            if is_expression_with_type_arguments(type_ref_node) {
                let expr = type_ref_node.expression().unwrap();
                if !is_entity_name_expression(expr) || is_optional_chain(expr) {
                    self.error(Some(expr), &diagnostics::A_CLASS_CAN_ONLY_IMPLEMENT_AN_IDENTIFIER_SLASHQUALIFIED_NAME_WITH_OPTIONAL_TYPE_ARGUMENTS, &[]);
                }
            }
            self.check_type_reference_node(type_ref_node);
            let t = self.get_type_from_type_node(type_ref_node);
            let t = self.get_reduced_type(t);
            if !self.is_error_type(t) {
                if self.is_valid_base_type(t) {
                    let generic_diag = if t
                        .symbol()
                        .is_some_and(|s| s.flags().intersects(SymbolFlags::Class))
                    {
                        &diagnostics::CLASS_0_INCORRECTLY_IMPLEMENTS_CLASS_1_DID_YOU_MEAN_TO_EXTEND_1_AND_INHERIT_ITS_MEMBERS_AS_A_SUBCLASS
                    } else {
                        &diagnostics::CLASS_0_INCORRECTLY_IMPLEMENTS_INTERFACE_1
                    };
                    let base_with_this = self.get_type_with_this_argument(
                        t,
                        class_type.as_interface_type().this_type.get(),
                        false,
                    );
                    if !self.check_type_assignable_to(type_with_this, base_with_this, None, None) {
                        self.issue_member_specific_error(
                            node,
                            type_with_this,
                            base_with_this,
                            generic_diag,
                        );
                    }
                } else {
                    self.error(Some(type_ref_node), &diagnostics::A_CLASS_CAN_ONLY_IMPLEMENT_AN_OBJECT_TYPE_OR_INTERSECTION_OF_OBJECT_TYPES_WITH_STATICALLY_KNOWN_MEMBERS, &[]);
                }
            }
        }
        self.check_index_constraints(class_type, symbol, false /*isStaticIndex*/);
        self.check_index_constraints(static_type, symbol, true /*isStaticIndex*/);
        self.check_class_or_interface_for_duplicate_index_signatures(node);
        self.check_property_initialization(node);
    }

    // checker.go:4450
    fn check_class_for_static_property_name_conflicts(&mut self, node: P<Node>) {
        if self.compiler_options.get_use_define_for_class_fields() {
            return;
        }
        for &member in node.members() {
            let member_name_node = member.name();
            let is_static_member = ast::is_static(member);
            if is_static_member && let Some(member_name_node) = member_name_node {
                let (member_name, _) =
                    self.get_effective_property_name_for_property_name_node(member_name_node);
                match member_name.as_str() {
                    "name" | "length" | "caller" | "arguments" => {
                        let class_symbol = self.get_symbol_of_declaration(node).unwrap();
                        let class_name = self.symbol_to_string(class_symbol);
                        self.error(
                                Some(member_name_node),
                                &diagnostics::STATIC_PROPERTY_0_CONFLICTS_WITH_BUILT_IN_PROPERTY_FUNCTION_0_OF_CONSTRUCTOR_FUNCTION_1,
                                &[&member_name, &class_name],
                            );
                    }
                    _ => {}
                }
            }
        }
    }

    // Check that type parameter lists are identical across multiple declarations
    // checker.go:4473
    fn check_type_parameter_lists_identical(&mut self, symbol: P<Symbol>) {
        if symbol.declarations().len() == 1 {
            return;
        }
        let links = self.declared_type_links.get(symbol);
        if !links.type_parameters_checked.get() {
            links.type_parameters_checked.set(true);
            let declarations = Self::get_class_or_interface_declarations_of_symbol(symbol);
            if declarations.len() <= 1 {
                return;
            }
            let t = self.get_declared_type_of_symbol(symbol);
            if !self.are_type_parameters_identical(
                &declarations,
                t.as_interface_type().local_type_parameters(),
                |_, n| n.type_parameters().to_vec(),
            ) {
                // Report an error on every conflicting declaration.
                let name = self.symbol_to_string(symbol);
                for declaration in declarations {
                    self.error(
                        declaration.name(),
                        &diagnostics::ALL_DECLARATIONS_OF_0_MUST_HAVE_IDENTICAL_TYPE_PARAMETERS,
                        &[&name],
                    );
                }
            }
        }
    }

    // checker.go:4495
    fn get_class_or_interface_declarations_of_symbol(symbol: P<Symbol>) -> Vec<P<Node>> {
        symbol
            .declarations()
            .iter()
            .copied()
            .filter(|&d| is_class_declaration(d) || is_interface_declaration(d))
            .collect()
    }

    // checker.go:4501
    pub(crate) fn are_type_parameters_identical(
        &mut self,
        declarations: &[P<Node>],
        target_parameters: &[P<Type>],
        mut get_type_parameter_declarations: impl FnMut(&mut Checker, P<Node>) -> Vec<P<Node>>,
    ) -> bool {
        let max_type_argument_count = target_parameters.len();
        let min_type_argument_count = Self::get_min_type_argument_count(target_parameters);
        for &declaration in declarations {
            // If this declaration has too few or too many type parameters, we report an error
            let source_parameters = get_type_parameter_declarations(self, declaration);
            if source_parameters.len() < min_type_argument_count
                || source_parameters.len() > max_type_argument_count
            {
                return false;
            }
            if source_parameters.iter().enumerate().any(|(i, &source)| {
                let target = target_parameters[i];
                // If the type parameter node does not have the same name as the resolved type
                // parameter at this position, we report an error.
                if source.name().unwrap().text() != target.symbol().unwrap().name() {
                    return true;
                }
                // If the type parameter node does not have an identical constraintNode as the resolved
                // type parameter at this position, we report an error.
                let constraint_node = source.as_type_parameter_declaration().constraint;
                let target_constraint = self.get_constraint_of_type_parameter(target);
                // relax check if later interface augmentation has no constraint, it's more broad and is OK to merge with
                // a more constrained interface (this could be generalized to a full hierarchy check, but that's maybe overkill)
                if let (Some(constraint_node), Some(target_constraint)) =
                    (constraint_node, target_constraint)
                {
                    let constraint_type = self.get_type_from_type_node(constraint_node);
                    if !self.is_type_identical_to(constraint_type, target_constraint) {
                        return true;
                    }
                }
                // If the type parameter node has a default and it is not identical to the default
                // for the type parameter at this position, we report an error.
                let default_node = source.as_type_parameter_declaration().default_type;
                let target_default = self.get_default_from_type_parameter(target);
                if let (Some(default_node), Some(target_default)) = (default_node, target_default) {
                    let default_type = self.get_type_from_type_node(default_node);
                    if !self.is_type_identical_to(default_type, target_default) {
                        return true;
                    }
                }

                false
            }) {
                return false;
            }
        }
        true
    }

    // checker.go:4538
    fn check_base_type_accessibility(&mut self, t: P<Type>, node: P<Node>) {
        let signatures = self.get_signatures_of_type(t, SignatureKind::Construct);
        let accessibility_error =
            self.get_constructor_accessibility_error(node, signatures, ModifierFlags::Private);
        if let Some(accessibility_error) = accessibility_error {
            let name = self.get_fully_qualified_name(
                accessibility_error.declaring_class.symbol().unwrap(),
                None,
            );
            self.error(
                Some(node),
                &diagnostics::CANNOT_EXTEND_A_CLASS_0_CLASS_CONSTRUCTOR_IS_MARKED_AS_PRIVATE,
                &[&name],
            );
        }
    }

    // checker.go:4546
    fn issue_member_specific_error(
        &mut self,
        node: P<Node>,
        type_with_this: P<Type>,
        base_with_this: P<Type>,
        broad_diag: &'static Message,
    ) {
        // iterate over all implemented properties and issue errors on each one which isn't compatible, rather than the class as a whole, if possible
        let mut issued_member_error = false;
        for &member in node.members() {
            if ast::is_static(member) {
                continue;
            }
            if let Some(declared_prop) = self.get_symbol_of_declaration(member)
                && declared_prop.name() != INTERNAL_SYMBOL_NAME_COMPUTED
            {
                let prop = self.get_property_of_type(type_with_this, declared_prop.name());
                let base_prop = self.get_property_of_type(base_with_this, declared_prop.name());
                if let (Some(prop), Some(base_prop)) = (prop, base_prop) {
                    let mut diags: Vec<P<Diagnostic>> = Vec::new();
                    let prop_type = self.get_type_of_symbol(prop);
                    let base_prop_type = self.get_type_of_symbol(base_prop);
                    if !self.check_type_assignable_to_ex(
                        prop_type,
                        base_prop_type,
                        Some(member.name().unwrap_or(member)),
                        None, /*headMessage*/
                        &mut diags,
                    ) {
                        let a0 = self.symbol_to_string(declared_prop);
                        let a1 = self.type_to_string(type_with_this, None);
                        let a2 = self.type_to_string(base_with_this, None);
                        self.add_diagnostic(ast::new_diagnostic_chain(diags[0], &diagnostics::PROPERTY_0_IN_TYPE_1_IS_NOT_ASSIGNABLE_TO_THE_SAME_PROPERTY_IN_BASE_TYPE_2, &[&a0, &a1, &a2]));
                        issued_member_error = true;
                    }
                }
            }
        }
        if !issued_member_error {
            // check again with diagnostics to generate a less-specific error
            self.check_type_assignable_to(
                type_with_this,
                base_with_this,
                Some(node.name().unwrap_or(node)),
                Some(broad_diag),
            );
        }
    }

    // checker.go:4571
    fn get_type_without_signatures(&mut self, t: P<Type>) -> P<Type> {
        if t.flags().intersects(TypeFlags::Object) {
            let resolved = self.resolve_structured_type_members(t);
            if !resolved.signatures().is_empty() {
                let result = self.new_object_type(ObjectFlags::Anonymous, t.symbol());
                result
                    .object_flags
                    .set(result.object_flags.get_lazy() | ObjectFlags::MembersResolved);
                result.as_object_type().set_members(resolved.members());
                result.as_object_type().set_properties(resolved.properties());
                return result;
            }
        } else if t.flags().intersects(TypeFlags::Intersection) {
            let types: Vec<P<Type>> = t
                .as_intersection_type()
                .types()
                .iter()
                .map(|&t| self.get_type_without_signatures(t))
                .collect();
            return self.get_intersection_type(&types);
        }
        t
    }

    // checker.go:4588
    fn check_kinds_of_property_member_overrides(&mut self, t: P<Type>, base_type: P<Type>) {
        // TypeScript 1.0 spec (April 2014): 8.2.3
        // A derived class inherits all members from its base class it doesn't override.
        // Inheritance means that a derived class implicitly contains all non - overridden members of the base class.
        // Both public and private property members are inherited, but only public property members can be overridden.
        // A property member in a derived class is said to override a property member in a base class
        // when the derived class property member has the same name and kind(instance or static)
        // as the base class property member.
        // The type of an overriding property member must be assignable(section 3.8.4)
        // to the type of the overridden property member, or otherwise a compile - time error occurs.
        // Base class instance member functions can be overridden by derived class instance member functions,
        // but not by other kinds of members.
        // Base class instance member variables and accessors can be overridden by
        // derived class instance member variables and accessors, but not by other kinds of members.
        // NOTE: assignability is checked in checkClassDeclaration
        #[derive(Clone, Default)]
        struct MemberInfo {
            missed_properties: Vec<String>,
            base_type_name: String,
            type_name: String,
        }
        // Go uses a map keyed by the derived class declaration (which may be nil); iteration order is irrelevant in
        // practice because the key is always t.symbol's class declaration. An insertion-ordered map keeps it deterministic.
        let mut not_implemented_info: Option<collections::OrderedMap<Option<P<Node>>, MemberInfo>> =
            None;
        'base_property_check: for base_property in
            self.get_properties_of_type(base_type).iter().copied()
        {
            let base = self.get_target_symbol(base_property).unwrap();
            if base.flags().intersects(SymbolFlags::Prototype) {
                continue;
            }
            let Some(base_symbol) = self.get_property_of_object_type(t, base.name()) else {
                continue;
            };
            let derived = self.get_target_symbol(base_symbol).unwrap();
            let base_declaration_flags = get_declaration_modifier_flags_from_symbol(base);
            // In order to resolve whether the inherited method was overridden in the base class or not,
            // we compare the Symbols obtained. Since getTargetSymbol returns the symbol on the *uninstantiated*
            // type declaration, derived and base resolve to the same symbol even in the case of generic classes.
            if derived == base {
                // derived class inherits base without override/redeclaration.
                if base_declaration_flags.intersects(ModifierFlags::Abstract) {
                    // It is an error to inherit an abstract member without implementing it or being declared abstract.
                    // If there is no declaration for the derived class (as in the case of class expressions),
                    // then the class cannot be declared abstract.
                    let derived_class_decl =
                        get_class_like_declaration_of_symbol(t.symbol().unwrap());
                    if derived_class_decl.is_none()
                        || !has_syntactic_modifier(
                            derived_class_decl.unwrap(),
                            ModifierFlags::Abstract,
                        )
                    {
                        // Searches other base types for a declaration that would satisfy the inherited abstract member.
                        // (The class may have more than one base type via declaration merging with an interface with the
                        // same name.)
                        for &other_base_type in self.get_base_types(t) {
                            if other_base_type == base_type {
                                continue;
                            }
                            if let Some(base_symbol) =
                                self.get_property_of_object_type(other_base_type, base.name())
                                && Some(base) != self.get_target_symbol(base_symbol)
                            {
                                // Derived property exists elsewhere.
                                continue 'base_property_check;
                            }
                        }
                        let base_type_name = self.type_to_string(base_type, None);
                        let type_name = self.type_to_string(t, None);
                        let mut missed_properties = not_implemented_info
                            .as_ref()
                            .and_then(|m| m.get(&derived_class_decl))
                            .map(|info| info.missed_properties.clone())
                            .unwrap_or_default();
                        missed_properties.push(self.symbol_to_string(base_property));
                        let map = not_implemented_info.get_or_insert_with(Default::default);
                        map.insert(
                            derived_class_decl,
                            MemberInfo { missed_properties, base_type_name, type_name },
                        );
                    }
                }
            } else {
                // derived overrides base.
                let derived_declaration_flags = get_declaration_modifier_flags_from_symbol(derived);
                if base_declaration_flags.intersects(ModifierFlags::Private)
                    || derived_declaration_flags.intersects(ModifierFlags::Private)
                {
                    // either base or derived property is private - not override, skip it
                    continue;
                }

                let base_property_flags = base.flags() & SymbolFlags::PropertyOrAccessor;
                let derived_property_flags = derived.flags() & SymbolFlags::PropertyOrAccessor;
                let error_message: &'static Message = if !base_property_flags.is_empty()
                    && !derived_property_flags.is_empty()
                {
                    // property/accessor is overridden with property/accessor
                    if base.check_flags().intersects(CheckFlags::Mapped)
                        || derived.value_declaration().is_some_and(is_binary_expression)
                        || Self::are_properties_abstract_or_interface(base, base_declaration_flags)
                    {
                        // when the base property is abstract or from an interface, base/derived flags don't need to match
                        // for intersection properties, this must be true of *any* of the declarations, for others it must be true of *all*
                        // same when the derived property is from an assignment
                        continue;
                    }
                    let overridden_instance_property = base_property_flags != SymbolFlags::Property
                        && derived_property_flags == SymbolFlags::Property;
                    let overridden_instance_accessor = base_property_flags == SymbolFlags::Property
                        && derived_property_flags != SymbolFlags::Property;
                    if overridden_instance_property || overridden_instance_accessor {
                        let error_message = if overridden_instance_property {
                            &diagnostics::X_0_IS_DEFINED_AS_AN_ACCESSOR_IN_CLASS_1_BUT_IS_OVERRIDDEN_HERE_IN_2_AS_AN_INSTANCE_PROPERTY
                        } else {
                            &diagnostics::X_0_IS_DEFINED_AS_A_PROPERTY_IN_CLASS_1_BUT_IS_OVERRIDDEN_HERE_IN_2_AS_AN_ACCESSOR
                        };
                        let a0 = self.symbol_to_string(base);
                        let a1 = self.type_to_string(base_type, None);
                        let a2 = self.type_to_string(t, None);
                        self.error(
                            get_name_of_declaration(derived.value_declaration())
                                .or(derived.value_declaration()),
                            error_message,
                            &[&a0, &a1, &a2],
                        );
                    } else if self.compiler_options.get_use_define_for_class_fields() {
                        let uninitialized = derived
                            .declarations()
                            .iter()
                            .copied()
                            .find(|&d| is_property_declaration(d) && d.initializer().is_none());
                        if let Some(uninitialized) = uninitialized
                            && !derived.flags().intersects(SymbolFlags::Transient)
                            && !base_declaration_flags.intersects(ModifierFlags::Abstract)
                            && !derived_declaration_flags.intersects(ModifierFlags::Abstract)
                            && !derived
                                .declarations()
                                .iter()
                                .any(|d| d.flags().intersects(NodeFlags::Ambient))
                        {
                            let constructor = find_constructor_declaration(
                                get_class_like_declaration_of_symbol(t.symbol().unwrap()).unwrap(),
                            );
                            let prop_name = uninitialized.name().unwrap();
                            if is_exclamation_token(uninitialized.postfix_token())
                                || constructor.is_none()
                                || !is_identifier(prop_name)
                                || !self.strict_null_checks
                                || !self.is_property_initialized_in_constructor(
                                    prop_name,
                                    t,
                                    constructor.unwrap(),
                                )
                            {
                                let error_message = &diagnostics::PROPERTY_0_WILL_OVERWRITE_THE_BASE_PROPERTY_IN_1_IF_THIS_IS_INTENTIONAL_ADD_AN_INITIALIZER_OTHERWISE_ADD_A_DECLARE_MODIFIER_OR_REMOVE_THE_REDUNDANT_DECLARATION;
                                let a0 = self.symbol_to_string(base);
                                let a1 = self.type_to_string(base_type, None);
                                self.error(
                                    get_name_of_declaration(derived.value_declaration())
                                        .or(derived.value_declaration()),
                                    error_message,
                                    &[&a0, &a1],
                                );
                            }
                        }
                    }
                    // correct case
                    continue;
                } else if is_prototype_property(base) {
                    if is_prototype_property(derived)
                        || derived.flags().intersects(SymbolFlags::Property)
                    {
                        // method is overridden with method or property -- correct case
                        continue;
                    }
                    &diagnostics::CLASS_0_DEFINES_INSTANCE_MEMBER_FUNCTION_1_BUT_EXTENDED_CLASS_2_DEFINES_IT_AS_INSTANCE_MEMBER_ACCESSOR
                } else if base.flags().intersects(SymbolFlags::Accessor) {
                    &diagnostics::CLASS_0_DEFINES_INSTANCE_MEMBER_ACCESSOR_1_BUT_EXTENDED_CLASS_2_DEFINES_IT_AS_INSTANCE_MEMBER_FUNCTION
                } else {
                    &diagnostics::CLASS_0_DEFINES_INSTANCE_MEMBER_PROPERTY_1_BUT_EXTENDED_CLASS_2_DEFINES_IT_AS_INSTANCE_MEMBER_FUNCTION
                };
                let a0 = self.type_to_string(base_type, None);
                let a1 = self.symbol_to_string(base);
                let a2 = self.type_to_string(t, None);
                self.error(
                    get_name_of_declaration(derived.value_declaration())
                        .or(derived.value_declaration()),
                    error_message,
                    &[&a0, &a1, &a2],
                );
            }
        }
        for (error_node, member_info) in not_implemented_info.unwrap_or_default() {
            if member_info.missed_properties.len() == 1 {
                let missed_property = &member_info.missed_properties[0];
                if is_class_expression(error_node.unwrap()) {
                    self.error(error_node, &diagnostics::NON_ABSTRACT_CLASS_EXPRESSION_DOES_NOT_IMPLEMENT_INHERITED_ABSTRACT_MEMBER_0_FROM_CLASS_1, &[missed_property, &member_info.base_type_name]);
                } else {
                    self.error(error_node, &diagnostics::NON_ABSTRACT_CLASS_0_DOES_NOT_IMPLEMENT_INHERITED_ABSTRACT_MEMBER_1_FROM_CLASS_2, &[&member_info.type_name, missed_property, &member_info.base_type_name]);
                }
            } else if member_info.missed_properties.len() > 5 {
                let items: Vec<&str> = member_info.missed_properties[..4]
                    .iter()
                    .map(std::string::String::as_str)
                    .collect();
                let missed_properties = quoted_and_comma_separated(&items);
                let remaining_missed_properties = member_info.missed_properties.len() - 4;
                if is_class_expression(error_node.unwrap()) {
                    self.error(error_node, &diagnostics::NON_ABSTRACT_CLASS_EXPRESSION_IS_MISSING_IMPLEMENTATIONS_FOR_THE_FOLLOWING_MEMBERS_OF_0_COLON_1_AND_2_MORE, &[&member_info.base_type_name, &missed_properties, &remaining_missed_properties]);
                } else {
                    self.error(error_node, &diagnostics::NON_ABSTRACT_CLASS_0_IS_MISSING_IMPLEMENTATIONS_FOR_THE_FOLLOWING_MEMBERS_OF_1_COLON_2_AND_3_MORE, &[&member_info.type_name, &member_info.base_type_name, &missed_properties, &remaining_missed_properties]);
                }
            } else {
                let items: Vec<&str> =
                    member_info.missed_properties.iter().map(std::string::String::as_str).collect();
                let missed_properties = quoted_and_comma_separated(&items);
                if is_class_expression(error_node.unwrap()) {
                    self.error(error_node, &diagnostics::NON_ABSTRACT_CLASS_EXPRESSION_IS_MISSING_IMPLEMENTATIONS_FOR_THE_FOLLOWING_MEMBERS_OF_0_COLON_1, &[&member_info.base_type_name, &missed_properties]);
                } else {
                    self.error(error_node, &diagnostics::NON_ABSTRACT_CLASS_0_IS_MISSING_IMPLEMENTATIONS_FOR_THE_FOLLOWING_MEMBERS_OF_1_COLON_2, &[&member_info.type_name, &member_info.base_type_name, &missed_properties]);
                }
            }
        }
    }

    // checker.go:4744
    fn are_properties_abstract_or_interface(
        base: P<Symbol>,
        base_declaration_flags: ModifierFlags,
    ) -> bool {
        let declarations = base.declarations();
        if base.check_flags().intersects(CheckFlags::Synthetic) {
            return declarations
                .iter()
                .any(|&d| Self::is_property_abstract_or_interface(d, base_declaration_flags));
        }
        declarations
            .iter()
            .all(|&d| Self::is_property_abstract_or_interface(d, base_declaration_flags))
    }

    // checker.go:4751
    fn is_property_abstract_or_interface(
        declaration: P<Node>,
        base_declaration_flags: ModifierFlags,
    ) -> bool {
        is_interface_declaration(declaration.parent().unwrap())
            || base_declaration_flags.intersects(ModifierFlags::Abstract)
                && (!is_property_declaration(declaration) || declaration.initializer().is_none())
    }

    // checker.go:4756
    fn check_members_for_override_modifier(
        &mut self,
        node: P<Node>,
        t: P<Type>,
        type_with_this: P<Type>,
        static_type: P<Type>,
    ) {
        let mut base_with_this: Option<P<Type>> = None;
        let base_type_node = get_class_extends_heritage_element(node);
        if base_type_node.is_some() {
            let base_types = self.get_base_types(t);
            if !base_types.is_empty() {
                base_with_this = Some(self.get_type_with_this_argument(
                    base_types[0],
                    t.as_interface_type().this_type.get(),
                    false,
                ));
            }
        }
        let base_static_type = self.get_base_constructor_type_of_class(t);
        for &member in node.members() {
            if !has_ambient_modifier(member) {
                if is_constructor_declaration(member) {
                    for &param in member.parameters() {
                        if is_parameter_property_declaration(param, member) {
                            self.check_member_for_override_modifier(
                                node,
                                static_type,
                                base_static_type,
                                base_with_this,
                                t,
                                type_with_this,
                                param,
                            );
                        }
                    }
                } else {
                    self.check_member_for_override_modifier(
                        node,
                        static_type,
                        base_static_type,
                        base_with_this,
                        t,
                        type_with_this,
                        member,
                    );
                }
            }
        }
    }

    // checker.go:4781
    fn check_member_for_override_modifier(
        &mut self,
        node: P<Node>,
        static_type: P<Type>,
        base_static_type: P<Type>,
        base_with_this: Option<P<Type>>,
        t: P<Type>,
        type_with_this: P<Type>,
        member: P<Node>,
    ) {
        let Some(symbol) = self.get_symbol_of_declaration(member) else {
            return;
        };

        self.check_member_for_override_modifier_worker(
            node,
            static_type,
            base_static_type,
            base_with_this,
            t,
            type_with_this,
            has_override_modifier(member),
            has_abstract_modifier(member),
            ast::is_static(member),
            is_parameter_declaration(member),
            symbol,
            Some(member),
        );
    }

    // checker.go:4790
    pub(crate) fn get_member_override_modifier_status(
        &mut self,
        node: P<Node>,
        member: P<Node>,
        member_symbol: Option<P<Symbol>>,
    ) -> MemberOverrideStatus {
        let Some(member_symbol) = member_symbol else {
            return MemberOverrideStatus::None;
        };
        if member.name().is_none() {
            return MemberOverrideStatus::None;
        }

        let Some(class_symbol) = self.get_symbol_of_declaration(node) else {
            return MemberOverrideStatus::None;
        };

        let t = self.get_declared_type_of_symbol(class_symbol);
        let type_with_this = self.get_type_with_this_argument(t, None, false);
        let static_type = self.get_type_of_symbol(class_symbol);

        let mut base_with_this: Option<P<Type>> = None;
        if get_class_extends_heritage_element(node).is_some() {
            let base_types = self.get_base_types(t);
            if !base_types.is_empty() {
                base_with_this = Some(self.get_type_with_this_argument(
                    base_types[0],
                    t.as_interface_type().this_type.get(),
                    false,
                ));
            }
        }

        let base_static_type = self.get_base_constructor_type_of_class(t);
        self.check_member_for_override_modifier_worker(
            node,
            static_type,
            base_static_type,
            base_with_this,
            t,
            type_with_this,
            has_syntactic_modifier(member, ModifierFlags::Override),
            has_abstract_modifier(member),
            ast::is_static(member),
            false, /*memberIsParameterProperty*/
            member_symbol,
            None, /*errorNode*/
        )
    }

    // checker.go:4815
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "The boolean parameters preserve the upstream Go function signature."
    )]
    fn check_member_for_override_modifier_worker(
        &mut self,
        node: P<Node>,
        static_type: P<Type>,
        base_static_type: P<Type>,
        base_with_this: Option<P<Type>>,
        t: P<Type>,
        type_with_this: P<Type>,
        member_has_override_modifier: bool,
        member_has_abstract_modifier: bool,
        member_is_static: bool,
        member_is_parameter_property: bool,
        member: P<Symbol>,
        error_node: Option<P<Node>>,
    ) -> MemberOverrideStatus {
        let is_js = is_in_js_file(node);
        if member_has_override_modifier
            && let Some(value_declaration) = member.value_declaration()
            && is_class_element(value_declaration)
            && let Some(name) = value_declaration.name()
            && self.is_non_bindable_dynamic_name(name)
        {
            if error_node.is_some() {
                self.error(error_node, if is_js { &diagnostics::THIS_MEMBER_CANNOT_HAVE_A_JSDOC_COMMENT_WITH_AN_OVERRIDE_TAG_BECAUSE_ITS_NAME_IS_DYNAMIC } else { &diagnostics::THIS_MEMBER_CANNOT_HAVE_AN_OVERRIDE_MODIFIER_BECAUSE_ITS_NAME_IS_DYNAMIC }, &[]);
            }
            return MemberOverrideStatus::HasInvalidOverride;
        }

        if let Some(base_with_this) = base_with_this.filter(|_| {
            member_has_override_modifier || self.compiler_options.no_implicit_override.is_true()
        }) {
            let this_type = if member_is_static { static_type } else { type_with_this };
            let base_type = if member_is_static { base_static_type } else { base_with_this };
            let prop = self.get_property_of_type(this_type, member.name());
            let base_prop = self.get_property_of_type(base_type, member.name());

            if prop.is_some() && base_prop.is_none() && member_has_override_modifier {
                if error_node.is_some() {
                    let suggestion = self.get_suggested_symbol_for_nonexistent_class_member(
                        symbol_name(member),
                        base_type,
                    );
                    if let Some(suggestion) = suggestion {
                        let a0 = self.type_to_string(base_with_this, None);
                        let a1 = self.symbol_to_string(suggestion);
                        self.error(error_node, if is_js { &diagnostics::THIS_MEMBER_CANNOT_HAVE_A_JSDOC_COMMENT_WITH_AN_OVERRIDE_TAG_BECAUSE_IT_IS_NOT_DECLARED_IN_THE_BASE_CLASS_0_DID_YOU_MEAN_1 } else { &diagnostics::THIS_MEMBER_CANNOT_HAVE_AN_OVERRIDE_MODIFIER_BECAUSE_IT_IS_NOT_DECLARED_IN_THE_BASE_CLASS_0_DID_YOU_MEAN_1 }, &[&a0, &a1]);
                    } else {
                        let a0 = self.type_to_string(base_with_this, None);
                        self.error(error_node, if is_js { &diagnostics::THIS_MEMBER_CANNOT_HAVE_A_JSDOC_COMMENT_WITH_AN_OVERRIDE_TAG_BECAUSE_IT_IS_NOT_DECLARED_IN_THE_BASE_CLASS_0 } else { &diagnostics::THIS_MEMBER_CANNOT_HAVE_AN_OVERRIDE_MODIFIER_BECAUSE_IT_IS_NOT_DECLARED_IN_THE_BASE_CLASS_0 }, &[&a0]);
                    }
                }
                return MemberOverrideStatus::HasInvalidOverride;
            }

            if let (Some(_), Some(base_prop)) = (prop, base_prop)
                && !base_prop.declarations().is_empty()
                && self.compiler_options.no_implicit_override.is_true()
                && !node.flags().intersects(NodeFlags::Ambient)
            {
                let base_has_abstract =
                    base_prop.declarations().iter().any(|&d| has_abstract_modifier(d));
                if member_has_override_modifier {
                    return MemberOverrideStatus::None;
                }
                if !base_has_abstract {
                    if error_node.is_some() {
                        let message = if member_is_parameter_property {
                            if is_js {
                                &diagnostics::THIS_PARAMETER_PROPERTY_MUST_HAVE_A_JSDOC_COMMENT_WITH_AN_OVERRIDE_TAG_BECAUSE_IT_OVERRIDES_A_MEMBER_IN_THE_BASE_CLASS_0
                            } else {
                                &diagnostics::THIS_PARAMETER_PROPERTY_MUST_HAVE_AN_OVERRIDE_MODIFIER_BECAUSE_IT_OVERRIDES_A_MEMBER_IN_BASE_CLASS_0
                            }
                        } else if is_js {
                            &diagnostics::THIS_MEMBER_MUST_HAVE_A_JSDOC_COMMENT_WITH_AN_OVERRIDE_TAG_BECAUSE_IT_OVERRIDES_A_MEMBER_IN_THE_BASE_CLASS_0
                        } else {
                            &diagnostics::THIS_MEMBER_MUST_HAVE_AN_OVERRIDE_MODIFIER_BECAUSE_IT_OVERRIDES_A_MEMBER_IN_THE_BASE_CLASS_0
                        };
                        let a0 = self.type_to_string(base_with_this, None);
                        self.error(error_node, message, &[&a0]);
                    }
                    return MemberOverrideStatus::NeedsOverride;
                }
                if member_has_abstract_modifier {
                    if error_node.is_some() {
                        let a0 = self.type_to_string(base_with_this, None);
                        self.error(error_node, &diagnostics::THIS_MEMBER_MUST_HAVE_AN_OVERRIDE_MODIFIER_BECAUSE_IT_OVERRIDES_AN_ABSTRACT_METHOD_THAT_IS_DECLARED_IN_THE_BASE_CLASS_0, &[&a0]);
                    }
                    return MemberOverrideStatus::NeedsOverride;
                }
            }
        } else if member_has_override_modifier {
            if error_node.is_some() {
                let a0 = self.type_to_string(t, None);
                self.error(error_node, if is_js { &diagnostics::THIS_MEMBER_CANNOT_HAVE_A_JSDOC_COMMENT_WITH_AN_OVERRIDE_TAG_BECAUSE_ITS_CONTAINING_CLASS_0_DOES_NOT_EXTEND_ANOTHER_CLASS } else { &diagnostics::THIS_MEMBER_CANNOT_HAVE_AN_OVERRIDE_MODIFIER_BECAUSE_ITS_CONTAINING_CLASS_0_DOES_NOT_EXTEND_ANOTHER_CLASS }, &[&a0]);
            }
            return MemberOverrideStatus::HasInvalidOverride;
        }

        MemberOverrideStatus::None
    }

    // checker.go:4873
    fn get_suggested_symbol_for_nonexistent_class_member(
        &mut self,
        name: &str,
        base_type: P<Type>,
    ) -> Option<P<Symbol>> {
        let properties = self.get_properties_of_type(base_type);
        self.get_spelling_suggestion_for_name(name, properties, SymbolFlags::ClassMember)
    }

    // checker.go:4877
    pub(crate) fn check_index_constraints(
        &mut self,
        t: P<Type>,
        symbol: P<Symbol>,
        is_static_index: bool,
    ) {
        let index_infos = self.get_index_infos_of_type(t);
        if index_infos.is_empty() {
            return;
        }
        for prop in self.get_properties_of_object_type(t).iter().copied() {
            if !(is_static_index && prop.flags().intersects(SymbolFlags::Prototype)) {
                let prop_name_type = self.get_literal_type_from_property(
                    prop,
                    TypeFlags::StringOrNumberLiteralOrUnique,
                    true, /*includeNonPublic*/
                );
                let prop_type = self.get_non_missing_type_of_symbol(prop);
                self.check_index_constraint_for_property(t, prop, prop_name_type, prop_type);
            }
        }
        let type_declaration = symbol.value_declaration();
        if let Some(type_declaration) = type_declaration.filter(|&d| is_class_like(d)) {
            for &member in type_declaration.members() {
                // Only process instance properties against instance index signatures and static properties against static index signatures
                if ast::is_static(member) == is_static_index && !self.has_bindable_name(member) {
                    let symbol = self.get_symbol_of_declaration(member).unwrap();
                    let prop_name_type =
                        self.get_type_of_expression(member.name().unwrap().expression().unwrap());
                    let prop_type = self.get_non_missing_type_of_symbol(symbol);
                    self.check_index_constraint_for_property(t, symbol, prop_name_type, prop_type);
                }
            }
        }
        if index_infos.len() > 1 {
            for &info in index_infos {
                self.check_index_constraint_for_index_signature(t, info);
            }
        }
    }

    // checker.go:4904
    fn check_index_constraint_for_property(
        &mut self,
        t: P<Type>,
        prop: P<Symbol>,
        prop_name_type: P<Type>,
        prop_type: P<Type>,
    ) {
        let declaration = prop.value_declaration();
        let name = get_name_of_declaration(declaration);
        if name.is_some_and(is_private_identifier) {
            return;
        }
        let index_infos = self.get_applicable_index_infos(t, prop_name_type);
        if index_infos.is_empty() {
            return;
        }
        let mut interface_declaration: Option<P<Node>> = None;
        if t.object_flags().intersects(ObjectFlags::Interface) {
            interface_declaration =
                get_declaration_of_kind(t.symbol().unwrap(), Kind::InterfaceDeclaration);
        }
        let mut prop_declaration: Option<P<Node>> = None;
        if declaration.is_some_and(is_binary_expression)
            || name.is_some_and(is_computed_property_name)
        {
            prop_declaration = declaration;
        }
        let mut local_prop_declaration: Option<P<Node>> = None;
        if self.get_parent_of_symbol(prop) == t.symbol() {
            local_prop_declaration = declaration;
        }
        for info in index_infos {
            let mut local_index_declaration: Option<P<Node>> = None;
            if let Some(info_declaration) = info.declaration() {
                let info_symbol = self.get_symbol_of_declaration(info_declaration).unwrap();
                if self.get_parent_of_symbol(info_symbol) == t.symbol() {
                    local_index_declaration = Some(info_declaration);
                }
            }
            // We check only when (a) the property is declared in the containing type, or (b) the applicable index signature is declared
            // in the containing type, or (c) the containing type is an interface and no base interface contains both the property and
            // the index signature (i.e. property and index signature are declared in separate inherited interfaces).
            let mut error_node = local_prop_declaration.or(local_index_declaration);
            if error_node.is_none() && interface_declaration.is_some() {
                let some = self.get_base_types(t).iter().any(|&base| {
                    self.get_property_of_object_type(base, prop.name()).is_some()
                        && self.get_index_type_of_type(base, info.key_type()).is_some()
                });
                if !some {
                    error_node = interface_declaration;
                }
            }
            if let Some(error_node) = error_node
                && !self.is_type_assignable_to(prop_type, info.value_type())
            {
                let a0 = self.symbol_to_string(prop);
                let a1 = self.type_to_string(prop_type, None);
                let a2 = self.type_to_string(info.key_type(), None);
                let a3 = self.type_to_string(info.value_type(), None);
                let diagnostic = new_diagnostic_for_node(
                    Some(error_node),
                    Some(&diagnostics::PROPERTY_0_OF_TYPE_1_IS_NOT_ASSIGNABLE_TO_2_INDEX_TYPE_3),
                    &[&a0, &a1, &a2, &a3],
                );
                if let Some(prop_declaration) = prop_declaration.filter(|&d| d != error_node) {
                    let a0 = self.symbol_to_string(prop);
                    diagnostic.add_related_info(new_diagnostic_for_node(
                        Some(prop_declaration),
                        Some(&diagnostics::X_0_IS_DECLARED_HERE),
                        &[&a0],
                    ));
                }
                self.add_diagnostic(diagnostic);
            }
        }
    }

    // checker.go:4950
    fn check_index_constraint_for_index_signature(&mut self, t: P<Type>, check_info: P<IndexInfo>) {
        let declaration = check_info.declaration();
        let index_infos = self.get_applicable_index_infos(t, check_info.key_type());
        if index_infos.is_empty() {
            return;
        }
        let mut interface_declaration: Option<P<Node>> = None;
        if t.object_flags().intersects(ObjectFlags::Interface) {
            interface_declaration =
                get_declaration_of_kind(t.symbol().unwrap(), Kind::InterfaceDeclaration);
        }
        let mut local_check_declaration: Option<P<Node>> = None;
        if let Some(declaration) = declaration {
            let declaration_symbol = self.get_symbol_of_declaration(declaration).unwrap();
            if self.get_parent_of_symbol(declaration_symbol) == t.symbol() {
                local_check_declaration = Some(declaration);
            }
        }
        for info in index_infos {
            if info == check_info {
                continue;
            }
            let mut local_index_declaration: Option<P<Node>> = None;
            if let Some(info_declaration) = info.declaration() {
                let info_symbol = self.get_symbol_of_declaration(info_declaration).unwrap();
                if self.get_parent_of_symbol(info_symbol) == t.symbol() {
                    local_index_declaration = Some(info_declaration);
                }
            }
            // We check only when (a) the check index signature is declared in the containing type, or (b) the applicable index
            // signature is declared in the containing type, or (c) the containing type is an interface and no base interface contains
            // both index signatures (i.e. the index signatures are declared in separate inherited interfaces).
            let mut error_node = local_check_declaration.or(local_index_declaration);
            if error_node.is_none() && interface_declaration.is_some() {
                let some = self.get_base_types(t).iter().any(|&base| {
                    self.get_index_info_of_type(base, check_info.key_type()).is_some()
                        && self.get_index_type_of_type(base, info.key_type()).is_some()
                });
                if !some {
                    error_node = interface_declaration;
                }
            }
            if error_node.is_some()
                && !self.is_type_assignable_to(check_info.value_type(), info.value_type())
            {
                let a0 = self.type_to_string(check_info.key_type(), None);
                let a1 = self.type_to_string(check_info.value_type(), None);
                let a2 = self.type_to_string(info.key_type(), None);
                let a3 = self.type_to_string(info.value_type(), None);
                self.error(
                    error_node,
                    &diagnostics::X_0_INDEX_TYPE_1_IS_NOT_ASSIGNABLE_TO_2_INDEX_TYPE_3,
                    &[&a0, &a1, &a2, &a3],
                );
            }
        }
    }

    // checker.go:4987
    fn check_class_or_interface_for_duplicate_index_signatures(&mut self, node: P<Node>) {
        // Only check the type once
        let symbol = self.get_symbol_of_declaration(node).unwrap();
        let links = self.declared_type_links.get(symbol);
        if !links.index_signatures_checked.get() {
            links.index_signatures_checked.set(true);
            self.check_type_for_duplicate_index_signatures(node);
        }
    }

    // checker.go:4995
    pub(crate) fn check_type_for_duplicate_index_signatures(&mut self, node: P<Node>) {
        // TypeScript 1.0 spec (April 2014)
        // 3.7.4: An object type can contain at most one string index signature and one numeric index signature.
        // 8.5: A class declaration can have at most one string index member declaration and one numeric index member declaration
        let symbol = self.get_symbol_of_declaration(node).unwrap();
        let index_symbol = self.get_index_symbol(symbol);
        let Some(index_symbol) = index_symbol.filter(|s| s.declarations().len() > 1) else {
            return;
        };
        // Go iterates a map here (random order); an insertion-ordered map keeps diagnostics deterministic.
        let mut index_signature_map: collections::OrderedMap<P<Type>, Vec<P<Node>>> =
            tsrs_core::collections::OrderedMap::default();
        let declarations = index_symbol.declarations();
        for &declaration in declarations {
            if is_index_signature_declaration(declaration) {
                let parameters = declaration.parameters();
                if parameters.len() == 1
                    && let Some(type_node) = parameters[0].type_node()
                {
                    for t in self.get_type_from_type_node(type_node).distributed_iter() {
                        index_signature_map.entry(t).or_default().push(declaration);
                    }
                }
            }
            // Do nothing for late-bound index signatures: allow these to duplicate one another and explicit indexes
        }
        for (t, declarations) in index_signature_map {
            if declarations.len() > 1 {
                for declaration in declarations {
                    let a0 = self.type_to_string(t, None);
                    self.error(
                        Some(declaration),
                        &diagnostics::DUPLICATE_INDEX_SIGNATURE_FOR_TYPE_0,
                        &[&a0],
                    );
                }
            }
        }
    }

    // checker.go:5024
    fn check_property_initialization(&mut self, node: P<Node>) {
        if !self.strict_null_checks
            || !self.strict_property_initialization
            || node.flags().intersects(NodeFlags::Ambient)
        {
            return;
        }
        let constructor = find_constructor_declaration(node);
        for &member in node.members() {
            if member.modifier_flags().intersects(ModifierFlags::Ambient) {
                continue;
            }
            if !ast::is_static(member) && Self::is_property_without_initializer(member) {
                let prop_name = member.name().unwrap();
                if is_identifier(prop_name)
                    || is_private_identifier(prop_name)
                    || is_computed_property_name(prop_name)
                {
                    let member_symbol = self.get_symbol_of_declaration(member).unwrap();
                    let t = self.get_type_of_symbol(member_symbol);
                    if !(t.flags().intersects(TypeFlags::AnyOrUnknown)
                        || Self::contains_undefined_type(t))
                        && (constructor.is_none()
                            || !self.is_property_initialized_in_constructor(
                                prop_name,
                                t,
                                constructor.unwrap(),
                            ))
                    {
                        self.error(member.name(), &diagnostics::PROPERTY_0_HAS_NO_INITIALIZER_AND_IS_NOT_DEFINITELY_ASSIGNED_IN_THE_CONSTRUCTOR, &[&tsrs_scanner::declaration_name_to_string(Some(prop_name))]);
                    }
                }
            }
        }
    }

    // checker.go:5047
    pub(crate) fn is_property_without_initializer(node: P<Node>) -> bool {
        is_property_declaration(node)
            && !has_abstract_modifier(node)
            && !is_exclamation_token(node.postfix_token())
            && node.initializer().is_none()
    }

    // checker.go:5051
    pub(crate) fn is_property_initialized_in_static_blocks(
        &mut self,
        prop_name: P<Node>,
        prop_type: P<Type>,
        static_blocks: &[P<Node>],
        start_pos: TextPos,
        end_pos: TextPos,
    ) -> bool {
        if static_blocks.iter().any(|&static_block| {
            // static block must be within the provided range as they are evaluated in document order (unlike constructors)
            if static_block.pos() >= start_pos && static_block.pos() <= end_pos {
                let this_keyword = self.factory.new_keyword_expression(Kind::ThisKeyword);
                let reference = self.factory.new_property_access_expression(
                    this_keyword,
                    None,
                    prop_name,
                    NodeFlags::None,
                );
                reference.expression().unwrap().set_parent(Some(reference));
                reference.set_parent(Some(static_block));
                reference.set_flow_node(
                    static_block.as_class_static_block_declaration().return_flow_node(),
                );
                let optional_type = self.get_optional_type(prop_type, false);
                let flow_type = self.get_flow_type_of_reference_ex(
                    reference,
                    prop_type,
                    optional_type,
                    None,
                    None,
                );
                if !Self::contains_undefined_type(flow_type) {
                    return true;
                }
            }

            false
        }) {
            return true;
        }
        false
    }

    // checker.go:5068
    fn is_property_initialized_in_constructor(
        &mut self,
        prop_name: P<Node>,
        prop_type: P<Type>,
        constructor: P<Node>,
    ) -> bool {
        let reference = if is_computed_property_name(prop_name) {
            let this_keyword = self.factory.new_keyword_expression(Kind::ThisKeyword);
            self.factory.new_element_access_expression(
                this_keyword,
                None,
                prop_name.expression().unwrap(),
                NodeFlags::None,
            )
        } else {
            let this_keyword = self.factory.new_keyword_expression(Kind::ThisKeyword);
            self.factory.new_property_access_expression(
                this_keyword,
                None,
                prop_name,
                NodeFlags::None,
            )
        };
        reference.expression().unwrap().set_parent(Some(reference));
        reference.set_parent(Some(constructor));
        reference.set_flow_node(constructor.as_constructor_declaration().return_flow_node());
        let optional_type = self.get_optional_type(prop_type, false);
        let flow_type =
            self.get_flow_type_of_reference_ex(reference, prop_type, optional_type, None, None);
        !Self::contains_undefined_type(flow_type)
    }

    // checker.go:5082
    pub(crate) fn check_interface_declaration(&mut self, node: P<Node>) {
        if !self.check_grammar_modifiers(node) {
            self.check_grammar_interface_declaration(node);
        }
        if !Self::container_allows_block_scoped_variable(node.parent().unwrap()) {
            self.grammar_error_on_node(
                node,
                &diagnostics::X_0_DECLARATIONS_CAN_ONLY_BE_DECLARED_INSIDE_A_BLOCK,
                &[&"interface"],
            );
        }
        self.check_type_parameters(node.type_parameters());
        self.check_type_name_is_reserved(
            node.name().unwrap(),
            &diagnostics::INTERFACE_NAME_CANNOT_BE_0,
        );
        self.check_exports_on_merged_declarations(node);
        let symbol = self.get_symbol_of_declaration(node).unwrap();
        self.check_type_parameter_lists_identical(symbol);
        // Check once per checker, but report on the first interface declaration,
        // independently of which declaration is checked first.
        if self.is_interface_check_site(symbol, node) {
            let first_interface_declaration =
                get_declaration_of_kind(symbol, Kind::InterfaceDeclaration).unwrap();
            let t = self.get_declared_type_of_symbol(symbol);
            let type_with_this = self.get_type_with_this_argument(t, None, false);
            let error_node = first_interface_declaration.name();
            // run subsequent checks only if first set succeeded
            if self.check_inherited_properties_are_identical(t, error_node) {
                for &base_type in self.get_base_types(t) {
                    let base_with_this = self.get_type_with_this_argument(
                        base_type,
                        t.as_interface_type().this_type.get(),
                        false,
                    );
                    self.check_type_assignable_to(
                        type_with_this,
                        base_with_this,
                        error_node,
                        Some(&diagnostics::INTERFACE_0_INCORRECTLY_EXTENDS_INTERFACE_1),
                    );
                }
                self.check_index_constraints(t, symbol /*isStaticIndex*/, false);
            }
        }
        self.check_object_type_for_duplicate_declarations(node, false /*checkPrivateNames*/);
        for &heritage_element in get_extends_heritage_clause_elements(node) {
            if is_expression_with_type_arguments(heritage_element) {
                let expr = heritage_element.expression().unwrap();
                if !is_entity_name_expression(expr) || is_optional_chain(expr) {
                    self.error(Some(expr), &diagnostics::AN_INTERFACE_CAN_ONLY_EXTEND_AN_IDENTIFIER_SLASHQUALIFIED_NAME_WITH_OPTIONAL_TYPE_ARGUMENTS, &[]);
                }
            }
            self.check_type_reference_node(heritage_element);
        }
        self.check_source_elements(node.members());
        self.check_class_or_interface_for_duplicate_index_signatures(node);
        self.register_for_unused_identifiers_check(node);
    }

    // Whether `node` runs the checks of its merged interface (TS2320, TS2430, index constraints). Go runs them once per
    // checker, at whichever declaration that checker visits first (`interfaceChecked`). With declarations in several
    // files, index-constraint checks can depend on checker assignment (notes/fix-history-dependent-diagnostics.md).
    // By default they run at the first interface declaration of the symbol
    // in each file, so each file gets the index-constraint errors at its own declarations whatever checked it before.
    // TS2320 and TS2430 are anchored to the first interface declaration in both modes.
    fn is_interface_check_site(&mut self, symbol: P<Symbol>, node: P<Node>) -> bool {
        if tsrs_core::compat::go_compatible_history() {
            let links = self.declared_type_links.get(symbol);
            if links.interface_checked.get() {
                return false;
            }
            links.interface_checked.set(true);
            return true;
        }
        let file = get_source_file_of_node(node);
        symbol
            .declarations()
            .iter()
            .copied()
            .find(|&d| is_interface_declaration(d) && get_source_file_of_node(d) == file)
            == Some(node)
    }

    // checker.go:5127
    fn check_inherited_properties_are_identical(
        &mut self,
        t: P<Type>,
        type_node: Option<P<Node>>,
    ) -> bool {
        let base_types = self.get_base_types(t);
        if base_types.len() < 2 {
            return true;
        }
        let mut seen: FxHashMap<&'static str, InheritanceInfo> = FxHashMap::default();
        let declared_members = self.resolve_declared_members(t).declared_members.get();
        if let Some(declared_members) = declared_members {
            seen.extend(declared_members.entries().into_iter().filter_map(|(id, p)| {
                if self.is_named_member(p, id) {
                    Some((p.name(), InheritanceInfo { prop: p, containing_type: t }))
                } else {
                    None
                }
            }));
        }
        let mut identical = true;
        for &base in base_types {
            let base_with_this = self.get_type_with_this_argument(
                base,
                t.as_interface_type().this_type.get(),
                false,
            );
            let properties = self.get_properties_of_type(base_with_this);
            for &prop in properties {
                match seen.entry(prop.name()) {
                    Entry::Vacant(entry) => {
                        entry.insert(InheritanceInfo { prop, containing_type: base });
                    }
                    Entry::Occupied(entry) => {
                        let InheritanceInfo {
                            prop: existing_prop,
                            containing_type: existing_containing_type,
                        } = *entry.get();
                        let is_inherited_property = existing_containing_type != t;
                        if is_inherited_property
                            && !self.is_property_identical_to(existing_prop, prop)
                        {
                            identical = false;
                            let Some(type_node) = type_node else { continue };
                            let type_name1 = self.type_to_string(existing_containing_type, None);
                            let type_name2 = self.type_to_string(base, None);
                            let a0 = self.symbol_to_string(prop);
                            let error_info = new_diagnostic_for_node(Some(type_node), Some(&diagnostics::NAMED_PROPERTY_0_OF_TYPES_1_AND_2_ARE_NOT_IDENTICAL), &[&a0, &type_name1, &type_name2]);
                            let a0 = self.type_to_string(t, None);
                            self.add_diagnostic(ast::new_diagnostic_chain(error_info, &diagnostics::INTERFACE_0_CANNOT_SIMULTANEOUSLY_EXTEND_TYPES_1_AND_2, &[&a0, &type_name1, &type_name2]));
                        }
                    }
                }
            }
        }
        identical
    }

    // checker.go:5159
    fn is_property_identical_to(&mut self, source_prop: P<Symbol>, target_prop: P<Symbol>) -> bool {
        self.compare_properties(
            source_prop,
            target_prop,
            super::checker::Checker::compare_types_identical,
        ) != Ternary::FALSE
    }

    // checker.go:5163
    pub(crate) fn check_enum_declaration(&mut self, node: P<Node>) {
        self.check_grammar_modifiers(node);
        self.check_collisions_for_declaration_name(node, node.name());
        self.check_exports_on_merged_declarations(node);
        self.check_source_elements(node.members());

        if self.should_check_erasable_syntax(node) && !node.flags().intersects(NodeFlags::Ambient) {
            self.error(
                Some(node),
                &diagnostics::THIS_SYNTAX_IS_NOT_ALLOWED_WHEN_ERASABLE_SYNTAX_ONLY_IS_ENABLED,
                &[],
            );
        }

        self.compute_enum_member_values(node);
        // Spec 2014 - Section 9.3:
        // It isn't possible for one enum declaration to continue the automatic numbering sequence of another,
        // and when an enum type has multiple declarations, only one declaration is permitted to omit a value
        // for the first member.
        //
        // Only perform this check once per symbol
        let enum_symbol = self.get_symbol_of_declaration(node).unwrap();
        let links = self.declared_type_links.get(enum_symbol);
        if !links.enum_checked.get() {
            links.enum_checked.set(true);
            let declarations = enum_symbol.declarations();
            if declarations.len() > 1 {
                let first_enum_declaration =
                    get_declaration_of_kind(enum_symbol, Kind::EnumDeclaration).unwrap();
                let enum_is_const = is_enum_const(first_enum_declaration);
                // check that const is placed\omitted on all enum declarations
                for &decl in declarations {
                    if is_enum_declaration(decl) && is_enum_const(decl) != enum_is_const {
                        self.error(
                            get_name_of_declaration(decl),
                            &diagnostics::ENUM_DECLARATIONS_MUST_ALL_BE_CONST_OR_NON_CONST,
                            &[],
                        );
                    }
                }
            }
            let mut seen_enum_missing_initial_initializer = false;
            for &declaration in declarations {
                // return true if we hit a violation of the rule, false otherwise
                if declaration.kind() != Kind::EnumDeclaration {
                    continue;
                }
                let members = declaration.members();
                if members.is_empty() {
                    continue;
                }
                let first_enum_member = members[0];
                if first_enum_member.initializer().is_none() {
                    if seen_enum_missing_initial_initializer {
                        self.error(first_enum_member.name(), &diagnostics::IN_AN_ENUM_WITH_MULTIPLE_DECLARATIONS_ONLY_ONE_DECLARATION_CAN_OMIT_AN_INITIALIZER_FOR_ITS_FIRST_ENUM_ELEMENT, &[]);
                    } else {
                        seen_enum_missing_initial_initializer = true;
                    }
                }
            }
        }
    }

    // checker.go:5214
    pub(crate) fn check_enum_member(&mut self, node: P<Node>) {
        if is_private_identifier(node.name().unwrap()) {
            self.error(
                Some(node),
                &diagnostics::AN_ENUM_MEMBER_CANNOT_BE_NAMED_WITH_A_PRIVATE_IDENTIFIER,
                &[],
            );
        }
        if is_computed_property_name(node.name().unwrap()) {
            self.check_expression(node.name().unwrap().expression().unwrap());
        }
        if let Some(initializer) = node.initializer() {
            self.check_expression(initializer);
        }
    }

    // checker.go:5223
    pub(crate) fn check_module_declaration(&mut self, node: P<Node>) {
        if let Some(body) = node.body() {
            self.check_source_element(Some(body));
            if !is_global_scope_augmentation(node) {
                self.register_for_unused_identifiers_check(node);
            }
        }
        let is_global_augmentation = is_global_scope_augmentation(node);
        let in_ambient_context = node.flags().intersects(NodeFlags::Ambient);
        if is_global_augmentation && !in_ambient_context {
            self.error(node.name(), &diagnostics::AUGMENTATIONS_FOR_THE_GLOBAL_SCOPE_SHOULD_HAVE_DECLARE_MODIFIER_UNLESS_THEY_APPEAR_IN_ALREADY_AMBIENT_CONTEXT, &[]);
        }
        let attributes = node.as_module_declaration().attributes;
        if let Some(attributes) = attributes {
            self.check_import_attributes_type(attributes);
        }
        let is_ambient_external_module = is_ambient_module(node);
        let context_error_message = if is_ambient_external_module {
            &diagnostics::AN_AMBIENT_MODULE_DECLARATION_IS_ONLY_ALLOWED_AT_THE_TOP_LEVEL_IN_A_FILE
        } else {
            &diagnostics::A_NAMESPACE_DECLARATION_IS_ONLY_ALLOWED_AT_THE_TOP_LEVEL_OF_A_NAMESPACE_OR_MODULE
        };
        if self.check_grammar_module_element_context(node, context_error_message) {
            // If we hit a module declaration in an illegal context, just bail out to avoid cascading errors.
            return;
        }
        if !self.check_grammar_modifiers(node)
            && !in_ambient_context
            && is_string_literal(node.name().unwrap())
        {
            self.grammar_error_on_node(
                node.name().unwrap(),
                &diagnostics::ONLY_AMBIENT_MODULES_CAN_USE_QUOTED_NAMES,
                &[],
            );
        }
        if is_identifier(node.name().unwrap()) {
            self.check_collisions_for_declaration_name(node, node.name());
            if node.as_module_declaration().keyword == Kind::ModuleKeyword {
                self.error(node.name(), &diagnostics::A_NAMESPACE_DECLARATION_SHOULD_NOT_BE_DECLARED_USING_THE_MODULE_KEYWORD_PLEASE_USE_THE_NAMESPACE_KEYWORD_INSTEAD, &[]);
            }
        }
        self.check_exports_on_merged_declarations(node);
        let symbol = self.get_symbol_of_declaration(node).unwrap();
        // The following checks only apply on a non-ambient instantiated module declaration.
        if symbol.flags().intersects(SymbolFlags::ValueModule)
            && !in_ambient_context
            && is_instantiated_module(node, self.compiler_options.should_preserve_const_enums())
        {
            if self.should_check_erasable_syntax(node) {
                self.error(
                    Some(node),
                    &diagnostics::THIS_SYNTAX_IS_NOT_ALLOWED_WHEN_ERASABLE_SYNTAX_ONLY_IS_ENABLED,
                    &[],
                );
            }
            if self.compiler_options.get_isolated_modules()
                && get_source_file_of_node(node).unwrap().external_module_indicator().is_none()
            {
                // This could be loosened a little if needed. The only problem we are trying to avoid is unqualified
                // references to namespace members declared in other files. But use of namespaces is discouraged anyway,
                // so for now we will just not allow them in scripts, which is the only place they can merge cross-file.
                let flag_name = self.get_isolated_modules_like_flag_name();
                self.error(node.name(), &diagnostics::NAMESPACES_ARE_NOT_ALLOWED_IN_GLOBAL_SCRIPT_FILES_WHEN_0_IS_ENABLED_IF_THIS_FILE_IS_NOT_INTENDED_TO_BE_A_GLOBAL_SCRIPT_SET_MODULE_DETECTION_TO_FORCE_OR_ADD_AN_EMPTY_EXPORT_STATEMENT, &[&flag_name]);
            }
            if symbol.declarations().len() > 1 {
                let first_non_ambient_class_or_func =
                    get_first_non_ambient_class_or_function_declaration(symbol);
                if let Some(first_non_ambient_class_or_func) = first_non_ambient_class_or_func {
                    if get_source_file_of_node(node)
                        != get_source_file_of_node(first_non_ambient_class_or_func)
                    {
                        self.error(node.name(), &diagnostics::A_NAMESPACE_DECLARATION_CANNOT_BE_IN_A_DIFFERENT_FILE_FROM_A_CLASS_OR_FUNCTION_WITH_WHICH_IT_IS_MERGED, &[]);
                    } else if node.pos() < first_non_ambient_class_or_func.pos() {
                        self.error(node.name(), &diagnostics::A_NAMESPACE_DECLARATION_CANNOT_BE_LOCATED_PRIOR_TO_A_CLASS_OR_FUNCTION_WITH_WHICH_IT_IS_MERGED, &[]);
                    }
                }
            }
            if self.compiler_options.verbatim_module_syntax.is_true()
                && is_source_file(node.parent().unwrap())
                && node.modifier_flags().intersects(ModifierFlags::Export)
                && self.emit_module_format_of_file(node.parent().unwrap().as_source_file_p())
                    == ModuleKind::COMMON_JS
            {
                let export_modifier =
                    node.modifier_nodes().iter().copied().find(|m| m.kind() == Kind::ExportKeyword);
                self.error(export_modifier, &diagnostics::A_TOP_LEVEL_EXPORT_MODIFIER_CANNOT_BE_USED_ON_VALUE_DECLARATIONS_IN_A_COMMON_JS_MODULE_WHEN_VERBATIM_MODULE_SYNTAX_IS_ENABLED, &[]);
            }
        }
        if is_ambient_external_module {
            if is_external_module_augmentation(node) {
                if attributes.is_some() {
                    self.error(
                        attributes,
                        &diagnostics::IMPORT_ATTRIBUTES_ARE_NOT_ALLOWED_ON_A_MODULE_AUGMENTATION,
                        &[],
                    );
                }
                // body of the augmentation should be checked for consistency only if augmentation was applied to its target (either global scope or module)
                // otherwise we'll be swamped in cascading errors.
                // We can detect if augmentation was applied using following rules:
                // - augmentation for a global scope is always applied
                // - augmentation for some external module is applied if symbol for augmentation is merged (it was combined with target module).
                let check_body = is_global_augmentation
                    || self
                        .get_symbol_of_declaration(node)
                        .unwrap()
                        .flags()
                        .intersects(SymbolFlags::Transient);
                if check_body && let Some(body) = node.body() {
                    for &statement in body.statements() {
                        self.check_module_augmentation_element(statement);
                    }
                }
            } else if is_global_source_file(node.parent().unwrap()) {
                if is_global_augmentation {
                    self.error(node.name(), &diagnostics::AUGMENTATIONS_FOR_THE_GLOBAL_SCOPE_CAN_ONLY_BE_DIRECTLY_NESTED_IN_EXTERNAL_MODULES_OR_AMBIENT_MODULE_DECLARATIONS, &[]);
                } else if tspath::is_external_module_name_relative(node.name().unwrap().text()) {
                    self.error(node.name(), &diagnostics::AMBIENT_MODULE_DECLARATION_CANNOT_SPECIFY_RELATIVE_MODULE_NAME, &[]);
                }
            } else if is_global_augmentation {
                self.error(node.name(), &diagnostics::AUGMENTATIONS_FOR_THE_GLOBAL_SCOPE_CAN_ONLY_BE_DIRECTLY_NESTED_IN_EXTERNAL_MODULES_OR_AMBIENT_MODULE_DECLARATIONS, &[]);
            } else {
                // Node is not an augmentation and is not located on the script level.
                // This means that this is declaration of ambient module that is located in other module or namespace which is prohibited.
                self.error(
                    node.name(),
                    &diagnostics::AMBIENT_MODULES_CANNOT_BE_NESTED_IN_OTHER_MODULES_OR_NAMESPACES,
                    &[],
                );
            }
        }
    }

    // checker.go:5320
    fn check_import_attributes_type(&mut self, attributes: P<Node>) {
        self.check_grammar_import_attributes_type(attributes);
        self.check_source_element(Some(attributes));
        let import_attributes_type = self.get_global_import_attributes_type_checked();
        let module_attributes_type =
            self.get_type_of_module_declaration_import_attributes(Some(attributes));
        if import_attributes_type != self.empty_object_type {
            self.check_type_assignable_to(
                module_attributes_type,
                import_attributes_type,
                Some(attributes),
                None,
            );
        }
    }

    // checker.go:5330
    fn get_type_of_module_declaration_import_attributes(
        &mut self,
        attributes: Option<P<Node>>,
    ) -> P<Type> {
        let Some(attributes) = attributes else {
            return self.empty_object_type;
        };
        self.get_type_from_type_node(attributes)
    }

    // checker.go:5337
    pub(crate) fn get_type_of_module_import_attributes(&mut self, symbol: P<Symbol>) -> P<Type> {
        if let Some(&t) = self.module_import_attributes_types.get(&symbol) {
            return t;
        }

        let module_decl =
            symbol.declarations().iter().copied().find(|&d| is_module_with_string_literal_name(d));
        let result = match module_decl {
            None => self.empty_object_type,
            Some(module_decl) => self.get_type_of_module_declaration_import_attributes(
                module_decl.as_module_declaration().attributes,
            ),
        };
        self.module_import_attributes_types.insert(symbol, result);
        result
    }
}

// checker.go:5352
fn is_instantiated_module(node: P<Node>, preserve_const_enums: bool) -> bool {
    let module_state = get_module_instance_state(node);
    module_state == ModuleInstanceState::Instantiated
        || preserve_const_enums && module_state == ModuleInstanceState::ConstEnumOnly
}

// checker.go:5357
fn get_first_non_ambient_class_or_function_declaration(symbol: P<Symbol>) -> Option<P<Node>> {
    if let Some(result) = symbol.declarations().iter().find_map(|&declaration| {
        if (is_class_declaration(declaration)
            || is_function_declaration(declaration) && node_is_present(declaration.body()))
            && !declaration.flags().intersects(NodeFlags::Ambient)
        {
            Some(Some(declaration))
        } else {
            None
        }
    }) {
        return result;
    }
    None
}

impl Checker {
    // checker.go:5366
    pub(crate) fn get_isolated_modules_like_flag_name(&self) -> String {
        if self.compiler_options.verbatim_module_syntax.is_true() {
            "verbatimModuleSyntax"
        } else {
            "isolatedModules"
        }
        .to_string()
    }

    // checker.go:5370
    fn check_module_augmentation_element(&mut self, node: P<Node>) {
        match node.kind() {
            Kind::VariableStatement => {
                // error each individual name in variable statement instead of marking the entire variable statement
                for &decl in node
                    .as_variable_statement()
                    .declaration_list
                    .as_variable_declaration_list()
                    .declarations
                    .nodes()
                {
                    self.check_module_augmentation_element(decl);
                }
            }
            Kind::ExportAssignment | Kind::ExportDeclaration => {
                self.grammar_error_on_first_token(node, &diagnostics::EXPORTS_AND_EXPORT_ASSIGNMENTS_ARE_NOT_PERMITTED_IN_MODULE_AUGMENTATIONS, &[]);
            }
            Kind::ImportEqualsDeclaration | Kind::ImportDeclaration | Kind::JSImportDeclaration => {
                // import a = e.x; in module augmentation is ok, but not import a = require('fs)
                if node.kind() == Kind::ImportEqualsDeclaration
                    && ast::is_internal_module_import_equals_declaration(node)
                {
                    return;
                }
                self.grammar_error_on_first_token(node, &diagnostics::IMPORTS_ARE_NOT_PERMITTED_IN_MODULE_AUGMENTATIONS_CONSIDER_MOVING_THEM_TO_THE_ENCLOSING_EXTERNAL_MODULE, &[]);
            }
            Kind::BindingElement | Kind::VariableDeclaration => {
                let name = node.name().unwrap();
                if is_binding_pattern(name) {
                    for &el in name.elements() {
                        // mark individual names in binding pattern
                        self.check_module_augmentation_element(el);
                    }
                }
            }
            _ => {}
        }
    }

    // checker.go:5398
    pub(crate) fn check_import_declaration(&mut self, node: P<Node>) {
        // Grammar checking
        let diagnostic = if is_in_js_file(node) {
            &diagnostics::AN_IMPORT_DECLARATION_CAN_ONLY_BE_USED_AT_THE_TOP_LEVEL_OF_A_MODULE
        } else {
            &diagnostics::AN_IMPORT_DECLARATION_CAN_ONLY_BE_USED_AT_THE_TOP_LEVEL_OF_A_NAMESPACE_OR_MODULE
        };
        if self.check_grammar_module_element_context(node, diagnostic) {
            // If we hit an import declaration in an illegal context, just bail out to avoid cascading errors.
            self.check_external_module_name_in_global_scope(node);
            return;
        }
        if !self.check_grammar_modifiers(node) && node.modifiers().is_some() {
            self.grammar_error_on_first_token(
                node,
                &diagnostics::AN_IMPORT_DECLARATION_CANNOT_HAVE_MODIFIERS,
                &[],
            );
        }
        if self.check_external_import_or_export_declaration(node) {
            let attributes = get_import_attributes(node);
            let mut resolved_module: Option<P<Symbol>> = None;
            let import_clause = node.import_clause();
            let module_specifier = node.module_specifier().unwrap();
            if let Some(import_clause) =
                import_clause.filter(|&ic| !self.check_grammar_import_clause(ic))
            {
                if import_clause.name().is_some() {
                    self.check_import_binding(import_clause);
                }
                let mut needs_import_star = false;
                let named_bindings = import_clause.as_import_clause().named_bindings;
                if let Some(named_bindings) = named_bindings {
                    if is_namespace_import(named_bindings) {
                        self.check_import_binding(named_bindings);
                        if self.emit_module_format_of_file(get_source_file_of_node(node).unwrap())
                            == ModuleKind::COMMON_JS
                        {
                            // import * as ns from "foo";
                            needs_import_star = true;
                            self.check_external_emit_helpers(node, ExternalEmitHelpers::ImportStar);
                        }
                    } else {
                        let import_attributes_type =
                            self.get_type_from_import_attributes(attributes);
                        resolved_module = self.resolve_external_module_name(
                            node,
                            node.module_specifier().unwrap(),
                            false,
                            import_attributes_type,
                        );
                        if resolved_module.is_some() {
                            for &binding in named_bindings.elements() {
                                self.check_import_binding(binding);
                            }
                        }
                    }
                }
                if import_clause.name().is_some()
                    && !needs_import_star
                    && self.emit_module_format_of_file(get_source_file_of_node(node).unwrap())
                        == ModuleKind::COMMON_JS
                {
                    // import d from "foo";
                    self.check_external_emit_helpers(node, ExternalEmitHelpers::ImportDefault);
                }

                if !import_clause.is_type_only()
                    && ModuleKind::NODE18 <= self.module_kind
                    && self.module_kind <= ModuleKind::NODE_NEXT
                    && {
                        let import_attributes_type =
                            self.get_type_from_import_attributes(attributes);
                        self.is_only_importable_as_default(
                            module_specifier,
                            resolved_module,
                            import_attributes_type,
                        )
                    }
                    && !has_type_json_import_attribute(node)
                {
                    let module_kind = self.module_kind.string();
                    self.error(Some(module_specifier), &diagnostics::IMPORTING_A_JSON_FILE_INTO_AN_ECMA_SCRIPT_MODULE_REQUIRES_A_TYPE_COLON_JSON_IMPORT_ATTRIBUTE_WHEN_MODULE_IS_SET_TO_0, &[&module_kind]);
                }
            } else if self.compiler_options.no_unchecked_side_effect_imports.is_true_or_unknown()
                && import_clause.is_none()
            {
                let ignore_errors = self.compiler_options.no_check.is_true();
                let mut error_message: Option<&'static Message> = None;
                if !ignore_errors {
                    error_message = Some(&diagnostics::CANNOT_FIND_MODULE_OR_TYPE_DECLARATIONS_FOR_SIDE_EFFECT_IMPORT_OF_0);
                }
                let import_attributes_type = self.get_type_from_import_attributes(attributes);
                self.resolve_external_module_name_worker(
                    Some(node),
                    Some(module_specifier),
                    error_message,
                    ignore_errors,
                    false, /*isForAugmentation*/
                    import_attributes_type,
                );
            }
        }
        self.check_import_attributes(node);
    }

    // checker.go:5467
    pub(crate) fn check_external_import_or_export_declaration(&mut self, node: P<Node>) -> bool {
        let module_name = get_external_module_name(node);
        let Some(module_name) = module_name.filter(|&m| !node_is_missing(m)) else {
            // Should be a parse error.
            return false;
        };
        if !is_string_literal(module_name) {
            self.error(Some(module_name), &diagnostics::STRING_LITERAL_EXPECTED, &[]);
            return false;
        }
        let parent = node.parent().unwrap();
        let in_ambient_external_module =
            is_module_block(parent) && is_ambient_module(parent.parent().unwrap());
        if !is_source_file(parent) && !in_ambient_external_module {
            self.error(
                Some(module_name),
                if is_export_declaration(node) {
                    &diagnostics::EXPORT_DECLARATIONS_ARE_NOT_PERMITTED_IN_A_NAMESPACE
                } else {
                    &diagnostics::IMPORT_DECLARATIONS_IN_A_NAMESPACE_CANNOT_REFERENCE_A_MODULE
                },
                &[],
            );
            return false;
        }
        if in_ambient_external_module
            && tspath::is_external_module_name_relative(module_name.text())
        {
            // we have already reported errors on top level imports/exports in external module augmentations in checkModuleDeclaration
            // no need to do this again.
            if !is_top_level_in_external_module_augmentation(Some(node)) {
                // TypeScript 1.0 spec (April 2013): 12.1.6
                // An ExternalImportDeclaration in an AmbientExternalModuleDeclaration may reference
                // other external modules only through top - level external module names.
                // Relative external module names are not permitted.
                self.error(Some(node), &diagnostics::IMPORT_OR_EXPORT_DECLARATION_IN_AN_AMBIENT_MODULE_DECLARATION_CANNOT_REFERENCE_MODULE_THROUGH_RELATIVE_MODULE_NAME, &[]);
                return false;
            }
        }
        if !is_import_equals_declaration(node) {
            let attributes = get_import_attributes(node);
            if let Some(attributes) = attributes
                && self.check_grammar_import_attribute_values(attributes)
            {
                return false;
            }
        }
        true
    }

    // checker.go:5505
    fn check_import_binding(&mut self, node: P<Node>) {
        self.check_collisions_for_declaration_name(node, node.name());
        self.check_alias_symbol(node);
        if is_import_specifier(node) {
            self.check_module_export_name(node.property_name(), true /*allowStringLiteral*/);
            if module_export_name_is_default(node.property_name_or_name().unwrap())
                && self.emit_module_format_of_file(get_source_file_of_node(node).unwrap())
                    == ModuleKind::COMMON_JS
            {
                self.check_external_emit_helpers(node, ExternalEmitHelpers::ImportDefault);
            }
        }
    }

    // checker.go:5517
    fn check_module_export_name(&mut self, name: Option<P<Node>>, allow_string_literal: bool) {
        let Some(name) = name.filter(|n| n.kind() == Kind::StringLiteral) else {
            return;
        };
        if !allow_string_literal {
            self.grammar_error_on_node(name, &diagnostics::IDENTIFIER_EXPECTED, &[]);
        } else if (self.module_kind == ModuleKind::ES2015 || self.module_kind == ModuleKind::ES2020)
            && !get_source_file_of_node(name).unwrap().is_declaration_file()
        {
            self.grammar_error_on_node(name, &diagnostics::STRING_LITERAL_IMPORT_AND_EXPORT_NAMES_ARE_NOT_SUPPORTED_WHEN_THE_MODULE_FLAG_IS_SET_TO_ES2015_OR_ES2020, &[]);
        }
    }
}

// checker.go:5530
fn has_type_json_import_attribute(node: P<Node>) -> bool {
    let attributes = node.as_import_declaration().attributes();
    attributes.is_some_and(|attributes| {
        attributes.as_import_attributes().attributes.nodes().iter().any(|&attr| {
            attr.name().unwrap().text() == "type"
                && is_string_literal_like(attr.as_import_attribute().value)
                && attr.as_import_attribute().value.text() == "json"
        })
    })
}

impl Checker {
    // checker.go:5537
    pub(crate) fn check_import_attributes(&mut self, declaration: P<Node>) {
        let Some(node) = get_import_attributes(declaration) else {
            return;
        };
        let import_attributes_type = self.get_global_import_attributes_type_checked();
        if import_attributes_type != self.empty_object_type {
            let source = self.get_type_from_import_attributes(Some(node)).unwrap();
            let target = self.get_nullable_type(import_attributes_type, TypeFlags::Undefined);
            self.check_type_assignable_to(source, target, Some(node), None);
        }
        let is_type_only = is_exclusively_type_only_import_or_export(declaration)
            || is_import_type_node(declaration);
        let override_ = self.get_resolution_mode_override(node, is_type_only);
        if is_type_only {
            return; // Other grammar checks do not apply to type-only imports with import attributes
        }

        if !self.module_kind.supports_import_attributes() {
            self.grammar_error_on_node(node, &diagnostics::IMPORT_ATTRIBUTES_ARE_ONLY_SUPPORTED_WHEN_THE_MODULE_OPTION_IS_SET_TO_ESNEXT_NODE18_NODE20_NODENEXT_OR_PRESERVE, &[]);
            return;
        }

        if let Some(module_specifier) = get_external_module_name(declaration)
            && self.get_emit_syntax_for_module_specifier_expression(module_specifier)
                == ModuleKind::COMMON_JS
        {
            self.grammar_error_on_node(node, &diagnostics::IMPORT_ATTRIBUTES_ARE_NOT_ALLOWED_ON_STATEMENTS_THAT_COMPILE_TO_COMMON_JS_REQUIRE_CALLS, &[]);
            return;
        }

        if override_ != ModuleKind::NONE {
            self.grammar_error_on_node(
                node,
                &diagnostics::X_RESOLUTION_MODE_CAN_ONLY_BE_SET_FOR_TYPE_ONLY_IMPORTS,
                &[],
            );
        }
    }

    // node Option<P<Node>> and result Option<P<Type>> (Go takes and returns nil).
    // checker.go:5569
    pub(crate) fn get_type_from_import_attributes(
        &mut self,
        node: Option<P<Node>>,
    ) -> Option<P<Type>> {
        let node = node?;
        if is_import_attributes(node) {
            return Some(self.check_import_attributes_expression(node));
        }
        Some(self.check_expression_cached(node))
    }

    // checker.go:5579
    pub(crate) fn check_import_attributes_expression(&mut self, node: P<Node>) -> P<Type> {
        let links = self.type_node_links.get(node);
        if links.resolved_type.get().is_none() {
            let symbol =
                self.new_symbol(SymbolFlags::ObjectLiteral, INTERNAL_SYMBOL_NAME_IMPORT_ATTRIBUTES);
            let members = SymbolTable::new();
            for &attribute in node.as_import_attributes().attributes.nodes() {
                let member =
                    self.new_symbol(SymbolFlags::Property, attribute.name().unwrap().text());
                let value_type =
                    self.check_expression_cached(attribute.as_import_attribute().value);
                let resolved_type = self.get_regular_type_of_literal_type(value_type);
                self.value_symbol_links.get(member).resolved_type.set(Some(resolved_type));
                members.set(member.name(), member);
            }
            let t = self.new_anonymous_type(Some(symbol), Some(members), &[], &[], &[]);
            t.object_flags.set(
                t.object_flags.get_lazy()
                    | ObjectFlags::ObjectLiteral
                    | ObjectFlags::NonInferrableType,
            );
            links.resolved_type.set(Some(t));
        }
        links.resolved_type.get().unwrap()
    }

    // checker.go:5596
    pub(crate) fn get_import_attributes_type_for_module_specifier(
        &mut self,
        module_specifier: P<Node>,
    ) -> Option<P<Type>> {
        let parent = module_specifier.parent().unwrap();
        if is_import_declaration_or_js_import_declaration(parent) || is_export_declaration(parent) {
            return self.get_type_from_import_attributes(get_import_attributes(parent));
        } else if is_literal_type_node(parent)
            && is_literal_import_type_node(parent.parent().unwrap())
        {
            return self
                .get_type_from_import_attributes(get_import_attributes(parent.parent().unwrap()));
        } else if is_import_call(parent) && parent.arguments().len() > 1 {
            let options = parent.arguments()[1];
            let options_type = self.check_expression_cached(options);
            return self.get_type_of_property_of_type(options_type, "with");
        }
        None
    }

    // checker.go:5610
    pub(crate) fn check_import_equals_declaration(&mut self, node: P<Node>) {
        let diagnostic = if is_in_js_file(node) {
            &diagnostics::AN_IMPORT_DECLARATION_CAN_ONLY_BE_USED_AT_THE_TOP_LEVEL_OF_A_MODULE
        } else {
            &diagnostics::AN_IMPORT_DECLARATION_CAN_ONLY_BE_USED_AT_THE_TOP_LEVEL_OF_A_NAMESPACE_OR_MODULE
        };
        if self.check_grammar_module_element_context(node, diagnostic) {
            self.check_external_module_name_in_global_scope(node);
            return; // If we hit an import declaration in an illegal context, just bail out to avoid cascading errors.
        }
        self.check_grammar_modifiers(node);
        if self.should_check_erasable_syntax(node) && !node.flags().intersects(NodeFlags::Ambient) {
            self.error(
                Some(node),
                &diagnostics::THIS_SYNTAX_IS_NOT_ALLOWED_WHEN_ERASABLE_SYNTAX_ONLY_IS_ENABLED,
                &[],
            );
        }
        if ast::is_internal_module_import_equals_declaration(node)
            || self.check_external_import_or_export_declaration(node)
        {
            self.check_import_binding(node);
            self.mark_linked_references(node, ReferenceHint::ExportImportEquals, None, None);
            let module_reference = node.as_import_equals_declaration().module_reference;
            if !is_external_module_reference(module_reference) {
                let node_symbol = self.get_symbol_of_declaration(node).unwrap();
                let target = self.resolve_alias(node_symbol);
                if target != self.unknown_symbol {
                    let target_flags = self.get_symbol_flags(target);
                    if target_flags.intersects(SymbolFlags::Value) {
                        // Target is a value symbol, check that it is not hidden by a local declaration with the same name
                        let module_name = get_first_identifier(module_reference);
                        if !self
                            .resolve_entity_name(
                                module_name,
                                SymbolFlags::Value | SymbolFlags::Namespace,
                                false,
                                false,
                                None,
                            )
                            .unwrap()
                            .flags()
                            .intersects(SymbolFlags::Namespace)
                        {
                            self.error(Some(module_name), &diagnostics::MODULE_0_IS_HIDDEN_BY_A_LOCAL_DECLARATION_WITH_THE_SAME_NAME, &[&tsrs_scanner::declaration_name_to_string(Some(module_name))]);
                        }
                    }
                    if target_flags.intersects(SymbolFlags::Type) {
                        self.check_type_name_is_reserved(
                            node.name().unwrap(),
                            &diagnostics::IMPORT_NAME_CANNOT_BE_0,
                        );
                    }
                }
                if node.is_type_only() {
                    self.grammar_error_on_node(
                        node,
                        &diagnostics::AN_IMPORT_ALIAS_CANNOT_USE_IMPORT_TYPE,
                        &[],
                    );
                }
            } else if ModuleKind::ES2015 <= self.module_kind
                && self.module_kind <= ModuleKind::ES_NEXT
                && !node.is_type_only()
                && !node.flags().intersects(NodeFlags::Ambient)
            {
                // Import equals declaration cannot be emitted as ESM
                self.grammar_error_on_node(node, &diagnostics::IMPORT_ASSIGNMENT_CANNOT_BE_USED_WHEN_TARGETING_ECMA_SCRIPT_MODULES_CONSIDER_USING_IMPORT_ASTERISK_AS_NS_FROM_MOD_IMPORT_A_FROM_MOD_IMPORT_D_FROM_MOD_OR_ANOTHER_MODULE_FORMAT_INSTEAD, &[]);
            }
        }
    }

    // checker.go:5653
    pub(crate) fn check_export_declaration(&mut self, node: P<Node>) {
        let diagnostic = if is_in_js_file(node) {
            &diagnostics::AN_EXPORT_DECLARATION_CAN_ONLY_BE_USED_AT_THE_TOP_LEVEL_OF_A_MODULE
        } else {
            &diagnostics::AN_EXPORT_DECLARATION_CAN_ONLY_BE_USED_AT_THE_TOP_LEVEL_OF_A_NAMESPACE_OR_MODULE
        };
        if self.check_grammar_module_element_context(node, diagnostic) {
            self.check_external_module_name_in_global_scope(node);
            return; // If we hit an export in an illegal context, just bail out to avoid cascading errors.
        }
        let export_decl = node.as_export_declaration();
        if !self.check_grammar_modifiers(node) && export_decl.modifiers().is_some() {
            self.grammar_error_on_first_token(
                node,
                &diagnostics::AN_EXPORT_DECLARATION_CANNOT_HAVE_MODIFIERS,
                &[],
            );
        }
        self.check_grammar_export_declaration(node);
        if export_decl.module_specifier.is_none()
            || self.check_external_import_or_export_declaration(node)
        {
            if let Some(export_clause) =
                export_decl.export_clause.filter(|&ec| !is_namespace_export(ec))
            {
                // export { x, y }
                // export { x, y } from "foo"
                for &binding in export_clause.elements() {
                    self.check_export_specifier(binding);
                }
                let parent = node.parent().unwrap();
                let in_ambient_external_module =
                    is_module_block(parent) && is_ambient_module(parent.parent().unwrap());
                let in_ambient_namespace_declaration = !in_ambient_external_module
                    && is_module_block(parent)
                    && export_decl.module_specifier.is_none()
                    && node.flags().intersects(NodeFlags::Ambient);
                if !is_source_file(parent)
                    && !in_ambient_external_module
                    && !in_ambient_namespace_declaration
                {
                    self.error(
                        Some(node),
                        &diagnostics::EXPORT_DECLARATIONS_ARE_NOT_PERMITTED_IN_A_NAMESPACE,
                        &[],
                    );
                }
            } else {
                // export * from "foo"
                // export * as ns from "foo";
                let import_attributes_type =
                    self.get_type_from_import_attributes(get_import_attributes(node));
                let module_symbol = self.resolve_external_module_name(
                    node,
                    export_decl.module_specifier.unwrap(),
                    false,
                    import_attributes_type,
                );
                if let Some(module_symbol) =
                    module_symbol.filter(|&s| has_export_assignment_symbol(s))
                {
                    let a0 = self.symbol_to_string(module_symbol);
                    self.error(
                        export_decl.module_specifier,
                        &diagnostics::MODULE_0_USES_EXPORT_AND_CANNOT_BE_USED_WITH_EXPORT_ASTERISK,
                        &[&a0],
                    );
                } else if let Some(export_clause) = export_decl.export_clause {
                    self.check_alias_symbol(export_clause);
                    self.check_module_export_name(
                        export_clause.name(),
                        true, /*allowStringLiteral*/
                    );
                }
                if self.emit_module_format_of_file(get_source_file_of_node(node).unwrap())
                    == ModuleKind::COMMON_JS
                {
                    if node.as_export_declaration().export_clause.is_some() {
                        // export * as ns from "foo";
                        self.check_external_emit_helpers(node, ExternalEmitHelpers::ImportStar);
                    } else {
                        // export * from "foo"
                        self.check_external_emit_helpers(node, ExternalEmitHelpers::ExportStar);
                    }
                }
            }
        }
        self.check_import_attributes(node);
    }

    // checker.go:5702
    fn check_external_module_name_in_global_scope(&mut self, node: P<Node>) {
        if get_enclosing_container(node).unwrap().kind() != Kind::SourceFile
            || (is_import_declaration_or_js_import_declaration(node)
                && node.import_clause().is_none())
        {
            return;
        }
        if let Some(module_name) = get_external_module_name(node) {
            let mut attributes: Option<P<Node>> = None;
            if has_import_attributes(node) {
                attributes = get_import_attributes(node);
            }
            let import_attributes_type = self.get_type_from_import_attributes(attributes);
            self.resolve_external_module_name(node, module_name, false, import_attributes_type);
        }
    }

    // checker.go:5716
    fn check_export_specifier(&mut self, node: P<Node>) {
        self.check_alias_symbol(node);
        let has_module_specifier =
            node.parent().unwrap().parent().unwrap().module_specifier().is_some();
        self.check_module_export_name(node.property_name(), has_module_specifier);
        self.check_module_export_name(node.name(), true /*allowStringLiteral*/);

        if !has_module_specifier {
            let exported_name = node.property_name_or_name().unwrap();
            if exported_name.kind() == Kind::StringLiteral {
                return; // Skip for invalid syntax like this: export { "x" }
            }
            // find immediate value referenced by exported name (SymbolFlags.Alias is set so we don't chase down aliases)
            let symbol = self.resolve_name(
                Some(exported_name),
                exported_name.text(),
                SymbolFlags::Value
                    | SymbolFlags::Type
                    | SymbolFlags::Namespace
                    | SymbolFlags::Alias,
                None, /*nameNotFoundMessage*/
                true, /*isUse*/
                false,
            );
            if symbol.is_some_and(|symbol| {
                symbol == self.undefined_symbol
                    || symbol == self.global_this_symbol
                    || !symbol.declarations().is_empty()
                        && get_declaration_container(symbol.declarations()[0])
                            .is_some_and(is_global_source_file)
            }) {
                self.error(Some(exported_name), &diagnostics::CANNOT_EXPORT_0_ONLY_LOCAL_DECLARATIONS_CAN_BE_EXPORTED_FROM_A_MODULE, &[&exported_name.text()]);
            } else {
                self.mark_linked_references(
                    node,
                    ReferenceHint::ExportSpecifier,
                    None, /*propSymbol*/
                    None, /*parentType*/
                );
            }
        } else if self.emit_module_format_of_file(get_source_file_of_node(node).unwrap())
            == ModuleKind::COMMON_JS
            && module_export_name_is_default(node.property_name_or_name().unwrap())
        {
            self.check_external_emit_helpers(node, ExternalEmitHelpers::ImportDefault);
        }
    }
}

// checker.go:5740
pub(crate) fn is_contained_by_namespace(node: P<Node>) -> bool {
    let mut container = node.parent().unwrap();
    if !is_source_file(container) {
        container = container.parent().unwrap();
    }
    is_module_declaration(container) && !is_ambient_module(container)
}

impl Checker {
    // checker.go:5748
    pub(crate) fn check_export_assignment(&mut self, node: P<Node>) {
        let is_export_equals = node.as_export_assignment().is_export_equals;
        // Always check the exported expression so its identifiers are resolved even when the
        // export assignment is misplaced (grammar error), keeping diagnostics stable
        // regardless of traversal order.
        let expr_type = self.check_expression_cached(node.expression().unwrap());
        let illegal_context_message = if is_export_equals {
            &diagnostics::AN_EXPORT_ASSIGNMENT_MUST_BE_AT_THE_TOP_LEVEL_OF_A_FILE_OR_MODULE_DECLARATION
        } else {
            &diagnostics::A_DEFAULT_EXPORT_MUST_BE_AT_THE_TOP_LEVEL_OF_A_FILE_OR_MODULE_DECLARATION
        };
        if self.check_grammar_module_element_context(node, illegal_context_message) {
            return; // If we hit an export assignment in an illegal context, just bail out to avoid cascading errors.
        }
        if self.should_check_erasable_syntax(node)
            && node.as_export_assignment().is_export_equals
            && !node.flags().intersects(NodeFlags::Ambient)
        {
            self.error(
                Some(node),
                &diagnostics::THIS_SYNTAX_IS_NOT_ALLOWED_WHEN_ERASABLE_SYNTAX_ONLY_IS_ENABLED,
                &[],
            );
        }
        if is_contained_by_namespace(node) {
            // TODO(danielr): should these be grammar errors?
            if is_export_equals {
                self.error(
                    Some(node),
                    &diagnostics::AN_EXPORT_ASSIGNMENT_CANNOT_BE_USED_IN_A_NAMESPACE,
                    &[],
                );
            } else {
                self.error(
                    Some(node),
                    &diagnostics::A_DEFAULT_EXPORT_CAN_ONLY_BE_USED_IN_AN_ECMA_SCRIPT_STYLE_MODULE,
                    &[],
                );
            }
            return;
        }
        if !self.check_grammar_modifiers(node)
            && is_export_assignment(node)
            && node.as_export_assignment().modifiers().is_some()
        {
            self.grammar_error_on_first_token(
                node,
                &diagnostics::AN_EXPORT_ASSIGNMENT_CANNOT_HAVE_MODIFIERS,
                &[],
            );
        }
        let is_illegal_export_default_in_cjs = !is_export_equals
            && !node.flags().intersects(NodeFlags::Ambient)
            && self.compiler_options.verbatim_module_syntax.is_true()
            && self.emit_module_format_of_file(get_source_file_of_node(node).unwrap())
                == ModuleKind::COMMON_JS;
        if is_identifier(node.expression().unwrap()) {
            let id = node.expression().unwrap();
            let resolved = self.resolve_entity_name(
                id,
                SymbolFlags::All,
                true, /*ignoreErrors*/
                true, /*dontResolveAlias*/
                Some(node),
            );
            let sym = self.get_export_symbol_of_value_symbol_if_exported(resolved);
            if let Some(sym) = sym {
                self.mark_linked_references(node, ReferenceHint::ExportAssignment, None, None);
                let type_only_declaration =
                    self.get_type_only_alias_declaration_ex(sym, SymbolFlags::Value);
                // If not a value, we're interpreting the identifier as a type export, along the lines of (`export { Id as default }`)
                if self.get_symbol_flags(sym).intersects(SymbolFlags::Value) {
                    // However if it is a value, we need to check it's being used correctly
                    if !is_illegal_export_default_in_cjs
                        && !node.flags().intersects(NodeFlags::Ambient)
                        && self.compiler_options.verbatim_module_syntax.is_true()
                        && type_only_declaration.is_some()
                    {
                        let message = if is_export_equals {
                            &diagnostics::AN_EXPORT_DECLARATION_MUST_REFERENCE_A_REAL_VALUE_WHEN_VERBATIM_MODULE_SYNTAX_IS_ENABLED_BUT_0_RESOLVES_TO_A_TYPE_ONLY_DECLARATION
                        } else {
                            &diagnostics::AN_EXPORT_DEFAULT_MUST_REFERENCE_A_REAL_VALUE_WHEN_VERBATIM_MODULE_SYNTAX_IS_ENABLED_BUT_0_RESOLVES_TO_A_TYPE_ONLY_DECLARATION
                        };
                        self.error(Some(id), message, &[&id.text()]);
                    }
                } else if !is_illegal_export_default_in_cjs
                    && !node.flags().intersects(NodeFlags::Ambient)
                    && self.compiler_options.verbatim_module_syntax.is_true()
                {
                    let message = if is_export_equals {
                        &diagnostics::AN_EXPORT_DECLARATION_MUST_REFERENCE_A_VALUE_WHEN_VERBATIM_MODULE_SYNTAX_IS_ENABLED_BUT_0_ONLY_REFERS_TO_A_TYPE
                    } else {
                        &diagnostics::AN_EXPORT_DEFAULT_MUST_REFERENCE_A_VALUE_WHEN_VERBATIM_MODULE_SYNTAX_IS_ENABLED_BUT_0_ONLY_REFERS_TO_A_TYPE
                    };
                    self.error(Some(id), message, &[&id.text()]);
                }
                if !is_illegal_export_default_in_cjs
                    && !node.flags().intersects(NodeFlags::Ambient)
                    && self.compiler_options.get_isolated_modules()
                    && !sym.flags().intersects(SymbolFlags::Value)
                {
                    let non_local_meanings = self.get_symbol_flags_ex(
                        sym, false, /*excludeTypeOnlyMeanings*/
                        true,  /*excludeLocalMeanings*/
                    );
                    if sym.flags().intersects(SymbolFlags::Alias)
                        && non_local_meanings.intersects(SymbolFlags::Type)
                        && !non_local_meanings.intersects(SymbolFlags::Value)
                        && (type_only_declaration.is_none()
                            || get_source_file_of_node(type_only_declaration)
                                != get_source_file_of_node(node))
                    {
                        // import { SomeType } from "./someModule";
                        // export default SomeType; OR
                        // export = SomeType;
                        let message = if is_export_equals {
                            &diagnostics::X_0_RESOLVES_TO_A_TYPE_AND_MUST_BE_MARKED_TYPE_ONLY_IN_THIS_FILE_BEFORE_RE_EXPORTING_WHEN_1_IS_ENABLED_CONSIDER_USING_IMPORT_TYPE_WHERE_0_IS_IMPORTED
                        } else {
                            &diagnostics::X_0_RESOLVES_TO_A_TYPE_AND_MUST_BE_MARKED_TYPE_ONLY_IN_THIS_FILE_BEFORE_RE_EXPORTING_WHEN_1_IS_ENABLED_CONSIDER_USING_EXPORT_TYPE_0_AS_DEFAULT
                        };
                        let flag_name = self.get_isolated_modules_like_flag_name();
                        self.error(Some(id), message, &[&id.text(), &flag_name]);
                    } else if type_only_declaration.is_some()
                        && get_source_file_of_node(type_only_declaration)
                            != get_source_file_of_node(node)
                    {
                        // import { SomeTypeOnlyValue } from "./someModule";
                        // export default SomeTypeOnlyValue; OR
                        // export = SomeTypeOnlyValue;
                        let message = if is_export_equals {
                            &diagnostics::X_0_RESOLVES_TO_A_TYPE_ONLY_DECLARATION_AND_MUST_BE_MARKED_TYPE_ONLY_IN_THIS_FILE_BEFORE_RE_EXPORTING_WHEN_1_IS_ENABLED_CONSIDER_USING_IMPORT_TYPE_WHERE_0_IS_IMPORTED
                        } else {
                            &diagnostics::X_0_RESOLVES_TO_A_TYPE_ONLY_DECLARATION_AND_MUST_BE_MARKED_TYPE_ONLY_IN_THIS_FILE_BEFORE_RE_EXPORTING_WHEN_1_IS_ENABLED_CONSIDER_USING_EXPORT_TYPE_0_AS_DEFAULT
                        };
                        let flag_name = self.get_isolated_modules_like_flag_name();
                        let diag = self.error(Some(id), message, &[&id.text(), &flag_name]);
                        Self::add_type_only_declaration_related_info(
                            diag,
                            type_only_declaration,
                            id.text(),
                        );
                    }
                }
            }
        }
        if is_illegal_export_default_in_cjs {
            self.error(Some(node), get_verbatim_module_syntax_error_message(node), &[]);
        }
        let mut container = node.parent().unwrap();
        if !is_source_file(container) {
            container = container.parent().unwrap();
        }
        self.check_external_module_exports(container);
        if let Some(type_node) = node.type_node().filter(|_| node.kind() == Kind::ExportAssignment)
        {
            let t = self.get_type_from_type_node(type_node);
            self.check_type_assignable_to_and_optionally_elaborate(
                expr_type,
                t,
                node.expression(),
                node.expression(),
                None, /*headMessage*/
                None,
            );
        }
        if node.flags().intersects(NodeFlags::Ambient)
            && !is_entity_name_expression(node.expression().unwrap())
        {
            self.grammar_error_on_node(node.expression().unwrap(), &diagnostics::THE_EXPRESSION_OF_AN_EXPORT_ASSIGNMENT_MUST_BE_AN_IDENTIFIER_OR_QUALIFIED_NAME_IN_AN_AMBIENT_CONTEXT, &[]);
        }
        if is_export_equals {
            // Forbid export= in esm implementation files, and esm mode declaration files
            if self.module_kind >= ModuleKind::ES2015
                && self.module_kind != ModuleKind::PRESERVE
                && ((node.flags().intersects(NodeFlags::Ambient)
                    && self
                        .program
                        .get_implied_node_format_for_emit(get_source_file_of_node(node).unwrap())
                        == ModuleKind::ES_NEXT)
                    || (!node.flags().intersects(NodeFlags::Ambient)
                        && self.program.get_implied_node_format_for_emit(
                            get_source_file_of_node(node).unwrap(),
                        ) != ModuleKind::COMMON_JS))
            {
                // export assignment is not supported in es6 modules
                self.grammar_error_on_node(node, &diagnostics::EXPORT_ASSIGNMENT_CANNOT_BE_USED_WHEN_TARGETING_ECMA_SCRIPT_MODULES_CONSIDER_USING_EXPORT_DEFAULT_OR_ANOTHER_MODULE_FORMAT_INSTEAD, &[]);
            } else if self.module_kind == ModuleKind::SYSTEM
                && !node.flags().intersects(NodeFlags::Ambient)
            {
                // system modules does not support export assignment
                self.grammar_error_on_node(
                    node,
                    &diagnostics::EXPORT_ASSIGNMENT_IS_NOT_SUPPORTED_WHEN_MODULE_FLAG_IS_SYSTEM,
                    &[],
                );
            }
        }
    }
}

// checker.go:5846
pub(crate) fn get_verbatim_module_syntax_error_message(node: P<Node>) -> &'static Message {
    let source_file = get_source_file_of_node(node).unwrap();
    let file_name = source_file.file_name();

    // Check if the file is .cts or .cjs (CommonJS-specific extensions)
    if tspath::file_extension_is_one_of(file_name, &[tspath::EXTENSION_CTS, tspath::EXTENSION_CJS])
    {
        return &diagnostics::ECMA_SCRIPT_IMPORTS_AND_EXPORTS_CANNOT_BE_WRITTEN_IN_A_COMMON_JS_FILE_UNDER_VERBATIM_MODULE_SYNTAX;
    }
    // For .ts, .tsx, .js, etc.
    &diagnostics::ECMA_SCRIPT_IMPORTS_AND_EXPORTS_CANNOT_BE_WRITTEN_IN_A_COMMON_JS_FILE_UNDER_VERBATIM_MODULE_SYNTAX_ADJUST_THE_TYPE_FIELD_IN_THE_NEAREST_PACKAGE_JSON_TO_MAKE_THIS_FILE_AN_ECMA_SCRIPT_MODULE_OR_ADJUST_YOUR_VERBATIM_MODULE_SYNTAX_MODULE_AND_MODULE_RESOLUTION_SETTINGS_IN_TYPE_SCRIPT
}

impl Checker {
    // checker.go:5858
    pub(crate) fn check_external_module_exports(&mut self, node: P<Node>) {
        let module_symbol = self.get_symbol_of_declaration(node).unwrap();
        let links = self.module_symbol_links.get(module_symbol);
        if !links.exports_checked.get() {
            let export_equals_symbol =
                module_symbol.exports().and_then(|e| e.lookup(INTERNAL_SYMBOL_NAME_EXPORT_EQUALS));
            // An export assignment is in error if (a) the module exports value members or (b) if the module exports type or
            // namespace members and the exported entity also exports type or namespace members.
            if let Some(export_equals_symbol) = export_equals_symbol
                && (self.has_exported_members_of_kind(module_symbol, SymbolFlags::Value)
                    || self.has_shadowed_namespace(export_equals_symbol))
            {
                let declaration = Self::get_declaration_of_alias_symbol(export_equals_symbol)
                    .or(export_equals_symbol.value_declaration());
                if let Some(declaration) = declaration
                    && !is_top_level_in_external_module_augmentation(Some(declaration))
                {
                    self.error(Some(declaration), &diagnostics::AN_EXPORT_ASSIGNMENT_CANNOT_BE_USED_IN_A_MODULE_WITH_OTHER_EXPORTED_ELEMENTS, &[]);
                }
            }
            // Checks for export * conflicts
            let exports = self.get_exports_of_module(module_symbol);
            for (id, symbol) in exports.entries() {
                if id == INTERNAL_SYMBOL_NAME_EXPORT_STAR {
                    continue;
                }
                // ECMA262: 15.2.1.1 It is a Syntax Error if the ExportedNames of ModuleItemList contains any duplicate entries.
                // (TS Exceptions: namespaces, function overloads, enums, and interfaces)
                if symbol.flags().intersects(SymbolFlags::Namespace | SymbolFlags::Enum) {
                    continue;
                }
                let declarations = symbol.declarations();
                let exported_declarations_count = declarations
                    .iter()
                    .filter(|&&d| {
                        is_not_overload(d) && !is_accessor(d) && !is_interface_declaration(d)
                    })
                    .count();
                if symbol.flags().intersects(SymbolFlags::TypeAlias)
                    && exported_declarations_count <= 2
                {
                    // it is legal to merge type alias with other values
                    // so count should be either 1 (just type alias) or 2 (type alias + merged value)
                    continue;
                }
                if exported_declarations_count > 1
                    && !declarations.iter().all(|&node| {
                        get_assignment_declaration_kind(node) == JSDeclarationKind::ExportsProperty
                    })
                {
                    for &declaration in declarations {
                        if is_not_overload(declaration) {
                            self.error(
                                Some(declaration),
                                &diagnostics::CANNOT_REDECLARE_EXPORTED_VARIABLE_0,
                                &[&id],
                            );
                        }
                    }
                }
            }
            links.exports_checked.set(true);
        }
    }

    // checker.go:5903
    fn has_exported_members_of_kind(
        &mut self,
        module_symbol: P<Symbol>,
        kind: SymbolFlags,
    ) -> bool {
        let Some(exports) = module_symbol.exports() else {
            return false;
        };
        exports.values().into_iter().any(|symbol| {
            symbol.name() != INTERNAL_SYMBOL_NAME_EXPORT_EQUALS
                && self.get_symbol_flags(symbol).intersects(kind)
        })
    }

    // checker.go:5912
    fn has_shadowed_namespace(&mut self, symbol: P<Symbol>) -> bool {
        if symbol.flags().intersects(SymbolFlags::NamespaceModule)
            && symbol.flags().intersects(SymbolFlags::Alias)
        {
            let target = self.resolve_alias(symbol);
            if target.flags().intersects(SymbolFlags::Namespace)
                && self.has_exported_members_of_kind(
                    target,
                    SymbolFlags::Type | SymbolFlags::Namespace,
                )
            {
                return true;
            }
        }
        false
    }
}

// checker.go:5921
fn is_not_overload(node: P<Node>) -> bool {
    !is_function_declaration(node) && !is_method_declaration(node) || node.body().is_some()
}

impl Checker {
    // checker.go:5925
    pub(crate) fn check_missing_declaration(&mut self, node: P<Node>) {
        self.check_decorators(node);
    }

    // checker.go:5929
    pub(crate) fn check_variable_statement(&mut self, node: P<Node>) {
        let var_statement = node.as_variable_statement();
        let declaration_list = var_statement.declaration_list;
        if !self.check_grammar_modifiers(node)
            && !self.check_grammar_variable_declaration_list(declaration_list)
        {
            self.check_grammar_for_disallowed_block_scoped_variable_statement(node);
        }
        self.check_variable_declaration_list(declaration_list);
    }

    // checker.go:5938
    pub(crate) fn check_variable_declaration_list(&mut self, node: P<Node>) {
        let block_scope_kind = get_combined_node_flags(node) & NodeFlags::BlockScoped;
        if (block_scope_kind == NodeFlags::Using || block_scope_kind == NodeFlags::AwaitUsing)
            && self.language_version < LANGUAGE_FEATURE_MINIMUM_TARGET.using_and_await_using
        {
            self.check_external_emit_helpers(
                node,
                ExternalEmitHelpers::AddDisposableResourceAndDisposeResources,
            );
        }
        self.check_source_elements(node.as_variable_declaration_list().declarations.nodes());
    }

    // checker.go:5946
    pub(crate) fn check_variable_declaration(&mut self, node: P<Node>) {
        self.check_grammar_variable_declaration(node);
        self.check_variable_like_declaration(node);
    }

    // Check variable, parameter, or property declaration
    // checker.go:5955
    pub(crate) fn check_variable_like_declaration(&mut self, node: P<Node>) {
        self.check_decorators(node);
        let Some(name) = node.name() else {
            return; // Missing array binding elements have no name
        };
        let type_node = node.type_node();
        let initializer = node.initializer();
        if !is_binding_element(node) {
            self.check_source_element(type_node);
        }
        // For a computed property, just check the initializer and exit
        // Do not use hasDynamicName here, because that returns false for well known symbols.
        // We want to perform checkComputedPropertyName for all computed properties, including
        // well known symbols.
        if is_computed_property_name(name) {
            self.check_computed_property_name(name);
            if let Some(initializer) = initializer {
                self.check_expression_cached(initializer);
            }
        }
        if is_binding_element(node) {
            let prop_name = node.property_name();

            if let Some(prop_name) = prop_name.filter(|&p| is_private_identifier(p)) {
                self.grammar_error_on_node(
                    prop_name,
                    &diagnostics::PRIVATE_IDENTIFIERS_CANNOT_BE_USED_IN_DESTRUCTURING_PATTERNS,
                    &[],
                );
            }

            if prop_name.is_some()
                && is_identifier(node.name().unwrap())
                && is_part_of_parameter_declaration(node)
                && node_is_missing(get_containing_function(node).unwrap().body())
            {
                // type F = ({a: string}) => void;
                //               ^^^^^^
                // variable renaming in function type notation is confusing,
                // so we forbid it even if noUnusedLocals is not enabled
                self.renamed_binding_elements_in_types.push(node);
                return;
            }
            if is_object_binding_pattern(node.parent().unwrap())
                && has_dot_dot_dot_token(node)
                && self.language_version < LANGUAGE_FEATURE_MINIMUM_TARGET.object_spread_rest
            {
                self.check_external_emit_helpers(node, ExternalEmitHelpers::Rest);
            }
            // check computed properties inside property names of binding elements
            if let Some(prop_name) = prop_name.filter(|&p| is_computed_property_name(p)) {
                self.check_computed_property_name(prop_name);
            }
            // check private/protected variable access
            let parent = node.parent().unwrap().parent().unwrap();
            let parent_check_mode = if has_dot_dot_dot_token(node) {
                CheckMode::RestBindingElement
            } else {
                CheckMode::Normal
            };
            let parent_type = self.get_type_for_binding_element_parent(parent, parent_check_mode);
            let prop_name_name = node.property_name_or_name().unwrap();
            if let Some(parent_type) = parent_type.filter(|_| !is_binding_pattern(prop_name_name)) {
                let expr_type = self.get_literal_type_from_property_name(prop_name_name);
                if is_type_usable_as_property_name(expr_type) {
                    let name_text = get_property_name_from_type(expr_type);
                    let property = self.get_property_of_type(parent_type, &name_text);
                    if let Some(property) = property {
                        self.mark_property_as_referenced(
                            property, None,  /*nodeForCheckWriteOnly*/
                            false, /*isSelfTypeAccess*/
                        );
                        // A destructuring is never a write-only reference.
                        let is_super =
                            parent.initializer().is_some_and(|i| i.kind() == Kind::SuperKeyword);
                        self.check_property_accessibility(
                            node,
                            is_super,
                            false, /*writing*/
                            parent_type,
                            property,
                        );
                    }
                }
            }
        }
        // For a binding pattern, check contained binding elements
        if is_binding_pattern(name) {
            self.check_source_elements(name.elements());
        }
        // For a parameter declaration with an initializer, error and exit if the containing function doesn't have a body
        if initializer.is_some()
            && is_part_of_parameter_declaration(node)
            && node_is_missing(get_containing_function(node).unwrap().body())
        {
            self.error(Some(node), &diagnostics::A_PARAMETER_INITIALIZER_IS_ONLY_ALLOWED_IN_A_FUNCTION_OR_CONSTRUCTOR_IMPLEMENTATION, &[]);
            return;
        }
        // For a binding pattern, validate the initializer and exit
        if is_binding_pattern(name) {
            if crate::is_in_ambient_or_type_node(node) {
                return;
            }
            let need_check_initializer = initializer.is_some()
                && node.parent().unwrap().parent().unwrap().kind() != Kind::ForInStatement;
            let need_check_widened_type = !name.elements().iter().any(|n| n.name().is_some());
            if need_check_initializer || need_check_widened_type {
                // Don't validate for-in initializer as it is already an error
                let widened_type = self.get_widened_type_for_variable_like_declaration(
                    node, false, /*reportErrors*/
                );
                if need_check_initializer {
                    let initializer = initializer.unwrap();
                    let initializer_type = self.check_expression_cached(initializer);
                    if self.strict_null_checks && need_check_widened_type {
                        self.check_non_null_non_void_type(initializer_type, node);
                    } else {
                        let target =
                            self.get_widened_type_for_variable_like_declaration(node, false);
                        self.check_type_assignable_to_and_optionally_elaborate(
                            initializer_type,
                            target,
                            Some(node),
                            Some(initializer),
                            None,
                            None,
                        );
                    }
                }
                // check the binding pattern with empty elements
                if need_check_widened_type {
                    if is_array_binding_pattern(name) {
                        let undefined_type = self.undefined_type;
                        self.check_iterated_type_or_element_type(
                            IterationUse::Destructuring,
                            widened_type,
                            undefined_type,
                            Some(node),
                        );
                    } else if self.strict_null_checks {
                        self.check_non_null_non_void_type(widened_type, node);
                    }
                }
            }
            return;
        }
        // For a commonjs `const x = require`, validate the alias and exit
        let symbol = self.get_symbol_of_declaration(node).unwrap();
        if symbol.flags().intersects(SymbolFlags::Alias)
            && is_variable_declaration_initialized_to_require(node)
        {
            self.check_alias_symbol(node);
            return;
        }
        if is_big_int_literal(name) {
            self.error(
                Some(name),
                &diagnostics::A_BIGINT_LITERAL_CANNOT_BE_USED_AS_A_PROPERTY_NAME,
                &[],
            );
        }
        let symbol_type = self.get_type_of_symbol(symbol);
        let t = self.convert_auto_to_any(symbol_type);
        if Some(node) == symbol.value_declaration() {
            // Node is the primary declaration of the symbol, just validate the initializer
            // Don't validate for-in initializer as it is already an error
            if let Some(initializer) = initializer
                .filter(|_| !is_for_in_statement(node.parent().unwrap().parent().unwrap()))
            {
                let initializer_type = self.check_expression_cached(initializer);
                self.check_type_assignable_to_and_optionally_elaborate(
                    initializer_type,
                    t,
                    Some(node),
                    Some(initializer),
                    None, /*headMessage*/
                    None,
                );
                let block_scope_kind =
                    self.get_combined_node_flags_cached(node) & NodeFlags::BlockScoped;
                if block_scope_kind == NodeFlags::AwaitUsing {
                    let global_async_disposable_type = self.get_global_async_disposable_type();
                    let global_disposable_type = self.get_global_disposable_type();
                    if global_async_disposable_type != self.empty_object_type
                        && global_disposable_type != self.empty_object_type
                    {
                        let types = [
                            global_async_disposable_type,
                            global_disposable_type,
                            self.null_type,
                            self.undefined_type,
                        ];
                        let optional_disposable_type = self.get_union_type(&types);
                        let widened = self.widen_type_for_variable_like_declaration(
                            Some(initializer_type),
                            node,
                            false,
                        );
                        self.check_type_assignable_to(widened, optional_disposable_type, Some(initializer), Some(&diagnostics::THE_INITIALIZER_OF_AN_AWAIT_USING_DECLARATION_MUST_BE_EITHER_AN_OBJECT_WITH_A_SYMBOL_ASYNC_DISPOSE_OR_SYMBOL_DISPOSE_METHOD_OR_BE_NULL_OR_UNDEFINED));
                    }
                } else if block_scope_kind == NodeFlags::Using {
                    let global_disposable_type = self.get_global_disposable_type();
                    if global_disposable_type != self.empty_object_type {
                        let types = [global_disposable_type, self.null_type, self.undefined_type];
                        let optional_disposable_type = self.get_union_type(&types);
                        let widened = self.widen_type_for_variable_like_declaration(
                            Some(initializer_type),
                            node,
                            false,
                        );
                        let mut diags = Vec::new();
                        if !self.check_type_assignable_to_ex(
                            widened,
                            optional_disposable_type,
                            Some(initializer),
                            Some(&diagnostics::THE_INITIALIZER_OF_A_USING_DECLARATION_MUST_BE_EITHER_AN_OBJECT_WITH_A_SYMBOL_DISPOSE_METHOD_OR_BE_NULL_OR_UNDEFINED),
                            &mut diags,
                        ) {
                            let global_async_disposable_type =
                                self.get_global_async_disposable_type();
                            let optional_async_disposable_type = self.get_union_type(&[
                                global_async_disposable_type,
                                self.null_type,
                                self.undefined_type,
                            ]);
                            if global_async_disposable_type != self.empty_object_type
                                && self.is_type_assignable_to(
                                    widened,
                                    optional_async_disposable_type,
                                )
                            {
                                diags[0].add_message_chain(ast::new_diagnostic_chain(
                                    None,
                                    &diagnostics::THIS_INITIALIZER_HAS_A_SYMBOL_ASYNC_DISPOSE_METHOD_DID_YOU_MEAN_TO_USE_AWAIT_USING,
                                    &[],
                                ));
                            }
                            self.add_diagnostic(diags[0]);
                        }
                    }
                }
            }
            let declarations = symbol.declarations();
            if declarations.len() > 1
                && declarations.iter().any(|&d| {
                    d != node
                        && is_variable_like(d)
                        && !Self::are_declaration_flags_identical(d, node)
                })
            {
                self.error(
                    Some(name),
                    &diagnostics::ALL_DECLARATIONS_OF_0_MUST_HAVE_IDENTICAL_MODIFIERS,
                    &[&tsrs_scanner::declaration_name_to_string(Some(name))],
                );
            }
        } else {
            // Node is a secondary declaration, check that type is identical to primary declaration and check that
            // initializer is consistent with type associated with the node
            let widened = self.get_widened_type_for_variable_like_declaration(node, false);
            let declaration_type = self.convert_auto_to_any(widened);
            if !self.is_error_type(t)
                && !self.is_error_type(declaration_type)
                && !self.is_type_identical_to(t, declaration_type)
                && !symbol.flags().intersects(SymbolFlags::Assignment)
            {
                self.error_next_variable_or_property_declaration_must_have_same_type(
                    symbol.value_declaration(),
                    t,
                    node,
                    declaration_type,
                );
            }
            if let Some(initializer) = initializer {
                let initializer_type = self.check_expression_cached(initializer);
                self.check_type_assignable_to_and_optionally_elaborate(
                    initializer_type,
                    declaration_type,
                    Some(node),
                    Some(initializer),
                    None, /*headMessage*/
                    None,
                );
            }
            if let Some(value_declaration) = symbol.value_declaration()
                && !Self::are_declaration_flags_identical(node, value_declaration)
            {
                self.error(
                    Some(name),
                    &diagnostics::ALL_DECLARATIONS_OF_0_MUST_HAVE_IDENTICAL_MODIFIERS,
                    &[&tsrs_scanner::declaration_name_to_string(Some(name))],
                );
            }
        }
        if !is_property_declaration(node) && !is_property_signature_declaration(node) {
            // We know we don't have a binding pattern or computed name here
            self.check_exports_on_merged_declarations(node);
            if is_variable_declaration(node) || is_binding_element(node) {
                self.check_var_declared_names_not_shadowed(node);
            }
            self.check_collisions_for_declaration_name(node, node.name());
        }
    }

    // checker.go:6119
    fn error_next_variable_or_property_declaration_must_have_same_type(
        &mut self,
        first_declaration: Option<P<Node>>,
        first_type: P<Type>,
        next_declaration: P<Node>,
        next_type: P<Type>,
    ) {
        let next_declaration_name = get_name_of_declaration(next_declaration);
        let message = if is_property_declaration(next_declaration)
            || is_property_signature_declaration(next_declaration)
        {
            &diagnostics::SUBSEQUENT_PROPERTY_DECLARATIONS_MUST_HAVE_THE_SAME_TYPE_PROPERTY_0_MUST_BE_OF_TYPE_1_BUT_HERE_HAS_TYPE_2
        } else {
            &diagnostics::SUBSEQUENT_VARIABLE_DECLARATIONS_MUST_HAVE_THE_SAME_TYPE_VARIABLE_0_MUST_BE_OF_TYPE_1_BUT_HERE_HAS_TYPE_2
        };
        let decl_name = tsrs_scanner::declaration_name_to_string(next_declaration_name);
        let a1 = self.type_to_string(first_type, None);
        let a2 = self.type_to_string(next_type, None);
        let err = self.error(next_declaration_name, message, &[&decl_name, &a1, &a2]);
        if let Some(first_declaration) = first_declaration {
            err.add_related_info(create_diagnostic_for_node(
                Some(first_declaration),
                &diagnostics::X_0_WAS_ALSO_DECLARED_HERE,
                &[&decl_name],
            ));
        }
    }

    // checker.go:6131
    fn check_var_declared_names_not_shadowed(&mut self, node: P<Node>) {
        // - ScriptBody : StatementList
        // It is a Syntax Error if any element of the LexicallyDeclaredNames of StatementList
        // also occurs in the VarDeclaredNames of StatementList.

        // - Block : { StatementList }
        // It is a Syntax Error if any element of the LexicallyDeclaredNames of StatementList
        // also occurs in the VarDeclaredNames of StatementList.

        // Variable declarations are hoisted to the top of their function scope. They can shadow
        // block scoped declarations, which bind tighter. this will not be flagged as duplicate definition
        // by the binder as the declaration scope is different.
        // A non-initialized declaration is a no-op as the block declaration will resolve before the var
        // declaration. the problem is if the declaration has an initializer. this will act as a write to the
        // block declared value. this is fine for let, but not const.
        // Only consider declarations with initializers, uninitialized const declarations will not
        // step on a let/const variable.
        // Do not consider const and const declarations, as duplicate block-scoped declarations
        // are handled by the binder.
        // We are only looking for const declarations that step on let\const declarations from a
        // different scope. e.g.:
        //      {
        //          const x = 0; // localDeclarationSymbol obtained after name resolution will correspond to this declaration
        //          const x = 0; // symbol for this declaration will be 'symbol'
        //      }

        // skip block-scoped variables and parameters
        if self.get_combined_node_flags_cached(node).intersects(NodeFlags::BlockScoped)
            || is_part_of_parameter_declaration(node)
        {
            return;
        }
        // NOTE: in ES6 spec initializer is required in variable declarations where name is binding pattern
        // so we'll always treat binding elements as initialized
        let symbol = self.get_symbol_of_declaration(node).unwrap();
        let name = node.name().unwrap();
        if symbol.flags().intersects(SymbolFlags::FunctionScopedVariable) {
            assert!(is_identifier(name), "Identifier expected");
            let local_declaration_symbol = self.resolve_name(
                Some(node),
                name.text(),
                SymbolFlags::Variable,
                None,  /*nameNotFoundMessage*/
                false, /*isUse*/
                false,
            );
            if let Some(local_declaration_symbol) = local_declaration_symbol
                && local_declaration_symbol != symbol
                && local_declaration_symbol.flags().intersects(SymbolFlags::BlockScopedVariable)
                && self
                    .get_declaration_node_flags_from_symbol(local_declaration_symbol)
                    .intersects(NodeFlags::BlockScoped)
            {
                let var_decl_list = find_ancestor_kind(
                    local_declaration_symbol.value_declaration(),
                    Kind::VariableDeclarationList,
                )
                .unwrap();
                let mut container: Option<P<Node>> = None;
                let var_decl_list_parent = var_decl_list.parent().unwrap();
                if is_variable_statement(var_decl_list_parent)
                    && var_decl_list_parent.parent().is_some()
                {
                    container = var_decl_list_parent.parent();
                }
                // names of block-scoped and function scoped variables can collide only
                // if block scoped variable is defined in the function\module\source file scope (because of variable hoisting)
                let names_share_scope = container.is_some_and(|container| {
                    is_block(container) && is_function_like(container.parent())
                        || is_module_block(container)
                        || is_module_declaration(container)
                        || is_source_file(container)
                });
                // here we know that function scoped variable is "shadowed" by block scoped one
                // a var declaration can't hoist past a lexical declaration and it results in a SyntaxError at runtime
                if !names_share_scope {
                    let name = self.symbol_to_string(local_declaration_symbol);
                    self.error(Some(node), &diagnostics::CANNOT_INITIALIZE_OUTER_SCOPED_VARIABLE_0_IN_THE_SAME_SCOPE_AS_BLOCK_SCOPED_DECLARATION_1, &[&name, &name]);
                }
            }
        }
    }

    // checker.go:6192
    pub(crate) fn check_decorators(&mut self, node: P<Node>) {
        // skip this check for nodes that cannot have decorators. These should have already had an error reported by
        // checkGrammarModifiers.
        if !can_have_decorators(node)
            || !has_decorators(node)
            || !node_can_be_decorated(
                self.legacy_decorators,
                node,
                node.parent(),
                node.parent().unwrap().parent(),
            )
        {
            return;
        }
        let Some(first_decorator) =
            node.modifier_nodes().iter().copied().find(|&m| is_decorator(m))
        else {
            return;
        };
        if self.legacy_decorators {
            self.check_external_emit_helpers(first_decorator, ExternalEmitHelpers::Decorate);
            if is_parameter_declaration(node) {
                self.check_external_emit_helpers(first_decorator, ExternalEmitHelpers::Param);
            }
        } else if self.language_version
            < LANGUAGE_FEATURE_MINIMUM_TARGET.class_and_class_element_decorators
        {
            self.check_external_emit_helpers(
                first_decorator,
                ExternalEmitHelpers::ESDecorateAndRunInitializers,
            );
            if is_class_declaration(node) {
                if node.name().is_none()
                    || self.get_first_transformable_static_class_element(node).is_some()
                {
                    self.check_external_emit_helpers(
                        first_decorator,
                        ExternalEmitHelpers::SetFunctionName,
                    );
                }
            } else if !is_class_expression(node) {
                let name = node.name().unwrap();
                if is_private_identifier(name)
                    && (is_method_declaration(node)
                        || is_accessor(node)
                        || is_auto_accessor_property_declaration(node))
                {
                    self.check_external_emit_helpers(
                        first_decorator,
                        ExternalEmitHelpers::SetFunctionName,
                    );
                }
                if is_computed_property_name(name) {
                    self.check_external_emit_helpers(first_decorator, ExternalEmitHelpers::PropKey);
                }
            }
        }
        self.mark_linked_references(node, ReferenceHint::Decorator, None, None);
        for &modifier in node.modifier_nodes() {
            if is_decorator(modifier) {
                self.check_decorator(modifier);
            }
        }
    }

    // checker.go:6231
    fn check_decorator(&mut self, node: P<Node>) {
        self.check_grammar_decorator(node);
        let signature = self.get_resolved_signature(node, None, CheckMode::Normal);
        self.check_deprecated_signature(signature, node);
        let return_type = self.get_return_type_of_signature(signature);
        if return_type.flags().intersects(TypeFlags::Any) {
            return;
        }
        // if we fail to get a signature and return type here, we will have already reported a grammar error in `checkDecorators`.
        let decorator_signature = self.get_decorator_call_signature(node);
        let Some(expected_return_type) =
            decorator_signature.and_then(|s| s.resolved_return_type.get())
        else {
            return;
        };
        let head_message: &'static Message = match node.parent().unwrap().kind() {
            Kind::PropertyDeclaration if !self.legacy_decorators => {
                &diagnostics::DECORATOR_FUNCTION_RETURN_TYPE_0_IS_NOT_ASSIGNABLE_TO_TYPE_1
            }
            Kind::PropertyDeclaration | Kind::Parameter => {
                &diagnostics::DECORATOR_FUNCTION_RETURN_TYPE_IS_0_BUT_IS_EXPECTED_TO_BE_VOID_OR_ANY
            }
            Kind::ClassDeclaration
            | Kind::ClassExpression
            | Kind::MethodDeclaration
            | Kind::GetAccessor
            | Kind::SetAccessor => {
                &diagnostics::DECORATOR_FUNCTION_RETURN_TYPE_0_IS_NOT_ASSIGNABLE_TO_TYPE_1
            }
            _ => panic!("Unhandled case in checkDecorator"),
        };
        self.check_type_assignable_to(
            return_type,
            expected_return_type,
            node.expression(),
            Some(head_message),
        );
    }

    // checker.go:6265
    pub(crate) fn check_iterated_type_or_element_type(
        &mut self,
        use_: IterationUse,
        input_type: P<Type>,
        sent_type: P<Type>,
        error_node: Option<P<Node>>,
    ) -> P<Type> {
        if is_type_any(Some(input_type)) {
            return input_type;
        }
        let t = self.get_iterated_type_or_element_type(
            use_, input_type, sent_type, error_node, true, /*checkAssignability*/
        );
        if let Some(t) = t {
            return t;
        }
        self.any_type
    }

    // checker.go:6276
    pub(crate) fn get_iterated_type_or_element_type(
        &mut self,
        use_: IterationUse,
        input_type: P<Type>,
        sent_type: P<Type>,
        error_node: Option<P<Node>>,
        check_assignability: bool,
    ) -> Option<P<Type>> {
        let allow_async_iterables = use_.intersects(IterationUse::AllowsAsyncIterablesFlag);
        if input_type == self.never_type {
            if let Some(error_node) = error_node {
                self.report_type_not_iterable_error(error_node, input_type, allow_async_iterables);
            }
            return None;
        }
        let iterable_exists = self.get_global_iterable_type() != self.empty_generic_type;
        let possible_out_of_bounds = self.compiler_options.no_unchecked_indexed_access
            == Tristate::True
            && use_.intersects(IterationUse::PossiblyOutOfBounds);
        if iterable_exists || allow_async_iterables {
            let iteration_types = self.get_iteration_types_of_iterable(
                input_type,
                use_,
                if iterable_exists { error_node } else { None },
            );
            if check_assignability && let Some(next_type) = iteration_types.next_type {
                let mut diagnostic: Option<&'static Message> = None;
                if use_.intersects(IterationUse::ForOfFlag) {
                    diagnostic = Some(&diagnostics::CANNOT_ITERATE_VALUE_BECAUSE_THE_NEXT_METHOD_OF_ITS_ITERATOR_EXPECTS_TYPE_1_BUT_FOR_OF_WILL_ALWAYS_SEND_0);
                } else if use_.intersects(IterationUse::SpreadFlag) {
                    diagnostic = Some(&diagnostics::CANNOT_ITERATE_VALUE_BECAUSE_THE_NEXT_METHOD_OF_ITS_ITERATOR_EXPECTS_TYPE_1_BUT_ARRAY_SPREAD_WILL_ALWAYS_SEND_0);
                } else if use_.intersects(IterationUse::DestructuringFlag) {
                    diagnostic = Some(&diagnostics::CANNOT_ITERATE_VALUE_BECAUSE_THE_NEXT_METHOD_OF_ITS_ITERATOR_EXPECTS_TYPE_1_BUT_ARRAY_DESTRUCTURING_WILL_ALWAYS_SEND_0);
                } else if use_.intersects(IterationUse::YieldStarFlag) {
                    diagnostic = Some(&diagnostics::CANNOT_DELEGATE_ITERATION_TO_VALUE_BECAUSE_THE_NEXT_METHOD_OF_ITS_ITERATOR_EXPECTS_TYPE_1_BUT_THE_CONTAINING_GENERATOR_WILL_ALWAYS_SEND_0);
                }
                if diagnostic.is_some() {
                    self.check_type_assignable_to(sent_type, next_type, error_node, diagnostic);
                }
            }
            if iteration_types.yield_type.is_some() || iterable_exists {
                let yield_type = iteration_types.yield_type?;
                if possible_out_of_bounds {
                    return Some(self.include_undefined_in_index_signature(yield_type));
                }
                return Some(yield_type);
            }
        }
        let mut array_type = input_type;
        let mut has_string_constituent = false;
        // If strings are permitted, remove any string-like constituents from the array type.
        // This allows us to find other non-string element types from an array unioned with
        // a string.
        if use_.intersects(IterationUse::AllowsStringInputFlag) {
            if array_type.flags().intersects(TypeFlags::Union) {
                // After we remove all types that are StringLike, we will know if there was a string constituent
                // based on whether the result of filter is a new array.
                let array_types = input_type.types();
                let filtered_types: Vec<P<Type>> = array_types
                    .iter()
                    .copied()
                    .filter(|t| !t.flags().intersects(TypeFlags::StringLike))
                    .collect();
                if filtered_types.len() != array_types.len() {
                    array_type = self.get_union_type_ex(
                        &filtered_types,
                        UnionReduction::Subtype,
                        AliasArg::None,
                        None,
                    );
                }
            } else if array_type.flags().intersects(TypeFlags::StringLike) {
                array_type = self.never_type;
            }
            has_string_constituent = array_type != input_type;
            if has_string_constituent {
                // Now that we've removed all the StringLike types, if no constituents remain, then the entire
                // arrayOrStringType was a string.
                if array_type.flags().intersects(TypeFlags::Never) {
                    if possible_out_of_bounds {
                        let string_type = self.string_type;
                        return Some(self.include_undefined_in_index_signature(string_type));
                    }
                    return Some(self.string_type);
                }
            }
        }
        if !self.is_array_like_type(array_type) {
            if error_node.is_some() {
                // Which error we report depends on whether we allow strings or if there was a
                // string constituent. For example, if the input type is number | string, we
                // want to say that number is not an array type. But if the input was just
                // number and string input is allowed, we want to say that number is not an
                // array type or a string type.
                let allows_strings =
                    use_.intersects(IterationUse::AllowsStringInputFlag) && !has_string_constituent;
                let (default_diagnostic, maybe_missing_await) =
                    self.get_iteration_diagnostic_details(use_, input_type, allows_strings);
                let maybe_missing_await =
                    maybe_missing_await && self.get_awaited_type_of_promise(array_type).is_some();
                let a0 = self.type_to_string(array_type, None);
                self.error_and_maybe_suggest_await(
                    error_node,
                    maybe_missing_await,
                    default_diagnostic,
                    &[&a0],
                );
            }
            if has_string_constituent {
                if possible_out_of_bounds {
                    let string_type = self.string_type;
                    return Some(self.include_undefined_in_index_signature(string_type));
                }
                return Some(self.string_type);
            }
            return None;
        }
        let number_type = self.number_type;
        let array_element_type = self.get_index_type_of_type(array_type, number_type);
        if let Some(array_element_type) = array_element_type.filter(|_| has_string_constituent) {
            // This is just an optimization for the case where arrayOrStringType is string | string[]
            if array_element_type.flags().intersects(TypeFlags::StringLike)
                && self.compiler_options.no_unchecked_indexed_access != Tristate::True
            {
                return Some(self.string_type);
            }
            if possible_out_of_bounds {
                let types = [array_element_type, self.string_type, self.undefined_type];
                return Some(self.get_union_type_ex(
                    &types,
                    UnionReduction::Subtype,
                    AliasArg::None,
                    None,
                ));
            }
            let types = [array_element_type, self.string_type];
            return Some(self.get_union_type_ex(
                &types,
                UnionReduction::Subtype,
                AliasArg::None,
                None,
            ));
        }
        if use_.intersects(IterationUse::PossiblyOutOfBounds) {
            return array_element_type.map(|t| self.include_undefined_in_index_signature(t));
        }
        array_element_type
    }

    // Gets the requested "iteration type" from a type that is either `Iterable`-like, `Iterator`-like,
    // `IterableIterator`-like, or `Generator`-like (for a non-async generator); or `AsyncIterable`-like,
    // `AsyncIterator`-like, `AsyncIterableIterator`-like, or `AsyncGenerator`-like (for an async generator).
    // checker.go:6386
    pub(crate) fn get_iteration_type_of_generator_function_return_type(
        &mut self,
        type_kind: IterationTypeKind,
        return_type: P<Type>,
        is_async_generator: bool,
    ) -> Option<P<Type>> {
        if is_type_any(Some(return_type)) {
            return None;
        }
        let iteration_types = self
            .get_iteration_types_of_generator_function_return_type(return_type, is_async_generator);
        iteration_types.get_type(type_kind)
    }

    // checker.go:6394
    pub(crate) fn get_iteration_types_of_generator_function_return_type(
        &mut self,
        t: P<Type>,
        is_async_generator: bool,
    ) -> IterationTypes {
        if is_type_any(Some(t)) {
            return IterationTypes {
                yield_type: Some(self.any_type),
                return_type: Some(self.any_type),
                next_type: Some(self.any_type),
            };
        }
        let use_ = if is_async_generator {
            IterationUse::AsyncGeneratorReturnType
        } else {
            IterationUse::GeneratorReturnType
        };
        let resolver = if is_async_generator {
            self.async_iteration_types_resolver
        } else {
            self.sync_iteration_types_resolver
        };
        let result = self.get_iteration_types_of_iterable(t, use_, None /*errorNode*/);
        if result.has_types() {
            return result;
        }
        self.get_iteration_types_of_iterator(
            t, resolver, None, /*errorNode*/
            None, /*diagnosticOutput*/
        )
    }

    // Gets the requested "iteration type" from an `Iterable`-like or `AsyncIterable`-like type.
    // checker.go:6408
    pub(crate) fn get_iteration_type_of_iterable(
        &mut self,
        use_: IterationUse,
        type_kind: IterationTypeKind,
        input_type: P<Type>,
        error_node: Option<P<Node>>,
    ) -> Option<P<Type>> {
        if is_type_any(Some(input_type)) {
            return None;
        }
        let iteration_types = self.get_iteration_types_of_iterable(input_type, use_, error_node);
        iteration_types.get_type(type_kind)
    }

    // Gets the *yield*, *return*, and *next* types from an `Iterable`-like or `AsyncIterable`-like type.
    //
    // At every level that involves analyzing return types of signatures, we union the return types of all the signatures.
    //
    // Another thing to note is that at any step of this process, we could run into a dead end,
    // meaning either the property is missing, or we run into the anyType. If either of these things
    // happens, we return a default `IterationTypes{}` to signal that we could not find the iteration type.
    // If a property is missing, and the previous step did not result in `any`, then we also give an error
    // if the caller requested it. Then the caller can decide what to do in the case where there is no
    // iterated type.
    //
    // For a **for-of** statement, `yield*` (in a normal generator), spread, array
    // destructuring, or normal generator we will only ever look for a `[Symbol.iterator]()`
    // method.
    //
    // For an async generator we will only ever look at the `[Symbol.asyncIterator]()` method.
    //
    // For a **for-await-of** statement or a `yield*` in an async generator we will look for
    // the `[Symbol.asyncIterator]()` method first, and then the `[Symbol.iterator]()` method.
    // checker.go:6435
    pub(crate) fn get_iteration_types_of_iterable(
        &mut self,
        t: P<Type>,
        use_: IterationUse,
        error_node: Option<P<Node>>,
    ) -> IterationTypes {
        let t = self.get_reduced_type(t);
        if is_type_any(Some(t)) {
            return IterationTypes {
                yield_type: Some(self.any_type),
                return_type: Some(self.any_type),
                next_type: Some(self.any_type),
            };
        }
        let key = IterationTypesKey { type_id: t.id, use_: use_ & IterationUse::CacheFlags };
        // If we are reporting errors and encounter a cached `noIterationTypes`, we should ignore the cached value and continue as if nothing was cached.
        // In addition, we should not cache any new results for this call.
        let mut no_cache = false;
        if let Some(&cached) = self.iteration_types_cache.get(&key) {
            if error_node.is_none() || cached.has_types() {
                return cached;
            }
            no_cache = true;
        }
        let result = self.get_iteration_types_of_iterable_worker(t, use_, error_node, no_cache);
        if !no_cache {
            self.iteration_types_cache.insert(key, result);
        }
        result
    }

    // checker.go:6457
    #[expect(
        clippy::only_used_in_recursion,
        reason = "The pinned Go function also carries these parameters through recursion without otherwise reading them."
    )]
    fn get_iteration_types_of_iterable_worker(
        &mut self,
        t: P<Type>,
        use_: IterationUse,
        error_node: Option<P<Node>>,
        no_cache: bool,
    ) -> IterationTypes {
        if t.flags().intersects(TypeFlags::Union) {
            let mut all_iteration_types: Vec<IterationTypes> = Vec::with_capacity(t.types().len());
            all_iteration_types = match t.types().iter().try_fold(
                all_iteration_types,
                |mut all_iteration_types, &constituent| {
                    let iteration_types = self.get_iteration_types_of_iterable_worker(
                        constituent,
                        use_,
                        None,
                        no_cache,
                    );
                    if !iteration_types.has_types() {
                        if let Some(error_node) = error_node {
                            self.add_deferred_diagnostic(move |c| {
                                c.report_type_not_iterable_error(
                                    error_node,
                                    t,
                                    use_.intersects(IterationUse::AllowsAsyncIterablesFlag),
                                );
                            });
                        }
                        return std::ops::ControlFlow::Break(IterationTypes::default());
                    }
                    all_iteration_types.push(iteration_types);

                    std::ops::ControlFlow::Continue(all_iteration_types)
                },
            ) {
                std::ops::ControlFlow::Continue(value) => value,
                std::ops::ControlFlow::Break(value) => return value,
            };
            return self.combine_iteration_types(&all_iteration_types);
        }
        let mut diags: Vec<P<Diagnostic>> = Vec::new();
        if use_.intersects(IterationUse::AllowsAsyncIterablesFlag) {
            let resolver = self.async_iteration_types_resolver;
            let iteration_types = self.get_iteration_types_of_iterable_fast(t, resolver);
            if iteration_types.has_types() {
                if use_.intersects(IterationUse::ForOfFlag) {
                    return self.get_async_from_sync_iteration_types(iteration_types, error_node);
                }
                return iteration_types;
            }
            let iteration_types =
                self.get_iteration_types_of_iterable_slow(t, resolver, error_node, &mut diags);
            if iteration_types.has_types() {
                if !diags.is_empty() {
                    for &d in &diags {
                        self.add_diagnostic(d);
                    }
                }
                return iteration_types;
            }
        }
        if use_.intersects(IterationUse::AllowsSyncIterablesFlag) {
            let resolver = self.sync_iteration_types_resolver;
            let iteration_types = self.get_iteration_types_of_iterable_fast(t, resolver);
            if iteration_types.has_types() {
                if use_.intersects(IterationUse::AllowsAsyncIterablesFlag) {
                    return self.get_async_from_sync_iteration_types(iteration_types, error_node);
                }
                return iteration_types;
            }
            let iteration_types =
                self.get_iteration_types_of_iterable_slow(t, resolver, error_node, &mut diags);
            if iteration_types.has_types() {
                if !diags.is_empty() {
                    for &d in &diags {
                        self.add_diagnostic(d);
                    }
                }
                if use_.intersects(IterationUse::AllowsAsyncIterablesFlag) {
                    return self.get_async_from_sync_iteration_types(iteration_types, error_node);
                }
                return iteration_types;
            }
        }
        if let Some(error_node) = error_node {
            // We defer the diagnostic because TypeToString may attempt to resolve symbols that are already being
            // resolved, possibly causing circularities.
            self.add_deferred_diagnostic(move |c| {
                let diagnostic = c.report_type_not_iterable_error(
                    error_node,
                    t,
                    use_.intersects(IterationUse::AllowsAsyncIterablesFlag),
                );
                for &d in &diags {
                    diagnostic.add_related_info(d);
                }
            });
        }
        IterationTypes::default()
    }
}
