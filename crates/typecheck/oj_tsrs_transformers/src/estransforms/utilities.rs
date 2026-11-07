#![expect(
    clippy::redundant_pub_crate,
    reason = "Only the public API is re-exported; implementation helpers remain crate-private."
)]

// Port of estransforms/utilities.go.

use super::async_::{assignment_target_contains_super_property, is_update_expression};
use crate::*;
use tsrs_core::collections::OrderedSet;

// utilities.go:10
pub(crate) fn convert_class_declaration_to_class_expression(
    emit_context: P<EmitContext>,
    node: P<Node>,
) -> P<Node> {
    let c = node.as_class_declaration();
    let updated = emit_context.factory.new_class_expression(
        extract_modifiers(emit_context, node.modifiers(), !ModifierFlags::ExportDefault),
        node.name(),
        c.type_parameters(),
        c.heritage_clauses(),
        c.members(),
    );
    emit_context.set_original(updated, node);
    updated.set_loc(node.loc());
    updated
}

// utilities.go:23
pub(crate) fn create_not_null_condition(
    emit_context: P<EmitContext>,
    left: P<Node>,
    right: P<Node>,
    invert: bool,
) -> P<Node> {
    let mut token = Kind::ExclamationEqualsEqualsToken;
    let mut op = Kind::AmpersandAmpersandToken;
    if invert {
        token = Kind::EqualsEqualsEqualsToken;
        op = Kind::BarBarToken;
    }

    let f = &emit_context.factory;
    f.new_binary_expression(
        None,
        f.new_binary_expression(
            None,
            left,
            None,
            f.new_token(token),
            f.new_keyword_expression(Kind::NullKeyword),
        ),
        None,
        f.new_token(op),
        f.new_binary_expression(
            None,
            right,
            None,
            f.new_token(token),
            f.new_void_zero_expression(),
        ),
    )
}

// superAccessState tracks super property/element accesses and super property assignments
// within async function or async generator bodies. It is embedded by both asyncTransformer
// and forawaitTransformer to share the tracking logic.
// utilities.go:55
#[derive(Default)]
pub struct SuperAccessState {
    factory: OnceCell<&'static printer::NodeFactory>,

    // Keeps track of property names accessed on super (`super.x`) within async functions.
    pub(crate) captured_super_properties: RefCell<Option<OrderedSet<String>>>,
    // Whether the async function contains an element access on super (`super[x]`).
    pub(crate) has_super_element_access: Cell<bool>,
    pub(crate) has_super_property_assignment: Cell<bool>,

    pub(crate) super_binding: Cell<Option<P<Node>>>,
    pub(crate) super_index_binding: Cell<Option<P<Node>>>,
    pub(crate) super_access_visitor: OnceCell<NodeVisitor>,
}

impl SuperAccessState {
    fn factory(&self) -> &'static printer::NodeFactory {
        self.factory.get().unwrap()
    }

    fn super_access_visitor(&self) -> NodeVisitor {
        self.super_access_visitor.get().unwrap().clone()
    }

    // utilities.go:69
    pub(crate) fn init_super_access_visitor(
        &'static self,
        emit_context: P<EmitContext>,
        factory: &'static printer::NodeFactory,
    ) {
        let _ = self.factory.set(factory);
        let s = P::from_static(self);
        let _ = self.super_access_visitor.set(emit_context.new_node_visitor(Rc::new(
            move |_: &mut NodeVisitor, n: P<Node>| Some(s.visit_super_access_node(n)),
        )));
    }

    // visitSuperAccessNode walks the async/generator body and replaces super property/element
    // accesses with _super/_superIndex references. This is necessary because the async body
    // ends up inside a generator function where `super` is not valid.
    // utilities.go:77
    fn visit_super_access_node(&self, node: P<Node>) -> P<Node> {
        match node.kind() {
            Kind::CallExpression => {
                if ast::is_super_property(node.expression().unwrap()) {
                    return self.substitute_call_expression_with_super_access(
                        node,
                        &mut self.super_access_visitor(),
                    );
                }
                self.super_access_visitor().visit_each_child(Some(node)).unwrap()
            }
            Kind::PropertyAccessExpression => {
                if node.expression().unwrap().kind() == Kind::SuperKeyword {
                    // super.x → _super.x
                    return self.factory().new_property_access_expression(
                        self.super_binding.get().unwrap(),
                        None,
                        node.name().unwrap(),
                        NodeFlags::None,
                    );
                }
                self.super_access_visitor().visit_each_child(Some(node)).unwrap()
            }
            Kind::ElementAccessExpression => {
                if node.expression().unwrap().kind() == Kind::SuperKeyword {
                    // super[x] → _superIndex(x) or _superIndex(x).value
                    return self.create_super_element_access_in_async_method(
                        node.as_element_access_expression().argument_expression,
                    );
                }
                self.super_access_visitor().visit_each_child(Some(node)).unwrap()
            }
            // Don't recurse into non-arrow function scopes or classes
            Kind::FunctionExpression
            | Kind::FunctionDeclaration
            | Kind::MethodDeclaration
            | Kind::GetAccessor
            | Kind::SetAccessor
            | Kind::Constructor
            | Kind::ClassDeclaration
            | Kind::ClassExpression => node,
            _ => self.super_access_visitor().visit_each_child(Some(node)).unwrap(),
        }
    }

    // utilities.go:111
    pub(crate) fn substitute_super_accesses_in_body(&self, body: P<Node>) -> P<Node> {
        self.super_access_visitor().visit_node(Some(body)).unwrap()
    }

    // substituteCallExpressionWithSuperAccess handles super.x(args) and super[x](args).
    // utilities.go:116
    fn substitute_call_expression_with_super_access(
        &self,
        call: P<Node>,
        visitor: &mut NodeVisitor,
    ) -> P<Node> {
        let f = self.factory();
        let expression = call.as_call_expression().expression;

        let target = if ast::is_property_access_expression(expression) {
            // super.x(args) → _super.x.call(this, args)
            f.new_property_access_expression(
                self.super_binding.get().unwrap(),
                None,
                expression.as_property_access_expression().name,
                NodeFlags::None,
            )
        } else if ast::is_element_access_expression(expression) {
            // super[x](args) → _superIndex(x).call(this, args) or _superIndex(x).value.call(this, args)
            self.create_super_element_access_in_async_method(
                expression.as_element_access_expression().argument_expression,
            )
        } else {
            return visitor.visit_each_child(Some(call)).unwrap();
        };

        let call_target = f.new_property_access_expression(
            target,
            None,
            f.new_identifier("call"),
            NodeFlags::None,
        );

        let mut all_args: Vec<P<Node>> = Vec::new();
        all_args.push(f.new_this_expression());
        let visited_args = visitor.visit_nodes(Some(call.as_call_expression().arguments));
        if let Some(visited_args) = visited_args {
            all_args.extend_from_slice(visited_args.nodes());
        }

        let result = f.new_call_expression(
            call_target,
            None,
            None,
            f.new_node_list(all_args),
            NodeFlags::None,
        );
        result.set_loc(call.loc());
        result
    }

    // createSuperElementAccessInAsyncMethod creates _superIndex(x) or _superIndex(x).value.
    // utilities.go:158
    fn create_super_element_access_in_async_method(&self, argument_expression: P<Node>) -> P<Node> {
        let f = self.factory();
        let super_index_call = f.new_call_expression(
            self.super_index_binding.get().unwrap(),
            None,
            None,
            f.new_node_list(vec![argument_expression]),
            NodeFlags::None,
        );
        if self.has_super_property_assignment.get() {
            return f.new_property_access_expression(
                super_index_call,
                None,
                f.new_identifier("value"),
                NodeFlags::None,
            );
        }
        super_index_call
    }

    // createSuperAccessVariableStatement creates a variable named `_super` with accessor
    // properties for the given property names.
    //
    // Create a variable declaration with a getter/setter (if binding) definition for each name:
    //
    //	const _super = Object.create(null, {
    //	    x: { get: () => super.x },                           // read-only
    //	    x: { get: () => super.x, set: (v) => super.x = v }, // read-write
    //	});
    // utilities.go:182
    pub(crate) fn create_super_access_variable_statement(&self) -> P<Node> {
        let f = self.factory();
        let mut accessors: Vec<P<Node>> = Vec::new();

        let names: Vec<String> = self
            .captured_super_properties
            .borrow()
            .as_ref()
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        for name in names {
            let name = alloc_str(&name);
            let mut descriptor_properties: Vec<P<Node>> = Vec::new();

            // getter: get: () => super.name
            let getter_body = f.new_property_access_expression(
                f.new_keyword_expression(Kind::SuperKeyword),
                None,
                f.new_identifier(name),
                NodeFlags::None,
            );
            let getter_arrow = f.new_arrow_function(
                None,
                None,
                Some(f.new_node_list(vec![])),
                None,
                None,
                Some(f.new_token(Kind::EqualsGreaterThanToken)),
                Some(getter_body),
            );
            let getter =
                f.new_property_assignment(None, f.new_identifier("get"), None, None, getter_arrow);
            descriptor_properties.push(getter);

            if self.has_super_property_assignment.get() {
                // setter: set: v => super.name = v
                let v_param = f.new_parameter_declaration(
                    None,
                    None,
                    f.new_identifier("v"),
                    None,
                    None,
                    None,
                );
                let super_prop = f.new_property_access_expression(
                    f.new_keyword_expression(Kind::SuperKeyword),
                    None,
                    f.new_identifier(name),
                    NodeFlags::None,
                );
                let assign_expr = f.new_assignment_expression(super_prop, f.new_identifier("v"));
                let setter_arrow = f.new_arrow_function(
                    None,
                    None,
                    Some(f.new_node_list(vec![v_param])),
                    None,
                    None,
                    Some(f.new_token(Kind::EqualsGreaterThanToken)),
                    Some(assign_expr),
                );
                let setter = f.new_property_assignment(
                    None,
                    f.new_identifier("set"),
                    None,
                    None,
                    setter_arrow,
                );
                descriptor_properties.push(setter);
            }

            let descriptor =
                f.new_object_literal_expression(f.new_node_list(descriptor_properties), false);
            let accessor =
                f.new_property_assignment(None, f.new_identifier(name), None, None, descriptor);
            accessors.push(accessor);
        }

        let descriptors_object = f.new_object_literal_expression(f.new_node_list(accessors), true);

        let object_create_call = f.new_call_expression(
            f.new_property_access_expression(
                f.new_identifier("Object"),
                None,
                f.new_identifier("create"),
                NodeFlags::None,
            ),
            None,
            None,
            f.new_node_list(vec![f.new_keyword_expression(Kind::NullKeyword), descriptors_object]),
            NodeFlags::None,
        );

        let decl = f.new_variable_declaration(
            self.super_binding.get().unwrap(),
            None,
            None,
            Some(object_create_call),
        );
        let decl_list =
            f.new_variable_declaration_list(f.new_node_list(vec![decl]), NodeFlags::Const);
        f.new_variable_statement(None, decl_list)
    }

    // trackSuperAccess records super property/element accesses and super property assignments
    // for the enclosing async method body. Called from both the main visitor and auxiliary
    // visitors to ensure super accesses are tracked regardless of whether the node has
    // transform flags.
    // utilities.go:251
    pub(crate) fn track_super_access(&self, node: P<Node>) {
        if self.captured_super_properties.borrow().is_none() {
            return;
        }
        match node.kind() {
            Kind::PropertyAccessExpression => {
                if node.expression().unwrap().kind() == Kind::SuperKeyword {
                    self.captured_super_properties
                        .borrow_mut()
                        .as_mut()
                        .unwrap()
                        .insert(node.name().unwrap().text().to_string());
                }
            }
            Kind::ElementAccessExpression => {
                if node.expression().unwrap().kind() == Kind::SuperKeyword {
                    self.has_super_element_access.set(true);
                }
            }
            Kind::BinaryExpression => {
                let b = node.as_binary_expression();
                if ast::is_assignment_operator(b.operator_token.kind())
                    && assignment_target_contains_super_property(b.left)
                {
                    self.has_super_property_assignment.set(true);
                }
            }
            Kind::PrefixUnaryExpression => {
                if is_update_expression(node)
                    && assignment_target_contains_super_property(
                        node.as_prefix_unary_expression().operand,
                    )
                {
                    self.has_super_property_assignment.set(true);
                }
            }
            Kind::PostfixUnaryExpression
                if is_update_expression(node)
                    && assignment_target_contains_super_property(
                        node.as_postfix_unary_expression().operand,
                    ) =>
            {
                self.has_super_property_assignment.set(true);
            }
            _ => {}
        }
    }
}

// utilities.go:280
// createAccessorPropertyBackingField creates a private backing field for an `accessor` PropertyDeclaration.
pub(crate) fn create_accessor_property_backing_field(
    f: &printer::NodeFactory,
    node: P<Node>,
    modifiers: Option<P<ModifierList>>,
    initializer: Option<P<Node>>,
) -> P<Node> {
    f.update_property_declaration(
        node,
        modifiers,
        f.new_generated_private_name_for_node_ex(
            node.name().unwrap(),
            printer::AutoGenerateOptions { suffix: "_accessor_storage", ..Default::default() },
        ),
        None, /*postfixToken*/
        None, /*typeNode*/
        initializer,
    )
}
