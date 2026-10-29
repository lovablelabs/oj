#![expect(
    clippy::redundant_pub_crate,
    reason = "Only the public API is re-exported; implementation helpers remain crate-private."
)]

use crate::*;
use tsrs_checker::TypeReferenceSerializationKind;

pub(crate) struct MetadataSerializer {
    resolver: Resolver,
    language_version: ScriptTarget,
    strict_null_checks: bool,
    f: &'static printer::NodeFactory,
    ec: P<EmitContext>,
    c: Cell<MetadataSerializerContext>,
}

#[derive(Clone, Copy, Default)]
pub struct MetadataSerializerContext {
    pub(crate) current_lexical_scope: Option<P<Node>>,
    pub(crate) current_name_scope: Option<P<Node>>,
    pub(crate) serializing_conditional_type_branch: bool,
}

// typeserializer.go:26
pub(crate) fn new_metadata_serializer(
    resolver: Resolver,
    f: &'static printer::NodeFactory,
    ec: P<EmitContext>,
    language_version: ScriptTarget,
    strict_null_checks: bool,
) -> P<MetadataSerializer> {
    P::new(MetadataSerializer {
        resolver,
        language_version,
        f,
        ec,
        strict_null_checks,
        c: Cell::new(MetadataSerializerContext::default()),
    })
}

impl MetadataSerializer {
    // typeserializer.go:30
    fn set_context(&self, ctx: MetadataSerializerContext) {
        self.c.set(ctx);
    }

    // typeserializer.go:34
    pub(crate) fn serialize_type_of_node(
        &self,
        ctx: MetadataSerializerContext,
        node: P<Node>,
        container: Option<P<Node>>,
    ) -> P<Node> {
        let old_ctx = self.c.get();
        self.c.set(ctx);
        let result = self.serialize_type_of_node_(node, container);
        self.set_context(old_ctx);
        result
    }

    // typeserializer.go:41
    pub(crate) fn serialize_parameter_types_of_node(
        &self,
        ctx: MetadataSerializerContext,
        node: P<Node>,
        container: Option<P<Node>>,
    ) -> P<Node> {
        let old_ctx = self.c.get();
        self.c.set(ctx);
        let result = self.serialize_parameter_types_of_node_(node, container);
        self.set_context(old_ctx);
        result
    }

    // typeserializer.go:48
    pub(crate) fn serialize_return_type_of_node(
        &self,
        ctx: MetadataSerializerContext,
        node: P<Node>,
    ) -> P<Node> {
        let old_ctx = self.c.get();
        self.c.set(ctx);
        let result = self.serialize_return_type_of_node_(node);
        self.set_context(old_ctx);
        result
    }
}

// typeserializer.go:55
fn get_set_accessor_value_parameter(node: Option<P<Node>>) -> Option<P<Node>> {
    let node = node?;
    let parameters = node.parameters();
    if !parameters.is_empty() {
        if parameters.len() >= 2 && ast::is_this_parameter(parameters[0]) {
            return Some(parameters[1]);
        }
        return Some(parameters[0]);
    }
    None
}

// typeserializer.go:70
/*
 * Get the type annotation for the value parameter.
 *
 * @internal
 */
fn get_set_accessor_type_annotation_node(node: Option<P<Node>>) -> Option<P<Node>> {
    let p = get_set_accessor_value_parameter(node)?;
    p.type_node()
}

// typeserializer.go:78
fn get_accessor_type_node(node: P<Node>, container: P<Node>) -> Option<P<Node>> {
    let accessors = ast::get_all_accessor_declarations(container.members(), node);
    if accessors.set_accessor.is_some() {
        return get_set_accessor_type_annotation_node(accessors.set_accessor);
    }
    if let Some(get_accessor) = accessors.get_accessor {
        return get_accessor.type_node();
    }
    None
}

impl MetadataSerializer {
    // typeserializer.go:93
    /*
     * Serializes the type of a node for use with decorator type metadata.
     * @param node The node that should have its type serialized.
     */
    fn serialize_type_of_node_(&self, node: P<Node>, container: Option<P<Node>>) -> P<Node> {
        match node.kind() {
            Kind::PropertyDeclaration | Kind::Parameter => {
                self.serialize_type_node(node.type_node())
            }
            Kind::GetAccessor | Kind::SetAccessor => {
                self.serialize_type_node(get_accessor_type_node(node, container.unwrap()))
            }
            Kind::ClassDeclaration | Kind::ClassExpression | Kind::MethodDeclaration => {
                self.f.new_identifier("Function")
            }
            _ => self.f.new_void_zero_expression(),
        }
    }

    // typeserializer.go:110
    /*
     * Serializes the type of a node for use with decorator type metadata.
     * @param node The node that should have its type serialized.
     */
    fn serialize_parameter_types_of_node_(
        &self,
        node: P<Node>,
        container: Option<P<Node>>,
    ) -> P<Node> {
        let mut value_declaration: Option<P<Node>> = None;
        if ast::is_class_like(node) {
            value_declaration = ast::get_first_constructor_with_body(node);
        } else if ast::is_function_like(node) && ast::node_is_present(node.body()) {
            value_declaration = Some(node);
        }

        let Some(value_declaration) = value_declaration else {
            return self.f.new_array_literal_expression(self.f.new_node_list(vec![]), false);
        };

        let mut expressions: Vec<P<Node>> = Vec::new();
        let parameters = get_parameters_of_decorated_declaration(value_declaration, container);
        expressions.extend(
            parameters.map_or(&[] as &[P<Node>], |p| p.nodes()).iter().enumerate().filter_map(
                |(i, parameter)| {
                    let parameter = *parameter;
                    if i == 0
                        && ast::is_identifier(parameter.name().unwrap())
                        && parameter.name().unwrap().text() == "this"
                    {
                        return None;
                    }
                    if parameter.as_parameter_declaration().dot_dot_dot_token().is_some() {
                        Some(self.serialize_type_node(ast::get_rest_parameter_element_type(
                            parameter.type_node(),
                        )))
                    } else {
                        Some(self.serialize_type_of_node_(parameter, container))
                    }
                },
            ),
        );
        self.f.new_array_literal_expression(self.f.new_node_list(expressions), false)
    }
}

// typeserializer.go:137
fn get_parameters_of_decorated_declaration(
    node: P<Node>,
    container: Option<P<Node>>,
) -> Option<P<NodeList>> {
    if let Some(container) = container
        && node.kind() == Kind::GetAccessor
    {
        let acc = ast::get_all_accessor_declarations(container.members(), node);
        if let Some(set_accessor) = acc.set_accessor {
            return set_accessor.parameter_list();
        }
    }
    node.parameter_list()
}

impl MetadataSerializer {
    // typeserializer.go:151
    /*
     * Serializes the return type of a node for use with decorator type metadata.
     * @param node The node that should have its return type serialized.
     */
    fn serialize_return_type_of_node_(&self, node: P<Node>) -> P<Node> {
        if ast::is_function_like(node) && node.type_node().is_some() {
            return self.serialize_type_node(node.type_node());
        } else if ast::is_async_function(node) {
            return self.f.new_identifier("Promise");
        }
        self.f.new_void_zero_expression()
    }

    // typeserializer.go:178
    /*
     * Serializes a type node for use with decorator type metadata.
     *
     * Types are serialized in the following fashion:
     * - Void types point to "undefined" (e.g. "void 0")
     * - Function and Constructor types point to the global "Function" constructor.
     * - Interface types with a call or construct signature types point to the global
     *   "Function" constructor.
     * - Array and Tuple types point to the global "Array" constructor.
     * - Type predicates and booleans point to the global "Boolean" constructor.
     * - String literal types and strings point to the global "String" constructor.
     * - Enum and number types point to the global "Number" constructor.
     * - Symbol types point to the global "Symbol" constructor.
     * - Type references to classes (or class-like variables) point to the constructor for the class.
     * - Anything else points to the global "Object" constructor.
     *
     * @param node The type node to serialize.
     */
    fn serialize_type_node(&self, node: Option<P<Node>>) -> P<Node> {
        let Some(node) = node else {
            return self.f.new_identifier("Object");
        };

        let node = ast::skip_type_parentheses(node);

        match node.kind() {
            Kind::VoidKeyword | Kind::UndefinedKeyword | Kind::NeverKeyword => {
                return self.f.new_void_zero_expression();
            }
            Kind::FunctionType | Kind::ConstructorType => return self.f.new_identifier("Function"),
            Kind::ArrayType | Kind::TupleType => return self.f.new_identifier("Array"),
            Kind::TypePredicate => {
                if node.as_type_predicate_node().asserts_modifier.is_some() {
                    return self.f.new_void_zero_expression();
                }
                return self.f.new_identifier("Boolean");
            }
            Kind::BooleanKeyword => return self.f.new_identifier("Boolean"),
            Kind::TemplateLiteralType | Kind::StringKeyword => {
                return self.f.new_identifier("String");
            }
            Kind::ObjectKeyword => return self.f.new_identifier("Object"),
            Kind::LiteralType => {
                return self
                    .serialize_literal_of_literal_type_node(node.as_literal_type_node().literal)
                    .unwrap();
            }
            Kind::NumberKeyword => return self.f.new_identifier("Number"),
            Kind::BigIntKeyword => return self.serialize_big_int_constructor(),
            Kind::SymbolKeyword => return self.f.new_identifier("Symbol"),
            Kind::TypeReference => return self.serialize_type_reference_node(node),
            Kind::IntersectionType => {
                return self.serialize_union_or_intersection_constituents(
                    node.as_intersection_type_node().types().nodes(),
                    true,
                );
            }
            Kind::UnionType => {
                return self.serialize_union_or_intersection_constituents(
                    node.as_union_type_node().types().nodes(),
                    false,
                );
            }
            Kind::ConditionalType => {
                let mut c = self.c.get();
                let old_state = c.serializing_conditional_type_branch;
                c.serializing_conditional_type_branch = true;
                self.c.set(c);
                let ct = node.as_conditional_type_node();
                let result = self.serialize_union_or_intersection_constituents(
                    &[ct.true_type, ct.false_type],
                    false,
                );
                let mut c = self.c.get();
                c.serializing_conditional_type_branch = old_state;
                self.c.set(c);
                return result;
            }
            Kind::TypeOperator => {
                if node.as_type_operator_node().operator == Kind::ReadonlyKeyword {
                    return self.serialize_type_node(node.type_node());
                }
                // TODO: why is `unique symbol` not handled as `Symbol`? This falls back to `Object`
            }
            Kind::TypeQuery
            | Kind::IndexedAccessType
            | Kind::MappedType
            | Kind::TypeLiteral
            | Kind::AnyKeyword
            | Kind::UnknownKeyword
            | Kind::ThisType
            | Kind::ImportType
            // no meaningful serialization for these invalid-parse JSDoc types
            | Kind::JSDocAllType | Kind::JSDocVariadicType => {
                // These types fall back to Object.
            }

            // handle JSDoc types from an invalid parse
            Kind::JSDocNullableType | Kind::JSDocNonNullableType | Kind::JSDocOptionalType => {
                return self.serialize_type_node(node.type_node());
            }
            _ => panic!("Unexpected node kind: {:?}", node.kind()),
        }
        self.f.new_identifier("Object")
    }

    // typeserializer.go:242
    fn serialize_union_or_intersection_constituents(
        &self,
        types: &[P<Node>],
        is_intersection: bool,
    ) -> P<Node> {
        // Note when updating logic here also update `getEntityNameForDecoratorMetadata` in checker.ts so that aliases can be marked as referenced
        let mut serialized_type: Option<P<Node>> = None;
        for type_node in types {
            let type_node = ast::skip_type_parentheses(*type_node);
            if type_node.kind() == Kind::NeverKeyword {
                if is_intersection {
                    return self.f.new_void_zero_expression(); // Reduce to `never` in an intersection
                }
                continue; // Elide `never` in a union
            }

            if type_node.kind() == Kind::UnknownKeyword {
                if !is_intersection {
                    return self.f.new_identifier("Object"); // Reduce to `unknown` in a union
                }
                continue; // Elide `unknown` in an intersection
            }

            if type_node.kind() == Kind::AnyKeyword {
                return self.f.new_identifier("Object"); // Reduce to `any` in a union or intersection
            }

            if !self.strict_null_checks
                && ((ast::is_literal_type_node(type_node)
                    && type_node.as_literal_type_node().literal.kind() == Kind::NullKeyword)
                    || type_node.kind() == Kind::UndefinedKeyword)
            {
                continue; // Elide null and undefined from unions for metadata, just like what we did prior to the implementation of strict null checks
            }

            let serialized_constituent = self.serialize_type_node(Some(type_node));
            if ast::is_identifier(serialized_constituent)
                && serialized_constituent.as_identifier().text() == "Object"
            {
                // One of the individual is global object, return immediately
                return serialized_constituent;
            }

            // If there exists union that is not `void 0` expression, check if the the common type is identifier.
            // anything more complex and we will just default to Object
            if let Some(serialized_type) = serialized_type {
                // Different types
                if !self.equate_serialized_type_nodes(serialized_type, serialized_constituent) {
                    return self.f.new_identifier("Object");
                }
            } else {
                // Initialize the union type
                serialized_type = Some(serialized_constituent);
            }
        }

        // If we were able to find common type, use it
        if let Some(serialized_type) = serialized_type {
            return serialized_type;
        }
        self.f.new_void_zero_expression() // Fallback is only hit if all union constituents are null/undefined/never
    }

    // typeserializer.go:295
    fn serialize_literal_of_literal_type_node(&self, node: P<Node>) -> Option<P<Node>> {
        match node.kind() {
            Kind::StringLiteral | Kind::NoSubstitutionTemplateLiteral => {
                Some(self.f.new_identifier("String"))
            }
            Kind::PrefixUnaryExpression => {
                let operand = node.as_prefix_unary_expression().operand;
                match operand.kind() {
                    Kind::NumericLiteral | Kind::BigIntLiteral => {
                        self.serialize_literal_of_literal_type_node(operand)
                    }
                    _ => panic!("Unexpected node kind: {:?}", operand.kind()),
                }
            }
            Kind::NumericLiteral => Some(self.f.new_identifier("Number")),
            Kind::BigIntLiteral => Some(self.serialize_big_int_constructor()),
            Kind::TrueKeyword | Kind::FalseKeyword => Some(self.f.new_identifier("Boolean")),
            Kind::NullKeyword => Some(self.f.new_void_zero_expression()),
            _ => panic!("Unexpected node kind: {:?}", node.kind()),
        }
    }

    // typeserializer.go:326
    /*
     * Serializes a TypeReferenceNode to an appropriate JS constructor value for use with decorator type metadata.
     * @param node The type reference node.
     */
    fn serialize_type_reference_node(&self, node: P<Node>) -> P<Node> {
        let type_name = node.as_type_reference_node().type_name;
        let c = self.c.get();
        let serial_scope = c.current_name_scope.or(c.current_lexical_scope);
        let kind = self.resolver.get_type_reference_serialization_kind(
            self.ec.parse_node(Some(type_name)).unwrap(),
            self.ec.parse_node(serial_scope),
        );
        match kind {
            TypeReferenceSerializationKind::Unknown => {
                // From conditional type type reference that cannot be resolved is Similar to any or unknown
                if c.serializing_conditional_type_branch {
                    return self.f.new_identifier("Object");
                }

                let serialized = self.serialize_entity_name_as_expression_fallback(type_name);
                let temp = self.f.new_temp_variable();
                self.ec.add_variable_declaration(temp);
                self.f.new_conditional_expression(
                    self.f.new_type_check(
                        self.f.new_assignment_expression(temp, serialized),
                        "function",
                    ),
                    self.f.new_token(Kind::QuestionToken),
                    temp,
                    self.f.new_token(Kind::ColonToken),
                    self.f.new_identifier("Object"),
                )
            }
            TypeReferenceSerializationKind::TypeWithConstructSignatureAndValue => {
                self.serialize_entity_name_as_expression(type_name).unwrap()
            }
            TypeReferenceSerializationKind::VoidNullableOrNeverType => {
                self.f.new_void_zero_expression()
            }
            TypeReferenceSerializationKind::BigIntLikeType => self.serialize_big_int_constructor(),
            TypeReferenceSerializationKind::BooleanType => self.f.new_identifier("Boolean"),
            TypeReferenceSerializationKind::NumberLikeType => self.f.new_identifier("Number"),
            TypeReferenceSerializationKind::StringLikeType => self.f.new_identifier("String"),
            TypeReferenceSerializationKind::ArrayLikeType => self.f.new_identifier("Array"),
            TypeReferenceSerializationKind::ESSymbolType => self.f.new_identifier("Symbol"),
            TypeReferenceSerializationKind::TypeWithCallSignature => {
                self.f.new_identifier("Function")
            }
            TypeReferenceSerializationKind::Promise => self.f.new_identifier("Promise"),
            TypeReferenceSerializationKind::ObjectType => self.f.new_identifier("Object"),
        }
    }

    // typeserializer.go:388
    fn serialize_big_int_constructor(&self) -> P<Node> {
        if self.language_version >= ScriptTarget::ES2020 {
            return self.f.new_identifier("BigInt");
        }
        self.f.new_conditional_expression(
            self.f.new_type_check(self.f.new_identifier("BigInt"), "function"),
            self.f.new_token(Kind::QuestionToken),
            self.f.new_identifier("BigInt"),
            self.f.new_token(Kind::ColonToken),
            self.f.new_identifier("Object"),
        )
    }

    // typeserializer.go:405
    /*
     * Serializes an entity name as an expression for decorator type metadata.
     * @param node The entity name to serialize.
     */
    fn serialize_entity_name_as_expression(&self, node: P<Node>) -> Option<P<Node>> {
        match node.kind() {
            Kind::Identifier => {
                // Create a clone of the name with a new parent, and treat it as if it were
                // a source tree node for the purposes of the checker.
                let name = node.clone_node(self.f.as_node_factory());
                name.set_loc(node.loc());
                self.ec.unset_original(name); // make this identifier emulate a parse node, making it behave correctly when inspected by the module transforms
                name.set_parent(self.ec.parse_node(self.c.get().current_lexical_scope)); // ensure the parent is set to a parse tree node.
                Some(name)
            }
            Kind::QualifiedName => Some(self.serialize_qualified_name_as_expression(node)),
            _ => None,
        }
    }

    // typeserializer.go:425
    /*
     * Serializes an qualified name as an expression for decorator type metadata.
     * @param node The qualified name to serialize.
     */
    fn serialize_qualified_name_as_expression(&self, node: P<Node>) -> P<Node> {
        let qn = node.as_qualified_name();
        self.f.new_property_access_expression(
            self.serialize_entity_name_as_expression(qn.left).unwrap(),
            None,
            qn.right,
            NodeFlags::None,
        )
    }

    // typeserializer.go:433
    /*
     * Serializes an entity name which may not exist at runtime, but whose access shouldn't throw
     * @param node The entity name to serialize.
     */
    fn serialize_entity_name_as_expression_fallback(&self, node: P<Node>) -> P<Node> {
        if node.kind() == Kind::Identifier {
            // A -> typeof A !== "undefined" && A
            let copied = self.serialize_entity_name_as_expression(node).unwrap();
            return self.create_checked_value(copied, copied);
        }
        let qn = node.as_qualified_name();
        if qn.left.kind() == Kind::Identifier {
            // A.B -> typeof A !== "undefined" && A.B
            return self.create_checked_value(
                self.serialize_entity_name_as_expression(qn.left).unwrap(),
                self.serialize_entity_name_as_expression(node).unwrap(),
            );
        }
        // A.B.C -> typeof A !== "undefined" && (_a = A.B) !== void 0 && _a.C
        let left = self.serialize_entity_name_as_expression_fallback(qn.left);
        let temp = self.f.new_temp_variable();
        self.ec.add_variable_declaration(temp);
        let lb = left.as_binary_expression();
        self.f.new_logical_and_expression(
            self.f.new_logical_and_expression(
                lb.left,
                self.f.new_strict_inequality_expression(
                    self.f.new_assignment_expression(temp, lb.right()),
                    self.f.new_void_zero_expression(),
                ),
            ),
            self.f.new_property_access_expression(temp, None, qn.right, NodeFlags::None),
        )
    }

    // typeserializer.go:467
    /*
     * Produces an expression that results in `right` if `left` is not undefined at runtime:
     *
     * ```
     * typeof left !== "undefined" && right
     * ```
     *
     * We use `typeof L !== "undefined"` (rather than `L !== undefined`) since `L` may not be declared.
     * It's acceptable for this expression to result in `false` at runtime, as the result is intended to be
     * further checked by any containing expression.
     */
    fn create_checked_value(&self, left: P<Node>, right: P<Node>) -> P<Node> {
        self.f.new_logical_and_expression(
            self.f.new_strict_inequality_expression(
                self.f.new_type_of_expression(left),
                self.f.new_string_literal("undefined", TokenFlags::None),
            ),
            right,
        )
    }

    // typeserializer.go:474
    fn equate_serialized_type_nodes(&self, left: P<Node>, right: P<Node>) -> bool {
        // temp vars used in fallback
        if is_generated_identifier(self.ec, left) {
            return is_generated_identifier(self.ec, right);
        }
        // entity names
        if ast::is_identifier(left) {
            return ast::is_identifier(right) && left.text() == right.text();
        }
        if ast::is_property_access_expression(left) {
            return ast::is_property_access_expression(right)
                && self.equate_serialized_type_nodes(
                    left.expression().unwrap(),
                    right.expression().unwrap(),
                )
                && self.equate_serialized_type_nodes(left.name().unwrap(), right.name().unwrap());
        }
        // `void 0`
        if ast::is_void_expression(left) {
            return ast::is_void_expression(right)
                && ast::is_numeric_literal(left.expression().unwrap())
                && ast::is_numeric_literal(right.expression().unwrap())
                && left.expression().unwrap().text() == "0"
                && right.expression().unwrap().text() == "0";
        }
        // `"undefined"` or `"function"` in `typeof` checks
        if ast::is_string_literal(left) {
            return ast::is_string_literal(right) && left.text() == right.text();
        }
        // used in `typeof` checks for fallback
        if ast::is_type_of_expression(left) {
            return ast::is_type_of_expression(right)
                && self.equate_serialized_type_nodes(
                    left.expression().unwrap(),
                    right.expression().unwrap(),
                );
        }
        // parens in `typeof` checks with temps
        if ast::is_parenthesized_expression(left) {
            return ast::is_parenthesized_expression(right)
                && self.equate_serialized_type_nodes(
                    left.expression().unwrap(),
                    right.expression().unwrap(),
                );
        }
        // conditionals used in fallback
        if ast::is_conditional_expression(left) {
            let (l, r) = (left.as_conditional_expression(), right_conditional(right));
            return ast::is_conditional_expression(right)
                && self.equate_serialized_type_nodes(l.condition, r.unwrap().condition)
                && self.equate_serialized_type_nodes(l.when_true, r.unwrap().when_true)
                && self.equate_serialized_type_nodes(l.when_false, r.unwrap().when_false);
        }
        // logical binary and assignments used in fallback
        if ast::is_binary_expression(left) {
            if !ast::is_binary_expression(right) {
                return false;
            }
            let (l, r) = (left.as_binary_expression(), right.as_binary_expression());
            return l.operator_token.kind() == r.operator_token.kind()
                && self.equate_serialized_type_nodes(l.left, r.left)
                && self.equate_serialized_type_nodes(l.right(), r.right());
        }
        false
    }
}

fn right_conditional(right: P<Node>) -> Option<&'static ast::ConditionalExpression> {
    if ast::is_conditional_expression(right) {
        Some(right.as_conditional_expression())
    } else {
        None
    }
}
