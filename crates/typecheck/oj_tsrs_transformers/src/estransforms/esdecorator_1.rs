#![expect(
    clippy::redundant_pub_crate,
    reason = "Only the public API is re-exported; implementation helpers remain crate-private."
)]

// esdecorator.go lines 1-1230 (types, constructor, lexical state, visitors, class transform, constructor).

use printer::EmitFlags;
use tsrs_core::collections::OrderedMap;

use super::*;

// esdecorator.go:48
// lexicalEntryKind discriminates the kind of lexical scope entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LexicalEntryKind {
    Class,
    ClassElement,
    Name,
    Other,
}

// esdecorator.go:59
// lexicalEntry represents a single entry in the lexical scope stack used to track
// nested class declarations and their state during transformation.
pub(crate) struct LexicalEntry {
    kind: LexicalEntryKind,
    next: Option<P<LexicalEntry>>,
    class_info_data: Option<P<ClassInfo>>,
    saved_pending_expressions: RefCell<Option<Vec<P<Node>>>>,
    class_this_data: Cell<Option<P<Node>>>,
    class_super_data: Cell<Option<P<Node>>>,
    depth: Cell<u32>,
}

// esdecorator.go:70
// memberInfo stores decoration-related data for a single class element.
#[derive(Default)]
pub struct MemberInfo {
    pub(crate) member_decorators_name: Cell<Option<P<Node>>>, // used in class definition step 4.a
    pub(crate) member_initializers_name: Cell<Option<P<Node>>>, // used in class definition step 12 and constructor evaluation step 2.a
    pub(crate) member_extra_initializers_name: Cell<Option<P<Node>>>, // used in class definition step 12 and constructor evaluation step 2.b
    pub(crate) member_descriptor_name: Cell<Option<P<Node>>>,
}

// esdecorator.go:78
// classInfo stores all transformation data for a single decorated class.
pub(crate) struct ClassInfo {
    class_decorators_name: Cell<Option<P<Node>>>, // used in class definition step 2
    class_descriptor_name: Cell<Option<P<Node>>>, // used in class definition step 10
    class_extra_initializers_name: Cell<Option<P<Node>>>, // used in class definition step 13
    pub(crate) class_this: Cell<Option<P<Node>>>, // `_classThis`, if needed.
    class_super: Cell<Option<P<Node>>>,           // `_classSuper`, if needed.
    pub(crate) metadata_reference: Cell<Option<P<Node>>>,
    pub(crate) member_infos: RefCell<OrderedMap<P<Node>, P<MemberInfo>>>, // used in class definition step 4.a, 12, and constructor evaluation
    pub(crate) instance_method_extra_initializers_name: Cell<Option<P<Node>>>, // used in constructor evaluation step 1
    pub(crate) static_method_extra_initializers_name: Cell<Option<P<Node>>>, // used in class definition step 11
    pub(crate) static_non_field_decoration_statements: RefCell<Vec<P<Node>>>,
    pub(crate) non_static_non_field_decoration_statements: RefCell<Vec<P<Node>>>,
    pub(crate) static_field_decoration_statements: RefCell<Vec<P<Node>>>,
    pub(crate) non_static_field_decoration_statements: RefCell<Vec<P<Node>>>,
    pub(crate) has_static_initializers: Cell<bool>,
    has_non_ambient_instance_fields: Cell<bool>,
    has_static_private_class_elements: Cell<bool>,
    pub(crate) pending_static_initializers: RefCell<Vec<P<Node>>>,
    pub(crate) pending_instance_initializers: RefCell<Vec<P<Node>>>,
}

// esdecorator.go:100
pub(crate) struct EsDecoratorTransformer {
    pub(crate) base: Transformer,
    top: Cell<Option<P<LexicalEntry>>>,
    pub(crate) class_info_stack: Cell<Option<P<ClassInfo>>>,
    pub(crate) class_this: Cell<Option<P<Node>>>,
    pub(crate) class_super: Cell<Option<P<Node>>>,
    pub(crate) pending_expressions: RefCell<Option<Vec<P<Node>>>>,
    outer_this: Cell<Option<P<Node>>>,
    should_transform_private_static_elements_in_file: Cell<bool>,
    outer_this_visitor: OnceCell<NodeVisitor>,
    pub(crate) discarded_visitor: OnceCell<NodeVisitor>,
    pub(crate) modifier_visitor: OnceCell<NodeVisitor>,
    export_stripping_modifier_visitor: OnceCell<NodeVisitor>,
    class_element_visitor: OnceCell<NodeVisitor>,
    non_constructor_class_element_visitor: OnceCell<NodeVisitor>,
    constructor_class_element_visitor: OnceCell<NodeVisitor>,
    pub(crate) array_assignment_visitor: OnceCell<NodeVisitor>,
    pub(crate) object_assignment_visitor: OnceCell<NodeVisitor>,
    pub(crate) static_only_modifier_visitor: OnceCell<NodeVisitor>,
    pub(crate) async_only_modifier_visitor: OnceCell<NodeVisitor>,
    pub(crate) accessor_stripping_modifier_visitor: OnceCell<NodeVisitor>,
}

// esdecorator.go:124
pub(crate) fn new_es_decorator_transformer(opts: &TransformOptions) -> Option<P<Transformer>> {
    // When experimentalDecorators is set, the legacy decorator transformer handles all
    // decorators. When targeting ESNext with useDefineForClassFields, there's nothing to
    // transform. In either case every node would be returned unchanged, so skip entirely.
    if opts.compiler_options.experimental_decorators.is_true()
        || (opts.compiler_options.get_emit_script_target() >= ScriptTarget::ES_NEXT
            && opts.compiler_options.get_use_define_for_class_fields())
    {
        return None;
    }
    let tx = P::new(EsDecoratorTransformer {
        base: Transformer::default(),
        top: Cell::new(None),
        class_info_stack: Cell::new(None),
        class_this: Cell::new(None),
        class_super: Cell::new(None),
        pending_expressions: RefCell::new(None),
        outer_this: Cell::new(None),
        should_transform_private_static_elements_in_file: Cell::new(false),
        outer_this_visitor: OnceCell::new(),
        discarded_visitor: OnceCell::new(),
        modifier_visitor: OnceCell::new(),
        export_stripping_modifier_visitor: OnceCell::new(),
        class_element_visitor: OnceCell::new(),
        non_constructor_class_element_visitor: OnceCell::new(),
        constructor_class_element_visitor: OnceCell::new(),
        array_assignment_visitor: OnceCell::new(),
        object_assignment_visitor: OnceCell::new(),
        static_only_modifier_visitor: OnceCell::new(),
        async_only_modifier_visitor: OnceCell::new(),
        accessor_stripping_modifier_visitor: OnceCell::new(),
    });
    let result = tx.get().base.new_transformer(
        Rc::new(move |_: &mut NodeVisitor, n: P<Node>| tx.visit(n)),
        Some(opts.context),
    );
    let ec = tx.base.emit_context();
    let mk = |f: fn(&EsDecoratorTransformer, P<Node>) -> Option<P<Node>>| {
        ec.new_node_visitor(Rc::new(move |_: &mut NodeVisitor, n: P<Node>| f(&tx, n)))
    };
    let _ = tx.outer_this_visitor.set(mk(EsDecoratorTransformer::outer_this_visit));
    let _ = tx.discarded_visitor.set(mk(EsDecoratorTransformer::discarded_value_visit));
    let _ = tx.modifier_visitor.set(mk(EsDecoratorTransformer::modifier_visitor_visit));
    let _ = tx
        .export_stripping_modifier_visitor
        .set(mk(EsDecoratorTransformer::export_stripping_modifier_visit));
    let _ = tx.class_element_visitor.set(mk(EsDecoratorTransformer::class_element_visitor_visit));
    let _ = tx
        .non_constructor_class_element_visitor
        .set(mk(EsDecoratorTransformer::non_constructor_class_element_visit));
    let _ = tx
        .constructor_class_element_visitor
        .set(mk(EsDecoratorTransformer::constructor_class_element_visit));
    let _ =
        tx.array_assignment_visitor.set(mk(EsDecoratorTransformer::visit_array_assignment_element));
    let _ = tx
        .object_assignment_visitor
        .set(mk(EsDecoratorTransformer::visit_object_assignment_element));
    let _ = tx.static_only_modifier_visitor.set(ec.new_node_visitor(Rc::new(
        |_: &mut NodeVisitor, node: P<Node>| {
            if node.kind() == Kind::StaticKeyword { Some(node) } else { None }
        },
    )));
    let _ = tx.async_only_modifier_visitor.set(ec.new_node_visitor(Rc::new(
        |_: &mut NodeVisitor, node: P<Node>| {
            if node.kind() == Kind::AsyncKeyword { Some(node) } else { None }
        },
    )));
    let _ = tx.accessor_stripping_modifier_visitor.set(ec.new_node_visitor(Rc::new(
        |_: &mut NodeVisitor, node: P<Node>| {
            if node.kind() == Kind::AccessorKeyword { None } else { Some(node) }
        },
    )));
    Some(result)
}

impl EsDecoratorTransformer {
    fn new_entry(
        &self,
        kind: LexicalEntryKind,
        class_info_data: Option<P<ClassInfo>>,
        saved_pending_expressions: Option<Vec<P<Node>>>,
    ) -> P<LexicalEntry> {
        P::new(LexicalEntry {
            kind,
            next: self.top.get(),
            class_info_data,
            saved_pending_expressions: RefCell::new(saved_pending_expressions),
            class_this_data: Cell::new(None),
            class_super_data: Cell::new(None),
            depth: Cell::new(0),
        })
    }

    // esdecorator.go:165
    fn update_state(&self) {
        self.class_info_stack.set(None);
        self.class_this.set(None);
        self.class_super.set(None);
        let Some(top) = self.top.get() else {
            return;
        };
        match top.kind {
            LexicalEntryKind::Class => self.class_info_stack.set(top.class_info_data),
            LexicalEntryKind::ClassElement => {
                self.class_info_stack.set(top.next.unwrap().class_info_data);
                self.class_this.set(top.class_this_data.get());
                self.class_super.set(top.class_super_data.get());
            }
            LexicalEntryKind::Name => {
                let grandparent = top.next.unwrap().next.unwrap().next;
                if let Some(grandparent) =
                    grandparent.filter(|g| g.kind == LexicalEntryKind::ClassElement)
                {
                    self.class_info_stack.set(grandparent.next.unwrap().class_info_data);
                    self.class_this.set(grandparent.class_this_data.get());
                    self.class_super.set(grandparent.class_super_data.get());
                }
            }
            LexicalEntryKind::Other => {}
        }
    }

    // esdecorator.go:189
    fn enter_class(&self, ci: Option<P<ClassInfo>>) {
        let saved = self.pending_expressions.borrow_mut().take();
        self.top.set(Some(self.new_entry(LexicalEntryKind::Class, ci, saved)));
        self.update_state();
    }

    // esdecorator.go:200
    fn exit_class(&self) {
        let top = self.top.get();
        assert!(
            top.is_some_and(|t| t.kind == LexicalEntryKind::Class),
            "Incorrect value for top.kind. Expected top.kind to be 'class'"
        );
        let top = top.unwrap();
        *self.pending_expressions.borrow_mut() = top.saved_pending_expressions.borrow_mut().take();
        self.top.set(top.next);
        self.update_state();
    }

    // esdecorator.go:207
    pub(crate) fn enter_class_element(&self, node: P<Node>) {
        assert!(
            self.top.get().is_some_and(|t| t.kind == LexicalEntryKind::Class),
            "Incorrect value for top.kind. Expected top.kind to be 'class'"
        );
        let top = self.new_entry(LexicalEntryKind::ClassElement, None, None);
        self.top.set(Some(top));
        if (ast::is_class_static_block_declaration(node)
            || ast::is_property_declaration(node) && ast::has_static_modifier(node))
            && let Some(ci) = top.next.unwrap().class_info_data
        {
            top.class_this_data.set(ci.class_this.get());
            top.class_super_data.set(ci.class_super.get());
        }
        self.update_state();
    }

    // esdecorator.go:222
    pub(crate) fn exit_class_element(&self) {
        let top = self.top.get();
        assert!(
            top.is_some_and(|t| t.kind == LexicalEntryKind::ClassElement),
            "Incorrect value for top.kind. Expected top.kind to be 'class-element'"
        );
        let top = top.unwrap();
        assert!(
            top.next.is_some_and(|t| t.kind == LexicalEntryKind::Class),
            "Incorrect value for top.next.kind. Expected top.next.kind to be 'class'"
        );
        self.top.set(top.next);
        self.update_state();
    }

    // esdecorator.go:229
    pub(crate) fn enter_name(&self) {
        assert!(
            self.top.get().is_some_and(|t| t.kind == LexicalEntryKind::ClassElement),
            "Incorrect value for top.kind. Expected top.kind to be 'class-element'"
        );
        self.top.set(Some(self.new_entry(LexicalEntryKind::Name, None, None)));
        self.update_state();
    }

    // esdecorator.go:238
    pub(crate) fn exit_name(&self) {
        let top = self.top.get();
        assert!(
            top.is_some_and(|t| t.kind == LexicalEntryKind::Name),
            "Incorrect value for top.kind. Expected top.kind to be 'name'"
        );
        self.top.set(top.unwrap().next);
        self.update_state();
    }

    // esdecorator.go:244
    fn enter_other(&self) {
        if let Some(top) = self.top.get().filter(|t| t.kind == LexicalEntryKind::Other) {
            assert_eq!(self.pending_expressions.borrow().as_ref().map_or(0, std::vec::Vec::len), 0);
            top.depth.set(top.depth.get() + 1);
        } else {
            let saved = self.pending_expressions.borrow_mut().take();
            self.top.set(Some(self.new_entry(LexicalEntryKind::Other, None, saved)));
            self.update_state();
        }
    }

    // esdecorator.go:259
    fn exit_other(&self) {
        let top = self.top.get();
        assert!(
            top.is_some_and(|t| t.kind == LexicalEntryKind::Other),
            "Incorrect value for top.kind. Expected top.kind to be 'other'"
        );
        let top = top.unwrap();
        if top.depth.get() > 0 {
            assert_eq!(self.pending_expressions.borrow().as_ref().map_or(0, std::vec::Vec::len), 0);
            top.depth.set(top.depth.get() - 1);
        } else {
            *self.pending_expressions.borrow_mut() =
                top.saved_pending_expressions.borrow_mut().take();
            self.top.set(top.next);
            self.update_state();
        }
    }

    // esdecorator.go:271
    fn visit_source_file(&self, node: P<Node>) -> P<Node> {
        self.top.set(None);
        self.should_transform_private_static_elements_in_file.set(false);
        let visited = self.base.visitor().visit_each_child(Some(node)).unwrap();
        let ec = self.base.emit_context();
        ec.add_emit_helper(visited, &ec.read_emit_helpers());
        if self.should_transform_private_static_elements_in_file.get() {
            ec.add_emit_flags(visited, EmitFlags::TransformPrivateStaticElements);
            self.should_transform_private_static_elements_in_file.set(false);
        }
        visited
    }

    // esdecorator.go:283
    fn outer_this_visit(&self, n: P<Node>) -> Option<P<Node>> {
        if !n.subtree_facts().intersects(SubtreeFacts::ContainsLexicalThis)
            && n.kind() != Kind::ThisKeyword
        {
            return Some(n);
        }
        if n.kind() == Kind::ThisKeyword {
            if self.outer_this.get().is_none() {
                self.outer_this.set(Some(self.base.factory().new_unique_name_ex(
                    "_outerThis",
                    printer::AutoGenerateOptions {
                        flags: printer::GeneratedIdentifierFlags::Optimistic,
                        ..Default::default()
                    },
                )));
            }
            return self.outer_this.get();
        }
        self.outer_this_visitor.get().unwrap().clone().visit_each_child(Some(n))
    }

    // esdecorator.go:298
    fn should_visit_node(&self, node: P<Node>) -> bool {
        node.subtree_facts().intersects(SubtreeFacts::ContainsDecorators)
            || (self.class_this.get().is_some()
                && node.subtree_facts().intersects(SubtreeFacts::ContainsLexicalThis))
            || (self.class_this.get().is_some()
                && self.class_super.get().is_some()
                && node.subtree_facts().intersects(SubtreeFacts::ContainsLexicalSuper))
    }
}

impl EsDecoratorTransformer {
    // esdecorator.go:304
    fn visit(&self, node: P<Node>) -> Option<P<Node>> {
        if node.kind() == Kind::SourceFile {
            return Some(self.visit_source_file(node));
        }
        if !self.should_visit_node(node) {
            return Some(node);
        }
        match node.kind() {
            // Decorators are elided. In Strada, a separate `modifierVisitor` drops decorators
            // before they reach `visitor` via visitEachChild. Here, `visit` serves as both
            // visitors, so decorators from modifier lists reach it directly.
            Kind::Decorator => None,
            Kind::ClassDeclaration => self.visit_class_declaration(node),
            Kind::ClassExpression => Some(self.visit_class_expression(node)),
            Kind::Constructor | Kind::PropertyDeclaration | Kind::ClassStaticBlockDeclaration => {
                panic!("Not supported outside of a class. Use 'classElementVisitor' instead.");
            }
            Kind::Parameter => Some(self.visit_parameter_declaration(node)),
            // Support NamedEvaluation to ensure the correct class name for class expressions.
            Kind::BinaryExpression => Some(self.visit_binary_expression(node, false /*discarded*/)),
            Kind::PropertyAssignment | Kind::VariableDeclaration | Kind::BindingElement => {
                Some(self.visit_named_evaluation_site(node, node.initializer()))
            }
            Kind::ExportAssignment => Some(self.visit_export_assignment(node)),
            Kind::ThisKeyword => Some(self.visit_this_expression(node)),
            Kind::ForStatement => Some(self.visit_for_statement(node)),
            Kind::ExpressionStatement => Some(self.visit_expression_statement(node)),
            Kind::ParenthesizedExpression => {
                Some(self.visit_parenthesized_expression(node, false /*discarded*/))
            }
            Kind::PartiallyEmittedExpression => {
                Some(self.visit_partially_emitted_expression(node, false /*discarded*/))
            }
            Kind::CallExpression => Some(self.visit_call_expression(node)),
            Kind::TaggedTemplateExpression => Some(self.visit_tagged_template_expression(node)),
            Kind::PrefixUnaryExpression | Kind::PostfixUnaryExpression => {
                Some(self.visit_pre_or_postfix_unary_expression(node, false /*discarded*/))
            }
            Kind::PropertyAccessExpression => Some(self.visit_property_access_expression(node)),
            Kind::ElementAccessExpression => Some(self.visit_element_access_expression(node)),
            Kind::ComputedPropertyName => Some(self.visit_computed_property_name(node)),
            Kind::MethodDeclaration
            | Kind::SetAccessor
            | Kind::GetAccessor
            | Kind::FunctionExpression
            | Kind::FunctionDeclaration => {
                self.enter_other();
                let result = self.base.visitor().visit_each_child(Some(node));
                self.exit_other();
                result
            }
            _ => self.base.visitor().visit_each_child(Some(node)),
        }
    }

    // esdecorator.go:369
    #[expect(
        clippy::unused_self,
        reason = "Node visitor callbacks share the transformer receiver used by the visitor factory."
    )]
    fn modifier_visitor_visit(&self, node: P<Node>) -> Option<P<Node>> {
        if node.kind() == Kind::Decorator {
            return None;
        }
        Some(node)
    }

    // esdecorator.go:376
    fn class_element_visitor_visit(&self, node: P<Node>) -> Option<P<Node>> {
        match node.kind() {
            Kind::Constructor => self.visit_constructor_declaration(node),
            Kind::MethodDeclaration => self.visit_method_declaration(node),
            Kind::GetAccessor => self.visit_get_accessor_declaration(node),
            Kind::SetAccessor => self.visit_set_accessor_declaration(node),
            Kind::PropertyDeclaration => self.visit_property_declaration(node),
            Kind::ClassStaticBlockDeclaration => self.visit_class_static_block_declaration(node),
            _ => self.visit(node),
        }
    }

    // esdecorator.go:395
    fn discarded_value_visit(&self, node: P<Node>) -> Option<P<Node>> {
        match node.kind() {
            Kind::PrefixUnaryExpression | Kind::PostfixUnaryExpression => {
                Some(self.visit_pre_or_postfix_unary_expression(node, true /*discarded*/))
            }
            Kind::BinaryExpression => Some(self.visit_binary_expression(node, true /*discarded*/)),
            Kind::ParenthesizedExpression => {
                Some(self.visit_parenthesized_expression(node, true /*discarded*/))
            }
            Kind::PartiallyEmittedExpression => {
                Some(self.visit_partially_emitted_expression(node, true /*discarded*/))
            }
            _ => self.visit(node),
        }
    }

    // esdecorator.go:410
    fn non_constructor_class_element_visit(&self, node: P<Node>) -> Option<P<Node>> {
        if ast::is_constructor_declaration(node) {
            return Some(node); // skip constructors in pass 1
        }
        self.class_element_visitor_visit(node)
    }

    // esdecorator.go:417
    fn constructor_class_element_visit(&self, node: P<Node>) -> Option<P<Node>> {
        if ast::is_constructor_declaration(node) {
            return self.class_element_visitor_visit(node);
        }
        Some(node)
    }

    // esdecorator.go:424
    fn export_stripping_modifier_visit(&self, node: P<Node>) -> Option<P<Node>> {
        if node.kind() == Kind::ExportKeyword {
            return None;
        }
        self.modifier_visitor_visit(node)
    }
}

// esdecorator.go:431
fn get_helper_variable_name(ec: P<EmitContext>, node: P<Node>) -> String {
    let name = node.name();
    let mut declaration_name: String = if let Some(name) =
        name.filter(|n| ast::is_identifier(*n) && !is_generated_identifier(ec, *n))
    {
        name.text().to_string()
    } else if let Some(name) =
        name.filter(|n| ast::is_private_identifier(*n) && !ec.has_auto_generate_info(Some(*n)))
    {
        let text = name.text();
        if text.len() > 1 { text[1..].to_string() } else { String::new() }
    } else if let Some(name) = name.filter(|n| {
        ast::is_string_literal(*n)
            && scanner::is_identifier_text(n.text(), tsrs_core::LanguageVariant::Standard)
    }) {
        name.text().to_string()
    } else if ast::is_class_like(node) {
        "class".to_string()
    } else {
        "member".to_string()
    };

    if ast::is_get_accessor_declaration(node) {
        declaration_name = format!("get_{declaration_name}");
    }
    if ast::is_set_accessor_declaration(node) {
        declaration_name = format!("set_{declaration_name}");
    }
    if name.is_some_and(ast::is_private_identifier) {
        declaration_name = format!("private_{declaration_name}");
    }
    if ast::is_static(node) {
        declaration_name = format!("static_{declaration_name}");
    }
    format!("_{declaration_name}")
}

impl EsDecoratorTransformer {
    // esdecorator.go:464
    pub(crate) fn create_helper_variable(&self, node: P<Node>, suffix: &str) -> P<Node> {
        self.base.factory().new_unique_name_ex(
            &format!("{}_{}", get_helper_variable_name(self.base.emit_context(), node), suffix),
            printer::AutoGenerateOptions {
                flags: printer::GeneratedIdentifierFlags::Optimistic
                    | printer::GeneratedIdentifierFlags::ReservedInNestedScopes,
                ..Default::default()
            },
        )
    }

    // esdecorator.go:471
    fn create_let(&self, name: P<Node>, initializer: Option<P<Node>>) -> P<Node> {
        let f = self.base.factory();
        f.new_variable_statement(
            None,
            f.new_variable_declaration_list(
                f.new_node_list(vec![f.new_variable_declaration(name, None, None, initializer)]),
                NodeFlags::Let,
            ),
        )
    }
}

impl EsDecoratorTransformer {
    // esdecorator.go:483
    fn create_class_info(&self, node: P<Node>) -> P<ClassInfo> {
        let f = self.base.factory();
        let ec = self.base.emit_context();
        let file_level = printer::AutoGenerateOptions {
            flags: printer::GeneratedIdentifierFlags::Optimistic
                | printer::GeneratedIdentifierFlags::FileLevel,
            ..Default::default()
        };
        let ci = P::new(ClassInfo {
            class_decorators_name: Cell::new(None),
            class_descriptor_name: Cell::new(None),
            class_extra_initializers_name: Cell::new(None),
            class_this: Cell::new(None),
            class_super: Cell::new(None),
            metadata_reference: Cell::new(Some(f.new_unique_name_ex("_metadata", file_level))),
            member_infos: RefCell::new(OrderedMap::default()),
            instance_method_extra_initializers_name: Cell::new(None),
            static_method_extra_initializers_name: Cell::new(None),
            static_non_field_decoration_statements: RefCell::new(Vec::new()),
            non_static_non_field_decoration_statements: RefCell::new(Vec::new()),
            static_field_decoration_statements: RefCell::new(Vec::new()),
            non_static_field_decoration_statements: RefCell::new(Vec::new()),
            has_static_initializers: Cell::new(false),
            has_non_ambient_instance_fields: Cell::new(false),
            has_static_private_class_elements: Cell::new(false),
            pending_static_initializers: RefCell::new(Vec::new()),
            pending_instance_initializers: RefCell::new(Vec::new()),
        });

        // Before visiting we perform a first pass to collect information we'll need
        // as we descend.

        // If the class itself is decorated, create a _classThis binding
        if ast::node_is_decorated(false, node, None, None) {
            let needs_unique_class_this = node.members().iter().any(|member| {
                (ast::is_private_identifier_class_element_declaration(*member)
                    || ast::is_auto_accessor_property_declaration(*member))
                    && ast::has_static_modifier(*member)
            });
            // We do not mark _classThis as FileLevel if it may be reused by class private fields, which requires the
            // ability access the captured `_classThis` of outer scopes.
            let mut flags = printer::GeneratedIdentifierFlags::Optimistic
                | printer::GeneratedIdentifierFlags::FileLevel;
            if needs_unique_class_this {
                flags = printer::GeneratedIdentifierFlags::Optimistic
                    | printer::GeneratedIdentifierFlags::ReservedInNestedScopes;
            }
            ci.class_this.set(Some(f.new_unique_name_ex(
                "_classThis",
                printer::AutoGenerateOptions { flags, ..Default::default() },
            )));
        }

        for member in node.members() {
            let member = *member;
            if ast::is_method_or_accessor(member)
                && ast::node_or_child_is_decorated(false, member, Some(node), None)
            {
                if ast::has_static_modifier(member) {
                    if ci.static_method_extra_initializers_name.get().is_none() {
                        let name = f.new_unique_name_ex("_staticExtraInitializers", file_level);
                        ci.static_method_extra_initializers_name.set(Some(name));
                        let renamed_class_this = match ci.class_this.get() {
                            Some(class_this) => class_this,
                            None => f.new_this_expression(),
                        };
                        let initializer =
                            f.new_run_initializers_helper(renamed_class_this, name, None);
                        match node.name() {
                            Some(name_range) => {
                                ec.set_source_map_range(initializer, name_range.loc());
                            }
                            None => ec.set_source_map_range(
                                initializer,
                                move_range_past_decorators(node),
                            ),
                        }
                        ci.pending_static_initializers.borrow_mut().push(initializer);
                    }
                } else if ci.instance_method_extra_initializers_name.get().is_none() {
                    let name = f.new_unique_name_ex("_instanceExtraInitializers", file_level);
                    ci.instance_method_extra_initializers_name.set(Some(name));
                    let initializer =
                        f.new_run_initializers_helper(f.new_this_expression(), name, None);
                    match node.name() {
                        Some(name_range) => ec.set_source_map_range(initializer, name_range.loc()),
                        None => {
                            ec.set_source_map_range(initializer, move_range_past_decorators(node));
                        }
                    }
                    ci.pending_instance_initializers.borrow_mut().push(initializer);
                }
            }

            if ast::is_class_static_block_declaration(member) {
                if !is_class_named_evaluation_helper_block(ec, member) {
                    ci.has_static_initializers.set(true);
                }
            } else if ast::is_property_declaration(member) {
                if ast::has_static_modifier(member) {
                    ci.has_static_initializers.set(
                        ci.has_static_initializers.get()
                            || member.initializer().is_some()
                            || ast::has_decorators(member),
                    );
                } else {
                    ci.has_non_ambient_instance_fields.set(
                        ci.has_non_ambient_instance_fields.get()
                            || !ast::has_syntactic_modifier(member, ModifierFlags::Ambient),
                    );
                }
            }

            if (ast::is_private_identifier_class_element_declaration(member)
                || ast::is_auto_accessor_property_declaration(member))
                && ast::has_static_modifier(member)
            {
                ci.has_static_private_class_elements.set(true);
            }

            // exit early if possible
            if ci.static_method_extra_initializers_name.get().is_some()
                && ci.instance_method_extra_initializers_name.get().is_some()
                && ci.has_static_initializers.get()
                && ci.has_non_ambient_instance_fields.get()
                && ci.has_static_private_class_elements.get()
            {
                break;
            }
        }

        ci
    }
}

impl EsDecoratorTransformer {
    // esdecorator.go:577
    fn transform_class_like(&self, node: P<Node>) -> P<Node> {
        let f = self.base.factory();
        let ec = self.base.emit_context();
        let file_level = printer::AutoGenerateOptions {
            flags: printer::GeneratedIdentifierFlags::Optimistic
                | printer::GeneratedIdentifierFlags::FileLevel,
            ..Default::default()
        };

        ec.start_variable_environment();

        // When a class has class decorators we end up transforming it into a statement that would otherwise give it an
        // assigned name. If the class doesn't have an assigned name, we'll give it an assigned name of `""`.
        let mut node = node;
        if !class_has_declared_or_explicitly_assigned_name(ec, node)
            && ast::class_or_constructor_parameter_is_decorated(false, node)
        {
            node = inject_class_named_evaluation_helper_block_if_missing(
                ec,
                node,
                f.new_string_literal("", TokenFlags::None),
                None,
            );
        }

        let class_reference = f.get_local_name_ex(node, printer::AssignedNameOptions::default());
        let ci = self.create_class_info(node);
        let mut class_definition_statements: Vec<P<Node>> = Vec::new();
        let mut leading_block_statements: Vec<P<Node>> = Vec::new();
        let mut trailing_block_statements: Vec<P<Node>> = Vec::new();
        let mut synthetic_constructor: Option<P<Node>> = None;
        let mut heritage_clauses: Option<P<NodeList>> = None;
        let mut should_transform_private_static_elements_in_class = false;

        // 1. Class decorators are evaluated outside the private name scope of the class.
        //
        // - Since class decorators don't have privileged access to private names defined inside the class,
        //   they must be evaluated outside of the class body.
        // - Since a class decorator can replace the class constructor, we must define a variable to keep track
        //   of the mutated class.
        // - Since a class decorator can add extra initializers, we must define a variable to keep track of
        //   extra initializers.
        let class_decorators = self.transform_all_decorators_of_declaration(&node.decorators());
        if !class_decorators.is_empty() {
            assert!(ci.class_this.get().is_some());

            ci.class_decorators_name
                .set(Some(f.new_unique_name_ex("_classDecorators", file_level)));
            ci.class_descriptor_name
                .set(Some(f.new_unique_name_ex("_classDescriptor", file_level)));
            ci.class_extra_initializers_name
                .set(Some(f.new_unique_name_ex("_classExtraInitializers", file_level)));

            let decorators_array =
                f.new_array_literal_expression(f.new_node_list(class_decorators.clone()), false);
            class_definition_statements.push(
                self.create_let(ci.class_decorators_name.get().unwrap(), Some(decorators_array)),
            );
            class_definition_statements
                .push(self.create_let(ci.class_descriptor_name.get().unwrap(), None));
            class_definition_statements.push(self.create_let(
                ci.class_extra_initializers_name.get().unwrap(),
                Some(f.new_array_literal_expression(f.new_node_list(vec![]), false)),
            ));
            class_definition_statements.push(self.create_let(ci.class_this.get().unwrap(), None));

            if !class_decorators.is_empty() && ci.has_static_private_class_elements.get() {
                should_transform_private_static_elements_in_class = true;
                self.should_transform_private_static_elements_in_file.set(true);
            }
        }

        // 2. ClassHeritage clause is evaluated outside of the private name scope of the class.
        let extends_clause = ast::get_heritage_clause(node, Kind::ExtendsKeyword);
        let mut extends_element: Option<P<Node>> = None;
        if let Some(extends_clause) = extends_clause {
            let types = extends_clause.as_heritage_clause().types();
            if !types.nodes().is_empty() {
                extends_element = Some(types.nodes()[0]);
            }
        }
        let mut extends_expression: Option<P<Node>> = None;
        if let Some(extends_element) = extends_element {
            extends_expression = self
                .base
                .visitor()
                .visit_node(Some(extends_element.as_expression_with_type_arguments().expression));
        }

        if let Some(extends_expression) = extends_expression {
            // Rewrite `super` in static initializers so that we can use the correct `this`.
            ci.class_super.set(Some(f.new_unique_name_ex("_classSuper", file_level)));

            // Ensure we do not give the class or function an assigned name due to the variable by prefixing it
            // with `0, `.
            let unwrapped =
                ast::skip_outer_expressions(extends_expression, OuterExpressionKinds::All);
            let mut safe_extends_expression = extends_expression;
            if (ast::is_class_expression(unwrapped) && unwrapped.name().is_none())
                || (ast::is_function_expression(unwrapped) && unwrapped.name().is_none())
                || ast::is_arrow_function(unwrapped)
            {
                safe_extends_expression = f.new_comma_expression(
                    f.new_numeric_literal("0", TokenFlags::None),
                    extends_expression,
                );
            }
            class_definition_statements.push(
                self.create_let(ci.class_super.get().unwrap(), Some(safe_extends_expression)),
            );

            let ewta = extends_element.unwrap();
            let updated_extends_element =
                f.update_expression_with_type_arguments(ewta, ci.class_super.get().unwrap(), None);
            let hc = extends_clause.unwrap();
            let updated_extends_clause = f.update_heritage_clause(
                hc,
                hc.as_heritage_clause().token,
                f.new_node_list(vec![updated_extends_element]),
            );
            heritage_clauses = Some(f.new_node_list(vec![updated_extends_clause]));
        }

        let renamed_class_this = match ci.class_this.get() {
            Some(class_this) => class_this,
            None => f.new_this_expression(),
        };

        // 3. The name of the class is assigned.
        //
        // If the class did not have a name, the caller should have performed injectClassNamedEvaluationHelperBlockIfMissing
        // prior to calling this function if a name was needed.

        // 4. For each member:
        //    a. Member Decorators are evaluated
        //    b. Computed Property Name is evaluated, if present
        //
        // We visit members in two passes:
        // - The first pass visits methods, accessors, and fields to collect decorators and computed property names.
        // - The second pass visits the constructor to add instance initializers.
        //
        // NOTE: If there are no constructors, but there are instance initializers, a synthetic constructor is added.
        self.enter_class(Some(ci));

        leading_block_statements
            .push(self.create_metadata(ci.metadata_reference.get().unwrap(), ci.class_super.get()));

        // Since the constructor can appear anywhere in the class body and its transform depends on other class elements,
        // we must first visit all non-constructor members, then visit the constructor, all while maintaining document order.
        let mut members = self
            .non_constructor_class_element_visitor
            .get()
            .unwrap()
            .clone()
            .visit_nodes(node.member_list());
        members =
            self.constructor_class_element_visitor.get().unwrap().clone().visit_nodes(members);

        // Handle pending expressions (computed property names and decorator evaluations)
        let pending = self.pending_expressions.borrow().clone().unwrap_or_default();
        if !pending.is_empty() {
            // If a pending expression contains a lexical `this`, we'll need to capture the lexical `this` of the
            // container and transform it in the expression. This ensures we use the correct `this` in the resulting
            // class `static` block. We don't use substitution here because the size of the tree we are visiting
            // is likely to be small and doesn't justify the complexity of introducing substitution.
            self.outer_this.set(None);
            leading_block_statements.extend(pending.into_iter().map(|expr| {
                let mut expr = expr;
                // If a pending expression contains lexical `this`, capture it
                if expr.subtree_facts().intersects(SubtreeFacts::ContainsLexicalThis) {
                    expr = self
                        .outer_this_visitor
                        .get()
                        .unwrap()
                        .clone()
                        .visit_node(Some(expr))
                        .unwrap();
                }

                f.new_expression_statement(expr)
            }));
            if let Some(outer_this) = self.outer_this.get() {
                class_definition_statements
                    .insert(0, self.create_let(outer_this, Some(f.new_this_expression())));
            }
            *self.pending_expressions.borrow_mut() = None;
        }
        self.exit_class();

        // If there are instance initializers but no constructor, synthesize one
        if !ci.pending_instance_initializers.borrow().is_empty()
            && ast::get_first_constructor_with_body(node).is_none()
        {
            let initializer_statements = self.prepare_constructor(ci);
            if !initializer_statements.is_empty() {
                let is_derived_class = extends_element.is_some_and(|e| {
                    ast::skip_outer_expressions(
                        e.as_expression_with_type_arguments().expression,
                        OuterExpressionKinds::All,
                    )
                    .kind()
                        != Kind::NullKeyword
                });
                let mut constructor_statements: Vec<P<Node>> = Vec::new();
                if is_derived_class {
                    let spread_arguments = f.new_spread_element(f.new_identifier("arguments"));
                    let super_call = f.new_call_expression(
                        f.new_keyword_expression(Kind::SuperKeyword),
                        None,
                        None,
                        f.new_node_list(vec![spread_arguments]),
                        NodeFlags::None,
                    );
                    constructor_statements.push(f.new_expression_statement(super_call));
                }
                constructor_statements.extend(initializer_statements);
                let constructor_body = f.new_block(f.new_node_list(constructor_statements), true);
                synthetic_constructor = Some(f.new_constructor_declaration(
                    None,
                    None,
                    Some(f.new_node_list(vec![])),
                    None,
                    None,
                    Some(constructor_body),
                ));
            }
        }

        // Used in class definition steps 5,7,11
        if let Some(name) = ci.static_method_extra_initializers_name.get() {
            class_definition_statements.push(self.create_let(
                name,
                Some(f.new_array_literal_expression(f.new_node_list(vec![]), false)),
            ));
        }

        // Used in class definition steps 6,8, and construction
        if let Some(name) = ci.instance_method_extra_initializers_name.get() {
            class_definition_statements.push(self.create_let(
                name,
                Some(f.new_array_literal_expression(f.new_node_list(vec![]), false)),
            ));
        }

        // Used in class definition steps 7, 8, 12, and construction.
        // Emit member info variable declarations; the reference implementation emits static member vars first, then non-static.
        if !ci.member_infos.borrow().is_empty() {
            class_definition_statements
                .extend(self.emit_member_info_declarations(ci, true /*isStatic*/));
            class_definition_statements
                .extend(self.emit_member_info_declarations(ci, false /*isStatic*/));
        }

        // 5. Static non-field element decorators are applied
        leading_block_statements
            .extend(ci.static_non_field_decoration_statements.borrow().iter().copied());

        // 6. Non-static non-field element decorators are applied
        leading_block_statements
            .extend(ci.non_static_non_field_decoration_statements.borrow().iter().copied());

        // 7. Static field element decorators are applied
        leading_block_statements
            .extend(ci.static_field_decoration_statements.borrow().iter().copied());

        // 8. Non-static field element decorators are applied
        leading_block_statements
            .extend(ci.non_static_field_decoration_statements.borrow().iter().copied());

        // 9. Class decorators are applied
        // 10. Class binding is initialized
        //
        // produces:
        //   __esDecorate(null, _classDescriptor = { value: this }, _classDecorators, { kind: "class", name: this.name, metadata }, null, _classExtraInitializers);
        if let (
            Some(class_descriptor_name),
            Some(class_decorators_name),
            Some(class_extra_initializers_name),
            Some(class_this),
        ) = (
            ci.class_descriptor_name.get(),
            ci.class_decorators_name.get(),
            ci.class_extra_initializers_name.get(),
            ci.class_this.get(),
        ) {
            let value_property = f.new_property_assignment(
                None,
                f.new_identifier("value"),
                None,
                None,
                renamed_class_this,
            );
            let class_descriptor =
                f.new_object_literal_expression(f.new_node_list(vec![value_property]), false);
            let class_descriptor_assignment =
                f.new_assignment_expression(class_descriptor_name, class_descriptor);
            let class_name_reference = f.new_property_access_expression(
                renamed_class_this,
                None,
                f.new_identifier("name"),
                NodeFlags::None,
            );

            let context_obj = f.new_es_decorate_class_context_object(
                class_name_reference,
                ci.metadata_reference.get().unwrap(),
            );

            let es_decorate_helper = f.new_es_decorate_helper(
                f.new_token(Kind::NullKeyword),
                class_descriptor_assignment,
                class_decorators_name,
                context_obj,
                f.new_token(Kind::NullKeyword),
                class_extra_initializers_name,
            );
            let es_decorate_statement = f.new_expression_statement(es_decorate_helper);
            ec.set_source_map_range(es_decorate_statement, move_range_past_decorators(node));
            leading_block_statements.push(es_decorate_statement);

            // produces:
            //   C = _classThis = _classDescriptor.value;
            let class_descriptor_value_ref = f.new_property_access_expression(
                class_descriptor_name,
                None,
                f.new_identifier("value"),
                NodeFlags::None,
            );
            let class_this_assignment =
                f.new_assignment_expression(class_this, class_descriptor_value_ref);
            let class_reference_assignment =
                f.new_assignment_expression(class_reference, class_this_assignment);
            leading_block_statements.push(f.new_expression_statement(class_reference_assignment));
        }

        // produces:
        //   if (metadata) Object.defineProperty(C, Symbol.metadata, { configurable: true, writable: true, value: metadata });
        leading_block_statements.push(
            self.create_symbol_metadata(renamed_class_this, ci.metadata_reference.get().unwrap()),
        );

        // 11. Static extra initializers
        // 12. Static fields are initialized
        let pending_static: Vec<P<Node>> =
            std::mem::take(&mut *ci.pending_static_initializers.borrow_mut());
        if !pending_static.is_empty() {
            trailing_block_statements.extend(pending_static.into_iter().map(|initializer| {
                let initializer_statement = f.new_expression_statement(initializer);
                ec.set_source_map_range(initializer_statement, ec.source_map_range(initializer));
                initializer_statement
            }));
        }

        // 13. Class extra initializers
        if let Some(class_extra_initializers_name) = ci.class_extra_initializers_name.get() {
            let run_class_initializers_helper = f.new_run_initializers_helper(
                renamed_class_this,
                class_extra_initializers_name,
                None,
            );
            let run_class_initializers_statement =
                f.new_expression_statement(run_class_initializers_helper);
            match node.name() {
                Some(name) => ec.set_source_map_range(run_class_initializers_statement, name.loc()),
                None => ec.set_source_map_range(
                    run_class_initializers_statement,
                    move_range_past_decorators(node),
                ),
            }
            trailing_block_statements.push(run_class_initializers_statement);
        }

        // If there are no other static initializers to run, combine the leading and trailing block statements
        if !leading_block_statements.is_empty()
            && !trailing_block_statements.is_empty()
            && !ci.has_static_initializers.get()
        {
            leading_block_statements.append(&mut trailing_block_statements);
        }

        // prepare a leading `static {}` block, if necessary
        let mut leading_static_block: Option<P<Node>> = None;
        if !leading_block_statements.is_empty() {
            leading_static_block = Some(f.new_class_static_block_declaration(
                None,
                f.new_block(f.new_node_list(leading_block_statements), true),
            ));
        }

        if let Some(block) = leading_static_block
            && should_transform_private_static_elements_in_class
        {
            // We use EFTransformPrivateStaticElements as a marker on a class static block
            // to inform the classFields transform that it shouldn't rename `this` to `_classThis` in the
            // transformed class static block.
            ec.set_emit_flags(block, EmitFlags::TransformPrivateStaticElements);
        }

        // prepare a trailing `static {}` block, if necessary
        let mut trailing_static_block: Option<P<Node>> = None;
        if !trailing_block_statements.is_empty() {
            trailing_static_block = Some(f.new_class_static_block_declaration(
                None,
                f.new_block(f.new_node_list(trailing_block_statements), true),
            ));
        }

        // Assemble new members list
        let mut members = members.unwrap();
        if leading_static_block.is_some()
            || synthetic_constructor.is_some()
            || trailing_static_block.is_some()
        {
            let nodes = members.nodes();
            let mut new_members: Vec<P<Node>> = Vec::with_capacity(nodes.len() + 3);

            // Find the existing NamedEvaluation helper block index
            let existing_named_evaluation_helper_block_index =
                nodes.iter().position(|m| is_class_named_evaluation_helper_block(ec, *m));

            // add the leading `static {}` block
            if let Some(leading_static_block) = leading_static_block {
                // add the `static {}` block after any existing NamedEvaluation helper block, if one exists.
                let split =
                    existing_named_evaluation_helper_block_index.map_or(0, |index| index + 1);
                new_members.extend_from_slice(&nodes[..split]);
                new_members.push(leading_static_block);
                new_members.extend_from_slice(&nodes[split..]);
            } else {
                new_members.extend_from_slice(nodes);
            }

            // append the synthetic constructor, if necessary
            if let Some(synthetic_constructor) = synthetic_constructor {
                new_members.push(synthetic_constructor);
            }

            // append a trailing `static {}` block, if necessary
            if let Some(trailing_static_block) = trailing_static_block {
                new_members.push(trailing_static_block);
            }

            let members_list = f.new_node_list(new_members);
            members_list.loc.set(members.loc.get());
            members = members_list;
        }

        let lexical_environment = ec.end_variable_environment();

        let mut class_expression: P<Node>;
        if class_decorators.is_empty() {
            // produces:
            //   return <classExpression>;
            class_expression =
                f.new_class_expression(None, node.name(), None, heritage_clauses, members);
            ec.set_original(class_expression, node);
            class_definition_statements.push(f.new_return_statement(Some(class_expression)));
        } else {
            class_expression = f.new_class_expression(None, None, None, heritage_clauses, members);
            ec.set_original(class_expression, node);
            if let Some(class_this) = ci.class_this.get() {
                class_expression =
                    inject_class_this_assignment_if_missing(ec, f, class_expression, class_this);
            }

            // We use `var` instead of `let` so we can leverage NamedEvaluation to define the class name
            // and still be able to ensure it is initialized prior to any use in `static {}`.
            let class_reference_declaration =
                f.new_variable_declaration(class_reference, None, None, Some(class_expression));
            let class_reference_var_decl_list = f.new_variable_declaration_list(
                f.new_node_list(vec![class_reference_declaration]),
                NodeFlags::None,
            );
            let return_expr = match ci.class_this.get() {
                Some(class_this) => f.new_assignment_expression(class_reference, class_this),
                None => class_reference,
            };
            class_definition_statements
                .push(f.new_variable_statement(None, class_reference_var_decl_list));
            class_definition_statements.push(f.new_return_statement(Some(return_expr)));
        }

        if should_transform_private_static_elements_in_class {
            ec.add_emit_flags(class_expression, EmitFlags::TransformPrivateStaticElements);
            for member in class_expression.members() {
                if (ast::is_private_identifier_class_element_declaration(*member)
                    || ast::is_auto_accessor_property_declaration(*member))
                    && ast::has_static_modifier(*member)
                {
                    ec.add_emit_flags(*member, EmitFlags::TransformPrivateStaticElements);
                }
            }
        }

        let merged_statements =
            ec.merge_environment(&class_definition_statements, &lexical_environment);
        f.new_immediately_invoked_arrow_function(merged_statements)
    }
}

impl EsDecoratorTransformer {
    // esdecorator.go:993
    // Generates let declarations for member decorator info variables, filtered by static/non-static.
    fn emit_member_info_declarations(&self, ci: P<ClassInfo>, is_static: bool) -> Vec<P<Node>> {
        let f = self.base.factory();
        let mut stmts: Vec<P<Node>> = Vec::new();
        #[expect(
            clippy::needless_collect,
            reason = "release the member-info borrow before constructing declarations, as in the original traversal"
        )]
        let entries: Vec<(P<Node>, P<MemberInfo>)> =
            ci.member_infos.borrow().iter().map(|(k, v)| (*k, *v)).collect();
        stmts = entries.into_iter().fold(stmts, |mut stmts, (member, mi)| {
            if ast::is_static(member) != is_static {
                return stmts;
            }
            stmts.push(self.create_let(mi.member_decorators_name.get().unwrap(), None));
            if let Some(name) = mi.member_initializers_name.get() {
                stmts.push(self.create_let(
                    name,
                    Some(f.new_array_literal_expression(f.new_node_list(vec![]), false)),
                ));
            }
            if let Some(name) = mi.member_extra_initializers_name.get() {
                stmts.push(self.create_let(
                    name,
                    Some(f.new_array_literal_expression(f.new_node_list(vec![]), false)),
                ));
            }
            if let Some(name) = mi.member_descriptor_name.get() {
                stmts.push(self.create_let(name, None));
            }

            stmts
        });
        stmts
    }
}

// esdecorator.go:1014
pub(crate) fn is_decorated_class_like(node: P<Node>) -> bool {
    ast::class_or_constructor_parameter_is_decorated(false, node)
        || ast::child_is_decorated(false, node, None)
}

impl EsDecoratorTransformer {
    // esdecorator.go:1019
    fn visit_class_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        let f = self.base.factory();
        let ec = self.base.emit_context();
        if is_decorated_class_like(node) {
            let mut statements: Vec<P<Node>> = Vec::new();

            let mut original_class = ec.most_original(Some(node)).unwrap();
            if !ast::is_class_like(original_class) {
                original_class = node;
            }
            let class_name = match original_class.name() {
                Some(name) => f.new_string_literal_from_node(name),
                None => f.new_string_literal("default", TokenFlags::None),
            };

            let is_export = ast::has_syntactic_modifier(node, ModifierFlags::Export);
            let is_default = ast::has_syntactic_modifier(node, ModifierFlags::Default);

            let mut class_node = node;
            if node.name().is_none() {
                class_node = inject_class_named_evaluation_helper_block_if_missing(
                    ec, class_node, class_name, None,
                );
            }

            if is_export && is_default {
                let iife = self.transform_class_like(class_node);
                if class_node.name().is_some() {
                    // produces:
                    //   let C = (() => { ... })();
                    //   export default C;
                    let var_decl = f.new_variable_declaration(
                        f.get_local_name(class_node),
                        None,
                        None,
                        Some(iife),
                    );
                    ec.set_original(var_decl, class_node);
                    let var_decls = f.new_variable_declaration_list(
                        f.new_node_list(vec![var_decl]),
                        NodeFlags::Let,
                    );
                    let var_statement = f.new_variable_statement(None, var_decls);
                    statements.push(var_statement);

                    let export_statement = f.new_export_default(f.get_declaration_name(class_node));
                    ec.set_original(export_statement, class_node);
                    ec.assign_comment_range(export_statement, class_node);
                    ec.set_source_map_range(
                        export_statement,
                        move_range_past_decorators(class_node),
                    );
                    statements.push(export_statement);
                } else {
                    // produces:
                    //   export default (() => { ... })();
                    let export_statement = f.new_export_default(iife);
                    ec.set_original(export_statement, class_node);
                    ec.assign_comment_range(export_statement, class_node);
                    ec.set_source_map_range(
                        export_statement,
                        move_range_past_decorators(class_node),
                    );
                    statements.push(export_statement);
                }
            } else {
                assert!(
                    class_node.name().is_some(),
                    "A class declaration that is not a default export must have a name."
                );
                // produces:
                //   let C = (() => { ... })();
                let iife = self.transform_class_like(class_node);
                let modifiers = self
                    .export_stripping_modifier_visitor
                    .get()
                    .unwrap()
                    .clone()
                    .visit_modifiers(class_node.modifiers());

                let decl_name = f.get_local_name_ex(
                    class_node,
                    printer::AssignedNameOptions { allow_source_maps: true, ..Default::default() },
                );
                let var_decl = f.new_variable_declaration(decl_name, None, None, Some(iife));
                ec.set_original(var_decl, class_node);
                let var_decls = f
                    .new_variable_declaration_list(f.new_node_list(vec![var_decl]), NodeFlags::Let);
                let var_statement = f.new_variable_statement(modifiers, var_decls);
                ec.set_original(var_statement, class_node);
                ec.assign_comment_range(var_statement, class_node);
                statements.push(var_statement);

                if is_export {
                    // produces:
                    //   export { C };
                    let export_statement = f.new_external_module_export(decl_name);
                    ec.set_original(export_statement, class_node);
                    statements.push(export_statement);
                }
            }

            return single_or_many(Some(statements), f);
        }

        // Non-decorated class
        let modifiers =
            self.modifier_visitor.get().unwrap().clone().visit_modifiers(node.modifiers());
        let heritage_clauses =
            self.base.visitor().visit_nodes(node.as_class_declaration().heritage_clauses());
        self.enter_class(None);
        let members =
            self.class_element_visitor.get().unwrap().clone().visit_nodes(node.member_list());
        self.exit_class();
        Some(f.update_class_declaration(
            node,
            modifiers,
            node.name(),
            None,
            heritage_clauses,
            members.unwrap(),
        ))
    }

    // esdecorator.go:1107
    fn visit_class_expression(&self, node: P<Node>) -> P<Node> {
        if is_decorated_class_like(node) {
            let iife = self.transform_class_like(node);
            self.base.emit_context().set_original(iife, node);
            return iife;
        }

        let modifiers =
            self.modifier_visitor.get().unwrap().clone().visit_modifiers(node.modifiers());
        let heritage_clauses =
            self.base.visitor().visit_nodes(node.as_class_expression().heritage_clauses());
        self.enter_class(None);
        let members =
            self.class_element_visitor.get().unwrap().clone().visit_nodes(node.member_list());
        self.exit_class();
        self.base.factory().update_class_expression(
            node,
            modifiers,
            node.name(),
            None,
            heritage_clauses,
            members.unwrap(),
        )
    }

    // esdecorator.go:1122
    fn prepare_constructor(&self, ci: P<ClassInfo>) -> Vec<P<Node>> {
        // Decorated instance members can add "extra" initializers to the instance. If a class contains any instance
        // fields, we'll inject the `__runInitializers()` call for these extra initializers into the initializer of
        // the first class member that will be initialized. However, if the class does not contain any fields that
        // we can piggyback on, we need to synthesize a `__runInitializers()` call in the constructor instead.
        if ci.pending_instance_initializers.borrow().is_empty() {
            return Vec::new();
        }
        let f = self.base.factory();
        let pending = std::mem::take(&mut *ci.pending_instance_initializers.borrow_mut());
        vec![f.new_expression_statement(f.inline_expressions(&pending).unwrap())]
    }
}

impl EsDecoratorTransformer {
    // esdecorator.go:1138
    fn transform_constructor_body_worker(
        &self,
        mut statements_out: Vec<P<Node>>,
        statements_in: &[P<Node>],
        statement_offset: usize,
        super_path: &[usize],
        super_path_depth: usize,
        initializer_statements: &[P<Node>],
    ) -> Vec<P<Node>> {
        let f = self.base.factory();
        let super_statement_index = super_path[super_path_depth];
        // Visit statements before super
        if super_statement_index > statement_offset {
            statements_out.extend(
                statements_in[statement_offset..super_statement_index]
                    .iter()
                    .filter_map(|s| self.base.visitor().visit_node(Some(*s))),
            );
        }

        let super_statement = statements_in[super_statement_index];
        if ast::is_try_statement(super_statement) {
            // Recurse into try block
            let ts = super_statement.as_try_statement();
            let try_block_node = ts.try_block;
            let try_block_statements = self.transform_constructor_body_worker(
                Vec::new(),
                try_block_node.statements(),
                0,
                super_path,
                super_path_depth + 1,
                initializer_statements,
            );

            let new_try_block = f.new_block(f.new_node_list(try_block_statements), true);
            // Use the original try block's range even though the statements may differ due to
            // injected initializer statements. This preserves source map fidelity for the enclosing
            // try statement.
            new_try_block.set_loc(try_block_node.loc());

            let catch_clause =
                ts.catch_clause.and_then(|c| self.base.visitor().visit_node(Some(c)));
            let finally_block =
                ts.finally_block.and_then(|b| self.base.visitor().visit_node(Some(b)));
            let updated =
                f.update_try_statement(super_statement, new_try_block, catch_clause, finally_block);
            statements_out.push(updated);
        } else {
            statements_out.extend(self.base.visitor().visit_node(Some(super_statement)));
            statements_out.extend_from_slice(initializer_statements);
        }

        // Visit statements after super
        if super_statement_index + 1 < statements_in.len() {
            statements_out.extend(
                statements_in[super_statement_index + 1..]
                    .iter()
                    .filter_map(|s| self.base.visitor().visit_node(Some(*s))),
            );
        }
        statements_out
    }

    // esdecorator.go:1184
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Visitor branches share an optional node result so sibling branches can remove a node."
    )]
    fn visit_constructor_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        let f = self.base.factory();
        self.enter_class_element(node);
        let modifiers =
            self.modifier_visitor.get().unwrap().clone().visit_modifiers(node.modifiers());
        let parameters = self.base.visitor().visit_nodes(node.parameter_list());

        let mut body: Option<P<Node>> = None;
        let ctor_body = node.body();
        if let (Some(ctor_body), Some(ci)) = (ctor_body, self.class_info_stack.get()) {
            // If there are instance extra initializers we need to add them to the body along with any
            // field initializers
            let initializer_statements = self.prepare_constructor(ci);
            if !initializer_statements.is_empty() {
                let mut stmts: Vec<P<Node>> = Vec::new();
                let (prologue, rest) = f.split_standard_prologue(ctor_body.statements());
                stmts.extend_from_slice(prologue);

                let super_statement_indices = find_super_statement_index_path(rest, 0);
                if super_statement_indices.is_empty() {
                    stmts.extend(initializer_statements);
                    let (visited, _) = self.base.visitor().visit_slice(alloc_slice(rest));
                    stmts.extend_from_slice(visited);
                } else {
                    stmts = self.transform_constructor_body_worker(
                        stmts,
                        rest,
                        0,
                        &super_statement_indices,
                        0,
                        &initializer_statements,
                    );
                }

                let b = f.new_block(f.new_node_list(stmts), true);
                self.base.emit_context().set_original(b, ctor_body);
                b.set_loc(ctor_body.loc());
                body = Some(b);
            }
        }

        if body.is_none() {
            body = self.base.visitor().visit_node(ctor_body);
        }
        self.exit_class_element();
        Some(f.update_constructor_declaration(node, modifiers, None, parameters, None, None, body))
    }

    // esdecorator.go:1222
    pub(crate) fn finish_class_element(&self, updated: P<Node>, original: P<Node>) -> P<Node> {
        if updated != original {
            // While we emit the source map for the node after skipping decorators and modifiers,
            // we need to emit the comments for the original range.
            self.base.emit_context().assign_comment_range(updated, original);
            self.base
                .emit_context()
                .set_source_map_range(updated, move_range_past_decorators(original));
        }
        updated
    }
}
