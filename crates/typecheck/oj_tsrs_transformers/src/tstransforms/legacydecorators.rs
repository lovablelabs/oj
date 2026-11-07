use crate::*;
use printer::{AssignedNameOptions, EmitFlags, NameOptions};

struct LegacyDecoratorsTransformer {
    base: Transformer,
    language_version: ScriptTarget,
    reference_resolver: ReferenceResolverRef,

    /*
     * A map that keeps track of aliases created for classes with decorators to avoid issues
     * with the double-binding behavior of classes.
     */
    class_aliases: RefCell<Option<FxHashMap<P<Node>, P<Node>>>>,
    enclosing_classes: RefCell<Vec<P<Node>>>,
}

// legacydecorators.go:25
#[expect(
    clippy::unnecessary_wraps,
    reason = "Transformer factories share an optional result so disabled transforms can return None."
)]
pub fn new_legacy_decorators_transformer(opt: &TransformOptions) -> Option<P<Transformer>> {
    let tx = P::new(LegacyDecoratorsTransformer {
        base: Transformer::default(),
        language_version: opt.compiler_options.get_emit_script_target(),
        reference_resolver: opt.resolver,
        class_aliases: RefCell::new(None),
        enclosing_classes: RefCell::new(Vec::new()),
    });
    Some(tx.get().base.new_transformer(
        Rc::new(move |_: &mut NodeVisitor, n: P<Node>| tx.visit(n)),
        Some(opt.context),
    ))
}

impl LegacyDecoratorsTransformer {
    fn emit_context(&self) -> P<EmitContext> {
        self.base.emit_context()
    }

    fn factory(&self) -> &'static printer::NodeFactory {
        self.base.factory()
    }

    fn visitor(&self) -> NodeVisitor {
        self.base.visitor()
    }

    // legacydecorators.go:30
    fn visit(&self, node: P<Node>) -> Option<P<Node>> {
        // we have to visit all identifiers in classes, just in case they require substitution
        if !node.subtree_facts().intersects(SubtreeFacts::ContainsDecorators)
            && self.enclosing_classes.borrow().is_empty()
        {
            return Some(node);
        }

        match node.kind() {
            Kind::Identifier => Some(self.visit_identifier(node)),
            Kind::PropertyAccessExpression => Some(self.visit_property_access_expression(node)),
            Kind::Decorator => {
                // Decorators are elided. They will be emitted as part of `visitClassDeclaration`.
                None
            }
            Kind::ClassDeclaration => Some(self.visit_class_declaration(node)),
            Kind::ClassExpression => Some(self.visit_class_expression(node)),
            Kind::Constructor => Some(self.visit_constructor_declaration(node)),
            Kind::MethodDeclaration => Some(self.visit_method_declaration(node)),
            Kind::SetAccessor => Some(self.visit_set_accessor_declaration(node)),
            Kind::GetAccessor => Some(self.visit_get_accessor_declaration(node)),
            Kind::PropertyDeclaration => self.visit_property_declaration(node),
            Kind::Parameter => Some(self.visit_paramer_declaration(node)),
            Kind::SourceFile => {
                *self.class_aliases.borrow_mut() = Some(FxHashMap::default());
                self.enclosing_classes.borrow_mut().clear();
                let result = self.visitor().visit_each_child(Some(node)).unwrap();
                self.emit_context()
                    .add_emit_helper(result, &self.emit_context().read_emit_helpers());
                *self.class_aliases.borrow_mut() = None;
                self.enclosing_classes.borrow_mut().clear();
                Some(result)
            }
            _ => self.visitor().visit_each_child(Some(node)),
        }
    }

    fn class_alias_of(&self, node: P<Node>) -> Option<P<Node>> {
        self.class_aliases.borrow().as_ref().and_then(|m| m.get(&node).copied())
    }

    // legacydecorators.go:73
    fn visit_identifier(&self, node: P<Node>) -> P<Node> {
        // takes the place of `substituteIdentifier` in the strada transform
        let enclosing: Vec<P<Node>> = self.enclosing_classes.borrow().clone();
        if let Some(result) = enclosing.into_iter().find_map(|d| {
            if let Some(alias) = self.class_alias_of(d)
                && self.reference_resolver.get_referenced_value_declaration(
                    self.emit_context().most_original(Some(node)).unwrap(),
                ) == self.emit_context().most_original(Some(d))
            {
                return Some(alias);
            }

            None
        }) {
            return result;
        }
        node
    }

    // legacydecorators.go:83
    fn visit_property_access_expression(&self, node: P<Node>) -> P<Node> {
        // Visit the expression but not the name, since property access names should not be substituted.
        // Strada's onSubstituteNode only fires for EmitHint.Expression, which excludes the
        // .name of PropertyAccessExpression.
        let pa = node.as_property_access_expression();
        let expression = self.visitor().visit_node(Some(pa.expression)).unwrap();
        if expression != pa.expression {
            return self.factory().update_property_access_expression(
                node,
                expression,
                pa.question_dot_token(),
                pa.name,
                node.flags(),
            );
        }
        node
    }
}

// legacydecorators.go:106
fn elide_modifiers(
    f: &printer::NodeFactory,
    nodes: Option<P<ModifierList>>,
) -> Option<P<ModifierList>> {
    let nodes = nodes?;
    if nodes.nodes().is_empty() {
        return Some(nodes);
    }
    let replacement = f.new_modifier_list(vec![]);
    replacement.list.loc.set(nodes.list.loc.get());
    Some(replacement)
}

impl LegacyDecoratorsTransformer {
    // legacydecorators.go:118
    fn finish_class_element(&self, updated: P<Node>, original: P<Node>) -> P<Node> {
        if updated != original {
            // While we emit the source map for the node after skipping decorators and modifiers,
            // we need to emit the comments for the original range.
            self.emit_context().set_comment_range(updated, original.loc());
            self.emit_context().set_source_map_range(updated, move_range_past_modifiers(original));
        }
        updated
    }

    // legacydecorators.go:128
    fn visit_paramer_declaration(&self, node: P<Node>) -> P<Node> {
        let p = node.as_parameter_declaration();
        let updated = self.factory().update_parameter_declaration(
            node,
            elide_modifiers(self.factory(), node.modifiers()),
            p.dot_dot_dot_token(),
            self.visitor().visit_node(Some(p.name)).unwrap(),
            None,
            None,
            self.visitor().visit_node(p.initializer()),
        );
        if updated != node {
            // While we emit the source map for the node after skipping decorators and modifiers,
            // we need to emit the comments for the original range.
            self.emit_context().set_comment_range(updated, node.loc());
            let new_loc = move_range_past_modifiers(node);
            updated.set_loc(new_loc);
            self.emit_context().set_source_map_range(updated, new_loc);
            self.emit_context()
                .set_emit_flags(updated.name().unwrap(), EmitFlags::NoTrailingSourceMap);
        }
        updated
    }

    // legacydecorators.go:153
    // visitPropertyNameOfClassElement visits the property name of a class element,
    // for use when emitting property initializers. For a computed property on a node
    // with decorators, a temporary value is stored for later use.
    fn visit_property_name_of_class_element(&self, member: P<Node>) -> P<Node> {
        let name = member.name().unwrap();
        if ast::is_computed_property_name(name) && ast::has_decorators(member) {
            let expression = self
                .visitor()
                .visit_node(Some(name.as_computed_property_name().expression))
                .unwrap();
            let inner_expression = ast::skip_partially_emitted_expressions(expression);
            if !is_simple_inlineable_expression(inner_expression) {
                let generated_name = self.factory().new_generated_name_for_node(name);
                self.emit_context().add_variable_declaration(generated_name);
                return self.factory().update_computed_property_name(
                    name,
                    self.factory().new_assignment_expression(generated_name, expression),
                );
            }
        }
        self.visitor().visit_node(Some(name)).unwrap()
    }

    // legacydecorators.go:167
    fn visit_property_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        if node.flags().intersects(NodeFlags::Ambient) {
            return None;
        }
        if ast::has_syntactic_modifier(node, ModifierFlags::Ambient | ModifierFlags::Abstract) {
            return None;
        }

        Some(self.finish_class_element(
            self.factory().update_property_declaration(
                node,
                self.visitor().visit_modifiers(node.modifiers()),
                self.visit_property_name_of_class_element(node),
                None,
                None,
                self.visitor().visit_node(node.initializer()),
            ),
            node,
        ))
    }

    // legacydecorators.go:188
    fn visit_get_accessor_declaration(&self, node: P<Node>) -> P<Node> {
        self.finish_class_element(
            self.factory().update_get_accessor_declaration(
                node,
                self.visitor().visit_modifiers(node.modifiers()),
                self.visit_property_name_of_class_element(node),
                None,
                self.visitor().visit_nodes(node.parameter_list()),
                None,
                None,
                self.visitor().visit_node(node.body()),
            ),
            node,
        )
    }

    // legacydecorators.go:204
    fn visit_set_accessor_declaration(&self, node: P<Node>) -> P<Node> {
        self.finish_class_element(
            self.factory().update_set_accessor_declaration(
                node,
                self.visitor().visit_modifiers(node.modifiers()),
                self.visit_property_name_of_class_element(node),
                None,
                self.visitor().visit_nodes(node.parameter_list()),
                None,
                None,
                self.visitor().visit_node(node.body()),
            ),
            node,
        )
    }

    // legacydecorators.go:220
    fn visit_method_declaration(&self, node: P<Node>) -> P<Node> {
        self.finish_class_element(
            self.factory().update_method_declaration(
                node,
                self.visitor().visit_modifiers(node.modifiers()),
                node.as_method_declaration().asterisk_token(),
                self.visit_property_name_of_class_element(node),
                None,
                None,
                self.visitor().visit_nodes(node.parameter_list()),
                None,
                None,
                self.visitor().visit_node(node.body()),
            ),
            node,
        )
    }

    // legacydecorators.go:238
    fn visit_constructor_declaration(&self, node: P<Node>) -> P<Node> {
        self.factory().update_constructor_declaration(
            node,
            self.visitor().visit_modifiers(node.modifiers()),
            None,
            self.visitor().visit_nodes(node.parameter_list()),
            None,
            None,
            self.visitor().visit_node(node.body()),
        )
    }

    // legacydecorators.go:250
    fn visit_class_expression(&self, node: P<Node>) -> P<Node> {
        // Legacy decorators were not supported on class expressions
        let c = node.as_class_expression();
        self.factory().update_class_expression(
            node,
            self.visitor().visit_modifiers(node.modifiers()),
            node.name(),
            None,
            self.visitor().visit_nodes(c.heritage_clauses()),
            self.visitor().visit_nodes(Some(c.members())).unwrap(),
        )
    }

    // legacydecorators.go:262
    fn visit_class_declaration(&self, node: P<Node>) -> P<Node> {
        let decorated = ast::class_or_constructor_parameter_is_decorated(true, node);
        if !(decorated || ast::child_is_decorated(true, node, None)) {
            return self.visitor().visit_each_child(Some(node)).unwrap();
        }

        if decorated {
            return self.transform_class_declaration_with_class_decorators(node, node.name());
        }
        self.transform_class_declaration_without_class_decorators(node, node.name())
    }

    // legacydecorators.go:280
    /*
     * Transforms a non-decorated class declaration.
     *
     * @param node A ClassDeclaration node.
     * @param name The name of the class.
     */
    fn transform_class_declaration_without_class_decorators(
        &self,
        node: P<Node>,
        name: Option<P<Node>>,
    ) -> P<Node> {
        //  ${modifiers} class ${name} ${heritageClauses} {
        //      ${members}
        //  }
        let c = node.as_class_declaration();
        let modifiers = self.visitor().visit_modifiers(node.modifiers());
        let heritage_clauses = self.visitor().visit_nodes(c.heritage_clauses());
        let initial_members = self.visitor().visit_nodes(Some(c.members()));
        let (members, decoration_statements) =
            self.transform_decorators_of_class_elements(node, initial_members);

        let mut name = name;
        if name.is_none() && !decoration_statements.is_empty() {
            name = Some(self.factory().new_generated_name_for_node(node));
        }

        let updated = self.factory().update_class_declaration(
            node,
            modifiers,
            name,
            None,
            heritage_clauses,
            members.unwrap(),
        );

        if decoration_statements.is_empty() {
            return updated;
        }
        let mut list = vec![updated];
        list.extend(decoration_statements);
        self.factory().new_syntax_list(alloc_vec(list))
    }

    // legacydecorators.go:308
    fn pop_enclosing_class(&self) {
        self.enclosing_classes.borrow_mut().pop();
    }

    // legacydecorators.go:312
    fn push_enclosing_class(&self, cls: P<Node>) {
        self.enclosing_classes.borrow_mut().push(cls);
    }

    // legacydecorators.go:320
    /*
     * Transforms a decorated class declaration and appends the resulting statements. If
     * the class requires an alias to avoid issues with double-binding, the alias is returned.
     */
    fn transform_class_declaration_with_class_decorators(
        &self,
        node: P<Node>,
        name: Option<P<Node>>,
    ) -> P<Node> {
        // When we emit an ES6 class that has a class decorator, we must tailor the
        // emit to certain specific cases.
        //
        // In the simplest case, we emit the class declaration as a let declaration, and
        // evaluate decorators after the close of the class body:
        //
        //  [Example 1]
        //  ---------------------------------------------------------------------
        //  TypeScript                      | Javascript
        //  ---------------------------------------------------------------------
        //  @dec                            | let C = class C {
        //  class C {                       | }
        //  }                               | C = __decorate([dec], C);
        //  ---------------------------------------------------------------------
        //  @dec                            | let C = class C {
        //  export class C {                | }
        //  }                               | C = __decorate([dec], C);
        //                                  | export { C };
        //  ---------------------------------------------------------------------
        //
        // If a class declaration contains a reference to itself *inside* of the class body,
        // this introduces two bindings to the class: One outside of the class body, and one
        // inside of the class body. If we apply decorators as in [Example 1] above, there
        // is the possibility that the decorator `dec` will return a new value for the
        // constructor, which would result in the binding inside of the class no longer
        // pointing to the same reference as the binding outside of the class.
        //
        // As a result, we must instead rewrite all references to the class *inside* of the
        // class body to instead point to a local temporary alias for the class:
        //
        //  [Example 2]
        //  ---------------------------------------------------------------------
        //  TypeScript                      | Javascript
        //  ---------------------------------------------------------------------
        //  @dec                            | let C = C_1 = class C {
        //  class C {                       |   static x() { return C_1.y; }
        //    static x() { return C.y; }    | }
        //    static y = 1;                 | C.y = 1;
        //  }                               | C = C_1 = __decorate([dec], C);
        //                                  | var C_1;
        //  ---------------------------------------------------------------------
        //  @dec                            | let C = class C {
        //  export class C {                |   static x() { return C_1.y; }
        //    static x() { return C.y; }    | }
        //    static y = 1;                 | C.y = 1;
        //  }                               | C = C_1 = __decorate([dec], C);
        //                                  | export { C };
        //                                  | var C_1;
        //  ---------------------------------------------------------------------
        //
        // If a class declaration is the default export of a module, we instead emit
        // the export after the decorated declaration:
        //
        //  [Example 3]
        //  ---------------------------------------------------------------------
        //  TypeScript                      | Javascript
        //  ---------------------------------------------------------------------
        //  @dec                            | let default_1 = class {
        //  export default class {          | }
        //  }                               | default_1 = __decorate([dec], default_1);
        //                                  | export default default_1;
        //  ---------------------------------------------------------------------
        //  @dec                            | let C = class C {
        //  export default class C {        | }
        //  }                               | C = __decorate([dec], C);
        //                                  | export default C;
        //  ---------------------------------------------------------------------
        //
        // If the class declaration is the default export and a reference to itself
        // inside of the class body, we must emit both an alias for the class *and*
        // move the export after the declaration:
        //
        //  [Example 4]
        //  ---------------------------------------------------------------------
        //  TypeScript                      | Javascript
        //  ---------------------------------------------------------------------
        //  @dec                            | let C = class C {
        //  export default class C {        |   static x() { return C_1.y; }
        //    static x() { return C.y; }    | }
        //    static y = 1;                 | C.y = 1;
        //  }                               | C = C_1 = __decorate([dec], C);
        //                                  | export default C;
        //                                  | var C_1;
        //  ---------------------------------------------------------------------
        //

        let f = self.factory();
        let c = node.as_class_declaration();
        let is_export = ast::has_syntactic_modifier(node, ModifierFlags::Export);
        let is_default = ast::has_syntactic_modifier(node, ModifierFlags::Default);
        let mut modifiers: Option<P<ModifierList>> = None;
        if let Some(node_modifiers) = node.modifiers()
            && !node_modifiers.nodes().is_empty()
        {
            let modifier_nodes: Vec<P<Node>> = node_modifiers
                .nodes()
                .iter()
                .copied()
                .filter(|n| is_not_export_or_default_or_decorator(*n))
                .collect();
            if modifier_nodes.len() == node_modifiers.nodes().len() {
                modifiers = Some(node_modifiers);
            } else {
                let m = f.new_modifier_list(modifier_nodes);
                m.list.loc.set(node_modifiers.list.loc.get());
                modifiers = Some(m);
            }
        }

        let location = move_range_past_modifiers(node);
        let class_alias = self.get_class_alias_if_needed(node);
        if class_alias.is_some() {
            self.push_enclosing_class(node);
        }

        // When we used to transform to ES5/3 this would be moved inside an IIFE and should reference the name
        // without any block-scoped variable collision handling - but we don't support that anymore, so we always
        // use the local name for the class
        let decl_name = f.get_local_name_ex(
            node,
            AssignedNameOptions {
                allow_comments: false,
                allow_source_maps: true,
                ignore_assigned_name: false,
            },
        );

        //  ... = class ${name} ${heritageClauses} {
        //      ${members}
        //  }
        let heritage_clauses = self.visitor().visit_nodes(c.heritage_clauses());
        let members = self.visitor().visit_nodes(Some(c.members()));

        let (mut members, decoration_statements) =
            self.transform_decorators_of_class_elements(node, members);

        // If we're emitting to ES2022 or later then we need to reassign the class alias before
        // static initializers are evaluated.
        let assign_class_alias_in_static_block = self.language_version >= ScriptTarget::ES2022
            && class_alias.is_some()
            && members.is_some_and(|m| {
                !m.nodes().is_empty()
                    && m.nodes()
                        .iter()
                        .any(|n| is_class_static_block_declaration_or_static_property(*n))
            });
        if assign_class_alias_in_static_block {
            let mut member_list: Vec<P<Node>> = Vec::new();
            member_list.push(f.new_class_static_block_declaration(
                None,
                f.new_block(
                    f.new_node_list(vec![f.new_expression_statement(f.new_assignment_expression(
                        class_alias.unwrap(),
                        f.new_keyword_expression(Kind::ThisKeyword),
                    ))]),
                    false,
                ),
            ));
            member_list.extend_from_slice(members.unwrap().nodes());
            let new_list = f.new_node_list(member_list);
            new_list.loc.set(members.unwrap().loc.get());
            members = Some(new_list);
        }

        let mut expr_name = name;
        if let Some(name) = name
            && is_generated_identifier(self.emit_context(), name)
        {
            expr_name = None;
        }
        let class_expression =
            f.new_class_expression(modifiers, expr_name, None, heritage_clauses, members.unwrap());

        self.emit_context().set_original(class_expression, node);
        class_expression.set_loc(location);

        //  let ${name} = ${classExpression} where name is either declaredName if the class doesn't contain self-reference
        //                                         or decoratedClassAlias if the class contain self-reference.
        let mut var_initializer = class_expression;
        if let Some(class_alias) = class_alias
            && !assign_class_alias_in_static_block
        {
            var_initializer = f.new_assignment_expression(class_alias, class_expression);
        }
        let var_decl = f.new_variable_declaration(decl_name, None, None, Some(var_initializer));
        self.emit_context().set_original(var_decl, node);

        let var_decl_list =
            f.new_variable_declaration_list(f.new_node_list(vec![var_decl]), NodeFlags::Let);
        let var_statement = f.new_variable_statement(None, var_decl_list);
        self.emit_context().set_original(var_statement, node);
        var_statement.set_loc(location);
        self.emit_context().set_comment_range(var_statement, node.loc());

        let mut statements: Vec<P<Node>> = vec![var_statement];
        statements.extend(decoration_statements);
        // Go appends the result even when it is nil; it is never nil here because the class is decorated.
        statements.extend(self.get_constructor_decoration_statement(node));

        if is_export {
            let export_statement = if is_default {
                f.new_export_default(decl_name)
            } else {
                f.new_external_module_export(f.get_declaration_name(node))
            };
            statements.push(export_statement);
        }

        // Go: defer tx.popEnclosingClass()
        if class_alias.is_some() {
            self.pop_enclosing_class();
        }

        if statements.len() == 1 {
            return statements[0];
        }
        f.new_syntax_list(alloc_vec(statements))
    }

    // legacydecorators.go:512
    fn has_internal_static_reference(&self, node: P<Node>) -> bool {
        fn is_or_contains_static_self_reference(
            tx: &LegacyDecoratorsTransformer,
            class_node: Option<P<Node>>,
            n: P<Node>,
        ) -> bool {
            if ast::is_identifier(n)
                && tx.reference_resolver.get_referenced_value_declaration(
                    tx.emit_context().most_original(Some(n)).unwrap(),
                ) == class_node
            {
                return true;
            }
            // For PropertyAccessExpression, only check the expression, not the name.
            // The .Name() is a property access name, not a value reference to the class.
            if ast::is_property_access_expression(n) {
                return is_or_contains_static_self_reference(
                    tx,
                    class_node,
                    n.expression().unwrap(),
                );
            }
            n.for_each_child(&mut |c| is_or_contains_static_self_reference(tx, class_node, c))
        }

        let class_node = self.emit_context().most_original(Some(node));
        node.members().iter().any(|member| {
            member
                .for_each_child(&mut |c| is_or_contains_static_self_reference(self, class_node, c))
        })
    }

    // legacydecorators.go:539
    /*
     * Gets a local alias for a class declaration if it is a decorated class with an internal
     * reference to the static side of the class. This is necessary to avoid issues with
     * double-binding semantics for the class name.
     */
    fn get_class_alias_if_needed(&self, node: P<Node>) -> Option<P<Node>> {
        if !self.has_internal_static_reference(node) {
            return None;
        }
        let mut name_text = "default";
        if let Some(name) = node.name()
            && !is_generated_identifier(self.emit_context(), name)
        {
            name_text = name.text();
        }

        let class_alias = self.factory().new_unique_name(name_text);
        self.emit_context().add_variable_declaration(class_alias);
        self.class_aliases.borrow_mut().as_mut().unwrap().insert(node, class_alias);

        Some(class_alias)
    }

    // legacydecorators.go:560
    /*
     * Generates a __decorate helper call for a class constructor.
     *
     * @param node The class node.
     */
    fn get_constructor_decoration_statement(&self, node: P<Node>) -> Option<P<Node>> {
        let expression = self.generate_constructor_decoration_expression(node)?;
        let result = self.factory().new_expression_statement(expression);
        self.emit_context().set_original(result, node);
        Some(result)
    }

    // legacydecorators.go:575
    /*
     * Generates a __decorate helper call for a class constructor.
     *
     * @param node The class node.
     */
    fn generate_constructor_decoration_expression(&self, node: P<Node>) -> Option<P<Node>> {
        let all_decorators = get_all_decorators_of_class(node, true);
        // Decorator expressions are evaluated outside the class body, so references to the
        // class name should use the original binding, not the class alias. In Strada, this is
        // handled by NodeCheckFlags.ConstructorReference which is only set for identifiers
        // inside the class body. Since Corsa lacks per-node flags, we temporarily pop the
        // enclosing class to prevent alias substitution during decorator expression visiting.
        let has_alias = self.enclosing_classes.borrow().last() == Some(&node);
        if has_alias {
            self.pop_enclosing_class();
        }
        let decorator_expressions =
            self.transform_all_decorators_of_declaration(all_decorators.as_ref());
        if has_alias {
            self.push_enclosing_class(node);
        }
        if decorator_expressions.is_empty() {
            return None;
        }

        let class_alias = self.class_alias_of(node);

        // When we used to transform to ES5/3 this would be moved inside an IIFE and should reference the name
        // without any block-scoped variable collision handling - but we don't support that anymore, so we always
        // use the local name for the class
        let f = self.factory();
        let local_name = f.get_declaration_name_ex(
            node,
            NameOptions { allow_comments: false, allow_source_maps: true },
        );
        let decorate = f.new_decorate_helper(decorator_expressions, local_name, None, None);
        let mut assignment_target = decorate;
        if let Some(class_alias) = class_alias {
            assignment_target = f.new_assignment_expression(class_alias, decorate);
        }
        let expression = f.new_assignment_expression(local_name, assignment_target);
        self.emit_context().set_emit_flags(expression, EmitFlags::NoComments);
        self.emit_context().set_source_map_range(expression, move_range_past_modifiers(node));
        Some(expression)
    }
}

// legacydecorators.go:614
fn is_class_static_block_declaration_or_static_property(node: P<Node>) -> bool {
    ast::is_class_static_block_declaration(node)
        || (ast::is_property_declaration(node) && ast::has_static_modifier(node))
}

// legacydecorators.go:618
fn is_not_export_or_default_or_decorator(node: P<Node>) -> bool {
    !(ast::is_decorator(node)
        || node.kind() == Kind::ExportKeyword
        || node.kind() == Kind::DefaultKeyword)
}

// legacydecorators.go:622
fn decorator_contains_private_identifier_in_expression(decorator: P<Node>) -> bool {
    decorator.subtree_facts().intersects(SubtreeFacts::ContainsPrivateIdentifierInExpression)
}

// legacydecorators.go:626
fn parameter_decorators_contain_private_identifier_in_expression(
    parameter_decorators: &[P<Node>],
) -> bool {
    parameter_decorators.iter().any(|d| decorator_contains_private_identifier_in_expression(*d))
}

// legacydecorators.go:630
fn has_class_element_with_decorator_containing_private_identifier_in_expression(
    node: P<Node>,
) -> bool {
    let members = node.members();
    if members.is_empty() {
        return false;
    }
    if members.iter().any(|member| {
        let member = *member;
        if !ast::can_have_decorators(member) {
            return false;
        }
        let Some(all_decorators) = get_all_decorators_of_class_element(member, node, true) else {
            return false;
        };
        if all_decorators
            .decorators
            .iter()
            .any(|d| decorator_contains_private_identifier_in_expression(*d))
        {
            return true;
        }
        all_decorators
            .parameters
            .iter()
            .any(|p| parameter_decorators_contain_private_identifier_in_expression(p))
    }) {
        return true;
    }
    false
}

// legacydecorators.go:652
struct AllDecorators {
    decorators: Vec<P<Node>>,
    parameters: Vec<Vec<P<Node>>>,
}

// legacydecorators.go:665
/*
 * Gets an allDecorators object containing the decorators for the class and the decorators for the
 * parameters of the constructor of the class.
 *
 * @param node The class node.
 *
 * @internal
 */
fn get_all_decorators_of_class(
    node: P<Node>,
    use_legacy_decorators: bool,
) -> Option<AllDecorators> {
    let decorators = node.decorators();
    let mut parameters: Vec<Vec<P<Node>>> = Vec::new();
    if use_legacy_decorators {
        parameters = get_decorators_of_parameters(ast::get_first_constructor_with_body(node));
    }
    if decorators.is_empty() && parameters.is_empty() {
        return None;
    }
    Some(AllDecorators { decorators, parameters })
}

// legacydecorators.go:685
/*
 * Gets an allDecorators object containing the decorators for the member and its parameters.
 *
 * @param parent The class node that contains the member.
 * @param member The class member.
 *
 * @internal
 */
fn get_all_decorators_of_class_element(
    member: P<Node>,
    parent: P<Node>,
    use_legacy_decorators: bool,
) -> Option<AllDecorators> {
    match member.kind() {
        Kind::GetAccessor | Kind::SetAccessor => {
            if !use_legacy_decorators {
                return get_all_decorators_of_method(member, false);
            }
            get_all_decorators_of_accessors(member, parent, true)
        }
        Kind::MethodDeclaration => get_all_decorators_of_method(member, use_legacy_decorators),
        Kind::PropertyDeclaration => get_all_decorators_of_property(member),
        _ => None,
    }
}

// legacydecorators.go:707
/*
 * Gets an allDecorators object containing the decorators for the accessor and its parameters.
 *
 * @param parent The class node that contains the accessor.
 * @param accessor The class accessor member.
 */
fn get_all_decorators_of_accessors(
    accessor: P<Node>,
    parent: P<Node>,
    use_legacy_decorators: bool,
) -> Option<AllDecorators> {
    accessor.body()?;
    let decls = ast::get_all_accessor_declarations(parent.members(), accessor);
    let mut first_accessor_with_decorators: Option<P<Node>> = None;
    if ast::has_decorators(decls.first_accessor) {
        first_accessor_with_decorators = Some(decls.first_accessor);
    } else if let Some(second_accessor) = decls.second_accessor
        && ast::has_decorators(second_accessor)
    {
        first_accessor_with_decorators = Some(second_accessor);
    }

    let first_accessor_with_decorators = first_accessor_with_decorators?;
    if accessor != first_accessor_with_decorators {
        return None;
    }

    let decorators = first_accessor_with_decorators.decorators();
    let mut parameters: Vec<Vec<P<Node>>> = Vec::new();
    if use_legacy_decorators && let Some(set_accessor) = decls.set_accessor {
        parameters = get_decorators_of_parameters(Some(set_accessor));
    }

    if decorators.is_empty() && parameters.is_empty() {
        return None;
    }

    Some(AllDecorators { decorators, parameters })
}

// legacydecorators.go:739
fn get_all_decorators_of_property(property: P<Node>) -> Option<AllDecorators> {
    let decorators = property.decorators();
    if decorators.is_empty() {
        return None;
    }
    Some(AllDecorators { decorators, parameters: Vec::new() })
}

// legacydecorators.go:747
fn get_all_decorators_of_method(
    method: P<Node>,
    use_legacy_decorators: bool,
) -> Option<AllDecorators> {
    method.body()?;
    let decorators = method.decorators();
    let mut parameters: Vec<Vec<P<Node>>> = Vec::new();
    if use_legacy_decorators {
        parameters = get_decorators_of_parameters(Some(method));
    }
    if decorators.is_empty() && parameters.is_empty() {
        return None;
    }
    Some(AllDecorators { decorators, parameters })
}

// legacydecorators.go:768
/*
 * Gets an array of arrays of decorators for the parameters of a function-like node.
 * The offset into the result array should correspond to the offset of the parameter.
 *
 * @param node The function-like node.
 */
#[expect(
    clippy::redundant_pub_crate,
    reason = "Only the public API is re-exported; implementation helpers remain crate-private."
)]
pub(crate) fn get_decorators_of_parameters(node: Option<P<Node>>) -> Vec<Vec<P<Node>>> {
    let mut decorators: Vec<Vec<P<Node>>> = Vec::new();
    if let Some(node) = node {
        let parameters = node.parameters();
        let first_parameter_is_this =
            !parameters.is_empty() && ast::is_this_parameter(parameters[0]);
        let mut first_parameter_offset = 0;
        let mut num_parameters = parameters.len();
        if first_parameter_is_this {
            first_parameter_offset = 1;
            num_parameters -= 1;
        }
        for i in 0..num_parameters {
            let p = parameters[i + first_parameter_offset];
            if !decorators.is_empty() || ast::has_decorators(p) {
                if decorators.is_empty() {
                    decorators = vec![Vec::new(); num_parameters];
                }
                decorators[i] = p.decorators();
            }
        }
    }
    decorators
}

impl LegacyDecoratorsTransformer {
    // legacydecorators.go:793
    fn transform_decorators_of_class_elements(
        &self,
        node: P<Node>,
        members: Option<P<NodeList>>,
    ) -> (Option<P<NodeList>>, Vec<P<Node>>) {
        let mut members = members;
        let mut decoration_statements: Vec<P<Node>> = Vec::new();
        decoration_statements.extend(self.get_class_element_decoration_statements(node, false));
        decoration_statements.extend(self.get_class_element_decoration_statements(node, true));
        if has_class_element_with_decorator_containing_private_identifier_in_expression(node) {
            let mut member_nodes: &[P<Node>] = &[];
            if let Some(m) = members
                && !m.nodes().is_empty()
            {
                member_nodes = m.nodes();
            }
            let f = self.factory();
            let mut list: Vec<P<Node>> = member_nodes.to_vec();
            list.push(f.new_class_static_block_declaration(
                None,
                f.new_block(f.new_node_list(std::mem::take(&mut decoration_statements)), true),
            ));
            members = Some(f.new_node_list(list));
        }

        (members, decoration_statements)
    }

    // legacydecorators.go:822
    /*
     * Generates statements used to apply decorators to either the static or instance members
     * of a class.
     *
     * @param node The class node.
     * @param isStatic A value indicating whether to generate statements for static or
     *                 instance members.
     */
    fn get_class_element_decoration_statements(
        &self,
        node: P<Node>,
        is_static: bool,
    ) -> Vec<P<Node>> {
        let exprs = self.generate_class_element_decoration_expressions(node, is_static);
        let mut statements: Vec<P<Node>> = Vec::new();
        statements.extend(exprs.into_iter().map(|e| self.factory().new_expression_statement(e)));
        statements
    }
}

// legacydecorators.go:837
/*
 * Determines whether a class member is either a static or an instance member of a class
 * that is decorated, or has parameters that are decorated.
 *
 * @param member The class member.
 */
fn is_decorated_class_element(member: P<Node>, is_static_element: bool, parent: P<Node>) -> bool {
    is_static_element == ast::is_static(member)
        && ast::node_or_child_is_decorated(true, member, Some(parent), None)
}

// legacydecorators.go:849
/*
 * Gets either the static or instance members of a class that are decorated, or have
 * parameters that are decorated.
 *
 * @param node The class containing the member.
 * @param isStatic A value indicating whether to retrieve static or instance members of
 *                 the class.
 */
fn get_decorated_class_elements(node: P<Node>, is_static: bool) -> Vec<P<Node>> {
    let mut members: Vec<P<Node>> = Vec::new();
    members.extend(node.members().iter().filter_map(|member| {
        if is_decorated_class_element(*member, is_static, node) { Some(*member) } else { None }
    }));
    members
}

impl LegacyDecoratorsTransformer {
    // legacydecorators.go:870
    /*
     * Generates expressions used to apply decorators to either the static or instance members
     * of a class.
     *
     * @param node The class node.
     * @param isStatic A value indicating whether to generate expressions for static or
     *                 instance members.
     */
    fn generate_class_element_decoration_expressions(
        &self,
        node: P<Node>,
        is_static: bool,
    ) -> Vec<P<Node>> {
        let members = get_decorated_class_elements(node, is_static);
        let mut expressions: Vec<P<Node>> = Vec::new();
        expressions.extend(
            members.into_iter().filter_map(|member| {
                self.generate_class_element_decoration_expression(node, member)
            }),
        );
        expressions
    }

    // legacydecorators.go:888
    /*
     * Generates an expression used to evaluate class element decorators at runtime.
     *
     * @param node The class node that contains the member.
     * @param member The class member.
     */
    fn generate_class_element_decoration_expression(
        &self,
        node: P<Node>,
        member: P<Node>,
    ) -> Option<P<Node>> {
        let all_decorators = get_all_decorators_of_class_element(member, node, true);
        let decorator_expressions =
            self.transform_all_decorators_of_declaration(all_decorators.as_ref());
        if decorator_expressions.is_empty() {
            return None;
        }

        // Emit the call to __decorate. Given the following:
        //
        //   class C {
        //     @dec method(@dec2 x) {}
        //     @dec get accessor() {}
        //     @dec prop;
        //   }
        //
        // The emit for a method is:
        //
        //   __decorate([
        //       dec,
        //       __param(0, dec2),
        //       __metadata("design:type", Function),
        //       __metadata("design:paramtypes", [Object]),
        //       __metadata("design:returntype", void 0)
        //   ], C.prototype, "method", null);
        //
        // The emit for an accessor is:
        //
        //   __decorate([
        //       dec
        //   ], C.prototype, "accessor", null);
        //
        // The emit for a property is:
        //
        //   __decorate([
        //       dec
        //   ], C.prototype, "prop");
        //

        let f = self.factory();
        let prefix = self.get_class_member_prefix(node, member);
        let member_name = self.get_expression_for_property_name(
            member,
            !member.flags().intersects(NodeFlags::Ambient),
        );
        let descriptor =
            if ast::is_property_declaration(member) && !ast::has_accessor_modifier(member) {
                // We emit `void 0` here to indicate to `__decorate` that it can invoke `Object.defineProperty` directly, but that it
                // should not invoke `Object.getOwnPropertyDescriptor`.
                f.new_void_zero_expression()
            } else {
                // We emit `null` here to indicate to `__decorate` that it can invoke `Object.getOwnPropertyDescriptor` directly.
                // We have this extra argument here so that we can inject an explicit property descriptor at a later date.
                f.new_keyword_expression(Kind::NullKeyword)
            };

        let helper = f.new_decorate_helper(
            decorator_expressions,
            prefix,
            Some(member_name),
            Some(descriptor),
        );

        self.emit_context().set_emit_flags(helper, EmitFlags::NoComments);
        self.emit_context().set_source_map_range(helper, move_range_past_modifiers(member));
        Some(helper)
    }

    // legacydecorators.go:951
    fn is_synthetic_metadata_decorator(&self, node: P<Node>) -> bool {
        self.emit_context().is_call_to_helper(node.expression().unwrap(), "__metadata")
    }

    // legacydecorators.go:960
    /*
     * Transforms all of the decorators for a declaration into an array of expressions.
     *
     * @param allDecorators An object containing all of the decorators for the declaration.
     */
    fn transform_all_decorators_of_declaration(
        &self,
        all_decorators: Option<&AllDecorators>,
    ) -> Vec<P<Node>> {
        let Some(all_decorators) = all_decorators else {
            return Vec::new();
        };

        // ensure that metadata decorators are last
        let (metadata, decorators): (Vec<P<Node>>, Vec<P<Node>>) = all_decorators
            .decorators
            .iter()
            .copied()
            .partition(|d| self.is_synthetic_metadata_decorator(*d));

        let mut decorator_expressions: Vec<P<Node>> = Vec::new();
        decorator_expressions.extend(self.transform_decorators(&decorators));
        decorator_expressions
            .extend(self.transform_decorators_of_parameters(&all_decorators.parameters));
        decorator_expressions.extend(self.transform_decorators(&metadata));
        decorator_expressions
    }

    // legacydecorators.go:977
    fn transform_decorators_of_parameters(&self, parameters: &[Vec<P<Node>>]) -> Vec<P<Node>> {
        let mut results: Vec<P<Node>> = Vec::new();
        for (i, decorators) in parameters.iter().enumerate() {
            if !decorators.is_empty() {
                results.extend(decorators.iter().map(|decorator| {
                    let helper = self.factory().new_param_helper(
                        self.visitor().visit_node(decorator.expression()).unwrap(),
                        i,
                        decorator.expression().unwrap().loc(),
                    );
                    self.emit_context().set_emit_flags(helper, EmitFlags::NoComments);
                    helper
                }));
            }
        }
        results
    }

    // legacydecorators.go:1000
    /*
     * Transforms a list of decorators into an expression.
     *
     * @param decorator The decorator node.
     */
    fn transform_decorators(&self, decorators: &[P<Node>]) -> Vec<P<Node>> {
        let mut results: Vec<P<Node>> = Vec::new();
        results.extend(decorators.iter().filter_map(|d| {
            // Go appends the visited expression even when it is nil.
            self.visitor().visit_node(d.expression())
        }));
        results
    }

    // legacydecorators.go:1008
    fn get_class_member_prefix(&self, node: P<Node>, member: P<Node>) -> P<Node> {
        if ast::is_static(member) {
            return self.factory().get_declaration_name(node);
        }
        self.get_class_prototype(node)
    }

    // legacydecorators.go:1015
    fn get_class_prototype(&self, node: P<Node>) -> P<Node> {
        let f = self.factory();
        f.new_property_access_expression(
            f.get_declaration_name(node),
            None,
            f.new_identifier("prototype"),
            NodeFlags::None,
        )
    }

    // legacydecorators.go:1024
    fn get_expression_for_property_name(
        &self,
        member: P<Node>,
        generate_name_for_computed_property_name: bool,
    ) -> P<Node> {
        let f = self.factory();
        let name = member.name().unwrap();
        if ast::is_private_identifier(name) {
            f.new_identifier("")
        } else if ast::is_computed_property_name(name) {
            if generate_name_for_computed_property_name
                && !is_simple_inlineable_expression(name.as_computed_property_name().expression)
            {
                return f.new_generated_name_for_node(name);
            }
            name.as_computed_property_name().expression
        } else if ast::is_identifier(name) {
            f.new_string_literal(name.text(), TokenFlags::None)
        } else {
            f.deep_clone_node(Some(name)).unwrap()
        }
    }
}
