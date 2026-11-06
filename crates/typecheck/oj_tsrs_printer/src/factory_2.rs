// factory.go from "Common Tokens" (line 189) to the end: the transform helpers of `printer.NodeFactory`. The
// functions factory.rs already defines (NewAssignmentExpression, NewStrictEqualityExpression, NewVoidZeroExpression,
// NewTypeCheck) are not repeated here.

use tsrs_ast as ast;
use tsrs_ast::*;
use tsrs_core::*;

use crate::*;

// factory.go:485
#[derive(Clone, Copy, Default)]
pub struct NameOptions {
    pub allow_comments: bool, // indicates whether comments may be emitted for the name.
    pub allow_source_maps: bool, // indicates whether source maps may be emitted for the name.
}

// factory.go:490
#[derive(Clone, Copy, Default)]
pub struct AssignedNameOptions {
    pub allow_comments: bool, // indicates whether comments may be emitted for the name.
    pub allow_source_maps: bool, // indicates whether source maps may be emitted for the name.
    pub ignore_assigned_name: bool, // indicates whether the assigned name of a declaration shouldn't be considered.
}

// factory.go:676
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PrivateIdentifierKind {
    Field,
    Method,
    Accessor,
    Untransformed,
}

impl PrivateIdentifierKind {
    fn as_str(self) -> &'static str {
        match self {
            PrivateIdentifierKind::Field => "f",
            PrivateIdentifierKind::Method => "m",
            PrivateIdentifierKind::Accessor => "a",
            PrivateIdentifierKind::Untransformed => "untransformed",
        }
    }
}

fn helper(h: &'static EmitHelper) -> SP<EmitHelper> {
    SP::from_static(h)
}

// factory.go:245
fn flatten_comma_element(node: P<Node>, mut expressions: Vec<P<Node>>) -> Vec<P<Node>> {
    if ast::is_binary_expression(node)
        && ast::node_is_synthesized(node)
        && node.as_binary_expression().operator_token.kind() == Kind::CommaToken
    {
        expressions = flatten_comma_element(node.as_binary_expression().left, expressions);
        expressions = flatten_comma_element(node.as_binary_expression().right(), expressions);
    } else {
        expressions.push(node);
    }
    expressions
}

// factory.go:255
fn flatten_comma_elements(expressions: &[P<Node>]) -> Vec<P<Node>> {
    let mut result = Vec::new();
    result = expressions.iter().fold(result, |mut result, &expression| {
        result = flatten_comma_element(expression, result);

        result
    });
    result
}

impl crate::NodeFactory {
    //
    // Common Tokens
    //

    // factory.go:193
    pub fn new_this_expression(&self) -> P<Node> {
        self.new_keyword_expression(Kind::ThisKeyword)
    }

    // factory.go:197
    pub fn new_true_expression(&self) -> P<Node> {
        self.new_keyword_expression(Kind::TrueKeyword)
    }

    // factory.go:201
    pub fn new_false_expression(&self) -> P<Node> {
        self.new_keyword_expression(Kind::FalseKeyword)
    }

    //
    // Common Operators
    //

    // factory.go:209
    pub fn new_comma_expression(&self, left: P<Node>, right: P<Node>) -> P<Node> {
        self.new_binary_expression(
            None, /*modifiers*/
            left,
            None, /*typeNode*/
            self.new_token(Kind::CommaToken),
            right,
        )
    }

    // factory.go:217
    pub fn new_logical_or_expression(&self, left: P<Node>, right: P<Node>) -> P<Node> {
        self.new_binary_expression(
            None, /*modifiers*/
            left,
            None, /*typeNode*/
            self.new_token(Kind::BarBarToken),
            right,
        )
    }

    // factory.go:221
    pub fn new_logical_and_expression(&self, left: P<Node>, right: P<Node>) -> P<Node> {
        self.new_binary_expression(
            None, /*modifiers*/
            left,
            None, /*typeNode*/
            self.new_token(Kind::AmpersandAmpersandToken),
            right,
        )
    }

    // factory.go:233
    pub fn new_strict_inequality_expression(&self, left: P<Node>, right: P<Node>) -> P<Node> {
        self.new_binary_expression(
            None, /*modifiers*/
            left,
            None, /*typeNode*/
            self.new_token(Kind::ExclamationEqualsEqualsToken),
            right,
        )
    }

    //
    // Compound Nodes
    //

    // Converts a slice of expressions into a single comma-delimited expression. Returns nil if expressions is nil or empty.
    // factory.go:264
    pub fn inline_expressions(&self, expressions: &[P<Node>]) -> Option<P<Node>> {
        if expressions.is_empty() {
            return None;
        }
        if expressions.len() == 1 {
            return Some(expressions[0]);
        }
        let expressions = flatten_comma_elements(expressions);
        let mut expression = expressions[0];
        expression = expressions[1..].iter().fold(expression, |mut expression, &next| {
            expression = self.new_comma_expression(expression, next);

            expression
        });
        Some(expression)
    }

    //
    // Utilities
    //

    // factory.go:283
    pub fn create_expression_from_entity_name(&self, node: P<Node>) -> P<Node> {
        if ast::is_qualified_name(node) {
            let left = self.create_expression_from_entity_name(node.as_qualified_name().left);
            let qn_right = node.as_qualified_name().right;
            let right = qn_right.clone_node(self.as_node_factory());
            right.set_loc(qn_right.loc());
            // TODO(rbuckton): Does this need to be parented?
            right.set_parent(qn_right.parent());
            let prop_access =
                self.new_property_access_expression(left, None, right, NodeFlags::None);
            prop_access.set_loc(node.loc());
            return prop_access;
        }
        let res = node.clone_node(self.as_node_factory());
        res.set_loc(node.loc());
        // TODO(rbuckton): Does this need to be parented?
        res.set_parent(node.parent());
        res
    }

    // factory.go:301
    pub fn restore_enclosing_label(
        &self,
        node: P<Node>,
        outermost_labeled_statement: Option<P<Node>>,
    ) -> P<Node> {
        let Some(outermost_labeled_statement) = outermost_labeled_statement else {
            return node;
        };
        let labeled = outermost_labeled_statement.as_labeled_statement();
        let mut inner_label = node;
        if ast::is_labeled_statement(labeled.statement) {
            inner_label = self.restore_enclosing_label(node, Some(labeled.statement));
        }
        self.update_labeled_statement(outermost_labeled_statement, labeled.label, inner_label)
    }

    // CreateForOfBindingStatement creates a statement to bind the iteration value.
    // factory.go:317
    pub fn create_for_of_binding_statement(&self, node: P<Node>, bound_value: P<Node>) -> P<Node> {
        if ast::is_variable_declaration_list(node) {
            let first_declaration = node.as_variable_declaration_list().declarations.nodes()[0];
            let updated_declaration = self.update_variable_declaration(
                first_declaration,
                first_declaration.name().unwrap(),
                None, /*exclamationToken*/
                None, /*type*/
                Some(bound_value),
            );
            let statement = self.new_variable_statement(
                None,
                self.update_variable_declaration_list(
                    node,
                    self.new_node_list(vec![updated_declaration]),
                    node.flags(),
                ),
            );
            statement.set_loc(node.loc());
            return statement;
        }
        let updated_expression = self.new_assignment_expression(node, bound_value);
        updated_expression.set_loc(node.loc());
        let statement = self.new_expression_statement(updated_expression);
        statement.set_loc(node.loc());
        statement
    }

    // factory.go:355
    pub fn new_method_call(
        &self,
        object: P<Node>,
        method_name: P<Node>,
        arguments_list: Vec<P<Node>>,
    ) -> P<Node> {
        // Preserve the optionality of `object`.
        if ast::is_call_expression(object) && object.flags().intersects(NodeFlags::OptionalChain) {
            return self.new_call_expression(
                self.new_property_access_expression(object, None, method_name, NodeFlags::None),
                None,
                None,
                self.new_node_list(arguments_list),
                NodeFlags::OptionalChain,
            );
        }
        self.new_call_expression(
            self.new_property_access_expression(object, None, method_name, NodeFlags::None),
            None,
            None,
            self.new_node_list(arguments_list),
            NodeFlags::None,
        )
    }

    // factory.go:375
    pub fn new_global_method_call(
        &self,
        global_object_name: &str,
        method_name: &str,
        arguments_list: Vec<P<Node>>,
    ) -> P<Node> {
        self.new_method_call(
            self.new_identifier(alloc_str(global_object_name)),
            self.new_identifier(alloc_str(method_name)),
            arguments_list,
        )
    }

    // factory.go:379
    pub fn new_function_call_call(
        &self,
        target: P<Node>,
        this_arg: Option<P<Node>>,
        arguments_list: &[P<Node>],
    ) -> P<Node> {
        let Some(this_arg) = this_arg else {
            panic!("Attempted to construct function call call without this argument expression");
        };
        let mut args = vec![this_arg];
        args.extend_from_slice(arguments_list);
        self.new_method_call(target, self.new_identifier("call"), args)
    }

    // factory.go:387
    pub fn new_array_slice_call(&self, array: P<Node>, start: usize) -> P<Node> {
        let mut args = Vec::new();
        if start != 0 {
            args.push(self.new_numeric_literal(alloc_str(&start.to_string()), TokenFlags::None));
        }
        self.new_method_call(array, self.new_identifier("slice"), args)
    }

    // Determines whether a node is a parenthesized expression that can be ignored when recreating outer expressions.
    //
    // A parenthesized expression can be ignored when all of the following are true:
    //
    // - It's `pos` and `end` are not -1
    // - It does not have a custom source map range
    // - It does not have a custom comment range
    // - It does not have synthetic leading or trailing comments
    //
    // If an outermost parenthesized expression is ignored, but the containing expression requires a parentheses around
    // the expression to maintain precedence, a new parenthesized expression should be created automatically when
    // the containing expression is created/updated.
    // factory.go:407
    fn is_ignorable_paren(&self, node: P<Node>) -> bool {
        let ec = self.emit_context();
        ast::is_parenthesized_expression(node)
            && ast::node_is_synthesized(node)
            && ast::range_is_synthesized(ec.source_map_range(node))
            && ast::range_is_synthesized(ec.comment_range(node)) // &&
        // len(emitContext.SyntheticLeadingComments(node)) == 0 &&
        // len(emitContext.SyntheticTrailingComments(node)) == 0
    }

    // factory.go:416
    fn update_outer_expression(
        &self,
        outer_expression: P<Node>, /*OuterExpression*/
        expression: P<Node>,
    ) -> P<Node> {
        match outer_expression.kind() {
            Kind::ParenthesizedExpression => {
                self.update_parenthesized_expression(outer_expression, expression)
            }
            Kind::TypeAssertionExpression => self.update_type_assertion(
                outer_expression,
                outer_expression.type_node().unwrap(),
                expression,
            ),
            Kind::AsExpression => self.update_as_expression(
                outer_expression,
                expression,
                outer_expression.type_node().unwrap(),
            ),
            Kind::SatisfiesExpression => self.update_satisfies_expression(
                outer_expression,
                expression,
                outer_expression.type_node().unwrap(),
            ),
            Kind::NonNullExpression => self.update_non_null_expression(
                outer_expression,
                expression,
                outer_expression.flags(),
            ),
            Kind::ExpressionWithTypeArguments => self.update_expression_with_type_arguments(
                outer_expression,
                expression,
                outer_expression.type_argument_list(),
            ),
            Kind::PartiallyEmittedExpression => {
                self.update_partially_emitted_expression(outer_expression, expression)
            }
            k => panic!("Unexpected outer expression kind: {k:?}"),
        }
    }

    // factory.go:437
    pub fn restore_outer_expressions(
        &self,
        outer_expression: Option<P<Node>>,
        inner_expression: P<Node>,
        kinds: OuterExpressionKinds,
    ) -> P<Node> {
        if let Some(outer_expression) = outer_expression
            && ast::is_outer_expression(outer_expression, kinds)
            && !self.is_ignorable_paren(outer_expression)
        {
            return self.update_outer_expression(
                outer_expression,
                self.restore_outer_expressions(
                    outer_expression.expression(),
                    inner_expression,
                    OuterExpressionKinds::All,
                ),
            );
        }
        inner_expression
    }

    // Ensures `"use strict"` is the first statement of a slice of statements.
    // factory.go:448
    pub fn ensure_use_strict(&self, statements: &[P<Node>]) -> Vec<P<Node>> {
        if let Some(&statement) = statements.first()
            && ast::is_prologue_directive(statement)
            && statement.expression().unwrap().text() == "use strict"
        {
            return statements.to_vec();
        }
        let use_strict_prologue =
            self.new_expression_statement(self.new_string_literal("use strict", TokenFlags::None));
        let mut result = Vec::with_capacity(statements.len() + 1);
        result.push(use_strict_prologue);
        result.extend_from_slice(statements);
        result
    }

    // Splits a slice of statements into two parts: standard prologue statements and the rest of the statements
    // factory.go:462
    #[expect(
        clippy::unused_self,
        reason = "The factory keeps the same instance API for node construction, hooks and arena lifecycle operations."
    )]
    pub fn split_standard_prologue<'a>(
        &self,
        source: &'a [P<Node>],
    ) -> (&'a [P<Node>], &'a [P<Node>]) {
        if let Some(result) = source.iter().enumerate().find_map(|(i, &statement)| {
            if ast::is_prologue_directive(statement) {
                None
            } else {
                Some((&source[..i], &source[i..]))
            }
        }) {
            return result;
        }
        (source, &[])
    }

    // Splits a slice of statements into two parts: custom prologue statements (e.g., with `EFCustomPrologue` set) and the rest of the statements
    // factory.go:472
    pub fn split_custom_prologue<'a>(
        &self,
        source: &'a [P<Node>],
    ) -> (&'a [P<Node>], &'a [P<Node>]) {
        if let Some(result) = source.iter().enumerate().find_map(|(i, &statement)| {
            if ast::is_prologue_directive(statement)
                || !self.emit_context().emit_flags(statement).intersects(EmitFlags::CustomPrologue)
            {
                Some((&source[..i], &source[i..]))
            } else {
                None
            }
        }) {
            return result;
        }
        (&[], source)
    }

    //
    // Declaration Names
    //

    // factory.go:496
    fn get_name(
        &self,
        node: Option<P<Node>>,
        mut emit_flags: EmitFlags,
        opts: AssignedNameOptions,
    ) -> P<Node> {
        let mut node_name = None;
        if let Some(node) = node {
            if opts.ignore_assigned_name {
                node_name = ast::get_non_assigned_name_of_declaration(node);
            } else {
                node_name = ast::get_name_of_declaration(node);
            }
        }

        if let Some(node_name) = node_name {
            let name = node_name.clone_node(self.as_node_factory());
            if !opts.allow_comments {
                emit_flags |= EmitFlags::NoComments;
            }
            if !opts.allow_source_maps {
                emit_flags |= EmitFlags::NoSourceMap;
            }
            self.emit_context().add_emit_flags(name, emit_flags);
            return name;
        }

        // Go passes a possibly nil node; NewGeneratedNameForNode dereferences it.
        self.new_generated_name_for_node(node.unwrap())
    }

    // Gets the local name of a declaration. This is primarily used for declarations that can be referred to by name in the
    // declaration's immediate scope (classes, enums, namespaces). A local name will *never* be prefixed with a module or
    // namespace export modifier like "exports." when emitted as an expression.
    // factory.go:524
    pub fn get_local_name(&self, node: P<Node>) -> P<Node> {
        self.get_local_name_ex(node, AssignedNameOptions::default())
    }

    // factory.go:531
    pub fn get_local_name_ex(&self, node: P<Node>, opts: AssignedNameOptions) -> P<Node> {
        self.get_name(Some(node), EmitFlags::LocalName, opts)
    }

    // Gets the export name of a declaration. This is primarily used for declarations that can be
    // referred to by name in the declaration's immediate scope (classes, enums, namespaces). An
    // export name will *always* be prefixed with an module or namespace export modifier like
    // `"exports."` when emitted as an expression if the name points to an exported symbol.
    // factory.go:539
    pub fn get_export_name(&self, node: P<Node>) -> P<Node> {
        self.get_export_name_ex(node, AssignedNameOptions::default())
    }

    // factory.go:547
    fn get_export_name_ex(&self, node: P<Node>, opts: AssignedNameOptions) -> P<Node> {
        self.get_name(Some(node), EmitFlags::ExportName, opts)
    }

    // Gets the name of a declaration to use during emit.
    // factory.go:552
    pub fn get_declaration_name(&self, node: P<Node>) -> P<Node> {
        self.get_declaration_name_ex(node, NameOptions::default())
    }

    // factory.go:557
    pub fn get_declaration_name_ex(&self, node: P<Node>, opts: NameOptions) -> P<Node> {
        self.get_name(
            Some(node),
            EmitFlags::None,
            AssignedNameOptions {
                allow_comments: opts.allow_comments,
                allow_source_maps: opts.allow_source_maps,
                ..Default::default()
            },
        )
    }

    // factory.go:561
    pub fn get_namespace_member_name(
        &self,
        ns: P<Node>,
        mut name: P<Node>,
        opts: NameOptions,
    ) -> P<Node> {
        let ec = self.emit_context();
        if !ec.has_auto_generate_info(Some(name)) {
            name = name.clone_node(self.as_node_factory());
        }
        let qualified_name = self.new_property_access_expression(
            ns,
            None, /*questionDotToken*/
            name,
            NodeFlags::None,
        );
        ec.assign_comment_and_source_map_ranges(qualified_name, name);
        if !opts.allow_comments {
            ec.add_emit_flags(qualified_name, EmitFlags::NoComments);
        }
        if !opts.allow_source_maps {
            ec.add_emit_flags(qualified_name, EmitFlags::NoSourceMap);
        }
        qualified_name
    }

    // Gets the export name of a declaration for use in expressions.
    //
    // An export name will *always* be prefixed with a module or namespace export modifier like
    // `"exports."` when emitted as an expression if the name points to an exported symbol.
    // factory.go:580
    pub fn get_external_module_or_namespace_export_name(
        &self,
        ns: Option<P<Node>>,
        node: P<Node>,
        allow_comments: bool,
        allow_source_maps: bool,
    ) -> P<Node> {
        if let Some(ns) = ns
            && ast::has_syntactic_modifier(node, ModifierFlags::Export)
        {
            let name_opts = NameOptions { allow_comments, allow_source_maps };
            return self.get_namespace_member_name(
                ns,
                self.get_declaration_name_ex(node, name_opts),
                name_opts,
            );
        }
        self.get_export_name_ex(
            node,
            AssignedNameOptions { allow_comments, allow_source_maps, ..Default::default() },
        )
    }

    //
    // Emit Helpers
    //

    // Allocates a new Identifier representing a reference to a helper function.
    // factory.go:593
    pub fn new_unscoped_helper_name(&self, name: &str) -> P<Node> {
        let node = self.new_identifier(alloc_str(name));
        self.emit_context().set_emit_flags(node, EmitFlags::HelperName);
        node
    }

    fn new_helper_call(&self, name: &str, arguments: Vec<P<Node>>) -> P<Node> {
        self.new_call_expression(
            self.new_unscoped_helper_name(name),
            None, /*questionDotToken*/
            None, /*typeArguments*/
            self.new_node_list(arguments),
            NodeFlags::None,
        )
    }

    // TypeScript Helpers

    // factory.go:601
    pub fn new_decorate_helper(
        &self,
        decorator_expressions: Vec<P<Node>>,
        target: P<Node>,
        member_name: Option<P<Node>>,
        descriptor: Option<P<Node>>,
    ) -> P<Node> {
        self.emit_context().request_emit_helper(helper(&DECORATE_HELPER));

        let mut arguments_array = Vec::new();
        arguments_array.push(
            self.new_array_literal_expression(self.new_node_list(decorator_expressions), true),
        );
        arguments_array.push(target);
        if let Some(member_name) = member_name {
            arguments_array.push(member_name);
            if let Some(descriptor) = descriptor {
                arguments_array.push(descriptor);
            }
        }

        self.new_helper_call("__decorate", arguments_array)
    }

    // factory.go:623
    pub fn new_metadata_helper(&self, metadata_key: &str, metadata_value: P<Node>) -> P<Node> {
        self.emit_context().request_emit_helper(helper(&METADATA_HELPER));

        self.new_helper_call(
            "__metadata",
            vec![
                self.new_string_literal(alloc_str(metadata_key), TokenFlags::None),
                metadata_value,
            ],
        )
    }

    // factory.go:638
    pub fn new_param_helper(
        &self,
        expression: P<Node>,
        parameter_offset: usize,
        location: TextRange,
    ) -> P<Node> {
        self.emit_context().request_emit_helper(helper(&PARAM_HELPER));
        let helper = self.new_helper_call(
            "__param",
            vec![
                self.new_numeric_literal(
                    alloc_str(&parameter_offset.to_string()),
                    TokenFlags::None,
                ),
                expression,
            ],
        );
        helper.set_loc(location);
        helper
    }

    // ESNext Helpers

    // factory.go:653
    pub fn new_add_disposable_resource_helper(
        &self,
        env_binding: P<Node>,
        value: P<Node>,
        async_: bool,
    ) -> P<Node> {
        self.emit_context().request_emit_helper(helper(&ADD_DISPOSABLE_RESOURCE_HELPER));
        self.new_helper_call(
            "__addDisposableResource",
            vec![
                env_binding,
                value,
                self.new_keyword_expression(if async_ {
                    Kind::TrueKeyword
                } else {
                    Kind::FalseKeyword
                }),
            ],
        )
    }

    // factory.go:664
    pub fn new_dispose_resources_helper(&self, env_binding: P<Node>) -> P<Node> {
        self.emit_context().request_emit_helper(helper(&DISPOSE_RESOURCES_HELPER));
        self.new_helper_call("__disposeResources", vec![env_binding])
    }

    // Class Fields Helpers

    // factory.go:686
    pub fn new_class_private_field_get_helper(
        &self,
        receiver: P<Node>,
        state: P<Node>,
        kind: PrivateIdentifierKind,
        fn_: Option<P<Node>>,
    ) -> P<Node> {
        self.emit_context().request_emit_helper(helper(&CLASS_PRIVATE_FIELD_GET_HELPER));
        let args = match fn_ {
            None => vec![receiver, state, self.new_string_literal(kind.as_str(), TokenFlags::None)],
            Some(fn_) => {
                vec![receiver, state, self.new_string_literal(kind.as_str(), TokenFlags::None), fn_]
            }
        };
        self.new_helper_call("__classPrivateFieldGet", args)
    }

    // factory.go:703
    pub fn new_class_private_field_set_helper(
        &self,
        receiver: P<Node>,
        state: P<Node>,
        value: P<Node>,
        kind: PrivateIdentifierKind,
        fn_: Option<P<Node>>,
    ) -> P<Node> {
        self.emit_context().request_emit_helper(helper(&CLASS_PRIVATE_FIELD_SET_HELPER));
        let args = match fn_ {
            None => vec![
                receiver,
                state,
                value,
                self.new_string_literal(kind.as_str(), TokenFlags::None),
            ],
            Some(fn_) => vec![
                receiver,
                state,
                value,
                self.new_string_literal(kind.as_str(), TokenFlags::None),
                fn_,
            ],
        };
        self.new_helper_call("__classPrivateFieldSet", args)
    }

    // factory.go:720
    pub fn new_class_private_field_in_helper(&self, state: P<Node>, receiver: P<Node>) -> P<Node> {
        self.emit_context().request_emit_helper(helper(&CLASS_PRIVATE_FIELD_IN_HELPER));
        self.new_helper_call("__classPrivateFieldIn", vec![state, receiver])
    }

    fn new_global_property_call(
        &self,
        object: &'static str,
        method: &'static str,
        arguments: Vec<P<Node>>,
    ) -> P<Node> {
        self.new_call_expression(
            self.new_property_access_expression(
                self.new_identifier(object),
                None,
                self.new_identifier(method),
                NodeFlags::None,
            ),
            None, /*questionDotToken*/
            None, /*typeArguments*/
            self.new_node_list(arguments),
            NodeFlags::None,
        )
    }

    // Creates `Object.defineProperty(target, name, descriptor)`.
    // factory.go:732
    pub fn new_object_define_property_call(
        &self,
        target: P<Node>,
        name: P<Node>,
        descriptor: P<Node>,
    ) -> P<Node> {
        self.new_global_property_call("Object", "defineProperty", vec![target, name, descriptor])
    }

    // Creates `Reflect.get(target, propertyKey, receiver)`.
    // factory.go:748
    pub fn new_reflect_get_call(
        &self,
        target: P<Node>,
        property_key: P<Node>,
        receiver: P<Node>,
    ) -> P<Node> {
        self.new_global_property_call("Reflect", "get", vec![target, property_key, receiver])
    }

    // Creates `Reflect.set(target, propertyKey, value, receiver)`.
    // factory.go:764
    pub fn new_reflect_set_call(
        &self,
        target: P<Node>,
        property_key: P<Node>,
        value: P<Node>,
        receiver: P<Node>,
    ) -> P<Node> {
        self.new_global_property_call("Reflect", "set", vec![target, property_key, value, receiver])
    }

    // Creates `target.bind(thisArg, ...args)`.
    // factory.go:780
    pub fn new_function_bind_call(
        &self,
        target: P<Node>,
        this_arg: P<Node>,
        arguments_list: &[P<Node>],
    ) -> P<Node> {
        let mut args = Vec::with_capacity(1 + arguments_list.len());
        args.push(this_arg);
        args.extend_from_slice(arguments_list);
        self.new_method_call(target, self.new_identifier("bind"), args)
    }

    // Creates `(() => { ...statements })()` — an immediately invoked arrow function.
    // factory.go:788
    pub fn new_immediately_invoked_arrow_function(&self, statements: Vec<P<Node>>) -> P<Node> {
        let arrow = self.new_arrow_function(
            None,                                               /*modifiers*/
            None,                                               /*typeParameters*/
            Some(self.new_node_list(vec![])),                   /*parameters*/
            None,                                               /*returnType*/
            None,                                               /*fullSignature*/
            Some(self.new_token(Kind::EqualsGreaterThanToken)), /*equalsGreaterThanToken*/
            Some(self.new_block(self.new_node_list(statements), true)),
        );
        self.new_call_expression(
            self.new_parenthesized_expression(arrow),
            None, /*questionDotToken*/
            None, /*typeArguments*/
            self.new_node_list(vec![]),
            NodeFlags::None,
        )
    }

    // Creates `export default <expression>;`.
    // factory.go:808
    pub fn new_export_default(&self, expression: P<Node>) -> P<Node> {
        self.new_export_assignment(None, false, None, expression)
    }

    // Creates `export { <name> };`.
    // factory.go:813
    pub fn new_external_module_export(&self, name: P<Node>) -> P<Node> {
        let specifier = self.new_export_specifier(false, None, name);
        let named_exports = self.new_named_exports(self.new_node_list(vec![specifier]));
        self.new_export_declaration(None, false, Some(named_exports), None, None)
    }

    // ES2018 Helpers
    // Chains a sequence of expressions using the __assign helper or Object.assign if available in the target
    // factory.go:821
    pub fn new_assign_helper(
        &self,
        attributes_segments: Vec<P<Node>>,
        _script_target: ScriptTarget,
    ) -> P<Node> {
        self.new_call_expression(
            self.new_property_access_expression(
                self.new_identifier("Object"),
                None,
                self.new_identifier("assign"),
                NodeFlags::None,
            ),
            None,
            None,
            self.new_node_list(attributes_segments),
            NodeFlags::None,
        )
    }

    // ES2018 Destructuring Helpers

    // factory.go:827
    pub fn new_rest_helper(
        &self,
        value: P<Node>,
        elements: &[P<Node>],
        computed_temp_variables: Option<&[P<Node>]>,
        location: TextRange,
    ) -> P<Node> {
        self.emit_context().request_emit_helper(helper(&REST_HELPER));
        let mut property_names = Vec::new();
        let mut computed_temp_variable_offset = 0;
        for (i, &element) in elements.iter().enumerate() {
            if i == elements.len() - 1 {
                break;
            }
            let property_name =
                ast::try_get_property_name_of_binding_or_assignment_element(element);
            if let Some(property_name) = property_name {
                if ast::is_computed_property_name(property_name) {
                    assert!(
                        computed_temp_variables.is_some(),
                        "Encountered computed property name but 'computedTempVariables' argument was not provided."
                    );
                    let temp = computed_temp_variables.unwrap()[computed_temp_variable_offset];
                    computed_temp_variable_offset += 1;
                    // typeof _tmp === "symbol" ? _tmp : _tmp + ""
                    property_names.push(self.new_conditional_expression(
                        self.new_type_check(temp, "symbol"),
                        self.new_token(Kind::QuestionToken),
                        temp,
                        self.new_token(Kind::ColonToken),
                        self.new_binary_expression(
                            None,
                            temp,
                            None,
                            self.new_token(Kind::PlusToken),
                            self.new_string_literal("", TokenFlags::None),
                        ),
                    ));
                } else {
                    property_names.push(self.new_string_literal_from_node(property_name));
                }
            }
        }
        let prop_names =
            self.new_array_literal_expression(self.new_node_list(property_names), false);
        prop_names.set_loc(location);
        self.new_call_expression(
            self.new_unscoped_helper_name("__rest"),
            None,
            None,
            self.new_node_list(vec![value, prop_names]),
            NodeFlags::None,
        )
    }

    // ES2018 Helpers

    // Allocates a new Call expression to the `__await` helper.
    // factory.go:871
    pub fn new_await_helper(&self, expression: P<Node>) -> P<Node> {
        self.emit_context().request_emit_helper(helper(&AWAIT_HELPER));
        self.new_helper_call("__await", vec![expression])
    }

    // Allocates a new Call expression to the `__asyncGenerator` helper.
    // factory.go:883
    pub fn new_async_generator_helper(
        &self,
        generator_func: P<Node>,
        has_lexical_this: bool,
    ) -> P<Node> {
        let ec = self.emit_context();
        ec.request_emit_helper(helper(&AWAIT_HELPER));
        ec.request_emit_helper(helper(&ASYNC_GENERATOR_HELPER));

        // Mark this node as originally an async function body
        ec.add_emit_flags(
            generator_func,
            EmitFlags::AsyncFunctionBody | EmitFlags::ReuseTempVariableScope,
        );

        let this_arg = if has_lexical_this {
            self.new_keyword_expression(Kind::ThisKeyword)
        } else {
            self.new_void_zero_expression()
        };

        self.new_helper_call(
            "__asyncGenerator",
            vec![this_arg, self.new_identifier("arguments"), generator_func],
        )
    }

    // Allocates a new Call expression to the `__asyncDelegator` helper.
    // factory.go:914
    pub fn new_async_delegator_helper(&self, expression: P<Node>) -> P<Node> {
        self.emit_context().request_emit_helper(helper(&AWAIT_HELPER));
        self.emit_context().request_emit_helper(helper(&ASYNC_DELEGATOR_HELPER));
        self.new_helper_call("__asyncDelegator", vec![expression])
    }

    // Allocates a new Call expression to the `__asyncValues` helper.
    // factory.go:927
    pub fn new_async_values_helper(&self, expression: P<Node>) -> P<Node> {
        self.emit_context().request_emit_helper(helper(&ASYNC_VALUES_HELPER));
        self.new_helper_call("__asyncValues", vec![expression])
    }

    // !!! ES2017 Helpers

    // Allocates a new Call expression to the `__awaiter` helper.
    // factory.go:941
    pub fn new_awaiter_helper(
        &self,
        has_lexical_this: bool,
        arguments_expression: Option<P<Node>>,
        parameters: Option<P<NodeList>>,
        body: P<Node>,
    ) -> P<Node> {
        let ec = self.emit_context();
        ec.request_emit_helper(helper(&AWAITER_HELPER));

        let params = match parameters {
            Some(parameters) => parameters,
            None => self.new_node_list(vec![]),
        };

        let generator_func = self.new_function_expression(
            None, /*modifiers*/
            Some(self.new_token(Kind::AsteriskToken)),
            None, /*name*/
            None, /*typeParameters*/
            Some(params),
            None, /*returnType*/
            None, /*fullSignature*/
            Some(body),
        );

        // Mark this node as originally an async function body
        ec.add_emit_flags(
            generator_func,
            EmitFlags::AsyncFunctionBody | EmitFlags::ReuseTempVariableScope,
        );

        let this_arg = if has_lexical_this {
            self.new_keyword_expression(Kind::ThisKeyword)
        } else {
            self.new_void_zero_expression()
        };

        let args_arg = match arguments_expression {
            Some(arguments_expression) => arguments_expression,
            None => self.new_void_zero_expression(),
        };

        self.new_helper_call(
            "__awaiter",
            vec![this_arg, args_arg, self.new_void_zero_expression(), generator_func],
        )
    }

    // ES Decorator Helpers

    // factory.go:1000
    pub fn new_es_decorate_class_context_object(
        &self,
        name_expr: P<Node>,
        metadata: P<Node>,
    ) -> P<Node> {
        let props = vec![
            self.new_property_assignment(
                None,
                self.new_identifier("kind"),
                None,
                None,
                self.new_string_literal("class", TokenFlags::None),
            ),
            self.new_property_assignment(None, self.new_identifier("name"), None, None, name_expr),
            self.new_property_assignment(
                None,
                self.new_identifier("metadata"),
                None,
                None,
                metadata,
            ),
        ];
        self.new_object_literal_expression(self.new_node_list(props), false)
    }

    // factory.go:1009
    fn new_es_decorate_class_element_access_get_method(
        &self,
        name_computed: bool,
        name_expr: P<Node>,
    ) -> P<Node> {
        let accessor = if name_computed {
            self.new_element_access_expression(
                self.new_identifier("obj"),
                None,
                name_expr,
                NodeFlags::None,
            )
        } else {
            self.new_property_access_expression(
                self.new_identifier("obj"),
                None,
                name_expr,
                NodeFlags::None,
            )
        };

        let obj_param = self.new_parameter_declaration(
            None,
            None,
            self.new_identifier("obj"),
            None,
            None,
            None,
        );

        let arrow = self.new_arrow_function(
            None,
            None,
            Some(self.new_node_list(vec![obj_param])),
            None,
            None,
            Some(self.new_token(Kind::EqualsGreaterThanToken)),
            Some(accessor),
        );

        self.new_property_assignment(None, self.new_identifier("get"), None, None, arrow)
    }

    // factory.go:1033
    fn new_es_decorate_class_element_access_set_method(
        &self,
        name_computed: bool,
        name_expr: P<Node>,
    ) -> P<Node> {
        let accessor = if name_computed {
            self.new_element_access_expression(
                self.new_identifier("obj"),
                None,
                name_expr,
                NodeFlags::None,
            )
        } else {
            self.new_property_access_expression(
                self.new_identifier("obj"),
                None,
                name_expr,
                NodeFlags::None,
            )
        };

        let assignment = self.new_assignment_expression(accessor, self.new_identifier("value"));
        let stmt = self.new_expression_statement(assignment);
        let body = self.new_block(self.new_node_list(vec![stmt]), false);

        let obj_param = self.new_parameter_declaration(
            None,
            None,
            self.new_identifier("obj"),
            None,
            None,
            None,
        );
        let value_param = self.new_parameter_declaration(
            None,
            None,
            self.new_identifier("value"),
            None,
            None,
            None,
        );

        let arrow = self.new_arrow_function(
            None,
            None,
            Some(self.new_node_list(vec![obj_param, value_param])),
            None,
            None,
            Some(self.new_token(Kind::EqualsGreaterThanToken)),
            Some(body),
        );

        self.new_property_assignment(None, self.new_identifier("set"), None, None, arrow)
    }

    // factory.go:1062
    fn new_es_decorate_class_element_access_has_method(
        &self,
        name_computed: bool,
        name_expr: Option<P<Node>>,
    ) -> P<Node> {
        // The property name for the "in" expression
        let property_name = match name_expr {
            Some(name_expr) if !name_computed && ast::is_identifier(name_expr) => {
                self.new_string_literal_from_node(name_expr)
            }
            _ => name_expr.unwrap(),
        };

        let obj_param = self.new_parameter_declaration(
            None,
            None,
            self.new_identifier("obj"),
            None,
            None,
            None,
        );
        let in_expr = self.new_binary_expression(
            None,
            property_name,
            None,
            self.new_token(Kind::InKeyword),
            self.new_identifier("obj"),
        );

        let arrow = self.new_arrow_function(
            None,
            None,
            Some(self.new_node_list(vec![obj_param])),
            None,
            None,
            Some(self.new_token(Kind::EqualsGreaterThanToken)),
            Some(in_expr),
        );

        self.new_property_assignment(None, self.new_identifier("has"), None, None, arrow)
    }

    // Creates the "access" object for a class element decorator context.
    //
    // 15.7.3 CreateDecoratorAccessObject (kind, name)
    //
    //  2. If _kind_ is ~field~, ~method~, ~accessor~, or ~getter~, then
    //     a. Let _getAccess_ be a new Abstract Closure with parameters (_object_) that captures _kind_ and _name_ ...
    //     b. Perform ! CreateDataPropertyOrThrow(_access_, "get", _getAccess_).
    //  3. If _kind_ is ~field~, ~accessor~, or ~setter~, then
    //     a. Let _setAccess_ be a new Abstract Closure with parameters (_object_, _value_) that captures _kind_ and _name_ ...
    //     b. Perform ! CreateDataPropertyOrThrow(_access_, "set", _setAccess_).
    // factory.go:1098
    fn new_es_decorate_class_element_access_object(
        &self,
        name_computed: bool,
        name_expr: P<Node>,
        has_get: bool,
        has_set: bool,
    ) -> P<Node> {
        let mut access_props = Vec::new();

        // "has" method: obj => name in obj
        access_props.push(
            self.new_es_decorate_class_element_access_has_method(name_computed, Some(name_expr)),
        );

        // "get" method: obj => obj.name or obj => obj[name]
        if has_get {
            access_props.push(
                self.new_es_decorate_class_element_access_get_method(name_computed, name_expr),
            );
        }

        // "set" method: (obj, value) => { obj.name = value; } or (obj, value) => { obj[name] = value; }
        if has_set {
            access_props.push(
                self.new_es_decorate_class_element_access_set_method(name_computed, name_expr),
            );
        }

        self.new_object_literal_expression(self.new_node_list(access_props), false)
    }

    // factory.go:1122
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "The boolean parameters preserve the upstream Go function signature."
    )]
    pub fn new_es_decorate_class_element_context_object(
        &self,
        kind: &str,
        name_computed: bool,
        name_expr: P<Node>,
        is_static: bool,
        is_private: bool,
        has_get: bool,
        has_set: bool,
        metadata: P<Node>,
    ) -> P<Node> {
        // Build the name value for the context's "name" property
        let name_value = if !name_computed
            && (ast::is_private_identifier(name_expr) || ast::is_identifier(name_expr))
        {
            self.new_string_literal_from_node(name_expr)
        } else {
            name_expr
        };

        // Build the access object with has/get/set arrow functions
        let access_obj = self.new_es_decorate_class_element_access_object(
            name_computed,
            name_expr,
            has_get,
            has_set,
        );

        let static_expr =
            if is_static { self.new_true_expression() } else { self.new_false_expression() };

        let private_expr =
            if is_private { self.new_true_expression() } else { self.new_false_expression() };

        let props = vec![
            self.new_property_assignment(
                None,
                self.new_identifier("kind"),
                None,
                None,
                self.new_string_literal(alloc_str(kind), TokenFlags::None),
            ),
            self.new_property_assignment(None, self.new_identifier("name"), None, None, name_value),
            self.new_property_assignment(
                None,
                self.new_identifier("static"),
                None,
                None,
                static_expr,
            ),
            self.new_property_assignment(
                None,
                self.new_identifier("private"),
                None,
                None,
                private_expr,
            ),
            self.new_property_assignment(
                None,
                self.new_identifier("access"),
                None,
                None,
                access_obj,
            ),
            self.new_property_assignment(
                None,
                self.new_identifier("metadata"),
                None,
                None,
                metadata,
            ),
        ];
        self.new_object_literal_expression(self.new_node_list(props), false)
    }

    // factory.go:1168
    pub fn new_es_decorate_helper(
        &self,
        ctor: P<Node>,
        descriptor_in: P<Node>,
        decorators: P<Node>,
        context_in: P<Node>,
        initializers: P<Node>,
        extra_initializers: P<Node>,
    ) -> P<Node> {
        self.emit_context().request_emit_helper(helper(&ES_DECORATE_HELPER));
        self.new_helper_call(
            "__esDecorate",
            vec![ctor, descriptor_in, decorators, context_in, initializers, extra_initializers],
        )
    }

    // factory.go:1179
    pub fn new_run_initializers_helper(
        &self,
        this_arg: P<Node>,
        initializers: P<Node>,
        value: Option<P<Node>>,
    ) -> P<Node> {
        self.emit_context().request_emit_helper(helper(&RUN_INITIALIZERS_HELPER));
        let arguments = match value {
            Some(value) => vec![this_arg, initializers, value],
            None => vec![this_arg, initializers],
        };
        self.new_helper_call("__runInitializers", arguments)
    }

    // ES2015 Helpers

    // factory.go:1198
    pub fn new_template_object_helper(&self, cooked_array: P<Node>, raw_array: P<Node>) -> P<Node> {
        self.emit_context().request_emit_helper(helper(&MAKE_TEMPLATE_OBJECT_HELPER));
        self.new_helper_call("__makeTemplateObject", vec![cooked_array, raw_array])
    }

    // factory.go:1209
    pub fn new_prop_key_helper(&self, expr: P<Node>) -> P<Node> {
        self.emit_context().request_emit_helper(helper(&PROP_KEY_HELPER));
        self.new_helper_call("__propKey", vec![expr])
    }

    // factory.go:1220
    pub fn new_set_function_name_helper(
        &self,
        fn_: P<Node>,
        name: P<Node>,
        prefix: &str,
    ) -> P<Node> {
        self.emit_context().request_emit_helper(helper(&SET_FUNCTION_NAME_HELPER));
        let arguments = if prefix.is_empty() {
            vec![fn_, name]
        } else {
            vec![fn_, name, self.new_string_literal(alloc_str(prefix), TokenFlags::None)]
        };
        self.new_helper_call("__setFunctionName", arguments)
    }

    // ES Module Helpers

    // Allocates a new Call expression to the `__importDefault` helper.
    // factory.go:1240
    pub fn new_import_default_helper(&self, expression: P<Node>) -> P<Node> {
        self.emit_context().request_emit_helper(helper(&IMPORT_DEFAULT_HELPER));
        self.new_helper_call("__importDefault", vec![expression])
    }

    // Allocates a new Call expression to the `__importStar` helper.
    // factory.go:1252
    pub fn new_import_star_helper(&self, expression: P<Node>) -> P<Node> {
        self.emit_context().request_emit_helper(helper(&IMPORT_STAR_HELPER));
        self.new_helper_call("__importStar", vec![expression])
    }

    // Allocates a new Call expression to the `__exportStar` helper.
    // factory.go:1264
    pub fn new_export_star_helper(
        &self,
        module_expression: P<Node>,
        exports_expression: P<Node>,
    ) -> P<Node> {
        self.emit_context().request_emit_helper(helper(&EXPORT_STAR_HELPER));
        self.new_helper_call("__exportStar", vec![module_expression, exports_expression])
    }

    // factory.go:1275
    pub fn new_assignment_target_wrapper(
        &self,
        param_name: P<Node>,
        expression: P<Node>,
    ) -> P<Node> {
        let set_accessor = self.new_set_accessor_declaration(
            None, /*modifiers*/
            self.new_identifier("value"),
            None, /*typeParameters*/
            Some(self.new_node_list(vec![
                self.new_parameter_declaration(None, None, param_name, None, None, None),
            ])),
            None, /*returnType*/
            None, /*fullSignature*/
            Some(self.new_block(
                self.new_node_list(vec![self.new_expression_statement(expression)]),
                false,
            )),
        );
        let obj_literal =
            self.new_object_literal_expression(self.new_node_list(vec![set_accessor]), false);
        // Explicit parens required because of v8 regression (https://bugs.chromium.org/p/v8/issues/detail?id=9560)
        self.new_property_access_expression(
            self.new_parenthesized_expression(obj_literal),
            None, /*questionDotToken*/
            self.new_identifier("value"),
            NodeFlags::None,
        )
    }

    // Allocates a new Call expression to the `__rewriteRelativeImportExtension` helper.
    // factory.go:1300
    pub fn new_rewrite_relative_import_extensions_helper(
        &self,
        first_argument: P<Node>,
        preserve_jsx: bool,
        extra_extensions: &[tspath::ExtensionRewrite],
        use_case_sensitive_file_names: bool,
    ) -> P<Node> {
        self.emit_context().request_emit_helper(helper(&REWRITE_RELATIVE_IMPORT_EXTENSIONS_HELPER));
        let mut arguments = vec![first_argument];
        if extra_extensions.is_empty() {
            if preserve_jsx {
                arguments.push(self.new_token(Kind::TrueKeyword));
            }
            return self.new_helper_call("__rewriteRelativeImportExtension", arguments);
        }

        arguments.push(self.new_token(if preserve_jsx {
            Kind::TrueKeyword
        } else {
            Kind::FalseKeyword
        }));
        let properties = extra_extensions
            .iter()
            .map(|extension| {
                self.new_property_assignment(
                    None,
                    self.new_string_literal(alloc_str(&extension.source), TokenFlags::None),
                    None,
                    None,
                    self.new_string_literal(alloc_str(&extension.target), TokenFlags::None),
                )
            })
            .collect();
        arguments.push(self.new_object_literal_expression(self.new_node_list(properties), false));
        if !use_case_sensitive_file_names {
            arguments.push(self.new_token(Kind::TrueKeyword));
        }
        self.new_helper_call("__rewriteRelativeImportExtension", arguments)
    }
}
