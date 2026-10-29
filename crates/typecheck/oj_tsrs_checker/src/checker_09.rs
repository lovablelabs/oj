use crate::*;
use smallvec::SmallVec;
use tsrs_ast::*;
use tsrs_core::*;
use tsrs_diagnostics as diagnostics;
use tsrs_diagnostics::Message;

// Non-function declarations in checker.go:17433-19607 (hand-ported by checker-foundation):
//   type CacheHashKey (checker.go:17701)
//   type keyBuilder (checker.go:17707)
//   type thisAssignmentDeclarationKind (checker.go:18394)
//   const (checker.go:18396): thisAssignmentDeclarationNone, thisAssignmentDeclarationTyped,
//     thisAssignmentDeclarationConstructor, thisAssignmentDeclarationMethod

impl Checker {
    // checker.go:17433
    pub(crate) fn get_constraint_or_unknown_from_type_parameter(&mut self, t: P<Type>) -> P<Type> {
        let result = self.get_constraint_from_type_parameter(t);
        result.unwrap_or(self.unknown_type)
    }

    // checker.go:17438
    pub(crate) fn get_inferred_type_parameter_constraint(
        &mut self,
        t: P<Type>,
        omit_type_references: bool,
    ) -> Option<P<Type>> {
        let mut inferences: Vec<P<Type>> = Vec::new();
        if let Some(symbol) = t.symbol()
            && !symbol.declarations().is_empty()
        {
            let declarations = symbol.declarations();
            for declaration in declarations {
                let declaration_parent = declaration.parent().unwrap();
                if is_infer_type_node(declaration_parent) {
                    // When an 'infer T' declaration is immediately contained in a type reference node
                    // (such as 'Foo<infer T>'), T's constraint is inferred from the constraint of the
                    // corresponding type parameter in 'Foo'. When multiple 'infer T' declarations are
                    // present, we form an intersection of the inferred constraint types.
                    let mut child = declaration_parent;
                    let mut parent = child.parent();
                    while let Some(p) = parent {
                        if !is_parenthesized_type_node(p) {
                            break;
                        }
                        child = p;
                        parent = child.parent();
                    }
                    let parent = parent.unwrap();
                    if is_type_reference_node(parent) && !omit_type_references {
                        let type_parameters =
                            self.get_type_parameters_for_type_reference_or_import(parent);
                        if !type_parameters.is_empty() {
                            let index = parent.type_arguments().iter().position(|&n| n == child);
                            if let Some(index) = index
                                && index < type_parameters.len()
                            {
                                let declared_constraint =
                                    self.get_constraint_of_type_parameter(type_parameters[index]);
                                if let Some(declared_constraint) = declared_constraint {
                                    // Type parameter constraints can reference other type parameters so
                                    // constraints need to be instantiated. If instantiation produces the
                                    // type parameter itself, we discard that inference. For example, in
                                    //   type Foo<T extends string, U extends T> = [T, U];
                                    //   type Bar<T> = T extends Foo<infer X, infer X> ? Foo<X, X> : T;
                                    // the instantiated constraint for U is X, so we discard that inference.
                                    let type_parameters: &'static [P<Type>] =
                                        alloc_vec(type_parameters);
                                    let targets: Vec<Box<dyn Fn(&mut Checker) -> P<Type>>> = (0
                                        ..type_parameters.len())
                                        .map(|index| {
                                            let f: Box<dyn Fn(&mut Checker) -> P<Type>> =
                                                Box::new(move |c: &mut Checker| {
                                                    c.get_effective_type_argument_at_index(
                                                        parent,
                                                        type_parameters,
                                                        index,
                                                    )
                                                });
                                            f
                                        })
                                        .collect();
                                    let mapper = new_deferred_type_mapper(type_parameters, targets);
                                    let constraint =
                                        self.instantiate_type(declared_constraint, Some(mapper));
                                    if constraint != t {
                                        inferences.push(constraint);
                                    }
                                }
                            }
                        }
                    } else if is_parameter_declaration(parent)
                        && parent.as_parameter_declaration().dot_dot_dot_token().is_some()
                        || is_rest_type_node(parent)
                        || is_named_tuple_member(parent)
                            && parent.as_named_tuple_member().dot_dot_dot_token.is_some()
                    {
                        let t = self.create_array_type(self.unknown_type);
                        inferences.push(t);
                    } else if is_template_literal_type_span(parent) {
                        inferences.push(self.string_type);
                    } else if is_type_parameter_declaration(parent)
                        && is_mapped_type_node(parent.parent().unwrap())
                    {
                        inferences.push(self.string_number_symbol_type);
                    } else if is_mapped_type_node(parent)
                        && parent.type_node().is_some()
                        && skip_parentheses(parent.type_node().unwrap()) == declaration_parent
                        && is_conditional_type_node(parent.parent().unwrap())
                        && parent.parent().unwrap().as_conditional_type_node().extends_type
                            == parent
                        && is_mapped_type_node(
                            parent.parent().unwrap().as_conditional_type_node().check_type,
                        )
                        && parent
                            .parent()
                            .unwrap()
                            .as_conditional_type_node()
                            .check_type
                            .type_node()
                            .is_some()
                    {
                        let check_mapped_type =
                            parent.parent().unwrap().as_conditional_type_node().check_type;
                        let node_type =
                            self.get_type_from_type_node(check_mapped_type.type_node().unwrap());
                        let check_mapped_type_parameter =
                            check_mapped_type.as_mapped_type_node().type_parameter;
                        let tp_symbol =
                            self.get_symbol_of_declaration(check_mapped_type_parameter).unwrap();
                        let source = self.get_declared_type_of_type_parameter(tp_symbol);
                        let target = match check_mapped_type_parameter
                            .as_type_parameter_declaration()
                            .constraint
                        {
                            Some(constraint) => self.get_type_from_type_node(constraint),
                            None => self.string_number_symbol_type,
                        };
                        let mapper = new_simple_type_mapper(source, target);
                        let t = self.instantiate_type(node_type, Some(mapper));
                        inferences.push(t);
                    }
                }
            }
        }
        if !inferences.is_empty() {
            return Some(self.get_intersection_type(&inferences));
        }
        None
    }

    // checker.go:17511
    pub(crate) fn get_type_parameters_for_type_reference_or_import(
        &mut self,
        node: P<Node>,
    ) -> Vec<P<Type>> {
        let t = self.get_type_from_type_node(node);
        if !self.is_error_type(t) {
            let symbol = self.get_resolved_symbol_or_nil(node);
            if let Some(symbol) = symbol {
                return self.get_type_parameters_for_type_and_symbol(t, symbol);
            }
        }
        Vec::new()
    }

    // checker.go:17522
    fn get_type_parameters_for_type_and_symbol(
        &mut self,
        t: P<Type>,
        symbol: P<Symbol>,
    ) -> Vec<P<Type>> {
        if !self.is_error_type(t) {
            if symbol.flags().intersects(SymbolFlags::TypeAlias) {
                let type_parameters = self.type_alias_links.get(symbol).type_parameters.get();
                if !type_parameters.is_empty() {
                    return type_parameters.to_vec();
                }
            }
            if t.object_flags().intersects(ObjectFlags::Reference) {
                return t.target().unwrap().as_interface_type().local_type_parameters().to_vec();
            }
        }
        Vec::new()
    }

    // checker.go:17536
    fn get_effective_type_argument_at_index(
        &mut self,
        node: P<Node>,
        type_parameters: &[P<Type>],
        index: usize,
    ) -> P<Type> {
        let type_arguments = node.type_arguments();
        if index < type_arguments.len() {
            return self.get_type_from_type_node(type_arguments[index]);
        }
        self.get_effective_type_arguments(node, type_parameters)[index]
    }

    // checker.go:17544
    pub(crate) fn get_constraint_of_indexed_access(&mut self, t: P<Type>) -> Option<P<Type>> {
        if self.has_non_circular_base_constraint(t) {
            return self.get_constraint_from_indexed_access(t);
        }
        None
    }

    // checker.go:17551
    fn get_constraint_from_indexed_access(&mut self, t: P<Type>) -> Option<P<Type>> {
        let d = t.as_indexed_access_type();
        let object_type = d.object_type.get().unwrap();
        let index_type = d.index_type.get().unwrap();
        let access_flags = d.access_flags.get();
        if self.is_mapped_type_generic_indexed_access(t) {
            // For indexed access types of the form { [P in K]: E }[X], where K is non-generic and X is generic,
            // we substitute an instantiation of E where P is replaced with X.
            return Some(self.substitute_indexed_mapped_type(object_type, index_type));
        }
        let index_constraint = self.get_simplified_type_or_constraint(index_type);
        if let Some(index_constraint) = index_constraint
            && index_constraint != index_type
        {
            let indexed_access = self.get_indexed_access_type_or_undefined(
                object_type,
                index_constraint,
                access_flags,
                None,
                AliasArg::None,
            );
            if indexed_access.is_some() {
                return indexed_access;
            }
        }
        let object_constraint = self.get_simplified_type_or_constraint(object_type);
        if let Some(object_constraint) = object_constraint
            && object_constraint != object_type
        {
            return self.get_indexed_access_type_or_undefined(
                object_constraint,
                index_type,
                access_flags,
                None,
                AliasArg::None,
            );
        }
        None
    }

    // checker.go:17572
    pub(crate) fn get_constraint_of_conditional_type(&mut self, t: P<Type>) -> Option<P<Type>> {
        if self.has_non_circular_base_constraint(t) {
            return Some(self.get_constraint_from_conditional_type(t));
        }
        None
    }

    // checker.go:17579
    pub(crate) fn get_constraint_from_conditional_type(&mut self, t: P<Type>) -> P<Type> {
        let constraint = self.get_constraint_of_distributive_conditional_type(t);
        if let Some(constraint) = constraint {
            return constraint;
        }
        self.get_default_constraint_of_conditional_type(t)
    }

    // checker.go:17587
    pub(crate) fn get_default_constraint_of_conditional_type(&mut self, t: P<Type>) -> P<Type> {
        let d = t.as_conditional_type();
        if d.resolved_default_constraint.get().is_none() {
            // An `any` branch of a conditional type would normally be viral - specifically, without special handling here,
            // a conditional type with a single branch of type `any` would be assignable to anything, since it's constraint would simplify to
            // just `any`. This result is _usually_ unwanted - so instead here we elide an `any` branch from the constraint type,
            // in effect treating `any` like `never` rather than `unknown` in this location.
            let true_constraint = self.get_inferred_true_type_from_conditional_type(t);
            let false_constraint = self.get_false_type_from_conditional_type(t);
            if is_type_any(Some(true_constraint)) {
                d.resolved_default_constraint.set(Some(false_constraint));
            } else if is_type_any(Some(false_constraint)) {
                d.resolved_default_constraint.set(Some(true_constraint));
            } else {
                let u = self.get_union_type(&[true_constraint, false_constraint]);
                d.resolved_default_constraint.set(Some(u));
            }
        }
        d.resolved_default_constraint.get().unwrap()
    }

    // checker.go:17608
    pub(crate) fn get_constraint_of_distributive_conditional_type(
        &mut self,
        t: P<Type>,
    ) -> Option<P<Type>> {
        let d = t.as_conditional_type();
        if d.resolved_constraint_of_distributive.get().is_none() {
            // Check if we have a conditional type of the form 'T extends U ? X : Y', where T is a constrained
            // type parameter. If so, create an instantiation of the conditional type where T is replaced
            // with its constraint. We do this because if the constraint is a union type it will be distributed
            // over the conditional type and possibly reduced. For example, 'T extends undefined ? never : T'
            // removes 'undefined' from T.
            // We skip returning a distributive constraint for a restrictive instantiation of a conditional type
            // as the constraint for all type params (check type included) have been replace with `unknown`, which
            // is going to produce even more false positive/negative results than the distribute constraint already does.
            // Please note: the distributive constraint is a kludge for emulating what a negated type could to do filter
            // a union - once negated types exist and are applied to the conditional false branch, this "constraint"
            // likely doesn't need to exist.
            let root = d.root.get().unwrap();
            if root.is_distributive.get()
                && self
                    .cached_types
                    .get(&CachedTypeKey {
                        kind: CachedTypeKind::RestrictiveInstantiation,
                        type_id: t.id,
                    })
                    .copied()
                    != Some(t)
            {
                let check_type = d.check_type.get().unwrap();
                let mut constraint = Some(self.get_simplified_type(check_type, false /*writing*/));
                if constraint == Some(check_type) {
                    constraint = self.get_constraint_of_type(constraint.unwrap());
                }
                if let Some(constraint) = constraint
                    && constraint != check_type
                {
                    let mapper = prepend_type_mapping(
                        root.check_type.get().unwrap(),
                        constraint,
                        d.mapper.get(),
                    );
                    let instantiated = self.get_conditional_type_instantiation(
                        t, mapper, true, /*forConstraint*/
                        None,
                    );
                    // SAFETY: made here for this one instantiation; kept if the result stored it.
                    unsafe { recycle_mapping(mapper, false) };
                    if !instantiated.flags().intersects(TypeFlags::Never) {
                        d.resolved_constraint_of_distributive.set(Some(instantiated));
                        return Some(instantiated);
                    }
                }
            }
            d.resolved_constraint_of_distributive.set(Some(self.no_constraint_type));
        }
        if d.resolved_constraint_of_distributive.get() != Some(self.no_constraint_type) {
            return d.resolved_constraint_of_distributive.get();
        }
        None
    }

    // checker.go:17643
    pub(crate) fn get_declared_type_of_class_or_interface(&mut self, symbol: P<Symbol>) -> P<Type> {
        let links = self.declared_type_links.get(symbol);
        if links.declared_type.get().is_none() {
            let kind = if symbol.flags().intersects(SymbolFlags::Class) {
                ObjectFlags::Class
            } else {
                ObjectFlags::Interface
            };
            let t = self.new_object_type(kind, Some(symbol));
            links.declared_type.set(Some(t));
            let outer_type_parameters =
                self.get_outer_type_parameters_of_class_or_interface(symbol);
            let outer_type_parameter_count = outer_type_parameters.len();
            let mut type_parameters = self
                .append_local_type_parameters_of_class_or_interface_or_type_alias(
                    &outer_type_parameters,
                    symbol,
                );
            // A class or interface is generic if it has type parameters or a "this" type. We always give classes a "this" type
            // because it is not feasible to analyze all members to determine if the "this" type escapes the class (in particular,
            // property types inferred from initializers and method return types inferred from return statements are very hard
            // to exhaustively analyze). We give interfaces a "this" type if we can't definitely determine that they are free of
            // "this" references.
            if !type_parameters.is_empty()
                || kind == ObjectFlags::Class
                || !self.is_thisless_interface(symbol)
            {
                t.object_flags.set(t.object_flags.get_lazy() | ObjectFlags::Reference);
                let d = t.as_interface_type();
                let this_type = self.new_type_parameter(Some(symbol));
                d.this_type.set(Some(this_type));
                this_type.as_type_parameter().is_this_type.set(true);
                this_type.as_type_parameter().constraint.set(Some(t));
                type_parameters.push(this_type);
                d.all_type_parameters.set(alloc_vec(type_parameters));
                d.outer_type_parameter_count.set(outer_type_parameter_count);
                d.resolved_type_arguments.set(Some(d.type_parameters()));
                d.instantiations.make();
                d.instantiations.add(t);
                d.target.set(Some(t));
            }
        }
        links.declared_type.get().unwrap()
    }

    // Returns true if the interface given by the symbol is free of "this" references.
    //
    // Specifically, the result is true if the interface itself contains no references
    // to "this" in its body, if all base types are interfaces,
    // and if none of the base interfaces have a "this" type.
    // checker.go:17680
    fn is_thisless_interface(&mut self, symbol: P<Symbol>) -> bool {
        let declarations = symbol.declarations();
        for &declaration in declarations {
            if is_interface_declaration(declaration) {
                if declaration.flags().intersects(NodeFlags::ContainsThis) {
                    return false;
                }
                let base_type_nodes = get_extends_heritage_clause_elements(declaration);
                if base_type_nodes.iter().any(|&node| {
                    let name = get_heritage_clause_element_name(node);
                    if is_entity_name(name) || is_entity_name_expression(name) {
                        let base_symbol = self.resolve_entity_name(
                            name,
                            SymbolFlags::Type,
                            true, /*ignoreErrors*/
                            false,
                            None,
                        );
                        match base_symbol {
                            None => return true,
                            Some(base_symbol) => {
                                if !base_symbol.flags().intersects(SymbolFlags::Interface)
                                    || self
                                        .get_declared_type_of_class_or_interface(base_symbol)
                                        .as_interface_type()
                                        .this_type
                                        .get()
                                        .is_some()
                                {
                                    return true;
                                }
                            }
                        }
                    }

                    false
                }) {
                    return false;
                }
            }
        }
        true
    }
}

impl CacheHashKey {
    // checker.go:17703
    pub(crate) fn is_zero(self) -> bool {
        self == CacheHashKey::default()
    }
}

impl KeyBuilder {
    // checker.go:17713
    #[inline]
    pub fn hash(&mut self) -> CacheHashKey {
        if self.overflow_buffer.is_none() {
            return CacheHashKey::hash_128(&self.inline_buffer[..self.inline_length]);
        }
        self.hash_spilled()
    }

    /// `hash` of a key longer than the inline buffer (rare).
    #[cold]
    #[inline(never)]
    fn hash_spilled(&mut self) -> CacheHashKey {
        let overflow = self.overflow_buffer.as_mut().unwrap();
        overflow.extend_from_slice(&self.inline_buffer[..self.inline_length]);
        self.inline_length = 0;
        CacheHashKey::hash_128(overflow)
    }

    // spill moves the buffered bytes onto the end of overflowBuffer, so the key's byte
    // stream stays overflowBuffer followed by inlineBuffer.
    // checker.go:17722
    #[cold]
    #[inline(never)]
    fn spill(&mut self) {
        let n = self.inline_length;
        let inline = self.inline_buffer;
        self.overflow_buffer.get_or_insert_with(Vec::new).extend_from_slice(&inline[..n]);
        self.inline_length = 0;
    }

    // checker.go:17829
    #[inline]
    fn reserve(&mut self, size: usize) {
        if self.overflow_buffer.is_some() || self.inline_length + size > self.inline_buffer.len() {
            self.overflow_buffer.get_or_insert_with(Vec::new).reserve(self.inline_length + size);
        }
    }

    // checker.go:17727
    #[inline]
    pub fn write_byte(&mut self, c: u8) {
        if self.inline_length == self.inline_buffer.len() {
            self.spill();
        }
        self.inline_buffer[self.inline_length] = c;
        self.inline_length += 1;
    }

    // checker.go:17735
    #[inline]
    pub fn write_string(&mut self, s: &str) {
        if self.inline_length + s.len() > self.inline_buffer.len() {
            self.spill();
            if s.len() > self.inline_buffer.len() {
                self.overflow_buffer.get_or_insert_with(Vec::new).extend_from_slice(s.as_bytes());
                return;
            }
        }
        let start = self.inline_length;
        self.inline_buffer[start..start + s.len()].copy_from_slice(s.as_bytes());
        self.inline_length += s.len();
    }

    // checker.go:17746
    #[inline]
    pub fn write_uint32(&mut self, v: u32) {
        if self.inline_length + 4 > self.inline_buffer.len() {
            self.spill();
        }
        let start = self.inline_length;
        self.inline_buffer[start..start + 4].copy_from_slice(&v.to_le_bytes());
        self.inline_length += 4;
    }

    // checker.go:17754
    #[inline]
    fn write_uint64(&mut self, v: u64) {
        if self.inline_length + 8 > self.inline_buffer.len() {
            self.spill();
        }
        let start = self.inline_length;
        self.inline_buffer[start..start + 8].copy_from_slice(&v.to_le_bytes());
        self.inline_length += 8;
    }

    // checker.go:17762
    #[inline]
    fn write_usize(&mut self, value: usize) {
        self.write_uint64(value as u64);
    }

    // checker.go:17766
    #[inline]
    pub(crate) fn write_symbol(&mut self, s: P<Symbol>) {
        self.write_uint64(get_symbol_id(s).0);
    }

    // checker.go:17770
    #[inline]
    pub(crate) fn write_type(&mut self, t: P<Type>) {
        self.write_uint32(t.id.0);
    }

    // checker.go:17774
    #[inline]
    pub fn write_types(&mut self, types: &[P<Type>]) {
        self.reserve(8 + 4 * types.len());
        self.write_usize(types.len());
        for &t in types {
            self.write_type(t);
        }
    }

    // checker.go:17781
    #[inline]
    pub(crate) fn write_alias(&mut self, alias: Option<P<TypeAlias>>) {
        self.write_alias_arg(alias.into());
    }

    /// `write_alias` of an `AliasArg` (a pending alias writes the same bytes as the alias it stands for). Most keys
    /// have no alias; that byte is written inline.
    #[inline]
    fn write_alias_arg(&mut self, alias: AliasArg<'_>) {
        match alias {
            AliasArg::None => self.write_byte(0),
            _ => self.write_some_alias_arg(alias),
        }
    }

    #[inline(never)]
    fn write_some_alias_arg(&mut self, alias: AliasArg<'_>) {
        let (symbol, type_arguments) = match alias {
            AliasArg::None => unreachable!("write_alias_arg writes a missing alias"),
            AliasArg::Some(alias) => (alias.symbol.get(), alias.type_arguments.get()),
            AliasArg::Pending(pending) => (pending.symbol, pending.type_arguments.as_slice()),
        };
        self.write_byte(1);
        self.write_symbol(symbol.unwrap());
        self.write_types(type_arguments);
    }

    // checker.go:17791
    fn write_generic_type_references(
        &mut self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
        ignore_constraints: bool,
    ) -> bool {
        fn write_type_reference(
            b: &mut KeyBuilder,
            c: &mut Checker,
            ref_: P<Type>,
            depth: u32,
            ignore_constraints: bool,
            type_parameters: &mut Vec<P<Type>>,
            constrained: &mut bool,
        ) {
            b.write_type(ref_.target().unwrap());
            for &t in ref_.as_type_reference().resolved_type_arguments.get().unwrap_or(&[]) {
                if t.flags().intersects(TypeFlags::TypeParameter) {
                    if ignore_constraints || c.get_constraint_of_type_parameter(t).is_none() {
                        let index =
                            if let Some(index) = type_parameters.iter().position(|&tp| tp == t) {
                                index
                            } else {
                                let index = type_parameters.len();
                                type_parameters.push(t);
                                index
                            };
                        b.write_byte(b'=');
                        b.write_usize(index);
                        continue;
                    }
                    *constrained = true;
                } else if depth < 4 && is_type_reference_with_generic_arguments(c, t) {
                    b.write_byte(b'<');
                    write_type_reference(
                        b,
                        c,
                        t,
                        depth + 1,
                        ignore_constraints,
                        type_parameters,
                        constrained,
                    );
                    b.write_byte(b'>');
                    continue;
                }
                b.write_byte(b'-');
                b.write_type(t);
            }
        }

        let mut constrained = false;
        let mut type_parameters: Vec<P<Type>> = Vec::with_capacity(8);
        write_type_reference(
            self,
            c,
            source,
            0,
            ignore_constraints,
            &mut type_parameters,
            &mut constrained,
        );
        self.write_byte(b',');
        write_type_reference(
            self,
            c,
            target,
            0,
            ignore_constraints,
            &mut type_parameters,
            &mut constrained,
        );
        constrained
    }

    // checker.go:17826
    #[inline]
    fn write_node_id(&mut self, id: NodeId) {
        self.write_uint64(id.0);
    }

    // checker.go:17830
    #[inline]
    pub(crate) fn write_node(&mut self, node: Option<P<Node>>) {
        if let Some(node) = node {
            self.write_node_id(get_node_id(node));
        }
    }
}

// checker.go:17836
pub fn get_type_list_key(types: &[P<Type>]) -> CacheHashKey {
    let mut b = KeyBuilder::default();
    b.write_types(types);
    b.hash()
}

// checker.go:17842
pub(crate) fn get_alias_key(alias: AliasArg<'_>) -> CacheHashKey {
    let mut b = KeyBuilder::default();
    b.write_alias_arg(alias);
    b.hash()
}

// checker.go:17848
pub(crate) fn get_union_key(
    types: &[P<Type>],
    origin: Option<P<Type>>,
    alias: AliasArg<'_>,
) -> CacheHashKey {
    let mut b = KeyBuilder::default();
    match origin {
        None => b.write_types(types),
        Some(origin) if origin.flags().intersects(TypeFlags::Union) => {
            b.write_byte(b'|');
            b.write_types(origin.types());
        }
        Some(origin) if origin.flags().intersects(TypeFlags::Intersection) => {
            b.write_byte(b'&');
            b.write_types(origin.types());
        }
        Some(origin) if origin.flags().intersects(TypeFlags::Index) => {
            // origin type id alone is insufficient, as `keyof x` may resolve to multiple WIP values while `x` is still resolving
            b.write_byte(b'#');
            b.write_type(origin);
            b.write_byte(b'|');
            b.write_types(types);
        }
        _ => panic!("Unhandled case in getUnionKey"),
    }
    b.write_alias_arg(alias);
    b.hash()
}

// checker.go:17872
pub(crate) fn get_intersection_key(
    types: &[P<Type>],
    flags: IntersectionFlags,
    alias: AliasArg<'_>,
) -> CacheHashKey {
    let mut b = KeyBuilder::default();
    b.write_types(types);
    if flags.intersects(IntersectionFlags::NoConstraintReduction) {
        b.write_byte(b'*');
    } else {
        b.write_alias_arg(alias);
    }
    b.hash()
}

// checker.go:17883
pub(crate) fn get_tuple_key(element_infos: &[TupleElementInfo], readonly: bool) -> CacheHashKey {
    let mut b = KeyBuilder::default();
    for e in element_infos {
        if e.flags.intersects(ElementFlags::Required) {
            b.write_byte(b'#');
        } else if e.flags.intersects(ElementFlags::Optional) {
            b.write_byte(b'?');
        } else if e.flags.intersects(ElementFlags::Rest) {
            b.write_byte(b'.');
        } else {
            b.write_byte(b'*');
        }
        if e.labeled_declaration.is_some() {
            b.write_node(e.labeled_declaration);
        }
    }
    if readonly {
        b.write_byte(b'!');
    }
    b.hash()
}

// checker.go:17906
pub(crate) fn get_type_alias_instantiation_key(
    type_arguments: &[P<Type>],
    alias: Option<P<TypeAlias>>,
) -> CacheHashKey {
    get_type_instantiation_key(type_arguments, alias.into(), false)
}

/// The tags of `packed_type_list_key`'s families (each family keys its own caches).
const PACKED_INSTANTIATION_KEY_TAG: u64 = 0x1a5e;
const PACKED_CONDITIONAL_KEY_TAG: u64 = 0xc0d7;

/// A key for a list of at most three types without an alias, computed without the key builder.
///
/// Go hashes the key bytes with xxh3-128 and only compares keys for equality, in caches private to one key family.
/// This encoding is injective: the high word holds the family tag, `flag`, the length and the third type id; the low
/// word is a bijective mix of the first two ids xored with a function of the high word. So two packed keys are equal
/// exactly when their inputs are, and a packed key equals an xxh3 key of a longer or aliased list with the same
/// 2^-128 odds as two xxh3 keys equal each other. The mix spreads every input bit over the low word, which
/// `PackedMap` uses as the hash.
#[inline]
fn packed_type_list_key(tag: u64, types: &[P<Type>], flag: bool) -> Option<CacheHashKey> {
    if types.len() > 3 {
        return None;
    }
    let id = |i: usize| types.get(i).map_or(0, |t| u64::from(t.id.0));
    let hi = (tag << 48) | (u64::from(flag) << 34) | ((types.len() as u64) << 32) | id(2);
    let y = ((id(0) | (id(1) << 32)) ^ hi.wrapping_mul(0x9e37_79b9_7f4a_7c15))
        .wrapping_mul(0xbf58_476d_1ce4_e5b9);
    Some(CacheHashKey { hi, lo: y ^ (y >> 32) })
}

// checker.go:17910
#[inline]
pub(crate) fn get_type_instantiation_key(
    type_arguments: &[P<Type>],
    alias: AliasArg<'_>,
    single_signature: bool,
) -> CacheHashKey {
    if matches!(alias, AliasArg::None)
        && let Some(key) =
            packed_type_list_key(PACKED_INSTANTIATION_KEY_TAG, type_arguments, single_signature)
    {
        return key;
    }
    get_type_instantiation_key_hashed(type_arguments, alias, single_signature)
}

#[inline(never)]
fn get_type_instantiation_key_hashed(
    type_arguments: &[P<Type>],
    alias: AliasArg<'_>,
    single_signature: bool,
) -> CacheHashKey {
    let mut b = KeyBuilder::default();
    b.write_types(type_arguments);
    b.write_alias_arg(alias);
    if single_signature {
        b.write_byte(b'!');
    }
    b.hash()
}

// checker.go:17920
pub(crate) fn get_indexed_access_key(
    object_type: P<Type>,
    index_type: P<Type>,
    access_flags: AccessFlags,
    alias: AliasArg<'_>,
) -> CacheHashKey {
    let mut b = KeyBuilder::default();
    b.write_type(object_type);
    b.write_type(index_type);
    b.write_uint32(access_flags.bits());
    b.write_alias_arg(alias);
    b.hash()
}

// checker.go:17929
pub(crate) fn get_template_type_key(texts: &[&str], types: &[P<Type>]) -> CacheHashKey {
    let mut b = KeyBuilder::default();
    b.write_types(types);
    b.write_byte(b'|');
    for s in texts {
        b.write_usize(s.len());
    }
    b.write_byte(b'|');
    for s in texts {
        b.write_string(s);
    }
    b.hash()
}

// checker.go:17943
#[inline]
pub(crate) fn get_conditional_type_key(
    type_arguments: &[P<Type>],
    alias: Option<P<TypeAlias>>,
    for_constraint: bool,
) -> CacheHashKey {
    if alias.is_none()
        && let Some(key) =
            packed_type_list_key(PACKED_CONDITIONAL_KEY_TAG, type_arguments, for_constraint)
    {
        return key;
    }
    get_conditional_type_key_hashed(type_arguments, alias, for_constraint)
}

#[inline(never)]
fn get_conditional_type_key_hashed(
    type_arguments: &[P<Type>],
    alias: Option<P<TypeAlias>>,
    for_constraint: bool,
) -> CacheHashKey {
    let mut b = KeyBuilder::default();
    b.write_types(type_arguments);
    b.write_alias(alias);
    if for_constraint {
        b.write_byte(b'!');
    }
    b.hash()
}

// checker.go:17953
pub(crate) fn get_relation_key(
    c: &mut Checker,
    source: P<Type>,
    target: P<Type>,
    intersection_state: IntersectionState,
    is_identity: bool,
    ignore_constraints: bool,
) -> (RelationKey, bool) {
    let (mut source, mut target) = (source, target);
    if is_identity && source.id > target.id {
        std::mem::swap(&mut source, &mut target);
    }
    // Only type references can have generic arguments: the rest (most calls) need no registers for the key builder.
    if (!is_non_deferred_type_reference(source) || !is_non_deferred_type_reference(target))
        && let Some(key) = RelationKey::pair(source.id, target.id, intersection_state)
    {
        return (key, false);
    }
    get_relation_key_worker(c, source, target, intersection_state, ignore_constraints)
}

#[inline(never)]
fn get_relation_key_worker(
    c: &mut Checker,
    source: P<Type>,
    target: P<Type>,
    intersection_state: IntersectionState,
    ignore_constraints: bool,
) -> (RelationKey, bool) {
    let mut b = KeyBuilder::default();

    let constrained = if is_type_reference_with_generic_arguments(c, source)
        && is_type_reference_with_generic_arguments(c, target)
    {
        b.write_byte(b'g');
        b.write_generic_type_references(c, source, target, ignore_constraints)
    } else {
        // The simple key `'s', source id, target id, intersection state`, packed when it fits (`RelationKey`).
        if let Some(key) = RelationKey::pair(source.id, target.id, intersection_state) {
            return (key, false);
        }
        b.write_byte(b's');
        b.write_type(source);
        b.write_type(target);
        false
    };
    b.write_uint32(intersection_state.bits());
    (RelationKey::Hashed(b.hash()), constrained)
}

// checker.go:17980
fn is_type_reference_with_generic_arguments(c: &mut Checker, t: P<Type>) -> bool {
    is_non_deferred_type_reference(t)
        && c.get_type_arguments(t).iter().any(|&t| {
            t.flags().intersects(TypeFlags::TypeParameter)
                || is_type_reference_with_generic_arguments(c, t)
        })
}

// checker.go:17986
fn is_non_deferred_type_reference(t: P<Type>) -> bool {
    t.object_flags().intersects(ObjectFlags::Reference)
        && t.as_type_reference().node.get().is_none()
}

impl Checker {
    // checker.go:18007
    pub(crate) fn is_null_or_undefined(&mut self, node: P<Node>) -> bool {
        let expr = skip_parentheses(node);
        match expr.kind() {
            Kind::NullKeyword => return true,
            Kind::Identifier => return self.get_resolved_symbol(expr) == self.undefined_symbol,
            _ => {}
        }
        false
    }

    // checker.go:18018
    pub(crate) fn check_right_hand_side_of_for_of(&mut self, statement: P<Node>) -> P<Type> {
        let use_ = if statement.as_for_in_or_of_statement().await_modifier.is_some() {
            IterationUse::ForAwaitOf
        } else {
            IterationUse::ForOf
        };
        let expression = statement.expression().unwrap();
        let t = self.check_non_null_expression(expression);
        self.check_iterated_type_or_element_type(use_, t, self.undefined_type, Some(expression))
    }

    // Return the inferred type for a binding element
    // checker.go:18024
    pub(crate) fn get_type_for_binding_element(&mut self, declaration: P<Node>) -> Option<P<Type>> {
        let check_mode = if has_dot_dot_dot_token(declaration) {
            CheckMode::RestBindingElement
        } else {
            CheckMode::Normal
        };
        let parent_type = self.get_type_for_binding_element_parent(
            declaration.parent().unwrap().parent().unwrap(),
            check_mode,
        );
        if let Some(parent_type) = parent_type {
            return Some(self.get_binding_element_type_from_parent_type(
                declaration,
                parent_type,
                false, /*noTupleBoundsCheck*/
            ));
        }
        None
    }

    // Return the type of a binding element parent. We check SymbolLinks first to see if a type has been
    // assigned by contextual typing.
    // checker.go:18035
    pub(crate) fn get_type_for_binding_element_parent(
        &mut self,
        node: P<Node>,
        check_mode: CheckMode,
    ) -> Option<P<Type>> {
        if check_mode == CheckMode::Normal {
            // We can use a cached resolved type if no optionality was included in that type.
            if let Some(symbol) = self.get_symbol_of_declaration(node)
                && let Some(resolved_type) = self.value_symbol_links.get(symbol).resolved_type.get()
                && !(self.strict_null_checks && is_optional_declaration(node))
            {
                return Some(resolved_type);
            }
        }
        self.get_type_for_variable_like_declaration(
            node, false, /*includeOptionality*/
            check_mode,
        )
    }

    // checker.go:18047
    pub(crate) fn get_binding_element_type_from_parent_type(
        &mut self,
        declaration: P<Node>,
        parent_type: P<Type>,
        no_tuple_bounds_check: bool,
    ) -> P<Type> {
        let mut parent_type = parent_type;
        // If an any type was inferred for parent, infer that for the binding element
        if is_type_any(Some(parent_type)) {
            return parent_type;
        }
        let pattern = declaration.parent().unwrap();
        // Relax null check on ambient destructuring parameters, since the parameters have no implementation and are just documentation
        if self.strict_null_checks
            && declaration.flags().intersects(NodeFlags::Ambient)
            && is_part_of_parameter_declaration(declaration)
        {
            parent_type = self.get_non_nullable_type(parent_type);
        } else if self.strict_null_checks && pattern.parent().unwrap().initializer().is_some() && {
            let init_type =
                self.get_type_of_initializer(pattern.parent().unwrap().initializer().unwrap());
            !self.has_type_facts(init_type, TypeFacts::EQUndefined)
        } {
            parent_type = self.get_type_with_facts(parent_type, TypeFacts::NEUndefined);
        }
        let access_flags = AccessFlags::ExpressionPosition
            | if no_tuple_bounds_check || Self::has_default_value(declaration) {
                AccessFlags::AllowMissing
            } else {
                AccessFlags::None
            };
        let t: P<Type>;
        match pattern.kind() {
            Kind::ObjectBindingPattern => {
                if has_dot_dot_dot_token(declaration) {
                    parent_type = self.get_reduced_type(parent_type);
                    if parent_type.flags().intersects(TypeFlags::Unknown)
                        || !self.is_valid_spread_type(parent_type)
                    {
                        self.error(
                            Some(declaration),
                            &diagnostics::REST_TYPES_MAY_ONLY_BE_CREATED_FROM_OBJECT_TYPES,
                            &[],
                        );
                        return self.error_type;
                    }
                    let elements = pattern.elements();
                    let mut literal_members: Vec<P<Node>> = Vec::with_capacity(elements.len());
                    literal_members.extend(elements.iter().filter_map(|&element| {
                        if has_dot_dot_dot_token(element) {
                            None
                        } else {
                            let name = element.property_name_or_name().unwrap();
                            Some(name)
                        }
                    }));
                    t = self.get_rest_type(parent_type, &literal_members, declaration.symbol());
                } else {
                    // Use explicitly specified property name ({ p: xxx } form), or otherwise the implied name ({ p } form)
                    let name = declaration.property_name_or_name().unwrap();
                    let index_type = self.get_literal_type_from_property_name(name);
                    let declared_type = self.get_indexed_access_type_ex(
                        parent_type,
                        index_type,
                        access_flags,
                        Some(name),
                        AliasArg::None,
                    );
                    t = self.get_flow_type_of_destructuring(declaration, declared_type);
                }
            }
            Kind::ArrayBindingPattern => {
                // This elementType will be used if the specific property corresponding to this index is not
                // present (aka the tuple element property). This call also checks that the parentType is in
                // fact an iterable or array (depending on target language).
                let element_type = self.check_iterated_type_or_element_type(
                    IterationUse::Destructuring
                        | if has_dot_dot_dot_token(declaration) {
                            IterationUse::empty()
                        } else {
                            IterationUse::PossiblyOutOfBounds
                        },
                    parent_type,
                    self.undefined_type,
                    Some(pattern),
                );
                let index = pattern
                    .elements()
                    .iter()
                    .position(|&e| e == declaration)
                    .expect("binding element belongs to its pattern");
                if has_dot_dot_dot_token(declaration) {
                    // If the parent is a tuple type, the rest element has a tuple type of the
                    // remaining tuple element types. Otherwise, the rest element has an array type with same
                    // element type as the parent type.
                    let base_constraint = self
                        .map_type(parent_type, |c, t| {
                            if t.flags().intersects(TypeFlags::InstantiableNonPrimitive) {
                                return Some(c.get_base_constraint_or_type(t));
                            }
                            Some(t)
                        })
                        .unwrap();
                    if every_type(self, base_constraint, |_, t| is_tuple_type(t)) {
                        t = self
                            .map_type(base_constraint, |c, t| Some(c.slice_tuple_type(t, index, 0)))
                            .unwrap();
                    } else {
                        t = self.create_array_type(element_type);
                    }
                } else if self.is_array_like_type(parent_type) {
                    let index_type = self.get_number_literal_type(Number(index as f64));
                    let declared_type = self
                        .get_indexed_access_type_or_undefined(
                            parent_type,
                            index_type,
                            access_flags,
                            declaration.name(),
                            AliasArg::None,
                        )
                        .unwrap_or(self.error_type);
                    t = self.get_flow_type_of_destructuring(declaration, declared_type);
                } else {
                    t = element_type;
                }
            }
            _ => panic!("Unhandled case in getBindingElementTypeFromParentType"),
        }
        if declaration.initializer().is_none() {
            return t;
        }
        if walk_up_binding_elements_and_patterns(declaration).type_node().is_some() {
            // In strict null checking mode, if a default value of a non-undefined type is specified, remove
            // undefined from the final type.
            if self.strict_null_checks && {
                let init_type =
                    self.check_declaration_initializer(declaration, CheckMode::Normal, None);
                !self.has_type_facts(init_type, TypeFacts::IsUndefined)
            } {
                return self.get_non_undefined_type(t);
            }
            return t;
        }
        let non_undefined = self.get_non_undefined_type(t);
        let init_type = self.check_declaration_initializer(declaration, CheckMode::Normal, None);
        let u = self.get_union_type_ex(
            &[non_undefined, init_type],
            UnionReduction::Subtype,
            AliasArg::None,
            None,
        );
        self.widen_type_inferred_from_initializer(declaration, u)
    }

    // checker.go:18132
    pub(crate) fn get_rest_type(
        &mut self,
        source: P<Type>,
        properties: &[P<Node>],
        symbol: Option<P<Symbol>>,
    ) -> P<Type> {
        let source = self.filter_type(source, |_, t| !t.flags().intersects(TypeFlags::Nullable));
        if source.flags().intersects(TypeFlags::Never) {
            return self.empty_object_type;
        }
        if source.flags().intersects(TypeFlags::Union) {
            return self
                .map_type(source, |c, t| Some(c.get_rest_type(t, properties, symbol)))
                .unwrap();
        }
        let key_types: Vec<P<Type>> =
            properties.iter().map(|&p| self.get_literal_type_from_property_name(p)).collect();
        let mut omit_key_type = self.get_union_type(&key_types);
        let mut spreadable_properties: Vec<P<Symbol>> = Vec::new();
        let mut unspreadable_to_rest_keys: Vec<P<Type>> = Vec::new();
        for prop in self.get_properties_of_type(source).iter().copied() {
            let literal_type_from_property = self.get_literal_type_from_property(
                prop,
                TypeFlags::StringOrNumberLiteralOrUnique,
                false,
            );
            if !self.is_type_assignable_to(literal_type_from_property, omit_key_type)
                && !get_declaration_modifier_flags_from_symbol(prop)
                    .intersects(ModifierFlags::Private | ModifierFlags::Protected)
                && Self::is_spreadable_property(prop)
            {
                spreadable_properties.push(prop);
            } else {
                unspreadable_to_rest_keys.push(literal_type_from_property);
            }
        }
        if self.is_generic_object_type(source) || self.is_generic_index_type(omit_key_type) {
            if !unspreadable_to_rest_keys.is_empty() {
                // If the type we're spreading from has properties that cannot
                // be spread into the rest type (e.g. getters, methods), ensure
                // they are explicitly omitted, as they would in the non-generic case.
                let mut types = vec![omit_key_type];
                types.extend(unspreadable_to_rest_keys);
                omit_key_type = self.get_union_type(&types);
            }
            if omit_key_type.flags().intersects(TypeFlags::Never) {
                return source;
            }
            let omit_type_alias = self.get_global_omit_symbol();
            let Some(omit_type_alias) = omit_type_alias else {
                return self.error_type;
            };
            return self.get_type_alias_instantiation(
                omit_type_alias,
                &[source, omit_key_type],
                None,
            );
        }
        let members = SymbolTable::new();
        for prop in spreadable_properties {
            let s = self.get_spread_symbol(prop, false /*readonly*/);
            members.set(prop.name(), s);
        }
        let index_infos = self.get_index_infos_of_type(source);
        let result = self.new_anonymous_type(symbol, Some(members), &[], &[], index_infos);
        result.object_flags.set(result.object_flags.get_lazy() | ObjectFlags::ObjectRestType);
        result
    }

    // Determine the control flow type associated with a destructuring declaration or assignment. The following
    // forms of destructuring are possible:
    //
    //	let { x } = obj;  // BindingElement
    //	let [ x ] = obj;  // BindingElement
    //	{ x } = obj;      // ShorthandPropertyAssignment
    //	{ x: v } = obj;   // PropertyAssignment
    //	[ x ] = obj;      // Expression
    //
    // We construct a synthetic element access expression corresponding to 'obj.x' such that the control
    // flow analyzer doesn't have to handle all the different syntactic forms.
    // checker.go:18189
    pub(crate) fn get_flow_type_of_destructuring(
        &mut self,
        node: P<Node>,
        declared_type: P<Type>,
    ) -> P<Type> {
        let reference = self.get_synthetic_element_access(node);
        if let Some(reference) = reference {
            return self.get_flow_type_of_reference(reference, declared_type);
        }
        declared_type
    }

    // checker.go:18197
    fn get_synthetic_element_access(&mut self, node: P<Node>) -> Option<P<Node>> {
        let parent_access = self.get_parent_element_access(node);
        if let Some(parent_access) = parent_access
            && get_flow_node_of_node(parent_access).is_some()
        {
            let (prop_name, ok) = self.get_destructuring_property_name(node);
            if ok {
                let literal =
                    self.factory.new_string_literal(alloc_str(&prop_name), TokenFlags::None);
                literal.set_loc(node.loc());
                let mut lhs_expr = parent_access;
                if !is_left_hand_side_expression(parent_access) {
                    lhs_expr = self.factory.new_parenthesized_expression(parent_access);
                    lhs_expr.set_loc(node.loc());
                }
                let result = self.factory.new_element_access_expression(
                    lhs_expr,
                    None,
                    literal,
                    NodeFlags::None,
                );
                result.set_loc(node.loc());
                literal.set_parent(Some(result));
                result.set_parent(Some(node));
                if lhs_expr != parent_access {
                    lhs_expr.set_parent(Some(result));
                }
                result.set_flow_node(get_flow_node_of_node(parent_access));
                return Some(result);
            }
        }
        None
    }

    // checker.go:18222
    fn get_parent_element_access(&mut self, node: P<Node>) -> Option<P<Node>> {
        let ancestor = node.parent().unwrap().parent().unwrap();
        match ancestor.kind() {
            Kind::BindingElement | Kind::PropertyAssignment => {
                return self.get_synthetic_element_access(ancestor);
            }
            Kind::ArrayLiteralExpression => {
                return self.get_synthetic_element_access(node.parent().unwrap());
            }
            Kind::VariableDeclaration => return ancestor.initializer(),
            Kind::BinaryExpression => return Some(ancestor.as_binary_expression().right()),
            _ => {}
        }
        None
    }

    // Return the type implied by a binding pattern. This is the type implied purely by the binding pattern itself
    // and without regard to its context (i.e. without regard any type annotation or initializer associated with the
    // declaration in which the binding pattern is contained). For example, the implied type of [x, y] is [any, any]
    // and the implied type of { x, y: z = 1 } is { x: any; y: number; }. The type implied by a binding pattern is
    // used as the contextual type of an initializer associated with the binding pattern. Also, for a destructuring
    // parameter with no type annotation or initializer, the type implied by the binding pattern becomes the type of
    // the parameter.
    // checker.go:18244
    pub(crate) fn get_type_from_binding_pattern(
        &mut self,
        pattern: P<Node>,
        include_pattern_in_type: bool,
        report_errors: bool,
    ) -> P<Type> {
        if include_pattern_in_type {
            self.contextual_binding_patterns.push(pattern);
        }
        let result = if is_object_binding_pattern(pattern) {
            self.get_type_from_object_binding_pattern(
                pattern,
                include_pattern_in_type,
                report_errors,
            )
        } else {
            self.get_type_from_array_binding_pattern(
                pattern,
                include_pattern_in_type,
                report_errors,
            )
        };
        if include_pattern_in_type {
            self.contextual_binding_patterns.pop();
        }
        result
    }

    // Return the type implied by an object binding pattern
    // checker.go:18261
    fn get_type_from_object_binding_pattern(
        &mut self,
        pattern: P<Node>,
        include_pattern_in_type: bool,
        report_errors: bool,
    ) -> P<Type> {
        let members = SymbolTable::new();
        let mut string_index_info: Option<P<IndexInfo>> = None;
        let mut object_flags =
            ObjectFlags::ObjectLiteral | ObjectFlags::ContainsObjectOrArrayLiteral;
        for &e in pattern.elements() {
            let name = e.property_name_or_name();
            if has_dot_dot_dot_token(e) {
                string_index_info = Some(Self::new_index_info(
                    self.string_type,
                    self.any_type,
                    false, /*isReadonly*/
                    None,
                    &[],
                ));
                continue;
            }
            let expr_type = self.get_literal_type_from_property_name(name.unwrap());
            if !is_type_usable_as_property_name(expr_type) {
                // do not include computed properties in the implied type
                object_flags |= ObjectFlags::ObjectLiteralPatternWithComputedProperties;
                continue;
            }
            let text = get_property_name_from_type(expr_type);
            let flags = SymbolFlags::Property
                | if e.initializer().is_some() { SymbolFlags::Optional } else { SymbolFlags::None };
            let symbol = self.new_symbol(flags, alloc_str(&text));
            let t = self.get_type_from_binding_element(e, include_pattern_in_type, report_errors);
            self.value_symbol_links.get(symbol).resolved_type.set(Some(t));
            members.set(symbol.name(), symbol);
        }
        let index_infos: Vec<P<IndexInfo>> = match string_index_info {
            Some(info) => vec![info],
            None => Vec::new(),
        };
        let result = self.new_anonymous_type(None, Some(members), &[], &[], &index_infos);
        result.object_flags.set(result.object_flags.get_lazy() | object_flags);
        if include_pattern_in_type {
            self.pattern_for_type.insert(result, pattern);
            result
                .object_flags
                .set(result.object_flags.get_lazy() | ObjectFlags::ContainsObjectOrArrayLiteral);
        }
        result
    }

    // Return the type implied by an array binding pattern
    // checker.go:18297
    fn get_type_from_array_binding_pattern(
        &mut self,
        pattern: P<Node>,
        include_pattern_in_type: bool,
        report_errors: bool,
    ) -> P<Type> {
        let elements = pattern.elements();
        let last_element = elements.last().copied();
        let mut rest_element: Option<P<Node>> = None;
        if let Some(last_element) = last_element
            && is_binding_element(last_element)
            && has_dot_dot_dot_token(last_element)
        {
            rest_element = Some(last_element);
        }
        if elements.is_empty() || elements.len() == 1 && rest_element.is_some() {
            // TODO: remove ScriptTargetES2015
            if self.language_version >= ScriptTarget::ES2015 {
                return self.create_iterable_type(self.any_type);
            }
            return self.any_array_type;
        }
        let mut min_length: usize = 0;
        for (i, element) in elements.iter().enumerate().rev() {
            let e = *element;
            if !(Some(e) == rest_element || e.name().is_none() || Self::has_default_value(e)) {
                min_length = i + 1;
                break;
            }
        }
        let mut element_types: Vec<P<Type>> = Vec::with_capacity(elements.len());
        let mut element_infos: Vec<TupleElementInfo> = Vec::with_capacity(elements.len());
        (element_infos, element_types) = elements.iter().enumerate().fold(
            (element_infos, element_types),
            |(mut element_infos, mut element_types), (i, &e)| {
                let t = if e.name().is_none() {
                    self.any_type
                } else {
                    self.get_type_from_binding_element(e, include_pattern_in_type, report_errors)
                };
                let flags = if Some(e) == rest_element {
                    ElementFlags::Rest
                } else if i >= min_length {
                    ElementFlags::Optional
                } else {
                    ElementFlags::Required
                };
                element_types.push(t);
                element_infos.push(TupleElementInfo { flags, labeled_declaration: None });

                (element_infos, element_types)
            },
        );
        let mut result = self.create_tuple_type_ex(&element_types, &element_infos, false);
        if include_pattern_in_type {
            result = self.clone_type_reference(result);
            self.pattern_for_type.insert(result, pattern);
            result
                .object_flags
                .set(result.object_flags.get_lazy() | ObjectFlags::ContainsObjectOrArrayLiteral);
        }
        result
    }

    // Return the type implied by a binding pattern element. This is the type of the initializer of the element if
    // one is present. Otherwise, if the element is itself a binding pattern, it is the type implied by the binding
    // pattern. Otherwise, it is the type any.
    // checker.go:18346
    pub(crate) fn get_type_from_binding_element(
        &mut self,
        element: P<Node>,
        include_pattern_in_type: bool,
        report_errors: bool,
    ) -> P<Type> {
        if element.initializer().is_some() {
            // The type implied by a binding pattern is independent of context, so we check the initializer with no
            // contextual type or, if the element itself is a binding pattern, with the type implied by that binding
            // pattern.
            let mut contextual_type = self.unknown_type;
            if is_binding_pattern(element.name().unwrap()) {
                contextual_type = self.get_type_from_binding_pattern(
                    element.name().unwrap(),
                    true,  /*includePatternInType*/
                    false, /*reportErrors*/
                );
            }
            let init_type = self.check_declaration_initializer(
                element,
                CheckMode::Normal,
                Some(contextual_type),
            );
            let widened = self.get_widened_literal_type_for_initializer(element, init_type);
            return self.add_optionality(widened);
        }
        if is_binding_pattern(element.name().unwrap()) {
            return self.get_type_from_binding_pattern(
                element.name().unwrap(),
                include_pattern_in_type,
                report_errors,
            );
        }
        if report_errors && !Self::declaration_belongs_to_private_ambient_member(element) {
            self.report_implicit_any(element, self.any_type, WideningKind::Normal);
        }
        // When we're including the pattern in the type (an indication we're obtaining a contextual type), we
        // use a non-inferrable any type. Inference will never directly infer this type, but it is possible
        // to infer a type that contains it, e.g. for a binding pattern like [foo] or { foo }. In such cases,
        // widening of the binding pattern type substitutes a regular any for the non-inferrable any.
        if include_pattern_in_type {
            return self.non_inferrable_any_type;
        }
        self.any_type
    }

    // checker.go:18373
    fn declaration_belongs_to_private_ambient_member(declaration: P<Node>) -> bool {
        let mut member_declaration = get_root_declaration(declaration);
        if is_parameter_declaration(member_declaration) {
            member_declaration = member_declaration.parent().unwrap();
        }
        is_private_within_ambient(member_declaration)
    }

    // checker.go:18381
    pub(crate) fn get_type_of_prototype_property(&mut self, prototype: P<Symbol>) -> P<Type> {
        // TypeScript 1.0 spec (April 2014): 8.4
        // Every class automatically contains a static property member named 'prototype',
        // the type of which is an instantiation of the class type with type Any supplied as a type argument for each type parameter.
        // It is an error to explicitly declare a static property member with the name 'prototype'.
        let parent = self.get_parent_of_symbol(prototype).unwrap();
        let class_type = self.get_declared_type_of_symbol(parent);
        let type_parameters = class_type.as_interface_type().type_parameters();
        if !type_parameters.is_empty() {
            let any_types: Vec<P<Type>> = type_parameters.iter().map(|_| self.any_type).collect();
            return self.create_type_reference(class_type, &any_types);
        }
        class_type
    }

    // checker.go:18403
    pub(crate) fn get_widened_type_for_assignment_declaration(
        &mut self,
        symbol: P<Symbol>,
    ) -> P<Type> {
        let mut t: Option<P<Type>> = None;
        let (kind, location) = self.is_constructor_declared_this_property(symbol);
        match kind {
            ThisAssignmentDeclarationKind::Typed => {
                let Some(location) = location else {
                    panic!("location should not be nil when this assignment has a type.");
                };
                t = Some(self.get_type_from_type_node(location));
            }
            ThisAssignmentDeclarationKind::Constructor => {
                let Some(location) = location else {
                    panic!(
                        "constructor should not be nil when this assignment is in a constructor."
                    );
                };
                t = self.get_flow_type_in_constructor(symbol, location);
            }
            ThisAssignmentDeclarationKind::Method => {
                t = self.get_type_of_property_in_base_class(symbol);
            }
            ThisAssignmentDeclarationKind::None => {}
        }
        if t.is_none() {
            let mut types: Vec<P<Type>> = Vec::new();
            let declarations = symbol.declarations();
            for (i, &declaration) in declarations.iter().enumerate() {
                if is_binary_expression(declaration) && declaration.type_node().is_some() {
                    t = Some(self.get_type_from_type_node(declaration.type_node().unwrap()));
                    break;
                }
                if let Some(assigned_type) =
                    self.get_assignment_declaration_initializer_type(declaration)
                {
                    // We ignore initial assignments of undefined to CommonJS exports when there are multiple assignment declarations
                    if get_assignment_declaration_kind(declaration)
                        != JSDeclarationKind::ExportsProperty
                        || i != 0
                        || declarations.len() == 1
                        || !assigned_type.flags().intersects(TypeFlags::Undefined)
                    {
                        append_if_unique(&mut types, assigned_type);
                    }
                }
            }
            if kind == ThisAssignmentDeclarationKind::Method
                && !types.is_empty()
                && self.strict_null_checks
            {
                append_if_unique(&mut types, self.undefined_or_missing_type);
            }
            if t.is_none() {
                let mut u = self.any_type;
                if !types.is_empty() {
                    u = self.get_union_type(&types);
                }
                t = Some(u);
            }
        }
        let t = self.get_widened_type(t.unwrap());
        // report an all-nullable or empty union as an implicit any in JS files
        if let Some(value_declaration) = symbol.value_declaration()
            && is_in_js_file(value_declaration)
            && self.filter_type(t, |_, c| !(c.flags() & !TypeFlags::Nullable).is_empty())
                == self.never_type
        {
            self.report_implicit_any(value_declaration, self.any_type, WideningKind::Normal);
            return self.any_type;
        }
        t
    }

    // checker.go:18456
    fn get_assignment_declaration_initializer_type(&mut self, node: P<Node>) -> Option<P<Type>> {
        if is_binary_expression(node) {
            let t: P<Type> = match get_assignment_declaration_kind(node) {
                JSDeclarationKind::ModuleExports | JSDeclarationKind::ExportsProperty => {
                    let e = self.check_expression_cached(get_right_most_assigned_expression(node));
                    self.get_regular_type_of_literal_type(e)
                }
                kind => {
                    if kind == JSDeclarationKind::ThisProperty {
                        let bin = node.as_binary_expression();
                        if self.contains_same_named_this_property(bin.left(), bin.right()) {
                            return None;
                        }
                    }
                    // fallthrough
                    self.check_expression_for_mutable_location(
                        node.as_binary_expression().right(),
                        CheckMode::Normal,
                    )
                }
            };
            if self.is_empty_array_literal_type(t)
                && !self.has_parent_with_type_annotation(node.symbol().unwrap())
            {
                self.report_implicit_any(node, self.any_array_type, WideningKind::Normal);
                return Some(self.any_array_type);
            }
            return Some(t);
        }
        if is_call_expression(node) {
            return Some(self.get_type_from_property_descriptor(node.arguments()[2]));
        }
        None
    }

    // Return true if the parent symbol of the given assignment declaration symbol has declaration with a type
    // annotation. For example, returns true for the symbol associated with `f.a` below:
    //
    //	const f: { (): void, a: string[] } = () => {};
    //	f.a = [];
    // checker.go:18487
    fn has_parent_with_type_annotation(&mut self, symbol: P<Symbol>) -> bool {
        if let Some(parent) = symbol.parent()
            && let Some(parent_value_declaration) = parent.value_declaration()
            && is_function_expression_or_arrow_function(parent_value_declaration)
            && let Some(possibly_annotated_symbol) =
                self.get_symbol_of_node(parent_value_declaration.parent().unwrap())
            && let Some(value_declaration) = possibly_annotated_symbol.value_declaration()
        {
            return value_declaration.type_node().is_some();
        }
        false
    }

    // checker.go:18496
    fn contains_same_named_this_property(
        &mut self,
        this_property: P<Node>,
        expression: P<Node>,
    ) -> bool {
        fn visit(c: &mut Checker, this_property: P<Node>, node: P<Node>) -> bool {
            if c.is_matching_reference(this_property, node) {
                return true;
            }
            if is_function_like(node) {
                return false;
            }
            node.for_each_child(&mut |child| visit(c, this_property, child))
        }
        visit(self, this_property, expression)
    }

    // checker.go:18510
    fn get_type_from_property_descriptor(&mut self, node: P<Node>) -> P<Type> {
        let object_literal_type = self.check_expression_cached(node);
        if let Some(value_type) = self.get_type_of_property_of_type(object_literal_type, "value") {
            return value_type;
        }
        if let Some(get_func) = self.get_type_of_property_of_type(object_literal_type, "get")
            && let Some(get_sig) = self.get_single_call_signature(get_func)
        {
            return self.get_return_type_of_signature(get_sig);
        }
        if let Some(set_func) = self.get_type_of_property_of_type(object_literal_type, "set")
            && let Some(set_sig) = self.get_single_call_signature(set_func)
        {
            return self.get_type_of_first_parameter_of_signature(set_sig);
        }
        self.any_type
    }

    // A property is considered a constructor declared property when all declaration sites are this.xxx assignments,
    // when no declaration sites have JSDoc type annotations, and when at least one declaration site is in the body of
    // a class constructor.
    // checker.go:18531
    pub(crate) fn is_constructor_declared_this_property(
        &mut self,
        symbol: P<Symbol>,
    ) -> (ThisAssignmentDeclarationKind, Option<P<Node>>) {
        match symbol.value_declaration() {
            Some(value_declaration) if is_binary_expression(value_declaration) => {}
            _ => return (ThisAssignmentDeclarationKind::None, None),
        }
        if let Some(&kind) = self.this_expando_kinds.get(&symbol) {
            let Some(&location) = self.this_expando_locations.get(&symbol) else {
                panic!("location should be cached whenever this expando symbol is cached");
            };
            return (kind, location);
        }
        let mut all_this = true;
        let mut type_annotation: Option<P<Node>> = None;
        let declarations = symbol.declarations();
        for &declaration in declarations {
            if !is_binary_expression(declaration) {
                all_this = false;
                break;
            }
            let bin = declaration.as_binary_expression();
            if get_assignment_declaration_kind(declaration) == JSDeclarationKind::ThisProperty
                && (bin.left().kind() != Kind::ElementAccessExpression
                    || is_string_or_numeric_literal_like(
                        bin.left().as_element_access_expression().argument_expression,
                    ))
            {
                if bin.type_().is_some() {
                    type_annotation = bin.type_();
                }
            } else {
                all_this = false;
                break;
            }
        }
        let mut location: Option<P<Node>> = None;
        let mut kind = ThisAssignmentDeclarationKind::None;
        if all_this {
            if type_annotation.is_some() {
                location = type_annotation;
                kind = ThisAssignmentDeclarationKind::Typed;
            } else {
                location = Self::get_declaring_constructor(symbol);
                kind = if location.is_none() {
                    ThisAssignmentDeclarationKind::Method
                } else {
                    ThisAssignmentDeclarationKind::Constructor
                };
            }
        }
        self.this_expando_kinds.insert(symbol, kind);
        self.this_expando_locations.insert(symbol, location);
        (kind, location)
    }

    // checker.go:18576
    fn is_global_symbol_constructor(&mut self, node: P<Node>) -> bool {
        let symbol = self.get_symbol_of_node(node);
        let global_symbol = self.get_global_es_symbol_constructor_type_symbol_or_nil();
        global_symbol.is_some() && symbol == global_symbol
    }

    // checker.go:18582
    pub(crate) fn widen_type_for_variable_like_declaration(
        &mut self,
        t: Option<P<Type>>,
        declaration: P<Node>,
        report_errors: bool,
    ) -> P<Type> {
        if let Some(mut t) = t {
            // This special case is required for backwards compatibility with libraries that merge a `symbol` property into `SymbolConstructor`.
            // See https://github.com/microsoft/TypeScript/tsc/issues/1212
            if t.flags().intersects(TypeFlags::ESSymbol)
                && self.is_global_symbol_constructor(declaration.parent().unwrap())
            {
                t = self.get_es_symbol_like_type_for_node(declaration);
            }

            if report_errors {
                self.report_errors_from_widening(declaration, t, WideningKind::Normal);
            }

            // always widen a 'unique symbol' type if the type was created for a different declaration.
            if t.flags().intersects(TypeFlags::UniqueESSymbol)
                && (is_binding_element(declaration) || declaration.type_node().is_none())
                && t.symbol() != self.get_symbol_of_declaration(declaration)
            {
                t = self.es_symbol_type;
            }
            return self.get_widened_type(t);
        }
        // Rest parameters default to type any[], other parameters default to type any
        let t = if is_parameter_declaration(declaration)
            && declaration.as_parameter_declaration().dot_dot_dot_token().is_some()
        {
            self.any_array_type
        } else {
            self.any_type
        };
        // Report implicit any errors unless this is a private property within an ambient declaration
        if report_errors && !declaration_belongs_to_private_ambient_member(declaration) {
            self.report_implicit_any(declaration, t, WideningKind::Normal);
        }
        t
    }

    // checker.go:18615
    pub(crate) fn report_implicit_any(
        &mut self,
        declaration: P<Node>,
        t: P<Type>,
        widening_kind: WideningKind,
    ) {
        if is_in_js_file(declaration)
            && !is_check_js_enabled_for_file(
                get_source_file_of_node(declaration).unwrap(),
                &self.compiler_options,
            )
        {
            // Only report implicit any errors/suggestions in TS and ts-check JS files
            return;
        }
        let widened = self.get_widened_type(t);
        let type_as_string = self.type_to_string(widened, None);
        let diagnostic: &'static Message;
        match declaration.kind() {
            Kind::BinaryExpression | Kind::PropertyDeclaration | Kind::PropertySignature => {
                diagnostic = if self.no_implicit_any {
                    &diagnostics::MEMBER_0_IMPLICITLY_HAS_AN_1_TYPE
                } else {
                    &diagnostics::MEMBER_0_IMPLICITLY_HAS_AN_1_TYPE_BUT_A_BETTER_TYPE_MAY_BE_INFERRED_FROM_USAGE
                };
            }
            Kind::Parameter => {
                let param = declaration.as_parameter_declaration();
                if is_identifier(param.name()) {
                    let name = param.name();
                    let original_keyword_kind = tsrs_scanner::identifier_to_keyword_kind(name);
                    let parent = declaration.parent().unwrap();
                    if (is_call_signature_declaration(parent)
                        || is_method_signature_declaration(parent)
                        || is_function_type_node(parent))
                        && parent.parameters().contains(&declaration)
                        && (is_type_node_kind(original_keyword_kind)
                            || self
                                .resolve_name(
                                    Some(declaration),
                                    name.text(),
                                    SymbolFlags::Type,
                                    None,  /*nameNotFoundMessage*/
                                    true,  /*isUse*/
                                    false, /*excludeGlobals*/
                                )
                                .is_some())
                    {
                        let index = parent
                            .parameters()
                            .iter()
                            .position(|&p| p == declaration)
                            .expect("parameter belongs to its declaration");
                        let new_name = format!("arg{index}");
                        let type_name =
                            tsrs_scanner::declaration_name_to_string(Some(param.name()))
                                + if param.dot_dot_dot_token().is_some() { "[]" } else { "" };
                        self.error_or_suggestion(
                            self.no_implicit_any,
                            Some(declaration),
                            &diagnostics::PARAMETER_HAS_A_NAME_BUT_NO_TYPE_DID_YOU_MEAN_0_COLON_1,
                            &[&new_name, &type_name],
                        );
                        return;
                    }
                }
                if param.dot_dot_dot_token().is_some() {
                    if self.no_implicit_any {
                        diagnostic = &diagnostics::REST_PARAMETER_0_IMPLICITLY_HAS_AN_ANY_TYPE;
                    } else {
                        diagnostic = &diagnostics::REST_PARAMETER_0_IMPLICITLY_HAS_AN_ANY_TYPE_BUT_A_BETTER_TYPE_MAY_BE_INFERRED_FROM_USAGE;
                    }
                } else if self.no_implicit_any {
                    diagnostic = &diagnostics::PARAMETER_0_IMPLICITLY_HAS_AN_1_TYPE;
                } else {
                    diagnostic = &diagnostics::PARAMETER_0_IMPLICITLY_HAS_AN_1_TYPE_BUT_A_BETTER_TYPE_MAY_BE_INFERRED_FROM_USAGE;
                }
            }
            Kind::BindingElement => {
                diagnostic = &diagnostics::BINDING_ELEMENT_0_IMPLICITLY_HAS_AN_1_TYPE;
                if !self.no_implicit_any {
                    // Don't issue a suggestion for binding elements since the codefix doesn't yet support them.
                    return;
                }
            }
            Kind::FunctionDeclaration
            | Kind::MethodDeclaration
            | Kind::MethodSignature
            | Kind::GetAccessor
            | Kind::SetAccessor
            | Kind::FunctionExpression
            | Kind::ArrowFunction => {
                if self.no_implicit_any && declaration.name().is_none() {
                    if widening_kind == WideningKind::GeneratorYield {
                        self.error(
                            Some(declaration),
                            &diagnostics::GENERATOR_IMPLICITLY_HAS_YIELD_TYPE_0_CONSIDER_SUPPLYING_A_RETURN_TYPE_ANNOTATION,
                            &[&type_as_string],
                        );
                    } else {
                        self.error(
                            Some(declaration),
                            &diagnostics::FUNCTION_EXPRESSION_WHICH_LACKS_RETURN_TYPE_ANNOTATION_IMPLICITLY_HAS_AN_0_RETURN_TYPE,
                            &[&type_as_string],
                        );
                    }
                    return;
                }
                if !self.no_implicit_any {
                    diagnostic = &diagnostics::X_0_IMPLICITLY_HAS_AN_1_RETURN_TYPE_BUT_A_BETTER_TYPE_MAY_BE_INFERRED_FROM_USAGE;
                } else if declaration.flags().intersects(NodeFlags::Reparsed) {
                    let name = tsrs_scanner::declaration_name_to_string(get_name_of_declaration(
                        declaration,
                    ));
                    if name.is_empty() {
                        self.error(
                            Some(declaration),
                            &diagnostics::THIS_OVERLOAD_IMPLICITLY_RETURNS_THE_TYPE_0_BECAUSE_IT_LACKS_A_RETURN_TYPE_ANNOTATION,
                            &[&type_as_string],
                        );
                    } else {
                        self.error(
                            Some(declaration),
                            &diagnostics::X_0_WHICH_LACKS_RETURN_TYPE_ANNOTATION_IMPLICITLY_HAS_AN_1_RETURN_TYPE,
                            &[&name, &type_as_string],
                        );
                    }
                    return;
                } else if widening_kind == WideningKind::GeneratorYield {
                    diagnostic = &diagnostics::X_0_WHICH_LACKS_RETURN_TYPE_ANNOTATION_IMPLICITLY_HAS_AN_1_YIELD_TYPE;
                } else {
                    diagnostic = &diagnostics::X_0_WHICH_LACKS_RETURN_TYPE_ANNOTATION_IMPLICITLY_HAS_AN_1_RETURN_TYPE;
                }
            }
            Kind::MappedType => {
                if self.no_implicit_any {
                    self.error(
                        Some(declaration),
                        &diagnostics::MAPPED_OBJECT_TYPE_IMPLICITLY_HAS_AN_ANY_TEMPLATE_TYPE,
                        &[],
                    );
                }
                return;
            }
            _ => {
                if self.no_implicit_any {
                    diagnostic = &diagnostics::VARIABLE_0_IMPLICITLY_HAS_AN_1_TYPE;
                } else {
                    diagnostic = &diagnostics::VARIABLE_0_IMPLICITLY_HAS_AN_1_TYPE_BUT_A_BETTER_TYPE_MAY_BE_INFERRED_FROM_USAGE;
                }
            }
        }
        let name = tsrs_scanner::declaration_name_to_string(get_name_of_declaration(declaration));
        self.error_or_suggestion(
            self.no_implicit_any,
            Some(declaration),
            diagnostic,
            &[&name, &type_as_string],
        );
    }

    // checker.go:18699
    pub(crate) fn get_widened_type(&mut self, t: P<Type>) -> P<Type> {
        self.get_widened_type_with_context(t, None /*context*/)
    }

    // checker.go:18703
    fn get_widened_type_with_context(
        &mut self,
        t: P<Type>,
        context: Option<P<WideningContext>>,
    ) -> P<Type> {
        if t.object_flags().intersects(ObjectFlags::RequiresWidening) {
            if context.is_none()
                && let Some(&cached) = self
                    .cached_types
                    .get(&CachedTypeKey { kind: CachedTypeKind::Widened, type_id: t.id })
            {
                return cached;
            }
            let mut result: Option<P<Type>> = None;
            if t.flags().intersects(TypeFlags::Any | TypeFlags::Nullable) {
                result = Some(self.any_type);
            } else if is_object_literal_type(t) {
                result = Some(self.get_widened_type_of_object_literal(t, context));
            } else if t.flags().intersects(TypeFlags::Union) {
                let union_context = match context {
                    Some(context) => context,
                    None => P::new(WideningContext {
                        siblings: Cell::new(Some(t.types())),
                        ..Default::default()
                    }),
                };
                let widened_types: Vec<P<Type>> = t
                    .types()
                    .iter()
                    .map(|&t| {
                        if t.flags().intersects(TypeFlags::Nullable) {
                            return t;
                        }
                        self.get_widened_type_with_context(t, Some(union_context))
                    })
                    .collect();
                // Widening an empty object literal transitions from a highly restrictive type to
                // a highly inclusive one. For that reason we perform subtype reduction here if the
                // union includes empty object types (e.g. reducing {} | string to just {}).
                let reduction = if widened_types.iter().any(|&t| self.is_empty_object_type(t)) {
                    UnionReduction::Subtype
                } else {
                    UnionReduction::Literal
                };
                result =
                    Some(self.get_union_type_ex(&widened_types, reduction, AliasArg::None, None));
            } else if t.flags().intersects(TypeFlags::Intersection) {
                let types: Vec<P<Type>> =
                    t.types().iter().map(|&t| self.get_widened_type(t)).collect();
                result = Some(self.get_intersection_type(&types));
            } else if self.is_array_or_tuple_type(t) {
                let type_arguments: Vec<P<Type>> =
                    self.get_type_arguments(t).iter().map(|&t| self.get_widened_type(t)).collect();
                result = Some(self.create_type_reference(t.target().unwrap(), &type_arguments));
            }
            if let Some(result) = result
                && context.is_none()
            {
                self.cached_types
                    .insert(CachedTypeKey { kind: CachedTypeKind::Widened, type_id: t.id }, result);
            }
            return result.unwrap_or(t);
        }
        t
    }

    // checker.go:18744
    fn get_widened_type_of_object_literal(
        &mut self,
        t: P<Type>,
        context: Option<P<WideningContext>>,
    ) -> P<Type> {
        if let Some(context) = context
            && let Some(cached) = context.widened_types.get(&t)
        {
            return cached;
        }
        let members = SymbolTable::new();
        for prop in self.get_properties_of_object_type(t).iter().copied() {
            let widened = self.get_widened_property(prop, context);
            members.set(prop.name(), widened);
        }
        if let Some(context) = context {
            for prop in self.get_properties_of_context(context) {
                if !members.has(prop.name()) {
                    let undefined_prop = self.get_undefined_property(prop);
                    members.set(prop.name(), undefined_prop);
                }
            }
        }
        let index_infos: Vec<P<IndexInfo>> = self
            .get_index_infos_of_type(t)
            .iter()
            .map(|info| {
                let value_type = self.get_widened_type(info.value_type());
                Self::new_index_info(
                    info.key_type(),
                    value_type,
                    info.is_readonly(),
                    info.declaration(),
                    info.components.get(),
                )
            })
            .collect();
        let result = self.new_anonymous_type(t.symbol(), Some(members), &[], &[], &index_infos);
        // Retain js literal flag through widening
        result.object_flags.set(
            result.object_flags.get_lazy()
                | (t.object_flags_lazy()
                    & (ObjectFlags::JSLiteral | ObjectFlags::NonInferrableType)),
        );
        // Only cache in child contexts since the root context never widens a particular object literal type more than once
        if let Some(context) = context
            && context.parent.get().is_some()
        {
            if context.widened_types.is_nil() {
                context.widened_types.make();
            }
            context.widened_types.set(t, result);
        }
        result
    }

    // checker.go:18776
    fn get_widened_property(
        &mut self,
        prop: P<Symbol>,
        context: Option<P<WideningContext>>,
    ) -> P<Symbol> {
        if !prop.flags().intersects(SymbolFlags::Property) {
            // Since get accessors already widen their return value there is no need to
            // widen accessor based properties here.
            return prop;
        }
        let original = self.get_type_of_symbol(prop);
        let mut prop_context: Option<P<WideningContext>> = None;
        if let Some(context) = context {
            prop_context = Some(WideningContext::get_child_context(context, prop.name()));
        }
        let widened = self.get_widened_type_with_context(original, prop_context);
        if widened == original {
            return prop;
        }
        self.create_symbol_with_type(prop, Some(widened))
    }
}

impl WideningContext {
    // Go method on *WideningContext; takes the arena handle explicitly because the new child
    // context stores a pointer to its parent.
    // checker.go:18794
    fn get_child_context(w: P<WideningContext>, property_name: &str) -> P<WideningContext> {
        if let Some(cached) = w.child_contexts.get(&property_name.to_string()) {
            return cached;
        }
        let result = P::new(WideningContext {
            parent: Cell::new(Some(w)),
            property_name: Cell::new(alloc_str(property_name)),
            ..Default::default()
        });
        if w.child_contexts.is_nil() {
            w.child_contexts.make();
        }
        w.child_contexts.set(property_name.to_string(), result);
        result
    }
}

impl Checker {
    // checker.go:18806
    fn get_properties_of_context(&mut self, context: P<WideningContext>) -> Vec<P<Symbol>> {
        if context.resolved_properties.get().is_none() {
            let mut names: OrderedMap<&'static str, P<Symbol>> = OrderedMap::default();
            for t in self.get_siblings_of_context(context) {
                if is_object_literal_type(t)
                    && !t.object_flags().intersects(ObjectFlags::ContainsSpread)
                {
                    names.extend(
                        self.get_properties_of_type(t)
                            .iter()
                            .copied()
                            .map(|prop| (prop.name(), prop)),
                    );
                }
            }
            let values: Vec<P<Symbol>> = names.values().copied().collect();
            context.resolved_properties.set(Some(alloc_vec(values)));
        }
        context.resolved_properties.get().unwrap().to_vec()
    }

    // checker.go:18821
    fn get_siblings_of_context(&mut self, context: P<WideningContext>) -> Vec<P<Type>> {
        if context.siblings.get().is_none() {
            let mut siblings: Vec<P<Type>> = Vec::new();
            siblings.extend(
                self.get_siblings_of_context(context.parent.get().unwrap())
                    .into_iter()
                    .filter_map(|t| {
                        if is_object_literal_type(t) {
                            let prop =
                                self.get_property_of_object_type(t, context.property_name.get());
                            if let Some(prop) = prop {
                                let prop_type = self.get_type_of_symbol(prop);
                                Some(prop_type.distributed_iter())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                    .flatten(),
            );
            context.siblings.set(Some(alloc_vec(siblings)));
        }
        context.siblings.get().unwrap().to_vec()
    }

    // checker.go:18929
    fn get_undefined_property(&mut self, prop: P<Symbol>) -> P<Symbol> {
        if let Some(&cached) = self.undefined_properties.get(&prop) {
            return cached;
        }
        let result = self.create_symbol_with_type(prop, Some(self.undefined_or_missing_type));
        result.flags.set(result.flags.get() | SymbolFlags::Optional);
        self.undefined_properties.insert(prop, result);
        result
    }

    // checker.go:18847
    pub(crate) fn get_type_of_enum_member(&mut self, symbol: P<Symbol>) -> P<Type> {
        let links = self.value_symbol_links.get(symbol);
        if links.resolved_type.get().is_none() {
            let t = self.get_declared_type_of_enum_member(symbol);
            links.resolved_type.set(Some(t));
        }
        links.resolved_type.get().unwrap()
    }

    // checker.go:18855
    pub(crate) fn get_type_of_accessors(&mut self, symbol: P<Symbol>) -> P<Type> {
        let links = self.value_symbol_links.get(symbol);
        if links.resolved_type.get().is_none() {
            if !self.push_type_resolution(symbol.into(), TypeSystemPropertyName::Type) {
                return self.error_type;
            }
            let getter = get_declaration_of_kind(symbol, Kind::GetAccessor);
            let setter = get_declaration_of_kind(symbol, Kind::SetAccessor);
            let accessor = symbol
                .declarations()
                .iter()
                .copied()
                .find(|&d| is_auto_accessor_property_declaration(d));
            // We try to resolve a getter type annotation, a setter type annotation, or a getter function
            // body return type inference, in that order.
            let mut t = self.get_annotated_accessor_type(getter);
            if t.is_none() {
                t = self.get_annotated_accessor_type(setter);
            }
            if t.is_none() {
                t = self.get_annotated_accessor_type(accessor);
            }
            if t.is_none()
                && let Some(getter) = getter
                && getter.body().is_some()
            {
                t = Some(self.get_return_type_from_body(getter, CheckMode::Normal));
            }
            if t.is_none()
                && let Some(accessor) = accessor
            {
                t = Some(self.get_widened_type_for_variable_like_declaration(
                    accessor, true, /*reportErrors*/
                ));
            }
            if t.is_none() {
                if setter.is_some() && !is_private_within_ambient(setter.unwrap()) {
                    let s = self.symbol_to_string(symbol);
                    self.error_or_suggestion(
                        self.no_implicit_any,
                        setter,
                        &diagnostics::PROPERTY_0_IMPLICITLY_HAS_TYPE_ANY_BECAUSE_ITS_SET_ACCESSOR_LACKS_A_PARAMETER_TYPE_ANNOTATION,
                        &[&s],
                    );
                } else if getter.is_some() && !is_private_within_ambient(getter.unwrap()) {
                    let s = self.symbol_to_string(symbol);
                    self.error_or_suggestion(
                        self.no_implicit_any,
                        getter,
                        &diagnostics::PROPERTY_0_IMPLICITLY_HAS_TYPE_ANY_BECAUSE_ITS_GET_ACCESSOR_LACKS_A_RETURN_TYPE_ANNOTATION,
                        &[&s],
                    );
                } else if accessor.is_some() && !is_private_within_ambient(accessor.unwrap()) {
                    let s = self.symbol_to_string(symbol);
                    self.error_or_suggestion(
                        self.no_implicit_any,
                        accessor,
                        &diagnostics::MEMBER_0_IMPLICITLY_HAS_AN_1_TYPE,
                        &[&s, &"any"],
                    );
                }
                t = Some(self.any_type);
            }
            if !self.pop_type_resolution() {
                if Self::get_annotated_accessor_type_node(getter).is_some() {
                    let s = self.symbol_to_string(symbol);
                    self.error(getter, &diagnostics::X_0_IS_REFERENCED_DIRECTLY_OR_INDIRECTLY_IN_ITS_OWN_TYPE_ANNOTATION, &[&s]);
                } else if (Self::get_annotated_accessor_type_node(setter).is_some())
                    || (Self::get_annotated_accessor_type_node(accessor).is_some())
                {
                    let s = self.symbol_to_string(symbol);
                    self.error(setter, &diagnostics::X_0_IS_REFERENCED_DIRECTLY_OR_INDIRECTLY_IN_ITS_OWN_TYPE_ANNOTATION, &[&s]);
                } else if getter.is_some() && self.no_implicit_any {
                    let s = self.symbol_to_string(symbol);
                    self.error(
                        getter,
                        &diagnostics::X_0_IMPLICITLY_HAS_RETURN_TYPE_ANY_BECAUSE_IT_DOES_NOT_HAVE_A_RETURN_TYPE_ANNOTATION_AND_IS_REFERENCED_DIRECTLY_OR_INDIRECTLY_IN_ONE_OF_ITS_RETURN_EXPRESSIONS,
                        &[&s],
                    );
                }
                t = Some(self.any_type);
            }
            if links.resolved_type.get().is_none() {
                links.resolved_type.set(t);
            }
        }
        links.resolved_type.get().unwrap()
    }

    // checker.go:18910
    pub(crate) fn get_write_type_of_accessors(&mut self, symbol: P<Symbol>) -> P<Type> {
        let links = self.value_symbol_links.get(symbol);
        if links.write_type().is_none() {
            if !self.push_type_resolution(symbol.into(), TypeSystemPropertyName::WriteType) {
                return self.error_type;
            }
            let mut setter = get_declaration_of_kind(symbol, Kind::SetAccessor);
            if setter.is_none() {
                let prop_declaration = get_declaration_of_kind(symbol, Kind::PropertyDeclaration);
                if let Some(prop_declaration) = prop_declaration
                    && is_auto_accessor_property_declaration(prop_declaration)
                {
                    setter = Some(prop_declaration);
                }
            }
            let mut write_type = self.get_annotated_accessor_type(setter);
            if !self.pop_type_resolution() {
                if Self::get_annotated_accessor_type_node(setter).is_some() {
                    let s = self.symbol_to_string(symbol);
                    self.error(setter, &diagnostics::X_0_IS_REFERENCED_DIRECTLY_OR_INDIRECTLY_IN_ITS_OWN_TYPE_ANNOTATION, &[&s]);
                }
                write_type = Some(self.any_type);
            }
            // Absent an explicit setter type annotation we use the read type of the accessor.
            if links.write_type().is_none() {
                if write_type.is_some() {
                    links.set_write_type(write_type);
                } else {
                    let t = self.get_type_of_accessors(symbol);
                    links.set_write_type(Some(t));
                }
            }
        }
        links.write_type().unwrap()
    }

    // checker.go:18942
    pub(crate) fn get_type_of_alias(&mut self, symbol: P<Symbol>) -> P<Type> {
        let links = self.value_symbol_links.get(symbol);
        if links.resolved_type.get().is_none() {
            if !self.push_type_resolution(symbol.into(), TypeSystemPropertyName::Type) {
                return self.error_type;
            }
            let target_symbol = self.resolve_alias(symbol);
            let alias_declaration = Self::get_declaration_of_alias_symbol(symbol);
            let export_symbol = self.get_target_of_alias_declaration(alias_declaration);
            // It only makes sense to get the type of a value symbol. If the result of resolving
            // the alias is not a value, then it has no type. To get the type associated with a
            // type symbol, call getDeclaredTypeOfSymbol.
            // This check is important because without it, a call to getTypeOfSymbol could end
            // up recursively calling getTypeOfAlias, causing a stack overflow.
            if links.resolved_type.get().is_none() {
                if self.get_symbol_flags(target_symbol).intersects(SymbolFlags::Value) {
                    let t = self.get_type_of_symbol(target_symbol);
                    links.resolved_type.set(Some(t));
                } else {
                    links.resolved_type.set(Some(self.error_type));
                }
            }
            if !self.pop_type_resolution() {
                self.report_circularity_error(export_symbol.unwrap_or(symbol));
                if links.resolved_type.get().is_none() {
                    links.resolved_type.set(Some(self.error_type));
                }
                return links.resolved_type.get().unwrap();
            }
        }
        links.resolved_type.get().unwrap()
    }

    // checker.go:18973
    pub(crate) fn add_optionality(&mut self, t: P<Type>) -> P<Type> {
        self.add_optionality_ex(t, false /*isProperty*/, true /*isOptional*/)
    }

    // checker.go:18977
    pub(crate) fn add_optionality_ex(
        &mut self,
        t: P<Type>,
        is_property: bool,
        is_optional: bool,
    ) -> P<Type> {
        if self.strict_null_checks && is_optional {
            return self.get_optional_type(t, is_property);
        }
        t
    }

    // checker.go:18984
    pub(crate) fn get_optional_type(&mut self, t: P<Type>, is_property: bool) -> P<Type> {
        assert!(self.strict_null_checks);
        let missing_or_undefined =
            if is_property { self.undefined_or_missing_type } else { self.undefined_type };
        if t == missing_or_undefined
            || t.flags().intersects(TypeFlags::Union) && t.types()[0] == missing_or_undefined
        {
            return t;
        }
        self.get_union_type(&[t, missing_or_undefined])
    }

    // Add undefined or null or both to a type if they are missing.
    // checker.go:18994
    pub(crate) fn get_nullable_type(&mut self, t: P<Type>, flags: TypeFlags) -> P<Type> {
        let missing = (flags & !t.flags()) & (TypeFlags::Undefined | TypeFlags::Null);
        if missing.is_empty() {
            t
        } else if missing == TypeFlags::Undefined {
            self.get_union_type(&[t, self.undefined_type])
        } else if missing == TypeFlags::Null {
            self.get_union_type(&[t, self.null_type])
        } else {
            self.get_union_type(&[t, self.undefined_type, self.null_type])
        }
    }

    // checker.go:19007
    pub fn get_non_nullable_type(&mut self, t: P<Type>) -> P<Type> {
        if self.strict_null_checks {
            return self.get_adjusted_type_with_facts(t, TypeFacts::NEUndefinedOrNull);
        }
        t
    }

    // checker.go:19014
    pub fn is_nullable_type(&mut self, t: P<Type>) -> bool {
        self.has_type_facts(t, TypeFacts::IsUndefinedOrNull)
    }

    // checker.go:19018
    pub(crate) fn get_non_nullable_type_if_needed(&mut self, t: P<Type>) -> P<Type> {
        if self.is_nullable_type(t) {
            return self.get_non_nullable_type(t);
        }
        t
    }

    // checker.go:19025
    pub(crate) fn get_declaration_node_flags_from_symbol(&mut self, s: P<Symbol>) -> NodeFlags {
        if let Some(value_declaration) = s.value_declaration() {
            return self.get_combined_node_flags_cached(value_declaration);
        }
        NodeFlags::None
    }

    // checker.go:19032
    pub(crate) fn get_combined_node_flags_cached(&mut self, node: P<Node>) -> NodeFlags {
        // we hold onto the last node and result to speed up repeated lookups against the same node.
        if self.last_get_combined_node_flags_node == Some(node) {
            return self.last_get_combined_node_flags_result;
        }
        self.last_get_combined_node_flags_node = Some(node);
        self.last_get_combined_node_flags_result = get_combined_node_flags(node);
        self.last_get_combined_node_flags_result
    }

    // checker.go:19042
    pub(crate) fn is_var_const_like(&mut self, node: P<Node>) -> bool {
        let block_scope_kind = self.get_combined_node_flags_cached(node) & NodeFlags::BlockScoped;
        block_scope_kind == NodeFlags::Const
            || block_scope_kind == NodeFlags::Using
            || block_scope_kind == NodeFlags::AwaitUsing
    }

    // checker.go:19047
    pub(crate) fn get_effective_property_name_for_property_name_node(
        &mut self,
        node: P<Node>,
    ) -> (String, bool) {
        let name = get_property_name_for_property_name_node(node);
        if name != INTERNAL_SYMBOL_NAME_MISSING {
            return (name.into_owned(), true);
        }
        if is_computed_property_name(node) {
            // This is cached so `getTypeOfExpression` isn't constantly reinvoked for every property name lookup
            let links = self.computed_name_links.get(node);
            if let Some(has_name) = links.has_name.get() {
                return (links.name.get().to_string(), has_name);
            }
            let expr_type = self.get_type_of_expression(node.expression().unwrap());
            let (name, exists) = Self::try_get_name_from_type(expr_type);
            links.name.set(alloc_str(&name));
            links.has_name.set(Some(exists));
            return (name, exists);
        }
        (String::new(), false)
    }

    // checker.go:19066
    fn try_get_name_from_type(t: P<Type>) -> (String, bool) {
        if t.flags().intersects(TypeFlags::UniqueESSymbol) {
            (t.as_unique_es_symbol_type().name.get().to_string(), true)
        } else if t.flags().intersects(TypeFlags::StringLiteral) {
            let s = get_string_literal_value(t).to_string();
            (s, true)
        } else if t.flags().intersects(TypeFlags::NumberLiteral) {
            let s = get_number_literal_value(t).to_string();
            (s, true)
        } else {
            (String::new(), false)
        }
    }

    // checker.go:19081
    pub(crate) fn get_combined_modifier_flags_cached(&mut self, node: P<Node>) -> ModifierFlags {
        // we hold onto the last node and result to speed up repeated lookups against the same node.
        if self.last_get_combined_modifier_flags_node == Some(node) {
            return self.last_get_combined_modifier_flags_result;
        }
        self.last_get_combined_modifier_flags_node = Some(node);
        self.last_get_combined_modifier_flags_result = get_combined_modifier_flags(node);
        self.last_get_combined_modifier_flags_result
    }

    // Push an entry on the type resolution stack. If an entry with the given target and the given property name
    // is already on the stack, and no entries in between already have a type, then a circularity has occurred.
    // In this case, the result values of the existing entry and all entries pushed after it are changed to false,
    // and the value false is returned. Otherwise, the new entry is just pushed onto the stack, and true is returned.
    // In order to see if the same query has already been done before, the target object and the propertyName both
    // must match the one passed in.
    //
    // @param target The symbol, type, or signature whose type is being queried
    // @param propertyName The property name that should be used to query the target for its type
    // checker.go:19102
    pub(crate) fn push_type_resolution(
        &mut self,
        target: TypeSystemEntity,
        property_name: TypeSystemPropertyName,
    ) -> bool {
        if let Some(resolution_cycle_start_index) =
            self.find_resolution_cycle_start_index(target, property_name)
        {
            // A cycle was found
            for element in self.type_resolutions.iter_mut().skip(resolution_cycle_start_index) {
                element.result = false;
            }
            return false;
        }
        let serial = self.flow_memo.source_now();
        self.type_resolutions.push(TypeResolution { target, property_name, result: true, serial });
        true
    }

    // Pop an entry from the type resolution stack and return its associated result value. The result value will
    // be true if no circularities were detected, or false if a circularity was found.
    // checker.go:19119
    pub(crate) fn pop_type_resolution(&mut self) -> bool {
        self.type_resolutions.pop().unwrap().result
    }

    // checker.go:19127
    pub(crate) fn find_resolution_cycle_start_index(
        &mut self,
        target: TypeSystemEntity,
        property_name: TypeSystemPropertyName,
    ) -> Option<usize> {
        for i in (self.resolution_start..self.type_resolutions.len()).rev() {
            let resolution = self.type_resolutions[i];
            if self.type_resolution_has_property(&resolution) {
                return None;
            }
            if resolution.target == target && resolution.property_name == property_name {
                // A circular or in-progress resolution: what the caller does next is not final until this
                // resolution is done (flow memo).
                self.flow_memo.taint(resolution.serial);
                return Some(i);
            }
        }
        None
    }

    // `r: P<TypeResolution>` -> `r: &TypeResolution` (TypeResolution is a plain value struct stored in
    // `type_resolutions`, not an arena object).
    // checker.go:19140
    fn type_resolution_has_property(&mut self, r: &TypeResolution) -> bool {
        match (r.property_name, r.target) {
            (TypeSystemPropertyName::Type, TypeSystemEntity::Symbol(s)) => {
                self.value_symbol_links.get(s).resolved_type.get().is_some()
            }
            (TypeSystemPropertyName::DeclaredType, TypeSystemEntity::Symbol(s)) => {
                self.type_alias_links.get(s).declared_type.get().is_some()
            }
            (TypeSystemPropertyName::ResolvedTypeArguments, TypeSystemEntity::Type(t)) => {
                t.as_type_reference().resolved_type_arguments.get().is_some()
            }
            (TypeSystemPropertyName::ResolvedBaseTypes, TypeSystemEntity::Type(t)) => {
                t.as_interface_type().base_types_resolved.get()
            }
            (TypeSystemPropertyName::ResolvedBaseConstructorType, TypeSystemEntity::Type(t)) => {
                t.as_interface_type().resolved_base_constructor_type.get().is_some()
            }
            (TypeSystemPropertyName::ResolvedReturnType, TypeSystemEntity::Signature(s)) => {
                s.resolved_return_type.get().is_some()
            }
            (TypeSystemPropertyName::ResolvedBaseConstraint, TypeSystemEntity::Type(t)) => {
                self.resolved_base_constraint_of(t).is_some()
            }
            (TypeSystemPropertyName::InitializerIsUndefined, TypeSystemEntity::Node(n)) => self
                .node_links
                .get(n)
                .flags
                .get()
                .intersects(NodeCheckFlags::InitializerIsUndefinedComputed),
            (TypeSystemPropertyName::WriteType, TypeSystemEntity::Symbol(s)) => {
                self.value_symbol_links.get(s).write_type().is_some()
            }
            (TypeSystemPropertyName::AliasTarget, TypeSystemEntity::Symbol(s)) => {
                self.alias_symbol_links.get(s).alias_target.get().is_some()
            }
            // Go: a failed type assertion on r.target
            _ => panic!(
                "interface conversion: unexpected TypeSystemEntity in typeResolutionHasProperty"
            ),
        }
    }

    // checker.go:19166
    pub(crate) fn report_circularity_error(&mut self, symbol: P<Symbol>) -> P<Type> {
        let declaration = symbol.value_declaration();
        // Check if variable has type annotation that circularly references the variable itself
        if let Some(declaration) = declaration {
            if declaration.type_node().is_some() {
                let s = self.symbol_to_string(symbol);
                self.error(symbol.value_declaration(), &diagnostics::X_0_IS_REFERENCED_DIRECTLY_OR_INDIRECTLY_IN_ITS_OWN_TYPE_ANNOTATION, &[&s]);
                return self.error_type;
            }
            // Check if variable has initializer that circularly references the variable itself
            if self.no_implicit_any
                && (!is_parameter_declaration(declaration) || declaration.initializer().is_some())
            {
                let s = self.symbol_to_string(symbol);
                self.error(
                    symbol.value_declaration(),
                    &diagnostics::X_0_IMPLICITLY_HAS_TYPE_ANY_BECAUSE_IT_DOES_NOT_HAVE_A_TYPE_ANNOTATION_AND_IS_REFERENCED_DIRECTLY_OR_INDIRECTLY_IN_ITS_OWN_INITIALIZER,
                    &[&s],
                );
            }
        } else if symbol.flags().intersects(SymbolFlags::Alias) {
            let node = Self::get_declaration_of_alias_symbol(symbol);
            if node.is_some() {
                let s = self.symbol_to_string(symbol);
                self.error(node, &diagnostics::CIRCULAR_DEFINITION_OF_IMPORT_ALIAS_0, &[&s]);
            }
        }
        // Circularities could also result from parameters in function expressions that end up
        // having themselves as contextual types following type argument inference. In those cases
        // we have already reported an implicit any error so we don't report anything here.
        self.any_type
    }

    // checker.go:19190
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub fn get_properties_of_type(&mut self, t: P<Type>) -> &'static [P<Symbol>] {
        let t = self.get_reduced_apparent_type(t);
        if t.flags().intersects(TypeFlags::UnionOrIntersection) {
            return self.get_properties_of_union_or_intersection_type(t);
        }
        self.get_properties_of_object_type(t)
    }

    // checker.go:19198
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn get_properties_of_object_type(&mut self, t: P<Type>) -> &'static [P<Symbol>] {
        if t.flags().intersects(TypeFlags::Object) {
            return self.resolve_structured_type_members(t).properties();
        }
        &[]
    }

    // checker.go:19205
    pub(crate) fn get_properties_of_union_or_intersection_type(
        &mut self,
        t: P<Type>,
    ) -> &'static [P<Symbol>] {
        let d = t.as_union_or_intersection_type();
        if d.resolved_properties().is_none() {
            let mut checked: FxHashSet<&'static str> = FxHashSet::default();
            let mut props: Vec<P<Symbol>> = Vec::new();
            for &current in d.types.get() {
                props.extend(self.get_properties_of_type(current).iter().filter_map(|&prop| {
                    if checked.insert(prop.name()) {

                        self.get_property_of_union_or_intersection_type(
                            t,
                            prop.name(),
                            t.flags().intersects(TypeFlags::Intersection), /*skipObjectFunctionPropertyAugment*/
                        )
                    } else {
                        None
                    }
                }));
                // The properties of a union type are those that are present in all constituent types, so
                // we only need to check the properties of the first type without index signature
                if t.flags().intersects(TypeFlags::Union)
                    && self.get_index_infos_of_type(current).is_empty()
                {
                    break;
                }
            }
            d.set_resolved_properties(Some(alloc_vec(props)));
        }
        d.resolved_properties().unwrap()
    }

    // checker.go:19231
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn get_property_of_type(&mut self, t: P<Type>, name: &str) -> Option<P<Symbol>> {
        self.get_property_of_type_ex(
            t, name, false, /*skipObjectFunctionPropertyAugment*/
            false, /*includeTypeOnlyMembers*/
        )
    }

    // Return the symbol for the property with the given name in the given type. Creates synthetic union properties when
    // necessary, maps primitive types and type parameters are to their apparent types, and augments with properties from
    // Object and Function as appropriate.
    //
    // @param type a type to look up property from
    // @param name a name of property to look up in a given type
    // checker.go:19243
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn get_property_of_type_ex(
        &mut self,
        t: P<Type>,
        name: &str,
        skip_object_function_property_augment: bool,
        include_type_only_members: bool,
    ) -> Option<P<Symbol>> {
        self.get_property_of_type_worker(
            t,
            name,
            skip_object_function_property_augment,
            include_type_only_members,
            true, /*instantiate*/
        )
    }

    // notes/mem-lazy.md L9: whether getPropertyOfType(t, name) returns a property. Members of lazy member tables
    // are not instantiated for this: the declared member stands in for its instantiation (same flags).
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn has_property_of_type(&mut self, t: P<Type>, name: &str) -> bool {
        self.lazy_member_stats.has_prop_queries += 1;
        self.get_property_of_type_worker(
            t, name, false, /*skipObjectFunctionPropertyAugment*/
            false, /*includeTypeOnlyMembers*/
            false, /*instantiate*/
        )
        .is_some()
    }

    #[cfg_attr(feature = "site-counts", track_caller)]
    fn get_property_of_type_worker(
        &mut self,
        t: P<Type>,
        name: &str,
        skip_object_function_property_augment: bool,
        include_type_only_members: bool,
        instantiate: bool,
    ) -> Option<P<Symbol>> {
        let t = self.get_reduced_apparent_type(t);
        if t.flags().intersects(TypeFlags::Object) {
            let key = HashedName::new(name);
            let symbol = if t.object_flags_lazy().intersects(ObjectFlags::MembersResolved) {
                t.as_structured_type().members().and_then(|m| m.lookup_hashed(key))
            } else {
                self.get_member_of_unresolved_structured_type(t, name, instantiate)
            };
            if let Some(symbol) = symbol {
                if !include_type_only_members
                    && t.symbol().is_some_and(|s| s.flags().intersects(SymbolFlags::ValueModule))
                    && self
                        .module_symbol_links
                        .get(t.symbol().unwrap())
                        .type_only_export_star_map
                        .get(name)
                        .is_some()
                {
                    // If this is the type of a module, `resolved.members.get(name)` might have effectively skipped over
                    // an `export type * from './foo'`, leaving `symbolIsValue` unable to see that the symbol is being
                    // viewed through a type-only export.
                    return None;
                }
                if self.symbol_is_value_ex(symbol, include_type_only_members) {
                    return Some(symbol);
                }
            }
            if skip_object_function_property_augment {
                return None;
            }
            // A name that no member of `Function`, `CallableFunction`, `NewableFunction` or `Object` has is not found
            // below; with `t` resolved, the signature tests below have no side effects to keep.
            if t.object_flags_lazy().intersects(ObjectFlags::MembersResolved)
                && !self.may_be_augment_member(key)
            {
                return None;
            }
            let function_type = if t == self.any_function_type {
                Some(self.global_function_type)
            } else if !self.signatures_of_structured_type(t, SignatureKind::Call).is_empty() {
                Some(self.global_callable_function_type)
            } else if !self.signatures_of_structured_type(t, SignatureKind::Construct).is_empty() {
                Some(self.global_newable_function_type)
            } else {
                None
            };
            if let Some(function_type) = function_type {
                let symbol = self.get_property_of_object_type_hashed(function_type, key);
                if symbol.is_some() {
                    return symbol;
                }
            }
            return self.get_property_of_object_type_hashed(self.global_object_type, key);
        } else if t.flags().intersects(TypeFlags::Intersection) {
            let prop = self.get_property_of_union_or_intersection_type(
                t, name, true, /*skipObjectFunctionPropertyAugment*/
            );
            if prop.is_some() {
                return prop;
            }
            if !skip_object_function_property_augment {
                return self.get_property_of_union_or_intersection_type(
                    t,
                    name,
                    skip_object_function_property_augment,
                );
            }
            return None;
        } else if t.flags().intersects(TypeFlags::Union) {
            return self.get_property_of_union_or_intersection_type(
                t,
                name,
                skip_object_function_property_augment,
            );
        }
        None
    }

    /// `t`'s resolved member table, for a loop that looks up many names in `t` with `get_resolved_property` (the
    /// relater's walks over a target's properties): `t` must be a plain object type (its own reduced apparent type)
    /// with resolved members and no module symbol, so that `get_property_of_type` answers from the table alone.
    pub(crate) fn resolved_property_source(&self, t: P<Type>) -> Option<ResolvedPropertySource> {
        if t.flags() != TypeFlags::Object
            || t.object_flags().intersects(ObjectFlags::Mapped)
            || !t.object_flags_lazy().intersects(ObjectFlags::MembersResolved)
            || t.symbol().is_some_and(|s| s.flags().intersects(SymbolFlags::ValueModule))
        {
            return None;
        }
        Some(ResolvedPropertySource {
            t,
            members: t.as_structured_type().members(),
            epoch: self.members_reset_epoch,
        })
    }

    /// `get_property_of_type(source.t, name)`: a value member or a name no augmenting global type has is answered
    /// from the table; anything else (and any lookup after a type's members were reset) takes the full path.
    #[inline]
    pub(crate) fn get_resolved_property(
        &mut self,
        source: &ResolvedPropertySource,
        name: &str,
    ) -> Option<P<Symbol>> {
        if source.epoch == self.members_reset_epoch {
            let key = HashedName::new(name);
            match source.members.and_then(|m| m.lookup_hashed(key)) {
                Some(symbol) if symbol.flags().intersects(SymbolFlags::Value) => {
                    return Some(symbol);
                }
                None if !self.may_be_augment_member(key) => return None,
                Some(_) | None => {}
            }
        }
        self.get_property_of_type(source.t, name)
    }

    /// `get_property_of_type_ex(t, key.name, skip_object_function_property_augment, false)` when `t` is a plain
    /// object type with resolved members and the member table alone answers it (as in `get_resolved_property`);
    /// `None` when the full path must decide. For a loop that looks up one name in many types.
    #[inline]
    #[expect(
        clippy::option_option,
        reason = "The outer option records whether a result is available; the inner option records an absent value."
    )]
    pub(crate) fn try_resolved_property(
        &mut self,
        t: P<Type>,
        key: HashedName<'_>,
        skip_object_function_property_augment: bool,
    ) -> Option<Option<P<Symbol>>> {
        if t.flags() != TypeFlags::Object
            || t.object_flags().intersects(ObjectFlags::Mapped)
            || !t.object_flags_lazy().intersects(ObjectFlags::MembersResolved)
            || t.symbol().is_some_and(|s| s.flags().intersects(SymbolFlags::ValueModule))
        {
            return None;
        }
        match t.as_structured_type().members().and_then(|m| m.lookup_hashed(key)) {
            Some(symbol) if symbol.flags().intersects(SymbolFlags::Value) => Some(Some(symbol)),
            None if skip_object_function_property_augment || !self.may_be_augment_member(key) => {
                Some(None)
            }
            Some(_) | None => None,
        }
    }

    /// `program.get_emit_module_format_of_file(file)`, remembered for the last file: the checker asks it for the
    /// file being checked, per import, export and declaration, and the program answers by hashing the file's path.
    #[inline]
    pub(crate) fn emit_module_format_of_file(&mut self, file: P<SourceFile>) -> ModuleKind {
        if let Some((last, kind)) = self.last_emit_module_format
            && last == file
        {
            return kind;
        }
        let kind = self.program.get_emit_module_format_of_file(file);
        self.last_emit_module_format = Some((file, kind));
        kind
    }

    /// False if `key` is a member of none of the global types `get_property_of_type_worker` looks up a missing
    /// property in (most lookups that miss a type's own members). True until all four are resolved: the filter is
    /// built from their member tables, which do not change after resolution, and asking must not resolve them early.
    /// The base-type cycle resets that make a type resolve its members again drop the filter.
    #[inline]
    pub(crate) fn may_be_augment_member(&mut self, key: HashedName<'_>) -> bool {
        match &self.augment_filter {
            Some(filter) => filter.may_contain(key),
            None => self.build_augment_filter(key),
        }
    }

    #[cold]
    #[inline(never)]
    fn build_augment_filter(&mut self, key: HashedName<'_>) -> bool {
        let types = [
            self.global_function_type,
            self.global_callable_function_type,
            self.global_newable_function_type,
            self.global_object_type,
        ];
        // Until all four are resolved every miss comes back here: test that before reading any of their tables.
        if types.iter().any(|t| {
            t.flags().intersects(TypeFlags::Object)
                && !t.object_flags_lazy().intersects(ObjectFlags::MembersResolved)
        }) {
            return true;
        }
        let mut filter = NameFilter::default();
        for t in types {
            if !t.flags().intersects(TypeFlags::Object) {
                continue; // get_property_of_object_type finds nothing in it
            }
            if let Some(members) = self.resolve_structured_type_members(t).members() {
                filter.add_keys(&members);
            }
        }
        self.augment_filter = Some(filter);
        filter.may_contain(key)
    }

    // Return the type of the given property in the given type, or nil if no such property exists
    // checker.go:19295
    pub(crate) fn get_type_of_property_of_type(
        &mut self,
        t: P<Type>,
        name: &str,
    ) -> Option<P<Type>> {
        let prop = self.get_property_of_type(t, name);
        if let Some(prop) = prop {
            return Some(self.get_type_of_symbol(prop));
        }
        None
    }

    // checker.go:19303
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub fn get_signatures_of_type(
        &mut self,
        t: P<Type>,
        kind: SignatureKind,
    ) -> &'static [P<Signature>] {
        let t = self.get_reduced_apparent_type(t);
        self.get_signatures_of_structured_type(t, kind)
    }

    // checker.go:19307
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn get_signatures_of_structured_type(
        &mut self,
        t: P<Type>,
        kind: SignatureKind,
    ) -> &'static [P<Signature>] {
        self.signatures_of_structured_type(t, kind)
    }

    // Go's getSignaturesOfStructuredType returns the stored slice; this is it without the copy.
    /// A type with resolved members has no lazy member table, so the resolved signatures are the answer (a mapped
    /// type's early return aside); that path makes no call and saves no registers.
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn signatures_of_structured_type(
        &mut self,
        t: P<Type>,
        kind: SignatureKind,
    ) -> &'static [P<Signature>] {
        if t.flags().intersects(TypeFlags::StructuredType)
            && t.object_flags_lazy().intersects(ObjectFlags::MembersResolved)
            && !(self.lazy_members && t.object_flags().intersects(ObjectFlags::Mapped))
        {
            let resolved = t.as_structured_type();
            return if kind == SignatureKind::Call {
                resolved.call_signatures()
            } else {
                resolved.construct_signatures()
            };
        }
        self.signatures_of_unresolved_structured_type(t, kind)
    }

    #[cfg_attr(feature = "site-counts", track_caller)]
    #[inline(never)]
    fn signatures_of_unresolved_structured_type(
        &mut self,
        t: P<Type>,
        kind: SignatureKind,
    ) -> &'static [P<Signature>] {
        if !t.flags().intersects(TypeFlags::StructuredType) {
            return &[];
        }
        if let Some(lm) = self.get_ready_lazy_member_table(t) {
            self.lazy_member_stats.member_signature_queries += 1;
            let ready = lm.ready.get().unwrap();
            if kind == SignatureKind::Call {
                return ready.call_signatures.get();
            }
            return ready.construct_signatures.get();
        }
        if self.lazy_members
            && self.mapped_types_resolving == 0
            && t.object_flags().intersects(ObjectFlags::Mapped)
        {
            if !t.object_flags_lazy().intersects(ObjectFlags::MembersResolved) {
                self.lazy_member_stats.mapped_signature_early_returns += 1;
            }
            // Mapped types have no signatures.
            return &[];
        }
        let resolved = self.resolve_structured_type_members(t);
        if kind == SignatureKind::Call {
            return resolved.call_signatures();
        }
        resolved.construct_signatures()
    }

    // checker.go:19318
    pub(crate) fn get_index_infos_of_type(&mut self, t: P<Type>) -> &'static [P<IndexInfo>] {
        let t = self.get_reduced_apparent_type(t);
        self.get_index_infos_of_structured_type(t)
    }

    // checker.go:19322
    pub(crate) fn get_index_infos_of_structured_type(
        &mut self,
        t: P<Type>,
    ) -> &'static [P<IndexInfo>] {
        self.index_infos_of_structured_type(t)
    }

    // Go's getIndexInfosOfStructuredType returns the stored slice; this is it without the copy.
    /// As `signatures_of_structured_type`: resolved members answer without a call.
    pub(crate) fn index_infos_of_structured_type(&mut self, t: P<Type>) -> &'static [P<IndexInfo>] {
        if t.flags().intersects(TypeFlags::StructuredType)
            && t.object_flags_lazy().intersects(ObjectFlags::MembersResolved)
        {
            return t.as_structured_type().index_infos();
        }
        self.index_infos_of_unresolved_structured_type(t)
    }

    #[inline(never)]
    fn index_infos_of_unresolved_structured_type(&mut self, t: P<Type>) -> &'static [P<IndexInfo>] {
        if t.flags().intersects(TypeFlags::StructuredType) {
            if let Some(lm) = self.get_ready_lazy_member_table(t) {
                self.lazy_member_stats.member_index_info_queries += 1;
                return lm.ready.get().unwrap().index_infos.get();
            }
            if let Some(lazy) = self.get_lazy_mapped_table(t) {
                return self.get_lazy_mapped_type_index_infos(t, &lazy);
            }
            return self.resolve_structured_type_members(t).index_infos();
        }
        &[]
    }

    // Return the indexing info of the given kind in the given type. Creates synthetic union index types when necessary and
    // maps primitive types and type parameters are to their apparent types.
    // checker.go:19331
    pub(crate) fn get_index_info_of_type(
        &mut self,
        t: P<Type>,
        key_type: P<Type>,
    ) -> Option<P<IndexInfo>> {
        let index_infos = self.get_index_infos_of_type(t);
        find_index_info(index_infos, key_type)
    }

    // Return the index type of the given kind in the given type. Creates synthetic union index types when necessary and
    // maps primitive types and type parameters are to their apparent types.
    // checker.go:19337
    pub(crate) fn get_index_type_of_type(
        &mut self,
        t: P<Type>,
        key_type: P<Type>,
    ) -> Option<P<Type>> {
        let info = self.get_index_info_of_type(t, key_type);
        if let Some(info) = info {
            return info.value_type.get();
        }
        None
    }

    // checker.go:19345
    pub(crate) fn get_index_type_of_type_ex(
        &mut self,
        t: P<Type>,
        key_type: P<Type>,
        default_type: P<Type>,
    ) -> P<Type> {
        if let Some(result) = self.get_index_type_of_type(t, key_type) {
            return result;
        }
        default_type
    }

    // checker.go:19352
    pub(crate) fn get_applicable_index_info(
        &mut self,
        t: P<Type>,
        key_type: P<Type>,
    ) -> Option<P<IndexInfo>> {
        let index_infos = self.get_index_infos_of_type(t);
        self.find_applicable_index_info(index_infos, key_type)
    }

    // checker.go:19356
    pub(crate) fn get_applicable_index_info_for_name(
        &mut self,
        t: P<Type>,
        name: &str,
    ) -> Option<P<IndexInfo>> {
        if is_late_bound_name(name) {
            return self.get_applicable_index_info(t, self.es_symbol_type);
        }
        let index_infos = self.get_index_infos_of_type(t);
        if index_infos.is_empty() {
            return None; // most types: the name's literal type is not needed
        }
        let key_type = self.get_string_literal_type(name);
        self.find_applicable_index_info(index_infos, key_type)
    }

    // checker.go:19363
    pub(crate) fn find_applicable_index_info(
        &mut self,
        index_infos: &[P<IndexInfo>],
        key_type: P<Type>,
    ) -> Option<P<IndexInfo>> {
        // Index signatures for type 'string' are considered only when no other index signatures apply.
        let mut string_index_info: Option<P<IndexInfo>> = None;
        let mut applicable_infos: SmallVec<[P<IndexInfo>; 4]> = SmallVec::new();
        for &info in index_infos {
            if info.key_type.get() == Some(self.string_type) {
                string_index_info = Some(info);
            } else if self.is_applicable_index_type(key_type, info.key_type()) {
                applicable_infos.push(info);
            }
        }
        // When more than one index signature is applicable we create a synthetic IndexInfo. Instead of computing
        // the intersected key type, we just use unknownType for the key type as nothing actually depends on the
        // keyType property of the returned IndexInfo.
        match applicable_infos.len() {
            0 => {
                if let Some(string_index_info) = string_index_info
                    && self.is_applicable_index_type(key_type, self.string_type)
                {
                    return Some(string_index_info);
                }
                None
            }
            1 => Some(applicable_infos[0]),
            _ => {
                let mut is_readonly = true;
                let mut types: SmallVec<[P<Type>; 4]> =
                    SmallVec::with_capacity(applicable_infos.len());
                types.extend(applicable_infos.iter().map(|info| {
                    let value_type = info.value_type();
                    is_readonly &= info.is_readonly();
                    value_type
                }));
                let value_type = self.get_intersection_type(&types);
                Some(Self::new_index_info(self.unknown_type, value_type, is_readonly, None, &[]))
            }
        }
    }

    // checker.go:19398
    pub(crate) fn is_applicable_index_type(&mut self, source: P<Type>, target: P<Type>) -> bool {
        // A 'string' index signature applies to types assignable to 'string' or 'number', and a 'number' index
        // signature applies to types assignable to 'number', `${number}` and numeric string literal types.
        self.is_type_assignable_to(source, target)
            || target == self.string_type && self.is_type_assignable_to(source, self.number_type)
            || target == self.number_type
                && (source == self.numeric_string_type
                    || source.flags().intersects(TypeFlags::StringLiteral)
                        && is_numeric_literal_name(get_string_literal_value(source)))
    }

    // checker.go:19406
    #[cfg_attr(feature = "site-counts", track_caller)]
    #[inline]
    pub(crate) fn resolve_structured_type_members(
        &mut self,
        t: P<Type>,
    ) -> &'static StructuredType {
        #[cfg(feature = "site-counts")]
        if !t.object_flags_lazy().intersects(ObjectFlags::MembersResolved) {
            // Exclusive symbol/signature counts created by this resolution, attributed to the code that asked for it.
            thread_local! { static NESTED: std::cell::RefCell<Vec<u64>> = const { std::cell::RefCell::new(Vec::new()) }; }
            let label = if t.object_flags().intersects(ObjectFlags::Reference)
                && t.target().is_some_and(|s| s.object_flags().intersects(ObjectFlags::Tuple))
            {
                if self.lazy_member_tables.contains_key(&t) { "tuple:lazy" } else { "tuple" }
            } else if t.object_flags().intersects(ObjectFlags::Reference)
                && self.lazy_member_tables.contains_key(&t)
            {
                "reference:lazy"
            } else if t.object_flags().intersects(ObjectFlags::Mapped) {
                if Self::is_mapped_type_with_keyof_constraint_declaration(t) {
                    if self.lazy_mapped_tables.contains_key(&t) {
                        "mapped:keyof-lazy"
                    } else {
                        let mapped_type = t.as_mapped_type().target.get().unwrap_or(t);
                        let modifiers = self.get_modifiers_type_from_mapped_type(t);
                        let modifiers = self.get_apparent_type(modifiers);
                        if self.get_name_type_from_mapped_type(mapped_type).is_some() {
                            "mapped:keyof-as"
                        } else if modifiers.flags().intersects(TypeFlags::Intersection) {
                            "mapped:keyof-intersection"
                        } else if modifiers.flags().intersects(TypeFlags::Object) {
                            "mapped:keyof-object"
                        } else {
                            "mapped:keyof-other"
                        }
                    }
                } else {
                    "mapped:other"
                }
            } else {
                type_kind_label(t.flags(), t.object_flags())
            };
            let before = self.symbol_count as u64 + self.signature_count as u64;
            NESTED.with(|n| n.borrow_mut().push(0));
            let r = self.resolve_structured_type_members_worker(t);
            let inner = NESTED.with(|n| n.borrow_mut().pop().unwrap());
            let delta = self.symbol_count as u64 + self.signature_count as u64 - before;
            NESTED.with(|n| {
                if let Some(parent) = n.borrow_mut().last_mut() {
                    *parent += delta;
                }
            });
            crate::sitecount::hit("first-resolve", label);
            for _ in 0..(delta - inner) {
                crate::sitecount::hit("first-resolve-created", label);
            }
            return r;
        }
        if t.object_flags_lazy().intersects(ObjectFlags::MembersResolved) {
            return t.as_structured_type();
        }
        self.resolve_structured_type_members_worker(t)
    }

    #[cfg_attr(feature = "site-counts", track_caller)]
    #[inline(never)]
    fn resolve_structured_type_members_worker(&mut self, t: P<Type>) -> &'static StructuredType {
        if !t.object_flags_lazy().intersects(ObjectFlags::MembersResolved) {
            self.members_resolving += 1;
            if t.flags().intersects(TypeFlags::Object) {
                if t.object_flags().intersects(ObjectFlags::Reference) {
                    self.resolve_type_reference_members(t);
                } else if t.object_flags().intersects(ObjectFlags::ClassOrInterface) {
                    self.resolve_class_or_interface_members(t);
                } else if t.object_flags().intersects(ObjectFlags::ReverseMapped) {
                    self.resolve_reverse_mapped_type_members(t);
                } else if t.object_flags().intersects(ObjectFlags::Anonymous) {
                    self.resolve_anonymous_type_members(t);
                } else if t.object_flags().intersects(ObjectFlags::Mapped) {
                    self.resolve_mapped_type_members(t);
                } else {
                    panic!("Unhandled case in resolveStructuredTypeMembers");
                }
            } else if t.flags().intersects(TypeFlags::Union) {
                self.resolve_union_type_members(t);
            } else if t.flags().intersects(TypeFlags::Intersection) {
                self.resolve_intersection_type_members(t);
            } else {
                panic!("Unhandled case in resolveStructuredTypeMembers");
            }
            self.members_resolving -= 1;
        }
        t.as_structured_type()
    }

    // checker.go:19435
    fn resolve_class_or_interface_members(&mut self, t: P<Type>) {
        self.resolve_object_type_members(t, t, &[], &[]);
    }

    // checker.go:19439
    #[cfg_attr(feature = "site-counts", track_caller)]
    fn resolve_type_reference_members(&mut self, t: P<Type>) {
        if let Some(lm) =
            self.lazy_member_tables.get(&t).filter(|lm| lm.ready.get().is_some()).copied()
        {
            let lm = self.own_lazy_member_table(t, lm);
            self.resolve_lazy_members(t, lm);
            return;
        }
        let source = t.target().unwrap();
        let (type_parameters, padded_type_arguments) =
            self.get_reference_member_type_arguments(t, source);
        self.resolve_object_type_members(t, source, type_parameters, &padded_type_arguments);
    }

    // checker.go:19450
    fn resolve_object_type_members(
        &mut self,
        t: P<Type>,
        source: P<Type>,
        type_parameters: &[P<Type>],
        type_arguments: &[P<Type>],
    ) {
        let mut mapper: Option<P<TypeMapper>> = None;
        let mut members: Option<P<SymbolTable>>;
        let mut call_signatures: Vec<P<Signature>>;
        let mut construct_signatures: Vec<P<Signature>>;
        let mut index_infos: Vec<P<IndexInfo>>;
        let mut instantiated = false;
        let resolved = self.resolve_declared_members(source);
        if type_parameters == type_arguments {
            members = resolved.declared_members.get();
            call_signatures = resolved.declared_call_signatures.get().to_vec();
            construct_signatures = resolved.declared_construct_signatures.get().to_vec();
            index_infos = resolved.declared_index_infos.get().to_vec();
        } else {
            instantiated = true;
            let m = new_type_mapper(alloc_slice(type_parameters), alloc_slice(type_arguments));
            mapper = Some(m);
            members = self.instantiate_symbol_table(resolved.declared_members.get(), m);
            call_signatures =
                self.instantiate_signatures(resolved.declared_call_signatures.get(), m);
            construct_signatures =
                self.instantiate_signatures(resolved.declared_construct_signatures.get(), m);
            index_infos = self.instantiate_index_infos(resolved.declared_index_infos.get(), m);
        }
        let base_types = self.get_base_types(source);
        if !base_types.is_empty() {
            if !instantiated {
                members = members.map(|m| m.clone_table());
            }
            let this_argument = type_arguments.last().copied();
            for &base_type in base_types {
                let mut instantiated_base_type = base_type;
                if this_argument.is_some() {
                    let inst = self.instantiate_type(base_type, mapper);
                    instantiated_base_type = self.get_type_with_this_argument(
                        inst,
                        this_argument,
                        false, /*needsApparentType*/
                    );
                }
                let base_properties = self.get_properties_of_type(instantiated_base_type);
                members = Self::add_inherited_members(members, base_properties);
                self.append_inherited_signatures_and_index_infos(
                    &mut call_signatures,
                    &mut construct_signatures,
                    &mut index_infos,
                    instantiated_base_type,
                );
            }
        }
        self.set_structured_type_members(
            t,
            members,
            &call_signatures,
            &construct_signatures,
            &index_infos,
        );
    }
}

// checker.go:19499
pub(crate) fn find_index_info(
    index_infos: &[P<IndexInfo>],
    key_type: P<Type>,
) -> Option<P<IndexInfo>> {
    if let Some(result) = index_infos.iter().find_map(|&info| {
        if info.key_type.get() == Some(key_type) { Some(Some(info)) } else { None }
    }) {
        return result;
    }
    None
}

// #64475 (behind `Checker::lazy_members`)
impl Checker {
    fn append_inherited_signatures_and_index_infos(
        &mut self,
        call_signatures: &mut Vec<P<Signature>>,
        construct_signatures: &mut Vec<P<Signature>>,
        index_infos: &mut Vec<P<IndexInfo>>,
        base_type: P<Type>,
    ) {
        let sigs = self.get_signatures_of_type(base_type, SignatureKind::Call);
        call_signatures.extend(sigs);
        let sigs = self.get_signatures_of_type(base_type, SignatureKind::Construct);
        construct_signatures.extend(sigs);
        let inherited_index_infos: Vec<P<IndexInfo>> = if base_type == self.any_type {
            vec![self.any_base_type_index_info]
        } else {
            self.get_index_infos_of_type(base_type).to_vec()
        };
        let filtered: Vec<P<IndexInfo>> = inherited_index_infos
            .into_iter()
            .filter(|info| find_index_info(index_infos, info.key_type()).is_none())
            .collect();
        index_infos.extend(filtered);
    }
}

// Instantiated class and interface references get a lazy member table in place of
// resolved members. It has the signatures and index infos, but only instantiates the
// members that are looked up, and reuses them if the members are later resolved in full.

/// One per instantiated reference whose members are answered lazily. In the arena (notes/mem-checker-heap.md):
/// a table can be dropped from `lazy_member_tables` (resolved in full) while callers still hold it, which an `Rc`
/// used to cover; the arena keeps it alive instead, and the one-word slices keep it at 80 bytes (an `Rc` box was
/// 168, rounded up to 192 by mimalloc).
#[expect(
    clippy::too_long_first_doc_paragraph,
    reason = "Public for the integration tests; the comment keeps its upstream wording."
)]
pub struct LazyMemberTable {
    mapper: P<TypeMapper>,
    // Go `ready` plus the fields prepareLazyMembers fills in before it sets `ready`.
    pub(crate) ready: std::cell::OnceCell<LazyMembers>,
    pub(crate) declared: SymbolTable, // keyed by the declared members' names
    // notes/mem-lazy.md L10: getPropertiesOfType order with declared members standing in (getLazyPropertiesInOrder).
    pub(crate) ordered_properties: std::cell::OnceCell<ThinSlice<P<Symbol>>>,
}

// 80 bytes in release builds (the symbol table is 24; debug builds add a borrow flag to it).
#[cfg(target_pointer_width = "64")]
const _: () =
    assert!(std::mem::size_of::<LazyMemberTable>() == std::mem::size_of::<SymbolTable>() + 56);
#[cfg(target_pointer_width = "32")]
const _: () =
    assert!(std::mem::size_of::<LazyMemberTable>() == std::mem::size_of::<SymbolTable>() + 52);

#[derive(Clone)]
pub struct LazyMembers {
    unaffected: ThinSlice<&'static str>, // sorted names of declared members that instantiate to themselves
    pub(crate) call_signatures: ThinSlice<P<Signature>>,
    pub(crate) construct_signatures: ThinSlice<P<Signature>>,
    pub(crate) index_infos: ThinSlice<P<IndexInfo>>,
    pub(crate) base_types: ThinSlice<P<Type>>,
}

/// What `lazy_unaffected_members` keeps per generic target: the declared-member table it describes, the sorted
/// names answered true for good, and the named members whose answers may still change.
pub struct LazyUnaffected {
    table: P<SymbolTable>,
    len: usize,
    maps_this_only: bool,
    named: u64,
    settled: &'static [&'static str],
    pending: Vec<(&'static str, P<Symbol>)>,
}

fn lazy_unaffected_shadow() -> bool {
    static SHADOW: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SHADOW.get_or_init(|| std::env::var("TSRS_LAZY_UNAFFECTED").as_deref() == Ok("shadow"))
}

pub(crate) fn may_have_lazy_members(t: P<Type>) -> bool {
    t.object_flags_lazy() & (ObjectFlags::MembersResolved | ObjectFlags::Reference)
        == ObjectFlags::Reference
}

impl Checker {
    fn get_reference_member_type_arguments(
        &mut self,
        t: P<Type>,
        source: P<Type>,
    ) -> (&'static [P<Type>], Vec<P<Type>>) {
        let type_parameters = source.as_interface_type().all_type_parameters.get();
        let mut type_arguments = self.get_type_arguments(t).to_vec();
        if type_arguments.len() + 1 == type_parameters.len() {
            type_arguments.push(t);
        }
        (type_parameters, type_arguments)
    }

    /// Whether Go's checker would report `ObjectFlagsMembersResolved` for `t` at this point: its members were
    /// resolved, or a member query was answered through a lazy member table (the port's replacement for
    /// `resolveStructuredTypeMembers` on instantiated references, which leaves the flag unset). Read-only;
    /// never resolves anything. Used by the Node API to report Go's objectFlags.
    pub fn members_resolved_like_go(&self, t: P<Type>) -> bool {
        t.object_flags_lazy().intersects(ObjectFlags::MembersResolved)
            || self.lazy_member_tables.contains_key(&t)
    }

    // Returns nil if t has no lazy member table or it is still being prepared.
    pub(crate) fn get_ready_lazy_member_table(&mut self, t: P<Type>) -> Option<P<LazyMemberTable>> {
        if !self.lazy_members || !may_have_lazy_members(t) {
            return None;
        }
        self.get_ready_lazy_member_table_worker(t)
    }

    fn get_ready_lazy_member_table_worker(&mut self, t: P<Type>) -> Option<P<LazyMemberTable>> {
        if !t.flags().intersects(TypeFlags::Object) {
            return None;
        }
        let source = t.object_target();
        if source.is_none()
            || source == Some(t)
            || !(source.unwrap().object_flags().intersects(ObjectFlags::ClassOrInterface)
                || self.lazy_tuples
                    && source.unwrap().object_flags().intersects(ObjectFlags::Tuple))
            || !self.lazy_tuples && source.unwrap().object_flags().intersects(ObjectFlags::Tuple)
            || t.symbol().is_some_and(|s| s.flags().intersects(SymbolFlags::ValueModule))
        {
            return None;
        }
        let lm = match self.lazy_member_tables.get(&t) {
            Some(&lm) => self.own_lazy_member_table(t, lm),
            None => self.create_lazy_member_table(t, source.unwrap())?,
        };
        if lm.ready.get().is_none()
            || t.object_flags_lazy().intersects(ObjectFlags::MembersResolved)
        {
            return None;
        }
        Some(lm)
    }

    /// The table-creating half of `get_ready_lazy_member_table_worker` (out of line, so the
    /// lookup half has a small frame). `None` when the reference's arguments are its target's own parameters.
    #[inline(never)]
    fn create_lazy_member_table(
        &mut self,
        t: P<Type>,
        source: P<Type>,
    ) -> Option<P<LazyMemberTable>> {
        // get_reference_member_type_arguments without a temporary list.
        let type_parameters = source.as_interface_type().all_type_parameters.get();
        let arguments = self.get_type_arguments(t);
        let type_arguments = if arguments.len() + 1 == type_parameters.len() {
            if type_parameters[..arguments.len()] == *arguments
                && type_parameters[arguments.len()] == t
            {
                return None;
            }
            let mut padded = self.free_type_lists.pop().unwrap_or_default();
            padded.extend_from_slice(arguments);
            padded.push(t);
            let type_arguments = alloc_slice(&padded);
            self.free_type_list(padded);
            type_arguments
        } else {
            if type_parameters == arguments {
                return None;
            }
            alloc_slice(arguments)
        };
        let lm = P::new(LazyMemberTable {
            mapper: {
                let m = new_type_mapper(type_parameters, type_arguments);
                escape_mapper(m); // kept by the table
                m
            },
            ready: std::cell::OnceCell::new(),
            declared: SymbolTable::default(),
            ordered_properties: std::cell::OnceCell::new(),
        });
        self.lazy_member_tables.insert(t, lm);
        self.lazy_member_stats.member_tables_created += 1;
        if t.object_target().unwrap().object_flags().intersects(ObjectFlags::Tuple) {
            self.lazy_member_stats.tuple_tables_created += 1;
        }
        self.prepare_lazy_members(t, lm, type_arguments.last().copied());
        Some(lm)
    }

    /// The sorted names of the named members in `declared_members` (the declared members of `source`) that
    /// `instantiateSymbol` with `mapper` returns unchanged now (`isSymbolUnaffectedByInstantiation`).
    ///
    /// Reuse: an answer settles once the member's type (and a setter's write type) is resolved. The links set those
    /// once, the type-variable test of a type never changes, and `maps_this_only` of a lazy table's mapper depends
    /// only on the target's type parameters (it is checked anyway). So each target keeps the names answered true
    /// for good and the members whose types were not resolved yet, and asks again only about those; members
    /// answered false for good are dropped. `TSRS_LAZY_UNAFFECTED=shadow` recomputes every list and panics on a
    /// difference.
    fn lazy_unaffected_members(
        &mut self,
        source: P<Type>,
        declared_members: P<SymbolTable>,
        mapper: P<TypeMapper>,
    ) -> &'static [&'static str] {
        let maps_this_only = mapper.maps_this_only();
        let memo = self.lazy_unaffected.get_mut(&source).filter(|memo| {
            memo.table == declared_members
                && memo.len == declared_members.len()
                && memo.maps_this_only == maps_this_only
        });
        let result = if let Some(memo) = memo {
            self.lazy_member_stats.member_table_declared_members += memo.named;
            if memo.pending.is_empty() {
                memo.settled
            } else {
                let pending = std::mem::take(&mut memo.pending);
                let settled = memo.settled;
                let mut added: Vec<&'static str> = Vec::new();
                let mut still_pending = Vec::new();
                for (name, symbol) in pending {
                    match self.settled_unaffected(symbol, mapper) {
                        Some(true) => added.push(name),
                        Some(false) => {}
                        None => still_pending.push((name, symbol)),
                    }
                }
                let memo = self.lazy_unaffected.get_mut(&source).unwrap();
                memo.pending = still_pending;
                if !added.is_empty() {
                    added.extend_from_slice(settled);
                    added.sort_unstable();
                    memo.settled = alloc_vec(added);
                }
                memo.settled
            }
        } else {
            let mut settled: Vec<&'static str> = Vec::new();
            let mut pending = Vec::new();
            let mut named = 0;
            for (name, symbol) in declared_members.entries() {
                if self.is_named_member(symbol, name) {
                    named += 1;
                    match self.settled_unaffected(symbol, mapper) {
                        Some(true) => settled.push(name),
                        Some(false) => {}
                        None => pending.push((name, symbol)),
                    }
                }
            }
            self.lazy_member_stats.member_table_declared_members += named;
            settled.sort_unstable();
            let settled = alloc_vec(settled);
            self.lazy_unaffected.insert(
                source,
                LazyUnaffected {
                    table: declared_members,
                    len: declared_members.len(),
                    maps_this_only,
                    named,
                    settled,
                    pending,
                },
            );
            settled
        };
        if lazy_unaffected_shadow() {
            let mut fresh: Vec<&'static str> = Vec::new();
            for (name, symbol) in declared_members.entries() {
                if self.is_named_member(symbol, name)
                    && self.is_symbol_unaffected_by_instantiation(symbol, Some(mapper))
                {
                    fresh.push(name);
                }
            }
            fresh.sort_unstable();
            assert!(
                fresh == result,
                "TSRS_LAZY_UNAFFECTED=shadow: target {}: reused {:?}, fresh {:?}",
                source.id.0,
                result,
                fresh
            );
        }
        result
    }

    /// `is_symbol_unaffected_by_instantiation(symbol, mapper)` if that answer cannot change any more (see
    /// `lazy_unaffected_members`), else `None`.
    fn settled_unaffected(&mut self, symbol: P<Symbol>, mapper: P<TypeMapper>) -> Option<bool> {
        if self.is_symbol_unaffected_by_instantiation(symbol, Some(mapper)) {
            return Some(true);
        }
        let links = self.value_symbol_links.get(symbol);
        (links.resolved_type.get().is_some()
            && (!symbol.flags().intersects(SymbolFlags::SetAccessor)
                || links.write_type().is_some()))
        .then_some(false)
    }

    // Mirrors resolveObjectTypeMembers without creating member symbols.
    fn prepare_lazy_members(
        &mut self,
        t: P<Type>,
        lm: P<LazyMemberTable>,
        this_argument: Option<P<Type>>,
    ) {
        let source = t.object_target().unwrap();
        let resolved = self.resolve_declared_members(source);
        let unaffected = match resolved.declared_members.get() {
            Some(declared_members) => {
                self.lazy_unaffected_members(source, declared_members, lm.mapper)
            }
            None => &[],
        };
        let mut call_signatures =
            self.instantiate_signatures(resolved.declared_call_signatures.get(), lm.mapper);
        let mut construct_signatures =
            self.instantiate_signatures(resolved.declared_construct_signatures.get(), lm.mapper);
        let mut index_infos =
            self.instantiate_index_infos(resolved.declared_index_infos.get(), lm.mapper);
        let mut base_types: Vec<P<Type>> = Vec::new();
        for &base_type in self.get_base_types(source) {
            let mut instantiated_base_type = base_type;
            if this_argument.is_some() {
                let inst = self.instantiate_type(base_type, Some(lm.mapper));
                instantiated_base_type = self.get_type_with_this_argument(
                    inst,
                    this_argument,
                    false, /*needsApparentType*/
                );
            }
            base_types.push(instantiated_base_type);
            let reduced = self.get_reduced_apparent_type(instantiated_base_type);
            if !reduced.flags().intersects(TypeFlags::Intersection)
                && self.get_ready_lazy_member_table(reduced).is_none()
            {
                self.get_properties_of_type(instantiated_base_type);
            }
            self.append_inherited_signatures_and_index_infos(
                &mut call_signatures,
                &mut construct_signatures,
                &mut index_infos,
                instantiated_base_type,
            );
        }
        let _ = lm.ready.set(LazyMembers {
            unaffected: ThinSlice::new(unaffected),
            call_signatures: ThinSlice::new(alloc_vec(call_signatures)),
            construct_signatures: ThinSlice::new(alloc_vec(construct_signatures)),
            index_infos: ThinSlice::new(alloc_vec(index_infos)),
            base_types: ThinSlice::new(alloc_vec(base_types)),
        });
        if t.object_flags_lazy().intersects(ObjectFlags::MembersResolved) {
            // t was resolved while preparing; resolveObjectTypeMembers would now replace its members.
            self.resolve_lazy_members(t, lm);
        }
    }

    /// Shared graph: `lm`, or this checker's copy of it if it belongs to the frozen seed (forks fill tables).
    #[inline]
    fn own_lazy_member_table(&mut self, t: P<Type>, lm: P<LazyMemberTable>) -> P<LazyMemberTable> {
        if !tsrs_core::sharedgraph::frozen(lm.get()) {
            return lm;
        }
        let ready = std::cell::OnceCell::new();
        if let Some(r) = lm.ready.get() {
            let _ = ready.set(r.clone());
        }
        let ordered_properties = std::cell::OnceCell::new();
        if let Some(&o) = lm.ordered_properties.get() {
            let _ = ordered_properties.set(o);
        }
        let copy = P::new(LazyMemberTable {
            mapper: lm.mapper,
            ready,
            declared: lm.declared.clone_value(),
            ordered_properties,
        });
        self.lazy_member_tables.insert(t, copy);
        copy
    }

    fn resolve_lazy_members(&mut self, t: P<Type>, lm: P<LazyMemberTable>) {
        self.lazy_member_stats.member_tables_resolved_in_full += 1;
        if t.target().unwrap().object_flags().intersects(ObjectFlags::Tuple) {
            self.lazy_member_stats.tuple_tables_resolved_in_full += 1;
        }
        let resolved = self.resolve_declared_members(t.target().unwrap());
        let mut members: Option<P<SymbolTable>> = None;
        if let Some(declared_members) = resolved.declared_members.get().filter(|m| !m.is_empty()) {
            let table = SymbolTable::with_capacity(declared_members.len());
            for (id, symbol) in declared_members.entries() {
                if self.is_named_member(symbol, id) {
                    let member = self.get_lazy_declared_member(lm, symbol, id);
                    table.set(id, member);
                }
            }
            members = Some(table);
        }
        let ready = lm.ready.get().unwrap();
        for &base_type in ready.base_types.get() {
            let base_properties = self.get_properties_of_type(base_type);
            members = Self::add_inherited_members(members, base_properties);
        }
        self.set_structured_type_members(
            t,
            members,
            ready.call_signatures.get(),
            ready.construct_signatures.get(),
            ready.index_infos.get(),
        );
        self.lazy_member_tables.remove(&t);
        // tsrs-only: every instantiated member in `lm.declared` is now in t's resolved members, and nothing looks
        // the table up again (`get_ready_lazy_member_table` answers None once t's members are resolved). The table
        // is in the arena, which never drops it, so free its heap buffers here.
        lm.declared.clear_and_free();
    }

    #[cfg_attr(feature = "site-counts", track_caller)]
    fn get_lazy_declared_member(
        &mut self,
        lm: P<LazyMemberTable>,
        symbol: P<Symbol>,
        name: &'static str,
    ) -> P<Symbol> {
        let existing = lm.declared.lookup(name);
        if let Some(result) = existing {
            return result;
        }
        let mut result = symbol;
        if lm.ready.get().unwrap().unaffected.get().binary_search(&name).is_err() {
            self.lazy_member_stats.member_table_declared_instantiated += 1;
            result = self.new_instantiated_symbol(symbol, Some(lm.mapper));
        }
        lm.declared.set(name, result);
        result
    }

    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn get_member_of_structured_type(
        &mut self,
        t: P<Type>,
        name: &str,
    ) -> Option<P<Symbol>> {
        self.get_member_of_structured_type_ex(t, name, true /*instantiate*/)
    }

    #[cfg_attr(feature = "site-counts", track_caller)]
    fn get_member_of_structured_type_ex(
        &mut self,
        t: P<Type>,
        name: &str,
        instantiate: bool,
    ) -> Option<P<Symbol>> {
        if t.object_flags_lazy().intersects(ObjectFlags::MembersResolved) {
            return t.as_structured_type().members()?.lookup(name);
        }
        self.get_member_of_unresolved_structured_type(t, name, instantiate)
    }

    // Without instantiate, a declared member that has not been instantiated yet is returned as is (for callers that
    // only test for the member or look at its flags).
    #[cfg_attr(feature = "site-counts", track_caller)]
    fn get_member_of_unresolved_structured_type(
        &mut self,
        t: P<Type>,
        name: &str,
        instantiate: bool,
    ) -> Option<P<Symbol>> {
        if t.object_flags().intersects(ObjectFlags::Mapped)
            && !is_reserved_member_name(name)
            && let Some(lazy) = self.get_lazy_mapped_table(t)
            && let Some(member) = self.get_lazy_mapped_type_member(t, &lazy, name)
        {
            return member;
        }
        let lm = self.get_ready_lazy_member_table(t);
        let Some(lm) = lm.filter(|_| !is_reserved_member_name(name)) else {
            return self.resolve_structured_type_members(t).members()?.lookup(name);
        };
        self.lazy_member_stats.member_lookups += 1;
        // The declared member, else the first base type's property (see addInheritedMembers).
        let mut result: Option<P<Symbol>> = None;
        let declared_members =
            self.resolve_declared_members(t.object_target().unwrap()).declared_members.get();
        if let Some((name, decl)) = declared_members.and_then(|m| m.lookup_entry(name))
            && self.is_named_member(decl, name)
        {
            if instantiate {
                result = Some(self.get_lazy_declared_member(lm, decl, name));
            } else {
                self.lazy_member_stats.has_prop_uninstantiated += 1;
                result = Some(lm.declared.lookup(name).unwrap_or(decl));
            }
        }
        for &base_type in lm.ready.get().unwrap().base_types.get() {
            if result.is_some_and(|r| r.flags().intersects(SymbolFlags::Value)) {
                break;
            }
            if let Some(prop) = self.get_property_of_type_worker(
                base_type,
                name,
                true,  /*skipObjectFunctionPropertyAugment*/
                false, /*includeTypeOnlyMembers*/
                instantiate,
            ) && !is_static_private_identifier_property(prop)
            {
                result = Some(prop);
            }
        }
        result
    }

    // f may see a declared member instead of its instantiation, which has the same flags.
    pub(crate) fn every_property_of_structured_type(
        &mut self,
        t: P<Type>,
        f: &mut dyn FnMut(&mut Checker, P<Symbol>) -> bool,
    ) -> bool {
        if let Some(lm) = self.get_ready_lazy_member_table(t) {
            self.lazy_member_stats.member_every_property_queries += 1;
            let mut seen = self.free_name_sets.pop().unwrap_or_default();
            let result = self.every_lazy_property(t, lm, &mut seen, f);
            if seen.capacity() <= 1024 {
                seen.clear();
                self.free_name_sets.push(seen);
            }
            return result;
        }
        let properties = self.resolve_structured_type_members(t).properties();
        properties.iter().all(|&p| f(self, p))
    }

    pub(crate) fn has_properties_of_structured_type(&mut self, t: P<Type>) -> bool {
        !self.every_property_of_structured_type(t, &mut |_, _| false)
    }

    // seen has the names of properties that hide inherited ones, as in addInheritedMembers.
    fn every_lazy_property(
        &mut self,
        t: P<Type>,
        lm: P<LazyMemberTable>,
        seen: &mut FxHashSet<&'static str>,
        f: &mut dyn FnMut(&mut Checker, P<Symbol>) -> bool,
    ) -> bool {
        if let Some(declared_members) =
            self.resolve_declared_members(t.target().unwrap()).declared_members.get()
            && (0..declared_members.len()).any(|i| {
                let (id, symbol) = declared_members.entry(i);
                self.is_named_member(symbol, id) && seen.insert(id) && !f(self, symbol)
            })
        {
            return false;
        }
        for &base_type in lm.ready.get().unwrap().base_types.get() {
            let reduced = self.get_reduced_apparent_type(base_type);
            if let Some(base_table) = self.get_ready_lazy_member_table(reduced) {
                if !self.every_lazy_property(reduced, base_table, seen, f) {
                    return false;
                }
                continue;
            }
            if self.get_properties_of_type(base_type).iter().copied().into_iter().any(|prop| {
                !is_static_private_identifier_property(prop)
                    && seen.insert(prop.name())
                    && !f(self, prop)
            }) {
                return false;
            }
        }
        true
    }
}

impl Checker {
    // checker.go:19508
    pub(crate) fn get_base_types(&mut self, t: P<Type>) -> &'static [P<Type>] {
        if !t.object_flags().intersects(ObjectFlags::ClassOrInterface | ObjectFlags::Tuple) {
            return &[];
        }
        let data = t.as_interface_type();
        if !data.base_types_resolved.get() {
            if !self.push_type_resolution(t.into(), TypeSystemPropertyName::ResolvedBaseTypes) {
                return data.resolved_base_types.get();
            }
            let canonical = !tsrs_core::compat::go_compatible_history();
            if canonical {
                self.base_types_depth += 1;
                self.base_types_resolved_log.push(t);
            }
            let symbol = t.symbol();
            if t.object_flags().intersects(ObjectFlags::Tuple) {
                let base = self.get_tuple_base_type(t);
                data.resolved_base_types.set(alloc_vec(vec![base]));
            } else if symbol
                .unwrap()
                .flags()
                .intersects(SymbolFlags::Class | SymbolFlags::Interface)
            {
                let symbol = symbol.unwrap();
                if symbol.flags().intersects(SymbolFlags::Class) {
                    self.resolve_base_types_of_class(t);
                }
                if symbol.flags().intersects(SymbolFlags::Interface) {
                    self.resolve_base_types_of_interface(t);
                }
            } else {
                panic!("Unhandled case in getBaseTypes");
            }
            // Go: `t.symbol.Declarations != nil`; a nil and an empty declaration list both iterate nothing.
            if !self.pop_type_resolution() {
                if canonical {
                    self.base_types_circular.push(t);
                }
                let declarations = t.symbol().unwrap().declarations();
                for &declaration in declarations {
                    if is_class_declaration(declaration) || is_interface_declaration(declaration) {
                        self.report_circular_base_type(declaration, t);
                    }
                }
            }
            // In general, base type resolution always precedes member resolution. However, it is possible
            // for resolution of type parameter defaults to cause circularity errors, possibly leaving
            // members partially resolved. Here we ensure any such partial resolution is reset.
            // See https://github.com/microsoft/TypeScript/issues/16861 for an example.
            t.object_flags.set(
                t.object_flags.get_lazy()
                    & !(ObjectFlags::MembersResolved | ObjectFlags::WeakTypeMemo),
            );
            self.members_reset_epoch += 1;
            self.augment_filter = None; // t may be one of the filter's four types: its members are resolved again
            data.base_types_resolved.set(true);
            if canonical {
                self.base_types_depth -= 1;
                if self.base_types_depth == 0 {
                    self.canonicalize_base_type_cycles(t);
                }
            }
        }
        data.resolved_base_types.get()
    }

    // Go resolves a cycle of base types (`interface A extends B`, `interface B extends A`) from whichever member a
    // checker asks about first: that member drops its edge into the cycle (TS2310) and the others keep theirs, so the
    // inherited members, and the diagnostics that follow from them, depend on what the checker resolved before
    // (notes/fix-history-dependent-diagnostics.md). By default tsrs enters every cycle of interfaces at the member
    // declared first in program order (`earliest_declaration`): when the outermost resolution `t` found a cycle entered
    // elsewhere, the base types it resolved are reset and resolved again from that member. If any circular type that
    // resolution found is a class or has no symbol, nothing is reset (Go's result stands for all of its cycles).
    // `go_compatible_history()` keeps Go's result.
    fn canonicalize_base_type_cycles(&mut self, t: P<Type>) {
        if self.base_types_circular.is_empty() {
            self.base_types_resolved_log.clear();
            return;
        }
        let resolved = std::mem::take(&mut self.base_types_resolved_log);
        let circular = std::mem::take(&mut self.base_types_circular);
        // The cycle was entered at the circular type resolved outermost, the last one to finish. Class cycles also
        // go through the base constructor type, which is not reset here.
        let Some(&entry) = circular.last() else {
            return;
        };
        if circular
            .iter()
            .any(|c| c.symbol().is_none_or(|s| s.flags().intersects(SymbolFlags::Class)))
        {
            return;
        }
        let mut first = entry;
        let mut first_declaration = self.earliest_declaration(entry);
        for &c in &circular {
            let declaration = self.earliest_declaration(c);
            if self.compare_nodes(declaration, first_declaration).is_lt() {
                first = c;
                first_declaration = declaration;
            }
        }
        if first == entry {
            return;
        }
        for &r in &resolved {
            let data = r.as_interface_type();
            data.base_types_resolved.set(false);
            data.resolved_base_types.set(&[]);
            r.object_flags.set(
                r.object_flags.get_lazy()
                    & !(ObjectFlags::MembersResolved | ObjectFlags::WeakTypeMemo),
            );
        }
        self.members_reset_epoch += 1;
        self.augment_filter = None; // as in get_base_types
        // Each pass resolves `first` for good, so a later reset (another cycle) covers fewer types and this ends.
        self.get_base_types(first);
        self.get_base_types(t);
    }

    // The declaration of `t`'s symbol that comes first in program order (`compareNodes`). The symbol's first
    // declaration is not always that one: global augmentations are merged after the files' globals.
    fn earliest_declaration(&mut self, t: P<Type>) -> Option<P<Node>> {
        let mut earliest = None;
        earliest = t.symbol().unwrap().declarations().iter().fold(earliest, |mut earliest, &d| {
            if earliest.is_none() || self.compare_nodes(Some(d), earliest).is_lt() {
                earliest = Some(d);
            }

            earliest
        });
        earliest
    }

    // checker.go:19547
    fn get_tuple_base_type(&mut self, t: P<Type>) -> P<Type> {
        let type_parameters = t.as_tuple_type().type_parameters();
        let element_infos = t.as_tuple_type().element_infos.get();
        let mut element_types: Vec<P<Type>> = Vec::with_capacity(type_parameters.len());
        element_types.extend(type_parameters.iter().enumerate().map(|(i, &tp)| {
            if element_infos[i].flags.intersects(ElementFlags::Variadic) {
                self.get_indexed_access_type(tp, self.number_type)
            } else {
                tp
            }
        }));
        let u = self.get_union_type(&element_types);
        self.create_array_type_ex(u, t.as_tuple_type().readonly.get())
    }

    // checker.go:19561
    fn resolve_base_types_of_class(&mut self, t: P<Type>) {
        let base_constructor = self.get_base_constructor_type_of_class(t);
        let base_constructor_type = self.get_apparent_type(base_constructor);
        if !base_constructor_type
            .flags()
            .intersects(TypeFlags::Object | TypeFlags::Intersection | TypeFlags::Any)
        {
            return;
        }
        let base_type_node = get_base_type_node_of_class(t).unwrap();

        let mut original_base_type: Option<P<Type>> = None;
        if let Some(symbol) = base_constructor_type.symbol() {
            original_base_type = Some(self.get_declared_type_of_symbol(symbol));
        }
        let base_type: P<Type> = if base_constructor_type
            .symbol()
            .is_some_and(|s| s.flags().intersects(SymbolFlags::Class))
            && self.are_all_outer_type_parameters_applied(original_base_type.unwrap())
        {
            // When base constructor type is a class with no captured type arguments we know that the constructors all have the same type parameters as the
            // class and all return the instance type of the class. There is no need for further checks and we can apply the
            // type arguments in the same manner as a type reference to get the same error reporting experience.
            self.get_type_from_class_or_interface_reference(
                base_type_node,
                base_constructor_type.symbol().unwrap(),
            )
        } else if base_constructor_type.flags().intersects(TypeFlags::Any) {
            base_constructor_type
        } else {
            // The class derives from a "class-like" constructor function, check that we have at least one construct signature
            // with a matching number of type parameters and use the return type of the first instantiated signature. Elsewhere
            // we check that all instantiated signatures return the same type.
            let constructors = self.get_instantiated_constructors_for_type_arguments(
                base_constructor_type,
                base_type_node.type_arguments(),
                base_type_node,
            );
            if constructors.is_empty() {
                self.error(
                    base_type_node.expression(),
                    &diagnostics::NO_BASE_CONSTRUCTOR_HAS_THE_SPECIFIED_NUMBER_OF_TYPE_ARGUMENTS,
                    &[],
                );
                return;
            }
            self.get_return_type_of_signature(constructors[0])
        };
        if self.is_error_type(base_type) {
            return;
        }
        let reduced_base_type = self.get_reduced_type(base_type);
        if !self.is_valid_base_type(reduced_base_type) {
            let error_node = base_type_node.expression().unwrap();
            let diagnostic = self.elaborate_never_intersection(None, error_node, base_type);
            let type_string = self.type_to_string(reduced_base_type, None);
            let diagnostic = new_diagnostic_chain_for_node(
                diagnostic,
                error_node,
                Some(&diagnostics::BASE_CONSTRUCTOR_RETURN_TYPE_0_IS_NOT_AN_OBJECT_TYPE_OR_INTERSECTION_OF_OBJECT_TYPES_WITH_STATICALLY_KNOWN_MEMBERS),
                &[&type_string],
            );
            self.add_diagnostic(diagnostic);
            return;
        }
        if t == reduced_base_type || self.has_base_type(reduced_base_type, Some(t)) {
            let type_string = self.type_to_string(t, None);
            self.error(
                t.symbol().unwrap().value_declaration(),
                &diagnostics::TYPE_0_RECURSIVELY_REFERENCES_ITSELF_AS_A_BASE_TYPE,
                &[&type_string],
            );
            return;
        }
        t.as_interface_type().resolved_base_types.set(alloc_vec(vec![reduced_base_type]));
    }
}

/// See `Checker::resolved_property_source`.
pub(crate) struct ResolvedPropertySource {
    t: P<Type>,
    members: Option<P<SymbolTable>>,
    epoch: u32,
}
