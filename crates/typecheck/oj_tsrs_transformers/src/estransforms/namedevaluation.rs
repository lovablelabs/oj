// Port of estransforms/namedevaluation.go.

use super::*;

// namedevaluation.go:16
/**
 * Gets whether a node is a `static {}` block containing only a single call to the `__setFunctionName` helper where that
 * call's second argument is the value stored in the `assignedName` property of the block's `EmitNode`.
 * @internal
 */
pub fn is_class_named_evaluation_helper_block(emit_context: P<EmitContext>, node: P<Node>) -> bool {
    if !ast::is_class_static_block_declaration(node)
        || node.as_class_static_block_declaration().body.statements().len() != 1
    {
        return false;
    }

    let statement = node.as_class_static_block_declaration().body.statements()[0];
    if ast::is_expression_statement(statement) {
        let expression = statement.expression().unwrap();
        if emit_context.is_call_to_helper(expression, "__setFunctionName") {
            let arguments = expression.as_call_expression().arguments;
            return arguments.nodes().len() >= 2
                && Some(arguments.nodes()[1]) == emit_context.assigned_name(node);
        }
    }
    false
}

// namedevaluation.go:38
/**
 * Gets whether a `ClassLikeDeclaration` has a `static {}` block containing only a single call to the
 * `__setFunctionName` helper.
 * @internal
 */
pub fn class_has_explicitly_assigned_name(emit_context: P<EmitContext>, node: P<Node>) -> bool {
    if emit_context.assigned_name(node).is_some()
        && node
            .members()
            .iter()
            .any(|&member| is_class_named_evaluation_helper_block(emit_context, member))
    {
        return true;
    }
    false
}

// namedevaluation.go:54
/**
 * Gets whether a `ClassLikeDeclaration` has a declared name or contains a `static {}` block containing only a single
 * call to the `__setFunctionName` helper.
 * @internal
 */
pub fn class_has_declared_or_explicitly_assigned_name(
    emit_context: P<EmitContext>,
    node: P<Node>,
) -> bool {
    node.name().is_some() || class_has_explicitly_assigned_name(emit_context, node)
}

// namedevaluation.go:63
// Indicates whether an expression is an anonymous function definition.
//
// See https://tc39.es/ecma262/#sec-isanonymousfunctiondefinition
pub fn is_anonymous_function_definition(
    emit_context: P<EmitContext>,
    node: P<Node>,
    cb: Option<&dyn Fn(P<Node>) -> bool>,
) -> bool {
    let node = ast::skip_outer_expressions(node, OEK::All);
    match node.kind() {
        Kind::ClassExpression => {
            if class_has_declared_or_explicitly_assigned_name(emit_context, node) {
                return false;
            }
        }
        Kind::FunctionExpression => {
            if node.as_function_expression().name().is_some() {
                return false;
            }
        }
        Kind::ArrowFunction => {
            // arrow functions are always anonymous
        }
        _ => return false,
    }
    if let Some(cb) = cb {
        return cb(node);
    }
    true
}

// namedevaluation.go:85
pub fn is_named_evaluation(emit_context: P<EmitContext>, node: P<Node>) -> bool {
    is_named_evaluation_and(emit_context, node, None)
}

// namedevaluation.go:89
pub fn is_named_evaluation_and(
    emit_context: P<EmitContext>,
    node: P<Node>,
    cb: Option<&dyn Fn(P<Node>) -> bool>,
) -> bool {
    if !ast::is_named_evaluation_source(node) {
        return false;
    }
    match node.kind() {
        Kind::ShorthandPropertyAssignment => is_anonymous_function_definition(
            emit_context,
            node.as_shorthand_property_assignment().object_assignment_initializer().unwrap(),
            cb,
        ),
        Kind::PropertyAssignment
        | Kind::VariableDeclaration
        | Kind::Parameter
        | Kind::BindingElement
        | Kind::PropertyDeclaration => {
            is_anonymous_function_definition(emit_context, node.initializer().unwrap(), cb)
        }
        Kind::BinaryExpression => {
            is_anonymous_function_definition(emit_context, node.as_binary_expression().right(), cb)
        }
        Kind::ExportAssignment => {
            is_anonymous_function_definition(emit_context, node.expression().unwrap(), cb)
        }
        _ => panic!("Unhandled case in isNamedEvaluation"),
    }
}

// namedevaluation.go:109
// Gets a string literal to use as the assigned name of an anonymous class or function declaration.
pub fn get_assigned_name_of_identifier(
    emit_context: P<EmitContext>,
    name: P<Node>,
    expression: P<Node>, /*WrappedExpression<AnonymousFunctionDefinition>*/
) -> P<Node> {
    let original = emit_context
        .most_original(Some(ast::skip_outer_expressions(expression, OEK::All)))
        .unwrap();
    if (ast::is_class_declaration(original) || ast::is_function_declaration(original))
        && original.name().is_none()
        && ast::has_syntactic_modifier(original, ModifierFlags::Default)
    {
        return emit_context.factory.new_string_literal("default", TokenFlags::None);
    }
    emit_context.factory.new_string_literal_from_node(name)
}

// namedevaluation.go:118
pub fn get_assigned_name_of_property_name(
    emit_context: P<EmitContext>,
    name: P<Node>,
    assigned_name_text: &str,
) -> (P<Node>, P<Node>) {
    let factory = &emit_context.factory;
    if !assigned_name_text.is_empty() {
        let assigned_name =
            factory.new_string_literal(alloc_str(assigned_name_text), TokenFlags::None);
        return (assigned_name, name);
    }

    if ast::is_property_name_literal(name) || ast::is_private_identifier(name) {
        let assigned_name = factory.new_string_literal_from_node(name);
        return (assigned_name, name);
    }

    let expression = name.expression().unwrap();
    if ast::is_property_name_literal(expression) && !ast::is_identifier(expression) {
        let assigned_name = factory.new_string_literal_from_node(expression);
        return (assigned_name, name);
    }

    assert!(ast::is_computed_property_name(name), "Expected computed property name");

    let assigned_name = factory.new_generated_name_for_node(name);
    emit_context.add_variable_declaration(assigned_name);

    let key = factory.new_prop_key_helper(expression);
    let assignment = factory.new_assignment_expression(assigned_name, key);
    let updated_name = factory.update_computed_property_name(name, assignment);
    (assigned_name, updated_name)
}

// namedevaluation.go:153
// Creates a class `static {}` block used to dynamically set the name of a class.
//
// The assignedName parameter is the expression used to resolve the assigned name at runtime. This expression should not produce
// side effects.
// The thisExpression parameter overrides the expression to use for the actual `this` reference. This can be used to provide an
// expression that has already had its `EmitFlags` set or may have been tracked to prevent substitution.
pub fn create_class_named_evaluation_helper_block(
    emit_context: P<EmitContext>,
    assigned_name: P<Node>,
    this_expression: Option<P<Node>>,
) -> P<Node> {
    // produces:
    //
    //  static { __setFunctionName(this, "C"); }
    //

    let this_expression =
        this_expression.unwrap_or_else(|| emit_context.factory.new_this_expression());

    let factory = &emit_context.factory;
    let expression =
        factory.new_set_function_name_helper(this_expression, assigned_name, "" /*prefix*/);
    let statement = factory.new_expression_statement(expression);
    let body = factory.new_block(factory.new_node_list(vec![statement]), false /*multiLine*/);
    let block = factory.new_class_static_block_declaration(None /*modifiers*/, body);

    // We use `emitNode.assignedName` to indicate this is a NamedEvaluation helper block
    // and to stash the expression used to resolve the assigned name.
    emit_context.set_assigned_name(block, assigned_name);
    block
}

// namedevaluation.go:176
// Injects a class `static {}` block used to dynamically set the name of a class, if one does not already exist.
pub fn inject_class_named_evaluation_helper_block_if_missing(
    emit_context: P<EmitContext>,
    node: P<Node>,
    assigned_name: P<Node>,
    this_expression: Option<P<Node>>,
) -> P<Node> {
    // given:
    //
    //  let C = class {
    //  };
    //
    // produces:
    //
    //  let C = class {
    //      static { __setFunctionName(this, "C"); }
    //  };

    // NOTE: If the class has a `_classThis` assignment block, this helper will be injected after that block.

    if class_has_explicitly_assigned_name(emit_context, node) {
        return node;
    }

    let factory = &emit_context.factory;
    let named_evaluation_block =
        create_class_named_evaluation_helper_block(emit_context, assigned_name, this_expression);
    if let Some(name) = node.name() {
        emit_context.set_source_map_range(
            named_evaluation_block.body().unwrap().statements()[0],
            name.loc(),
        );
    }

    let insertion_index = node
        .members()
        .iter()
        .position(|&n| is_class_this_assignment_block(emit_context, n))
        .map_or(0, |index| index + 1);
    let leading = &node.members()[..insertion_index];
    let trailing = &node.members()[insertion_index..];

    let mut members: Vec<P<Node>> = Vec::new();
    members.extend_from_slice(leading);
    members.push(named_evaluation_block);
    members.extend_from_slice(trailing);
    let members_list = factory.new_node_list(members);
    members_list.loc.set(node.member_list().unwrap().loc.get());

    let old_node = node;
    let node = if ast::is_class_declaration(node) {
        factory.update_class_declaration(
            node,
            node.modifiers(),
            node.name(),
            node.type_parameter_list(),
            node.as_class_declaration().heritage_clauses(),
            members_list,
        )
    } else {
        factory.update_class_expression(
            node,
            node.modifiers(),
            node.name(),
            node.type_parameter_list(),
            node.as_class_expression().heritage_clauses(),
            members_list,
        )
    };

    emit_context.set_assigned_name(node, assigned_name);

    // Transfer ClassThis from old to new node, since UpdateClassExpression creates
    // a new node that won't have ClassThis set on it.
    if let Some(ct) = emit_context.class_this(old_node) {
        emit_context.set_class_this(node, ct);
    }

    node
}

// namedevaluation.go:250
pub fn finish_transform_named_evaluation(
    emit_context: P<EmitContext>,
    expression: P<Node>, // WrappedExpression<AnonymousFunctionDefinition>,
    assigned_name: P<Node>,
    ignore_empty_string_literal: bool,
) -> P<Node> {
    if ignore_empty_string_literal
        && ast::is_string_literal(assigned_name)
        && assigned_name.text().is_empty()
    {
        return expression;
    }

    let factory = &emit_context.factory;
    let inner_expression = ast::skip_outer_expressions(expression, OEK::All);

    let updated_expression = if ast::is_class_expression(inner_expression) {
        inject_class_named_evaluation_helper_block_if_missing(
            emit_context,
            inner_expression,
            assigned_name,
            None, /*thisExpression*/
        )
    } else {
        factory.new_set_function_name_helper(inner_expression, assigned_name, "" /*prefix*/)
    };

    factory.restore_outer_expressions(Some(expression), updated_expression, OEK::All)
}

// namedevaluation.go:273
pub fn transform_named_evaluation_of_property_assignment(
    context: P<EmitContext>,
    node: P<Node>, /*NamedEvaluation & PropertyAssignment*/
    ignore_empty_string_literal: bool,
    assigned_name_text: &str,
) -> P<Node> {
    // 13.2.5.5 RS: PropertyDefinitionEvaluation
    //   PropertyAssignment : PropertyName `:` AssignmentExpression
    //     ...
    //     5. If IsAnonymousFunctionDefinition(|AssignmentExpression|) is *true* and _isProtoSetter_ is *false*, then
    //        a. Let _popValue_ be ? NamedEvaluation of |AssignmentExpression| with argument _propKey_.
    //     ...

    let factory = &context.factory;
    let (assigned_name, name) =
        get_assigned_name_of_property_name(context, node.name().unwrap(), assigned_name_text);
    let initializer = finish_transform_named_evaluation(
        context,
        node.initializer().unwrap(),
        assigned_name,
        ignore_empty_string_literal,
    );
    factory.update_property_assignment(
        node,
        None, /*modifiers*/
        name,
        None, /*postfixToken*/
        None, /*typeNode*/
        initializer,
    )
}

// namedevaluation.go:287
pub fn transform_named_evaluation_of_shorthand_assignment_property(
    emit_context: P<EmitContext>,
    node: P<Node>, /*NamedEvaluation & ShorthandPropertyAssignment*/
    ignore_empty_string_literal: bool,
    assigned_name_text: &str,
) -> P<Node> {
    // 13.15.5.3 RS: PropertyDestructuringAssignmentEvaluation
    //   AssignmentProperty : IdentifierReference Initializer?
    //     ...
    //     4. If |Initializer?| is present and _v_ is *undefined*, then
    //        a. If IsAnonymousFunctionDefinition(|Initializer|) is *true*, then
    //           i. Set _v_ to ? NamedEvaluation of |Initializer| with argument _P_.
    //     ...

    let factory = &emit_context.factory;
    let s = node.as_shorthand_property_assignment();
    let object_assignment_initializer_in = s.object_assignment_initializer().unwrap();
    let assigned_name = if assigned_name_text.is_empty() {
        get_assigned_name_of_identifier(
            emit_context,
            node.name().unwrap(),
            object_assignment_initializer_in,
        )
    } else {
        factory.new_string_literal(alloc_str(assigned_name_text), TokenFlags::None)
    };
    let object_assignment_initializer = finish_transform_named_evaluation(
        emit_context,
        object_assignment_initializer_in,
        assigned_name,
        ignore_empty_string_literal,
    );
    factory.update_shorthand_property_assignment(
        node,
        None, /*modifiers*/
        node.name().unwrap(),
        None, /*postfixToken*/
        None, /*typeNode*/
        s.equals_token,
        Some(object_assignment_initializer),
    )
}

// namedevaluation.go:315
pub fn transform_named_evaluation_of_variable_declaration(
    emit_context: P<EmitContext>,
    node: P<Node>, /*NamedEvaluation & VariableDeclaration*/
    ignore_empty_string_literal: bool,
    assigned_name_text: &str,
) -> P<Node> {
    // 14.3.1.2 RS: Evaluation
    //   LexicalBinding : BindingIdentifier Initializer
    //     ...
    //     3. If IsAnonymousFunctionDefinition(|Initializer|) is *true*, then
    //        a. Let _value_ be ? NamedEvaluation of |Initializer| with argument _bindingId_.
    //     ...
    //
    // 14.3.2.1 RS: Evaluation
    //   VariableDeclaration : BindingIdentifier Initializer
    //     ...
    //     3. If IsAnonymousFunctionDefinition(|Initializer|) is *true*, then
    //        a. Let _value_ be ? NamedEvaluation of |Initializer| with argument _bindingId_.
    //     ...

    let factory = &emit_context.factory;
    let assigned_name = if assigned_name_text.is_empty() {
        get_assigned_name_of_identifier(
            emit_context,
            node.name().unwrap(),
            node.initializer().unwrap(),
        )
    } else {
        factory.new_string_literal(alloc_str(assigned_name_text), TokenFlags::None)
    };
    let initializer = finish_transform_named_evaluation(
        emit_context,
        node.initializer().unwrap(),
        assigned_name,
        ignore_empty_string_literal,
    );
    factory.update_variable_declaration(
        node,
        node.name().unwrap(),
        None, /*exclamationToken*/
        None, /*typeNode*/
        Some(initializer),
    )
}

// namedevaluation.go:347
pub fn transform_named_evaluation_of_parameter_declaration(
    emit_context: P<EmitContext>,
    node: P<Node>, /*NamedEvaluation & ParameterDeclaration*/
    ignore_empty_string_literal: bool,
    assigned_name_text: &str,
) -> P<Node> {
    // 8.6.3 RS: IteratorBindingInitialization
    //   SingleNameBinding : BindingIdentifier Initializer?
    //     ...
    //     5. If |Initializer| is present and _v_ is *undefined*, then
    //        a. If IsAnonymousFunctionDefinition(|Initializer|) is *true*, then
    //           i. Set _v_ to ? NamedEvaluation of |Initializer| with argument _bindingId_.
    //     ...
    //
    // 14.3.3.3 RS: KeyedBindingInitialization
    //   SingleNameBinding : BindingIdentifier Initializer?
    //     ...
    //     4. If |Initializer| is present and _v_ is *undefined*, then
    //        a. If IsAnonymousFunctionDefinition(|Initializer|) is *true*, then
    //           i. Set _v_ to ? NamedEvaluation of |Initializer| with argument _bindingId_.
    //     ...

    let factory = &emit_context.factory;
    let assigned_name = if assigned_name_text.is_empty() {
        get_assigned_name_of_identifier(
            emit_context,
            node.name().unwrap(),
            node.initializer().unwrap(),
        )
    } else {
        factory.new_string_literal(alloc_str(assigned_name_text), TokenFlags::None)
    };
    let initializer = finish_transform_named_evaluation(
        emit_context,
        node.initializer().unwrap(),
        assigned_name,
        ignore_empty_string_literal,
    );
    factory.update_parameter_declaration(
        node,
        None, /*modifiers*/
        node.as_parameter_declaration().dot_dot_dot_token(),
        node.name().unwrap(),
        None, /*questionToken*/
        None, /*typeNode*/
        Some(initializer),
    )
}

// namedevaluation.go:383
pub fn transform_named_evaluation_of_binding_element(
    emit_context: P<EmitContext>,
    node: P<Node>, /*NamedEvaluation & BindingElement*/
    ignore_empty_string_literal: bool,
    assigned_name_text: &str,
) -> P<Node> {
    // 8.6.3 RS: IteratorBindingInitialization
    //   SingleNameBinding : BindingIdentifier Initializer?
    //     ...
    //     5. If |Initializer| is present and _v_ is *undefined*, then
    //        a. If IsAnonymousFunctionDefinition(|Initializer|) is *true*, then
    //           i. Set _v_ to ? NamedEvaluation of |Initializer| with argument _bindingId_.
    //     ...
    //
    // 14.3.3.3 RS: KeyedBindingInitialization
    //   SingleNameBinding : BindingIdentifier Initializer?
    //     ...
    //     4. If |Initializer| is present and _v_ is *undefined*, then
    //        a. If IsAnonymousFunctionDefinition(|Initializer|) is *true*, then
    //           i. Set _v_ to ? NamedEvaluation of |Initializer| with argument _bindingId_.
    //     ...

    let factory = &emit_context.factory;
    let assigned_name = if assigned_name_text.is_empty() {
        get_assigned_name_of_identifier(
            emit_context,
            node.name().unwrap(),
            node.initializer().unwrap(),
        )
    } else {
        factory.new_string_literal(alloc_str(assigned_name_text), TokenFlags::None)
    };
    let initializer = finish_transform_named_evaluation(
        emit_context,
        node.initializer().unwrap(),
        assigned_name,
        ignore_empty_string_literal,
    );
    let b = node.as_binding_element();
    factory.update_binding_element(
        node,
        b.dot_dot_dot_token(),
        b.property_name(),
        node.name(),
        Some(initializer),
    )
}

// namedevaluation.go:417
pub fn transform_named_evaluation_of_property_declaration(
    emit_context: P<EmitContext>,
    node: P<Node>, /*NamedEvaluation & PropertyDeclaration*/
    ignore_empty_string_literal: bool,
    assigned_name_text: &str,
) -> P<Node> {
    // 10.2.1.3 RS: EvaluateBody
    //   Initializer : `=` AssignmentExpression
    //     ...
    //     3. If IsAnonymousFunctionDefinition(|AssignmentExpression|) is *true*, then
    //        a. Let _value_ be ? NamedEvaluation of |Initializer| with argument _functionObject_.[[ClassFieldInitializerName]].
    //     ...

    let factory = &emit_context.factory;
    let (assigned_name, name) =
        get_assigned_name_of_property_name(emit_context, node.name().unwrap(), assigned_name_text);
    let initializer = finish_transform_named_evaluation(
        emit_context,
        node.initializer().unwrap(),
        assigned_name,
        ignore_empty_string_literal,
    );
    factory.update_property_declaration(
        node,
        node.modifiers(),
        name,
        None, /*postfixToken*/
        None, /*typeNode*/
        Some(initializer),
    )
}

// namedevaluation.go:438
pub fn transform_named_evaluation_of_assignment_expression(
    emit_context: P<EmitContext>,
    node: P<Node>, /*NamedEvaluation & BinaryExpression*/
    ignore_empty_string_literal: bool,
    assigned_name_text: &str,
) -> P<Node> {
    // 13.15.2 RS: Evaluation
    //   AssignmentExpression : LeftHandSideExpression `=` AssignmentExpression
    //     1. If |LeftHandSideExpression| is neither an |ObjectLiteral| nor an |ArrayLiteral|, then
    //        a. Let _lref_ be ? Evaluation of |LeftHandSideExpression|.
    //        b. If IsAnonymousFunctionDefinition(|AssignmentExpression|) and IsIdentifierRef of |LeftHandSideExpression| are both *true*, then
    //           i. Let _rval_ be ? NamedEvaluation of |AssignmentExpression| with argument _lref_.[[ReferencedName]].
    //     ...
    //
    //   AssignmentExpression : LeftHandSideExpression `&&=` AssignmentExpression
    //     ...
    //     5. If IsAnonymousFunctionDefinition(|AssignmentExpression|) is *true* and IsIdentifierRef of |LeftHandSideExpression| is *true*, then
    //        a. Let _rval_ be ? NamedEvaluation of |AssignmentExpression| with argument _lref_.[[ReferencedName]].
    //     ...
    //
    //   AssignmentExpression : LeftHandSideExpression `||=` AssignmentExpression
    //     ...
    //     5. If IsAnonymousFunctionDefinition(|AssignmentExpression|) is *true* and IsIdentifierRef of |LeftHandSideExpression| is *true*, then
    //        a. Let _rval_ be ? NamedEvaluation of |AssignmentExpression| with argument _lref_.[[ReferencedName]].
    //     ...
    //
    //   AssignmentExpression : LeftHandSideExpression `??=` AssignmentExpression
    //     ...
    //     4. If IsAnonymousFunctionDefinition(|AssignmentExpression|) is *true* and IsIdentifierRef of |LeftHandSideExpression| is *true*, then
    //        a. Let _rval_ be ? NamedEvaluation of |AssignmentExpression| with argument _lref_.[[ReferencedName]].
    //     ...

    let factory = &emit_context.factory;
    let b = node.as_binary_expression();
    let assigned_name = if assigned_name_text.is_empty() {
        get_assigned_name_of_identifier(emit_context, b.left, b.right())
    } else {
        factory.new_string_literal(alloc_str(assigned_name_text), TokenFlags::None)
    };
    let right = finish_transform_named_evaluation(
        emit_context,
        b.right(),
        assigned_name,
        ignore_empty_string_literal,
    );
    factory.update_binary_expression(
        node,
        None, /*modifiers*/
        b.left,
        None, /*typeNode*/
        b.operator_token,
        right,
    )
}

// namedevaluation.go:483
pub fn transform_named_evaluation_of_export_assignment(
    emit_context: P<EmitContext>,
    node: P<Node>, /*NamedEvaluation & ExportAssignment*/
    ignore_empty_string_literal: bool,
    assigned_name_text: &str,
) -> P<Node> {
    // 16.2.3.7 RS: Evaluation
    //   ExportDeclaration : `export` `default` AssignmentExpression `;`
    //     1. If IsAnonymousFunctionDefinition(|AssignmentExpression|) is *true*, then
    //        a. Let _value_ be ? NamedEvaluation of |AssignmentExpression| with argument `"default"`.
    //     ...

    // NOTE: Since emit for `export =` translates to `module.exports = ...`, the assigned name of the class or function
    // is `""`.

    let factory = &emit_context.factory;
    let e = node.as_export_assignment();
    let assigned_name = if !assigned_name_text.is_empty() {
        factory.new_string_literal(alloc_str(assigned_name_text), TokenFlags::None)
    } else if e.is_export_equals {
        factory.new_string_literal("", TokenFlags::None)
    } else {
        factory.new_string_literal("default", TokenFlags::None)
    };
    let expression = finish_transform_named_evaluation(
        emit_context,
        e.expression(),
        assigned_name,
        ignore_empty_string_literal,
    );
    factory.update_export_assignment(
        node,
        None, /*modifiers*/
        e.is_export_equals,
        None, /*typeNode*/
        expression,
    )
}

// namedevaluation.go:513
// Performs a shallow transformation of a `NamedEvaluation` node, such that a valid name will be assigned.
pub fn transform_named_evaluation(
    context: P<EmitContext>,
    node: P<Node>, /*NamedEvaluation*/
    ignore_empty_string_literal: bool,
    assigned_name: &str,
) -> P<Node> {
    match node.kind() {
        Kind::PropertyAssignment => transform_named_evaluation_of_property_assignment(
            context,
            node,
            ignore_empty_string_literal,
            assigned_name,
        ),
        Kind::ShorthandPropertyAssignment => {
            transform_named_evaluation_of_shorthand_assignment_property(
                context,
                node,
                ignore_empty_string_literal,
                assigned_name,
            )
        }
        Kind::VariableDeclaration => transform_named_evaluation_of_variable_declaration(
            context,
            node,
            ignore_empty_string_literal,
            assigned_name,
        ),
        Kind::Parameter => transform_named_evaluation_of_parameter_declaration(
            context,
            node,
            ignore_empty_string_literal,
            assigned_name,
        ),
        Kind::BindingElement => transform_named_evaluation_of_binding_element(
            context,
            node,
            ignore_empty_string_literal,
            assigned_name,
        ),
        Kind::PropertyDeclaration => transform_named_evaluation_of_property_declaration(
            context,
            node,
            ignore_empty_string_literal,
            assigned_name,
        ),
        Kind::BinaryExpression => transform_named_evaluation_of_assignment_expression(
            context,
            node,
            ignore_empty_string_literal,
            assigned_name,
        ),
        Kind::ExportAssignment => transform_named_evaluation_of_export_assignment(
            context,
            node,
            ignore_empty_string_literal,
            assigned_name,
        ),
        _ => panic!("Unhandled case in transformNamedEvaluation"),
    }
}
