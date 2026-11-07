// Port of transformers/utilities.go. Most of it (everything but IsSimpleCopiableExpression, IsOriginalNodeSingleLine,
// IsSimpleInlineableExpression and the last three functions) is taken from the port the emit/transforms wave wrote
// in its stand-in file.

use crate::*;

// utilities.go:12
pub(crate) fn is_generated_identifier(emit_context: P<EmitContext>, name: P<Node>) -> bool {
    emit_context.has_auto_generate_info(Some(name))
}

// utilities.go:16
pub(crate) fn is_helper_name(emit_context: P<EmitContext>, name: P<Node>) -> bool {
    emit_context.emit_flags(name).intersects(EmitFlags::HelperName)
}

// utilities.go:20
pub(crate) fn is_local_name(emit_context: P<EmitContext>, name: P<Node>) -> bool {
    emit_context.emit_flags(name).intersects(EmitFlags::LocalName)
}

// utilities.go:24
pub(crate) fn is_export_name(emit_context: P<EmitContext>, name: P<Node>) -> bool {
    emit_context.emit_flags(name).intersects(EmitFlags::ExportName)
}

// utilities.go:28
pub(crate) fn is_identifier_reference(name: P<Node>, parent: P<Node>) -> bool {
    match parent.kind() {
        Kind::BinaryExpression
        | Kind::PrefixUnaryExpression
        | Kind::PostfixUnaryExpression
        | Kind::YieldExpression
        | Kind::AsExpression
        | Kind::SatisfiesExpression
        | Kind::ElementAccessExpression
        | Kind::NonNullExpression
        | Kind::SpreadElement
        | Kind::SpreadAssignment
        | Kind::ParenthesizedExpression
        | Kind::ArrayLiteralExpression
        | Kind::DeleteExpression
        | Kind::TypeOfExpression
        | Kind::VoidExpression
        | Kind::AwaitExpression
        | Kind::TypeAssertionExpression
        | Kind::ExpressionWithTypeArguments
        | Kind::JsxSelfClosingElement
        | Kind::JsxSpreadAttribute
        | Kind::JsxExpression
        | Kind::PartiallyEmittedExpression => {
            // all immediate children that can be `Identifier` would be instances of `IdentifierReference`
            true
        }
        Kind::ComputedPropertyName
        | Kind::Decorator
        | Kind::IfStatement
        | Kind::DoStatement
        | Kind::WhileStatement
        | Kind::WithStatement
        | Kind::ReturnStatement
        | Kind::SwitchStatement
        | Kind::CaseClause
        | Kind::ThrowStatement
        | Kind::ExpressionStatement
        | Kind::ExportAssignment
        | Kind::PropertyAccessExpression
        | Kind::TemplateSpan => {
            // only an `Expression()` child that can be `Identifier` would be an instance of `IdentifierReference`
            parent.expression() == Some(name)
        }
        Kind::VariableDeclaration
        | Kind::Parameter
        | Kind::BindingElement
        | Kind::PropertyDeclaration
        | Kind::PropertySignature
        | Kind::PropertyAssignment
        | Kind::EnumMember
        | Kind::JsxAttribute => {
            // only an `Initializer()` child that can be `Identifier` would be an instance of `IdentifierReference`
            parent.initializer() == Some(name)
        }
        Kind::ShorthandPropertyAssignment => {
            parent.as_shorthand_property_assignment().object_assignment_initializer() == Some(name)
        }
        Kind::ForStatement => {
            parent.initializer() == Some(name)
                || parent.as_for_statement().condition == Some(name)
                || parent.as_for_statement().incrementor == Some(name)
        }
        Kind::ForInStatement | Kind::ForOfStatement => {
            parent.initializer() == Some(name) || parent.expression() == Some(name)
        }
        Kind::ImportEqualsDeclaration => {
            parent.as_import_equals_declaration().module_reference == name
        }
        Kind::ArrowFunction => parent.body() == Some(name),
        Kind::ConditionalExpression => {
            let c = parent.as_conditional_expression();
            c.condition == name || c.when_true == name || c.when_false == name
        }
        Kind::CallExpression | Kind::NewExpression => {
            parent.expression() == Some(name) || parent.arguments().contains(&name)
        }
        Kind::TaggedTemplateExpression => parent.as_tagged_template_expression().tag == name,
        Kind::ImportAttribute => parent.as_import_attribute().value == name,
        Kind::JsxOpeningElement | Kind::JsxClosingElement => parent.tag_name() == name,
        _ => false,
    }
}

// utilities.go:112
fn convert_binding_element_to_array_assignment_element(
    emit_context: P<EmitContext>,
    element: P<Node>,
) -> P<Node> {
    let f = &emit_context.get().factory;
    let Some(name) = element.name() else {
        let elision = f.new_omitted_expression();
        emit_context.set_original(elision, element);
        emit_context.assign_comment_and_source_map_ranges(elision, element);
        return elision;
    };
    let expression = convert_binding_name_to_assignment_element_target(emit_context, name);
    let e = element.as_binding_element();
    if e.dot_dot_dot_token().is_some() {
        let spread = f.new_spread_element(expression);
        emit_context.set_original(spread, element);
        emit_context.assign_comment_and_source_map_ranges(spread, element);
        return spread;
    }
    if let Some(initializer) = e.initializer() {
        let assignment = f.new_assignment_expression(expression, initializer);
        emit_context.set_original(assignment, element);
        emit_context.assign_comment_and_source_map_ranges(assignment, element);
        return assignment;
    }
    expression
}

// utilities.go:135
fn convert_binding_element_to_object_assignment_element(
    emit_context: P<EmitContext>,
    element: P<Node>,
) -> P<Node> {
    let f = &emit_context.get().factory;
    let e = element.as_binding_element();
    if e.dot_dot_dot_token().is_some() {
        let spread = f.new_spread_assignment(element.name().unwrap());
        emit_context.set_original(spread, element);
        emit_context.assign_comment_and_source_map_ranges(spread, element);
        return spread;
    }
    if let Some(property_name) = e.property_name() {
        let mut expression = convert_binding_name_to_assignment_element_target(
            emit_context,
            element.name().unwrap(),
        );
        if let Some(initializer) = e.initializer() {
            expression = f.new_assignment_expression(expression, initializer);
        }
        let assignment = f.new_property_assignment(
            None, /*modifiers*/
            property_name,
            None, /*postfixToken*/
            None, /*typeNode*/
            expression,
        );
        emit_context.set_original(assignment, element);
        emit_context.assign_comment_and_source_map_ranges(assignment, element);
        return assignment;
    }
    let equals_token =
        if e.initializer().is_some() { Some(f.new_token(Kind::EqualsToken)) } else { None };
    let assignment = f.new_shorthand_property_assignment(
        None, /*modifiers*/
        element.name().unwrap(),
        None, /*postfixToken*/
        None, /*typeNode*/
        equals_token,
        e.initializer(),
    );
    emit_context.set_original(assignment, element);
    emit_context.assign_comment_and_source_map_ranges(assignment, element);
    assignment
}

// utilities.go:169
pub(crate) fn convert_binding_pattern_to_assignment_pattern(
    emit_context: P<EmitContext>,
    element: P<Node>,
) -> P<Node> {
    match element.kind() {
        Kind::ArrayBindingPattern => {
            convert_binding_element_to_array_assignment_pattern(emit_context, element)
        }
        Kind::ObjectBindingPattern => {
            convert_binding_element_to_object_assignment_pattern(emit_context, element)
        }
        _ => panic!("Unknown binding pattern"),
    }
}

// utilities.go:180
fn convert_binding_element_to_object_assignment_pattern(
    emit_context: P<EmitContext>,
    element: P<Node>,
) -> P<Node> {
    let f = &emit_context.get().factory;
    let mut properties: Vec<P<Node>> = Vec::new();
    let elements = element.as_binding_pattern().elements;
    properties.extend(
        elements
            .nodes()
            .iter()
            .map(|&e| convert_binding_element_to_object_assignment_element(emit_context, e)),
    );
    let property_list = f.new_node_list(properties);
    property_list.loc.set(elements.loc.get());
    let object = f.new_object_literal_expression(property_list, false /*multiLine*/);
    emit_context.set_original(object, element);
    emit_context.assign_comment_and_source_map_ranges(object, element);
    object
}

// utilities.go:193
fn convert_binding_element_to_array_assignment_pattern(
    emit_context: P<EmitContext>,
    element: P<Node>,
) -> P<Node> {
    let f = &emit_context.get().factory;
    let mut out: Vec<P<Node>> = Vec::new();
    let elements = element.as_binding_pattern().elements;
    out.extend(
        elements
            .nodes()
            .iter()
            .map(|&e| convert_binding_element_to_array_assignment_element(emit_context, e)),
    );
    let element_list = f.new_node_list(out);
    element_list.loc.set(elements.loc.get());
    let object = f.new_array_literal_expression(element_list, false /*multiLine*/);
    emit_context.set_original(object, element);
    emit_context.assign_comment_and_source_map_ranges(object, element);
    object
}

// utilities.go:206
fn convert_binding_name_to_assignment_element_target(
    emit_context: P<EmitContext>,
    element: P<Node>,
) -> P<Node> {
    if ast::is_binding_pattern(element) {
        return convert_binding_pattern_to_assignment_pattern(emit_context, element);
    }
    element
}

// utilities.go:213
pub(crate) fn convert_variable_declaration_to_assignment_expression(
    emit_context: P<EmitContext>,
    element: P<Node>,
) -> Option<P<Node>> {
    let initializer = element.initializer()?;
    let expression =
        convert_binding_name_to_assignment_element_target(emit_context, element.name().unwrap());
    let assignment = emit_context.factory.new_assignment_expression(expression, initializer);
    emit_context.set_original(assignment, element);
    emit_context.assign_comment_and_source_map_ranges(assignment, element);
    Some(assignment)
}

// utilities.go:224. Go distinguishes a nil slice (-> nil) from an empty one (-> an empty SyntaxList).
pub(crate) fn single_or_many(
    nodes: Option<Vec<P<Node>>>,
    factory: &printer::NodeFactory,
) -> Option<P<Node>> {
    let nodes = nodes?;
    if nodes.len() == 1 {
        return Some(nodes[0]);
    }
    Some(factory.new_syntax_list(alloc_vec(nodes)))
}

// utilities.go:241
pub fn is_simple_copiable_expression(expression: P<Node>) -> bool {
    ast::is_string_literal_like(expression)
        || ast::is_numeric_literal(expression)
        || ast::is_keyword_kind(expression.kind())
        || ast::is_identifier(expression)
}

// utilities.go:248
pub fn is_original_node_single_line(emit_context: P<EmitContext>, node: Option<P<Node>>) -> bool {
    let Some(node) = node else {
        return false;
    };
    let Some(original) = emit_context.most_original(Some(node)) else {
        return false;
    };
    let Some(source) = ast::get_source_file_of_node(Some(original)) else {
        return false;
    };
    // The most original node can be a synthesized one in a parsed file. Go passes its position (-1) on to
    // GetECMALineOfPosition, which returns line -1 for it, so two synthesized positions are on the same line.
    let line_of = |pos: tsrs_core::TextPos| {
        pos.source_offset()
            .map_or(-1, |pos| scanner::get_ecma_line_of_position(&*source, pos) as isize)
    };
    line_of(original.pos()) == line_of(original.end())
}

// utilities.go:270
pub fn is_simple_inlineable_expression(expression: P<Node>) -> bool {
    !ast::is_identifier(expression) && is_simple_copiable_expression(expression)
}

// utilities.go:275
pub(crate) fn find_super_statement_index_path(statements: &[P<Node>], start: usize) -> Vec<usize> {
    let mut indices =
        find_super_statement_index_path_worker(statements, start, Vec::new()).unwrap_or_default();
    indices.reverse();
    indices
}

// utilities.go:281
fn find_super_statement_index_path_worker(
    statements: &[P<Node>],
    start: usize,
    indices: Vec<usize>,
) -> Option<Vec<usize>> {
    for (i, element) in statements.iter().enumerate().skip(start) {
        let statement = *element;
        if get_super_call_from_statement(statement).is_some() {
            let mut indices = indices;
            indices.push(i);
            return Some(indices);
        } else if ast::is_try_statement(statement)
            && let Some(mut result) = find_super_statement_index_path_worker(
                statement.as_try_statement().try_block.statements(),
                0,
                indices.clone(),
            )
        {
            result.push(i);
            return Some(result);
        }
    }
    None
}

// utilities.go:296
fn get_super_call_from_statement(statement: P<Node>) -> Option<P<Node>> {
    if !ast::is_expression_statement(statement) {
        return None;
    }
    let expression = ast::skip_parentheses(statement.expression().unwrap());
    if is_super_call(expression) {
        return Some(expression);
    }
    None
}

// MoveRangePastModifiers returns a text range that starts past any modifiers on the node.
// utilities.go:308
pub(crate) fn move_range_past_modifiers(node: P<Node>) -> tsrs_core::TextRange {
    if ast::is_property_declaration(node) || ast::is_method_declaration(node) {
        return tsrs_core::TextRange::from_text_pos(node.name().unwrap().pos(), node.end());
    }

    let mut last_modifier = None;
    if ast::can_have_modifiers(node) {
        last_modifier = node.modifier_nodes().last().copied();
    }

    if let Some(last_modifier) = last_modifier
        && !ast::position_is_synthesized(last_modifier.end())
    {
        return tsrs_core::TextRange::from_text_pos(last_modifier.end(), node.end());
    }
    move_range_past_decorators(node)
}

// MoveRangePastDecorators returns a text range that starts past any decorators on the node.
// utilities.go:325
pub(crate) fn move_range_past_decorators(node: P<Node>) -> tsrs_core::TextRange {
    let mut last_decorator = None;
    if ast::can_have_modifiers(node) {
        let nodes = node.modifier_nodes();
        if !nodes.is_empty() {
            last_decorator = nodes.iter().rev().find(|&&n| ast::is_decorator(n)).copied();
        }
    }

    if let Some(last_decorator) = last_decorator
        && !ast::position_is_synthesized(last_decorator.end())
    {
        return tsrs_core::TextRange::from_text_pos(last_decorator.end(), node.end());
    }
    node.loc()
}

// GetNonAssignmentOperatorForCompoundAssignment returns the non-assignment operator for a compound assignment.
// utilities.go:341
pub(crate) fn get_non_assignment_operator_for_compound_assignment(kind: Kind) -> Kind {
    match kind {
        Kind::PlusEqualsToken => Kind::PlusToken,
        Kind::MinusEqualsToken => Kind::MinusToken,
        Kind::AsteriskEqualsToken => Kind::AsteriskToken,
        Kind::AsteriskAsteriskEqualsToken => Kind::AsteriskAsteriskToken,
        Kind::SlashEqualsToken => Kind::SlashToken,
        Kind::PercentEqualsToken => Kind::PercentToken,
        Kind::LessThanLessThanEqualsToken => Kind::LessThanLessThanToken,
        Kind::GreaterThanGreaterThanEqualsToken => Kind::GreaterThanGreaterThanToken,
        Kind::GreaterThanGreaterThanGreaterThanEqualsToken => {
            Kind::GreaterThanGreaterThanGreaterThanToken
        }
        Kind::AmpersandEqualsToken => Kind::AmpersandToken,
        Kind::BarEqualsToken => Kind::BarToken,
        Kind::CaretEqualsToken => Kind::CaretToken,
        Kind::BarBarEqualsToken => Kind::BarBarToken,
        Kind::AmpersandAmpersandEqualsToken => Kind::AmpersandAmpersandToken,
        Kind::QuestionQuestionEqualsToken => Kind::QuestionQuestionToken,
        _ => kind,
    }
}

// ast/utilities.go:2138 (Go ast.IsSuperCall; kept here because tsrs_checker has its own glob-imported copy)
fn is_super_call(node: P<Node>) -> bool {
    ast::is_call_expression(node) && node.expression().unwrap().kind() == Kind::SuperKeyword
}

// ast/utilities.go:4038 (Go ast.IsEmptyObjectLiteral; here because tsrs_checker glob-imports its own copy)
pub(crate) fn is_empty_object_literal(expression: P<Node>) -> bool {
    ast::is_object_literal_expression(expression) && expression.properties().is_empty()
}

// ast/utilities.go:4042 (Go ast.IsEmptyArrayLiteral; same reason)
pub(crate) fn is_empty_array_literal(expression: P<Node>) -> bool {
    ast::is_array_literal_expression(expression) && expression.elements().is_empty()
}
