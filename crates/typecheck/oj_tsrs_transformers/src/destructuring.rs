use crate::*;

// FlattenLevel controls how deeply binding/assignment patterns are decomposed.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum FlattenLevel {
    All,        // Fully decompose all patterns into individual assignments/bindings
    ObjectRest, // Only decompose patterns containing object rest elements
}

// CreateAssignmentCallback is a callback used to create custom assignment expressions during destructuring flattening.
// When provided, the target will always be an Identifier, and the callback can wrap the assignment with additional logic
// (e.g., export expressions in CJS modules or namespace member assignments).
pub(crate) type CreateAssignmentCallback<'a> =
    &'a dyn Fn(P<Node>, P<Node>, Option<&TextRange>) -> P<Node>;

// FlattenDestructuringAssignment flattens a destructuring assignment expression into a sequence of
// individual property/element access assignments. Supports custom assignment callbacks for module
// export or namespace member expressions.
// destructuring.go:27
pub(crate) fn flatten_destructuring_assignment(
    tx: &Transformer,
    node: P<Node>, // VariableDeclaration | DestructuringAssignment
    needs_value: bool,
    level: FlattenLevel,
    create_assignment_callback: Option<CreateAssignmentCallback>,
) -> P<Node> {
    let mut f = new_flattener(tx, level);
    f.create_assignment_callback = create_assignment_callback;
    f.hoist_temp_variables = true;
    // Assignment mode callbacks
    f.emit_binding_or_assignment = Flattener::emit_assignment;
    f.create_array_binding_or_assignment_pattern = Flattener::create_array_assignment_pattern;
    f.create_object_binding_or_assignment_pattern = Flattener::create_object_assignment_pattern;
    f.create_array_binding_or_assignment_element = Flattener::create_array_assignment_element;
    f.flatten_destructuring_assignment(node, needs_value)
}

// pendingDecl tracks a pending variable declaration during binding flattening.
#[derive(Clone)]
struct PendingDecl {
    pending_expressions: Vec<P<Node>>,
    name: P<Node>,
    value: P<Node>,
    location: TextRange,
    original: Option<P<Node>>,
}

// FlattenDestructuringBinding flattens a binding pattern in a variable declaration or parameter
// into individual variable declarations. Returns a single VariableDeclaration, a SyntaxList of
// declarations, or nil.
// destructuring.go:56
pub(crate) fn flatten_destructuring_binding(
    tx: &Transformer,
    node: P<Node>, // VariableDeclaration | ParameterDeclaration | BindingElement
    rval: Option<P<Node>>,
    level: FlattenLevel,
    hoist_temp_variables: bool,
    skip_initializer: bool,
) -> Option<P<Node>> {
    let mut f = new_flattener(tx, level);
    f.hoist_temp_variables = hoist_temp_variables;
    // Binding mode callbacks
    f.emit_binding_or_assignment = Flattener::emit_binding;
    f.create_array_binding_or_assignment_pattern = Flattener::create_array_binding_pattern;
    f.create_object_binding_or_assignment_pattern = Flattener::create_object_binding_pattern;
    f.create_array_binding_or_assignment_element = Flattener::create_array_binding_element;
    f.flatten_destructuring_binding(node, rval, skip_initializer)
}

// flattener encapsulates the state and logic for flattening destructuring patterns.
// It is equivalent to TypeScript's FlattenContext in destructuring.ts.
struct Flattener<'a> {
    tx: &'a Transformer,
    level: FlattenLevel,

    create_assignment_callback: Option<CreateAssignmentCallback<'a>>,

    // State
    expressions: Vec<P<Node>>,
    declarations: Vec<PendingDecl>,
    has_transformed_prior_element: bool,
    hoist_temp_variables: bool,

    // Mode callbacks (set by FlattenDestructuringAssignment or FlattenDestructuringBinding)
    emit_binding_or_assignment:
        fn(&mut Flattener<'a>, P<Node>, P<Node>, TextRange, Option<P<Node>>),
    create_array_binding_or_assignment_pattern: fn(&mut Flattener<'a>, Vec<P<Node>>) -> P<Node>,
    create_object_binding_or_assignment_pattern: fn(&mut Flattener<'a>, Vec<P<Node>>) -> P<Node>,
    create_array_binding_or_assignment_element: fn(&mut Flattener<'a>, P<Node>) -> P<Node>,
}

// destructuring.go:97
fn new_flattener(tx: &Transformer, level: FlattenLevel) -> Flattener<'_> {
    Flattener {
        tx,
        level,
        create_assignment_callback: None,
        expressions: Vec::new(),
        declarations: Vec::new(),
        has_transformed_prior_element: false,
        hoist_temp_variables: false,
        emit_binding_or_assignment: Flattener::emit_assignment,
        create_array_binding_or_assignment_pattern: Flattener::create_array_assignment_pattern,
        create_object_binding_or_assignment_pattern: Flattener::create_object_assignment_pattern,
        create_array_binding_or_assignment_element: Flattener::create_array_assignment_element,
    }
}

#[derive(Clone, Copy)]
struct RestIdElemPair {
    id: P<Node>,
    element: P<Node>,
}

impl Flattener<'_> {
    fn factory(&self) -> &'static printer::NodeFactory {
        self.tx.factory()
    }

    fn visit_node(&self, node: Option<P<Node>>) -> Option<P<Node>> {
        self.tx.visitor().visit_node(node)
    }

    // --- Assignment mode callbacks ---

    // destructuring.go:106
    fn create_array_assignment_pattern(&mut self, elements: Vec<P<Node>>) -> P<Node> {
        let f = self.factory();
        f.new_array_literal_expression(f.new_node_list(elements), false)
    }

    // destructuring.go:110
    fn create_object_assignment_pattern(&mut self, elements: Vec<P<Node>>) -> P<Node> {
        let f = self.factory();
        f.new_object_literal_expression(f.new_node_list(elements), false)
    }

    // destructuring.go:114
    #[expect(
        clippy::unused_self,
        reason = "The binding and assignment element callbacks share a mutable flattener receiver."
    )]
    fn create_array_assignment_element(&mut self, expr: P<Node>) -> P<Node> {
        expr
    }

    // destructuring.go:118
    fn emit_assignment(
        &mut self,
        target: P<Node>,
        value: P<Node>,
        location: TextRange,
        original: Option<P<Node>>,
    ) {
        let expression = match self.create_assignment_callback {
            Some(callback) if ast::is_identifier(target) => {
                callback(target, value, Some(&location))
            }
            _ => {
                let expression = self
                    .factory()
                    .new_assignment_expression(self.visit_node(Some(target)).unwrap(), value);
                expression.set_loc(location);
                expression
            }
        };
        if let Some(original) = original {
            self.tx.emit_context().set_original(expression, original);
        }
        self.emit_expression(expression);
    }

    // --- Binding mode callbacks ---

    // destructuring.go:132
    fn create_array_binding_pattern(&mut self, elements: Vec<P<Node>>) -> P<Node> {
        let f = self.factory();
        f.new_binding_pattern(Kind::ArrayBindingPattern, f.new_node_list(elements))
    }

    // destructuring.go:136
    fn create_object_binding_pattern(&mut self, elements: Vec<P<Node>>) -> P<Node> {
        let f = self.factory();
        f.new_binding_pattern(Kind::ObjectBindingPattern, f.new_node_list(elements))
    }

    // destructuring.go:140
    fn create_array_binding_element(&mut self, expr: P<Node>) -> P<Node> {
        self.factory().new_binding_element(None, None, Some(expr), None)
    }

    // destructuring.go:144
    fn emit_binding(
        &mut self,
        target: P<Node>,
        value: P<Node>,
        location: TextRange,
        original: Option<P<Node>>,
    ) {
        let mut value = value;
        if !self.expressions.is_empty() {
            let mut expressions = std::mem::take(&mut self.expressions);
            expressions.push(value);
            value = self.factory().inline_expressions(&expressions).unwrap();
        }
        self.declarations.push(PendingDecl {
            pending_expressions: Vec::new(),
            name: target,
            value,
            location,
            original,
        });
    }

    // --- Shared helpers ---

    // destructuring.go:160
    fn emit_expression(&mut self, expr: P<Node>) {
        self.expressions.push(expr);
    }

    // destructuring.go:164
    fn ensure_identifier(
        &mut self,
        value: P<Node>,
        reuse_identifier_expressions: bool,
        location: TextRange,
    ) -> P<Node> {
        if reuse_identifier_expressions && ast::is_identifier(value) {
            return value;
        }
        let f = self.factory();
        let temp = f.new_temp_variable();
        if self.hoist_temp_variables {
            self.tx.emit_context().add_variable_declaration(temp);
            let assign = f.new_assignment_expression(temp, value);
            assign.set_loc(location);
            self.emit_expression(assign);
        } else {
            (self.emit_binding_or_assignment)(self, temp, value, location, None);
        }
        temp
    }

    // destructuring.go:180
    fn create_default_value_check(
        &mut self,
        value: P<Node>,
        default_value: P<Node>,
        location: TextRange,
    ) -> P<Node> {
        let value = self.ensure_identifier(value, true, location);
        let f = self.factory();
        f.new_conditional_expression(
            f.new_type_check(value, "undefined"),
            f.new_token(Kind::QuestionToken),
            default_value,
            f.new_token(Kind::ColonToken),
            value,
        )
    }

    // destructuring.go:191
    fn create_destructuring_property_access(
        &mut self,
        value: P<Node>,
        property_name: P<Node>,
    ) -> P<Node> {
        let f = self.factory();
        if ast::is_computed_property_name(property_name) {
            let visited = self.visit_node(property_name.expression()).unwrap();
            let argument_expression = self.ensure_identifier(visited, false, property_name.loc());
            f.new_element_access_expression(value, None, argument_expression, NodeFlags::None)
        } else if ast::is_string_or_numeric_literal_like(property_name)
            || ast::is_big_int_literal(property_name)
        {
            let argument_expression = property_name.clone_node(f);
            f.new_element_access_expression(value, None, argument_expression, NodeFlags::None)
        } else {
            let name = f.new_identifier(property_name.text());
            f.new_property_access_expression(value, None, name, NodeFlags::None)
        }
    }

    // --- Entry points ---

    // destructuring.go:206
    fn flatten_destructuring_assignment(&mut self, node: P<Node>, needs_value: bool) -> P<Node> {
        let mut node = node;
        let mut location = node.loc();
        let mut value: Option<P<Node>> = None;
        if ast::is_destructuring_assignment(node) {
            value = Some(node.as_binary_expression().right());
            while is_empty_array_literal(node.as_binary_expression().left)
                || is_empty_object_literal(node.as_binary_expression().left)
            {
                if ast::is_destructuring_assignment(value.unwrap()) {
                    node = value.unwrap();
                    location = node.loc();
                    value = Some(node.as_binary_expression().right());
                } else {
                    return self.visit_node(value).unwrap();
                }
            }
        }

        if let Some(v) = value {
            let mut v = self.visit_node(Some(v)).unwrap();
            if ast::is_identifier(v)
                && binding_or_assignment_element_assigns_to_name(node, v.text())
                || binding_or_assignment_element_contains_non_literal_computed_name(node)
            {
                v = self.ensure_identifier(v, false, location);
            } else if needs_value {
                v = self.ensure_identifier(v, true, location);
            } else if ast::node_is_synthesized(node) {
                location = v.loc();
            }
            value = Some(v);
        }

        self.flatten_binding_or_assignment_element(
            node,
            value,
            location,
            ast::is_destructuring_assignment(node),
        );

        if let Some(value) = value
            && needs_value
        {
            if self.expressions.is_empty() {
                return value;
            }
            self.expressions.push(value);
        }

        let res = self.factory().inline_expressions(&self.expressions);
        if let Some(res) = res {
            return res;
        }
        self.factory().new_omitted_expression()
    }

    // destructuring.go:246
    fn flatten_destructuring_binding(
        &mut self,
        node: P<Node>,
        rval: Option<P<Node>>,
        skip_initializer: bool,
    ) -> Option<P<Node>> {
        let mut node = node;
        if ast::is_variable_declaration(node) {
            let initializer = get_initializer_of_binding_or_assignment_element(Some(node));
            if let Some(initializer) = initializer
                && (ast::is_identifier(initializer)
                    && binding_or_assignment_element_assigns_to_name(node, initializer.text())
                    || binding_or_assignment_element_contains_non_literal_computed_name(node))
            {
                let visited = self.visit_node(Some(initializer)).unwrap();
                let initializer = self.ensure_identifier(visited, false, initializer.loc());
                node = self.factory().update_variable_declaration(
                    node,
                    node.name().unwrap(),
                    None,
                    None,
                    Some(initializer),
                );
            }
        }

        self.flatten_binding_or_assignment_element(node, rval, node.loc(), skip_initializer);

        if !self.expressions.is_empty() {
            let f = self.factory();
            let temp = f.new_temp_variable();
            if self.hoist_temp_variables {
                let value = f.inline_expressions(&self.expressions).unwrap();
                self.expressions.clear();
                (self.emit_binding_or_assignment)(self, temp, value, TextRange::default(), None);
            } else {
                self.tx.emit_context().add_variable_declaration(temp);
                let expressions = self.expressions.clone();
                let last = self.declarations.last_mut().unwrap();
                last.pending_expressions.push(f.new_assignment_expression(temp, last.value));
                last.pending_expressions.extend(expressions);
                last.value = temp;
            }
        }

        let mut decls: Vec<P<Node>> = Vec::with_capacity(self.declarations.len());
        decls.extend(self.declarations.iter().map(|pending| {
            let f = self.factory();
            let mut expr = pending.value;
            if !pending.pending_expressions.is_empty() {
                let mut expressions = pending.pending_expressions.clone();
                expressions.push(pending.value);
                expr = f.inline_expressions(&expressions).unwrap();
            }
            let decl = f.new_variable_declaration(pending.name, None, None, Some(expr));
            decl.set_loc(pending.location);
            if let Some(original) = pending.original {
                self.tx.emit_context().set_original(decl, original);
            }
            decl
        }));

        if decls.len() == 1 {
            return Some(decls[0]);
        }
        if decls.is_empty() {
            return None;
        }
        Some(self.factory().new_syntax_list(alloc_vec(decls)))
    }

    // --- Core flattening ---

    // destructuring.go:298
    fn flatten_binding_or_assignment_element(
        &mut self,
        element: P<Node>,
        value: Option<P<Node>>,
        location: TextRange,
        skip_initializer: bool,
    ) {
        let Some(binding_target) = ast::get_target_of_binding_or_assignment_element(element) else {
            return;
        };
        let mut value = value;
        if !skip_initializer {
            let initializer =
                self.visit_node(get_initializer_of_binding_or_assignment_element(Some(element)));
            if let Some(initializer) = initializer {
                if let Some(v) = value {
                    let mut v = self.create_default_value_check(v, initializer, location);
                    if !is_simple_copiable_expression(initializer)
                        && (ast::is_binding_pattern(binding_target)
                            || ast::is_assignment_pattern(binding_target))
                    {
                        v = self.ensure_identifier(v, true, location);
                    }
                    value = Some(v);
                } else {
                    value = Some(initializer);
                }
            } else if value.is_none() {
                value = Some(self.factory().new_void_zero_expression());
            }
        }

        if is_object_binding_or_assignment_pattern(Some(binding_target)) {
            self.flatten_object_binding_or_assignment_pattern(
                element,
                binding_target,
                value.unwrap(),
                location,
            );
        } else if is_array_binding_or_assignment_pattern(Some(binding_target)) {
            self.flatten_array_binding_or_assignment_pattern(
                element,
                binding_target,
                value.unwrap(),
                location,
            );
        } else {
            (self.emit_binding_or_assignment)(
                self,
                binding_target,
                value.unwrap(),
                location,
                Some(element),
            );
        }
    }

    // destructuring.go:327
    fn flatten_object_binding_or_assignment_pattern(
        &mut self,
        parent: P<Node>,
        pattern: P<Node>,
        value: P<Node>,
        location: TextRange,
    ) {
        let elements = ast::get_elements_of_binding_or_assignment_pattern(pattern);
        let num_elements = elements.len();
        let mut value = value;
        if num_elements != 1 {
            let reuse_identifier_expressions =
                !ast::is_declaration_binding_element(parent) || num_elements != 0;
            value = self.ensure_identifier(value, reuse_identifier_expressions, location);
        }
        let mut binding_elements: Vec<P<Node>> = Vec::new();
        let mut computed_temp_variables: Vec<P<Node>> = Vec::new();
        for (i, &element) in elements.iter().enumerate() {
            if get_rest_indicator_of_binding_or_assignment_element(element).is_none() {
                let property_name =
                    try_get_property_name_of_binding_or_assignment_element(element).unwrap();
                if self.level >= FlattenLevel::ObjectRest
                    && !element.subtree_facts().intersects(
                        SubtreeFacts::ContainsRestOrSpread
                            | SubtreeFacts::ContainsObjectRestOrSpread,
                    )
                    && !ast::get_target_of_binding_or_assignment_element(element)
                        .unwrap()
                        .subtree_facts()
                        .intersects(
                            SubtreeFacts::ContainsRestOrSpread
                                | SubtreeFacts::ContainsObjectRestOrSpread,
                        )
                    && !ast::is_computed_property_name(property_name)
                {
                    binding_elements.push(self.visit_node(Some(element)).unwrap());
                } else {
                    if !binding_elements.is_empty() {
                        let pattern_node = (self.create_object_binding_or_assignment_pattern)(
                            self,
                            std::mem::take(&mut binding_elements),
                        );
                        (self.emit_binding_or_assignment)(
                            self,
                            pattern_node,
                            value,
                            location,
                            Some(pattern),
                        );
                    }
                    let rhs_value = self.create_destructuring_property_access(value, property_name);
                    if ast::is_computed_property_name(property_name) {
                        computed_temp_variables
                            .push(rhs_value.as_element_access_expression().argument_expression);
                    }
                    self.flatten_binding_or_assignment_element(
                        element,
                        Some(rhs_value),
                        element.loc(),
                        false,
                    );
                }
            } else if i == num_elements - 1 {
                if !binding_elements.is_empty() {
                    let pattern_node = (self.create_object_binding_or_assignment_pattern)(
                        self,
                        std::mem::take(&mut binding_elements),
                    );
                    (self.emit_binding_or_assignment)(
                        self,
                        pattern_node,
                        value,
                        location,
                        Some(pattern),
                    );
                }
                let rhs_value = self.factory().new_rest_helper(
                    value,
                    elements,
                    if computed_temp_variables.is_empty() {
                        None
                    } else {
                        Some(&computed_temp_variables)
                    },
                    pattern.loc(),
                );
                self.flatten_binding_or_assignment_element(
                    element,
                    Some(rhs_value),
                    element.loc(),
                    false,
                );
            }
        }
        if !binding_elements.is_empty() {
            let pattern_node =
                (self.create_object_binding_or_assignment_pattern)(self, binding_elements);
            (self.emit_binding_or_assignment)(self, pattern_node, value, location, Some(pattern));
        }
    }

    // destructuring.go:379
    fn flatten_array_binding_or_assignment_pattern(
        &mut self,
        parent: P<Node>,
        pattern: P<Node>,
        value: P<Node>,
        location: TextRange,
    ) {
        let elements = ast::get_elements_of_binding_or_assignment_pattern(pattern);
        let num_elements = elements.len();
        let mut value = value;
        if num_elements != 1 && (self.level < FlattenLevel::ObjectRest || num_elements == 0)
            || elements.iter().all(|&e| ast::is_omitted_expression(e))
        {
            let reuse_identifier_expressions =
                !ast::is_declaration_binding_element(parent) || num_elements != 0;
            value = self.ensure_identifier(value, reuse_identifier_expressions, location);
        }
        let mut binding_elements: Vec<P<Node>> = Vec::new();
        let mut rest_containing_elements: Vec<RestIdElemPair> = Vec::new();
        for (i, &element) in elements.iter().enumerate() {
            if self.level >= FlattenLevel::ObjectRest {
                if element.subtree_facts().intersects(SubtreeFacts::ContainsObjectRestOrSpread)
                    || self.has_transformed_prior_element
                        && !is_simple_binding_or_assignment_element(element)
                {
                    self.has_transformed_prior_element = true;
                    let temp = self.factory().new_temp_variable();
                    if self.hoist_temp_variables {
                        self.tx.emit_context().add_variable_declaration(temp);
                    }
                    rest_containing_elements.push(RestIdElemPair { id: temp, element });
                    let e = (self.create_array_binding_or_assignment_element)(self, temp);
                    binding_elements.push(e);
                } else {
                    binding_elements.push(element);
                }
            } else if !ast::is_omitted_expression(element) {
                if get_rest_indicator_of_binding_or_assignment_element(element).is_none() {
                    let f = self.factory();
                    let rhs_value = f.new_element_access_expression(
                        value,
                        None,
                        f.new_numeric_literal(alloc_str(&i.to_string()), TokenFlags::None),
                        NodeFlags::None,
                    );
                    self.flatten_binding_or_assignment_element(
                        element,
                        Some(rhs_value),
                        element.loc(),
                        false,
                    );
                } else if i == num_elements - 1 {
                    let rhs_value = self.factory().new_array_slice_call(value, i);
                    self.flatten_binding_or_assignment_element(
                        element,
                        Some(rhs_value),
                        element.loc(),
                        false,
                    );
                }
            }
        }
        if !binding_elements.is_empty() {
            let pattern_node =
                (self.create_array_binding_or_assignment_pattern)(self, binding_elements);
            (self.emit_binding_or_assignment)(self, pattern_node, value, location, Some(pattern));
        }
        if !rest_containing_elements.is_empty() {
            for pair in rest_containing_elements {
                self.flatten_binding_or_assignment_element(
                    pair.element,
                    Some(pair.id),
                    pair.element.loc(),
                    false,
                );
            }
        }
    }
}

// --- Exported helper functions ---

// BindingOrAssignmentElementAssignsToName checks if any target in a binding/assignment pattern assigns to the given name.
// destructuring.go:428
fn binding_or_assignment_element_assigns_to_name(element: P<Node>, name: &str) -> bool {
    let Some(target) = ast::get_target_of_binding_or_assignment_element(element) else {
        return false;
    };
    if ast::is_binding_pattern(target) || ast::is_assignment_pattern(target) {
        return binding_or_assignment_pattern_assigns_to_name(target, name);
    } else if ast::is_identifier(target) {
        return target.text() == name;
    }
    false
}

// destructuring.go:441
fn binding_or_assignment_pattern_assigns_to_name(pattern: P<Node>, name: &str) -> bool {
    let elements = ast::get_elements_of_binding_or_assignment_pattern(pattern);
    elements.iter().any(|&element| binding_or_assignment_element_assigns_to_name(element, name))
}

// BindingOrAssignmentElementContainsNonLiteralComputedName checks if any element has a non-literal computed property name.
// destructuring.go:452
fn binding_or_assignment_element_contains_non_literal_computed_name(element: P<Node>) -> bool {
    let property_name = try_get_property_name_of_binding_or_assignment_element(element);
    if let Some(property_name) = property_name
        && ast::is_computed_property_name(property_name)
        && !ast::is_literal_expression(property_name.expression().unwrap())
    {
        return true;
    }
    let target = ast::get_target_of_binding_or_assignment_element(element);
    match target {
        Some(target) => {
            (ast::is_binding_pattern(target) || ast::is_assignment_pattern(target))
                && binding_or_assignment_pattern_contains_non_literal_computed_name(target)
        }
        None => false,
    }
}

// destructuring.go:461
fn binding_or_assignment_pattern_contains_non_literal_computed_name(pattern: P<Node>) -> bool {
    let elements = ast::get_elements_of_binding_or_assignment_pattern(pattern);
    elements.iter().any(|&e| binding_or_assignment_element_contains_non_literal_computed_name(e))
}

// GetInitializerOfBindingOrAssignmentElement returns the initializer/default value of a binding or assignment element.
// destructuring.go:467
fn get_initializer_of_binding_or_assignment_element(
    binding_element: Option<P<Node>>,
) -> Option<P<Node>> {
    let binding_element = binding_element?;
    if ast::is_declaration_binding_element(binding_element) {
        return binding_element.initializer();
    }
    if ast::is_property_assignment(binding_element) {
        let initializer = binding_element.initializer().unwrap();
        if ast::is_assignment_expression(initializer, true) {
            return Some(initializer.as_binary_expression().right());
        }
        return None;
    }
    if ast::is_shorthand_property_assignment(binding_element) {
        return binding_element.as_shorthand_property_assignment().object_assignment_initializer();
    }
    if ast::is_assignment_expression(binding_element, true) {
        return Some(binding_element.as_binary_expression().right());
    }
    if ast::is_spread_element(binding_element) {
        return get_initializer_of_binding_or_assignment_element(binding_element.expression());
    }
    None
}

// destructuring.go:494
fn is_object_binding_or_assignment_pattern(node: Option<P<Node>>) -> bool {
    matches!(node, Some(node) if node.kind() == Kind::ObjectBindingPattern || node.kind() == Kind::ObjectLiteralExpression)
}

// destructuring.go:498
fn is_array_binding_or_assignment_pattern(node: Option<P<Node>>) -> bool {
    matches!(node, Some(node) if node.kind() == Kind::ArrayBindingPattern || node.kind() == Kind::ArrayLiteralExpression)
}

// destructuring.go:502
fn is_simple_binding_or_assignment_element(element: P<Node>) -> bool {
    let target = ast::get_target_of_binding_or_assignment_element(element);
    let Some(target) = target else {
        return true;
    };
    if ast::is_omitted_expression(target) {
        return true;
    }
    let property_name = try_get_property_name_of_binding_or_assignment_element(element);
    if let Some(property_name) = property_name
        && !ast::is_property_name_literal(property_name)
    {
        return false;
    }
    let initializer = get_initializer_of_binding_or_assignment_element(Some(element));
    if let Some(initializer) = initializer
        && !is_simple_inlineable_expression(initializer)
    {
        return false;
    }
    if ast::is_binding_pattern(target) || ast::is_assignment_pattern(target) {
        return ast::get_elements_of_binding_or_assignment_pattern(target)
            .iter()
            .all(|&e| is_simple_binding_or_assignment_element(e));
    }
    ast::is_identifier(target)
}
