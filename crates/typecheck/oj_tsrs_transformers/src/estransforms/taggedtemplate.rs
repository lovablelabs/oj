use crate::*;

// taggedtemplate.go:13 (Go `strings.NewReplacer("\r\n", "\n", "\r", "\n")`)
fn newline_normalizer_replace(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

// taggedtemplate.go:15
struct TaggedTemplateTransformer {
    base: Transformer,
    current_source_file: Cell<Option<P<SourceFile>>>,

    tagged_template_string_declarations: RefCell<Vec<P<Node>>>,
}

// taggedtemplate.go:22
#[expect(
    clippy::unnecessary_wraps,
    reason = "Transformer factories share an optional result so disabled transforms can return None."
)]
pub fn new_tagged_template_lift_restriction_transformer(
    opt: &TransformOptions,
) -> Option<P<Transformer>> {
    let tx = P::new(TaggedTemplateTransformer {
        base: Transformer::default(),
        current_source_file: Cell::new(None),
        tagged_template_string_declarations: RefCell::new(Vec::new()),
    });
    Some(tx.get().base.new_transformer(
        Rc::new(move |_: &mut NodeVisitor, n: P<Node>| tx.visit(n)),
        Some(opt.context),
    ))
}

impl TaggedTemplateTransformer {
    fn emit_context(&self) -> P<EmitContext> {
        self.base.emit_context()
    }

    fn factory(&self) -> &'static printer::NodeFactory {
        self.base.factory()
    }

    fn visitor(&self) -> NodeVisitor {
        self.base.visitor()
    }

    // taggedtemplate.go:27
    fn visit(&self, node: P<Node>) -> Option<P<Node>> {
        if !node.subtree_facts().intersects(SubtreeFacts::ContainsInvalidTemplateEscape) {
            return Some(node);
        }
        match node.kind() {
            Kind::SourceFile => Some(self.visit_source_file(node.as_source_file_p())),
            Kind::TaggedTemplateExpression => Some(self.visit_tagged_template_expression(node)),
            _ => self.visitor().visit_each_child(Some(node)),
        }
    }

    // taggedtemplate.go:41
    fn visit_source_file(&self, node: P<SourceFile>) -> P<Node> {
        self.current_source_file.set(Some(node));
        self.tagged_template_string_declarations.borrow_mut().clear();
        let mut visited = self.visitor().visit_each_child(Some(node.as_node())).unwrap();

        let declarations: Vec<P<Node>> = self.tagged_template_string_declarations.borrow().clone();
        if !declarations.is_empty() {
            let f = self.factory();
            let visited_source_file = visited.as_source_file_p();
            let mut statements: Vec<P<Node>> = visited_source_file.statements.nodes().to_vec();
            statements.push(f.new_variable_statement(
                None, /*modifiers*/
                f.new_variable_declaration_list(f.new_node_list(declarations), NodeFlags::None),
            ));
            let stmt_list = f.new_node_list(statements);
            stmt_list.loc.set(node.statements.loc.get());
            visited =
                f.update_source_file(visited, stmt_list, visited_source_file.end_of_file_token);
        }

        self.emit_context().add_emit_helper(visited, &self.emit_context().read_emit_helpers());
        visited
    }

    // taggedtemplate.go:67
    fn visit_tagged_template_expression(&self, node: P<Node>) -> P<Node> {
        self.process_tagged_template_expression(node)
    }

    // taggedtemplate.go:71
    fn process_tagged_template_expression(&self, node: P<Node>) -> P<Node> {
        let t = node.as_tagged_template_expression();
        let tag = self.visitor().visit_node(Some(t.tag)).unwrap();
        let template = t.template;

        if !has_invalid_escape(template) {
            return self.visitor().visit_each_child(Some(node)).unwrap();
        }

        let f = self.factory();

        // Build up the template arguments and the raw and cooked strings for the template.
        let mut template_arguments: Vec<Option<P<Node>>> = vec![None]; // placeholder for the template object
        let mut cooked_strings: Vec<P<Node>> = Vec::new();
        let mut raw_strings: Vec<P<Node>> = Vec::new();

        if ast::is_no_substitution_template_literal(template) {
            cooked_strings
                .push(create_template_cooked(f, template.template_literal_like_data().unwrap()));
            raw_strings.push(get_raw_literal(f, template));
        } else {
            let te = template.as_template_expression();
            cooked_strings
                .push(create_template_cooked(f, te.head.template_literal_like_data().unwrap()));
            raw_strings.push(get_raw_literal(f, te.head));
            (cooked_strings, raw_strings, template_arguments) =
                te.template_spans.nodes().iter().fold(
                    (cooked_strings, raw_strings, template_arguments),
                    |(mut cooked_strings, mut raw_strings, mut template_arguments), span| {
                        let ts = span.as_template_span();
                        cooked_strings.push(create_template_cooked(
                            f,
                            ts.literal.template_literal_like_data().unwrap(),
                        ));
                        raw_strings.push(get_raw_literal(f, ts.literal));
                        template_arguments.push(self.visitor().visit_node(Some(ts.expression)));

                        (cooked_strings, raw_strings, template_arguments)
                    },
                );
        }

        let helper_call = f.new_template_object_helper(
            f.new_array_literal_expression(f.new_node_list(cooked_strings), false),
            f.new_array_literal_expression(f.new_node_list(raw_strings), false),
        );

        // Create a variable to cache the template object if we're in a module.
        // Do not do this in the global scope, as any variable we currently generate could conflict with
        // variables from outside of the current compilation. In the future, we can revisit this behavior.
        if ast::is_external_module(self.current_source_file.get().unwrap()) {
            let temp_var = f.new_unique_name("templateObject");
            self.tagged_template_string_declarations
                .borrow_mut()
                .push(f.new_variable_declaration(temp_var, None, None, None));
            template_arguments[0] = Some(f.new_logical_or_expression(
                temp_var,
                f.new_assignment_expression(temp_var, helper_call),
            ));
        } else {
            template_arguments[0] = Some(helper_call);
        }

        let call = f.new_call_expression(
            tag,
            None, /*questionDotToken*/
            None, /*typeArguments*/
            f.new_node_list(template_arguments.into_iter().flatten().collect()),
            NodeFlags::None,
        );
        call.set_loc(node.loc());
        call
    }
}

// taggedtemplate.go:128
fn create_template_cooked(
    f: &printer::NodeFactory,
    template: &TemplateLiteralLikeNodeBase,
) -> P<Node> {
    if template.template_flags.intersects(TokenFlags::IsInvalid) {
        return f.new_void_zero_expression();
    }
    f.new_string_literal(template.literal_like_node_base.text(), TokenFlags::None)
}

// taggedtemplate.go:135
fn get_raw_literal(f: &printer::NodeFactory, node: P<Node>) -> P<Node> {
    let mut text: String = node.template_literal_like_data().unwrap().raw_text.to_string();
    if text.is_empty() {
        text = scanner::get_source_text_of_node_from_source_file(
            ast::get_source_file_of_node(node).unwrap(),
            node,
            false, /*includeTrivia*/
        );
        // text contains the original source, it will also contain quotes ("`"), dollar signs and braces ("${" and "}"),
        // thus we need to remove those characters.
        // First template piece starts with "`", others with "}"
        // Last template piece ends with "`", others with "${"
        let is_last =
            node.kind() == Kind::NoSubstitutionTemplateLiteral || node.kind() == Kind::TemplateTail;
        let mut end_len = 2;
        if is_last {
            end_len = 1;
        }
        text = text[1..text.len() - end_len].to_string();
    }

    // Newline normalization:
    // ES6 Spec 11.8.6.1 - Static Semantics of TV's and TRV's
    // <CR><LF> and <CR> LineTerminatorSequences are normalized to <LF> for both TV and TRV.
    let text = newline_normalizer_replace(&text);

    let result = f.new_string_literal(alloc_str(&text), TokenFlags::None);
    result.set_loc(node.loc());
    result
}

// taggedtemplate.go:161
fn has_invalid_escape(template: P<Node>) -> bool {
    if ast::is_no_substitution_template_literal(template) {
        return template
            .template_literal_like_data()
            .unwrap()
            .template_flags
            .intersects(TokenFlags::ContainsInvalidEscape);
    }
    let te = template.as_template_expression();
    if te
        .head
        .template_literal_like_data()
        .unwrap()
        .template_flags
        .intersects(TokenFlags::ContainsInvalidEscape)
    {
        return true;
    }
    te.template_spans.nodes().iter().any(|span| {
        span.as_template_span()
            .literal
            .template_literal_like_data()
            .unwrap()
            .template_flags
            .intersects(TokenFlags::ContainsInvalidEscape)
    })
}
