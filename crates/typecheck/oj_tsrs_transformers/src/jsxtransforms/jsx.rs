use crate::*;
use printer::EmitFlags;
use tsrs_core::TextRange;
use tsrs_core::collections::OrderedMap;
use tsrs_core::stringutil;
use tsrs_core::{JsxEmit, LanguageVariant};

struct JSXTransformer {
    base: Transformer,
    compiler_options: P<CompilerOptions>,
    emit_resolver: Resolver,

    import_specifier: RefCell<String>,
    filename_declaration: Cell<Option<P<Node>>>,
    utilized_implicit_runtime_imports: RefCell<OrderedMap<String, FxHashMap<String, P<Node>>>>,
    in_jsx_child: Cell<bool>,

    current_source_file: Cell<Option<P<SourceFile>>>,
}

// jsx.go:32
#[expect(
    clippy::unnecessary_wraps,
    reason = "Transformer factories share an optional result so disabled transforms can return None."
)]
pub fn new_jsx_transformer(opts: &TransformOptions) -> Option<P<Transformer>> {
    let compiler_options = opts.compiler_options;
    let emit_context = opts.context;
    let tx = P::new(JSXTransformer {
        base: Transformer::default(),
        compiler_options,
        emit_resolver: opts.emit_resolver.unwrap(),
        import_specifier: RefCell::new(String::new()),
        filename_declaration: Cell::new(None),
        utilized_implicit_runtime_imports: RefCell::new(OrderedMap::default()),
        in_jsx_child: Cell::new(false),
        current_source_file: Cell::new(None),
    });
    Some(tx.get().base.new_transformer(
        Rc::new(move |_: &mut NodeVisitor, n: P<Node>| tx.visit(Some(n))),
        Some(emit_context),
    ))
}

impl JSXTransformer {
    fn emit_context(&self) -> P<EmitContext> {
        self.base.emit_context()
    }

    fn factory(&self) -> &'static printer::NodeFactory {
        self.base.factory()
    }

    fn visitor(&self) -> NodeVisitor {
        self.base.visitor()
    }

    fn current_source_file(&self) -> P<SourceFile> {
        self.current_source_file.get().unwrap()
    }

    // jsx.go:42
    fn get_current_file_name_expression(&self) -> P<Node> {
        if let Some(d) = self.filename_declaration.get() {
            return d.as_variable_declaration().name();
        }
        let f = self.factory();
        let d = f.new_variable_declaration(
            f.new_unique_name_ex(
                "_jsxFileName",
                printer::AutoGenerateOptions {
                    flags: printer::GeneratedIdentifierFlags::Optimistic
                        | printer::GeneratedIdentifierFlags::FileLevel,
                    ..Default::default()
                },
            ),
            None,
            None,
            Some(f.new_string_literal(
                alloc_str(self.current_source_file().file_name()),
                TokenFlags::None,
            )),
        );
        self.filename_declaration.set(Some(d));
        d.as_variable_declaration().name()
    }

    // jsx.go:58
    fn get_jsx_factory_callee_primitive(&self, is_static_children: bool) -> &'static str {
        if self.compiler_options.jsx == JsxEmit::REACT_JSX_DEV {
            return "jsxDEV";
        }
        if is_static_children {
            return "jsxs";
        }
        "jsx"
    }

    // jsx.go:68
    fn get_jsx_factory_callee(&self, is_static_children: bool) -> P<Node> {
        let t = self.get_jsx_factory_callee_primitive(is_static_children);
        self.get_implicit_import_for_name(t)
    }

    // jsx.go:73
    fn get_implicit_jsx_fragment_reference(&self) -> P<Node> {
        self.get_implicit_import_for_name("Fragment")
    }

    // jsx.go:77
    fn get_implicit_import_for_name(&self, name: &'static str) -> P<Node> {
        let mut import_source = self.import_specifier.borrow().clone();
        if name != "createElement" {
            import_source = ast::get_jsx_runtime_import(&import_source, &self.compiler_options);
        }
        {
            let imports = self.utilized_implicit_runtime_imports.borrow();
            if let Some(existing) = imports.get(&import_source)
                && let Some(elem) = existing.get(name)
            {
                return elem.as_import_specifier().name();
            }
        }
        if !self.utilized_implicit_runtime_imports.borrow().contains_key(&import_source) {
            self.utilized_implicit_runtime_imports
                .borrow_mut()
                .insert(import_source.clone(), FxHashMap::default());
        }

        let f = self.factory();
        let generated_name = f.new_unique_name_ex(
            &format!("_{name}"),
            printer::AutoGenerateOptions {
                flags: printer::GeneratedIdentifierFlags::Optimistic
                    | printer::GeneratedIdentifierFlags::FileLevel
                    | printer::GeneratedIdentifierFlags::AllowNameSubstitution,
                ..Default::default()
            },
        );
        let specifier = f.new_import_specifier(false, Some(f.new_identifier(name)), generated_name);
        self.emit_resolver.set_referenced_import_declaration(generated_name, specifier);
        self.utilized_implicit_runtime_imports
            .borrow_mut()
            .get_mut(&import_source)
            .unwrap()
            .insert(name.to_string(), specifier);
        specifier.as_import_specifier().name()
    }

    // jsx.go:102
    fn set_in_child(&self, v: bool) {
        self.in_jsx_child.set(v);
    }

    // jsx.go:106
    fn visit(&self, node: Option<P<Node>>) -> Option<P<Node>> {
        let node = node?;
        if !node.subtree_facts().intersects(SubtreeFacts::ContainsJsx) {
            return Some(node);
        }
        match node.kind() {
            Kind::SourceFile => {
                self.set_in_child(false);
                return Some(self.visit_source_file(node.as_source_file_p()));
            }
            Kind::JsxElement => return Some(self.visit_jsx_element(node)),
            Kind::JsxSelfClosingElement => return Some(self.visit_jsx_self_closing_element(node)),
            Kind::JsxFragment => return Some(self.visit_jsx_fragment(node)),
            Kind::JsxOpeningElement => {
                panic!("JsxOpeningElement should not be visited, handled in visitJsxElement")
            }
            Kind::JsxOpeningFragment => {
                panic!("JsxOpeningFragment should not be visited, handled in visitJsxFragment")
            }
            Kind::JsxText => {
                self.set_in_child(false);
                return self.visit_jsx_text(node);
            }
            Kind::JsxExpression => {
                self.set_in_child(false);
                return self.visit_jsx_expression(node);
            }
            _ => {}
        }
        self.set_in_child(false);
        self.visitor().visit_each_child(Some(node)) // by default, do nothing
    }
}

// jsx.go:141
/*
 * The react jsx/jsxs transform falls back to `createElement` when an explicit `key` argument comes after a spread
 */
fn has_key_after_props_spread(node: P<Node>) -> bool {
    let mut spread = false;
    let mut opener = node;
    if node.kind() == Kind::JsxElement {
        opener = node.as_jsx_element().opening_element;
    } // otherwise self-closing
    for elem in opener.attributes().unwrap().properties() {
        let elem = *elem;
        if ast::is_jsx_spread_attribute(elem)
            && (!ast::is_object_literal_expression(elem.expression().unwrap())
                || elem
                    .expression()
                    .unwrap()
                    .properties()
                    .iter()
                    .any(|p| ast::is_spread_assignment(*p)))
        {
            spread = true;
        } else if spread
            && ast::is_jsx_attribute(elem)
            && ast::is_identifier(elem.name().unwrap())
            && elem.name().unwrap().text() == "key"
        {
            return true;
        }
    }
    false
}

impl JSXTransformer {
    // jsx.go:157
    fn should_use_create_element(&self, node: P<Node>) -> bool {
        self.import_specifier.borrow().is_empty() || has_key_after_props_spread(node)
    }
}

// jsx.go:161
fn insert_statement_after_prologue<T: Copy>(
    to: &mut Vec<P<Node>>,
    statement: Option<P<Node>>,
    is_prologue_directive: fn(T, P<Node>) -> bool,
    callee: T,
) {
    let Some(statement) = statement else {
        return;
    };
    let mut statement_idx = 0;
    // skip all prologue directives to insert at the correct position
    while statement_idx < to.len() {
        if !is_prologue_directive(callee, to[statement_idx]) {
            break;
        }
        statement_idx += 1;
    }
    to.insert(statement_idx, statement);
}

impl JSXTransformer {
    // jsx.go:175
    fn is_any_prologue_directive(&self, node: P<Node>) -> bool {
        ast::is_prologue_directive(node)
            || self.emit_context().emit_flags(node).intersects(EmitFlags::CustomPrologue)
    }

    // jsx.go:179
    fn insert_statement_after_custom_prologue(
        &self,
        to: &mut Vec<P<Node>>,
        statement: Option<P<Node>>,
    ) {
        insert_statement_after_prologue(
            to,
            statement,
            |tx: &JSXTransformer, n| tx.is_any_prologue_directive(n),
            self,
        );
    }
}

// jsx.go:183
fn sort_import_specifiers(a: P<Node>, b: P<Node>) -> std::cmp::Ordering {
    let res = stringutil::compare_strings_case_sensitive(
        a.property_name().unwrap().text(),
        b.property_name().unwrap().text(),
    );
    if res != std::cmp::Ordering::Equal {
        return res;
    }
    stringutil::compare_strings_case_sensitive(
        a.as_import_specifier().name().text(),
        b.as_import_specifier().name().text(),
    )
}

// jsx.go:191
fn get_sorted_specifiers(m: &FxHashMap<String, P<Node>>) -> Vec<P<Node>> {
    let mut res: Vec<P<Node>> = m.values().copied().collect();
    res.sort_by(|a, b| sort_import_specifiers(*a, *b));
    res
}

impl JSXTransformer {
    // jsx.go:197
    fn visit_source_file(&self, file: P<SourceFile>) -> P<Node> {
        if file.is_declaration_file() {
            return file.as_node();
        }

        self.current_source_file.set(Some(file));
        *self.import_specifier.borrow_mut() =
            ast::get_jsx_implicit_import_base(&self.compiler_options, Some(file));
        self.filename_declaration.set(None);
        self.utilized_implicit_runtime_imports.borrow_mut().clear();

        let f = self.factory();
        let mut visited = self.visitor().visit_each_child(Some(file.as_node())).unwrap();
        self.emit_context().add_emit_helper(visited, &self.emit_context().read_emit_helpers());
        let mut statements: Vec<P<Node>> = visited.statements().to_vec();
        let mut statements_updated = false;
        if let Some(filename_declaration) = self.filename_declaration.get() {
            let s = f.new_variable_statement(
                None,
                f.new_variable_declaration_list(
                    f.new_node_list(vec![filename_declaration]),
                    NodeFlags::Const,
                ),
            );
            self.insert_statement_after_custom_prologue(&mut statements, Some(s));
            statements_updated = true;
        }

        let imports: Vec<(String, FxHashMap<String, P<Node>>)> = self
            .utilized_implicit_runtime_imports
            .borrow()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if !imports.is_empty() {
            if ast::is_external_module(file) {
                statements_updated = true;
                let mut new_statements: Vec<P<Node>> = Vec::with_capacity(imports.len());
                new_statements.extend(imports.iter().map(
                    |(import_source, import_specifiers_map)| {
                        let s = f.new_import_declaration(
                            None,
                            Some(f.new_import_clause(
                                Kind::Unknown,
                                None,
                                Some(f.new_named_imports(
                                    f.new_node_list(get_sorted_specifiers(import_specifiers_map)),
                                )),
                            )),
                            f.new_string_literal(alloc_str(import_source), TokenFlags::None),
                            None,
                        );
                        ast::set_parent_in_children(s);
                        s
                    },
                ));
                for e in new_statements {
                    self.insert_statement_after_custom_prologue(&mut statements, Some(e));
                }
            } else if ast::is_external_or_common_js_module(file) {
                statements_updated = true;
                let mut new_statements: Vec<P<Node>> = Vec::with_capacity(imports.len());
                for (import_source, import_specifiers_map) in &imports {
                    let sorted = get_sorted_specifiers(import_specifiers_map);
                    let mut as_binding_elems: Vec<P<Node>> = Vec::with_capacity(sorted.len());
                    as_binding_elems.extend(sorted.into_iter().map(|elem| {
                        f.new_binding_element(
                            None,
                            elem.property_name(),
                            Some(elem.as_import_specifier().name()),
                            None,
                        )
                    }));
                    let s = f.new_variable_statement(
                        None,
                        f.new_variable_declaration_list(
                            f.new_node_list(vec![f.new_variable_declaration(
                                f.new_binding_pattern(
                                    Kind::ObjectBindingPattern,
                                    f.new_node_list(as_binding_elems),
                                ),
                                None,
                                None,
                                Some(f.new_call_expression(
                                    f.new_identifier("require"),
                                    None,
                                    None,
                                    f.new_node_list(vec![f.new_string_literal(
                                        alloc_str(import_source),
                                        TokenFlags::None,
                                    )]),
                                    NodeFlags::None,
                                )),
                            )]),
                            NodeFlags::Const,
                        ),
                    );
                    ast::set_parent_in_children(s);
                    new_statements.push(s);
                }
                for e in new_statements {
                    self.insert_statement_after_custom_prologue(&mut statements, Some(e));
                }
            } else {
                // Do nothing (script file) - consider an error in the checker?
            }
        }

        if statements_updated {
            visited = f.update_source_file(
                file.as_node(),
                f.new_node_list(statements),
                file.end_of_file_token,
            );
        }

        self.current_source_file.set(None);
        self.import_specifier.borrow_mut().clear();
        self.filename_declaration.set(None);
        self.utilized_implicit_runtime_imports.borrow_mut().clear();

        visited
    }

    // jsx.go:280
    fn visit_jsx_element(&self, element: P<Node>) -> P<Node> {
        let location = TextRange::new(
            scanner::skip_trivia(
                self.current_source_file().text(),
                element.pos().expect_source("parsed nodes have source positions"),
            ),
            element.end().expect_source("parsed nodes have source positions"),
        );
        let e = element.as_jsx_element();
        if self.should_use_create_element(element) {
            return self.visit_jsx_opening_like_element_create_element(
                e.opening_element,
                Some(e.children),
                location,
            );
        }
        self.visit_jsx_opening_like_element_jsx(e.opening_element, Some(e.children), location)
    }

    // jsx.go:289
    fn visit_jsx_self_closing_element(&self, element: P<Node>) -> P<Node> {
        let location = TextRange::new(
            scanner::skip_trivia(
                self.current_source_file().text(),
                element.pos().expect_source("parsed nodes have source positions"),
            ),
            element.end().expect_source("parsed nodes have source positions"),
        );
        if self.should_use_create_element(element) {
            return self.visit_jsx_opening_like_element_create_element(element, None, location);
        }
        self.visit_jsx_opening_like_element_jsx(element, None, location)
    }

    // jsx.go:298
    fn visit_jsx_fragment(&self, fragment: P<Node>) -> P<Node> {
        let location = TextRange::new(
            scanner::skip_trivia(
                self.current_source_file().text(),
                fragment.pos().expect_source("parsed nodes have source positions"),
            ),
            fragment.end().expect_source("parsed nodes have source positions"),
        );
        let fr = fragment.as_jsx_fragment();
        if self.import_specifier.borrow().is_empty() {
            return self.visit_jsx_opening_fragment_create_element(
                fr.opening_fragment,
                Some(fr.children),
                location,
            );
        }
        self.visit_jsx_opening_fragment_jsx(fr.opening_fragment, Some(fr.children), location)
    }

    // jsx.go:307
    fn convert_jsx_children_to_children_prop_object(
        &self,
        children: &[P<Node>],
    ) -> Option<P<Node>> {
        let prop = self.convert_jsx_children_to_children_prop_assignment(children)?;
        Some(
            self.factory()
                .new_object_literal_expression(self.factory().new_node_list(vec![prop]), false),
        )
    }

    // jsx.go:315
    fn transform_jsx_child_to_expression(&self, node: P<Node>) -> Option<P<Node>> {
        let prev = self.in_jsx_child.get();
        self.set_in_child(true);
        // Go: tx.Visitor().Visit(node), the visitor's raw visit callback.
        let result = self.visit(Some(node));
        self.set_in_child(prev);
        result
    }

    // jsx.go:322
    fn convert_jsx_children_to_children_prop_assignment(
        &self,
        children: &[P<Node>],
    ) -> Option<P<Node>> {
        let f = self.factory();
        let non_whitespce_children = ast::get_semantic_jsx_children(children);
        if non_whitespce_children.len() == 1
            && (non_whitespce_children[0].kind() != Kind::JsxExpression
                || non_whitespce_children[0].as_jsx_expression().dot_dot_dot_token.is_none())
        {
            let result = self.transform_jsx_child_to_expression(non_whitespce_children[0])?;
            return Some(f.new_property_assignment(
                None,
                f.new_identifier("children"),
                None,
                None,
                result,
            ));
        }
        // For multiple children in the children property array, don't set StartOnNewLine
        // on child elements — the array literal is single-line.
        let mut results: Vec<P<Node>> = Vec::with_capacity(non_whitespce_children.len());
        results.extend(non_whitespce_children.into_iter().filter_map(|child| {
            let res = self.transform_jsx_child_to_expression(child)?;
            self.emit_context().set_emit_flags(
                res,
                self.emit_context().emit_flags(res) & !EmitFlags::StartOnNewLine,
            );
            Some(res)
        }));
        if results.is_empty() {
            return None;
        }
        Some(f.new_property_assignment(
            None,
            f.new_identifier("children"),
            None,
            None,
            f.new_array_literal_expression(f.new_node_list(results), false),
        ))
    }

    // jsx.go:348
    fn get_tag_name(&self, node: P<Node>) -> P<Node> {
        let f = self.factory();
        if node.kind() == Kind::JsxElement {
            self.get_tag_name(node.as_jsx_element().opening_element)
        } else if ast::is_jsx_opening_like_element(node) {
            let tag_name = node.tag_name();
            if ast::is_identifier(tag_name) && scanner::is_intrinsic_jsx_name(tag_name.text()) {
                f.new_string_literal(tag_name.text(), TokenFlags::None)
            } else if ast::is_jsx_namespaced_name(tag_name) {
                let nn = tag_name.as_jsx_namespaced_name();
                f.new_string_literal(
                    alloc_str(&format!("{}:{}", nn.namespace.text(), nn.name.text())),
                    TokenFlags::None,
                )
            } else {
                f.create_expression_from_entity_name(tag_name)
            }
        } else {
            panic!("unhandled node kind passed to getTagName: {:?}", node.kind());
        }
    }

    // jsx.go:367
    fn visit_jsx_opening_like_element_jsx(
        &self,
        element: P<Node>,
        children: Option<P<NodeList>>,
        location: TextRange,
    ) -> P<Node> {
        let f = self.factory();
        let tag_name = self.get_tag_name(element);
        let mut children_prop: Option<P<Node>> = None;
        if let Some(children) = children
            && !children.nodes().is_empty()
        {
            children_prop = self.convert_jsx_children_to_children_prop_assignment(children.nodes());
        }
        let mut key_attr: Option<P<Node>> = None;
        let mut attrs: Vec<P<Node>> = element.attributes().unwrap().properties().to_vec();
        for (i, p) in attrs.iter().enumerate() {
            let p = *p;
            if p.kind() == Kind::JsxAttribute
                && ast::is_identifier(p.as_jsx_attribute().name())
                && p.as_jsx_attribute().name().text() == "key"
            {
                key_attr = Some(p);
                attrs.remove(i);
                break;
            }
        }
        let object = if attrs.is_empty() {
            let mut object_children: Vec<P<Node>> = Vec::new();
            if let Some(children_prop) = children_prop {
                object_children.push(children_prop);
            }
            f.new_object_literal_expression(f.new_node_list(object_children), false) // When there are no attributes, React wants {}
        } else {
            self.transform_jsx_attributes_to_object_props(&attrs, children_prop)
        };
        self.visit_jsx_opening_like_element_or_fragment_jsx(
            tag_name, object, key_attr, children, location,
        )
    }

    // jsx.go:402
    fn transform_jsx_attributes_to_object_props(
        &self,
        attrs: &[P<Node>],
        children_prop: Option<P<Node>>,
    ) -> P<Node> {
        let target = self.compiler_options.get_emit_script_target();
        if target >= ScriptTarget::ES2018 {
            // target has object spreads, can keep as-is
            return self.factory().new_object_literal_expression(
                self.factory()
                    .new_node_list(self.transform_jsx_attributes_to_props(attrs, children_prop)),
                false,
            );
        }
        self.transform_jsx_attributes_to_expression(attrs, children_prop)
    }

    // jsx.go:411
    fn transform_jsx_attributes_to_expression(
        &self,
        attrs: &[P<Node>],
        children_prop: Option<P<Node>>,
    ) -> P<Node> {
        let f = self.factory();
        let mut expressions: Vec<P<Node>> = Vec::with_capacity(2);
        let mut properties: Vec<P<Node>> = Vec::with_capacity(attrs.len());

        for attr in attrs {
            let attr = *attr;
            if ast::is_jsx_spread_attribute(attr) {
                // as an optimization we try to flatten the first level of spread inline object
                // as if its props would be passed as JSX attributes
                let expr = attr.expression().unwrap();
                if ast::is_object_literal_expression(expr) && !has_proto(expr) {
                    for prop in expr.properties() {
                        let prop = *prop;
                        if ast::is_spread_assignment(prop) {
                            self.combine_properties_into_new_expression(
                                &mut expressions,
                                &mut properties,
                            );
                            expressions.extend(self.visit(prop.expression()));
                            continue;
                        }
                        properties.extend(self.visit(Some(prop)));
                    }
                    continue;
                }
                self.combine_properties_into_new_expression(&mut expressions, &mut properties);
                expressions.extend(self.visit(Some(expr)));
                continue;
            }
            properties.push(self.transform_jsx_attribute_to_object_literal_element(attr));
        }

        if let Some(children_prop) = children_prop {
            properties.push(children_prop);
        }

        self.combine_properties_into_new_expression(&mut expressions, &mut properties);

        if !expressions.is_empty() && !ast::is_object_literal_expression(expressions[0]) {
            // We must always emit at least one object literal before a spread attribute
            // as the JSX always factory expects a fresh object, so we need to make a copy here
            // we also avoid mutating an external reference by doing this (first expression is used as assign's target)
            expressions.insert(0, f.new_object_literal_expression(f.new_node_list(vec![]), false));
        }

        if expressions.len() == 1 {
            return expressions[0];
        }
        f.new_assign_helper(expressions, self.compiler_options.get_emit_script_target())
    }

    // jsx.go:456
    fn combine_properties_into_new_expression(
        &self,
        expressions: &mut Vec<P<Node>>,
        props: &mut Vec<P<Node>>,
    ) {
        if props.is_empty() {
            return;
        }
        let new_obj = self.factory().new_object_literal_expression(
            self.factory().new_node_list(std::mem::take(props)),
            false,
        );
        expressions.push(new_obj);
    }

    // jsx.go:465
    fn transform_jsx_attributes_to_props(
        &self,
        attrs: &[P<Node>],
        children_prop: Option<P<Node>>,
    ) -> Vec<P<Node>> {
        let mut props: Vec<P<Node>> = Vec::with_capacity(attrs.len());
        props = attrs.iter().fold(props, |mut props, attr| {
            let attr = *attr;
            if attr.kind() == Kind::JsxSpreadAttribute {
                let res = self.transform_jsx_spread_attributes_to_props(attr);
                props.extend(res);
            } else {
                props.push(self.transform_jsx_attribute_to_object_literal_element(attr));
            }

            props
        });
        if let Some(children_prop) = children_prop {
            props.push(children_prop);
        }
        props
    }
}

// jsx.go:481
fn has_proto(obj: P<Node>) -> bool {
    if obj.properties().iter().any(|p| {
        let p = *p;
        ast::is_property_assignment(p)
            && (ast::is_string_literal(p.name().unwrap()) || ast::is_identifier(p.name().unwrap()))
            && p.name().unwrap().text() == "__proto__"
    }) {
        return true;
    }
    false
}

impl JSXTransformer {
    // jsx.go:490
    fn transform_jsx_spread_attributes_to_props(&self, node: P<Node>) -> Vec<P<Node>> {
        let expression = node.as_jsx_spread_attribute().expression;
        if ast::is_object_literal_expression(expression) && !has_proto(expression) {
            let (res, _) = self.visitor().visit_slice(expression.properties());
            return res.to_vec();
        }
        // Go: tx.Visitor().Visit(node.Expression)
        vec![self.factory().new_spread_assignment(self.visit(Some(expression)).unwrap())]
    }

    // jsx.go:498
    fn transform_jsx_attribute_to_object_literal_element(&self, node: P<Node>) -> P<Node> {
        let name = self.get_attribute_name(node);
        let expression =
            self.transform_jsx_attribute_initializer(node.as_jsx_attribute().initializer);
        self.factory().new_property_assignment(None, name, None, None, expression)
    }

    // jsx.go:509
    /*
     * Emit an attribute name, which is quoted if it needs to be quoted. Because
     * these emit into an object literal property name, we don't need to be worried
     * about keywords, just non-identifier characters
     */
    fn get_attribute_name(&self, node: P<Node>) -> P<Node> {
        let name = node.as_jsx_attribute().name;
        if ast::is_identifier(name) {
            let text = name.text();
            if scanner::is_identifier_text(text, LanguageVariant::Standard) {
                return name;
            }
            return self.factory().new_string_literal(text, TokenFlags::None);
        }
        // must be jsx namespace
        let nn = name.as_jsx_namespaced_name();
        self.factory().new_string_literal(
            alloc_str(&format!("{}:{}", nn.namespace.text(), nn.name.text())),
            TokenFlags::None,
        )
    }

    // jsx.go:524
    fn transform_jsx_attribute_initializer(&self, node: Option<P<Node>>) -> P<Node> {
        let f = self.factory();
        let Some(node) = node else {
            return f.new_true_expression();
        };
        if node.kind() == Kind::StringLiteral {
            // Always recreate the literal to escape any escape sequences or newlines which may be in the original jsx string and which
            // Need to be escaped to be handled correctly in a normal string
            let token_flags = node.as_string_literal().literal_like_node_base.token_flags.get();
            let res = f.new_string_literal(alloc_str(&decode_entities(node.text())), token_flags);
            res.set_loc(node.loc());
            // Preserve the original quote style (single vs double quotes)
            res.as_string_literal().literal_like_node_base.token_flags.set(token_flags);
            return res;
        }
        if node.kind() == Kind::JsxExpression {
            let Some(expression) = node.expression() else {
                return f.new_true_expression();
            };
            return self.visit(Some(expression)).unwrap();
        }
        if ast::is_jsx_element(node)
            || ast::is_jsx_self_closing_element(node)
            || ast::is_jsx_fragment(node)
        {
            self.set_in_child(false);
            return self.visit(Some(node)).unwrap();
        }
        panic!("Unhandled node kind found in jsx initializer: {:?}", node.kind());
    }

    // jsx.go:550
    fn visit_jsx_opening_like_element_or_fragment_jsx(
        &self,
        tag_name: P<Node>,
        object: P<Node>,
        key_attr: Option<P<Node>>,
        children: Option<P<NodeList>>,
        location: TextRange,
    ) -> P<Node> {
        let f = self.factory();
        let mut non_whitespace_children: Vec<P<Node>> = Vec::new();
        if let Some(children) = children {
            non_whitespace_children = ast::get_semantic_jsx_children(children.nodes());
        }
        let is_static_children = non_whitespace_children.len() > 1
            || (non_whitespace_children.len() == 1
                && ast::is_jsx_expression(non_whitespace_children[0])
                && non_whitespace_children[0].as_jsx_expression().dot_dot_dot_token.is_some());
        let mut args: Vec<P<Node>> = Vec::with_capacity(3);
        args.push(tag_name);
        args.push(object);
        // function jsx(type, config, maybeKey) {}
        // "maybeKey" is optional. It is acceptable to use "_jsx" without a third argument
        if let Some(key_attr) = key_attr {
            args.push(self.transform_jsx_attribute_initializer(key_attr.initializer()));
        }

        if self.compiler_options.jsx == JsxEmit::REACT_JSX_DEV {
            let original_file =
                self.emit_context().most_original(Some(self.current_source_file().as_node()));
            if let Some(original_file) = original_file.filter(|n| ast::is_source_file(*n)) {
                // "maybeKey" has to be replaced with "void 0" to not break the jsxDEV signature
                if key_attr.is_none() {
                    args.push(f.new_void_zero_expression());
                }
                // isStaticChildren development flag
                if is_static_children {
                    args.push(f.new_true_expression());
                } else {
                    args.push(f.new_false_expression());
                }
                // __source development flag
                let (line, col) = scanner::get_ecma_line_and_utf16_character_of_position(
                    original_file.as_source_file(),
                    location.pos().expect_source("JSX source map ranges have source positions"),
                );
                args.push(f.new_object_literal_expression(
                    f.new_node_list(vec![
                        f.new_property_assignment(
                            None,
                            f.new_identifier("fileName"),
                            None,
                            None,
                            self.get_current_file_name_expression(),
                        ),
                        f.new_property_assignment(
                            None,
                            f.new_identifier("lineNumber"),
                            None,
                            None,
                            f.new_numeric_literal(
                                alloc_str(&(line as i64 + 1).to_string()),
                                TokenFlags::None,
                            ),
                        ),
                        f.new_property_assignment(
                            None,
                            f.new_identifier("columnNumber"),
                            None,
                            None,
                            f.new_numeric_literal(
                                alloc_str(&(col as i64 + 1).to_string()),
                                TokenFlags::None,
                            ),
                        ),
                    ]),
                    false,
                ));
                // __self development flag
                args.push(f.new_this_expression());
            }
        }

        let element = f.new_call_expression(
            self.get_jsx_factory_callee(is_static_children),
            None,
            None,
            f.new_node_list(args),
            NodeFlags::None,
        );
        element.set_loc(location);

        if self.in_jsx_child.get() {
            self.emit_context().add_emit_flags(element, EmitFlags::StartOnNewLine);
        }

        element
    }

    // jsx.go:605
    fn visit_jsx_opening_fragment_jsx(
        &self,
        _fragment: P<Node>,
        children: Option<P<NodeList>>,
        location: TextRange,
    ) -> P<Node> {
        let mut children_props: Option<P<Node>> = None;
        if let Some(children) = children
            && !children.nodes().is_empty()
            && let Some(result) =
                self.convert_jsx_children_to_children_prop_object(children.nodes())
        {
            children_props = Some(result);
        }
        let children_props = match children_props {
            Some(c) => c,
            None => self
                .factory()
                .new_object_literal_expression(self.factory().new_node_list(vec![]), false),
        };
        self.visit_jsx_opening_like_element_or_fragment_jsx(
            self.get_implicit_jsx_fragment_reference(),
            children_props,
            None,
            children,
            location,
        )
    }

    // jsx.go:625
    fn create_react_namespace(&self, react_namespace: &str, parent: P<Node>) -> P<Node> {
        // To ensure the emit resolver can properly resolve the namespace, we need to
        // treat this identifier as if it were a source tree node by clearing the `Synthesized`
        // flag and setting a parent node. TODO: Is this still true? The emit resolver is supposed to be
        // hardened aginast this, so long as the node retains original node pointers back to a parsed node
        let f = self.factory();
        let react_namespace = if react_namespace.is_empty() { "React" } else { react_namespace };
        let react = f.new_identifier(alloc_str(react_namespace));
        react.set_flags(react.flags() & !NodeFlags::Synthesized);

        // Set the parent that is in parse tree
        // this makes sure that parent chain is intact for checker to traverse complete scope tree
        react.set_parent(self.emit_context().parse_node(Some(parent)));

        // If the identifier refers to an exported member of a namespace, substitute with
        // a qualified namespace property access (e.g., `React` -> `M.React`).
        // See also: RuntimeSyntaxTransformer.visitExpressionIdentifier in runtimesyntax.go
        if let Some(container) =
            self.emit_resolver.get_referenced_export_container(react, false /*prefixLocals*/)
            && ast::is_module_declaration(container)
        {
            let container_name = f.new_generated_name_for_node(container);
            return f.new_property_access_expression(container_name, None, react, NodeFlags::None);
        }

        react
    }

    // jsx.go:651
    fn create_jsx_factory_expression_from_entity_name(
        &self,
        e: P<Node>,
        parent: P<Node>,
    ) -> P<Node> {
        if ast::is_qualified_name(e) {
            let left = self
                .create_jsx_factory_expression_from_entity_name(e.as_qualified_name().left, parent);
            let right = self.factory().new_identifier(e.as_qualified_name().right.text());
            return self.factory().new_property_access_expression(
                left,
                None,
                right,
                NodeFlags::None,
            );
        }
        self.create_react_namespace(e.text(), parent)
    }

    // jsx.go:660
    fn create_jsx_pseudo_factory_expression(
        &self,
        parent: P<Node>,
        e: Option<P<Node>>,
        target: &'static str,
    ) -> P<Node> {
        if let Some(e) = e {
            return self.create_jsx_factory_expression_from_entity_name(e, parent);
        }
        self.factory().new_property_access_expression(
            self.create_react_namespace(&self.compiler_options.react_namespace, parent),
            None,
            self.factory().new_identifier(target),
            NodeFlags::None,
        )
    }

    // jsx.go:672
    fn create_jsx_factory_expression(&self, parent: P<Node>) -> P<Node> {
        let e = self.emit_resolver.get_jsx_factory_entity(self.current_source_file().as_node());
        self.create_jsx_pseudo_factory_expression(parent, e, "createElement")
    }

    // jsx.go:677
    fn create_jsx_fragment_factory_expression(&self, parent: P<Node>) -> P<Node> {
        let e = self
            .emit_resolver
            .get_jsx_fragment_factory_entity(self.current_source_file().as_node());
        self.create_jsx_pseudo_factory_expression(parent, e, "Fragment")
    }

    // jsx.go:682
    fn visit_jsx_opening_like_element_create_element(
        &self,
        element: P<Node>,
        children: Option<P<NodeList>>,
        location: TextRange,
    ) -> P<Node> {
        let f = self.factory();
        let tag_name = self.get_tag_name(element);
        let attrs = element.attributes().unwrap().properties();
        let object_properties = if attrs.is_empty() {
            f.new_keyword_expression(Kind::NullKeyword) // When there are no attributes, React wants "null"
        } else {
            self.transform_jsx_attributes_to_object_props(attrs, None)
        };

        let callee = if self.import_specifier.borrow().is_empty() {
            self.create_jsx_factory_expression(element)
        } else {
            self.get_implicit_import_for_name("createElement")
        };

        let mut new_children: Vec<P<Node>> = Vec::new();
        if let Some(children) = children {
            new_children.extend(
                children.nodes().iter().filter_map(|&c| self.transform_jsx_child_to_expression(c)),
            );
        }

        // Add StartOnNewLine flag only if there are multiple actual children (after filtering)
        if new_children.len() > 1 {
            for child in &new_children {
                self.emit_context().add_emit_flags(*child, EmitFlags::StartOnNewLine);
            }
        }

        let mut args: Vec<P<Node>> = Vec::with_capacity(new_children.len() + 2);
        args.push(tag_name);
        args.push(object_properties);
        args.extend(new_children);

        let result =
            f.new_call_expression(callee, None, None, f.new_node_list(args), NodeFlags::None);
        result.set_loc(location);

        if self.in_jsx_child.get() {
            self.emit_context().add_emit_flags(result, EmitFlags::StartOnNewLine);
        }
        result
    }

    // jsx.go:736
    fn visit_jsx_opening_fragment_create_element(
        &self,
        fragment: P<Node>,
        children: Option<P<NodeList>>,
        location: TextRange,
    ) -> P<Node> {
        let f = self.factory();
        let tag_name = self.create_jsx_fragment_factory_expression(fragment);
        let callee = self.create_jsx_factory_expression(fragment);

        let mut new_children: Vec<P<Node>> = Vec::new();
        if let Some(children) = children {
            new_children.extend(
                children.nodes().iter().filter_map(|&c| self.transform_jsx_child_to_expression(c)),
            );
        }

        // Add StartOnNewLine flag only if there are multiple actual children (after filtering)
        if new_children.len() > 1 {
            for child in &new_children {
                self.emit_context().add_emit_flags(*child, EmitFlags::StartOnNewLine);
            }
        }

        let mut args: Vec<P<Node>> = Vec::with_capacity(new_children.len() + 2);
        args.push(tag_name);
        args.push(f.new_keyword_expression(Kind::NullKeyword));
        args.extend(new_children);

        let result =
            f.new_call_expression(callee, None, None, f.new_node_list(args), NodeFlags::None);
        result.set_loc(location);

        if self.in_jsx_child.get() {
            self.emit_context().add_emit_flags(result, EmitFlags::StartOnNewLine);
        }
        result
    }

    // jsx.go:777
    fn visit_jsx_text(&self, text: P<Node>) -> Option<P<Node>> {
        let fixed = fixup_whitespace_and_decode_entities(text.as_jsx_text().text());
        if fixed.is_empty() {
            return None;
        }
        Some(self.factory().new_string_literal(alloc_str(&fixed), TokenFlags::None))
    }
}

// jsx.go:785
fn add_line_of_jsx_text(b: &mut String, trimmed_line: &str, is_initial: bool) {
    // We do not escape the string here as that is handled by the printer
    // when it emits the literal. We do, however, need to decode JSX entities.
    let decoded = decode_entities(trimmed_line);
    if !is_initial {
        b.push(' ');
    }
    b.push_str(&decoded);
}

// jsx.go:810
/*
 * JSX trims whitespace at the end and beginning of lines, except that the
 * start/end of a tag is considered a start/end of a line only if that line is
 * on the same line as the closing tag. See examples in
 * tests/cases/conformance/jsx/tsxReactEmitWhitespace.tsx
 * See also https://www.w3.org/TR/html4/struct/text.html#h-9.1 and https://www.w3.org/TR/CSS2/text.html#white-space-model
 *
 * An equivalent algorithm would be:
 * - If there is only one line, return it.
 * - If there is only whitespace (but multiple lines), return `undefined`.
 * - Split the text into lines.
 * - 'trimRight' the first line, 'trimLeft' the last line, 'trim' middle lines.
 * - Decode entities on each line (individually).
 * - Remove empty lines and join the rest with " ".
 */
fn fixup_whitespace_and_decode_entities(text: &str) -> String {
    let mut acc = String::new();
    let mut initial = true;
    // First non-whitespace character on this line.
    let mut first_non_whitespace: Option<usize> = Some(0);
    // End byte position of the last non-whitespace character on this line.
    let mut last_non_whitespace_end: Option<usize> = None;
    // These initial values are special because the first line is:
    // firstNonWhitespace = 0 to indicate that we want leading whitespace,
    // but lastNonWhitespaceEnd = None as a special flag to indicate that we *don't* include the line if it's all whitespace.
    let bytes = text.as_bytes();
    let mut i: usize = 0;
    while i < bytes.len() {
        let (c, size) = stringutil::decode_rune(&bytes[i..]);
        if stringutil::is_line_break(c) {
            // If we've seen any non-whitespace characters on this line, add the 'trim' of the line.
            // (lastNonWhitespaceEnd being None is a special flag to detect whether the first line is all whitespace.)
            if let (Some(first_non_whitespace), Some(last_non_whitespace_end)) =
                (first_non_whitespace, last_non_whitespace_end)
            {
                add_line_of_jsx_text(
                    &mut acc,
                    &text[first_non_whitespace..=last_non_whitespace_end],
                    initial,
                );
                initial = false;
            }

            // Reset firstNonWhitespace for the next line.
            // Don't bother to reset lastNonWhitespaceEnd because we ignore it if firstNonWhitespace is absent.
            first_non_whitespace = None;
        } else if !stringutil::is_white_space_single_line(c) {
            last_non_whitespace_end = Some(i + size - 1); // Store the end byte position of the character
            first_non_whitespace.get_or_insert(i);
        }

        if size > 1 {
            i += size - 1;
        }
        i += 1;
    }

    if let Some(first_non_whitespace) = first_non_whitespace {
        // Last line had a non-whitespace character. Emit the 'trimLeft', meaning keep trailing whitespace.
        add_line_of_jsx_text(&mut acc, &text[first_non_whitespace..], initial);
    }
    acc
}

impl JSXTransformer {
    // jsx.go:852
    fn visit_jsx_expression(&self, expression: P<Node>) -> Option<P<Node>> {
        let e = self.visit(expression.as_jsx_expression().expression);
        if expression.as_jsx_expression().dot_dot_dot_token.is_some() {
            return Some(self.factory().new_spread_element(e.unwrap()));
        }
        e
    }
}

// jsx.go:864
/*
 * Replace entities like "&nbsp;", "&#123;", and "&#xDEADBEEF;" with the characters they encode.
 * See https://en.wikipedia.org/wiki/List_of_XML_and_HTML_character_entity_references
 */
fn decode_entities(text: &str) -> String {
    let Some(mut i) = text.find('&') else {
        return text.to_string();
    };

    let mut text = text;
    let mut result = String::with_capacity(text.len());
    loop {
        result.push_str(&text[..i]);
        text = &text[i..];

        let Some(mut semi) = text.find(';') else {
            break;
        };

        // Skip past any intervening '&' characters between the current '&'
        // and the ';'. Each such '&' is not part of a valid entity, so emit
        // it (and any text before the next '&') as literals.
        while let Some(next_amp) = text[1..semi].find('&') {
            result.push_str(&text[..=next_amp]);
            text = &text[next_amp + 1..];
            semi -= next_amp + 1;
        }

        let entity = &text[1..semi];
        if let Some(decoded) = decode_entity(entity) {
            // Use the JS-string encoder so lone surrogates (e.g. "&#xD800;")
            // are preserved rather than being lost to U+FFFD by WriteRune.
            result.push_str(&stringutil::encode_js_string_rune(decoded));
        } else {
            result.push_str(&text[..=semi]);
        }
        text = &text[semi + 1..];

        match text.find('&') {
            Some(next) => i = next,
            None => break,
        }
    }
    result.push_str(text);
    result
}

// jsx.go:914
fn decode_entity(entity: &str) -> Option<stringutil::Rune> {
    if entity.is_empty() {
        return None;
    }

    if entity.as_bytes()[0] == b'#' {
        let mut entity = &entity[1..];
        if entity.is_empty() {
            return None;
        }

        let mut base = 10;
        if entity.as_bytes()[0] == b'x' {
            base = 16;
            entity = &entity[1..];
        }

        if entity.is_empty() {
            return None;
        }

        if let Some(result) = entity.chars().into_iter().find_map(|c| {
            if base == 16 && !stringutil::is_hex_digit(c as u32) {
                return Some(None);
            }
            if base == 10 && !stringutil::is_digit(c as u32) {
                return Some(None);
            }

            None
        }) {
            return result;
        }

        return u32::from_str_radix(entity, base).ok();
    }

    entities(entity)
}

// jsx.go:955
fn entities(name: &str) -> Option<stringutil::Rune> {
    Some(match name {
        "quot" => 0x0022,
        "amp" => 0x0026,
        "apos" => 0x0027,
        "lt" => 0x003C,
        "gt" => 0x003E,
        "nbsp" => 0x00A0,
        "iexcl" => 0x00A1,
        "cent" => 0x00A2,
        "pound" => 0x00A3,
        "curren" => 0x00A4,
        "yen" => 0x00A5,
        "brvbar" => 0x00A6,
        "sect" => 0x00A7,
        "uml" => 0x00A8,
        "copy" => 0x00A9,
        "ordf" => 0x00AA,
        "laquo" => 0x00AB,
        "not" => 0x00AC,
        "shy" => 0x00AD,
        "reg" => 0x00AE,
        "macr" => 0x00AF,
        "deg" => 0x00B0,
        "plusmn" => 0x00B1,
        "sup2" => 0x00B2,
        "sup3" => 0x00B3,
        "acute" => 0x00B4,
        "micro" => 0x00B5,
        "para" => 0x00B6,
        "middot" => 0x00B7,
        "cedil" => 0x00B8,
        "sup1" => 0x00B9,
        "ordm" => 0x00BA,
        "raquo" => 0x00BB,
        "frac14" => 0x00BC,
        "frac12" => 0x00BD,
        "frac34" => 0x00BE,
        "iquest" => 0x00BF,
        "Agrave" => 0x00C0,
        "Aacute" => 0x00C1,
        "Acirc" => 0x00C2,
        "Atilde" => 0x00C3,
        "Auml" => 0x00C4,
        "Aring" => 0x00C5,
        "AElig" => 0x00C6,
        "Ccedil" => 0x00C7,
        "Egrave" => 0x00C8,
        "Eacute" => 0x00C9,
        "Ecirc" => 0x00CA,
        "Euml" => 0x00CB,
        "Igrave" => 0x00CC,
        "Iacute" => 0x00CD,
        "Icirc" => 0x00CE,
        "Iuml" => 0x00CF,
        "ETH" => 0x00D0,
        "Ntilde" => 0x00D1,
        "Ograve" => 0x00D2,
        "Oacute" => 0x00D3,
        "Ocirc" => 0x00D4,
        "Otilde" => 0x00D5,
        "Ouml" => 0x00D6,
        "times" => 0x00D7,
        "Oslash" => 0x00D8,
        "Ugrave" => 0x00D9,
        "Uacute" => 0x00DA,
        "Ucirc" => 0x00DB,
        "Uuml" => 0x00DC,
        "Yacute" => 0x00DD,
        "THORN" => 0x00DE,
        "szlig" => 0x00DF,
        "agrave" => 0x00E0,
        "aacute" => 0x00E1,
        "acirc" => 0x00E2,
        "atilde" => 0x00E3,
        "auml" => 0x00E4,
        "aring" => 0x00E5,
        "aelig" => 0x00E6,
        "ccedil" => 0x00E7,
        "egrave" => 0x00E8,
        "eacute" => 0x00E9,
        "ecirc" => 0x00EA,
        "euml" => 0x00EB,
        "igrave" => 0x00EC,
        "iacute" => 0x00ED,
        "icirc" => 0x00EE,
        "iuml" => 0x00EF,
        "eth" => 0x00F0,
        "ntilde" => 0x00F1,
        "ograve" => 0x00F2,
        "oacute" => 0x00F3,
        "ocirc" => 0x00F4,
        "otilde" => 0x00F5,
        "ouml" => 0x00F6,
        "divide" => 0x00F7,
        "oslash" => 0x00F8,
        "ugrave" => 0x00F9,
        "uacute" => 0x00FA,
        "ucirc" => 0x00FB,
        "uuml" => 0x00FC,
        "yacute" => 0x00FD,
        "thorn" => 0x00FE,
        "yuml" => 0x00FF,
        "OElig" => 0x0152,
        "oelig" => 0x0153,
        "Scaron" => 0x0160,
        "scaron" => 0x0161,
        "Yuml" => 0x0178,
        "fnof" => 0x0192,
        "circ" => 0x02C6,
        "tilde" => 0x02DC,
        "Alpha" => 0x0391,
        "Beta" => 0x0392,
        "Gamma" => 0x0393,
        "Delta" => 0x0394,
        "Epsilon" => 0x0395,
        "Zeta" => 0x0396,
        "Eta" => 0x0397,
        "Theta" => 0x0398,
        "Iota" => 0x0399,
        "Kappa" => 0x039A,
        "Lambda" => 0x039B,
        "Mu" => 0x039C,
        "Nu" => 0x039D,
        "Xi" => 0x039E,
        "Omicron" => 0x039F,
        "Pi" => 0x03A0,
        "Rho" => 0x03A1,
        "Sigma" => 0x03A3,
        "Tau" => 0x03A4,
        "Upsilon" => 0x03A5,
        "Phi" => 0x03A6,
        "Chi" => 0x03A7,
        "Psi" => 0x03A8,
        "Omega" => 0x03A9,
        "alpha" => 0x03B1,
        "beta" => 0x03B2,
        "gamma" => 0x03B3,
        "delta" => 0x03B4,
        "epsilon" => 0x03B5,
        "zeta" => 0x03B6,
        "eta" => 0x03B7,
        "theta" => 0x03B8,
        "iota" => 0x03B9,
        "kappa" => 0x03BA,
        "lambda" => 0x03BB,
        "mu" => 0x03BC,
        "nu" => 0x03BD,
        "xi" => 0x03BE,
        "omicron" => 0x03BF,
        "pi" => 0x03C0,
        "rho" => 0x03C1,
        "sigmaf" => 0x03C2,
        "sigma" => 0x03C3,
        "tau" => 0x03C4,
        "upsilon" => 0x03C5,
        "phi" => 0x03C6,
        "chi" => 0x03C7,
        "psi" => 0x03C8,
        "omega" => 0x03C9,
        "thetasym" => 0x03D1,
        "upsih" => 0x03D2,
        "piv" => 0x03D6,
        "ensp" => 0x2002,
        "emsp" => 0x2003,
        "thinsp" => 0x2009,
        "zwnj" => 0x200C,
        "zwj" => 0x200D,
        "lrm" => 0x200E,
        "rlm" => 0x200F,
        "ndash" => 0x2013,
        "mdash" => 0x2014,
        "lsquo" => 0x2018,
        "rsquo" => 0x2019,
        "sbquo" => 0x201A,
        "ldquo" => 0x201C,
        "rdquo" => 0x201D,
        "bdquo" => 0x201E,
        "dagger" => 0x2020,
        "Dagger" => 0x2021,
        "bull" => 0x2022,
        "hellip" => 0x2026,
        "permil" => 0x2030,
        "prime" => 0x2032,
        "Prime" => 0x2033,
        "lsaquo" => 0x2039,
        "rsaquo" => 0x203A,
        "oline" => 0x203E,
        "frasl" => 0x2044,
        "euro" => 0x20AC,
        "image" => 0x2111,
        "weierp" => 0x2118,
        "real" => 0x211C,
        "trade" => 0x2122,
        "alefsym" => 0x2135,
        "larr" => 0x2190,
        "uarr" => 0x2191,
        "rarr" => 0x2192,
        "darr" => 0x2193,
        "harr" => 0x2194,
        "crarr" => 0x21B5,
        "lArr" => 0x21D0,
        "uArr" => 0x21D1,
        "rArr" => 0x21D2,
        "dArr" => 0x21D3,
        "hArr" => 0x21D4,
        "forall" => 0x2200,
        "part" => 0x2202,
        "exist" => 0x2203,
        "empty" => 0x2205,
        "nabla" => 0x2207,
        "isin" => 0x2208,
        "notin" => 0x2209,
        "ni" => 0x220B,
        "prod" => 0x220F,
        "sum" => 0x2211,
        "minus" => 0x2212,
        "lowast" => 0x2217,
        "radic" => 0x221A,
        "prop" => 0x221D,
        "infin" => 0x221E,
        "ang" => 0x2220,
        "and" => 0x2227,
        "or" => 0x2228,
        "cap" => 0x2229,
        "cup" => 0x222A,
        "int" => 0x222B,
        "there4" => 0x2234,
        "sim" => 0x223C,
        "cong" => 0x2245,
        "asymp" => 0x2248,
        "ne" => 0x2260,
        "equiv" => 0x2261,
        "le" => 0x2264,
        "ge" => 0x2265,
        "sub" => 0x2282,
        "sup" => 0x2283,
        "nsub" => 0x2284,
        "sube" => 0x2286,
        "supe" => 0x2287,
        "oplus" => 0x2295,
        "otimes" => 0x2297,
        "perp" => 0x22A5,
        "sdot" => 0x22C5,
        "lceil" => 0x2308,
        "rceil" => 0x2309,
        "lfloor" => 0x230A,
        "rfloor" => 0x230B,
        "lang" => 0x2329,
        "rang" => 0x232A,
        "loz" => 0x25CA,
        "spades" => 0x2660,
        "clubs" => 0x2663,
        "hearts" => 0x2665,
        "diams" => 0x2666,
        _ => return None,
    })
}
