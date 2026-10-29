use rustc_hash::FxHashMap;
use tsrs_ast::*;
use tsrs_core::*;
use tsrs_scanner as scanner;

use crate::*;

//
// Lists
//

impl Printer {
    pub(crate) fn emit_list(
        &mut self,
        emit: fn(&mut Printer, P<Node>),
        parent_node: P<Node>,
        children: Option<P<NodeList>>,
        format: ListFormat,
    ) {
        let mut format = format;
        if self.should_emit_on_multiple_lines(parent_node) {
            format |= ListFormat::PreferNewLine | ListFormat::Indented;
        }

        self.emit_list_range(emit, Some(parent_node), children, format, 0, None);
    }

    pub(crate) fn emit_list_range(
        &mut self,
        emit: fn(&mut Printer, P<Node>),
        parent_node: Option<P<Node>>,
        children: Option<P<NodeList>>,
        format: ListFormat,
        start: usize,
        count: Option<usize>,
    ) {
        let is_nil = children.is_none();

        let mut length = 0;
        if let Some(children) = children {
            length = children.nodes().len();
        }
        let count = count.unwrap_or_else(|| length.saturating_sub(start));

        if is_nil && format.intersects(ListFormat::OptionalIfNil) {
            return;
        }

        let is_empty = is_nil || start >= length || count == 0;
        if is_empty && format.intersects(ListFormat::OptionalIfEmpty) {
            if let Some(on_before_emit_node_list) =
                &mut self.print_handlers.on_before_emit_node_list
            {
                on_before_emit_node_list(children);
            }
            if let Some(on_after_emit_node_list) = &mut self.print_handlers.on_after_emit_node_list
            {
                on_after_emit_node_list(children);
            }
            return;
        }

        if format.intersects(ListFormat::BracketsMask) {
            self.write_punctuation(get_opening_bracket(format));
            if is_empty && !is_nil {
                self.emit_trailing_comments(children.unwrap().pos(), CommentSeparator::Before); // Emit comments within empty lists
            }
        }

        if let Some(on_before_emit_node_list) = &mut self.print_handlers.on_before_emit_node_list {
            on_before_emit_node_list(children);
        }

        if is_empty {
            // Write a line terminator if the parent node was multi-line
            if format.intersects(ListFormat::MultiLine)
                && !(self.options.preserve_source_newlines
                    && (parent_node.is_none()
                        || self.current_source_file().is_some()
                            && range_is_on_single_line(
                                parent_node.unwrap().loc(),
                                self.current_source_file().unwrap(),
                            )))
            {
                self.write_line();
            } else if format.intersects(ListFormat::SpaceBetweenBraces)
                && !format.intersects(ListFormat::NoSpaceIfEmpty)
            {
                self.write_space();
            }
        } else {
            let children = children.unwrap();
            let end = std::cmp::min(start + count, length);

            let has_trailing_comma = self.has_trailing_comma(parent_node.unwrap(), children);
            self.emit_list_items(
                emit,
                parent_node,
                &children.nodes()[start..end],
                format,
                has_trailing_comma,
                children.loc(),
            );
        }

        if let Some(on_after_emit_node_list) = &mut self.print_handlers.on_after_emit_node_list {
            on_after_emit_node_list(children);
        }

        if format.intersects(ListFormat::BracketsMask) {
            if is_empty && !is_nil {
                self.emit_leading_comments(children.unwrap().end(), false /*elided*/); // Emit comments within empty lists
            }
            self.write_punctuation(get_closing_bracket(format));
        }
    }

    fn has_trailing_comma(&self, parent_node: P<Node>, children: P<NodeList>) -> bool {
        // NodeList.HasTrailingComma() is unreliable on transformed nodes as some nodes may have been removed. In the event
        // we believe we may need to emit a trailing comma, we must first look to the respective node list on the original
        // node first.
        if !children.has_trailing_comma() {
            return false;
        }

        let original_parent = self.emit_context.most_original(Some(parent_node)).unwrap();
        if original_parent == parent_node {
            // if this node is the original node, we can trust the result
            return true;
        }

        if original_parent.kind() != parent_node.kind() {
            // if the original node is some other kind of node, we cannot correlate the list
            return false;
        }

        // find the respective node list on the original parent
        let mut original_list = Some(children);
        match original_parent.kind() {
            Kind::ObjectLiteralExpression => original_list = Some(original_parent.property_list()),
            Kind::ArrayLiteralExpression => original_list = Some(original_parent.element_list()),
            Kind::CallExpression | Kind::NewExpression => {
                if Some(children) == parent_node.type_argument_list() {
                    original_list = original_parent.type_argument_list();
                } else if Some(children) == parent_node.argument_list() {
                    original_list = original_parent.argument_list();
                }
            }
            Kind::Constructor
            | Kind::MethodDeclaration
            | Kind::GetAccessor
            | Kind::SetAccessor
            | Kind::FunctionDeclaration
            | Kind::FunctionExpression
            | Kind::ArrowFunction
            | Kind::FunctionType
            | Kind::ConstructorType
            | Kind::CallSignature
            | Kind::ConstructSignature => {
                if Some(children) == parent_node.type_parameter_list() {
                    original_list = original_parent.type_parameter_list();
                } else if Some(children) == parent_node.parameter_list() {
                    original_list = original_parent.parameter_list();
                }
            }
            Kind::ClassDeclaration
            | Kind::ClassExpression
            | Kind::InterfaceDeclaration
            | Kind::TypeAliasDeclaration
            | Kind::JSTypeAliasDeclaration => {
                if Some(children) == parent_node.type_parameter_list() {
                    original_list = original_parent.type_parameter_list();
                }
            }
            Kind::ObjectBindingPattern | Kind::ArrayBindingPattern => {
                if children == parent_node.element_list() {
                    original_list = Some(original_parent.element_list());
                }
            }
            Kind::NamedImports | Kind::NamedExports => {
                original_list = Some(original_parent.element_list());
            }
            Kind::ImportAttributes => {
                original_list = Some(original_parent.as_import_attributes().attributes());
            }
            _ => {}
        }

        // if we have the original list, we can use it's result.
        if let Some(original_list) = original_list {
            return original_list.has_trailing_comma();
        }

        false
    }

    fn write_delimiter(&mut self, format: ListFormat) {
        let delimiter = format & ListFormat::DelimitersMask;
        if delimiter == ListFormat::None {
            // no delimiter for this format
        } else if delimiter == ListFormat::CommaDelimited {
            self.write_punctuation(",");
        } else if delimiter == ListFormat::BarDelimited {
            self.write_space();
            self.write_punctuation("|");
        } else if delimiter == ListFormat::AsteriskDelimited {
            self.write_space();
            self.write_punctuation("*");
            self.write_space();
        } else if delimiter == ListFormat::AmpersandDelimited {
            self.write_space();
            self.write_punctuation("&");
        }
    }

    // Emits a list without brackets or raising events.
    //
    // NOTE: You probably don't want to call this directly and should be using `emitList` instead.
    pub(crate) fn emit_list_items(
        &mut self,
        emit: fn(&mut Printer, P<Node>),
        parent_node: Option<P<Node>>,
        children: &[P<Node>],
        format: ListFormat,
        has_trailing_comma: bool,
        children_text_range: TextRange,
    ) {
        // Write the opening line terminator or leading whitespace.
        let may_emit_intervening_comments = !format.intersects(ListFormat::NoInterveningComments);
        let mut should_emit_intervening_comments = may_emit_intervening_comments;

        let mut leading_line_terminator_count = 0;
        if !children.is_empty() {
            leading_line_terminator_count =
                self.get_leading_line_terminator_count(parent_node, Some(children[0]), format);
        }
        if leading_line_terminator_count > 0 {
            for _ in 0..leading_line_terminator_count {
                self.write_line();
            }
            should_emit_intervening_comments = false;
        } else if format.intersects(ListFormat::SpaceBetweenBraces) {
            self.write_space();
        }

        // Increase the indent, if requested.
        if format.intersects(ListFormat::Indented) {
            self.increase_indent();
        }

        let parent_end = greatest_end(TextPos::Synthetic, &[&parent_node]);

        // Emit each child.
        let mut previous_sibling: Option<P<Node>> = None;
        let mut should_decrease_indent_after_emit = false;
        for &child in children {
            // Write the delimiter if this is not the first node.
            if format.intersects(ListFormat::AsteriskDelimited) {
                // always write JSDoc in the format "\n *"
                self.write_line();
                self.write_delimiter(format);
            } else if let Some(previous) = previous_sibling {
                // i.e
                //      function commentedParameters(
                //          /* Parameter a */
                //          a
                //          /* End of parameter a */ -> this comment isn't considered to be trailing comment of parameter "a" due to newline
                //          ,
                if format.intersects(ListFormat::DelimitersMask)
                    && previous.end() != parent_end
                    && !self.comments_disabled
                    && self.should_emit_trailing_comments(previous)
                {
                    self.emit_leading_comments(previous.end(), false /*elided*/);
                }

                self.write_delimiter(format);

                // Write either a line terminator or whitespace to separate the elements.
                let separating_line_terminator_count =
                    self.get_separating_line_terminator_count(Some(previous), Some(child), format);
                if separating_line_terminator_count > 0 {
                    // If a synthesized node in a single-line list starts on a new
                    // line, we should increase the indent.
                    if format & (ListFormat::LinesMask | ListFormat::Indented)
                        == ListFormat::SingleLine
                    {
                        self.increase_indent();
                        should_decrease_indent_after_emit = true;
                    }

                    if should_emit_intervening_comments
                        && format.intersects(ListFormat::DelimitersMask)
                        && !position_is_synthesized(child.pos())
                        && self.should_emit_leading_comments(child)
                    {
                        let comment_range = self.emit_context.comment_range(child);
                        self.emit_trailing_comments_of_position(
                            comment_range.pos(),
                            format.intersects(ListFormat::SpaceBetweenSiblings),
                            true, /*forceNoNewline*/
                        );
                    }

                    for _ in 0..separating_line_terminator_count {
                        self.write_line();
                    }

                    should_emit_intervening_comments = false;
                } else if format.intersects(ListFormat::SpaceBetweenSiblings) {
                    self.write_space();
                }
            }

            // Emit this child.
            if should_emit_intervening_comments && self.should_emit_leading_comments(child) {
                let comment_range = self.emit_context.comment_range(child);
                self.emit_trailing_comments_of_position(
                    comment_range.pos(),
                    false, /*prefixSpace*/
                    false, /*forceNoNewline*/
                );
            } else {
                should_emit_intervening_comments = may_emit_intervening_comments;
            }

            self.next_list_element_pos = child.pos();
            emit(self, child);

            if should_decrease_indent_after_emit {
                self.decrease_indent();
                should_decrease_indent_after_emit = false;
            }

            previous_sibling = Some(child);
        }

        // Write a trailing comma, if requested.
        // NOTE: Go calls shouldEmitTrailingComments(nil) for an empty list, which reads the emit flags of nil (none).
        let skip_trailing_comments = self.comments_disabled
            || previous_sibling.is_some_and(|p| !self.should_emit_trailing_comments(p));
        let emit_trailing_comma = has_trailing_comma
            && format.intersects(ListFormat::AllowTrailingComma)
            && format.intersects(ListFormat::CommaDelimited);
        if emit_trailing_comma {
            if let Some(previous) = previous_sibling
                && !skip_trailing_comments
            {
                self.emit_token(Kind::CommaToken, previous.end(), WriteKind::Punctuation, previous);
            } else {
                self.write_punctuation(",");
            }
        }

        // Emit any trailing comment of the last element in the list
        // i.e
        //       var array = [...
        //          2
        //          /* end of element 2 */
        //       ];
        if let Some(previous) = previous_sibling
            && parent_end != previous.end()
            && format.intersects(ListFormat::DelimitersMask)
            && !skip_trailing_comments
        {
            let comments_pos = if emit_trailing_comma
                && children_text_range.end().source_offset().is_some_and(|end| end > 0)
            {
                children_text_range.end()
            } else {
                previous.end()
            };
            self.emit_leading_comments(comments_pos, false /*elided*/);
        }

        // Decrease the indent, if requested.
        if format.intersects(ListFormat::Indented) {
            self.decrease_indent();
        }

        // Write the closing line terminator or closing whitespace.
        let closing_line_terminator_count = self.get_closing_line_terminator_count(
            parent_node,
            children.last().copied(),
            format,
            children_text_range,
        );
        if closing_line_terminator_count > 0 {
            for _ in 0..closing_line_terminator_count {
                self.write_line();
            }
        } else if format.intersects(ListFormat::SpaceAfterList | ListFormat::SpaceBetweenBraces) {
            self.write_space();
        }
    }
}

//
// General
//

/// Adapter that lets the printer hold the caller's writer for the duration of `Printer::write`, like Go's
/// `p.writer = writer` (a Rust field cannot hold the `&mut` borrow without a lifetime parameter).
struct BorrowedWriter(*mut (dyn EmitTextWriter + 'static));

impl BorrowedWriter {
    fn w(&mut self) -> &mut (dyn EmitTextWriter + 'static) {
        // SAFETY: `Printer::write` stores this adapter in `Printer::writer` only while it holds the `&mut` borrow the
        // pointer came from, and removes it (restoring the previous writer) before returning; no other code can reach it.
        unsafe { &mut *self.0 }
    }

    fn r(&self) -> &(dyn EmitTextWriter + 'static) {
        // SAFETY: see `w`.
        unsafe { &*self.0 }
    }
}

impl EmitTextWriter for BorrowedWriter {
    fn write(&mut self, s: &str) {
        self.w().write(s);
    }
    fn write_trailing_semicolon(&mut self, text: &str) {
        self.w().write_trailing_semicolon(text);
    }
    fn write_comment(&mut self, text: &str) {
        self.w().write_comment(text);
    }
    fn write_keyword(&mut self, text: &str) {
        self.w().write_keyword(text);
    }
    fn write_operator(&mut self, text: &str) {
        self.w().write_operator(text);
    }
    fn write_punctuation(&mut self, text: &str) {
        self.w().write_punctuation(text);
    }
    fn write_space(&mut self, text: &str) {
        self.w().write_space(text);
    }
    fn write_string_literal(&mut self, text: &str) {
        self.w().write_string_literal(text);
    }
    fn write_parameter(&mut self, text: &str) {
        self.w().write_parameter(text);
    }
    fn write_property(&mut self, text: &str) {
        self.w().write_property(text);
    }
    fn write_symbol(&mut self, text: &str, symbol: P<Symbol>) {
        self.w().write_symbol(text, symbol);
    }
    fn write_line(&mut self) {
        self.w().write_line();
    }
    fn write_line_force(&mut self, force: bool) {
        self.w().write_line_force(force);
    }
    fn increase_indent(&mut self) {
        self.w().increase_indent();
    }
    fn decrease_indent(&mut self) {
        self.w().decrease_indent();
    }
    fn clear(&mut self) {
        self.w().clear();
    }
    fn string(&self) -> String {
        self.r().string()
    }
    fn take_string(&mut self) -> String {
        self.w().take_string()
    }
    fn raw_write(&mut self, s: &str) {
        self.w().raw_write(s);
    }
    fn write_literal(&mut self, s: &str) {
        self.w().write_literal(s);
    }
    fn get_text_pos(&self) -> u32 {
        self.r().get_text_pos()
    }
    fn get_line(&self) -> u32 {
        self.r().get_line()
    }
    fn get_column(&self) -> UTF16Offset {
        self.r().get_column()
    }
    fn get_indent(&self) -> u32 {
        self.r().get_indent()
    }
    fn is_at_start_of_line(&self) -> bool {
        self.r().is_at_start_of_line()
    }
    fn has_trailing_comment(&self) -> bool {
        self.r().has_trailing_comment()
    }
    fn has_trailing_whitespace(&self) -> bool {
        self.r().has_trailing_whitespace()
    }
    fn grow(&mut self, n: usize) {
        self.w().grow(n);
    }
}

impl Printer {
    pub fn emit(&mut self, node: P<Node>, source_file: Option<P<SourceFile>>) -> String {
        // ensure a reusable writer
        if self.own_writer.is_none() {
            self.own_writer =
                Some(new_text_writer(self.options.new_line.get_new_line_character(), 0));
        }

        let mut own_writer = self.own_writer.take().unwrap();
        self.write(node, source_file, &mut *own_writer, None /*sourceMapGenerator*/);
        let text = own_writer.take_string();
        self.own_writer = Some(own_writer);
        text
    }

    pub fn emit_source_file(&mut self, source_file: P<SourceFile>) -> String {
        self.emit(source_file.as_node(), Some(source_file))
    }

    fn set_source_file(&mut self, source_file: Option<P<SourceFile>>) {
        self.set_current_source_file(source_file);
        self.unique_helper_names = None;
        self.external_helpers_module_name = None;
        if let Some(source_file) = source_file {
            if self
                .emit_context
                .emit_flags(self.emit_context.most_original(Some(source_file.as_node())).unwrap())
                .intersects(EmitFlags::ExternalHelpers)
            {
                self.unique_helper_names = Some(FxHashMap::default());
            }
            self.external_helpers_module_name =
                self.emit_context.get_external_helpers_module_name(source_file);
            self.set_source_map_source(source_file.get());
        }

        // !!!
    }

    pub fn write(
        &mut self,
        node: P<Node>,
        source_file: Option<P<SourceFile>>,
        writer: &mut (dyn EmitTextWriter + 'static),
        source_map_generator: Option<&mut SourceMapGenerator>,
    ) {
        let saved_current_source_file = self.current_source_file();
        let saved_writer = self.writer.take();
        let saved_unique_helper_names = self.unique_helper_names.take();
        let saved_source_maps_disabled = self.source_maps_disabled;
        let saved_source_map_generator = self.source_map_generator;
        let saved_source_map_source = self.source_map_source;
        let saved_source_map_source_index = self.source_map_source_index;
        let saved_source_map_line_char_cache = self.source_map_line_char_cache.take();

        self.source_maps_disabled = source_map_generator.is_none();
        self.source_map_generator =
            source_map_generator.map(std::ptr::from_mut::<SourceMapGenerator>);
        self.source_map_source = None;
        self.source_map_source_index = tsrs_sourcemap::SourceIndex::Missing;
        self.source_map_line_char_cache = None;
        self.text_state.target.set(self.options.target);

        self.set_source_file(source_file);
        let mut writer: Box<dyn EmitTextWriter> =
            Box::new(BorrowedWriter(std::ptr::from_mut::<dyn EmitTextWriter + 'static>(writer)));
        if self.options.omit_trailing_semicolon {
            writer = get_trailing_semicolon_deferring_writer(writer);
        }
        self.writer = Some(writer);
        self.writer().clear();
        if let Some(source_file) = source_file {
            self.writer().grow(source_file.text().len());
        }

        match node.kind() {
            // Pseudo-literals
            Kind::TemplateHead => self.emit_template_head(node),
            Kind::TemplateMiddle => self.emit_template_middle(node),
            Kind::TemplateTail => self.emit_template_tail(node),

            // Identifiers
            Kind::Identifier => self.emit_identifier_name(node),

            // PrivateIdentifiers
            Kind::PrivateIdentifier => self.emit_private_identifier(node),

            // Parse tree nodes
            // Names
            Kind::QualifiedName => self.emit_qualified_name(node),
            Kind::ComputedPropertyName => self.emit_computed_property_name(node),

            // Signature elements
            Kind::TypeParameter => self.emit_type_parameter(node),
            Kind::Parameter => self.emit_parameter(node),
            Kind::Decorator => self.emit_decorator(node),

            // Type members
            Kind::PropertySignature => self.emit_property_signature(node),
            Kind::PropertyDeclaration => self.emit_property_declaration(node),
            Kind::MethodSignature => self.emit_method_signature(node),
            Kind::MethodDeclaration => self.emit_method_declaration(node),
            Kind::ClassStaticBlockDeclaration => self.emit_class_static_block_declaration(node),
            Kind::Constructor => self.emit_constructor(node),
            Kind::GetAccessor => self.emit_get_accessor_declaration(node),
            Kind::SetAccessor => self.emit_set_accessor_declaration(node),
            Kind::CallSignature => self.emit_call_signature(node),
            Kind::ConstructSignature => self.emit_construct_signature(node),
            Kind::IndexSignature => self.emit_index_signature(node),

            // Binding patterns
            Kind::ObjectBindingPattern => self.emit_object_binding_pattern(node),
            Kind::ArrayBindingPattern => self.emit_array_binding_pattern(node),
            Kind::BindingElement => self.emit_binding_element(node),

            // Misc
            Kind::TemplateSpan => self.emit_template_span(node),
            Kind::SemicolonClassElement => self.emit_semicolon_class_element(node),

            // Declarations (non-statement)
            Kind::VariableDeclaration => self.emit_variable_declaration(node),
            Kind::VariableDeclarationList => self.emit_variable_declaration_list(node),
            Kind::ModuleBlock => self.emit_module_block(node),
            Kind::CaseBlock => self.emit_case_block(node),
            Kind::ImportClause => self.emit_import_clause(node),
            Kind::NamespaceImport => self.emit_namespace_import(node),
            Kind::NamespaceExport => self.emit_namespace_export(node),
            Kind::NamedImports => self.emit_named_imports(node),
            Kind::ImportSpecifier => self.emit_import_specifier(node),
            Kind::NamedExports => self.emit_named_exports(node),
            Kind::ExportSpecifier => self.emit_export_specifier(node),
            Kind::ImportAttributes => self.emit_import_attributes(node),
            Kind::ImportAttribute => self.emit_import_attribute(node),

            // Module references
            Kind::ExternalModuleReference => self.emit_external_module_reference(node),

            // JSX (non-expression)
            Kind::JsxText => self.emit_jsx_text(node),
            Kind::JsxOpeningElement => self.emit_jsx_opening_element(node),
            Kind::JsxOpeningFragment => self.emit_jsx_opening_fragment(node),
            Kind::JsxClosingElement => self.emit_jsx_closing_element(node),
            Kind::JsxClosingFragment => self.emit_jsx_closing_fragment(node),
            Kind::JsxAttribute => self.emit_jsx_attribute(node),
            Kind::JsxAttributes => self.emit_jsx_attributes(node),
            Kind::JsxSpreadAttribute => self.emit_jsx_spread_attribute(node),
            Kind::JsxExpression => self.emit_jsx_expression(node),
            Kind::JsxNamespacedName => self.emit_jsx_namespaced_name(node),

            // Clauses
            Kind::CaseClause => self.emit_case_clause(node),
            Kind::DefaultClause => self.emit_default_clause(node),
            Kind::HeritageClause => self.emit_heritage_clause(node),
            Kind::CatchClause => self.emit_catch_clause(node),

            // Property assignments
            Kind::PropertyAssignment => self.emit_property_assignment(node),
            Kind::ShorthandPropertyAssignment => self.emit_shorthand_property_assignment(node),
            Kind::SpreadAssignment => self.emit_spread_assignment(node),

            // Enum
            Kind::EnumMember => self.emit_enum_member(node),

            // Top-level nodes
            Kind::SourceFile => self.emit_source_file_(node),

            // Transformation nodes
            Kind::NotEmittedTypeElement => self.emit_not_emitted_type_element(node),

            _ => {
                if is_type_node(node) {
                    self.emit_type_node_outside_extends(node);
                } else if is_statement(node) {
                    self.emit_statement(node);
                } else if is_expression(node) {
                    self.emit_expression(node, OperatorPrecedence::LOWEST);
                } else if is_keyword_kind(node.kind()) {
                    self.emit_keyword_node(Some(node));
                } else if is_punctuation_kind(node.kind()) {
                    self.emit_punctuation_node(Some(node));
                } else if is_jsdoc_kind(node.kind()) {
                    Self::emit_jsdoc_node(node);
                } else {
                    panic!("unhandled Node: {:?}", node.kind());
                }
            }
        }

        self.set_current_source_file(saved_current_source_file);
        self.writer = saved_writer;
        self.unique_helper_names = saved_unique_helper_names;
        self.source_maps_disabled = saved_source_maps_disabled;
        self.source_map_generator = saved_source_map_generator;
        self.source_map_source = saved_source_map_source;
        self.source_map_source_index = saved_source_map_source_index;
        self.source_map_line_char_cache = saved_source_map_line_char_cache;
    }
}

//
// Comments
//

impl Printer {
    fn emit_comments_before_node(&mut self, node: P<Node>) -> Option<CommentState> {
        if !self.should_emit_comments(node) {
            return None;
        }

        let emit_flags = self.emit_context.emit_flags(node);
        let comment_range = self.emit_context.comment_range(node);
        let container_pos = self.container_pos;
        let container_end = self.container_end;
        let declaration_list_container_end = self.declaration_list_container_end;

        // Emit leading comments
        self.emit_leading_comments_of_node(node, emit_flags, comment_range);
        self.emit_leading_synthetic_comments_of_node(node, emit_flags);
        if emit_flags.intersects(EmitFlags::NoNestedComments) {
            self.comments_disabled = true;
        }

        Some(CommentState {
            emit_flags,
            comment_range,
            container_pos,
            container_end,
            declaration_list_container_end,
        })
    }

    fn emit_comments_after_node(&mut self, node: P<Node>, state: Option<CommentState>) {
        let Some(state) = state else {
            return;
        };

        let emit_flags = state.emit_flags;
        let comment_range = state.comment_range;
        let container_pos = state.container_pos;
        let container_end = state.container_end;
        let declaration_list_container_end = state.declaration_list_container_end;

        // Emit trailing comments
        if emit_flags.intersects(EmitFlags::NoNestedComments) {
            self.comments_disabled = false;
        }

        self.emit_trailing_synthetic_comments_of_node(node, emit_flags);
        self.emit_trailing_comments_of_node(
            node,
            emit_flags,
            comment_range,
            container_pos,
            container_end,
            declaration_list_container_end,
        );

        // Preserve comments from erased type annotation
        if let Some(type_node) = self.emit_context.get_type_node(node) {
            self.emit_trailing_comments_of_node(
                node,
                emit_flags,
                type_node.loc(),
                container_pos,
                container_end,
                declaration_list_container_end,
            );
        }
    }

    fn emit_comments_before_token(
        &mut self,
        _token: Kind,
        pos: TextPos,
        context_node: P<Node>,
        flags: tokenEmitFlags,
    ) -> (Option<CommentState>, TextPos) {
        let mut pos = pos;
        if flags.intersects(tokenEmitFlags::NoComments) || self.comments_disabled {
            // Still skip trivia so that the returned pos correctly identifies the token position.
            // This is needed for trailing source map positions (writeTokenText advances pos by token length).
            if let Some(current_source_file) = self.current_source_file()
                && !position_is_synthesized(pos)
            {
                pos = scanner::skip_trivia(
                    current_source_file.text(),
                    pos.expect_source("source tokens have source positions"),
                )
                .into();
            }
            return (None, pos);
        }

        let start_pos = pos;
        if !start_pos.is_synthetic()
            && let Some(current_source_file) = self.current_source_file()
        {
            pos = scanner::skip_trivia(
                current_source_file.text(),
                start_pos.expect_source("source tokens have source positions"),
            )
            .into();
        }

        let node = self.emit_context.parse_node(Some(context_node));
        let is_similar_node = node.is_some_and(|n| n.kind() == context_node.kind());
        if !is_similar_node {
            return (None, pos);
        }

        if context_node.pos() != start_pos {
            let indent_leading = flags.intersects(tokenEmitFlags::IndentLeadingComments);
            let needs_indent = indent_leading
                && self.current_source_file().is_some()
                && !positions_are_on_same_line(start_pos, pos, self.current_source_file().unwrap());
            self.increase_indent_if(needs_indent);
            self.emit_leading_comments(start_pos, false /*elided*/);
            self.decrease_indent_if(needs_indent);
        }

        (Some(CommentState::default()), pos)
    }

    fn emit_comments_after_token(
        &mut self,
        _token: Kind,
        pos: TextPos,
        context_node: P<Node>,
        state: Option<CommentState>,
    ) {
        if state.is_none() {
            return;
        }

        if context_node.end() != pos {
            let is_jsx_expr_context = context_node.kind() == Kind::JsxExpression;
            self.emit_trailing_comments(
                pos,
                if is_jsx_expr_context { CommentSeparator::None } else { CommentSeparator::Before },
            );
        }
    }

    pub(crate) fn emit_detached_comments_before_statement_list(
        &mut self,
        node: P<Node>,
        detached_range: TextRange,
    ) -> Option<CommentState> {
        if !Self::should_emit_detached_comments(node) {
            return None;
        }

        let emit_flags = self.emit_context.emit_flags(node);
        let container_pos = self.container_pos;
        let container_end = self.container_end;
        let declaration_list_container_end = self.declaration_list_container_end;
        let skip_leading_comments = position_is_synthesized(detached_range.pos())
            || emit_flags.intersects(EmitFlags::NoLeadingComments);

        if !skip_leading_comments {
            self.emit_detached_comments_and_update_comments_info(detached_range);
        }

        if emit_flags.intersects(EmitFlags::NoNestedComments) {
            self.comments_disabled = true;
        }

        Some(CommentState {
            emit_flags,
            comment_range: detached_range,
            container_pos,
            container_end,
            declaration_list_container_end,
        })
    }

    pub(crate) fn emit_detached_comments_after_statement_list(
        &mut self,
        _node: P<Node>,
        detached_range: TextRange,
        state: Option<CommentState>,
    ) {
        let Some(state) = state else {
            return;
        };

        let emit_flags = state.emit_flags;
        let skip_trailing_comments = self.comments_disabled
            || position_is_synthesized(detached_range.end())
            || emit_flags.intersects(EmitFlags::NoTrailingComments);

        if !skip_trailing_comments {
            let has_written_comment =
                self.emit_leading_comments(detached_range.end(), false /*elided*/);
            if has_written_comment && !self.writer().is_at_start_of_line() {
                self.write_line();
            }
        }
    }

    fn emit_leading_comments_of_node(
        &mut self,
        node: P<Node>,
        emit_flags: EmitFlags,
        comment_range: TextRange,
    ) {
        let pos = comment_range.pos();
        let end = comment_range.end();

        // Save current container state on the stack.
        if (!position_is_synthesized(pos) || !position_is_synthesized(end)) && pos != end {
            // We have to explicitly check that the node is JsxText because if the compilerOptions.jsx is "preserve" we will not do any transformation.
            // It is expensive to walk entire tree just to set one kind of node to have no comments.
            let skip_leading_comments = position_is_synthesized(pos)
                || emit_flags.intersects(EmitFlags::NoLeadingComments)
                || node.kind() == Kind::JsxText;
            let skip_trailing_comments = position_is_synthesized(end)
                || emit_flags.intersects(EmitFlags::NoTrailingComments)
                || node.kind() == Kind::JsxText;

            // Emit leading comments if the position is not synthesized and the node
            // has not opted out from emitting leading comments.
            if !skip_leading_comments {
                self.emit_leading_comments(
                    pos,
                    node.kind() == Kind::NotEmittedStatement, /*elided*/
                );
            }

            if !skip_leading_comments
                || (!pos.is_synthetic() && emit_flags.intersects(EmitFlags::NoLeadingComments))
            {
                // Advance the container position if comments get emitted or if they've been disabled explicitly using NoLeadingComments.
                self.container_pos = pos;
            }

            if !skip_trailing_comments
                || (!end.is_synthetic() && emit_flags.intersects(EmitFlags::NoTrailingComments))
            {
                // Advance the container end if comments get emitted or if they've been disabled explicitly using NoTrailingComments.
                self.container_end = end;

                // To avoid invalid comment emit in a down-level binding pattern, we
                // keep track of the last declaration list container's end
                if node.kind() == Kind::VariableDeclarationList {
                    self.declaration_list_container_end = end;
                }
            }
        }
    }

    fn emit_trailing_comments_of_node(
        &mut self,
        node: P<Node>,
        emit_flags: EmitFlags,
        comment_range: TextRange,
        container_pos: TextPos,
        container_end: TextPos,
        declaration_list_container_end: TextPos,
    ) {
        let pos = comment_range.pos();
        let end = comment_range.end();
        let skip_trailing_comments = end.is_synthetic()
            || emit_flags.intersects(EmitFlags::NoTrailingComments)
            || node.kind() == Kind::JsxText;
        if (!position_is_synthesized(pos) || !position_is_synthesized(end)) && pos != end {
            // Restore previous container state.
            self.container_pos = container_pos;
            self.container_end = container_end;
            self.declaration_list_container_end = declaration_list_container_end;

            // Emit trailing comments if the position is not synthesized and the node
            // has not opted out from emitting leading comments and is an emitted node.
            if !skip_trailing_comments && node.kind() != Kind::NotEmittedStatement {
                self.emit_trailing_comments(end, CommentSeparator::Before);
            }
        }
    }

    fn emit_leading_synthetic_comments_of_node(&mut self, node: P<Node>, emit_flags: EmitFlags) {
        if emit_flags.intersects(EmitFlags::NoLeadingComments) {
            return;
        }
        let synth = self.emit_context.get_synthetic_leading_comments(node);
        for c in &synth {
            self.emit_leading_synthesized_comment(c);
        }
    }

    fn emit_leading_synthesized_comment(&mut self, comment: &SynthesizedComment) {
        if comment.has_leading_new_line || comment.kind == Kind::SingleLineCommentTrivia {
            self.writer().write_line();
        }
        self.write_synthesized_comment(comment);
        if comment.has_trailing_new_line || comment.kind == Kind::SingleLineCommentTrivia {
            self.writer().write_line();
        } else {
            self.writer().write_space(" ");
        }
    }

    fn emit_trailing_synthetic_comments_of_node(&mut self, node: P<Node>, emit_flags: EmitFlags) {
        if emit_flags.intersects(EmitFlags::NoTrailingComments) {
            return;
        }
        let synth = self.emit_context.get_synthetic_trailing_comments(node);
        for c in &synth {
            self.emit_trailing_synthesized_comment(c);
        }
    }

    fn emit_trailing_synthesized_comment(&mut self, comment: &SynthesizedComment) {
        if !self.writer().is_at_start_of_line() {
            self.writer().write_space(" ");
        }
        self.write_synthesized_comment(comment);
        if comment.has_trailing_new_line {
            self.writer().write_line();
        }
    }
}

fn format_synthesized_comment(comment: &SynthesizedComment) -> String {
    if comment.kind == Kind::MultiLineCommentTrivia {
        return format!("/*{}*/", comment.text);
    }
    format!("//{}", comment.text)
}

impl Printer {
    fn write_synthesized_comment(&mut self, comment: &SynthesizedComment) {
        let text = format_synthesized_comment(comment);
        let mut line_map: Vec<u32> = Vec::new();
        if comment.kind == Kind::MultiLineCommentTrivia {
            line_map = compute_ecma_line_starts(&text);
        }
        self.write_comment_range_worker(
            &text,
            &line_map,
            comment.kind,
            TextRange::new(0, text.len() as u32),
        );
    }

    fn emit_leading_comments(&mut self, pos: TextPos, elided: bool) -> bool {
        // Emit the leading comments only if the container's pos doesn't match because the container should take care of emitting these comments
        let Some(current_source_file) = self.current_source_file() else {
            return false;
        };
        if self.comments_disabled || position_is_synthesized(pos) || pos == self.container_pos {
            return false;
        }

        let mut triple_slash = Tristate::Unknown;
        if !elided {
            if pos.source_offset() == Some(0) && current_source_file.is_declaration_file() {
                triple_slash = Tristate::False;
            }
        } else if pos.source_offset() == Some(0) {
            // If the node will not be emitted in JS, remove all the comments(normal, pinned and ///) associated with the node,
            // unless it is a triple slash comment at the top of the file.
            // For Example:
            //      /// <reference-path ...>
            //      declare var x;
            //      /// <reference-path ...>
            //      interface F {}
            //  The first /// will NOT be removed while the second one will be removed even though both node will not be emitted
            triple_slash = Tristate::True;
        } else {
            return false;
        }

        let mut pos = pos;
        // skip detached comments
        if let Some(info) = self.detached_comments_info.pop_if(|info| info.node_pos == pos) {
            pos = info.detached_comment_end_pos;
        }

        let mut comments: Vec<CommentRange> = Vec::new();
        comments.extend(
            scanner::get_leading_comment_ranges(
                current_source_file.text(),
                pos.expect_source("source comments have source positions"),
            )
            .filter(|&comment| {
                self.should_write_comment(comment)
                    && self.should_emit_comment_if_triple_slash(comment, triple_slash)
            }),
        );

        if !comments.is_empty()
            && self.should_emit_new_line_before_leading_comment_of_position(
                pos.expect_source("source comments have source positions"),
                comments[0].pos(),
            )
        {
            self.write_line();
        }

        // Leading comments are emitted as /*leading comment1*/space/*leading comment*/space
        self.emit_comments(&comments, CommentSeparator::After)
    }

    fn should_emit_comment_if_triple_slash(
        &self,
        comment: CommentRange,
        triple_slash: Tristate,
    ) -> bool {
        match triple_slash {
            Tristate::True => self.is_triple_slash_comment(comment),
            Tristate::False => !self.is_triple_slash_comment(comment),
            Tristate::Unknown => true,
        }
    }

    fn should_emit_new_line_before_leading_comment_of_position(
        &self,
        pos: u32,
        comment_pos: u32,
    ) -> bool {
        // If the leading comments start on different line than the start of node, write new line
        let Some(current_source_file) = self.current_source_file() else {
            return false;
        };
        pos != comment_pos
            && scanner::compute_line_of_position(current_source_file.ecma_line_map(), pos)
                != scanner::compute_line_of_position(
                    current_source_file.ecma_line_map(),
                    comment_pos,
                )
    }

    pub(crate) fn emit_leading_comments_of_position(&mut self, pos: TextPos) {
        if self.comments_disabled || pos.is_synthetic() {
            return;
        }

        self.emit_leading_comments(pos, false /*elided*/);
    }

    pub(crate) fn emit_trailing_comments(
        &mut self,
        pos: TextPos,
        comment_separator: CommentSeparator,
    ) {
        if self.comments_disabled {
            return;
        }
        // Emit the trailing comments only if the container's end doesn't match because the container should take care of emitting these comments
        let Some(current_source_file) = self.current_source_file() else {
            return;
        };
        if self.comments_disabled
            || pos.is_synthetic()
            || !self.container_end.is_synthetic()
                && (pos == self.container_end || pos == self.declaration_list_container_end)
        {
            return;
        }

        let mut comments: Vec<CommentRange> = Vec::new();
        comments.extend(
            scanner::get_trailing_comment_ranges(
                current_source_file.text(),
                pos.expect_source("source comments have source positions"),
            )
            .filter(|&comment| self.should_write_comment(comment)),
        );

        // trailing comments are normally emitted as space/*trailing comment1*/space/*trailing comment2*/
        self.emit_comments(&comments, comment_separator);
    }

    pub(crate) fn emit_trailing_comments_of_position(
        &mut self,
        pos: TextPos,
        prefix_space: bool,
        force_no_newline: bool,
    ) {
        let Some(current_source_file) = self.current_source_file() else {
            return;
        };
        if self.comments_disabled || pos.is_synthetic() {
            return;
        }
        if !self.container_end.is_synthetic()
            && (pos == self.container_end || pos == self.declaration_list_container_end)
        {
            return;
        }

        let comments: Vec<CommentRange> = scanner::get_trailing_comment_ranges(
            current_source_file.text(),
            pos.expect_source("source comments have source positions"),
        )
        .collect();
        if comments.is_empty() {
            return;
        }

        for comment in comments {
            if prefix_space {
                if !self.should_write_comment(comment) {
                    continue;
                }
                if !self.writer().is_at_start_of_line() {
                    self.write_space();
                }
                self.emit_comment(comment);
                if comment.has_trailing_new_line {
                    self.write_line();
                }
                continue;
            }

            self.emit_comment(comment);
            if force_no_newline {
                if comment.kind == Kind::SingleLineCommentTrivia {
                    self.write_line();
                }
            } else if comment.has_trailing_new_line {
                self.write_line();
            } else {
                self.write_space();
            }
        }
    }

    fn emit_detached_comments_and_update_comments_info(&mut self, text_range: TextRange) {
        if self.current_source_file().is_none() {
            return;
        }
        if let Some(current_detached_comment_info) = self.emit_detached_comments(text_range) {
            self.detached_comments_info.push(current_detached_comment_info);
        }
    }

    fn emit_detached_comments(&mut self, text_range: TextRange) -> Option<DetachedCommentsInfo> {
        let current_source_file = self.current_source_file()?;

        let text = current_source_file.text();
        let line_map = current_source_file.ecma_line_map();

        let mut leading_comments: Vec<CommentRange> = Vec::new();
        if self.comments_disabled {
            // removeComments is true, only reserve pinned comment at the top of file
            // For example:
            //      /*! Pinned Comment */
            //
            //      var x = 10;
            if text_range.pos().source_offset() == Some(0) {
                leading_comments.extend(
                    scanner::get_leading_comment_ranges(
                        text,
                        text_range
                            .pos()
                            .expect_source("detached comment ranges have source positions"),
                    )
                    .filter(|&comment| is_pinned_comment(text, comment)),
                );
            }
        } else {
            // removeComments is false, just get detached as normal and bypass the process to filter comment
            leading_comments = scanner::get_leading_comment_ranges(
                text,
                text_range.pos().expect_source("detached comment ranges have source positions"),
            )
            .collect();
        }

        let mut result = None;
        if !leading_comments.is_empty() {
            let mut detached_comments: Vec<CommentRange> = Vec::new();
            let mut last_comment: Option<CommentRange> = None;
            for (i, comment) in leading_comments.iter().enumerate() {
                if i > 0 {
                    let last_comment_line =
                        scanner::compute_line_of_position(line_map, last_comment.unwrap().end());
                    let comment_line = scanner::compute_line_of_position(line_map, comment.pos());

                    if comment_line >= last_comment_line + 2 {
                        // There was a blank line between the last comment and this comment.  This
                        // comment is not part of the copyright comments.  Return what we have so
                        // far.
                        break;
                    }
                }

                detached_comments.push(*comment);
                last_comment = Some(*comment);
            }

            if !detached_comments.is_empty() {
                // All comments look like they could have been part of the copyright header.  Make
                // sure there is at least one blank line between it and the node.  If not, it's not
                // a copyright header.
                let last_comment_line = scanner::compute_line_of_position(
                    line_map,
                    detached_comments.last().unwrap().end(),
                );
                let node_line = scanner::compute_line_of_position(
                    line_map,
                    scanner::skip_trivia(
                        text,
                        text_range
                            .pos()
                            .expect_source("detached comment ranges have source positions"),
                    ),
                );
                if node_line >= last_comment_line + 2 {
                    // Valid detachedComments

                    // Filter to only comments that should be written (e.g., JSDoc-style in declaration emit)
                    let mut comments_to_emit: Vec<CommentRange> = Vec::new();
                    comments_to_emit.extend(detached_comments.iter().filter_map(|comment| {
                        if self.should_write_comment(*comment) { Some(*comment) } else { None }
                    }));

                    if !comments_to_emit.is_empty() {
                        if self.should_emit_new_line_before_leading_comment_of_position(
                            text_range
                                .pos()
                                .expect_source("detached comment ranges have source positions"),
                            comments_to_emit[0].pos(),
                        ) {
                            self.write_line();
                        }

                        self.emit_comments(&comments_to_emit, CommentSeparator::After);
                    }
                    result = Some(DetachedCommentsInfo {
                        node_pos: text_range.pos(),
                        detached_comment_end_pos: detached_comments.last().unwrap().end().into(),
                    });
                }
            }
        }
        result
    }
}

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum CommentSeparator {
    None,
    Before,
    After,
}

impl Printer {
    fn emit_comments(
        &mut self,
        comments: &[CommentRange],
        comment_separator: CommentSeparator,
    ) -> bool {
        let mut intervening_separator = false;
        if comments.is_empty() {
            return false;
        }

        if comment_separator == CommentSeparator::Before {
            self.write_space();
        }

        for &comment in comments {
            if intervening_separator {
                self.write_space();
                intervening_separator = false;
            }

            self.emit_comment(comment);

            if comment.kind == Kind::SingleLineCommentTrivia
                || comment.has_trailing_new_line && comment_separator != CommentSeparator::None
            {
                self.write_line();
            } else {
                intervening_separator = comment_separator != CommentSeparator::None;
            }
        }

        if intervening_separator && comment_separator == CommentSeparator::After {
            self.write_space();
        }

        true
    }

    fn emit_comment(&mut self, comment: CommentRange) {
        self.emit_pos(comment.pos().into());
        self.write_comment_range(comment);
        self.emit_pos(comment.end().into());
    }

    fn is_triple_slash_comment(&self, comment: CommentRange) -> bool {
        self.current_source_file()
            .is_some_and(|f| is_recognized_triple_slash_comment(f.text(), comment))
    }
}

//
// Source Maps
//

impl Printer {
    // The generator borrowed by `write` (Go keeps the `*sourcemap.Generator` in a field for the duration of the call).
    fn source_map_generator(&mut self) -> &mut SourceMapGenerator {
        // SAFETY: `source_map_generator` is only set by `write`, from a `&mut SourceMapGenerator` that outlives the
        // call, and is restored before `write` returns; the printer is the only user of that borrow meanwhile.
        unsafe { &mut *self.source_map_generator.unwrap() }
    }

    // printer.go:5812
    fn set_source_map_source(&mut self, source: SourceMapSource) {
        if self.source_maps_disabled {
            return;
        }

        self.source_map_source = Some(source);
        self.source_map_line_char_cache = Some(new_line_character_cache(source));
        if same_source_map_source(self.most_recent_source_map_source, Some(source)) {
            self.source_map_source_index = self.most_recent_source_map_source_index;
            return;
        }

        self.source_map_source_is_json =
            tspath::file_extension_is(source.file_name(), tspath::EXTENSION_JSON);
        if self.source_map_source_is_json {
            return;
        }

        self.source_map_source_index = self.source_map_generator().add_source(source.file_name());
        if self.options.inline_sources {
            let index = self.source_map_source_index;
            if let Err(err) = self.source_map_generator().set_source_content(index, source.text()) {
                panic!("{}", err);
            }
        }

        self.most_recent_source_map_source = Some(source);
        self.most_recent_source_map_source_index = self.source_map_source_index;
    }

    // printer.go:5840
    fn emit_pos(&mut self, pos: TextPos) {
        if self.source_maps_disabled
            || self.source_map_source.is_none()
            || self.source_map_generator.is_none()
            || self.source_map_source_is_json
            || position_is_synthesized(pos)
        {
            return;
        }

        let mut pos = pos.expect_source("source-map positions must belong to source text");
        let source = self.source_map_source.unwrap();
        let mut source_index = self.source_map_source_index;
        // Go shares the cache through a pointer; `mapped_cache` holds the mapped source's fresh cache instead.
        let mut mapped_cache: Option<LineCharacterCache> = None;
        if let Some(map_source_position) = &self.print_handlers.map_source_position {
            let Some((mapped_source, mapped_pos)) = map_source_position(source, pos) else {
                let (line, column) = (self.writer().get_line(), self.writer().get_column());
                if let Err(err) = self.source_map_generator().add_generated_mapping(line, column) {
                    panic!("{}", err);
                }
                return;
            };
            pos = mapped_pos;
            if !same_source_map_source(Some(mapped_source), Some(source)) {
                let saved_source = self.source_map_source;
                let saved_source_index = self.source_map_source_index;
                let saved_source_is_json = self.source_map_source_is_json;
                let saved_line_char_cache = self.source_map_line_char_cache.take();
                self.set_source_map_source(mapped_source);
                source_index = self.source_map_source_index;
                mapped_cache = self.source_map_line_char_cache.take();
                self.source_map_source = saved_source;
                self.source_map_source_index = saved_source_index;
                self.source_map_source_is_json = saved_source_is_json;
                self.source_map_line_char_cache = saved_line_char_cache;
            }
        }

        let (source_line, source_character) = match &mut mapped_cache {
            Some(cache) => cache.get_line_and_character(pos),
            None => self.source_map_line_char_cache.as_mut().unwrap().get_line_and_character(pos),
        };
        let (line, column) = (self.writer().get_line(), self.writer().get_column());
        if let Err(err) = self.source_map_generator().add_source_mapping(
            line,
            column,
            source_index,
            source_line,
            source_character,
        ) {
            panic!("{}", err);
        }
    }

    // TODO: Support emitting nameIndex for source maps (Go emitPosName is commented out)

    // printer.go:5904
    fn emit_source_pos(&mut self, source: Option<SourceMapSource>, pos: TextPos) {
        if same_source_map_source(source, self.source_map_source) {
            self.emit_pos(pos);
        } else {
            let saved_source_map_source = self.source_map_source;
            let saved_source_map_source_index = self.source_map_source_index;
            let saved_source_map_line_char_cache = self.source_map_line_char_cache.take();
            self.set_source_map_source(source.unwrap());
            self.emit_pos(pos);
            self.source_map_source = saved_source_map_source;
            self.source_map_source_index = saved_source_map_source_index;
            self.source_map_line_char_cache = saved_source_map_line_char_cache;
        }
    }

    // TODO: Support emitting nameIndex for source maps (Go emitSourcePosName is commented out)

    fn emit_source_maps_before_node(&mut self, node: P<Node>) -> Option<SourceMapState> {
        if !self.should_emit_source_maps(node) {
            return None;
        }

        let emit_flags = self.emit_context.emit_flags(node);
        let loc = self.emit_context.source_map_range(node);

        if !is_not_emitted_statement(node)
            && !emit_flags.intersects(EmitFlags::NoLeadingSourceMap)
            && self.current_source_file().is_some()
            && !position_is_synthesized(loc.pos())
        {
            let pos = scanner::skip_trivia(
                self.current_source_file().unwrap().text(),
                loc.pos().expect_source("source-map ranges have source positions"),
            );
            self.emit_source_pos(self.source_map_source, pos.into());
        }

        if emit_flags.intersects(EmitFlags::NoNestedSourceMaps) {
            self.source_maps_disabled = true;
        }

        Some(SourceMapState {
            emit_flags,
            source_map_range: loc,
            has_token_source_map_range: false,
        })
    }

    fn emit_source_maps_after_node(
        &mut self,
        node: P<Node>,
        previous_state: Option<SourceMapState>,
    ) {
        let Some(previous_state) = previous_state else {
            return;
        };

        let emit_flags = previous_state.emit_flags;
        let loc = previous_state.source_map_range;

        if emit_flags.intersects(EmitFlags::NoNestedSourceMaps) {
            self.source_maps_disabled = false;
        }

        if !is_not_emitted_statement(node)
            && !emit_flags.intersects(EmitFlags::NoTrailingSourceMap)
            && !position_is_synthesized(loc.end())
        {
            self.emit_source_pos(self.source_map_source, loc.end());
        }
    }

    fn emit_source_maps_before_token(
        &mut self,
        token: Kind,
        pos: TextPos,
        context_node: P<Node>,
        flags: tokenEmitFlags,
    ) -> Option<SourceMapState> {
        if !self.should_emit_token_source_maps(token, pos, context_node, flags) {
            return None;
        }

        let mut pos = pos;
        let emit_flags = self.emit_context.emit_flags(context_node);
        let loc = self.emit_context.token_source_map_range(context_node, token);
        let has_loc = loc.is_some();
        let loc = loc.unwrap_or_default();
        if has_loc {
            pos = loc.pos();
        }
        if !pos.is_synthetic()
            && let Some(current_source_file) = self.current_source_file()
        {
            pos = scanner::skip_trivia(
                current_source_file.text(),
                pos.expect_source("source tokens have source positions"),
            )
            .into();
        }
        if !emit_flags.intersects(EmitFlags::NoTokenLeadingSourceMaps) && !pos.is_synthetic() {
            self.emit_source_pos(self.source_map_source, pos);
        }

        Some(SourceMapState {
            emit_flags,
            source_map_range: loc,
            has_token_source_map_range: has_loc,
        })
    }

    fn emit_source_maps_after_token(
        &mut self,
        _token: Kind,
        pos: TextPos,
        _context_node: P<Node>,
        previous_state: Option<SourceMapState>,
    ) {
        let Some(previous_state) = previous_state else {
            return;
        };

        let mut pos = pos;
        let emit_flags = previous_state.emit_flags;
        let loc = previous_state.source_map_range;
        let has_loc = previous_state.has_token_source_map_range;
        if !emit_flags.intersects(EmitFlags::NoTokenTrailingSourceMaps) {
            if has_loc {
                pos = loc.end();
            }
            if !pos.is_synthetic() {
                self.emit_source_pos(self.source_map_source, pos);
            }
        }
    }
}

//
// Name Generation
//

impl Printer {
    fn should_reuse_temp_variable_scope(&self, node: Option<P<Node>>) -> bool {
        node.is_some_and(|node| {
            self.emit_context.emit_flags(node).intersects(EmitFlags::ReuseTempVariableScope)
        })
    }

    pub(crate) fn push_name_generation_scope(&mut self, node: Option<P<Node>>) {
        let reuse = self.should_reuse_temp_variable_scope(node);
        self.name_generator.push_scope(reuse);
    }

    pub(crate) fn pop_name_generation_scope(&mut self, node: Option<P<Node>>) {
        let reuse = self.should_reuse_temp_variable_scope(node);
        self.name_generator.pop_scope(reuse);
    }

    pub(crate) fn generate_all_names(&mut self, nodes: Option<P<NodeList>>) {
        let Some(nodes) = nodes else {
            return;
        };
        for node in nodes.nodes() {
            self.generate_names(Some(*node));
        }
    }

    pub(crate) fn generate_names(&mut self, node: Option<P<Node>>) {
        let Some(node) = node else {
            return;
        };

        match node.kind() {
            Kind::Block | Kind::CaseClause | Kind::DefaultClause => {
                self.generate_all_names(node.statement_list());
            }
            Kind::LabeledStatement
            | Kind::WithStatement
            | Kind::DoStatement
            | Kind::WhileStatement => self.generate_names(Some(node.statement())),
            Kind::IfStatement => {
                self.generate_names(Some(node.as_if_statement().then_statement()));
                self.generate_names(node.as_if_statement().else_statement());
            }
            Kind::ForStatement | Kind::ForOfStatement | Kind::ForInStatement => {
                self.generate_names(node.initializer());
                self.generate_names(Some(node.statement()));
            }
            Kind::SwitchStatement => {
                self.generate_names(Some(node.as_switch_statement().case_block()));
            }
            Kind::CaseBlock => self.generate_all_names(Some(node.as_case_block().clauses())),
            Kind::TryStatement => {
                self.generate_names(Some(node.as_try_statement().try_block()));
                self.generate_names(node.as_try_statement().catch_clause());
                self.generate_names(node.as_try_statement().finally_block());
            }
            Kind::CatchClause => {
                self.generate_names(node.as_catch_clause().variable_declaration());
                self.generate_names(Some(node.as_catch_clause().block()));
            }
            Kind::VariableStatement => {
                self.generate_names(Some(node.as_variable_statement().declaration_list()));
            }
            Kind::VariableDeclarationList => {
                self.generate_all_names(Some(node.as_variable_declaration_list().declarations()));
            }
            Kind::VariableDeclaration
            | Kind::Parameter
            | Kind::BindingElement
            | Kind::ClassDeclaration => self.generate_name_if_needed(node.name()),
            Kind::FunctionDeclaration => {
                self.generate_name_if_needed(node.name());
                if self.should_reuse_temp_variable_scope(Some(node)) {
                    self.generate_all_names(node.as_function_declaration().parameters());
                    self.generate_names(node.as_function_declaration().body());
                }
            }
            Kind::ObjectBindingPattern | Kind::ArrayBindingPattern => {
                self.generate_all_names(Some(node.element_list()));
            }
            Kind::ImportDeclaration | Kind::JSImportDeclaration => {
                self.generate_names(node.as_import_declaration().import_clause());
            }
            Kind::ImportClause => {
                self.generate_name_if_needed(node.as_import_clause().name());
                self.generate_names(node.as_import_clause().named_bindings());
            }
            Kind::NamespaceImport | Kind::NamespaceExport => {
                self.generate_name_if_needed(node.name());
            }
            Kind::NamedImports => self.generate_all_names(Some(node.element_list())),
            Kind::ImportSpecifier => {
                let n = node.as_import_specifier();
                if let Some(property_name) = n.property_name() {
                    self.generate_name_if_needed(Some(property_name));
                } else {
                    self.generate_name_if_needed(Some(n.name()));
                }
            }
            _ => {}
        }
    }

    pub(crate) fn generate_all_member_names(&mut self, nodes: Option<P<NodeList>>) {
        let Some(nodes) = nodes else {
            return;
        };
        for node in nodes.nodes() {
            self.generate_member_names(Some(*node));
        }
    }

    fn generate_member_names(&mut self, node: Option<P<Node>>) {
        let Some(node) = node else {
            return;
        };
        match node.kind() {
            Kind::PropertyAssignment
            | Kind::ShorthandPropertyAssignment
            | Kind::PropertyDeclaration
            | Kind::PropertySignature
            | Kind::MethodDeclaration
            | Kind::MethodSignature
            | Kind::GetAccessor
            | Kind::SetAccessor => self.generate_name_if_needed(node.name()),
            _ => {}
        }
    }

    pub(crate) fn generate_name_if_needed(&mut self, name: Option<P<Node>>) {
        if let Some(name) = name {
            if is_member_name(name) {
                self.generate_name(name);
            } else if is_binding_pattern(name) {
                self.generate_names(Some(name));
            }
        }
    }

    // Generate the text for a generated identifier or private identifier
    pub(crate) fn generate_name(&mut self, name: P<Node>) {
        let _ = self.name_generator.generate_name(name);
    }
}

//
// Scoped operations
//

impl Printer {
    pub(crate) fn enter_node(&mut self, node: P<Node>) -> PrinterState {
        let mut state = PrinterState::default();

        if let Some(on_before_emit_node) = &mut self.print_handlers.on_before_emit_node {
            on_before_emit_node(Some(node));
        }

        state.comment_state = self.emit_comments_before_node(node);
        state.source_map_state = self.emit_source_maps_before_node(node);
        state
    }

    pub(crate) fn exit_node(&mut self, node: P<Node>, previous_state: PrinterState) {
        self.emit_source_maps_after_node(node, previous_state.source_map_state);
        self.emit_comments_after_node(node, previous_state.comment_state);

        if let Some(on_after_emit_node) = &mut self.print_handlers.on_after_emit_node {
            on_after_emit_node(Some(node));
        }
    }

    pub(crate) fn enter_token_node(
        &mut self,
        node: P<Node>,
        flags: tokenEmitFlags,
    ) -> PrinterState {
        let mut state = PrinterState::default();

        if let Some(on_before_emit_token) = &mut self.print_handlers.on_before_emit_token {
            on_before_emit_token(Some(node));
        }

        if !flags.intersects(tokenEmitFlags::NoComments) {
            state.comment_state = self.emit_comments_before_node(node);
        }
        if !flags.intersects(tokenEmitFlags::NoSourceMaps) {
            state.source_map_state = self.emit_source_maps_before_node(node);
        }
        state
    }

    pub(crate) fn exit_token_node(&mut self, node: P<Node>, previous_state: PrinterState) {
        self.emit_source_maps_after_node(node, previous_state.source_map_state);
        self.emit_comments_after_node(node, previous_state.comment_state);

        if let Some(on_after_emit_token) = &mut self.print_handlers.on_after_emit_token {
            on_after_emit_token(Some(node));
        }
    }
}

bitflags::bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    #[expect(non_camel_case_types, reason = "Keep the upstream bitflags type name.")]
    pub(crate) struct tokenEmitFlags: u32 {
        const NoComments = 1 << 0;
        const IndentLeadingComments = 1 << 1;
        const NoSourceMaps = 1 << 2;

        const None = 0;
    }
}

impl Printer {
    pub(crate) fn enter_token(
        &mut self,
        token: Kind,
        pos: TextPos,
        context_node: P<Node>,
        flags: tokenEmitFlags,
    ) -> (PrinterState, TextPos) {
        let mut state = PrinterState::default();
        let (comment_state, pos) = self.emit_comments_before_token(token, pos, context_node, flags);
        state.comment_state = comment_state;
        state.source_map_state =
            self.emit_source_maps_before_token(token, pos, context_node, flags);
        (state, pos)
    }

    pub(crate) fn exit_token(
        &mut self,
        token: Kind,
        pos: TextPos,
        context_node: P<Node>,
        previous_state: PrinterState,
    ) {
        self.emit_source_maps_after_token(
            token,
            pos,
            context_node,
            previous_state.source_map_state,
        );
        self.emit_comments_after_token(token, pos, context_node, previous_state.comment_state);
    }
}

bitflags::bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct ListFormat: u32 {
        const None = 0;

        // Line separators
        const SingleLine = 0; // Prints the list on a single line (default).
        const MultiLine = 1 << 0; // Prints the list on multiple lines.
        const PreserveLines = 1 << 1; // Prints the list using line preservation if possible.
        const LinesMask = Self::SingleLine.bits() | Self::MultiLine.bits() | Self::PreserveLines.bits();

        // Delimiters
        const NotDelimited = 0; // There is no delimiter between list items (default).
        const BarDelimited = 1 << 2; // Each list item is space-and-bar (" |") delimited.
        const AmpersandDelimited = 1 << 3; // Each list item is space-and-ampersand (" &") delimited.
        const CommaDelimited = 1 << 4; // Each list item is comma (",") delimited.
        const AsteriskDelimited = 1 << 5; // Each list item is asterisk ("\n *") delimited, used with JSDoc.
        const DelimitersMask = Self::BarDelimited.bits() | Self::AmpersandDelimited.bits() | Self::CommaDelimited.bits() | Self::AsteriskDelimited.bits();

        const AllowTrailingComma = 1 << 6; // Write a trailing comma (",") if present.

        // Whitespace
        const Indented = 1 << 7; // The list should be indented.
        const SpaceBetweenBraces = 1 << 8; // Inserts a space after the opening brace and before the closing brace.
        const SpaceBetweenSiblings = 1 << 9; // Inserts a space between each sibling node.

        // Brackets/Braces
        const Braces = 1 << 10; // The list is surrounded by "{" and "}".
        const Parenthesis = 1 << 11; // The list is surrounded by "(" and ")".
        const AngleBrackets = 1 << 12; // The list is surrounded by "<" and ">".
        const SquareBrackets = 1 << 13; // The list is surrounded by "[" and "]".
        const BracketsMask = Self::Braces.bits() | Self::Parenthesis.bits() | Self::AngleBrackets.bits() | Self::SquareBrackets.bits();

        const OptionalIfNil = 1 << 14; // Do not emit brackets if the list is nil.
        const OptionalIfEmpty = 1 << 15; // Do not emit brackets if the list is empty.
        const Optional = Self::OptionalIfNil.bits() | Self::OptionalIfEmpty.bits();

        // Other
        const PreferNewLine = 1 << 16; // Prefer adding a LineTerminator between synthesized nodes.
        const NoTrailingNewLine = 1 << 17; // Do not emit a trailing NewLine for a MultiLine list.
        const NoInterveningComments = 1 << 18; // Do not emit comments between each node
        const NoSpaceIfEmpty = 1 << 19; // If the literal is empty, do not add spaces between braces.
        const SingleElement = 1 << 20;
        const SpaceAfterList = 1 << 21; // Add space after list

        // Precomputed Formats
        const Modifiers = Self::SingleLine.bits() | Self::SpaceBetweenSiblings.bits() | Self::NoInterveningComments.bits() | Self::SpaceAfterList.bits();
        const HeritageClauses = Self::SingleLine.bits() | Self::SpaceBetweenSiblings.bits();
        const SingleLineTypeLiteralMembers = Self::SingleLine.bits() | Self::SpaceBetweenBraces.bits() | Self::SpaceBetweenSiblings.bits();
        const MultiLineTypeLiteralMembers = Self::MultiLine.bits() | Self::Indented.bits() | Self::OptionalIfEmpty.bits();

        const SingleLineTupleTypeElements = Self::CommaDelimited.bits() | Self::SpaceBetweenSiblings.bits() | Self::SingleLine.bits();
        const MultiLineTupleTypeElements = Self::CommaDelimited.bits() | Self::Indented.bits() | Self::SpaceBetweenSiblings.bits() | Self::MultiLine.bits();
        const UnionTypeConstituents = Self::BarDelimited.bits() | Self::SpaceBetweenSiblings.bits() | Self::SingleLine.bits();
        const IntersectionTypeConstituents = Self::AmpersandDelimited.bits() | Self::SpaceBetweenSiblings.bits() | Self::SingleLine.bits();
        const ObjectBindingPatternElements = Self::SingleLine.bits() | Self::AllowTrailingComma.bits() | Self::SpaceBetweenBraces.bits() | Self::CommaDelimited.bits() | Self::SpaceBetweenSiblings.bits() | Self::NoSpaceIfEmpty.bits();
        const ArrayBindingPatternElements = Self::SingleLine.bits() | Self::AllowTrailingComma.bits() | Self::CommaDelimited.bits() | Self::SpaceBetweenSiblings.bits() | Self::NoSpaceIfEmpty.bits();
        const ObjectLiteralExpressionProperties = Self::PreserveLines.bits() | Self::CommaDelimited.bits() | Self::SpaceBetweenSiblings.bits() | Self::SpaceBetweenBraces.bits() | Self::Indented.bits() | Self::Braces.bits() | Self::NoSpaceIfEmpty.bits();
        const ImportAttributes = Self::PreserveLines.bits() | Self::CommaDelimited.bits() | Self::SpaceBetweenSiblings.bits() | Self::SpaceBetweenBraces.bits() | Self::Indented.bits() | Self::Braces.bits() | Self::NoSpaceIfEmpty.bits();
        const ArrayLiteralExpressionElements = Self::PreserveLines.bits() | Self::CommaDelimited.bits() | Self::SpaceBetweenSiblings.bits() | Self::AllowTrailingComma.bits() | Self::Indented.bits() | Self::SquareBrackets.bits();
        const CommaListElements = Self::CommaDelimited.bits() | Self::SpaceBetweenSiblings.bits() | Self::SingleLine.bits();
        const CallExpressionArguments = Self::CommaDelimited.bits() | Self::SpaceBetweenSiblings.bits() | Self::SingleLine.bits() | Self::Parenthesis.bits();
        const NewExpressionArguments = Self::CommaDelimited.bits() | Self::SpaceBetweenSiblings.bits() | Self::SingleLine.bits() | Self::Parenthesis.bits() | Self::OptionalIfNil.bits();
        const TemplateExpressionSpans = Self::SingleLine.bits() | Self::NoInterveningComments.bits();
        const SingleLineBlockStatements = Self::SpaceBetweenBraces.bits() | Self::SpaceBetweenSiblings.bits() | Self::SingleLine.bits();
        const MultiLineBlockStatements = Self::Indented.bits() | Self::MultiLine.bits();
        const VariableDeclarationList = Self::CommaDelimited.bits() | Self::SpaceBetweenSiblings.bits() | Self::SingleLine.bits();
        const SingleLineFunctionBodyStatements = Self::SingleLine.bits() | Self::SpaceBetweenSiblings.bits() | Self::SpaceBetweenBraces.bits();
        const MultiLineFunctionBodyStatements = Self::MultiLine.bits();
        const ClassHeritageClauses = Self::SingleLine.bits();
        const ClassMembers = Self::Indented.bits() | Self::MultiLine.bits();
        const InterfaceMembers = Self::Indented.bits() | Self::MultiLine.bits();
        const EnumMembers = Self::CommaDelimited.bits() | Self::Indented.bits() | Self::MultiLine.bits();
        const CaseBlockClauses = Self::Indented.bits() | Self::MultiLine.bits();
        const NamedImportsOrExportsElements = Self::CommaDelimited.bits() | Self::SpaceBetweenSiblings.bits() | Self::AllowTrailingComma.bits() | Self::SingleLine.bits() | Self::SpaceBetweenBraces.bits() | Self::NoSpaceIfEmpty.bits();
        const JsxElementOrFragmentChildren = Self::SingleLine.bits() | Self::NoInterveningComments.bits();
        const JsxElementAttributes = Self::SingleLine.bits() | Self::SpaceBetweenSiblings.bits() | Self::NoInterveningComments.bits();
        const CaseOrDefaultClauseStatements = Self::Indented.bits() | Self::MultiLine.bits() | Self::NoTrailingNewLine.bits() | Self::OptionalIfEmpty.bits();
        const HeritageClauseTypes = Self::CommaDelimited.bits() | Self::SpaceBetweenSiblings.bits() | Self::SingleLine.bits();
        const SourceFileStatements = Self::MultiLine.bits() | Self::NoTrailingNewLine.bits();
        const Decorators = Self::MultiLine.bits() | Self::Optional.bits() | Self::SpaceAfterList.bits();
        const TypeArguments = Self::CommaDelimited.bits() | Self::SpaceBetweenSiblings.bits() | Self::SingleLine.bits() | Self::AngleBrackets.bits() | Self::Optional.bits();
        const TypeParameters = Self::CommaDelimited.bits() | Self::SpaceBetweenSiblings.bits() | Self::SingleLine.bits() | Self::AngleBrackets.bits() | Self::Optional.bits();
        const Parameters = Self::CommaDelimited.bits() | Self::SpaceBetweenSiblings.bits() | Self::SingleLine.bits() | Self::Parenthesis.bits();
        const SingleArrowParameter = Self::CommaDelimited.bits() | Self::SpaceBetweenSiblings.bits() | Self::SingleLine.bits();
        const IndexSignatureParameters = Self::CommaDelimited.bits() | Self::SpaceBetweenSiblings.bits() | Self::SingleLine.bits() | Self::Indented.bits() | Self::SquareBrackets.bits();
        const JSDocComment = Self::MultiLine.bits() | Self::AsteriskDelimited.bits();
        const ImportClauseEntries = Self::ImportAttributes.bits(); // Deprecated: Use LFImportAttributes
    }
}

fn get_opening_bracket(format: ListFormat) -> &'static str {
    let brackets = format & ListFormat::BracketsMask;
    if brackets == ListFormat::Braces {
        "{"
    } else if brackets == ListFormat::Parenthesis {
        "("
    } else if brackets == ListFormat::AngleBrackets {
        "<"
    } else if brackets == ListFormat::SquareBrackets {
        "["
    } else {
        panic!("Unexpected bracket: {brackets:?}")
    }
}

fn get_closing_bracket(format: ListFormat) -> &'static str {
    let brackets = format & ListFormat::BracketsMask;
    if brackets == ListFormat::Braces {
        "}"
    } else if brackets == ListFormat::Parenthesis {
        ")"
    } else if brackets == ListFormat::AngleBrackets {
        ">"
    } else if brackets == ListFormat::SquareBrackets {
        "]"
    } else {
        panic!("Unexpected bracket: {brackets:?}")
    }
}
