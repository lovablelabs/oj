use crate::*;
use rustc_hash::FxHashMap;
use std::rc::Rc;
use tsrs_ast as ast;
use tsrs_ast::*;
use tsrs_core::*;

// Non-function declarations in nodebuilderimpl.go:1-1850 (hand-ported by printer-foundation in nodebuilder_types.rs):
//   type CompositeSymbolIdentity (nodebuilderimpl.go:24)
//   type TrackedSymbolArgs (nodebuilderimpl.go:30)
//   type SerializedTypeEntry (nodebuilderimpl.go:36)
//   type CompositeTypeCacheIdentity (nodebuilderimpl.go:43)
//   type NodeBuilderLinks (nodebuilderimpl.go:49)
//   type NodeBuilderSymbolLinks (nodebuilderimpl.go:54)
//   type moduleSpecifierResult (nodebuilderimpl.go:58)
//   type NodeBuilderContext (nodebuilderimpl.go:63)
//   type NodeBuilderImpl (nodebuilderimpl.go:97)
//   const (nodebuilderimpl.go:118): defaultMaximumTruncationLength, noTruncationMaximumTruncationLength
//   type sortedSymbolNamePair (nodebuilderimpl.go:1081)

// nodebuilderimpl.go:125
pub(crate) fn new_node_builder_impl(
    ch: &Checker,
    e: P<EmitContext>,
    id_to_symbol: Option<P<RefCell<FxHashMap<P<Node>, P<Symbol>>>>>,
) -> P<NodeBuilderImpl> {
    // A builder whose emit context is a per-file scratch one (emit's requests) dies with that file, and so do its
    // state and caches (notes/mem-emit-regions.md).
    let scratch = e.factory.is_scratch();
    // The map is shared with the caller (Go map semantics): language service code reads it back.
    let id_to_symbol = match id_to_symbol {
        Some(id_to_symbol) => id_to_symbol,
        None => P::new_in(scratch, RefCell::new(FxHashMap::default())),
    };
    let b = P::new_in(
        scratch,
        NodeBuilderImpl {
            f: e.factory.as_node_factory().clone(),
            e,
            pc: new_pseudo_checker(ch.strict_null_checks),
            links: tsrs_core::LinkStore::new_scratch(scratch),
            symbol_links: tsrs_core::LinkStore::new_scratch(scratch),
            ctx: Cell::new(None),
            clone_binding_name_visitor: std::cell::OnceCell::new(),
            id_to_symbol,
            checker_slot: P::new_in(scratch, CheckerSlot::default()),
            this: std::cell::OnceCell::new(),
        },
    );
    let _ = b.this.set(b);
    // Go: b.cloneBindingNameVisitor = ast.NewNodeVisitor(b.cloneBindingName, b.f, ast.NodeVisitorHooks{})
    let slot = b.checker_slot;
    let visit: VisitFn = Rc::new(move |_v: &mut NodeVisitor, node: P<Node>| {
        Some(slot.with(|c| b.clone_binding_name(c, node)))
    });
    let _ = b.clone_binding_name_visitor.set(visit);
    b
}

impl NodeBuilderImpl {
    // Go `b.ctx.tracker` (always set while a node builder entry point runs).
    pub(crate) fn tracker(&self) -> &'static dyn SymbolTracker {
        self.ctx().tracker.get().unwrap()
    }

    // nodebuilderimpl.go:134
    pub(crate) fn save_restore_flags(&self, _c: &mut Checker) -> Box<dyn FnMut(&mut Checker)> {
        let flags = self.ctx().flags.get();
        let internal_flags = self.ctx().internal_flags.get();
        let depth = self.ctx().depth.get();

        let b = self.as_p();
        Box::new(move |_c: &mut Checker| {
            b.ctx().flags.set(flags);
            b.ctx().internal_flags.set(internal_flags);
            b.ctx().depth.set(depth);
        })
    }

    // nodebuilderimpl.go:146
    pub(crate) fn check_truncation_length(&self, _c: &mut Checker) -> bool {
        let ctx = self.ctx();
        if ctx.truncating.get() {
            return ctx.truncating.get();
        }
        let max_length = if ctx.flags.get().intersects(Flags::NoTruncation) {
            NO_TRUNCATION_MAXIMUM_TRUNCATION_LENGTH
        } else if ctx.max_truncation_length.get() > 0 {
            ctx.max_truncation_length.get()
        } else {
            DEFAULT_MAXIMUM_TRUNCATION_LENGTH
        };
        ctx.truncating.set(ctx.approximate_length.get() > max_length);
        ctx.truncating.get()
    }

    // checkTruncationLengthIfExpanding returns true if maxExpansionDepth >= 0 and truncation length exceeded.
    // When expanding, we need to mark the output as truncated so we know not to offer further expansion.
    // nodebuilderimpl.go:164
    pub(crate) fn check_truncation_length_if_expanding(&self, c: &mut Checker) -> bool {
        if self.ctx().max_expansion_depth.get().is_some() && self.check_truncation_length(c) {
            self.ctx().expansion_truncated.set(true);
            return true;
        }
        false
    }

    // isExpandableType reports whether t has a named representation that could be inlined
    // as its structural form during hover expansion. Filters out lib types.
    // When isAlias is true, checks whether t's alias symbol is from user code (not lib).
    // nodebuilderimpl.go:175
    fn is_expandable_type(c: &Checker, t: P<Type>, is_alias: bool) -> bool {
        if is_alias {
            return !c.is_lib_symbol_for_hover_verbosity(t.alias().symbol());
        }
        if c.is_lib_type_for_hover_verbosity(t) {
            return false;
        }
        let object_flags = t.object_flags();
        if t.flags().intersects(TypeFlags::EnumLike)
            || object_flags.intersects(ObjectFlags::Reference)
            || object_flags.intersects(ObjectFlags::ClassOrInterface)
        {
            return true;
        }
        if object_flags.intersects(ObjectFlags::Anonymous)
            && t.symbol().is_some()
            && t.symbol().unwrap().flags().intersects(
                SymbolFlags::Class
                    | SymbolFlags::Enum
                    | SymbolFlags::ValueModule
                    | SymbolFlags::Function
                    | SymbolFlags::Method,
            )
        {
            return true;
        }
        false
    }

    // isTypeOnStack reports whether t is already being processed in the current expansion,
    // excluding the last element (which is the type currently being serialized by typeToTypeNode).
    // nodebuilderimpl.go:197
    fn is_type_on_stack(&self, _c: &mut Checker, t: P<Type>) -> bool {
        let ctx = self.ctx();
        let type_stack = ctx.type_stack.borrow();

        (0..type_stack.len().saturating_sub(1)).any(|i| type_stack[i] == Some(t))
    }

    // shouldExpandType decides whether to expand this type at the current depth.
    // Returns true when depth < maxExpansionDepth (expand now).
    // At the boundary (depth == maxExpansionDepth), sets canIncreaseExpansionDepth
    // to signal that a higher verbosity level would reveal more detail.
    // Returns false when expansion is disabled (maxExpansionDepth < 0), the type
    // is not expandable, or the type is cyclic.
    // nodebuilderimpl.go:212
    pub(crate) fn should_expand_type(&self, c: &mut Checker, t: P<Type>, is_alias: bool) -> bool {
        let Some(max_expansion_depth) = self.ctx().max_expansion_depth.get() else {
            return false;
        };
        if !Self::is_expandable_type(c, t, is_alias) {
            return false;
        }
        if self.is_type_on_stack(c, t) {
            return false;
        }
        if self.ctx().depth.get() < max_expansion_depth {
            return true;
        }
        self.ctx().can_increase_expansion_depth.set(true);
        false
    }

    // isActivelyExpanding reports whether the current depth is below maxExpansionDepth,
    // meaning type-node reuse should be skipped so typeToTypeNode can expand named types.
    // nodebuilderimpl.go:231
    pub(crate) fn is_actively_expanding(&self, _c: &mut Checker) -> bool {
        self.ctx()
            .max_expansion_depth
            .get()
            .is_some_and(|max_depth| max_depth > 0 && self.ctx().depth.get() < max_depth)
    }

    // checkTypeExpandability probes whether a type (or its type arguments) could be expanded,
    // for use after type-node reuse where shouldExpandType was never called.
    // Delegates to shouldExpandType for the actual check, then recurses into type arguments
    // of reference types (e.g., Apple inside Promise<Apple>).
    // nodebuilderimpl.go:239
    pub(crate) fn check_type_expandability(&self, c: &mut Checker, t: Option<P<Type>>) {
        if self.ctx().max_expansion_depth.get().is_none()
            || t.is_none()
            || self.ctx().can_increase_expansion_depth.get()
        {
            return;
        }
        let t = t.unwrap();
        // Push t onto the type stack so shouldExpandType's cycle detection works correctly.
        self.ctx().type_stack.borrow_mut().push(Some(t));
        self.check_type_expandability_worker(c, t);
        // Go: defer b.ctx.typeStack = b.ctx.typeStack[:len(b.ctx.typeStack)-1]
        let ctx = self.ctx();
        let mut type_stack = ctx.type_stack.borrow_mut();
        let n = type_stack.len();
        type_stack.truncate(n - 1);
    }

    // The part of checkTypeExpandability that runs while t is pushed (Go's deferred pop).
    fn check_type_expandability_worker(&self, c: &mut Checker, t: P<Type>) {
        // If t is an ancestor in the current expansion, return early to avoid unbounded recursion.
        if self.is_type_on_stack(c, t) {
            return;
        }
        if t.alias().is_some() {
            self.should_expand_type(c, t, true);
        }
        if !self.ctx().can_increase_expansion_depth.get() {
            self.should_expand_type(c, t, false);
        }
        if self.ctx().can_increase_expansion_depth.get() {
            return;
        }
        // Recurse into type arguments (e.g., check Apple in Promise<Apple>).
        if t.object_flags().intersects(ObjectFlags::Reference) {
            for &arg in c.get_type_arguments(t) {
                self.check_type_expandability(c, Some(arg));
                if self.ctx().can_increase_expansion_depth.get() {
                    return;
                }
            }
        }
    }

    // nodebuilderimpl.go:272
    pub(crate) fn append_reference_to_type(
        &self,
        c: &mut Checker,
        root: P<Node>,
        ref_: P<Node>,
    ) -> P<Node> {
        if is_import_type_node(root) {
            // first shift type arguments

            // !!! In the old emitter, an Identifier could have type arguments for use with quickinfo:
            // typeArguments := root.TypeArguments
            // qualifier := root.AsImportTypeNode().Qualifier
            // if qualifier != nil {
            // 	if ast.IsIdentifier(qualifier) {
            // 		if typeArguments != getIdentifierTypeArguments(qualifier) {
            // 			qualifier = setIdentifierTypeArguments(b.f.CloneNode(qualifier), typeArguments)
            // 		}
            // 	} else {
            // 		if typeArguments != getIdentifierTypeArguments(qualifier.Right) {
            // 			qualifier = b.f.UpdateQualifiedName(qualifier, qualifier.Left, setIdentifierTypeArguments(b.f.cloneNode(qualifier.Right), typeArguments))
            // 		}
            // 	}
            // }
            // !!! Without the above, nested type args are silently elided
            let imprt = root.as_import_type_node();
            // then move qualifiers
            let ids = get_access_stack(ref_);
            let mut qualifier = root.as_import_type_node().qualifier;
            for id in ids {
                if let Some(q) = qualifier {
                    qualifier = Some(self.f.new_qualified_name(q, id));
                } else {
                    qualifier = Some(id);
                }
            }
            return self.f.update_import_type_node(
                root,
                imprt.is_type_of,
                imprt.argument,
                imprt.attributes,
                qualifier,
                ref_.type_argument_list(),
            );
        } else if is_type_reference_node(root) {
            let type_ref = root.as_type_reference_node();
            let type_arguments = root.type_argument_list();
            if self.ctx().flags.get().intersects(Flags::UseInstantiationExpressions)
                && type_arguments.is_some()
                && !type_arguments.unwrap().nodes().is_empty()
            {
                let access = self.create_access_expression(type_ref.type_name);
                let mut expr =
                    self.create_expression_with_type_arguments(c, access, type_arguments);
                expr = get_access_stack(ref_).into_iter().fold(expr, |mut expr, id| {
                    expr = self.f.new_property_access_expression(expr, None, id, NodeFlags::None);

                    expr
                });
                return expr;
            }
            let mut type_name: P<Node> = type_ref.type_name;
            type_name = get_access_stack(ref_).into_iter().fold(type_name, |mut type_name, id| {
                type_name = self.f.new_qualified_name(type_name, id);

                type_name
            });
            return self.f.update_type_reference_node(root, type_name, ref_.type_argument_list());
        }
        let mut expr = self.create_access_expression(root);
        expr = get_access_stack(ref_).into_iter().fold(expr, |mut expr, id| {
            expr = self.f.new_property_access_expression(expr, None, id, NodeFlags::None);

            expr
        });
        expr
    }
}

// nodebuilderimpl.go:325
fn get_access_stack(ref_: P<Node>) -> Vec<P<Node>> {
    let mut state: P<Node> = ref_.as_type_reference_node().type_name;
    let mut ids: Vec<P<Node>> = vec![];
    while !is_identifier(state) {
        let entity = state.as_qualified_name();
        ids.insert(0, entity.right);
        state = entity.left;
    }
    ids.insert(0, state);
    ids
}

// nodebuilderimpl.go:337
pub(crate) fn is_class_instance_side(c: &mut Checker, t: P<Type>) -> bool {
    t.symbol().is_some()
        && t.symbol().unwrap().flags().intersects(SymbolFlags::Class)
        && (t == c.get_declared_type_of_class_or_interface(t.symbol().unwrap())
            || (t.flags().intersects(TypeFlags::Object)
                && t.object_flags().intersects(ObjectFlags::IsClassInstanceClone)))
}

impl NodeBuilderImpl {
    // nodebuilderimpl.go:341
    pub(crate) fn create_elided_information_placeholder(&self, _c: &mut Checker) -> P<Node> {
        let ctx = self.ctx();
        ctx.approximate_length.set(ctx.approximate_length.get() + 3);
        if !ctx.flags.get().intersects(Flags::NoTruncation) {
            return self.f.new_type_reference_node(
                self.f.new_identifier("..."),
                None, /*typeArguments*/
            );
        }
        self.e.add_synthetic_leading_comment(
            self.f.new_keyword_type_node(Kind::AnyKeyword),
            Kind::MultiLineCommentTrivia,
            "elided",
            false, /*hasTrailingNewLine*/
        )
    }

    // nodebuilderimpl.go:349
    pub(crate) fn map_to_type_nodes(
        &self,
        c: &mut Checker,
        list: &[P<Type>],
        is_bare_list: bool,
    ) -> Option<P<NodeList>> {
        #[derive(Clone, Copy)]
        struct SeenName {
            t: P<Type>,
            i: usize,
        }

        if list.is_empty() {
            return None;
        }

        if self.check_truncation_length(c) {
            if !is_bare_list {
                let node = if self.ctx().flags.get().intersects(Flags::NoTruncation) {
                    self.e.add_synthetic_leading_comment(
                        self.f.new_keyword_type_node(Kind::AnyKeyword),
                        Kind::MultiLineCommentTrivia,
                        "elided",
                        false, /*hasTrailingNewLine*/
                    )
                } else {
                    self.f.new_type_reference_node(
                        self.f.new_identifier("..."),
                        None, /*typeArguments*/
                    )
                };
                return Some(self.f.new_node_list(vec![node]));
            } else if list.len() > 2 {
                let first = self.type_to_type_node(c, Some(list[0]));
                let last = self.type_to_type_node(c, Some(list[list.len() - 1]));

                let middle = if self.ctx().flags.get().intersects(Flags::NoTruncation) {
                    self.e.add_synthetic_leading_comment(
                        self.f.new_keyword_type_node(Kind::AnyKeyword),
                        Kind::MultiLineCommentTrivia,
                        &format!("... {} more elided ...", list.len() - 2),
                        false, /*hasTrailingNewLine*/
                    )
                } else {
                    let text = format!("... {} more ...", list.len() - 2);
                    self.f.new_type_reference_node(
                        self.f.new_identifier(self.f.alloc_text(&text)),
                        None, /*typeArguments*/
                    )
                };
                return Some(self.f.new_node_list(vec![first.unwrap(), middle, last.unwrap()]));
            }
        }

        let may_have_name_collisions =
            !self.ctx().flags.get().intersects(Flags::UseFullyQualifiedType);
        let mut seen_names: Option<tsrs_core::collections::MultiMap<&'static str, SeenName>> = None;
        if may_have_name_collisions {
            seen_names = Some(tsrs_core::collections::MultiMap::default());
        }

        let mut result: Vec<P<Node>> = Vec::with_capacity(list.len());

        for (i, &t) in list.iter().enumerate() {
            let display_index = i + 1;
            if self.check_truncation_length(c) && (display_index + 2 < list.len() - 1) {
                if self.ctx().flags.get().intersects(Flags::NoTruncation) {
                    result.push(self.e.add_synthetic_leading_comment(
                        self.f.new_keyword_type_node(Kind::AnyKeyword),
                        Kind::MultiLineCommentTrivia,
                        &format!("... {} more elided ...", list.len() - display_index),
                        false, /*hasTrailingNewLine*/
                    ));
                } else {
                    let text = format!("... {} more ...", list.len() - display_index);
                    result.push(self.f.new_type_reference_node(
                        self.f.new_identifier(self.f.alloc_text(&text)),
                        None, /*typeArguments*/
                    ));
                }
                let type_node = self.type_to_type_node(c, Some(list[list.len() - 1]));
                if let Some(type_node) = type_node {
                    result.push(type_node);
                }
                break;
            }
            let ctx = self.ctx();
            ctx.approximate_length.set(ctx.approximate_length.get() + 2); // Account for whitespace + separator
            let type_node = self.type_to_type_node(c, Some(t));
            if let Some(type_node) = type_node {
                result.push(type_node);
                if let Some(seen_names) = seen_names.as_mut()
                    && is_identifier_type_reference(type_node)
                {
                    seen_names.add(
                        type_node.as_type_reference_node().type_name.text(),
                        SeenName { t, i: result.len() - 1 },
                    );
                }
            }
        }

        if let Some(seen_names) = seen_names {
            // To avoid printing types like `[Foo, Foo]` or `Bar & Bar` where
            // occurrences of the same name actually come from different
            // namespaces, go through the single-identifier type reference nodes
            // we just generated, and see if any names were generated more than
            // once while referring to different types. If so, regenerate the
            // type node for each entry by that name with the
            // `UseFullyQualifiedType` flag enabled.
            let mut restore_flags = self.save_restore_flags(c);
            let ctx = self.ctx();
            ctx.flags.set(ctx.flags.get() | Flags::UseFullyQualifiedType);
            for types in seen_names.values() {
                if !array_is_homogeneous(types, |a: SeenName, b: SeenName| {
                    types_are_same_reference(a.t, b.t)
                }) {
                    for seen in types {
                        // Go stores the result as is (possibly nil).
                        result[seen.i] = self.type_to_type_node(c, Some(seen.t)).unwrap();
                    }
                }
            }
            restore_flags(c);
        }

        Some(self.f.new_node_list(result))
    }

    // nodebuilderimpl.go:442
    pub(crate) fn serialize_type_name(
        &self,
        c: &mut Checker,
        node: P<Node>,
        is_type_of: bool,
        type_arguments: Option<P<NodeList>>,
    ) -> Option<P<Node>> {
        let mut meaning = SymbolFlags::Type;
        if is_type_of {
            meaning = SymbolFlags::Value;
        }
        let symbol = c.resolve_entity_name(node, meaning, true, false, Some(node))?;

        let mut resolved_symbol = symbol;
        if symbol.flags().intersects(SymbolFlags::Alias) {
            resolved_symbol = c.resolve_alias(symbol);
        }

        if c.is_symbol_accessible(
            Some(symbol),
            self.ctx().enclosing_declaration.get(),
            meaning,
            false,
        )
        .accessibility
            != SymbolAccessibility::Accessible
        {
            return None;
        }
        self.symbol_to_type_node(c, resolved_symbol, meaning, type_arguments)
    }
}

// nodebuilderimpl.go:463
fn is_identifier_type_reference(node: P<Node>) -> bool {
    is_type_reference_node(node) && is_identifier(node.as_type_reference_node().type_name)
}

// nodebuilderimpl.go:467
fn array_is_homogeneous<T: Clone>(array: &[T], mut comparer: impl FnMut(T, T) -> bool) -> bool {
    if array.len() < 2 {
        return true;
    }
    let first = &array[0];
    for element in array.iter().skip(1) {
        let target = element;
        if !comparer(first.clone(), target.clone()) {
            return false;
        }
    }
    true
}

// nodebuilderimpl.go:481
fn types_are_same_reference(a: P<Type>, b: P<Type>) -> bool {
    a == b
        || a.symbol().is_some() && a.symbol() == b.symbol()
        || a.alias().is_some() && a.alias() == b.alias()
}

impl NodeBuilderImpl {
    // nodebuilderimpl.go:485
    pub(crate) fn set_comment_range(
        &self,
        _c: &mut Checker,
        node: P<Node>,
        range_: Option<P<Node>>,
    ) {
        if let Some(range_) = range_
            && self.ctx().enclosing_file.get().is_some()
            && self.ctx().enclosing_file.get() == get_source_file_of_node(range_)
        {
            // Copy comments to node for declaration emit
            self.e.assign_comment_range(node, range_);
        }
    }

    // nodebuilderimpl.go:492
    fn type_node_is_equivalent_to_type(
        c: &mut Checker,
        annotated_declaration: Option<P<Node>>,
        t: P<Type>,
        type_from_type_node: P<Type>,
    ) -> bool {
        if type_from_type_node == t {
            return true;
        }
        let Some(annotated_declaration) = annotated_declaration else {
            return false;
        };
        // !!!
        // used to be hasEffectiveQuestionToken for JSDoc
        if is_optional_declaration(annotated_declaration) {
            return c.get_type_with_facts(t, TypeFacts::NEUndefined) == type_from_type_node;
        }
        false
    }

    // nodebuilderimpl.go:507
    pub(crate) fn can_reuse_existing_js_type_node(
        c: &mut Checker,
        existing: P<Node>,
        t: P<Type>,
    ) -> bool {
        c.get_intended_type_from_jsdoc_type_reference(existing).is_none() && Self::existing_type_node_is_not_reference_or_is_reference_with_compatible_type_argument_count(c, existing, t)
    }

    // nodebuilderimpl.go:511
    pub(crate) fn try_get_resolved_symbol_from_type_node(
        c: &mut Checker,
        node: P<Node>,
    ) -> Option<P<Symbol>> {
        node.parent()?;
        c.get_type_from_type_node(node);
        // call to ensure symbol is resolved
        let links = c.symbol_node_links.try_get(node)?;
        links.resolved_symbol.get()
    }

    // nodebuilderimpl.go:524
    fn existing_type_node_is_not_reference_or_is_reference_with_compatible_type_argument_count(
        c: &mut Checker,
        existing: P<Node>,
        t: P<Type>,
    ) -> bool {
        // In JS, you can say something like `Foo` and get a `Foo<any>` implicitly - we don't want to preserve that original `Foo` in these cases, though.
        if !t.object_flags().intersects(ObjectFlags::Reference) {
            return true;
        }
        if !is_type_reference_node(existing) {
            return true;
        }

        let Some(symbol) = Self::try_get_resolved_symbol_from_type_node(c, existing) else {
            return true;
        };

        // `type` is a reference type, and `existing` is a type reference node, but we still need to make sure they refer to the _same_ target type
        // before we go comparing their type argument counts.

        let existing_target = c.get_declared_type_of_symbol(symbol);
        let target = t.as_type_reference().target.get();
        if Some(existing_target) != target {
            return true;
        }
        let type_parameters = target.unwrap().as_interface_type().type_parameters();
        existing.type_arguments().len()
            >= checker::Checker::get_min_type_argument_count(type_parameters)
    }

    // nodebuilderimpl.go:548
    pub(crate) fn try_reuse_existing_non_parameter_type_node(
        &self,
        c: &mut Checker,
        existing: P<Node>,
        t: P<Type>,
        host: Option<P<Node>>,
        annotation_type: Option<P<Type>>,
    ) -> Option<P<Node>> {
        let mut host = host;
        if host.is_none() {
            host = self.ctx().enclosing_declaration.get();
        }
        let mut annotation_type = annotation_type;
        if annotation_type.is_none() {
            annotation_type = self.get_type_from_type_node(c, existing, true);
        }
        if let Some(annotation_type) = annotation_type
            && Self::type_node_is_equivalent_to_type(c, host, t, annotation_type)
            && Self::can_reuse_existing_js_type_node(c, existing, t)
        {
            let result = self.try_reuse_existing_node_helper(c, existing);
            if result.is_some() {
                return result;
            }
        }
        None
    }

    // Go passes the resolved `*StructuredType`; Rust passes the type itself (a payload has no back pointer to its
    // `Type` header, which carries `symbol` and is what Go's `t.AsType()` returns).
    // nodebuilderimpl.go:564
    pub(crate) fn get_resolved_type_without_abstract_construct_signatures(
        c: &mut Checker,
        t: P<Type>,
    ) -> P<Type> {
        let st = t.as_structured_type();
        if st.construct_signatures().is_empty() {
            return t;
        }
        if let Some(&existing) = c.object_types_without_abstract_construct_signatures.get(&t) {
            return existing;
        }
        let construct_signatures: Vec<P<Signature>> = st
            .construct_signatures()
            .iter()
            .copied()
            .filter(|signature| !signature.flags.get().intersects(SignatureFlags::Abstract))
            .collect();
        if construct_signatures.len() == st.construct_signatures().len() {
            c.object_types_without_abstract_construct_signatures.insert(t, t);
            return t;
        }
        let type_copy = c.new_anonymous_type(
            t.symbol(),
            st.members(),
            st.call_signatures(),
            &construct_signatures,
            st.index_infos(),
        );
        c.object_types_without_abstract_construct_signatures.insert(t, type_copy);
        c.object_types_without_abstract_construct_signatures.insert(type_copy, type_copy);
        type_copy
    }

    // nodebuilderimpl.go:584
    pub(crate) fn symbol_to_node(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
        meaning: SymbolFlags,
    ) -> P<Node> {
        if self.ctx().internal_flags.get().intersects(InternalFlags::WriteComputedProps) {
            if let Some(value_declaration) = symbol.value_declaration() {
                let name = get_name_of_declaration(value_declaration);
                if let Some(name) = name
                    && is_computed_property_name(name)
                {
                    return name;
                }
            }
            if c.value_symbol_links.has(symbol) {
                let name_type = c.value_symbol_links.get(symbol).name_type();
                if let Some(name_type) = name_type
                    && name_type
                        .flags()
                        .intersects(TypeFlags::EnumLiteral | TypeFlags::UniqueESSymbol)
                {
                    let old_enclosing = self.ctx().enclosing_declaration.get();
                    self.ctx()
                        .enclosing_declaration
                        .set(name_type.symbol().unwrap().value_declaration());
                    let result = self.f.new_computed_property_name(
                        self.symbol_to_expression_worker(c, name_type.symbol().unwrap(), meaning),
                    );
                    self.ctx().enclosing_declaration.set(old_enclosing);
                    return result;
                }
            }
        }
        self.symbol_to_expression(c, symbol, meaning)
    }

    // nodebuilderimpl.go:606
    pub(crate) fn symbol_to_name(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
        meaning: SymbolFlags,
        expects_identifier: bool,
    ) -> P<Node> {
        let chain = self.lookup_symbol_chain(c, symbol, meaning, false);
        let ctx = self.ctx();
        if expects_identifier
            && chain.len() != 1
            && !ctx.encountered_error.get()
            && ctx.flags.get().intersects(Flags::AllowQualifiedNameInPlaceOfIdentifier)
        {
            ctx.encountered_error.set(true);
        }
        self.create_entity_name_from_symbol_chain(c, &chain, chain.len() - 1)
    }

    // nodebuilderimpl.go:614
    fn create_entity_name_from_symbol_chain(
        &self,
        c: &mut Checker,
        chain: &[P<Symbol>],
        index: usize,
    ) -> P<Node> {
        // typeParameterNodes := b.lookupTypeParameterNodes(chain, index)
        let symbol = chain[index];

        if index == 0 {
            self.ctx().flags.set(self.ctx().flags.get() | Flags::InInitialEntityName);
        }
        let symbol_name = self.get_name_of_symbol_as_written(c, symbol);
        if index == 0 {
            self.ctx().flags.set(self.ctx().flags.get() ^ Flags::InInitialEntityName);
        }

        let identifier = self.new_identifier(c, &symbol_name, Some(symbol));
        self.e.add_emit_flags(identifier, EmitFlags::NoAsciiEscaping);
        // !!! TODO: smuggle type arguments out
        // if (typeParameterNodes) setIdentifierTypeArguments(identifier, factory.createNodeArray<TypeNode | TypeParameterDeclaration>(typeParameterNodes));
        // identifier.symbol = symbol;
        // expression = identifier;
        if index > 0 {
            return self.f.new_qualified_name(
                self.create_entity_name_from_symbol_chain(c, chain, index - 1),
                identifier,
            );
        }
        identifier
    }

    // TODO: Audit usages of symbolToEntityNameNode - they should probably all be symbolToName
    // nodebuilderimpl.go:642
    pub(crate) fn symbol_to_entity_name_node(&self, c: &mut Checker, symbol: P<Symbol>) -> P<Node> {
        let identifier = self.new_identifier(c, symbol.name(), Some(symbol));
        if let Some(parent) = symbol.parent() {
            return self
                .f
                .new_qualified_name(self.symbol_to_entity_name_node(c, parent), identifier);
        }
        identifier
    }

    // nodebuilderimpl.go:650
    pub(crate) fn symbol_to_type_node(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
        mask: SymbolFlags,
        type_arguments: Option<P<NodeList>>,
    ) -> Option<P<Node>> {
        let chain = self.lookup_symbol_chain(
            c,
            symbol,
            mask,
            !self.ctx().flags.get().intersects(Flags::UseAliasDefinedOutsideCurrentScope),
        ); // If we're using aliases outside the current scope, dont bother with the module
        if chain.is_empty() {
            return None; // TODO: shouldn't be possible, `lookupSymbolChain` should always at least return the input symbol and issue an error
        }
        let is_type_of = mask == SymbolFlags::Value;
        let root_is_module = chain[0]
            .declarations()
            .iter()
            .any(|&d| has_non_global_augmentation_external_module_symbol(d));
        if root_is_module {
            // module is root, must use `ImportTypeNode`
            let mut non_root_parts: Option<P<Node>> = None;
            if chain.len() > 1 {
                non_root_parts = Some(self.create_access_from_symbol_chain(
                    c,
                    &chain,
                    chain.len() - 1,
                    1,
                    type_arguments,
                ));
            }
            let mut type_parameter_nodes = type_arguments;
            if type_parameter_nodes.is_none() {
                type_parameter_nodes = self.lookup_type_parameter_nodes(c, &chain, 0);
            }
            let context_file = get_source_file_of_node(
                self.e.most_original(self.ctx().enclosing_declaration.get()),
            ); // TODO: Just use b.ctx.enclosingFile ? Or is the delayed lookup important for context moves?
            let target_file = get_source_file_of_module(chain[0]);
            let mut specifier_result = ModuleSpecifierResult::default();
            let mut import_mode_override = RESOLUTION_MODE_NONE;
            let module_resolution_kind = c.compiler_options.get_module_resolution_kind();
            if module_resolution_kind == ModuleResolutionKind::NODE16
                || module_resolution_kind == ModuleResolutionKind::NODE_NEXT
            {
                // An `import` type directed at an esm format file is only going to resolve in esm mode - set the esm mode assertion
                if let (Some(target_file), Some(context_file)) = (target_file, context_file)
                    && c.emit_module_format_of_file(target_file) == ModuleKind::ES_NEXT
                    && c.emit_module_format_of_file(target_file)
                        != c.emit_module_format_of_file(context_file)
                {
                    specifier_result =
                        self.get_specifier_for_module_symbol(c, chain[0], ModuleKind::ES_NEXT);
                    import_mode_override = ModuleKind::ES_NEXT;
                }
            }
            if specifier_result.specifier.is_empty() {
                specifier_result =
                    self.get_specifier_for_module_symbol(c, chain[0], RESOLUTION_MODE_NONE);
            }
            if !self.ctx().flags.get().intersects(Flags::AllowNodeModulesRelativePaths) /* && b.ch.compilerOptions.GetModuleResolutionKind() != core.ModuleResolutionKindClassic */ && specifier_result.specifier.contains("/node_modules/")
            {
                let old_specifier_result = specifier_result;

                let module_resolution_kind = c.compiler_options.get_module_resolution_kind();
                if module_resolution_kind == ModuleResolutionKind::NODE16
                    || module_resolution_kind == ModuleResolutionKind::NODE_NEXT
                {
                    // We might be able to write a portable import type using a mode override; try specifier generation again, but with a different mode set
                    let mut swapped_mode = ModuleKind::ES_NEXT;
                    if c.emit_module_format_of_file(context_file.unwrap()) == ModuleKind::ES_NEXT {
                        swapped_mode = ModuleKind::COMMON_JS;
                    }
                    specifier_result =
                        self.get_specifier_for_module_symbol(c, chain[0], swapped_mode);

                    if specifier_result.specifier.contains("/node_modules/") {
                        // Still unreachable :(
                        specifier_result = old_specifier_result;
                    } else {
                        import_mode_override = swapped_mode;
                    }
                }

                if import_mode_override == RESOLUTION_MODE_NONE {
                    // If ultimately we can only name the symbol with a reference that dives into a `node_modules` folder, we should error
                    // since declaration files with these kinds of references are liable to fail when published :(
                    self.ctx().encountered_error.set(true);
                    self.tracker().report_likely_unsafe_import_required_error(
                        old_specifier_result.specifier,
                        symbol.name(),
                    );
                }
            }

            let attributes = self.create_import_attributes_for_module_specifier(
                c,
                specifier_result,
                import_mode_override,
            );
            let lit = self
                .f
                .new_literal_type_node(self.new_string_literal(c, specifier_result.specifier));
            let ctx = self.ctx();
            ctx.approximate_length
                .set(ctx.approximate_length.get() + specifier_result.specifier.len() + 10); // specifier + import("")
            if non_root_parts.is_none() || is_entity_name(non_root_parts.unwrap()) {
                if non_root_parts.is_some() {
                    // !!! TODO: smuggle type arguments out
                    // const lastId = isIdentifier(nonRootParts) ? nonRootParts : nonRootParts.right;
                    // setIdentifierTypeArguments(lastId, /*typeArguments*/ undefined);
                }
                return Some(self.f.new_import_type_node(
                    is_type_of,
                    lit,
                    attributes,
                    non_root_parts,
                    type_parameter_nodes,
                ));
            }

            let split_node = get_topmost_indexed_access_type(non_root_parts.unwrap());
            let split = split_node.as_indexed_access_type_node();
            let qualifier = split.object_type.as_type_reference_node().type_name;
            return Some(self.f.new_indexed_access_type_node(
                self.f.new_import_type_node(
                    is_type_of,
                    lit,
                    attributes,
                    Some(qualifier),
                    type_parameter_nodes,
                ),
                split.index_type,
            ));
        }

        let entity_name =
            self.create_access_from_symbol_chain(c, &chain, chain.len() - 1, 0, type_arguments);
        if is_indexed_access_type_node(entity_name) {
            return Some(entity_name); // Indexed accesses can never be `typeof`
        }
        if is_entity_name(entity_name) {
            if is_type_of {
                return Some(self.f.new_type_query_node(entity_name, None));
            }
            return Some(self.f.new_type_reference_node(entity_name, type_arguments));
        }
        if is_type_of && is_expression_with_type_arguments(entity_name) {
            let expr = entity_name.as_expression_with_type_arguments();
            return Some(self.f.new_type_query_node(
                self.f.deep_clone_node(Some(expr.expression)).unwrap(),
                expr.type_arguments.get(),
            ));
        }
        Some(entity_name)
    }
}

// nodebuilderimpl.go:745
fn get_topmost_indexed_access_type(node: P<Node>) -> P<Node> {
    let object_type = node.as_indexed_access_type_node().object_type;
    if is_indexed_access_type_node(object_type) {
        return get_topmost_indexed_access_type(object_type);
    }
    node
}

impl NodeBuilderImpl {
    // nodebuilderimpl.go:752
    fn create_access_from_symbol_chain(
        &self,
        c: &mut Checker,
        chain: &[P<Symbol>],
        index: usize,
        stopper: usize,
        override_type_arguments: Option<P<NodeList>>,
    ) -> P<Node> {
        let mut type_parameter_nodes = override_type_arguments;
        if index != chain.len() - 1 {
            type_parameter_nodes = self.lookup_type_parameter_nodes(c, chain, index);
        }
        let symbol = chain[index];
        let mut parent: Option<P<Symbol>> = None;
        if index > 0 {
            parent = Some(chain[index - 1]);
        }

        let mut symbol_name = String::new();
        if index == 0 {
            let ctx = self.ctx();
            ctx.flags.set(ctx.flags.get() | Flags::InInitialEntityName);
            symbol_name = self.get_name_of_symbol_as_written(c, symbol);
            ctx.approximate_length.set(ctx.approximate_length.get() + symbol_name.len() + 1);
            ctx.flags.set(ctx.flags.get() ^ Flags::InInitialEntityName);
        } else {
            // lookup a ref to symbol within parent to handle export aliases
            if let Some(parent) = parent {
                let exports = c.get_exports_of_symbol(parent);
                if let Some(exports) = exports {
                    // avoid exhaustive iteration in the common case
                    let res = exports.lookup(symbol.name());
                    if symbol.name() != ast::INTERNAL_SYMBOL_NAME_EXPORT_EQUALS
                        && !is_late_bound_name(symbol.name())
                        && res.is_some()
                        && c.get_symbol_if_same_reference(res.unwrap(), symbol).is_some()
                    {
                        symbol_name = symbol.name().to_string();
                    } else {
                        let mut results: FxHashMap<P<Symbol>, &'static str> = FxHashMap::default();
                        results.extend(exports.entries().into_iter().filter_map(|(name, ex)| {
                            if c.get_symbol_if_same_reference(ex, symbol).is_some()
                                && !is_late_bound_name(name)
                                && name != ast::INTERNAL_SYMBOL_NAME_EXPORT_EQUALS
                            {
                                Some((ex, name))
                                // break // must collect all results and sort them - exports are randomly iterated
                            } else {
                                None
                            }
                        }));
                        let mut result_symbols: Vec<P<Symbol>> = results.keys().copied().collect();
                        if !result_symbols.is_empty() {
                            c.sort_symbols(&mut result_symbols);
                            symbol_name = results[&result_symbols[0]].to_string();
                        }
                    }
                }
            }
        }

        if symbol_name.is_empty() {
            let mut name: Option<P<Node>> = None;
            let declarations = symbol.declarations().to_vec();
            for d in declarations {
                name = get_name_of_declaration(d);
                if name.is_some() {
                    break;
                }
            }
            if let Some(name) = name
                && is_computed_property_name(name)
                && is_entity_name(name.expression().unwrap())
            {
                let lhs = self.create_access_from_symbol_chain(
                    c,
                    chain,
                    index - 1,
                    stopper,
                    override_type_arguments,
                );
                if is_entity_name(lhs) {
                    return self.f.new_indexed_access_type_node(
                        self.f.new_parenthesized_type_node(self.f.new_type_query_node(lhs, None)),
                        self.f.new_type_query_node(name.expression().unwrap(), None),
                    );
                }
                return lhs;
            }
            symbol_name = self.get_name_of_symbol_as_written(c, symbol);
        }
        let ctx = self.ctx();
        ctx.approximate_length.set(ctx.approximate_length.get() + symbol_name.len() + 1);

        if !self.ctx().flags.get().intersects(Flags::ForbidIndexedAccessSymbolReferences)
            && parent.is_some()
            && c.get_members_of_symbol(parent.unwrap()).is_some()
            && c.get_members_of_symbol(parent.unwrap()).unwrap().lookup(symbol.name()).is_some()
            && {
                let member = c
                    .get_members_of_symbol(parent.unwrap())
                    .unwrap()
                    .lookup(symbol.name())
                    .unwrap();
                c.get_symbol_if_same_reference(member, symbol).is_some()
            }
        {
            // Should use an indexed access
            let lhs = self.create_access_from_symbol_chain(
                c,
                chain,
                index - 1,
                stopper,
                override_type_arguments,
            );
            let lit = self.mark_symbol_name_literal(self.new_string_literal(c, &symbol_name));
            if is_indexed_access_type_node(lhs) {
                return self.f.new_indexed_access_type_node(lhs, self.f.new_literal_type_node(lit));
            }
            return self.f.new_indexed_access_type_node(
                self.f.new_type_reference_node(lhs, type_parameter_nodes),
                self.f.new_literal_type_node(lit),
            );
        }

        let identifier = self.new_identifier(c, &symbol_name, Some(symbol));
        self.e.add_emit_flags(identifier, EmitFlags::NoAsciiEscaping);

        if index > stopper {
            let lhs = self.create_access_from_symbol_chain(
                c,
                chain,
                index - 1,
                stopper,
                override_type_arguments,
            );
            if !self.ctx().flags.get().intersects(Flags::UseInstantiationExpressions)
                || is_entity_name(lhs)
                    && (type_parameter_nodes.is_none()
                        || type_parameter_nodes.unwrap().nodes().is_empty())
            {
                return self.f.new_qualified_name(lhs, identifier);
            }
            let access = self.create_access_expression(lhs);
            return self.create_expression_with_type_arguments(
                c,
                self.f.new_property_access_expression(access, None, identifier, NodeFlags::None),
                type_parameter_nodes,
            );
        }
        identifier
    }

    // nodebuilderimpl.go:848
    pub(crate) fn symbol_to_expression(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
        mask: SymbolFlags,
    ) -> P<Node> {
        self.tracker().track_symbol(c, symbol, self.ctx().enclosing_declaration.get(), mask);
        self.symbol_to_expression_worker(c, symbol, mask)
    }

    // nodebuilderimpl.go:853
    pub(crate) fn symbol_to_expression_worker(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
        mask: SymbolFlags,
    ) -> P<Node> {
        let chain = self.lookup_symbol_chain_worker(c, symbol, mask, false);
        self.create_expression_from_symbol_chain(c, &chain, chain.len() - 1)
    }

    // nodebuilderimpl.go:858
    fn create_expression_from_symbol_chain(
        &self,
        c: &mut Checker,
        chain: &[P<Symbol>],
        index: usize,
    ) -> P<Node> {
        let type_parameter_nodes =
            self.lookup_expression_chain_type_argument_nodes(c, chain, index);
        let symbol = chain[index];

        if index == 0 {
            self.ctx().flags.set(self.ctx().flags.get() | Flags::InInitialEntityName);
        }
        let mut symbol_name = self.get_name_of_symbol_as_written(c, symbol);
        if index == 0 {
            self.ctx().flags.set(self.ctx().flags.get() ^ Flags::InInitialEntityName);
        }

        if starts_with_single_or_double_quote(&symbol_name)
            && symbol
                .declarations()
                .iter()
                .any(|&d| has_non_global_augmentation_external_module_symbol(d))
        {
            let specifier_result =
                self.get_specifier_for_module_symbol(c, symbol, RESOLUTION_MODE_NONE);
            let ctx = self.ctx();
            ctx.approximate_length
                .set(ctx.approximate_length.get() + 2 + specifier_result.specifier.len());
            return self.new_string_literal(c, specifier_result.specifier);
        }

        if index == 0 || can_use_property_access(&symbol_name) {
            let identifier = self.new_identifier(c, &symbol_name, Some(symbol));
            self.e.add_emit_flags(identifier, EmitFlags::NoAsciiEscaping);
            let ctx = self.ctx();
            ctx.approximate_length.set(ctx.approximate_length.get() + 1 + symbol_name.len());
            if index > 0 {
                let result = self.f.new_property_access_expression(
                    self.create_expression_from_symbol_chain(c, chain, index - 1),
                    None,
                    identifier,
                    NodeFlags::None,
                );
                self.e.add_emit_flags(result, EmitFlags::NoIndentation);
                return self.create_expression_with_type_arguments(c, result, type_parameter_nodes);
            }
            return self.create_expression_with_type_arguments(c, identifier, type_parameter_nodes);
        }

        if starts_with_square_bracket(&symbol_name) {
            symbol_name = symbol_name[1..symbol_name.len() - 1].to_string();
        }

        let mut expression: Option<P<Node>> = None;
        if starts_with_single_or_double_quote(&symbol_name)
            && !symbol.flags().intersects(SymbolFlags::EnumMember)
        {
            let literal_text = tsrs_core::stringutil::unquote_string(&symbol_name);
            let ctx = self.ctx();
            ctx.approximate_length.set(ctx.approximate_length.get() + literal_text.len() + 2);
            expression = Some(self.new_string_literal_ex(
                c,
                &literal_text,
                symbol_name.as_bytes()[0] == b'\'',
            ));
        } else if jsnum::from_string(&symbol_name).string() == symbol_name {
            // TODO: the follwing in strada would assert if the number is negative, but no such assertion exists here
            // Moreover, what's even guaranteeing the name *isn't* -1 here anyway? Needs double-checking.
            let ctx = self.ctx();
            ctx.approximate_length.set(ctx.approximate_length.get() + symbol_name.len());
            expression =
                Some(self.f.new_numeric_literal(self.f.alloc_text(&symbol_name), TokenFlags::None));
        }
        if expression.is_none() {
            let ctx = self.ctx();
            ctx.approximate_length.set(ctx.approximate_length.get() + symbol_name.len());
            let e = self.new_identifier(c, &symbol_name, Some(symbol));
            self.e.add_emit_flags(e, EmitFlags::NoAsciiEscaping);
            expression = Some(e);
        }
        let ctx = self.ctx();
        ctx.approximate_length.set(ctx.approximate_length.get() + 2); // []
        let lhs = self.create_expression_from_symbol_chain(c, chain, index - 1);
        self.create_expression_with_type_arguments(
            c,
            self.f.new_element_access_expression(lhs, None, expression.unwrap(), NodeFlags::None),
            type_parameter_nodes,
        )
    }
}

// nodebuilderimpl.go:912
fn can_use_property_access(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    // TODO: in strada, this only used `isIdentifierStart` on the first character, while this checks the whole string for validity
    // - possible strada bug?
    if let Some(name) = name.strip_prefix('#') {
        return !name.is_empty()
            && tsrs_scanner::is_identifier_text(name, LanguageVariant::Standard);
    }
    tsrs_scanner::is_identifier_text(name, LanguageVariant::Standard)
}

// nodebuilderimpl.go:924
fn starts_with_single_or_double_quote(str: &str) -> bool {
    str.starts_with('\'') || str.starts_with('"')
}

// nodebuilderimpl.go:928
fn starts_with_square_bracket(str: &str) -> bool {
    str.starts_with('[')
}

// nodebuilderimpl.go:932
fn is_default_binding_context(location: P<Node>) -> bool {
    location.kind() == Kind::SourceFile || is_ambient_module(location)
}

impl NodeBuilderImpl {
    // nodebuilderimpl.go:936
    fn get_name_of_symbol_from_name_type(&self, c: &mut Checker, symbol: P<Symbol>) -> String {
        if c.value_symbol_links.has(symbol) {
            let Some(name_type) = c.value_symbol_links.get(symbol).name_type() else {
                return String::new();
            };
            if name_type.flags().intersects(TypeFlags::StringOrNumberLiteral) {
                let value = name_type.as_literal_type().value.get();
                let name = match value {
                    Some(LiteralValue::String(v)) => v.to_string(),
                    Some(LiteralValue::Number(v)) => v.string(),
                    _ => String::new(),
                };
                if !tsrs_scanner::is_identifier_text(&name, LanguageVariant::Standard)
                    && !is_numeric_literal_name(&name)
                {
                    return checker::Checker::value_to_string(value.unwrap());
                }
                if is_numeric_literal_name(&name) && name.starts_with('-') {
                    return format!("[{name}]");
                }
                return name;
            }
            if name_type.flags().intersects(TypeFlags::UniqueESSymbol) {
                let text = self.get_name_of_symbol_as_written(c, name_type.symbol().unwrap());
                return format!("[{text}]");
            }
        }
        String::new()
    }

    /**
     * Gets a human-readable name for a symbol.
     * Should *not* be used for the right-hand side of a `.` -- use `symbolName(symbol)` for that instead.
     *
     * Unlike `symbolName(symbol)`, this will include quotes if the name is from a string literal.
     * It will also use a representation of a number as written instead of a decimal form, e.g. `0o11` instead of `9`.
     */
    // nodebuilderimpl.go:973
    fn get_name_of_symbol_as_written(&self, c: &mut Checker, symbol: P<Symbol>) -> String {
        let mut symbol = symbol;
        let result =
            self.ctx().remapped_symbol_references.borrow().get(&get_symbol_id(symbol)).copied();
        if let Some(result) = result {
            symbol = result;
        }
        let ctx = self.ctx();
        if symbol.name() == ast::INTERNAL_SYMBOL_NAME_DEFAULT
            && !ctx.flags.get().intersects(Flags::UseAliasDefinedOutsideCurrentScope)
            // If it's not the first part of an entity name, it must print as `default`
            && (!ctx.flags.get().intersects(Flags::InInitialEntityName)
                // if the symbol is synthesized, it will only be referenced externally it must print as `default`
                || symbol.declarations().is_empty()
                // if not in the same binding context (source file, module declaration), it must print as `default`
                || (ctx.enclosing_declaration.get().is_some()
                    && find_ancestor(symbol.declarations()[0], is_default_binding_context) != find_ancestor(ctx.enclosing_declaration.get(), is_default_binding_context)))
        {
            return "default".to_string();
        }
        let declarations = symbol.declarations().to_vec();
        if !declarations.is_empty() {
            let name = first_non_nil(&declarations, |&d| get_name_of_declaration(d)); // Try using a declaration with a name, first
            if let Some(name) = name {
                // !!! TODO: JS Object.defineProperty declarations
                // if ast.IsCallExpression(declaration) && ast.IsBindableObjectDefinePropertyCall(declaration) {
                // 	return symbol.Name
                // }
                if is_computed_property_name(name)
                    && !symbol.check_flags().intersects(CheckFlags::Late)
                    && c.value_symbol_links.has(symbol)
                    && c.value_symbol_links.get(symbol).name_type().is_some()
                    && c.value_symbol_links
                        .get(symbol)
                        .name_type()
                        .unwrap()
                        .flags()
                        .intersects(TypeFlags::StringOrNumberLiteral)
                {
                    let result = self.get_name_of_symbol_from_name_type(c, symbol);
                    if !result.is_empty() {
                        return result;
                    }
                }
                return tsrs_scanner::declaration_name_to_string(Some(name));
            }
            let declaration = declarations[0]; // Declaration may be nameless, but we'll try anyway
            if let Some(parent) = declaration.parent()
                && parent.kind() == Kind::VariableDeclaration
            {
                return tsrs_scanner::declaration_name_to_string(Some(
                    parent.as_variable_declaration().name,
                ));
            }
            if is_class_expression(declaration)
                || is_function_expression(declaration)
                || is_arrow_function(declaration)
            {
                if self.ctx.get().is_some()
                    && !ctx.encountered_error.get()
                    && !ctx.flags.get().intersects(Flags::AllowAnonymousIdentifier)
                {
                    ctx.encountered_error.set(true);
                }
                match declaration.kind() {
                    Kind::ClassExpression => return "(Anonymous class)".to_string(),
                    Kind::FunctionExpression | Kind::ArrowFunction => {
                        return "(Anonymous function)".to_string();
                    }
                    _ => {}
                }
            }
        }
        let name = self.get_name_of_symbol_from_name_type(c, symbol);
        if !name.is_empty() {
            return name;
        }
        escape_internal_symbol_name(symbol.name())
    }

    // The full set of type parameters for a generic class or interface type consists of its outer type parameters plus
    // its locally declared type parameters.
    // nodebuilderimpl.go:1029
    pub(crate) fn get_type_parameters_of_class_or_interface(
        c: &mut Checker,
        symbol: P<Symbol>,
    ) -> Vec<P<Type>> {
        let mut result: Vec<P<Type>> = Vec::new();
        result.extend(c.get_outer_type_parameters_of_class_or_interface(symbol));
        result.extend(c.get_local_type_parameters_of_class_or_interface_or_type_alias(symbol));
        result
    }

    // nodebuilderimpl.go:1036
    fn lookup_type_parameter_nodes(
        &self,
        c: &mut Checker,
        chain: &[P<Symbol>],
        index: usize,
    ) -> Option<P<NodeList>> {
        assert!(index < chain.len());
        let symbol = chain[index];
        let symbol_id = get_symbol_id(symbol);
        if self.ctx().type_parameter_symbol_list.borrow().has(&symbol_id) {
            return None;
        }
        self.ctx().type_parameter_symbol_list.borrow_mut().add(symbol_id);

        if self.ctx().flags.get().intersects(Flags::WriteTypeParametersInQualifiedName)
            && index < chain.len() - 1
        {
            if let Some(type_argument_nodes) =
                self.lookup_instantiated_type_argument_nodes(c, chain, index)
            {
                return Some(type_argument_nodes);
            }
            let type_parameter_nodes =
                self.type_parameters_to_type_parameter_declarations(c, symbol);
            if !type_parameter_nodes.is_empty() {
                return Some(self.f.new_node_list(type_parameter_nodes));
            }
            return None;
        }

        None
    }

    // TODO: move `lookupSymbolChain` and co to `symbolaccessibility.go` (but getSpecifierForModuleSymbol uses much context which makes that hard?)
    // nodebuilderimpl.go:1061
    pub(crate) fn lookup_symbol_chain(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
        meaning: SymbolFlags,
        yield_module_symbol: bool,
    ) -> Vec<P<Symbol>> {
        self.tracker().track_symbol(c, symbol, self.ctx().enclosing_declaration.get(), meaning);
        self.lookup_symbol_chain_worker(c, symbol, meaning, yield_module_symbol)
    }

    // nodebuilderimpl.go:1066
    fn lookup_symbol_chain_worker(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
        meaning: SymbolFlags,
        yield_module_symbol: bool,
    ) -> Vec<P<Symbol>> {
        // Try to get qualified name if the symbol is not a type parameter and there is an enclosing declaration.
        let mut chain: Vec<P<Symbol>> = Vec::new();
        let is_type_parameter = symbol.flags().intersects(SymbolFlags::TypeParameter);
        let ctx = self.ctx();
        if !is_type_parameter
            && (ctx.enclosing_declaration.get().is_some()
                || ctx.flags.get().intersects(Flags::UseFullyQualifiedType))
            && !ctx.internal_flags.get().intersects(InternalFlags::DoNotIncludeSymbolChain)
        {
            let res = self.get_symbol_chain(
                c,
                symbol,
                meaning,
                true, /*endOfChain*/
                yield_module_symbol,
            );
            chain = res;
            assert_ne!(chain, [] as [tsrs_core::P<tsrs_ast::Symbol>; 0]);
        } else {
            chain.push(symbol);
        }
        chain
    }

    /** @param endOfChain Set to false for recursive calls; non-recursive calls should always output something. */
    // nodebuilderimpl.go:1087
    fn get_symbol_chain(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
        meaning: SymbolFlags,
        end_of_chain: bool,
        yield_module_symbol: bool,
    ) -> Vec<P<Symbol>> {
        let enclosing_declaration = self.ctx().enclosing_declaration.get();
        let mut accessible_symbol_chain = c.get_accessible_symbol_chain(
            symbol,
            enclosing_declaration,
            meaning,
            self.ctx().flags.get().intersects(Flags::UseOnlyExternalAliasing),
        );
        let mut qualifier_meaning = meaning;
        if accessible_symbol_chain.len() > 1 {
            qualifier_meaning = get_qualified_left_meaning(meaning);
        }
        if accessible_symbol_chain.is_empty()
            || c.needs_qualification(
                accessible_symbol_chain[0],
                enclosing_declaration,
                qualifier_meaning,
            )
        {
            // Go up and add our parent.
            let mut root = symbol;
            if !accessible_symbol_chain.is_empty() {
                root = accessible_symbol_chain[0];
            }
            let parents = c.get_containers_of_symbol(root, enclosing_declaration, meaning);
            if !parents.is_empty() {
                let mut parent_specifiers: Vec<SortedSymbolNamePair> = parents
                    .iter()
                    .map(|&symbol| {
                        if symbol
                            .declarations()
                            .iter()
                            .any(|&d| has_non_global_augmentation_external_module_symbol(d))
                        {
                            return SortedSymbolNamePair {
                                sym: symbol,
                                name: self
                                    .get_specifier_for_module_symbol(
                                        c,
                                        symbol,
                                        RESOLUTION_MODE_NONE,
                                    )
                                    .specifier
                                    .to_string(),
                            };
                        }
                        SortedSymbolNamePair { sym: symbol, name: String::new() }
                    })
                    .collect();
                parent_specifiers.sort_by(|a, b| Self::sort_by_best_name(c, a, b));
                for pair in &parent_specifiers {
                    let parent = pair.sym;
                    let parent_chain = self.get_symbol_chain(
                        c,
                        parent,
                        get_qualified_left_meaning(meaning),
                        false,
                        yield_module_symbol,
                    );
                    if !parent_chain.is_empty() {
                        if let Some(exports) = parent.exports() {
                            let exported = exports.lookup(ast::INTERNAL_SYMBOL_NAME_EXPORT_EQUALS);
                            if let Some(exported) = exported
                                && c.get_symbol_if_same_reference(exported, symbol).is_some()
                            {
                                // parentChain root _is_ symbol - symbol is a module export=, so it kinda looks like it's own parent
                                // No need to lookup an alias for the symbol in itself
                                accessible_symbol_chain = parent_chain;
                                break;
                            }
                        }
                        let mut next_syms = accessible_symbol_chain.clone();
                        if next_syms.is_empty() {
                            let mut fallback = c.get_alias_for_symbol_in_container(parent, symbol);
                            if fallback.is_none() {
                                fallback = Some(symbol);
                            }
                            next_syms.push(fallback.unwrap());
                        }
                        let mut chain = parent_chain;
                        chain.extend(next_syms);
                        accessible_symbol_chain = chain;
                        break;
                    }
                }
            }
        }
        if !accessible_symbol_chain.is_empty() {
            return accessible_symbol_chain;
        }
        if
        // If this is the last part of outputting the symbol, always output. The cases apply only to parent symbols.
        end_of_chain ||
            // If a parent symbol is an anonymous type, don't write it.
            !symbol.flags().intersects(SymbolFlags::TypeLiteral | SymbolFlags::ObjectLiteral)
        {
            // If a parent symbol is an external module, don't write it. (We prefer just `x` vs `"foo/bar".x`.)
            if !end_of_chain
                && !yield_module_symbol
                && symbol
                    .declarations()
                    .iter()
                    .any(|&d| has_non_global_augmentation_external_module_symbol(d))
            {
                return Vec::new();
            }
            return vec![symbol];
        }
        Vec::new()
    }

    // nodebuilderimpl.go:1153
    fn sort_by_best_name(
        c: &mut Checker,
        a: &SortedSymbolNamePair,
        b: &SortedSymbolNamePair,
    ) -> std::cmp::Ordering {
        let specifier_a = &a.name;
        let specifier_b = &b.name;
        if !specifier_a.is_empty() && !specifier_b.is_empty() {
            let is_b_relative = tsrs_core::tspath::path_is_relative(specifier_b);
            if tsrs_core::tspath::path_is_relative(specifier_a) == is_b_relative {
                // Both relative or both non-relative, sort by number of parts
                return modulespecifiers::count_path_components(specifier_a)
                    .cmp(&modulespecifiers::count_path_components(specifier_b));
            }
            if is_b_relative {
                // A is non-relative, B is relative: prefer A
                return std::cmp::Ordering::Less;
            }
            // A is relative, B is non-relative: prefer B
            return std::cmp::Ordering::Greater;
        }
        c.compare_symbols(Some(a.sym), Some(b.sym)) // must sort symbols for stable ordering
    }
}

// nodebuilderimpl.go:1172
fn can_have_module_specifier(node: Option<P<Node>>) -> bool {
    let Some(node) = node else {
        return false;
    };
    matches!(
        node.kind(),
        Kind::VariableDeclaration
            | Kind::BindingElement
            | Kind::ImportDeclaration
            | Kind::ExportDeclaration
            | Kind::ImportEqualsDeclaration
            | Kind::ImportClause
            | Kind::NamespaceExport
            | Kind::NamespaceImport
            | Kind::ExportSpecifier
            | Kind::ImportSpecifier
            | Kind::ImportType
    )
}

// nodebuilderimpl.go:1193
pub fn try_get_module_specifier_from_declaration(node: P<Node>) -> Option<P<Node>> {
    let res = try_get_module_specifier_from_declaration_worker(node);
    match res {
        Some(res) if is_string_literal(res) => Some(res),
        _ => None,
    }
}

// nodebuilderimpl.go:1201
fn try_get_module_specifier_from_declaration_worker(node: P<Node>) -> Option<P<Node>> {
    match node.kind() {
        Kind::VariableDeclaration | Kind::BindingElement => {
            let module_call = find_ancestor(node.initializer(), |node| {
                is_require_call(node, true /*requireStringLiteralLikeArgument*/)
                    || is_import_call(node)
            })?;
            Some(module_call.arguments()[0])
        }
        Kind::ImportDeclaration | Kind::ExportDeclaration | Kind::JSDocImportTag => {
            node.module_specifier()
        }
        Kind::ImportEqualsDeclaration => {
            let ref_ = node.as_import_equals_declaration().module_reference;
            if ref_.kind() != Kind::ExternalModuleReference {
                return None;
            }
            ref_.expression()
        }
        Kind::ImportClause | Kind::NamespaceExport => node.parent().unwrap().module_specifier(),
        Kind::NamespaceImport | Kind::ExportSpecifier => {
            node.parent().unwrap().parent().unwrap().module_specifier()
        }
        Kind::ImportSpecifier => {
            node.parent().unwrap().parent().unwrap().parent().unwrap().module_specifier()
        }
        Kind::ImportType => {
            if is_literal_import_type_node(node) {
                return Some(node.as_import_type_node().argument.as_literal_type_node().literal);
            }
            None
        }
        _ => panic!("Unhandled case in tryGetModuleSpecifierFromDeclarationWorker"),
    }
}

impl NodeBuilderImpl {
    // nodebuilderimpl.go:1249
    pub(crate) fn get_specifier_for_module_symbol(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
        override_import_mode: ModuleKind,
    ) -> ModuleSpecifierResult {
        let enclosing_declaration = self.e.most_original(self.ctx().enclosing_declaration.get());
        let mut original_module_specifier: Option<P<Node>> = None;
        if can_have_module_specifier(enclosing_declaration) {
            original_module_specifier =
                try_get_module_specifier_from_declaration(enclosing_declaration.unwrap());
        }
        let mut original_import_attributes_type: Option<P<Type>> = None;
        if let Some(original_module_specifier) = original_module_specifier {
            original_import_attributes_type =
                c.get_import_attributes_type_for_module_specifier(original_module_specifier);
        }

        let mut file = get_declaration_of_kind(symbol, Kind::SourceFile);
        if file.is_none() {
            let declarations = symbol.declarations().to_vec();
            let equivalent_symbol = first_non_nil(&declarations, |&d| {
                c.get_file_symbol_if_file_symbol_export_equals_container(d, symbol)
            });
            if let Some(equivalent_symbol) = equivalent_symbol {
                file = get_declaration_of_kind(equivalent_symbol, Kind::SourceFile);
            }
        }

        if file.is_none() {
            let declaration = symbol
                .declarations()
                .iter()
                .copied()
                .find(|&d| is_module_with_string_literal_name(d));
            if let Some(declaration) = declaration {
                let specifier = declaration.name().unwrap().text();
                if original_import_attributes_type.is_some()
                    && self.module_specifier_resolves_to_symbol(
                        c,
                        specifier,
                        original_import_attributes_type.unwrap(),
                        symbol,
                    )
                {
                    return ModuleSpecifierResult {
                        specifier,
                        import_attributes_type: original_import_attributes_type,
                    };
                }
                return ModuleSpecifierResult {
                    specifier,
                    import_attributes_type: Some(c.get_type_of_module_import_attributes(symbol)),
                };
            }
            if let Some(specifier) = try_get_ambient_module_name_from_symbol_name(symbol.name()) {
                return ModuleSpecifierResult { specifier, import_attributes_type: None };
            }
        }
        if self.ctx().enclosing_file.get().is_none() {
            if let Some(specifier) = try_get_ambient_module_name_from_symbol_name(symbol.name()) {
                return ModuleSpecifierResult { specifier, import_attributes_type: None };
            }
            return ModuleSpecifierResult {
                specifier: get_source_file_of_module(symbol).unwrap().get().file_name(),
                import_attributes_type: None,
            };
        }

        let context_file = self.ctx().enclosing_file.get().unwrap();
        let mut resolution_mode = override_import_mode;
        if resolution_mode == RESOLUTION_MODE_NONE
            && let Some(original_module_specifier) = original_module_specifier
        {
            resolution_mode =
                c.program.get_mode_for_usage_location(context_file, original_module_specifier);
        } else if resolution_mode == RESOLUTION_MODE_NONE {
            resolution_mode = c.program.get_default_resolution_mode_for_file(context_file);
        }
        let cache_key = tsrs_module::ModeAwareCacheKey {
            name: context_file.get().path().as_str(),
            mode: resolution_mode,
        };
        let links = self.symbol_links.get(symbol);
        if links.specifier_cache.is_nil() {
            links.specifier_cache.make();
        }
        if let Some(result) = links.specifier_cache.get(&cache_key) {
            return self.module_specifier_result_for_symbol(
                c,
                result,
                original_import_attributes_type,
                symbol,
            );
        }
        // For declaration bundles, we need to generate absolute paths relative to the common source dir for imports,
        // just like how the declaration emitter does for the ambient module declarations - we can easily accomplish this
        // using the `baseUrl` compiler option (which we would otherwise never use in declaration emit) and a non-relative
        // specifier preference
        let host = self.ctx().host;
        let specifier_compiler_options = c.compiler_options;
        let specifier_pref = modulespecifiers::ImportModuleSpecifierPreference::ProjectRelative;
        let mut ending_pref = modulespecifiers::ImportModuleSpecifierEndingPreference::None;
        if resolution_mode == RESOLUTION_MODE_ESM {
            ending_pref = modulespecifiers::ImportModuleSpecifierEndingPreference::Js;
        }
        let module_specifiers_result = modulespecifiers::get_module_specifiers(
            symbol,
            c,
            &specifier_compiler_options,
            context_file,
            host,
            &modulespecifiers::UserPreferences {
                import_module_specifier_preference: specifier_pref,
                import_module_specifier_ending: ending_pref,
                ..Default::default()
            },
            modulespecifiers::ModuleSpecifierOptions { override_import_mode },
            false, /*forAutoImports*/
        );
        assert_ne!(module_specifiers_result.specifiers, [] as [std::string::String; 0]);
        let mut result = ModuleSpecifierResult {
            specifier: self.f.alloc_text(&module_specifiers_result.specifiers[0]),
            import_attributes_type: None,
        };
        if let Some(ambient_module_symbol) = module_specifiers_result.ambient_module_symbol {
            result.import_attributes_type =
                Some(c.get_type_of_module_import_attributes(ambient_module_symbol));
        }
        links.specifier_cache.set(cache_key, result);
        self.module_specifier_result_for_symbol(c, result, original_import_attributes_type, symbol)
    }

    // nodebuilderimpl.go:1340
    fn module_specifier_result_for_symbol(
        &self,
        c: &mut Checker,
        result: ModuleSpecifierResult,
        import_attributes_type: Option<P<Type>>,
        symbol: P<Symbol>,
    ) -> ModuleSpecifierResult {
        let mut result = result;
        if let Some(import_attributes_type) = import_attributes_type
            && self.module_specifier_resolves_to_symbol(
                c,
                result.specifier,
                import_attributes_type,
                symbol,
            )
        {
            result.import_attributes_type = Some(import_attributes_type);
        }
        if let Some(specifier) = self.rewrite_content_mapped_module_specifier(c, result.specifier) {
            result.specifier = self.f.alloc_text(&specifier);
        }
        result
    }

    pub(crate) fn rewrite_content_mapped_module_specifier(
        &self,
        c: &Checker,
        specifier: &str,
    ) -> Option<String> {
        let ctx = self.ctx();
        if !ctx.internal_flags.get().intersects(InternalFlags::RewriteModuleSpecifiers)
            || !c.compiler_options.rewrite_relative_import_extensions.is_true()
        {
            return None;
        }
        let rewrites = ctx.host.content_mapper_extension_rewrites();
        let case_sensitive = ctx.host.use_case_sensitive_file_names();
        if tspath::should_rewrite_extension(specifier, &rewrites, case_sensitive) {
            let (rewritten, _) = tspath::rewrite_extension(specifier, &rewrites, case_sensitive);
            return Some(rewritten);
        }
        None
    }

    // nodebuilderimpl.go:1347
    fn module_specifier_resolves_to_symbol(
        &self,
        c: &mut Checker,
        specifier: &str,
        import_attributes_type: P<Type>,
        symbol: P<Symbol>,
    ) -> bool {
        let mut location = self.ctx().enclosing_declaration.get();
        if location.is_none()
            && let Some(enclosing_file) = self.ctx().enclosing_file.get()
        {
            location = Some(enclosing_file.as_node());
        }
        if location.is_none() {
            return false;
        }
        let resolved = c.resolve_external_module(
            location,
            specifier,
            None,
            None,
            false, /*isForAugmentation*/
            Some(import_attributes_type),
        );
        match resolved {
            Some(resolved) => c.get_merged_symbol(resolved) == c.get_merged_symbol(symbol),
            None => false,
        }
    }

    // nodebuilderimpl.go:1359
    fn create_import_attributes_for_module_specifier(
        &self,
        c: &mut Checker,
        result: ModuleSpecifierResult,
        import_mode_override: ModuleKind,
    ) -> Option<P<Node>> {
        let is_empty_attributes_type = result.import_attributes_type.is_none()
            || c.is_empty_object_type(result.import_attributes_type.unwrap());
        if is_empty_attributes_type && import_mode_override == RESOLUTION_MODE_NONE {
            return None;
        }

        let mut properties: Vec<P<Symbol>> = Vec::new();
        if !is_empty_attributes_type {
            properties = c.get_properties_of_type(result.import_attributes_type.unwrap()).to_vec();
        }
        properties.sort_by(|a, b| a.name().cmp(b.name()));
        let mut attributes: Vec<P<Node>> = Vec::with_capacity(properties.len() + 1);
        if import_mode_override != RESOLUTION_MODE_NONE {
            let resolution_mode =
                if import_mode_override == RESOLUTION_MODE_ESM { "import" } else { "require" };
            attributes.push(self.f.new_import_attribute(
                Some(self.new_string_literal(c, "resolution-mode")),
                self.new_string_literal(c, resolution_mode),
            ));
            let ctx = self.ctx();
            ctx.approximate_length.set(
                ctx.approximate_length.get() + "resolution-mode".len() + resolution_mode.len() + 6,
            ); // `"resolution-mode": "value"`
        }
        for property in properties {
            let name = property.name();
            let property_type = c.get_type_of_symbol(property);
            if !property_type.flags().intersects(TypeFlags::StringLiteral) {
                continue;
            }
            let value = get_string_literal_value(property_type);
            let name_node = if tsrs_scanner::is_identifier_text(name, LanguageVariant::Standard) {
                self.f.new_identifier(name)
            } else {
                let n = self.new_string_literal(c, name);
                let ctx = self.ctx();
                ctx.approximate_length.set(ctx.approximate_length.get() + 2);
                n
            };
            attributes.push(
                self.f.new_import_attribute(Some(name_node), self.new_string_literal(c, value)),
            );
            let ctx = self.ctx();
            ctx.approximate_length.set(ctx.approximate_length.get() + name.len() + value.len() + 4); // `name: "value"`
        }
        if attributes.is_empty() {
            return None;
        }
        let ctx = self.ctx();
        ctx.approximate_length.set(ctx.approximate_length.get() + 16 + 2 * (attributes.len() - 1)); // `, { with: { , ... , } }`
        Some(self.f.new_import_attributes(
            Kind::WithKeyword,
            self.f.new_node_list(attributes),
            false,
        ))
    }

    // nodebuilderimpl.go:1401
    pub(crate) fn type_parameter_to_declaration_with_constraint(
        &self,
        c: &mut Checker,
        type_parameter: P<Type>,
        constraint_node: Option<P<Node>>,
    ) -> P<Node> {
        let mut restore_flags = self.save_restore_flags(c);
        let ctx = self.ctx();
        ctx.flags.set(ctx.flags.get() & !Flags::WriteTypeParametersInQualifiedName); // Avoids potential infinite loop when building for a claimspace with a generic
        let modifier_flags = checker::Checker::get_type_parameter_modifiers(type_parameter);
        let modifiers =
            create_modifiers_from_modifier_flags(modifier_flags, |k| self.f.new_modifier(k));
        let mut modifiers_list: Option<P<ModifierList>> = None;
        if !modifiers.is_empty() {
            modifiers_list = Some(self.f.new_modifier_list(modifiers));
        }
        let name = self.type_parameter_to_name(c, type_parameter);
        let default_parameter = c.get_default_from_type_parameter(type_parameter);
        let mut default_parameter_declaration_node: Option<P<Node>> = None;
        if let Some(default_parameter) = default_parameter {
            default_parameter_declaration_node = self.type_to_type_node(c, Some(default_parameter));
        }
        restore_flags(c);
        self.f.new_type_parameter_declaration(
            modifiers_list,
            name,
            constraint_node,
            None, // expression
            default_parameter_declaration_node,
        )
    }

    /**
     * Unlike the utilities `setTextRange`, this checks if the `location` we're trying to set on `range` is within the
     * same file as the active context. If not, the range is not applied. This prevents us from copying ranges across files,
     * which will confuse the node printer (as it assumes all node ranges are within the current file).
     * Additionally, if `range` _isn't synthetic_, or isn't in the current file, it will _copy_ it to _remove_ its' position
     * information.
     *
     * It also calls `setOriginalNode` to setup a `.original` pointer, since you basically *always* want these in the node builder.
     */
    // nodebuilderimpl.go:1434
    pub(crate) fn set_text_range(
        &self,
        _c: &mut Checker,
        range_: P<Node>,
        location: Option<P<Node>>,
    ) -> P<Node> {
        let mut range_ = range_;
        let enclosing_file = self.ctx().enclosing_file.get();
        if !node_is_synthesized(range_)
            || !range_.flags().intersects(NodeFlags::Synthesized)
            || enclosing_file.is_none()
            || enclosing_file != get_source_file_of_node(self.e.most_original(Some(range_)))
        {
            let original = range_;
            range_ = range_.clone_node(&self.f); // if `range` is synthesized or originates in another file, copy it so it definitely has synthetic positions
            range_.set_loc(TextRange::synthetic());
            let symbol = self.id_to_symbol.borrow().get(&original).copied();
            if let Some(symbol) = symbol {
                self.id_to_symbol.borrow_mut().insert(range_, symbol);
            }
        }
        let Some(location) = location else {
            return range_;
        };
        if range_ == location {
            return range_;
        }
        // Don't overwrite the original node if `range` has an `original` node that points either directly or indirectly to `location`
        let mut original = self.e.original(range_);
        while original.is_some() && original != Some(location) {
            original = self.e.original(original.unwrap());
        }
        if original.is_none() {
            self.e.set_original_ex(range_, location, true);
        }

        // only set positions if range comes from the same file since copying text across files isn't supported by the emitter
        let enclosing_file = self.ctx().enclosing_file.get();
        if enclosing_file.is_some()
            && enclosing_file == get_source_file_of_node(self.e.most_original(Some(location)))
        {
            range_.set_loc(location.loc());
            return range_;
        }
        range_.set_loc(TextRange::synthetic());
        range_
    }

    // nodebuilderimpl.go:1468
    fn type_parameter_shadows_other_type_parameter_in_scope(
        &self,
        c: &mut Checker,
        name: &str,
        type_parameter: P<Type>,
    ) -> bool {
        let result = c.resolve_name(
            self.ctx().enclosing_declaration.get(),
            name,
            SymbolFlags::Type,
            None,
            false,
            false,
        );
        if let Some(result) = result
            && result.flags().intersects(SymbolFlags::TypeParameter)
        {
            return Some(result) != type_parameter.symbol();
        }
        false
    }

    // nodebuilderimpl.go:1476
    pub(crate) fn type_parameter_to_name(
        &self,
        c: &mut Checker,
        type_parameter: P<Type>,
    ) -> P<Node> {
        if self.ctx().flags.get().intersects(Flags::GenerateNamesForShadowedTypeParams) {
            let cached = self.ctx().type_parameter_names.borrow().get(&type_parameter.id).copied();
            if let Some(cached) = cached {
                return cached;
            }
        }
        let mut result = self.symbol_to_name(
            c,
            type_parameter.symbol().unwrap(),
            SymbolFlags::Type,
            true, /*expectsIdentifier*/
        );
        if !is_identifier(result) {
            return self.f.new_identifier("(Missing type parameter)");
        }
        if let Some(symbol) = type_parameter.symbol() {
            let decl = symbol.declarations().first().copied();
            if let Some(decl) = decl
                && is_type_parameter_declaration(decl)
            {
                result = self.set_text_range(c, result, decl.name());
            }
        }
        if self.ctx().flags.get().intersects(Flags::GenerateNamesForShadowedTypeParams) {
            let raw_text = result.text();
            let mut i = self
                .ctx()
                .type_parameter_names_by_text_next_name_count
                .borrow()
                .get(&raw_text.to_string())
                .copied()
                .unwrap_or(0);
            let mut text = raw_text.to_string();

            loop {
                if !self.ctx().type_parameter_names_by_text.borrow().has(&text)
                    && !self.type_parameter_shadows_other_type_parameter_in_scope(
                        c,
                        &text,
                        type_parameter,
                    )
                {
                    break;
                }
                i += 1;
                text = format!("{raw_text}_{i}");
            }

            if text != raw_text {
                // !!! TODO: smuggle type arguments out
                // const typeArguments = getIdentifierTypeArguments(result);
                result = self.new_identifier(c, &text, type_parameter.symbol());
                // setIdentifierTypeArguments(result, typeArguments);
            }

            // avoiding iterations of the above loop turns out to be worth it when `i` starts to get large, so we cache the max
            // `i` we've used thus far, to save work later
            let ctx = self.ctx();
            ctx.type_parameter_names_by_text_next_name_count
                .borrow_mut()
                .set(raw_text.to_string(), i);
            ctx.type_parameter_names.borrow_mut().set(type_parameter.id, result);
            ctx.type_parameter_names_by_text.borrow_mut().add(text);
        }

        result
    }

    // nodebuilderimpl.go:1522
    fn is_mapped_type_homomorphic(c: &mut Checker, mapped: P<Type>) -> bool {
        c.get_homomorphic_type_variable(mapped).is_some()
    }

    // Go passes the `*MappedType`; Rust passes the type itself (the payload has no back pointer for `mapped.AsType()`).
    // nodebuilderimpl.go:1526
    fn is_homomorphic_mapped_type_with_non_homomorphic_instantiation(
        c: &mut Checker,
        mapped: P<Type>,
    ) -> bool {
        let target = mapped.as_mapped_type().target.get();
        target.is_some()
            && !Self::is_mapped_type_homomorphic(c, mapped)
            && Self::is_mapped_type_homomorphic(c, target.unwrap())
    }

    // nodebuilderimpl.go:1530
    pub(crate) fn create_mapped_type_node_from_type(&self, c: &mut Checker, t: P<Type>) -> P<Node> {
        assert!(t.flags().intersects(TypeFlags::Object));
        let mapped = t.as_mapped_type();
        let declaration = mapped.declaration.get().unwrap();
        let decl = declaration.as_mapped_type_node();
        let mut readonly_token: Option<P<Node>> = None;
        if let Some(token) = decl.readonly_token {
            readonly_token = Some(self.f.new_token(token.kind()));
        }
        let mut question_token: Option<P<Node>> = None;
        if let Some(token) = decl.question_token {
            question_token = Some(self.f.new_token(token.kind()));
        }

        let mut new_type_variable: Option<P<Node>> = None;
        let mut template_type = c.get_template_type_from_mapped_type(t);
        let type_parameter = c.get_type_parameter_from_mapped_type(t);

        // If the mapped type isn't `keyof` constraint-declared, _but_ still has modifiers preserved, and its naive instantiation won't preserve modifiers because its constraint isn't `keyof` constrained, we have work to do
        let needs_modifier_preserving_wrapper =
            !(checker::Checker::is_mapped_type_with_keyof_constraint_declaration(t)
                || c.get_modifiers_type_from_mapped_type(t).flags().intersects(TypeFlags::Unknown)
                || !self.ctx().flags.get().intersects(Flags::GenerateNamesForShadowedTypeParams)
                || c.get_constraint_type_from_mapped_type(t)
                    .flags()
                    .intersects(TypeFlags::TypeParameter)
                    && {
                        let constraint_type = c.get_constraint_type_from_mapped_type(t);
                        c.get_constraint_of_type_parameter(constraint_type)
                            .unwrap()
                            .flags()
                            .intersects(TypeFlags::Index)
                    });

        let appropriate_constraint_type_node: Option<P<Node>> =
            if checker::Checker::is_mapped_type_with_keyof_constraint_declaration(t) {
                // We have a { [P in keyof T]: X }
                // We do this to ensure we retain the toplevel keyof-ness of the type which may be lost due to keyof distribution during `getConstraintTypeFromMappedType`
                if self.ctx().flags.get().intersects(Flags::GenerateNamesForShadowedTypeParams)
                    && Self::is_homomorphic_mapped_type_with_non_homomorphic_instantiation(c, t)
                {
                    let new_symbol = c.new_symbol(SymbolFlags::TypeParameter, "T");
                    let new_constraint_param = c.new_type_parameter(Some(new_symbol));
                    let name = self.type_parameter_to_name(c, new_constraint_param);
                    let target = t.target().unwrap();
                    new_type_variable = Some(self.f.new_type_reference_node(name, None));
                    let target_template = c.get_template_type_from_mapped_type(target);
                    let target_type_parameter = c.get_type_parameter_from_mapped_type(target);
                    let target_modifiers = c.get_modifiers_type_from_mapped_type(target);
                    template_type = c.instantiate_type(
                        target_template,
                        Some(new_type_mapper(
                            alloc_slice(&[target_type_parameter, target_modifiers]),
                            alloc_slice(&[type_parameter, new_constraint_param]),
                        )),
                    );
                }
                let mut index_target = new_type_variable;
                if index_target.is_none() {
                    let modifiers_type = c.get_modifiers_type_from_mapped_type(t);
                    index_target = self.type_to_type_node(c, Some(modifiers_type));
                }
                Some(self.f.new_type_operator_node(Kind::KeyOfKeyword, index_target.unwrap()))
            } else if needs_modifier_preserving_wrapper {
                // So, step 1: new type variable
                let new_symbol = c.new_symbol(SymbolFlags::TypeParameter, "T");
                let new_param = c.new_type_parameter(Some(new_symbol));
                let name = self.type_parameter_to_name(c, new_param);
                new_type_variable = Some(self.f.new_type_reference_node(name, None));
                // step 2: make that new type variable itself the constraint node, making the mapped type `{[K in T_1]: Template}`
                new_type_variable
            } else {
                let constraint_type = c.get_constraint_type_from_mapped_type(t);
                self.type_to_type_node(c, Some(constraint_type))
            };

        // nameType and templateType nodes have to be in the new scope
        let scope_type_parameter = c.get_type_parameter_from_mapped_type(t);
        let mut cleanup =
            self.enter_new_scope(c, Some(declaration), &[], &[scope_type_parameter], None, None);
        let type_parameter_declaration_node = self.type_parameter_to_declaration_with_constraint(
            c,
            type_parameter,
            appropriate_constraint_type_node,
        );
        let mut name_type_node: Option<P<Node>> = None;
        if decl.name_type.is_some() {
            let name_type = c.get_name_type_from_mapped_type(t);
            name_type_node = self.type_to_type_node(c, name_type);
        }
        let without_missing = c.remove_missing_type(
            template_type,
            get_mapped_type_modifiers(t).intersects(MappedTypeModifiers::IncludeOptional),
        );
        let template_type_node = self.type_to_type_node(c, Some(without_missing));
        cleanup(c);
        let result = self.f.new_mapped_type_node(
            readonly_token,
            type_parameter_declaration_node,
            name_type_node,
            question_token,
            template_type_node,
            None,
        );
        let ctx = self.ctx();
        ctx.approximate_length.set(ctx.approximate_length.get() + 10);
        self.e.add_emit_flags(result, EmitFlags::SingleLine);

        if self.ctx().flags.get().intersects(Flags::GenerateNamesForShadowedTypeParams)
            && Self::is_homomorphic_mapped_type_with_non_homomorphic_instantiation(c, t)
        {
            // homomorphic mapped type with a non-homomorphic naive inlining
            // wrap it with a conditional like `SomeModifiersType extends infer U ? {..the mapped type...} : never` to ensure the resulting
            // type stays homomorphic

            let constraint_node = decl
                .type_parameter
                .as_type_parameter_declaration()
                .constraint
                .unwrap()
                .type_node()
                .unwrap();
            let mut raw_constraint_type_from_declaration =
                self.get_type_from_type_node(c, constraint_node, false);
            if let Some(raw) = raw_constraint_type_from_declaration {
                raw_constraint_type_from_declaration = c.get_constraint_of_type_parameter(raw);
            }
            if raw_constraint_type_from_declaration.is_none() {
                raw_constraint_type_from_declaration = Some(c.unknown_type);
            }
            let original_constraint = c.instantiate_type(
                raw_constraint_type_from_declaration.unwrap(),
                mapped.mapper.get(),
            );

            let mut original_constraint_node: Option<P<Node>> = None;
            if !original_constraint.flags().intersects(TypeFlags::Unknown) {
                original_constraint_node = self.type_to_type_node(c, Some(original_constraint));
            }

            let modifiers_type = c.get_modifiers_type_from_mapped_type(t);
            let check_type = self.type_to_type_node(c, Some(modifiers_type)).unwrap();
            return self.f.new_conditional_type_node(
                check_type,
                self.f.new_infer_type_node(
                    self.f.new_type_parameter_declaration(
                        None,
                        new_type_variable
                            .unwrap()
                            .as_type_reference_node()
                            .type_name
                            .clone_node(&self.f),
                        original_constraint_node,
                        None,
                        None,
                    ),
                ),
                result,
                self.f.new_keyword_type_node(Kind::NeverKeyword),
            );
        } else if needs_modifier_preserving_wrapper {
            // and step 3: once the mapped type is reconstructed, create a `ConstraintType extends infer T_1 extends keyof ModifiersType ? {[K in T_1]: Template} : never`
            // subtly different from the `keyof` constraint case, by including the `keyof` constraint on the `infer` type parameter, it doesn't rely on the constraint type being itself
            // constrained to a `keyof` type to preserve its modifier-preserving behavior. This is all basically because we preserve modifiers for a wider set of mapped types than
            // just homomorphic ones.
            let constraint_type = c.get_constraint_type_from_mapped_type(t);
            let check_type = self.type_to_type_node(c, Some(constraint_type)).unwrap();
            let name =
                new_type_variable.unwrap().as_type_reference_node().type_name.clone_node(&self.f);
            let modifiers_type = c.get_modifiers_type_from_mapped_type(t);
            let modifiers_type_node = self.type_to_type_node(c, Some(modifiers_type)).unwrap();
            return self.f.new_conditional_type_node(
                check_type,
                self.f.new_infer_type_node(self.f.new_type_parameter_declaration(
                    None,
                    name,
                    Some(self.f.new_type_operator_node(Kind::KeyOfKeyword, modifiers_type_node)),
                    None,
                    None,
                )),
                result,
                self.f.new_keyword_type_node(Kind::NeverKeyword),
            );
        }

        result
    }

    // nodebuilderimpl.go:1646
    pub(crate) fn type_predicate_to_type_predicate_node(
        &self,
        c: &mut Checker,
        predicate: P<TypePredicate>,
    ) -> P<Node> {
        let kind = predicate.kind.get();
        let mut asserts_modifier: Option<P<Node>> = None;
        if kind == TypePredicateKind::AssertsIdentifier || kind == TypePredicateKind::AssertsThis {
            asserts_modifier = Some(self.f.new_token(Kind::AssertsKeyword));
        }
        let parameter_name: P<Node>;
        if kind == TypePredicateKind::Identifier || kind == TypePredicateKind::AssertsIdentifier {
            parameter_name = self.f.new_identifier(predicate.parameter_name.get());
            self.e.add_emit_flags(parameter_name, EmitFlags::NoAsciiEscaping);
        } else {
            parameter_name = self.f.new_this_type_node();
        }
        let mut type_node: Option<P<Node>> = None;
        if let Some(t) = predicate.t.get() {
            type_node = self.type_to_type_node(c, Some(t));
        }
        self.f.new_type_predicate_node(asserts_modifier, parameter_name, type_node)
    }

    // nodebuilderimpl.go:1669
    fn type_to_type_node_helper_with_possible_reusable_type_node(
        &self,
        c: &mut Checker,
        t: Option<P<Type>>,
        type_node: Option<P<Node>>,
    ) -> Option<P<Node>> {
        let Some(t) = t else {
            return Some(self.f.new_keyword_type_node(Kind::AnyKeyword));
        };
        if !self.is_actively_expanding(c)
            && let Some(type_node) = type_node
            && self.get_type_from_type_node(c, type_node, false) == Some(t)
        {
            let reused = self.try_reuse_existing_node_helper(c, type_node);
            if reused.is_some() {
                self.check_type_expandability(c, Some(t));
                return reused;
            }
        }
        self.type_to_type_node(c, Some(t))
    }

    // nodebuilderimpl.go:1683
    pub(crate) fn type_parameter_to_declaration(
        &self,
        c: &mut Checker,
        parameter: P<Type>,
    ) -> P<Node> {
        let constraint = c.get_constraint_of_type_parameter(parameter);
        let mut constraint_node: Option<P<Node>> = None;
        if let Some(constraint) = constraint {
            let constraint_declaration = checker::Checker::get_constraint_declaration(parameter);
            constraint_node = self.type_to_type_node_helper_with_possible_reusable_type_node(
                c,
                Some(constraint),
                constraint_declaration,
            );
        }
        self.type_parameter_to_declaration_with_constraint(c, parameter, constraint_node)
    }

    // nodebuilderimpl.go:1692
    pub(crate) fn symbol_to_type_parameter_declarations(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
    ) -> Vec<P<Node>> {
        self.type_parameters_to_type_parameter_declarations(c, symbol)
    }

    // nodebuilderimpl.go:1696
    pub(crate) fn type_parameters_to_type_parameter_declarations(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
    ) -> Vec<P<Node>> {
        let target_symbol = c.get_target_symbol(symbol).unwrap();
        if target_symbol
            .flags()
            .intersects(SymbolFlags::Class | SymbolFlags::Interface | SymbolFlags::Alias)
        {
            let mut results: Vec<P<Node>> = Vec::new();
            let params = c.get_local_type_parameters_of_class_or_interface_or_type_alias(symbol);
            results.extend(
                params.into_iter().map(|param| self.type_parameter_to_declaration(c, param)),
            );
            return results;
        } else if target_symbol.flags().intersects(SymbolFlags::Function) {
            let mut results: Vec<P<Node>> = Vec::new();
            results.extend(
                c.get_type_parameters_from_declaration(symbol.value_declaration().unwrap())
                    .into_iter()
                    .map(|param| self.type_parameter_to_declaration(c, param)),
            );
            return results;
        }
        Vec::new()
    }
}

// nodebuilderimpl.go:1715
fn get_effective_parameter_declaration(symbol: P<Symbol>) -> Option<P<Node>> {
    let parameter_declaration = get_declaration_of_kind(symbol, Kind::Parameter);
    if parameter_declaration.is_some() {
        return parameter_declaration;
    }
    if !symbol.flags().intersects(SymbolFlags::Transient) {
        return get_declaration_of_kind(symbol, Kind::JSDocParameterTag);
    }
    None
}

impl NodeBuilderImpl {
    // nodebuilderimpl.go:1726
    pub(crate) fn symbol_to_parameter_declaration(
        &self,
        c: &mut Checker,
        parameter_symbol: P<Symbol>,
        preserve_modifier_flags: bool,
    ) -> P<Node> {
        let parameter_declaration = get_effective_parameter_declaration(parameter_symbol);

        let parameter_type = c.get_type_of_symbol(parameter_symbol);
        let parameter_type_node = self.serialize_type_for_declaration(
            c,
            parameter_declaration,
            Some(parameter_type),
            Some(parameter_symbol),
            true,
        );
        let mut modifiers: Option<P<ModifierList>> = None;
        if !self.ctx().flags.get().intersects(Flags::OmitParameterModifiers)
            && preserve_modifier_flags
            && let Some(parameter_declaration) = parameter_declaration
            && can_have_modifiers(parameter_declaration)
        {
            let originals: Vec<P<Node>> = parameter_declaration
                .modifier_nodes()
                .iter()
                .copied()
                .filter(|&n| is_modifier(n))
                .collect();
            let clones: Vec<P<Node>> =
                originals.iter().map(|node| node.clone_node(&self.f)).collect();
            if !clones.is_empty() {
                modifiers = Some(self.f.new_modifier_list(clones));
            }
        }
        let is_rest = parameter_declaration.is_some()
            && is_rest_parameter(parameter_declaration.unwrap())
            || parameter_symbol.check_flags().intersects(CheckFlags::RestParameter);
        let mut dot_dot_dot_token: Option<P<Node>> = None;
        if is_rest {
            dot_dot_dot_token = Some(self.f.new_token(Kind::DotDotDotToken));
        }
        let name = self.parameter_to_parameter_declaration_name(
            c,
            parameter_symbol,
            parameter_declaration,
        );
        let is_optional = parameter_declaration.is_some()
            && c.is_optional_parameter(parameter_declaration.unwrap())
            || parameter_symbol.check_flags().intersects(CheckFlags::OptionalParameter);
        let mut question_token: Option<P<Node>> = None;
        if is_optional {
            question_token = Some(self.f.new_token(Kind::QuestionToken));
        }

        let parameter_node = self.f.new_parameter_declaration(
            modifiers,
            dot_dot_dot_token,
            name,
            question_token,
            Some(parameter_type_node),
            None, /*initializer*/
        );
        let ctx = self.ctx();
        ctx.approximate_length
            .set(ctx.approximate_length.get() + parameter_symbol.name().len() + 3);
        parameter_node
    }

    // nodebuilderimpl.go:1763
    pub(crate) fn parameter_to_parameter_declaration_name(
        &self,
        c: &mut Checker,
        parameter_symbol: P<Symbol>,
        parameter_declaration: Option<P<Node>>,
    ) -> P<Node> {
        if parameter_declaration.is_none() || parameter_declaration.unwrap().name().is_none() {
            return self.new_identifier(c, parameter_symbol.name(), Some(parameter_symbol));
        }

        let name = parameter_declaration.unwrap().name().unwrap();
        match name.kind() {
            Kind::Identifier => {
                let cloned = self.f.deep_clone_node(Some(name)).unwrap();
                self.e.set_emit_flags(cloned, EmitFlags::NoAsciiEscaping);
                self.id_to_symbol.borrow_mut().insert(cloned, parameter_symbol);
                cloned
            }
            Kind::QualifiedName => {
                let cloned = self.f.deep_clone_node(Some(name.as_qualified_name().right)).unwrap();
                self.e.set_emit_flags(cloned, EmitFlags::NoAsciiEscaping);
                self.id_to_symbol.borrow_mut().insert(cloned, parameter_symbol);
                cloned
            }
            _ => self.clone_binding_name(c, name),
        }
    }

    // nodebuilderimpl.go:1785
    fn clone_binding_name(&self, c: &mut Checker, node: P<Node>) -> P<Node> {
        if is_computed_property_name(node) && c.is_late_bindable_name(node) {
            self.track_computed_name(
                c,
                node.expression().unwrap(),
                self.ctx().enclosing_declaration.get(),
            );
        }

        let mut visitor = new_node_visitor(
            Some(Rc::clone(self.clone_binding_name_visitor.get().unwrap())),
            Some(self.f.clone()),
            NodeVisitorHooks::default(),
        );
        let mut visited =
            self.checker_slot.lend(c, || visitor.visit_each_child(Some(node))).unwrap();

        if is_binding_element(visited) {
            let binding_element = visited.as_binding_element();
            visited = self.f.update_binding_element(
                visited,
                binding_element.dot_dot_dot_token(),
                binding_element.property_name(),
                binding_element.name,
                None, // remove initializer
            );
        }

        if !node_is_synthesized(visited) {
            visited = self.f.deep_clone_node(Some(visited)).unwrap();
        }

        self.e.set_emit_flags(visited, EmitFlags::SingleLine | EmitFlags::NoAsciiEscaping);
        visited
    }

    // nodebuilderimpl.go:1811
    pub(crate) fn serialize_type_for_expression(
        &self,
        c: &mut Checker,
        expr: P<Node>,
    ) -> Option<P<Node>> {
        // !!! TODO: shim, add node reuse
        let regular = c.get_regular_type_of_expression(expr);
        let widened = c.get_widened_type(regular);
        let t = c.instantiate_type(widened, self.ctx().mapper.get());
        self.type_to_type_node(c, Some(t))
    }

    // nodebuilderimpl.go:1817
    pub(crate) fn serialize_inferred_return_type_for_signature(
        &self,
        c: &mut Checker,
        signature: P<Signature>,
        return_type: P<Type>,
    ) -> Option<P<Node>> {
        let old_suppress_report_inference_fallback =
            self.ctx().suppress_report_inference_fallback.get();
        self.ctx().suppress_report_inference_fallback.set(true);
        let type_predicate = c.get_type_predicate_of_signature(signature);
        let return_type_node: Option<P<Node>>;
        if let Some(type_predicate) = type_predicate {
            let predicate = if let Some(mapper) = self.ctx().mapper.get() {
                c.instantiate_type_predicate(type_predicate, mapper)
            } else {
                type_predicate
            };
            return_type_node =
                Some(self.type_predicate_to_type_predicate_node_helper(c, predicate));
        } else {
            return_type_node = self.type_to_type_node(c, Some(return_type));
        }
        self.ctx().suppress_report_inference_fallback.set(old_suppress_report_inference_fallback);
        return_type_node
    }

    // nodebuilderimpl.go:1837
    fn type_predicate_to_type_predicate_node_helper(
        &self,
        c: &mut Checker,
        type_predicate: P<TypePredicate>,
    ) -> P<Node> {
        let kind = type_predicate.kind.get();
        let asserts_modifier: Option<P<Node>> = if kind == TypePredicateKind::AssertsThis
            || kind == TypePredicateKind::AssertsIdentifier
        {
            Some(self.f.new_token(Kind::AssertsKeyword))
        } else {
            None
        };
        let parameter_name: P<Node>;
        if kind == TypePredicateKind::Identifier || kind == TypePredicateKind::AssertsIdentifier {
            parameter_name =
                self.new_identifier(c, type_predicate.parameter_name.get(), None /*symbol*/);
            self.e.set_emit_flags(parameter_name, EmitFlags::NoAsciiEscaping);
        } else {
            parameter_name = self.f.new_this_type_node();
        }
        let mut type_node: Option<P<Node>> = None;
        if let Some(t) = type_predicate.t.get() {
            type_node = self.type_to_type_node(c, Some(t));
        }
        self.f.new_type_predicate_node(asserts_modifier, parameter_name, type_node)
    }
}
