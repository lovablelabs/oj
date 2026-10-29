use crate::*;
use tsrs_ast as ast;
use tsrs_ast::*;
use tsrs_core::*;

impl PseudoChecker {
    // lookup.go:11
    pub fn get_return_type_of_signature(&self, signature_node: P<Node>) -> Option<P<PseudoType>> {
        match signature_node.kind() {
            Kind::GetAccessor => self.get_type_of_accessor(signature_node),
            Kind::MethodDeclaration
            | Kind::FunctionDeclaration
            | Kind::Constructor
            | Kind::MethodSignature
            | Kind::CallSignature
            | Kind::ConstructSignature
            | Kind::SetAccessor
            | Kind::IndexSignature
            | Kind::FunctionType
            | Kind::ConstructorType
            | Kind::FunctionExpression
            | Kind::ArrowFunction
            | Kind::JSDocSignature => self.create_return_from_signature(signature_node),
            _ => debug::fail_bad_syntax_kind(
                &signature_node.kind_string(),
                &[&"Node needs to be an inferrable node"],
            ),
        }
    }

    // lookup.go:26
    pub fn get_type_of_accessor(&self, accessor: P<Node>) -> Option<P<PseudoType>> {
        self.type_from_accessor(accessor)
    }

    // lookup.go:34
    pub fn get_type_of_declaration(&self, node: P<Node>) -> Option<P<PseudoType>> {
        match node.kind() {
            Kind::Parameter => self.type_from_parameter(node),
            Kind::VariableDeclaration => Some(self.type_from_variable(node)),
            Kind::PropertySignature | Kind::PropertyDeclaration | Kind::JSDocPropertyTag => {
                Some(self.type_from_property(node))
            }
            Kind::BindingElement => Some(new_pseudo_type_no_result(node)),
            Kind::ExportAssignment => {
                self.type_from_expression(node.as_export_assignment().expression())
            }
            Kind::PropertyAccessExpression
            | Kind::ElementAccessExpression
            | Kind::BinaryExpression => Some(Self::type_from_expando_property(node)),
            Kind::PropertyAssignment | Kind::ShorthandPropertyAssignment => {
                Some(self.type_from_property_assignment(node))
            }
            Kind::CallExpression => {
                // TODO: How much of the checker's getTypeFromPropertyDescriptor is worth trying to emulate over ASTs
                // for ObjectDefinePropertyValue and ObjectDefinePropertyExports?
                ast::get_assignment_declaration_kind(node);
                Some(new_pseudo_type_no_result(node))
            }
            _ => debug::fail_bad_syntax_kind(
                &node.kind_string(),
                &[&"node needs to be an inferrable node"],
            ),
        }
    }

    // lookup.go:69
    fn type_from_property_assignment(&self, node: P<Node>) -> P<PseudoType> {
        let annotation = node.type_node();
        if let Some(annotation) = annotation {
            return new_pseudo_type_direct(annotation);
        }
        if node.kind() == Kind::PropertyAssignment {
            let init = node.initializer();
            if let Some(init) = init {
                let expr = self.type_from_expression(init);
                if let Some(expr) = expr
                    && (expr.kind != PseudoTypeKind::Inferred
                        || !expr.as_pseudo_type_inferred().error_nodes.is_empty())
                {
                    return expr;
                }
                // fallback to NoResult if PseudoTypeKindInferred without error nodes
            }
        }
        new_pseudo_type_no_result(node)
    }

    // lookup.go:88
    // This is _not_ redundant with the reparser; see how expandoFunctionSymbolProperty.ts and similar behaves
    fn type_from_expando_property(node: P<Node>) -> P<PseudoType> {
        let declared_type = node.type_node();
        if let Some(declared_type) = declared_type {
            return new_pseudo_type_direct(declared_type);
        }
        // While `node` is an expression, as an expando, it should also always be a
        // declaration with a `.Symbol()` which requires declaration fallback handling
        new_pseudo_type_no_result(node)
    }

    // lookup.go:98
    fn type_from_property(&self, node: P<Node>) -> P<PseudoType> {
        let t = node.type_node();
        if let Some(t) = t {
            return new_pseudo_type_direct(t);
        }
        if is_property_declaration(node) {
            let init = node.initializer();
            if let Some(init) = init
                && !is_contextually_typed(node)
            {
                // explicit fail on readonly template literals to allow for literal freshness in the future
                if has_modifier(node, ModifierFlags::Readonly) && is_template_expression(init) {
                    return new_pseudo_type_no_result(node);
                }
                let expr = self.type_from_expression(init);
                if let Some(expr) = expr
                    && (expr.kind != PseudoTypeKind::Inferred
                        || !expr.as_pseudo_type_inferred().error_nodes.is_empty())
                {
                    let postfix_token = node.as_property_declaration().postfix_token();
                    if expr.kind != PseudoTypeKind::Direct
                        && postfix_token.is_some_and(|t| t.kind() == Kind::QuestionToken)
                    {
                        // type comes from the initializer expression on a property with a `?` - add `| undefined` to the type
                        return add_undefined_if_definitely_required(expr);
                    }
                    return expr;
                }
                // fallback to NoResult if PseudoTypeKindInferred without error nodes
            }
        }
        new_pseudo_type_no_result(node)
    }

    // lookup.go:124
    fn type_from_variable(&self, declaration: P<Node>) -> P<PseudoType> {
        let t = declaration.type_node();
        if let Some(t) = t {
            return new_pseudo_type_direct(t);
        }
        let init = declaration.initializer();
        if let (Some(init), Some(symbol)) = (init, declaration.symbol()) {
            let qualifies = {
                let decls = symbol.declarations();
                decls.len() == 1 || count_where(decls, |d| is_variable_declaration(*d)) == 1
            };
            if qualifies && !is_contextually_typed(declaration) {
                // TODO: also should bail on expando declarations; reuse syntactic expando check used in declaration emit
                // TODO: Strada forces an inference fallback on `const` variables with template expression initializers, to leave space for template literal freshness in the future
                if is_var_const(declaration) && is_template_expression(init) {
                    return new_pseudo_type_no_result(declaration);
                }
                let expr = self.type_from_expression(init);
                if let Some(expr) = expr
                    && (expr.kind != PseudoTypeKind::Inferred
                        || !expr.as_pseudo_type_inferred().error_nodes.is_empty())
                {
                    return expr;
                }
                // fallback to NoResult if PseudoTypeKindInferred without error nodes
            }
        }
        new_pseudo_type_no_result(declaration)
    }

    // lookup.go:146
    #[expect(
        clippy::unnecessary_wraps,
        reason = "The pseudo-checker accessor hook returns None to let the caller fall back to full checker inference."
    )]
    fn type_from_accessor(&self, accessor: P<Node>) -> Option<P<PseudoType>> {
        let accessor_declarations = {
            let decls = accessor.declaration_data().unwrap().symbol().unwrap().declarations();
            get_all_accessor_declarations_for_declaration(accessor, decls)
        };
        let accessor_type = Self::get_type_annotation_from_all_accessor_declarations(
            accessor,
            accessor_declarations,
        );
        if let Some(accessor_type) = accessor_type
            && !is_type_predicate_node(accessor_type)
        {
            return Some(new_pseudo_type_direct(accessor_type));
        }
        if let Some(get_accessor) = accessor_declarations.get_accessor {
            let mut res = self.create_return_from_signature(get_accessor).unwrap();
            if res.kind == PseudoTypeKind::Inferred
                && res.as_pseudo_type_inferred().error_nodes.is_empty()
            {
                let mut error_nodes = vec![get_accessor];
                if let Some(set_accessor) = accessor_declarations.set_accessor {
                    error_nodes.push(set_accessor);
                }
                res = new_pseudo_type_inferred_with_errors(
                    res.as_pseudo_type_inferred().expression,
                    res.as_pseudo_type_inferred().is_signature_return,
                    &error_nodes,
                ); // Move error up to the accessor
            }
            return Some(res);
        }
        Some(new_pseudo_type_no_result(accessor))
    }

    // lookup.go:166
    fn get_type_annotation_from_all_accessor_declarations(
        node: P<Node>,
        accessors: AllAccessorDeclarations,
    ) -> Option<P<Node>> {
        let mut accessor_type = Self::get_type_annotation_from_accessor(node);
        if accessor_type.is_none() && node != accessors.first_accessor {
            accessor_type = Self::get_type_annotation_from_accessor(accessors.first_accessor);
        }
        if accessor_type.is_none()
            && let Some(second_accessor) = accessors.second_accessor
            && node != second_accessor
        {
            accessor_type = Self::get_type_annotation_from_accessor(second_accessor);
        }
        accessor_type
    }

    // lookup.go:177
    fn get_type_annotation_from_accessor(node: P<Node>) -> Option<P<Node>> {
        // !!! TODO: support ripping return type off of .FullSignature
        if node.kind() == Kind::GetAccessor {
            return node.as_get_accessor_declaration().type_();
        }
        let set = node.as_set_accessor_declaration();
        let params = {
            let params = set.parameters()?;
            params.nodes()
        };
        if params.is_empty() {
            return None;
        }
        let p = params[0];
        if !is_parameter_declaration(p) {
            return None;
        }
        p.as_parameter_declaration().type_()
    }
}

// lookup.go:196
fn is_value_signature_declaration(node: P<Node>) -> bool {
    is_function_expression(node)
        || is_arrow_function(node)
        || is_method_declaration(node)
        || is_accessor(node)
        || is_function_declaration(node)
        || is_constructor_declaration(node)
}

impl PseudoChecker {
    // lookup.go:201
    // does not return `nil`, returns a `NoResult` pseudotype instead
    fn create_return_from_signature(&self, fn_: P<Node>) -> Option<P<PseudoType>> {
        if is_function_like(fn_) {
            let d = fn_.function_like_data().unwrap();
            // !!! TODO: support ripping return type off of .FullSignature
            let r = d.type_();
            if let Some(r) = r {
                return Some(new_pseudo_type_direct(r));
            }
        }
        if is_value_signature_declaration(fn_) {
            return self.type_from_single_return_expression(Some(fn_));
        }
        Some(new_pseudo_type_no_result(fn_))
    }

    // lookup.go:216
    fn type_from_single_return_expression(&self, fn_: Option<P<Node>>) -> Option<P<PseudoType>> {
        let mut candidate_expr: Option<P<Node>> = None;
        if let Some(f) = fn_
            && !node_is_missing(f.body())
        {
            let flags = get_function_flags(Some(f));
            if flags.intersects(FunctionFlags::AsyncGenerator) {
                return Some(new_pseudo_type_inferred(f, true));
            }

            let body = f.body().unwrap();
            if is_block(body) {
                for_each_return_statement(body, |stmt| {
                    if stmt.parent() != Some(body) {
                        // Why bail on nested return statements?
                        candidate_expr = None;
                        return true;
                    }
                    if candidate_expr.is_none() {
                        candidate_expr = stmt.as_return_statement().expression();
                    } else {
                        candidate_expr = None;
                        return true;
                    }
                    false
                });
            } else {
                candidate_expr = Some(body);
            }
        }
        if let Some(candidate_expr) = candidate_expr {
            if is_contextually_typed(candidate_expr) {
                let mut t: Option<P<Node>> = None;
                if candidate_expr.kind() == Kind::TypeAssertionExpression {
                    t = Some(candidate_expr.as_type_assertion().type_);
                } else if candidate_expr.kind() == Kind::AsExpression {
                    t = Some(candidate_expr.as_as_expression().type_);
                }
                if let Some(t) = t
                    && !is_const_type_reference(t)
                {
                    return Some(new_pseudo_type_direct(t));
                }
            } else {
                return self.type_from_expression(candidate_expr);
            }
        }
        Some(new_pseudo_type_inferred(fn_.unwrap(), true))
    }

    // lookup.go:262
    // This is basically `checkExpression` for pseudotypes
    fn type_from_expression(&self, node: P<Node>) -> Option<P<PseudoType>> {
        match node.kind() {
            Kind::OmittedExpression => return Some(*PSEUDO_TYPE_UNDEFINED),
            Kind::ParenthesizedExpression => {
                // assertions transformed on reparse, just unwrap
                return self.type_from_expression(node.as_parenthesized_expression().expression());
            }
            Kind::Identifier => {
                // !!! TODO: in strada, this uses symbol information to ensure `node` refers to the global `undefined` symbol instead
                // we should probably import `resolveName` and use it here to check for the same; but we have to setup some barebones pseudoglobals for that to work!
                if node.as_identifier().text() == "undefined" {
                    return Some(*PSEUDO_TYPE_UNDEFINED);
                }
            }
            Kind::NullKeyword => return Some(*PSEUDO_TYPE_NULL),
            Kind::ArrowFunction | Kind::FunctionExpression => {
                return Some(self.type_from_function_like_expression(node));
            }
            Kind::TypeAssertionExpression => {
                return self.type_from_type_assertion(
                    node.as_type_assertion().expression,
                    node.as_type_assertion().type_,
                );
            }
            Kind::AsExpression => {
                return self.type_from_type_assertion(
                    node.as_as_expression().expression,
                    node.as_as_expression().type_,
                );
            }
            Kind::PrefixUnaryExpression => {
                if is_primitive_literal_value(node, true) {
                    return Self::type_from_primitive_literal_prefix(node);
                }
            }
            Kind::ArrayLiteralExpression => return Some(self.type_from_array_literal(node)),
            Kind::ObjectLiteralExpression => return Some(self.type_from_object_literal(node)),
            Kind::ClassExpression => {
                return Some(new_pseudo_type_inferred_with_errors(node, false, &[node]));
            } // No possible annotation/directly mappable syntax
            Kind::TemplateExpression => {
                // templateLitWithHoles as const, not supported
                if is_in_const_context(node) {
                    return Some(new_pseudo_type_inferred(node, false));
                }
                return Some(new_pseudo_type_maybe_const_location(
                    node,
                    new_pseudo_type_inferred(node, false),
                    *PSEUDO_TYPE_STRING,
                ));
            }
            Kind::NumericLiteral => {
                return Some(new_pseudo_type_maybe_const_location(
                    node,
                    new_pseudo_type_numeric_literal(node),
                    *PSEUDO_TYPE_NUMBER,
                ));
            }
            Kind::NoSubstitutionTemplateLiteral | Kind::StringLiteral => {
                return Some(new_pseudo_type_maybe_const_location(
                    node,
                    new_pseudo_type_string_literal(node),
                    *PSEUDO_TYPE_STRING,
                ));
            }
            Kind::BigIntLiteral => {
                return Some(new_pseudo_type_maybe_const_location(
                    node,
                    new_pseudo_type_big_int_literal(node),
                    *PSEUDO_TYPE_BIG_INT,
                ));
            }
            Kind::TrueKeyword => {
                return Some(new_pseudo_type_maybe_const_location(
                    node,
                    *PSEUDO_TYPE_TRUE,
                    *PSEUDO_TYPE_BOOLEAN,
                ));
            }
            Kind::FalseKeyword => {
                return Some(new_pseudo_type_maybe_const_location(
                    node,
                    *PSEUDO_TYPE_FALSE,
                    *PSEUDO_TYPE_BOOLEAN,
                ));
            }
            _ => {}
        }
        Some(new_pseudo_type_inferred(node, false))
    }

    // lookup.go:315
    fn type_from_object_literal(&self, node: P<Node>) -> P<PseudoType> {
        let error_nodes = Self::can_get_type_from_object_literal(node);
        if !error_nodes.is_empty() {
            return new_pseudo_type_inferred_with_errors(node, false, &error_nodes);
        }
        // we are in a const context producing an object literal type, there are no shorthand or spread assignments
        let properties = node.properties();
        if properties.is_empty() {
            return new_pseudo_type_object_literal(&[]);
        }
        let mut results: Vec<P<PseudoObjectElement>> = Vec::with_capacity(properties.len());
        results.extend(properties.iter().filter_map(|&e| {
            match e.kind() {
                Kind::MethodDeclaration => {
                    let optional = e
                        .as_method_declaration()
                        .postfix_token()
                        .is_some_and(|t| t.kind() == Kind::QuestionToken);
                    if let Some(full_signature) = e.function_like_data().unwrap().full_signature() {
                        Some(new_pseudo_property_assignment(
                            false,
                            e.name().unwrap(),
                            optional,
                            new_pseudo_type_direct(full_signature),
                        ))
                    } else {
                        Some(new_pseudo_object_method(
                            e,
                            e.name().unwrap(),
                            optional,
                            &Self::clone_type_parameters(
                                e.as_method_declaration().type_parameters(),
                            ),
                            &self.clone_parameters(e.parameter_list()),
                            self.create_return_from_signature(e).unwrap(),
                        ))
                    }
                }
                Kind::PropertyAssignment => Some(new_pseudo_property_assignment(
                    false,
                    e.name().unwrap(),
                    e.as_property_assignment()
                        .postfix_token()
                        .is_some_and(|t| t.kind() == Kind::QuestionToken),
                    self.type_from_expression(e.initializer().unwrap()).unwrap(),
                )),
                Kind::SetAccessor | Kind::GetAccessor => {
                    self.get_accessor_member(e, e.name().unwrap())
                }
                _ => None,
            }
        }));
        new_pseudo_type_object_literal(&results)
    }

    // lookup.go:363
    // roughly analogous to typeFromObjectLiteralAccessor in strada
    fn get_accessor_member(
        &self,
        accessor: P<Node>,
        name: P<Node>,
    ) -> Option<P<PseudoObjectElement>> {
        let all_accessors = {
            let decls = accessor.symbol().unwrap().declarations();
            get_all_accessor_declarations_for_declaration(accessor, decls) // TODO: node preservation for late-bound accessor pairs?
        };

        // TODO: handle pseudo-annotations from get accessor return positions?
        if let (Some(get_accessor), Some(set_accessor)) =
            (all_accessors.get_accessor, all_accessors.set_accessor)
            && get_accessor.as_get_accessor_declaration().type_().is_some()
        {
            let set_params = set_accessor.parameters();
            if !set_params.is_empty() && set_params[0].as_parameter_declaration().type_().is_some()
            {
                // We have possible types for both accessors, we can't know if they are the same type so we keep both accessors

                if is_get_accessor_declaration(accessor) {
                    return Some(new_pseudo_get_accessor(
                        accessor,
                        name,
                        false,
                        self.type_from_accessor(accessor).unwrap(),
                    ));
                }
                return Some(new_pseudo_set_accessor(
                    accessor,
                    name,
                    false,
                    self.clone_parameters(accessor.as_set_accessor_declaration().parameters())[0],
                ));
            }
        }

        if accessor == all_accessors.first_accessor {
            // only one annotated accessor; output a property - `readonly` for a single `get` accessor

            let accessor_type = self.type_from_accessor(accessor);
            let readonly =
                is_get_accessor_declaration(accessor) && all_accessors.second_accessor.is_none();
            return Some(new_pseudo_property_assignment(
                readonly,
                name,
                false,
                accessor_type.unwrap(),
            ));
        }
        None
    }

    // lookup.go:406
    // canGetTypeFromObjectLiteral checks whether an object literal can be typed by the pseudochecker.
    // Returns nil if the object can be typed, or a slice of error nodes (shorthand/spread properties,
    // non-literal computed names) that prevent typing.
    fn can_get_type_from_object_literal(node: P<Node>) -> Vec<P<Node>> {
        let properties = node.properties();
        if properties.is_empty() {
            return Vec::new(); // empty object, ok
        }
        let mut error_nodes: Vec<P<Node>> = Vec::new();
        error_nodes.extend(properties.iter().filter_map(|&e| {
            if e.flags().intersects(NodeFlags::ThisNodeHasError) {
                return Some(e);
            }
            if e.kind() == Kind::ShorthandPropertyAssignment || e.kind() == Kind::SpreadAssignment {
                return Some(e);
            }
            let name = e.name().unwrap();
            if name.flags().intersects(NodeFlags::ThisNodeHasError) {
                return Some(name);
            }
            if name.kind() == Kind::PrivateIdentifier {
                return Some(e);
            }
            if name.kind() == Kind::ComputedPropertyName {
                let expression = name.expression().unwrap();
                if is_primitive_literal_value(expression, false) { None } else { Some(name) }
            } else {
                None
            }
        }));
        error_nodes
    }

    // lookup.go:438
    fn type_from_array_literal(&self, node: P<Node>) -> P<PseudoType> {
        let error_nodes = Self::can_get_type_from_array_literal(node);
        if !error_nodes.is_empty() {
            return new_pseudo_type_inferred_with_errors(node, false, &error_nodes);
        }
        if is_in_const_context(node) && is_contextually_typed(node) {
            return new_pseudo_type_inferred(node, false); // expr in an as const cast with a contextual type has variable readonly state, bail
        }
        // we are in a const context producing a tuple type, there are no spread elements
        let elements = node.elements();
        let mut results: Vec<P<PseudoType>> = Vec::with_capacity(elements.len());
        results.extend(elements.iter().map(|&e| self.type_from_expression(e).unwrap()));
        new_pseudo_type_tuple(&results)
    }

    // lookup.go:457
    // canGetTypeFromArrayLiteral checks whether an array literal can be typed by the pseudochecker.
    // Returns nil if the array can be typed, or a slice of error nodes that prevent typing.
    // For non-const arrays, the error node is the array expression itself.
    // For const arrays with spreads, the error node is the spread element.
    fn can_get_type_from_array_literal(node: P<Node>) -> Vec<P<Node>> {
        if !is_in_const_context(node) {
            return vec![node];
        }
        if let Some(result) = node
            .elements()
            .iter()
            .find_map(|&e| if e.kind() == Kind::SpreadElement { Some(vec![e]) } else { None })
        {
            return result;
        }
        Vec::new()
    }
}

// lookup.go:470
// See `isConstContext` in `checker.go` - this is basically any node kind mentioned in that
fn is_const_context_propagating_kind(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::ArrayLiteralExpression
            | Kind::ObjectLiteralExpression
            | Kind::ParenthesizedExpression
            | Kind::SpreadElement
            | Kind::PropertyAssignment
            | Kind::ShorthandPropertyAssignment
            | Kind::TemplateSpan
            | Kind::PrefixUnaryExpression
    )
}

// lookup.go:482
// IsInConstContext traverses up the parent chain to determine if the node is within a const context without needing any
// persistent traversal scope tracking (which could be unreliable in the presence of `typeof` queries anyway!)
pub fn is_in_const_context(node: P<Node>) -> bool {
    // An expression is in a const context if an ancestor is a const type maybeAssertion expression
    let maybe_assertion = find_ancestor(node.parent(), |n| {
        // stop traversing at assertions or anything not an array/object literal, since only those create or transfer const-ness
        is_assertion_expression(n) || !is_const_context_propagating_kind(n.kind())
    });
    is_const_assertion(maybe_assertion.unwrap())
}

impl PseudoChecker {
    // lookup.go:494
    fn type_from_primitive_literal_prefix(node: P<Node>) -> Option<P<PseudoType>> {
        let p = node.as_prefix_unary_expression();
        let mut expr = node;
        if p.operator == Kind::PlusToken {
            expr = p.operand;
        }
        let inner = p.operand;
        if inner.kind() == Kind::BigIntLiteral {
            return Some(new_pseudo_type_maybe_const_location(
                node,
                new_pseudo_type_big_int_literal(expr),
                *PSEUDO_TYPE_BIG_INT,
            ));
        }
        if inner.kind() == Kind::NumericLiteral {
            return Some(new_pseudo_type_maybe_const_location(
                node,
                new_pseudo_type_numeric_literal(expr),
                *PSEUDO_TYPE_NUMBER,
            ));
        }
        debug::fail_bad_syntax_kind(&inner.kind_string(), &[])
    }

    // lookup.go:510
    fn type_from_type_assertion(
        &self,
        expression: P<Node>,
        type_node: P<Node>,
    ) -> Option<P<PseudoType>> {
        if is_const_type_reference(type_node) {
            return self.type_from_expression(expression);
        }
        Some(new_pseudo_type_direct(type_node))
    }

    // lookup.go:517
    fn type_from_function_like_expression(&self, node: P<Node>) -> P<PseudoType> {
        let d = node.function_like_data().unwrap();
        if let Some(full_signature) = d.full_signature() {
            return new_pseudo_type_direct(full_signature);
        }
        let return_type = self.create_return_from_signature(node).unwrap();
        let type_parameters = Self::clone_type_parameters(d.type_parameters());
        let parameters = self.clone_parameters(d.parameters());
        new_pseudo_type_single_call_signature(node, &parameters, &type_parameters, return_type)
    }

    // lookup.go:532
    fn clone_type_parameters(nodes: Option<P<NodeList>>) -> Vec<P<Node>> {
        let Some(nodes) = nodes else {
            return Vec::new();
        };
        if nodes.nodes().is_empty() {
            return Vec::new();
        }
        let mut result = Vec::with_capacity(nodes.nodes().len());
        result.extend(nodes.nodes().iter().map(|&e| {
            e.as_type_parameter_declaration();
            e
        }));
        result
    }
}

// lookup.go:546
fn is_undefined_pseudo_type(t: P<PseudoType>) -> bool {
    t.kind == PseudoTypeKind::Undefined
        || (t.kind == PseudoTypeKind::MaybeConstLocation
            && is_undefined_pseudo_type(t.as_pseudo_type_maybe_const_location().const_type))
}

// lookup.go:550
fn type_node_could_refer_to_undefined(node: P<Node>) -> bool {
    let mut node = node;
    while node.kind() == Kind::ParenthesizedType {
        node = node.as_parenthesized_type_node().type_;
    }
    match node.kind() {
        Kind::IntersectionType => {
            // TODO: why is this not `core.Every`? strada treated unions and intersections the same, but logically every intersection member needs to contain a possible `undefined`
            // for the result type to contain `undefined`. Likely a bug persisting from strada.
            node.as_intersection_type_node()
                .types()
                .nodes()
                .iter()
                .any(|n| type_node_could_refer_to_undefined(*n))
        }
        Kind::UnionType => node
            .as_union_type_node()
            .types()
            .nodes()
            .iter()
            .any(|n| type_node_could_refer_to_undefined(*n)),
        // these types require symbolic/type resolution to know if they definitely do or do not refer to `undefined`, so might (or definitely do)
        Kind::TypeReference
        | Kind::IndexedAccessType
        | Kind::TypeQuery
        | Kind::OptionalType
        | Kind::RestType
        | Kind::ImportType
        // suspect - should be treated as a union of both branches instead, likely a bug persisted from strada
        | Kind::ConditionalType
        // suspect - always refers to a subset of `string | number | symbol` for `keyof` or `symbol` for `unique`
        | Kind::TypeOperator
        // suspect - always refers to `never` or `boolean`, depending on kind - considered possibly-`undefined` referencing for strada compat
        | Kind::TypePredicate
        | Kind::UndefinedKeyword => true,
        _ => false, // all other keywords, literal types, function-y types, array/tuple types, type literals, template types, this types
    }
}

// lookup.go:578
// see this as the inverse of `canAddUndefined` in `expressionToTypeNode` in strada
pub fn could_already_refer_to_undefined_type(t: P<PseudoType>) -> bool {
    if t.kind == PseudoTypeKind::NoResult
        || t.kind == PseudoTypeKind::Inferred
        || is_undefined_pseudo_type(t)
    {
        return true;
    }
    if t.kind == PseudoTypeKind::MaybeConstLocation {
        let mc = t.as_pseudo_type_maybe_const_location();
        return could_already_refer_to_undefined_type(mc.regular_type); // if we're even asking this question, it's not a `const` location
    }
    if t.kind == PseudoTypeKind::Direct {
        // inspect the direct type node
        let node = t.as_pseudo_type_direct().type_node;
        return type_node_could_refer_to_undefined(node);
    }
    if t.kind == PseudoTypeKind::Union {
        return t
            .as_pseudo_type_union()
            .types
            .iter()
            .any(|t| could_already_refer_to_undefined_type(*t));
    }
    false
}

// lookup.go:597
fn is_optional_initialized_or_rest_parameter(node: P<Node>) -> bool {
    let p = node.as_parameter_declaration();
    if p.dot_dot_dot_token().is_some() || p.initializer().is_some() || p.question_token().is_some()
    {
        return true;
    }
    false
}

// lookup.go:610
// lastRequiredParamIndex returns the index just past the last required parameter
// in the list. A parameter is "required" if it has no question token, no initializer,
// and no rest token. This is computed in a single reverse pass so callers can
// determine "has required parameter after index i" with `i+1 < lastRequired`
// (equivalently, `i < lastRequired-1`) in O(1).
fn last_required_param_index(params: &[P<Node>]) -> usize {
    if let Some(result) = params.iter().enumerate().rev().into_iter().find_map(|(i, param)| {
        if is_optional_initialized_or_rest_parameter(*param) { None } else { Some(i + 1) }
    }) {
        return result;
    }
    0
}

// lookup.go:619
fn add_undefined_if_definitely_required(expr: P<PseudoType>) -> P<PseudoType> {
    // If `expr` doesn't already contain `| undefined` or a direct/inferred type that may contain `undefined`, add `| undefined`
    // in Strada, this reached into the checker to see if `undefined` was necessary, using `isRequiredOptionalParameter` from the emit resolver,
    // but that's not required on top of the syntactic checks to get the same behavior. (If we get the type wrong, it'll mismatch later and be discarded
    // for an inference error since corsa actually validates that pseudotypes semantically match the inferred type the checker produces)
    if could_already_refer_to_undefined_type(expr) {
        return expr; // will just error later, more like than not, unless the `undefined` is explicit in the pseudo
    }
    // Explicitly add an `| undefined`
    new_pseudo_type_union(&[expr, *PSEUDO_TYPE_UNDEFINED])
}

impl PseudoChecker {
    // lookup.go:631
    fn type_from_parameter(&self, node: P<Node>) -> Option<P<PseudoType>> {
        let parent = node.parent().unwrap();
        if parent.kind() == Kind::SetAccessor {
            return self.get_type_of_accessor(parent);
        }
        // Fast path: no initializer means we never need parameter position info.
        let p_decl = node.as_parameter_declaration();
        if p_decl.initializer().is_none() {
            if let Some(t) = p_decl.type_() {
                return Some(new_pseudo_type_direct(t));
            }
            return Some(new_pseudo_type_no_result(node));
        }
        let p = parent.parameters();
        let self_idx = p
            .iter()
            .position(|n| *n == node)
            .expect("parameter belongs to its parent's parameter list");
        let last_required = last_required_param_index(p);
        self.type_from_parameter_worker(node, self_idx, last_required)
    }

    // lookup.go:649
    fn type_from_parameter_worker(
        &self,
        node: P<Node>,
        self_idx: usize,
        last_required: usize,
    ) -> Option<P<PseudoType>> {
        let parent = node.parent().unwrap();
        if parent.kind() == Kind::SetAccessor {
            return self.get_type_of_accessor(parent);
        }
        let has_required_after = self_idx + 1 < last_required;
        let p_decl = node.as_parameter_declaration();
        let declared_type = p_decl.type_();
        if let Some(declared_type) = declared_type {
            let result = new_pseudo_type_direct(declared_type);
            // When the parameter has an initializer and strict null checks are enabled,
            // check if `| undefined` needs to be added because there are required parameters after this one.
            // This mirrors the checker's getTypeOfParameter which adds optionality for initialized parameters.
            if self.strict_null_checks && p_decl.initializer().is_some() && has_required_after {
                return Some(add_undefined_if_definitely_required(result));
            }
            return Some(result);
        }
        if let Some(initializer) = p_decl.initializer()
            && is_identifier(node.name().unwrap())
            && !is_contextually_typed(node)
        {
            let mut expr = self.type_from_expression(initializer);
            if let Some(e) = expr
                && e.kind == PseudoTypeKind::Inferred
                && e.as_pseudo_type_inferred().error_nodes.is_empty()
            {
                expr = Some(new_pseudo_type_inferred_with_errors(
                    e.as_pseudo_type_inferred().expression,
                    false,
                    &[node],
                )); // Move error up to the parameter
            }
            if !self.strict_null_checks {
                return expr;
            }
            if !has_required_after {
                return expr;
            }
            // if there is a non-optional parameter after this one, a `| undefined` will need to explicitly be emitted on this parameter, if it's not already there
            return Some(add_undefined_if_definitely_required(expr.unwrap()));
        }
        // TODO: In strada, the ID checker doesn't infer a parameter type from binding pattern names, but the real checker _does_!
        // This means ID won't let you write, say, `({elem}) => false` without an annotation, even though it's trivially of type
        // `(p0: {elem: any}) => boolean` and error-free under `noImplicitAny: false`!
        // That limitation is retained here.
        Some(new_pseudo_type_no_result(node))
    }

    // lookup.go:687
    fn clone_parameters(&self, nodes: Option<P<NodeList>>) -> Vec<P<PseudoParameter>> {
        let Some(nodes) = nodes else {
            return Vec::new();
        };
        if nodes.nodes().is_empty() {
            return Vec::new();
        }
        let last_required = last_required_param_index(nodes.nodes());
        let mut result = Vec::with_capacity(nodes.nodes().len());
        result.extend(nodes.nodes().iter().enumerate().map(|(i, &e)| {
            let p = e.as_parameter_declaration();
            let mut optional = p.question_token().is_some();
            if !optional && p.initializer().is_some() {
                // A parameter with an initializer is optional only if all subsequent
                // parameters are also optional/have initializers/are rest parameters.
                // This matches the checker's isOptionalParameter semantics.
                optional = i + 1 >= last_required;
            }
            new_pseudo_parameter(
                p.dot_dot_dot_token().is_some(),
                e.name().unwrap(),
                optional,
                self.type_from_parameter_worker(e, i, last_required).unwrap(),
            )
        }));
        result
    }
}

// lookup.go:715
fn is_contextually_typed(node: P<Node>) -> bool {
    find_ancestor(node.parent(), |n| {
        // Functions calls or parent type annotations (but not the return type of a function expression) may impact the inferred type and local inference is unreliable
        if is_call_expression(n) {
            return true;
        }
        if is_satisfies_expression(n) {
            return true;
        }
        if (is_variable_parameter_or_property(n) || is_assertion_expression(n))
            && n.type_node().is_some()
            && !is_const_assertion(n)
        {
            return true;
        }
        is_jsx_element(n) || is_jsx_expression(n)
    })
    .is_some()
}
