// Package printer exports a Printer for pretty-printing TS ASTs and writer interfaces and implementations for using them
// Intended ultimate usage:
//
//	func nodeToInlineStr(node *ast.Node) {
//		// Reuse singleton single-line writer (TODO: thread safety?)
//		p = printer.NewPrinter(printer.PrinterOptions{ RemoveComments: true }, printer.PrintHandlers{})
//		p.Write(node, nil /*sourceFile*/, printer.SingleLineTextWriter)
//		return printer.SingleLineTextWriter.getText()
//	}
//
// // or
//
//	func nodeToStr(node *ast.Node, options CompilerOptions) {
//		// Use own writer
//		p := printer.NewPrinter(printer.PrinterOptions{ NewLine: options.NewLine}, printer.PrintHandlers{})
//		return p.Emit(node, nil /*sourceFile*/)
//	}

use std::cell::Cell;
use std::rc::Rc;

use rustc_hash::FxHashMap;
use tsrs_ast::*;
use tsrs_core::stringutil;
use tsrs_core::*;
use tsrs_scanner as scanner;

use crate::*;

#[derive(Clone, Copy, Debug, Default)]
pub struct PrinterOptions {
    pub remove_comments: bool,
    pub new_line: NewLineKind,
    pub omit_trailing_semicolon: bool,
    pub no_emit_helpers: bool,
    // Module                        core.ModuleKind
    // ModuleResolution              core.ModuleResolutionKind
    pub target: ScriptTarget,
    pub source_map: bool,
    pub inline_source_map: bool,
    pub inline_sources: bool,
    pub omit_brace_source_map_positions: bool,
    // ExtendedDiagnostics           bool
    pub only_print_js_doc_style: bool,
    pub never_ascii_escape: bool,
    // StripInternal                 bool
    pub preserve_source_newlines: bool,
    pub terminate_unterminated_literals: bool, // !!!
}

/// Go's `*sourcemap.Generator`.
pub type SourceMapGenerator = tsrs_sourcemap::Generator;

/// Go's `sourcemap.Source` interface value: sources live in the arena (`SourceFile`) or are leaked by the emitter
/// (`declarationMapSource`), and Go compares them by identity.
pub type SourceMapSource = &'static dyn tsrs_sourcemap::Source;

pub(crate) fn same_source_map_source(
    a: Option<SourceMapSource>,
    b: Option<SourceMapSource>,
) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => std::ptr::addr_eq(
            std::ptr::from_ref::<dyn tsrs_sourcemap::Source>(a),
            std::ptr::from_ref::<dyn tsrs_sourcemap::Source>(b),
        ),
        (None, None) => true,
        _ => false,
    }
}

#[derive(Default)]
pub struct PrintHandlers {
    // A hook used by the Printer when generating unique names to avoid collisions with
    // globally defined names that exist outside of the current source file.
    pub has_global_name: Option<Rc<dyn Fn(&str) -> bool>>,
    // MapSourcePosition composes source-map positions before they reach the generator.
    // Returning ok=false (here `None`) emits a generated-only mapping for the current output position.
    pub map_source_position:
        Option<Box<dyn Fn(SourceMapSource, u32) -> Option<(SourceMapSource, u32)>>>,

    // !!! OnEmitNode, IsEmitNotificationEnabled, SubstituteNode, OnEmitSourceMapOf* (commented out in Go)
    pub on_before_emit_node: Option<Box<dyn FnMut(Option<P<Node>>)>>,
    pub on_after_emit_node: Option<Box<dyn FnMut(Option<P<Node>>)>>,
    pub on_before_emit_node_list: Option<Box<dyn FnMut(Option<P<NodeList>>)>>,
    pub on_after_emit_node_list: Option<Box<dyn FnMut(Option<P<NodeList>>)>>,
    pub on_before_emit_token: Option<Box<dyn FnMut(Option<P<Node>>)>>,
    pub on_after_emit_token: Option<Box<dyn FnMut(Option<P<Node>>)>>,
}

/// The state Go's `getTextOfNode`/`getLiteralTextOfNode` read from the printer, shared with the name generator's
/// callbacks (which Go builds as closures over the printer).
pub struct PrinterTextState {
    emit_context: P<EmitContext>,
    current_source_file: Cell<Option<P<SourceFile>>>,
    pub(crate) target: Cell<ScriptTarget>,
}

pub struct Printer {
    pub(crate) print_handlers: PrintHandlers,
    pub(crate) options: PrinterOptions,
    pub(crate) emit_context: P<EmitContext>,
    // Go `currentSourceFile` lives in `text_state` (see `current_source_file()`).
    pub(crate) text_state: Rc<PrinterTextState>,
    pub(crate) unique_helper_names: Option<FxHashMap<String, P<Node>>>,
    pub(crate) external_helpers_module_name: Option<P<Node>>,
    pub(crate) next_list_element_pos: TextPos,
    pub(crate) writer: Option<Box<dyn EmitTextWriter>>,
    pub(crate) own_writer: Option<Box<dyn EmitTextWriter>>,
    write_kind: WriteKind,
    pub(crate) source_maps_disabled: bool,
    // Borrowed from the caller of `write` for the duration of the call (Go `*sourcemap.Generator`); see
    // `source_map_generator()`.
    pub(crate) source_map_generator: Option<*mut SourceMapGenerator>,
    pub(crate) source_map_source: Option<SourceMapSource>,
    pub(crate) source_map_source_index: tsrs_sourcemap::SourceIndex,
    pub(crate) source_map_source_is_json: bool,
    pub(crate) source_map_line_char_cache: Option<LineCharacterCache>,
    pub(crate) most_recent_source_map_source: Option<SourceMapSource>,
    pub(crate) most_recent_source_map_source_index: tsrs_sourcemap::SourceIndex,
    pub(crate) container_pos: TextPos,
    pub(crate) container_end: TextPos,
    pub(crate) declaration_list_container_end: TextPos,
    pub(crate) detached_comments_info: Vec<DetachedCommentsInfo>,
    pub(crate) comments_disabled: bool,
    in_extends: bool, // whether we are emitting the `extends` clause of a ConditionalTypeNode or InferTypeNode
    pub(crate) name_generator: NameGenerator,
    pub id_to_symbol: Option<FxHashMap<P<Node>, P<Symbol>>>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DetachedCommentsInfo {
    pub(crate) node_pos: TextPos,
    pub(crate) detached_comment_end_pos: TextPos,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CommentState {
    pub(crate) emit_flags: EmitFlags, // holds the emit flags for the current node
    pub(crate) comment_range: TextRange, // holds the comment range calculated for the current node
    pub(crate) container_pos: TextPos, // captures the value of containerPos prior to entering an node
    pub(crate) container_end: TextPos, // captures the value of containerEnd prior to entering an node
    pub(crate) declaration_list_container_end: TextPos, // captures the value of declarationListContainerEnd prior to entering an node
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SourceMapState {
    pub(crate) emit_flags: EmitFlags, // holds the emit flags for the current node
    pub(crate) source_map_range: TextRange, // holds the source map range calculated for the current node
    pub(crate) has_token_source_map_range: bool, // captures whether the source map range was set for the current node
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PrinterState {
    pub(crate) comment_state: Option<CommentState>,
    pub(crate) source_map_state: Option<SourceMapState>,
}

pub fn new_printer(
    options: PrinterOptions,
    handlers: PrintHandlers,
    emit_context: Option<P<EmitContext>>,
) -> Printer {
    // wire up name generator
    let emit_context = match emit_context {
        Some(emit_context) => emit_context,
        None => new_emit_context(),
    };
    let text_state = Rc::new(PrinterTextState {
        emit_context,
        current_source_file: Cell::new(None),
        target: Cell::new(options.target),
    });
    let mut printer = Printer {
        print_handlers: handlers,
        options,
        emit_context,
        text_state: Rc::clone(&text_state),
        unique_helper_names: None,
        external_helpers_module_name: None,
        next_list_element_pos: TextPos::source(0),
        writer: None,
        own_writer: None,
        write_kind: WriteKind::None,
        source_maps_disabled: false,
        source_map_generator: None,
        source_map_source: None,
        source_map_source_index: tsrs_sourcemap::SourceIndex::Missing,
        source_map_source_is_json: false,
        source_map_line_char_cache: None,
        most_recent_source_map_source: None,
        most_recent_source_map_source_index: tsrs_sourcemap::SourceIndex::Missing,
        container_pos: TextPos::Synthetic,
        container_end: TextPos::Synthetic,
        declaration_list_container_end: TextPos::Synthetic,
        detached_comments_info: Vec::new(),
        comments_disabled: false,
        in_extends: false,
        name_generator: NameGenerator::default(),
        id_to_symbol: None,
    };
    printer.name_generator.context = Some(printer.emit_context);
    let state = Rc::clone(&text_state);
    printer.name_generator.get_text_of_node =
        Some(Rc::new(move |g: &mut NameGenerator, node: P<Node>| {
            get_text_of_node_worker(g, &state, node, false)
        }));
    let state = text_state;
    let has_global_name = printer.print_handlers.has_global_name.clone();
    printer.name_generator.is_file_level_unique_name_in_current_file =
        Some(Rc::new(move |name: &str, _private_name: bool| {
            is_file_level_unique_name_in_current_file_worker(
                &state,
                has_global_name.as_deref(),
                name,
            )
        }));
    printer.comments_disabled = options.remove_comments;
    printer
}

/// Go `(*Printer).getLiteralTextOfNode` over the state it reads (see `printerTextState`).
fn get_literal_text_of_node_worker(
    g: &mut NameGenerator,
    s: &PrinterTextState,
    node: P<Node>,
    source_file: Option<P<SourceFile>>,
    flags: getLiteralTextFlags,
) -> String {
    let mut flags = flags;
    if is_string_literal(node)
        && let Some(text_source_node) = s.emit_context.text_source(node)
    {
        let text = match text_source_node.kind() {
            Kind::NumericLiteral => text_source_node.text().to_string(),
            Kind::Identifier | Kind::PrivateIdentifier | Kind::JsxNamespacedName => {
                get_text_of_node_worker(g, s, text_source_node, false)
            }
            _ => {
                return get_literal_text_of_node_worker(
                    g,
                    s,
                    text_source_node,
                    get_source_file_of_node(text_source_node),
                    flags,
                );
            }
        };

        if flags.intersects(getLiteralTextFlags::JsxAttributeEscape) {
            return format!("\"{}\"", escape_jsx_attribute_string(&text, QuoteChar::DoubleQuote));
        } else if flags.intersects(getLiteralTextFlags::NeverAsciiEscape)
            || s.emit_context.emit_flags(node).intersects(EmitFlags::NoAsciiEscaping)
        {
            return format!("\"{}\"", escape_string(&text, QuoteChar::DoubleQuote));
        }
        return format!("\"{}\"", escape_non_ascii_string(&text, QuoteChar::DoubleQuote));
    }
    // !!! Printer option to control whether to terminate unterminated literals
    if s.emit_context.emit_flags(node).intersects(EmitFlags::NoAsciiEscaping) {
        flags |= getLiteralTextFlags::NeverAsciiEscape;
    }
    if s.emit_context.emit_flags(node).intersects(EmitFlags::InternalSymbolName) {
        flags |= getLiteralTextFlags::InternalSymbolName;
    }
    if s.target.get() >= ScriptTarget::ES2021 {
        flags |= getLiteralTextFlags::AllowNumericSeparator;
    }
    get_literal_text(node, coalesce(source_file, s.current_source_file.get()), flags)
}

/// Go `(*Printer).getTextOfNode` over the state it reads (see `printerTextState`).
// `node` must be one of Identifier | PrivateIdentifier | LiteralExpression | JsxNamespacedName
fn get_text_of_node_worker(
    g: &mut NameGenerator,
    s: &PrinterTextState,
    node: P<Node>,
    include_trivia: bool,
) -> String {
    if is_member_name(node) && s.emit_context.has_auto_generate_info(Some(node)) {
        return g.generate_name(node);
    }

    if is_string_literal(node)
        && let Some(text_source_node) = s.emit_context.text_source(node)
    {
        return get_text_of_node_worker(g, s, text_source_node, include_trivia);
    }

    let current_source_file = s.current_source_file.get();
    let can_use_source_file =
        current_source_file.is_some() && node.parent().is_some() && !node_is_synthesized(node);

    match node.kind() {
        Kind::Identifier | Kind::PrivateIdentifier | Kind::JsxNamespacedName => {
            if !can_use_source_file
                || get_source_file_of_node(node).map(|f| f.as_node())
                    != Some(
                        s.emit_context
                            .most_original(Some(current_source_file.unwrap().as_node()))
                            .unwrap(),
                    )
            {
                return node.text().to_string();
            }
        }
        Kind::StringLiteral
        | Kind::NumericLiteral
        | Kind::BigIntLiteral
        | Kind::NoSubstitutionTemplateLiteral
        | Kind::TemplateHead
        | Kind::TemplateMiddle
        | Kind::TemplateTail => {
            return get_literal_text_of_node_worker(
                g,
                s,
                node,
                None, /*sourceFile*/
                getLiteralTextFlags::None,
            );
        }
        _ => panic!("unexpected node: {:?}", node.kind()),
    }
    scanner::get_source_text_of_node_from_source_file(
        current_source_file.unwrap(),
        node,
        include_trivia,
    )
}

/// Go `(*Printer).isFileLevelUniqueNameInCurrentFile` over the state it reads.
// Returns a value indicating whether a name is unique globally or within the current file.
fn is_file_level_unique_name_in_current_file_worker(
    s: &PrinterTextState,
    has_global_name: Option<&dyn Fn(&str) -> bool>,
    name: &str,
) -> bool {
    if let Some(current_source_file) = s.current_source_file.get() {
        s.emit_context.is_file_level_unique_name(current_source_file, name, has_global_name)
    } else {
        true
    }
}

impl Printer {
    pub(crate) fn current_source_file(&self) -> Option<P<SourceFile>> {
        self.text_state.current_source_file.get()
    }

    pub(crate) fn set_current_source_file(&self, source_file: Option<P<SourceFile>>) {
        self.text_state.current_source_file.set(source_file);
    }

    pub(crate) fn writer(&mut self) -> &mut dyn EmitTextWriter {
        self.writer.as_deref_mut().unwrap()
    }

    pub(crate) fn get_literal_text_of_node(
        &mut self,
        node: P<Node>,
        source_file: Option<P<SourceFile>>,
        flags: getLiteralTextFlags,
    ) -> String {
        get_literal_text_of_node_worker(
            &mut self.name_generator,
            &self.text_state,
            node,
            source_file,
            flags,
        )
    }

    // `node` must be one of Identifier | PrivateIdentifier | LiteralExpression | JsxNamespacedName
    fn get_text_of_node(&mut self, node: P<Node>, include_trivia: bool) -> String {
        get_text_of_node_worker(&mut self.name_generator, &self.text_state, node, include_trivia)
    }
}

//
// Low-level writing
//

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum WriteKind {
    #[default]
    None,
    Keyword,
    Operator,
    Punctuation,
    StringLiteral,
    Parameter,
    Property,
    Comment,
    Literal,
}

impl Printer {
    fn write_as(&mut self, text: &str, write_kind: WriteKind) {
        match write_kind {
            WriteKind::None => self.writer().write(text),
            WriteKind::Parameter => self.write_parameter(text),
            WriteKind::Keyword => self.write_keyword(text),
            WriteKind::Operator => self.write_operator(text),
            WriteKind::Property => self.write_property(text),
            WriteKind::Punctuation => self.write_punctuation(text),
            WriteKind::StringLiteral => self.writer().write_string_literal(text),
            WriteKind::Comment => self.write_comment(text),
            WriteKind::Literal => self.write_literal(text),
        }
    }

    // Go `(*Printer).write`; suffixed because the exported `Write` takes the plain snake_case name.
    fn write_(&mut self, text: &str) {
        self.write_as(text, self.write_kind);
    }

    fn write_symbol(&mut self, text: &str, opt_symbol: Option<P<Symbol>>) {
        match opt_symbol {
            None => self.write_(text),
            Some(symbol) => self.writer().write_symbol(text, symbol),
        }
    }

    pub(crate) fn write_literal(&mut self, text: &str) {
        self.writer().write_literal(text);
    }

    pub(crate) fn write_punctuation(&mut self, text: &str) {
        self.writer().write_punctuation(text);
    }

    pub(crate) fn write_operator(&mut self, text: &str) {
        self.writer().write_operator(text);
    }

    pub(crate) fn write_keyword(&mut self, text: &str) {
        self.writer().write_keyword(text);
    }

    fn write_property(&mut self, text: &str) {
        self.writer().write_property(text);
    }

    fn write_parameter(&mut self, text: &str) {
        self.writer().write_parameter(text);
    }

    pub(crate) fn write_comment(&mut self, text: &str) {
        self.writer().write_comment(text);
    }

    pub(crate) fn write_space(&mut self) {
        self.writer().write_space(" ");
    }

    pub(crate) fn write_line(&mut self) {
        self.writer().write_line();
    }

    pub(crate) fn write_line_repeat(&mut self, count: usize) {
        for _ in 0..count {
            self.write_line();
        }
    }

    pub(crate) fn write_lines(&mut self, text: &str) {
        let lines = stringutil::split_lines(text);
        let indentation = stringutil::guess_indentation(&lines);
        for line in lines {
            let mut line = line;
            if indentation > 0 {
                line = &line[indentation..];
            }
            if !line.is_empty() {
                self.write_line();
                self.write_(line);
            }
        }
    }

    pub(crate) fn write_trailing_semicolon(&mut self) {
        self.writer().write_trailing_semicolon(";");
    }

    pub(crate) fn increase_indent(&mut self) {
        self.writer().increase_indent();
    }

    pub(crate) fn decrease_indent(&mut self) {
        self.writer().decrease_indent();
    }

    pub(crate) fn increase_indent_if(&mut self, indent_requested: bool) {
        if indent_requested {
            self.increase_indent();
        }
    }

    pub(crate) fn decrease_indent_if(&mut self, indent_requested: bool) {
        if indent_requested {
            self.decrease_indent();
        }
    }

    pub(crate) fn write_line_or_space(
        &mut self,
        parent_node: P<Node>,
        prev_child_node: P<Node>,
        next_child_node: P<Node>,
    ) {
        if self.should_emit_on_single_line(parent_node) {
            self.write_space();
        } else if self.options.preserve_source_newlines {
            let lines = self.get_lines_between_nodes(parent_node, prev_child_node, next_child_node);
            if lines > 0 {
                self.write_line_repeat(lines);
            } else {
                self.write_space();
            }
        } else {
            self.write_line();
        }
    }

    pub(crate) fn write_lines_and_indent(
        &mut self,
        line_count: usize,
        write_space_if_not_indenting: bool,
    ) {
        if line_count > 0 {
            self.increase_indent();
            self.write_line_repeat(line_count);
        } else if write_space_if_not_indenting {
            self.write_space();
        }
    }

    pub(crate) fn write_line_separators_and_indent_before(
        &mut self,
        node: P<Node>,
        parent: P<Node>,
    ) -> bool {
        if self.options.preserve_source_newlines {
            let leading_newlines =
                self.get_leading_line_terminator_count(Some(parent), Some(node), ListFormat::None);
            if leading_newlines > 0 {
                self.write_lines_and_indent(
                    leading_newlines,
                    false, /*writeSpaceIfNotIndenting*/
                );
                return true;
            }
        }
        false
    }

    pub(crate) fn write_line_separators_after(&mut self, node: P<Node>, parent: P<Node>) {
        if self.options.preserve_source_newlines {
            let trailing_newlines = self.get_closing_line_terminator_count(
                Some(parent),
                Some(node),
                ListFormat::None,
                TextRange::synthetic(),
            );
            if trailing_newlines > 0 {
                self.write_line_repeat(trailing_newlines);
            }
        }
    }

    pub(crate) fn get_lines_between_nodes(
        &self,
        parent: P<Node>,
        node1: P<Node>,
        node2: P<Node>,
    ) -> usize {
        if self.should_elide_indentation(parent) {
            return 0;
        }

        let parent = skip_synthesized_parentheses(parent);
        let node1 = skip_synthesized_parentheses(node1);
        let node2 = skip_synthesized_parentheses(node2);

        // Always use a newline for synthesized code if the synthesizer desires it.
        if self.should_emit_on_new_line(node2, ListFormat::None) {
            return 1;
        }

        if let Some(current_source_file) = self.current_source_file()
            && !node_is_synthesized(parent)
            && !node_is_synthesized(node1)
            && !node_is_synthesized(node2)
        {
            if self.options.preserve_source_newlines {
                return self.get_effective_lines(|include_comments| {
                    get_lines_between_range_end_and_range_start(
                        node1.loc(),
                        node2.loc(),
                        current_source_file,
                        include_comments,
                    )
                });
            }
            return usize::from(!range_end_is_on_same_line_as_range_start(
                node1.loc(),
                node2.loc(),
                current_source_file,
            ));
        }

        0
    }

    fn get_effective_lines(&self, get_line_difference: impl Fn(bool) -> LineDifference) -> usize {
        // If 'preserveSourceNewlines' is disabled, we should never call this function
        // because it could be more expensive than alternative approximations.
        assert!(
            self.options.preserve_source_newlines,
            "Should not be called when preserveSourceNewlines is false"
        );
        // We start by measuring the line difference from a position to its adjacent comments,
        // so that this is counted as a one-line difference, not two:
        //
        //   node1;
        //   // NODE2 COMMENT
        //   node2;
        let lines = get_line_difference(true /*includeComments*/);
        if lines.is_same() {
            // However, if the line difference considering comments was 0, we might have this:
            //
            //   node1; // NODE2 COMMENT
            //   node2;
            //
            // in which case we should be ignoring node2's comment, so this too is counted as
            // a one-line difference, not zero.
            return get_line_difference(false /*includeComments*/).forward_count();
        }
        lines.forward_count()
    }

    pub(crate) fn get_leading_line_terminator_count(
        &self,
        parent_node: Option<P<Node>>,
        first_child: Option<P<Node>>,
        format: ListFormat,
    ) -> usize {
        if format.intersects(ListFormat::PreserveLines) || self.options.preserve_source_newlines {
            if format.intersects(ListFormat::PreferNewLine) {
                return 1;
            }

            let Some(first_child) = first_child else {
                return usize::from(
                    !(parent_node.is_none()
                        || self.current_source_file().is_some()
                            && range_is_on_single_line(
                                parent_node.unwrap().loc(),
                                self.current_source_file().unwrap(),
                            )),
                );
            };
            if self.next_list_element_pos.source_offset().is_some_and(|pos| pos > 0)
                && first_child.pos() == self.next_list_element_pos
            {
                // If this child starts at the beginning of a list item in a parent list, its leading
                // line terminators have already been written as the separating line terminators of the
                // parent list. Example:
                //
                // class Foo {
                //   constructor() {}
                //   public foo() {}
                // }
                //
                // The outer list is the list of class members, with one line terminator between the
                // constructor and the method. The constructor is written, the separating line terminator
                // is written, and then we start emitting the method. Its modifiers ([public]) constitute an inner
                // list, so we look for its leading line terminators. If we didn't know that we had already
                // written a newline as part of the parent list, it would appear that we need to write a
                // leading newline to start the modifiers.
                return 0;
            }
            if first_child.kind() == Kind::JsxText {
                // JsxText will be written with its leading whitespace, so don't add more manually.
                return 0;
            }
            if let (Some(current_source_file), Some(parent_node)) =
                (self.current_source_file(), parent_node)
                && !position_is_synthesized(parent_node.pos())
                && !node_is_synthesized(first_child)
                && (first_child.parent().is_none()/*|| getOriginalNode(firstChild.Parent) == getOriginalNode(parentNode)*/)
            {
                if self.options.preserve_source_newlines {
                    return self.get_effective_lines(|include_comments| {
                        get_lines_between_position_and_preceding_non_whitespace_character(
                            first_child.pos(),
                            parent_node.pos(),
                            current_source_file,
                            include_comments,
                        )
                    });
                }
                return usize::from(!range_start_positions_are_on_same_line(
                    parent_node.loc(),
                    first_child.loc(),
                    current_source_file,
                ));
            }
            if self.should_emit_on_new_line(first_child, format) {
                return 1;
            }
        }
        usize::from(format.intersects(ListFormat::MultiLine))
    }

    pub(crate) fn get_separating_line_terminator_count(
        &self,
        previous_node: Option<P<Node>>,
        next_node: Option<P<Node>>,
        format: ListFormat,
    ) -> usize {
        if format.intersects(ListFormat::PreserveLines) || self.options.preserve_source_newlines {
            let (Some(previous_node), Some(next_node)) = (previous_node, next_node) else {
                return 0;
            };
            if next_node.kind() == Kind::JsxText {
                // JsxText will be written with its leading whitespace, so don't add more manually.
                return 0;
            } else if self.current_source_file().is_some()
                && !node_is_synthesized(previous_node)
                && !node_is_synthesized(next_node)
            {
                let current_source_file = self.current_source_file().unwrap();
                if self.options.preserve_source_newlines
                    && sibling_node_positions_are_comparable(
                        self.emit_context,
                        previous_node,
                        next_node,
                    )
                {
                    return self.get_effective_lines(|include_comments| {
                        get_lines_between_range_end_and_range_start(
                            previous_node.loc(),
                            next_node.loc(),
                            current_source_file,
                            include_comments,
                        )
                    });
                } else if !self.options.preserve_source_newlines
                    && original_nodes_have_same_parent(self.emit_context, previous_node, next_node)
                {
                    // If `preserveSourceNewlines` is `false` we do not intend to preserve the effective lines between the
                    // previous and next node. Instead we naively check whether nodes are on separate lines within the
                    // same node parent. If so, we intend to preserve a single line terminator. This is less precise and
                    // expensive than checking with `preserveSourceNewlines` as above, but the goal is not to preserve the
                    // effective source lines between two sibling nodes.
                    return usize::from(!range_end_is_on_same_line_as_range_start(
                        previous_node.loc(),
                        next_node.loc(),
                        current_source_file,
                    ));
                }
                // If the two nodes are not comparable, add a line terminator based on the format that can indicate
                // whether new lines are preferred or not.
                return usize::from(format.intersects(ListFormat::PreferNewLine));
            } else if self.should_emit_on_new_line(previous_node, format)
                || self.should_emit_on_new_line(next_node, format)
            {
                return 1;
            }
        } else if self.should_emit_on_new_line(next_node.unwrap(), ListFormat::None) {
            return 1;
        }
        usize::from(format.intersects(ListFormat::MultiLine))
    }

    pub(crate) fn get_closing_line_terminator_count(
        &self,
        parent_node: Option<P<Node>>,
        last_child: Option<P<Node>>,
        format: ListFormat,
        children_text_range: TextRange,
    ) -> usize {
        if format.intersects(ListFormat::PreserveLines) || self.options.preserve_source_newlines {
            if format.intersects(ListFormat::PreferNewLine) {
                return 1;
            }
            let Some(last_child) = last_child else {
                return usize::from(
                    !(parent_node.is_none()
                        || self.current_source_file().is_some()
                            && range_is_on_single_line(
                                parent_node.unwrap().loc(),
                                self.current_source_file().unwrap(),
                            )),
                );
            };
            if let (Some(current_source_file), Some(parent_node)) =
                (self.current_source_file(), parent_node)
                && !position_is_synthesized(parent_node.pos())
                && !node_is_synthesized(last_child)
                && (last_child.parent().is_none() || last_child.parent() == Some(parent_node))
            {
                if self.options.preserve_source_newlines {
                    let end = greatest_end(last_child.end(), &[&children_text_range]);
                    return self.get_effective_lines(|include_comments| {
                        get_lines_between_position_and_next_non_whitespace_character(
                            end,
                            parent_node.end(),
                            current_source_file,
                            include_comments,
                        )
                    });
                }
                return usize::from(!range_end_positions_are_on_same_line(
                    parent_node.loc(),
                    last_child.loc(),
                    current_source_file,
                ));
            }
            if self.should_emit_on_new_line(last_child, format) {
                return 1;
            }
        }
        if format.intersects(ListFormat::MultiLine)
            && !format.intersects(ListFormat::NoTrailingNewLine)
        {
            return 1;
        }
        0
    }

    pub(crate) fn write_comment_range(&mut self, comment: CommentRange) {
        let Some(current_source_file) = self.current_source_file() else {
            return;
        };

        let text = current_source_file.text();
        let line_map = current_source_file.ecma_line_map();
        self.write_comment_range_worker(text, line_map, comment.kind, comment.text_range);
    }

    pub(crate) fn write_comment_range_worker(
        &mut self,
        text: &str,
        line_map: &[u32],
        kind: Kind,
        loc: TextRange,
    ) {
        if kind == Kind::MultiLineCommentTrivia {
            let indent_size = get_default_indent_size() as i32;
            let loc_pos = loc.pos().expect_source("comment ranges have source positions");
            let loc_end = loc.end().expect_source("comment ranges have source positions");
            let first_line = scanner::compute_line_of_position(line_map, loc_pos);
            let line_count = line_map.len();
            let mut first_comment_line_indent = None;
            let mut pos = loc_pos;
            let mut current_line = first_line;
            while pos < loc_end {
                let next_line_start = if current_line + 1 == line_count {
                    text.len() as u32 + 1
                } else {
                    line_map[current_line + 1]
                };

                if pos != loc_pos {
                    // If we are not emitting first line, we need to write the spaces to adjust the alignment
                    let first_comment_line_indent =
                        *first_comment_line_indent.get_or_insert_with(|| {
                            calculate_indent(text, line_map[first_line], loc_pos)
                        });

                    // These are number of spaces writer is going to write at current indent
                    let current_writer_indent_spacing =
                        self.writer().get_indent() as i32 * indent_size;

                    // Number of spaces we want to be writing
                    // eg: Assume writer indent
                    // module m {
                    //         /* starts at character 9 this is line 1
                    //    * starts at character pos 4 line                        --1  = 8 - 8 + 3
                    //   More left indented comment */                            --2  = 8 - 8 + 2
                    //     class c { }
                    // }
                    // module m {
                    //     /* this is line 1 -- Assume current writer indent 8
                    //      * line                                                --3 = 8 - 4 + 5
                    //            More right indented comment */                  --4 = 8 - 4 + 11
                    //     class c { }
                    // }
                    let spaces_to_emit = current_writer_indent_spacing - first_comment_line_indent
                        + calculate_indent(text, pos, next_line_start);
                    if spaces_to_emit > 0 {
                        let mut number_of_single_spaces_to_emit = spaces_to_emit % indent_size;
                        let indent_size_space_string = get_indent_string(
                            ((spaces_to_emit - number_of_single_spaces_to_emit) / indent_size)
                                .try_into()
                                .expect("positive indentation fits in u32"),
                            indent_size as usize,
                        );

                        // Write indent size string ( in eg 1: = "", 2: "" , 3: string with 8 spaces 4: string with 12 spaces
                        self.writer().raw_write(&indent_size_space_string);

                        // Emit the single spaces (in eg: 1: 3 spaces, 2: 2 spaces, 3: 1 space, 4: 3 spaces)
                        while number_of_single_spaces_to_emit > 0 {
                            self.writer().raw_write(" ");
                            number_of_single_spaces_to_emit -= 1;
                        }
                    } else {
                        // No spaces to emit write empty string
                        self.writer().raw_write("");
                    }
                }

                // Write the comment line text
                let mut end = std::cmp::min(loc_end, next_line_start);
                let mut scan = pos;
                while scan < end {
                    let (ch, size) =
                        stringutil::decode_rune(&text.as_bytes()[scan as usize..end as usize]);
                    if size == 0 {
                        break;
                    }
                    if stringutil::is_line_break(ch) {
                        end = scan;
                        break;
                    }
                    scan += size as u32;
                }
                let current_line_text = text[pos as usize..end as usize].trim();
                if current_line_text.is_empty() {
                    // Empty string - make sure we write empty line
                    self.writer().write_line_force(true);
                } else {
                    self.write_comment(current_line_text);
                    if end != loc_end {
                        self.write_line();
                    }
                }

                pos = next_line_start;
                current_line += 1;
            }
        } else {
            // Single line comment of style //....
            self.write_comment(
                &text[loc.pos().expect_source("comment ranges have source positions") as usize
                    ..loc.end().expect_source("comment ranges have source positions") as usize],
            );
        }
    }
}

//
// Custom emit behavior stubs (i.e., from `EmitNode`, `EmitFlags`, etc.)
//

impl Printer {
    pub(crate) fn should_emit_comments(&self, node: P<Node>) -> bool {
        !self.comments_disabled && self.current_source_file().is_some() && !is_source_file(node)
    }

    pub(crate) fn should_write_comment(&self, comment: CommentRange) -> bool {
        !self.options.only_print_js_doc_style
            || self.current_source_file().is_some()
                && is_jsdoc_like_text(self.current_source_file().unwrap().text(), comment)
            || self.current_source_file().is_some()
                && is_pinned_comment(self.current_source_file().unwrap().text(), comment)
    }

    pub(crate) fn should_emit_indented(&self, node: P<Node>) -> bool {
        self.emit_context.emit_flags(node).intersects(EmitFlags::Indented)
    }

    fn should_elide_indentation(&self, node: P<Node>) -> bool {
        self.emit_context.emit_flags(node).intersects(EmitFlags::NoIndentation)
    }

    pub(crate) fn should_emit_on_single_line(&self, node: P<Node>) -> bool {
        self.emit_context.emit_flags(node).intersects(EmitFlags::SingleLine)
    }

    pub(crate) fn should_emit_on_multiple_lines(&self, node: P<Node>) -> bool {
        self.emit_context.emit_flags(node).intersects(EmitFlags::MultiLine)
    }

    fn should_emit_block_function_body_on_single_line(&self, body: P<Node>) -> bool {
        // We must emit a function body as a single-line body in the following case:
        // * The body has NodeEmitFlags.SingleLine specified.

        // We must emit a function body as a multi-line body in the following cases:
        // * The body is explicitly marked as multi-line.
        // * A non-synthesized body's start and end position are on different lines.
        // * Any statement in the body starts on a new line.

        if self.should_emit_on_single_line(body) {
            return true;
        }

        let block = body.as_block();
        if block.multi_line {
            return false;
        }

        if !node_is_synthesized(body)
            && self.current_source_file().is_some()
            && !range_is_on_single_line(body.loc(), self.current_source_file().unwrap())
        {
            return false;
        }

        if self.get_leading_line_terminator_count(
            Some(body),
            block.statements.nodes().first().copied(),
            ListFormat::PreserveLines,
        ) > 0
            || self.get_closing_line_terminator_count(
                Some(body),
                block.statements.nodes().last().copied(),
                ListFormat::PreserveLines,
                block.statements.loc(),
            ) > 0
        {
            return false;
        }

        let mut previous_statement: Option<P<Node>> = None;
        for statement in block.statements.nodes() {
            if self.get_separating_line_terminator_count(
                previous_statement,
                Some(*statement),
                ListFormat::PreserveLines,
            ) > 0
            {
                return false;
            }

            previous_statement = Some(*statement);
        }

        true
    }

    fn should_emit_on_new_line(&self, node: P<Node>, format: ListFormat) -> bool {
        if self.emit_context.emit_flags(node).intersects(EmitFlags::StartOnNewLine) {
            return true;
        }
        format.intersects(ListFormat::PreferNewLine)
    }

    pub(crate) fn should_emit_source_maps(&self, node: P<Node>) -> bool {
        !self.source_maps_disabled
            && self.source_map_source.is_some()
            && !is_source_file(node)
            && !is_in_json_file(node)
    }

    pub(crate) fn should_emit_token_source_maps(
        &self,
        token: Kind,
        _pos: TextPos,
        context_node: P<Node>,
        flags: tokenEmitFlags,
    ) -> bool {
        // We don't emit source positions for most tokens as it tends to be quite noisy, however
        // we need to emit source positions for open and close braces so that tools like istanbul
        // can map branches for code coverage. However, we still omit brace source positions when
        // the output is a declaration file.
        !flags.intersects(tokenEmitFlags::NoSourceMaps)
            && self.should_emit_source_maps(context_node)
            && !self.options.omit_brace_source_map_positions
            && (token == Kind::OpenBraceToken || token == Kind::CloseBraceToken)
    }

    pub(crate) fn should_emit_leading_comments(&self, node: P<Node>) -> bool {
        !self.emit_context.emit_flags(node).intersects(EmitFlags::NoLeadingComments)
    }

    pub(crate) fn should_emit_trailing_comments(&self, node: P<Node>) -> bool {
        !self.emit_context.emit_flags(node).intersects(EmitFlags::NoTrailingComments)
    }

    pub(crate) fn should_emit_detached_comments(node: P<Node>) -> bool {
        if !is_source_file(node) {
            return true;
        }

        let file = node.as_source_file();

        // Emit detached comment if there are no prologue directives or if the first node is synthesized.
        // The synthesized node will have no leading comment so some comments may be missed.
        file.statements.nodes().is_empty()
            || !is_prologue_directive(file.statements.nodes()[0])
            || node_is_synthesized(file.statements.nodes()[0])
    }

    pub(crate) fn has_comments_at_position(&self, pos: TextPos) -> bool {
        let Some(current_source_file) = self.current_source_file() else {
            return false;
        };
        if pos.is_synthetic() {
            return false;
        }

        let pos = pos.expect_source("source comments have source positions") + 1;
        if scanner::get_trailing_comment_ranges(current_source_file.text(), pos).next().is_some() {
            return true;
        }
        if scanner::get_leading_comment_ranges(current_source_file.text(), pos).next().is_some() {
            return true;
        }
        false
    }

    pub(crate) fn should_emit_indirect_call(&self, node: P<Node>) -> bool {
        self.emit_context.emit_flags(node).intersects(EmitFlags::IndirectCall)
    }

    pub(crate) fn should_allow_trailing_comma(
        &self,
        node: P<Node>,
        list: Option<P<NodeList>>,
    ) -> bool {
        let Some(current_source_file) = self.current_source_file() else {
            return false;
        };
        if current_source_file.script_kind() == ScriptKind::JSON {
            return false;
        }

        match node.kind() {
            Kind::ClassExpression | Kind::ClassDeclaration | Kind::InterfaceDeclaration => {
                list == node.type_parameter_list()
            }
            Kind::ObjectLiteralExpression
            | Kind::ArrayLiteralExpression
            | Kind::ArrowFunction
            | Kind::Constructor
            | Kind::GetAccessor
            | Kind::SetAccessor
            | Kind::TypeAliasDeclaration
            | Kind::JSTypeAliasDeclaration
            | Kind::FunctionType
            | Kind::ConstructorType
            | Kind::CallSignature
            | Kind::ConstructSignature
            | Kind::TaggedTemplateExpression
            | Kind::ObjectBindingPattern
            | Kind::ArrayBindingPattern
            | Kind::NamedImports
            | Kind::NamedExports
            | Kind::ImportAttributes
            | Kind::FunctionDeclaration
            | Kind::FunctionExpression
            | Kind::MethodDeclaration
            | Kind::CallExpression
            | Kind::NewExpression => true,
            _ => false,
        }
    }
}

//
// Tokens/Keywords
//

impl Printer {
    pub(crate) fn write_token_text(
        &mut self,
        token: Kind,
        write_kind: WriteKind,
        pos: TextPos,
    ) -> TextPos {
        // !!! emit leading and trailing comments
        // !!! emit leading and trailing source maps
        let token_string = scanner::token_to_string(token);
        self.write_as(token_string, write_kind);
        if position_is_synthesized(pos) { pos } else { pos.offset_by(token_string.len() as u32) }
    }

    pub(crate) fn emit_token(
        &mut self,
        token: Kind,
        pos: TextPos,
        write_kind: WriteKind,
        context_node: P<Node>,
    ) -> TextPos {
        self.emit_token_ex(token, pos, write_kind, context_node, tokenEmitFlags::None)
    }

    pub(crate) fn emit_token_ex(
        &mut self,
        token: Kind,
        pos: TextPos,
        write_kind: WriteKind,
        context_node: P<Node>,
        flags: tokenEmitFlags,
    ) -> TextPos {
        let (state, pos) = self.enter_token(token, pos, context_node, flags);
        let pos = self.write_token_text(token, write_kind, pos);
        self.exit_token(token, pos, context_node, state);
        pos
    }

    pub(crate) fn emit_keyword_node(&mut self, node: Option<P<Node>>) {
        self.emit_keyword_node_ex(node, tokenEmitFlags::None);
    }

    fn emit_keyword_node_ex(&mut self, node: Option<P<Node>>, flags: tokenEmitFlags) {
        let Some(node) = node else {
            return;
        };

        let state = self.enter_token_node(node, flags);
        self.write_token_text(node.kind(), WriteKind::Keyword, node.pos());
        self.exit_token_node(node, state);
    }

    pub(crate) fn emit_punctuation_node(&mut self, node: Option<P<Node>>) {
        self.emit_punctuation_node_ex(node, tokenEmitFlags::None);
    }

    fn emit_punctuation_node_ex(&mut self, node: Option<P<Node>>, flags: tokenEmitFlags) {
        let Some(node) = node else {
            return;
        };

        let state = self.enter_token_node(node, flags);
        self.write_token_text(node.kind(), WriteKind::Punctuation, node.pos());
        self.exit_token_node(node, state);
    }

    pub(crate) fn emit_token_node(&mut self, node: Option<P<Node>>) {
        self.emit_token_node_ex(node, tokenEmitFlags::None);
    }

    pub(crate) fn emit_token_node_ex(&mut self, node: Option<P<Node>>, flags: tokenEmitFlags) {
        let Some(n) = node else {
            return;
        };

        if is_keyword_kind(n.kind()) {
            self.emit_keyword_node_ex(node, flags);
        } else if is_punctuation_kind(n.kind()) {
            self.emit_punctuation_node_ex(node, flags);
        } else {
            panic!("unexpected TokenNode: {:?}", n.kind());
        }
    }
}

//
// Literals
//

impl Printer {
    // Emits literals of the following kinds
    //
    //	SyntaxKindNumericLiteral
    //	SyntaxKindBigIntLiteral
    //	SyntaxKindStringLiteral
    //	SyntaxKindNoSubstitutionTemplateLiteral
    //	SyntaxKindRegularExpressionLiteral
    //	SyntaxKindTemplateHead
    //	SyntaxKindTemplateMiddle
    //	SyntaxKindTemplateTail
    fn emit_literal(&mut self, node: P<Node>, flags: getLiteralTextFlags) {
        let mut flags = flags;
        // Add NeverAsciiEscape flag if the printer option is set
        if self.options.never_ascii_escape {
            flags |= getLiteralTextFlags::NeverAsciiEscape;
        }
        if self.options.terminate_unterminated_literals {
            flags |= getLiteralTextFlags::TerminateUnterminatedLiterals;
        }

        let text = self.get_literal_text_of_node(node, None /*sourceFile*/, flags);

        // !!! Printer option to control source map emit, which causes us to use a different write method on the
        // emit text writer:

        ////if (
        ////	(printerOptions.sourceMap || printerOptions.inlineSourceMap)
        ////	&& (node.kind === SyntaxKindStringLiteral || isTemplateLiteralKind(node.kind))
        ////) {
        ////	writeLiteral(text);
        ////} else {

        // Quick info expects all literals to be called with writeStringLiteral, as there's no specific type for
        // numberLiterals
        self.writer().write_string_literal(&text);

        // }
    }

    pub(crate) fn emit_numeric_literal(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_literal(node, getLiteralTextFlags::None);
        self.exit_node(node, state);
    }

    pub(crate) fn emit_big_int_literal(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_literal(node, getLiteralTextFlags::None); // TODO: Preserve numeric literal separators after Strada migration
        self.exit_node(node, state);
    }

    pub(crate) fn emit_string_literal(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_literal(node, getLiteralTextFlags::None);
        self.exit_node(node, state);
    }

    pub(crate) fn emit_no_substitution_template_literal(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_literal(node, getLiteralTextFlags::None);
        self.exit_node(node, state);
    }

    pub(crate) fn emit_regular_expression_literal(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_literal(node, getLiteralTextFlags::None);
        self.exit_node(node, state);
    }
}

//
// Pseudo-literals
//

impl Printer {
    pub(crate) fn emit_template_head(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_literal(node, getLiteralTextFlags::None);
        self.exit_node(node, state);
    }

    pub(crate) fn emit_template_middle(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_literal(node, getLiteralTextFlags::None);
        self.exit_node(node, state);
    }

    pub(crate) fn emit_template_tail(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_literal(node, getLiteralTextFlags::None);
        self.exit_node(node, state);
    }

    pub(crate) fn emit_template_middle_tail(&mut self, node: P<Node>) {
        match node.kind() {
            Kind::TemplateMiddle => self.emit_template_middle(node),
            Kind::TemplateTail => self.emit_template_tail(node),
            _ => {}
        }
    }
}

//
// Snippet Elements
//

impl Printer {
    pub(crate) fn emit_snippet_node(&mut self, node: P<Node>, snippet_element: SnippetElement) {
        match snippet_element.kind {
            SnippetKind::TabStop => self.emit_tab_stop(node, snippet_element),
        }
    }

    fn emit_tab_stop(&mut self, node: P<Node>, snippet_element: SnippetElement) {
        assert!(
            node.kind() == Kind::EmptyStatement,
            "Snippet tab stops can only be emitted on empty statements"
        );
        self.writer().raw_write(&format!("${}", snippet_element.order));
    }
}

//
// Names
//

impl Printer {
    fn emit_identifier_text(&mut self, node: P<Node>) {
        let f = get_source_file_of_node(node);
        assert!(
            f.is_none()
                || self.current_source_file().is_none()
                || f.unwrap().file_name() == self.current_source_file().unwrap().file_name()
        );
        let text = self.get_text_of_node(node, false /*includeTrivia*/);

        if let Some(id_to_symbol) = &self.id_to_symbol
            && let Some(symbol) = id_to_symbol.get(&node).copied()
        {
            self.write_symbol(&text, Some(symbol));
            return;
        }
        self.write_(&text);
    }

    pub(crate) fn emit_identifier_name(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_identifier_text(node);
        self.exit_node(node, state);
    }

    pub(crate) fn emit_identifier_name_node(&mut self, node: Option<P<Node>>) {
        let Some(node) = node else {
            return;
        };
        self.emit_identifier_name(node);
    }

    fn get_unique_helper_name(&mut self, name: &str) -> P<Node> {
        let helper_name = self.unique_helper_names.as_ref().unwrap().get(name).copied();
        match helper_name {
            None => {
                let helper_name = self.emit_context.factory.new_unique_name_ex(
                    name,
                    AutoGenerateOptions {
                        flags: GeneratedIdentifierFlags::FileLevel
                            | GeneratedIdentifierFlags::Optimistic,
                        ..Default::default()
                    },
                );
                self.generate_name(helper_name);
                self.unique_helper_names.as_mut().unwrap().insert(name.to_string(), helper_name);
                helper_name
            }
            Some(helper_name) => helper_name.clone_node(&self.emit_context.factory),
        }
    }

    pub(crate) fn emit_identifier_reference(&mut self, node: P<Node>) {
        let mut node = node;
        if (self.external_helpers_module_name.is_some() || self.unique_helper_names.is_some())
            && self.emit_context.emit_flags(node).intersects(EmitFlags::HelperName)
        {
            if let Some(external_helpers_module_name) = self.external_helpers_module_name {
                // Substitute `__helper` with `tslib_1.__helper`
                let helper = {
                    let f = &self.emit_context.factory;
                    let module_name = external_helpers_module_name.clone_node(f);
                    let name = node.clone_node(f);
                    f.new_property_access_expression(
                        module_name,
                        None, /*questionDotToken*/
                        name,
                        NodeFlags::None,
                    )
                };
                self.emit_context.assign_comment_and_source_map_ranges(helper, node);
                self.emit_property_access_expression(helper);
                return;
            }
            if self.unique_helper_names.is_some() {
                // Substitute `__helper` with `__helper_1` if there is a conflict in an ES module.
                let helper_name = self.get_unique_helper_name(node.as_identifier().text());
                self.emit_context.assign_comment_and_source_map_ranges(helper_name, node);
                node = helper_name;
            }
        }

        let state = self.enter_node(node);
        self.emit_identifier_text(node);
        self.exit_node(node, state);
    }

    pub(crate) fn emit_binding_identifier(&mut self, node: P<Node>) {
        let mut node = node;
        if self.unique_helper_names.is_some()
            && self.emit_context.emit_flags(node).intersects(EmitFlags::HelperName)
        {
            // Substitute `__helper` with `__helper_1` if there is a conflict in an ES module.
            let helper_name = self.get_unique_helper_name(node.as_identifier().text());
            self.emit_context.assign_comment_and_source_map_ranges(helper_name, node);
            node = helper_name;
        }

        let state = self.enter_node(node);
        self.emit_identifier_text(node);
        self.exit_node(node, state);
    }

    pub(crate) fn emit_label_identifier(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_identifier_text(node);
        self.exit_node(node, state);
    }

    pub(crate) fn emit_private_identifier(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let text = self.get_text_of_node(node, false /*includeTrivia*/);
        self.write_(&text);
        self.exit_node(node, state);
    }

    pub(crate) fn emit_qualified_name(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_qualified_name();
        self.emit_entity_name(n.left);
        self.write_punctuation(".");
        self.emit_member_name(Some(n.right));
        self.exit_node(node, state);
    }

    pub(crate) fn emit_computed_property_name(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.write_punctuation("[");
        self.emit_expression(
            node.as_computed_property_name().expression,
            OperatorPrecedence::DISALLOW_COMMA,
        );
        self.write_punctuation("]");
        self.exit_node(node, state);
    }

    fn emit_entity_name(&mut self, node: P<Node>) {
        match node.kind() {
            Kind::Identifier => self.emit_identifier_reference(node),
            Kind::QualifiedName => self.emit_qualified_name(node),
            Kind::PropertyAccessExpression => {
                // TypeQuery nodes may have PropertyAccessExpression as exprName (e.g. typeof foo.x).
                // TS's emitter handles this via generic emit(); we dispatch to expression emitter here.
                self.emit_expression(node, OperatorPrecedence::DISALLOW_COMMA);
            }
            _ => panic!("unexpected EntityName: {:?}", node.kind()),
        }
    }

    pub(crate) fn emit_binding_name(&mut self, node: Option<P<Node>>) {
        let Some(node) = node else {
            return;
        };

        match node.kind() {
            Kind::Identifier => self.emit_binding_identifier(node),
            Kind::ObjectBindingPattern => self.emit_object_binding_pattern(node),
            Kind::ArrayBindingPattern => self.emit_array_binding_pattern(node),
            _ => panic!("unexpected BindingName: {:?}", node.kind()),
        }
    }

    pub(crate) fn emit_property_name(&mut self, node: Option<P<Node>>) {
        let Some(node) = node else {
            return;
        };

        let saved_write_kind = self.write_kind;
        self.write_kind = WriteKind::Property;

        match node.kind() {
            Kind::Identifier => self.emit_identifier_name(node),
            Kind::PrivateIdentifier => self.emit_private_identifier(node),
            Kind::StringLiteral => self.emit_string_literal(node),
            Kind::NoSubstitutionTemplateLiteral => self.emit_no_substitution_template_literal(node),
            Kind::NumericLiteral => self.emit_numeric_literal(node),
            Kind::BigIntLiteral => self.emit_big_int_literal(node),
            Kind::ComputedPropertyName => self.emit_computed_property_name(node),
            _ => panic!("unexpected PropertyName: {:?}", node.kind()),
        }

        self.write_kind = saved_write_kind;
    }

    pub(crate) fn emit_member_name(&mut self, node: Option<P<Node>>) {
        let Some(node) = node else {
            return;
        };

        match node.kind() {
            Kind::Identifier => self.emit_identifier_name(node),
            Kind::PrivateIdentifier => self.emit_private_identifier(node),
            _ => panic!("unexpected MemberName: {:?}", node.kind()),
        }
    }

    pub(crate) fn emit_module_name(&mut self, node: Option<P<Node>>) {
        let Some(node) = node else {
            return;
        };

        match node.kind() {
            Kind::Identifier => self.emit_binding_identifier(node),
            Kind::StringLiteral => self.emit_string_literal(node),
            _ => panic!("unexpected ModuleName: {:?}", node.kind()),
        }
    }

    pub(crate) fn emit_module_export_name(&mut self, node: Option<P<Node>>) {
        let Some(node) = node else {
            return;
        };

        match node.kind() {
            Kind::Identifier => self.emit_identifier_name(node),
            Kind::StringLiteral => self.emit_string_literal(node),
            _ => panic!("unexpected ModuleExportName: {:?}", node.kind()),
        }
    }

    pub(crate) fn emit_import_attribute_name(&mut self, node: P<Node>) {
        match node.kind() {
            Kind::Identifier => self.emit_identifier_name(node),
            Kind::StringLiteral => self.emit_string_literal(node),
            _ => panic!("unexpected ImportAttributeName: {:?}", node.kind()),
        }
    }

    pub(crate) fn emit_nested_module_name(&mut self, node: Option<P<Node>>) {
        let Some(node) = node else {
            return;
        };

        match node.kind() {
            Kind::Identifier => self.emit_identifier_name(node),
            Kind::StringLiteral => self.emit_string_literal(node),
            _ => panic!("unexpected ModuleName: {:?}", node.kind()),
        }
    }
}

//
// Signature elements
//

#[derive(Clone, Copy, PartialEq, Eq)]
#[expect(
    clippy::enum_variant_names,
    reason = "The variant names preserve the upstream Go enum names."
)]
enum Mode {
    ModeNone,
    ModeModifiers,
    ModeDecorators,
}

impl Printer {
    pub(crate) fn emit_modifier_list(
        &mut self,
        parent_node: P<Node>,
        modifiers: Option<P<ModifierList>>,
        allow_decorators: bool,
    ) -> TextPos {
        let Some(modifiers) = modifiers.filter(|m| !m.nodes().is_empty()) else {
            return parent_node.pos();
        };
        let modifier_nodes = modifiers.nodes();
        let modifier_list = P::from_static(&modifiers.get().list);

        if modifier_nodes.iter().all(|n| is_modifier(*n)) {
            // if all modifier-likes are `Modifier`, simply emit the list as modifiers.
            self.emit_list(
                |p, n| p.emit_keyword_node(Some(n)),
                parent_node,
                Some(modifier_list),
                ListFormat::Modifiers,
            );
        } else if modifier_nodes.iter().all(|n| is_decorator(*n)) {
            if !allow_decorators {
                return parent_node.pos();
            }

            // if all modifier-likes are `Decorator`, simply emit the list as decorators.
            self.emit_list(
                Printer::emit_modifier_like,
                parent_node,
                Some(modifier_list),
                ListFormat::Decorators,
            );
        } else {
            if let Some(on_before_emit_node_list) =
                &mut self.print_handlers.on_before_emit_node_list
            {
                on_before_emit_node_list(Some(modifier_list));
            }

            // partition modifiers into contiguous chunks of `Modifier` or `Decorator` so as to
            // use consistent formatting for each chunk
            let mut last_mode = Mode::ModeNone;
            let mut mode = Mode::ModeNone;
            let mut start = 0;
            let mut pos = 0;

            let mut last_modifier: Option<P<Node>>;
            while start < modifier_nodes.len() {
                while pos < modifier_nodes.len() {
                    last_modifier = Some(modifier_nodes[pos]);
                    if is_decorator(last_modifier.unwrap()) {
                        mode = Mode::ModeDecorators;
                    } else {
                        mode = Mode::ModeModifiers;
                    }
                    if last_mode == Mode::ModeNone {
                        last_mode = mode;
                    } else if mode != last_mode {
                        break;
                    }
                    pos += 1;
                }

                let mut text_range = TextRange::synthetic();
                if start == 0 {
                    text_range = text_range.with_pos(modifiers.pos());
                }
                if pos == modifier_nodes.len() - 1 {
                    text_range = text_range.with_end(modifiers.end());
                }
                if allow_decorators || last_mode == Mode::ModeModifiers {
                    self.emit_list_items(
                        Printer::emit_modifier_like,
                        Some(parent_node),
                        &modifier_nodes[start..pos],
                        if last_mode == Mode::ModeModifiers {
                            ListFormat::Modifiers
                        } else {
                            ListFormat::Decorators
                        },
                        false, /*hasTrailingComma*/
                        text_range,
                    );
                }
                start = pos;
                last_mode = mode;
                pos += 1;
            }

            if let Some(on_after_emit_node_list) = &mut self.print_handlers.on_after_emit_node_list
            {
                on_after_emit_node_list(Some(modifier_list));
            }
        }

        greatest_end(parent_node.pos(), &[&modifier_nodes.last().copied()])
    }

    pub(crate) fn emit_type_parameter(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_type_parameter_declaration();
        self.emit_modifier_list(node, node.modifiers(), false /*allowDecorators*/);
        self.emit_binding_identifier(n.name());
        if let Some(constraint) = n.constraint() {
            self.write_space();
            self.write_keyword("extends");
            self.write_space();
            self.emit_type_node_outside_extends(constraint);
        }
        if let Some(default_type) = n.default_type() {
            self.write_space();
            self.write_operator("=");
            self.write_space();
            self.emit_type_node_outside_extends(default_type);
        }
        self.exit_node(node, state);
    }

    fn emit_type_parameter_declaration_node(&mut self, node: P<Node>) {
        // NOTE: QuickInfo uses TypeFormatFlagsWriteTypeArgumentsOfSignature to instruct the NodeBuilder to store type arguments
        // (i.e. type nodes) instead of type parameter declarations in the type parameter list.
        if is_type_parameter_declaration(node) {
            self.emit_type_parameter(node);
        } else {
            self.emit_type_argument(node);
        }
    }

    fn emit_parameter_name(&mut self, node: Option<P<Node>>) {
        let saved_write_kind = self.write_kind;
        self.write_kind = WriteKind::Parameter;
        self.emit_binding_name(node);
        self.write_kind = saved_write_kind;
    }

    pub(crate) fn emit_parameter(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_parameter_declaration();
        self.emit_modifier_list(node, node.modifiers(), true /*allowDecorators*/);
        self.emit_token_node(n.dot_dot_dot_token());
        self.emit_parameter_name(Some(n.name()));
        self.emit_token_node(n.question_token());

        self.emit_type_annotation(n.type_());

        // The comment position has to fallback to any present node within the parameter declaration because as it turns
        // out, the parser can make parameter declarations with _just_ an initializer.
        self.emit_initializer(
            n.initializer(),
            greatest_end(
                node.pos(),
                &[&n.type_(), &n.question_token(), &n.name(), &node.modifiers()],
            ),
            node,
        );
        self.exit_node(node, state);
    }

    fn emit_parameter_declaration_node(&mut self, node: P<Node>) {
        self.emit_parameter(node);
    }

    pub(crate) fn emit_decorator(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.write_punctuation("@");
        self.emit_expression(node.as_decorator().expression, OperatorPrecedence::LeftHandSide);
        self.exit_node(node, state);
    }

    fn emit_modifier_like(&mut self, node: P<Node>) {
        if is_decorator(node) {
            self.emit_decorator(node);
        } else if is_modifier(node) {
            self.emit_keyword_node(Some(node));
        } else {
            panic!("unhandled ModifierLike: {:?}", node.kind());
        }
    }

    pub(crate) fn emit_type_parameters(
        &mut self,
        parent_node: P<Node>,
        nodes: Option<P<NodeList>>,
    ) {
        if nodes.is_none() {
            return;
        }
        self.emit_list(
            Printer::emit_type_parameter_declaration_node,
            parent_node,
            nodes,
            ListFormat::TypeParameters
                | if is_arrow_function(parent_node)
                /*p.shouldAllowTrailingComma(parentNode, nodes)*/
                {
                    ListFormat::AllowTrailingComma
                } else {
                    ListFormat::None
                },
        ); // TODO: preserve trailing comma after Strada migration
    }

    pub(crate) fn emit_type_annotation(&mut self, node: Option<P<Node>>) {
        let Some(node) = node else {
            return;
        };

        self.write_punctuation(":");
        self.write_space();
        self.emit_type_node_outside_extends(node);
    }

    pub(crate) fn emit_initializer(
        &mut self,
        node: Option<P<Node>>,
        equal_token_pos: TextPos,
        context_node: P<Node>,
    ) {
        let Some(node) = node else {
            return;
        };

        self.write_space();
        self.emit_token(Kind::EqualsToken, equal_token_pos, WriteKind::Operator, context_node);
        self.write_space();
        self.emit_expression(node, OperatorPrecedence::DISALLOW_COMMA);
    }

    fn emit_parameters(&mut self, parent_node: P<Node>, parameters: Option<P<NodeList>>) {
        self.generate_all_names(parameters);
        self.emit_list(Printer::emit_parameter_declaration_node, parent_node, parameters, ListFormat::Parameters /*|core.IfElse(p.shouldAllowTrailingComma(parentNode, parameters), LFAllowTrailingComma, LFNone)*/); // TODO: preserve trailing comma after Strada migration
    }
}

fn can_emit_simple_arrow_head(parent_node: P<Node>, parameters: P<NodeList>) -> bool {
    // only arrow functions with a single parameter may have simple arrow head
    if !is_arrow_function(parent_node) || parameters.nodes().len() != 1 {
        return false;
    }

    let parent = parent_node.as_arrow_function();
    let parameter_node = parameters.nodes()[0];
    let parameter = parameter_node.as_parameter_declaration();

    parameter_node.pos() == parent_node.pos() // may not have parsed tokens between start of parent and parameter
        && parent.type_parameters().is_none() // parent may not have type parameters
        && parent.type_().is_none() // parent may not have return type annotation
        && (parent_node.modifiers().is_none() || parent_node.modifiers().unwrap().nodes().is_empty()) // parent may not have modifiers
        && !parameters.has_trailing_comma() // parameters may not have a trailing comma
        && parameter_node.modifiers().is_none() // parameter may not have decorators or modifiers
        && parameter.dot_dot_dot_token().is_none() // parameter may not be rest
        && parameter.question_token().is_none() // parameter may not be optional
        && parameter.type_().is_none() // parameter may not have a type annotation
        && parameter.initializer().is_none() // parameter may not have an initializer
        && is_identifier(parameter.name()) // parameter name must be identifier
}

impl Printer {
    pub(crate) fn emit_parameters_for_arrow(
        &mut self,
        parent_node: P<Node>, /*FunctionType | ConstructorType | ArrowFunction*/
        parameters: Option<P<NodeList>>,
    ) {
        if can_emit_simple_arrow_head(parent_node, parameters.unwrap()) {
            self.generate_all_names(parameters);
            self.emit_list(
                Printer::emit_parameter_declaration_node,
                parent_node,
                parameters,
                ListFormat::SingleArrowParameter,
            );
        } else {
            self.emit_parameters(parent_node, parameters);
        }
    }

    fn emit_parameters_for_index_signature(
        &mut self,
        parent_node: P<Node>,
        parameters: Option<P<NodeList>>,
    ) {
        self.generate_all_names(parameters);
        self.emit_list(
            Printer::emit_parameter_declaration_node,
            parent_node,
            parameters,
            ListFormat::IndexSignatureParameters,
        );
    }

    pub(crate) fn emit_signature(&mut self, node: P<Node>) {
        let n = node.function_like_data().unwrap();

        // !!! In old emitter, quickinfo used type arguments in place of type parameters on instantiated signatures
        ////if n.TypeArguments != nil {
        ////	p.emitTypeArguments(node, n.TypeArguments)
        ////} else {
        self.emit_type_parameters(node, n.type_parameters());
        ////}

        self.emit_parameters(node, n.parameters());
        self.emit_type_annotation(n.type_());
    }

    pub(crate) fn emit_function_body(&mut self, body: P<Node>) {
        self.emit_context.add_emit_flags(body, EmitFlags::NoSourceMap);

        // Use only notification hooks for the body block, not the full comment pipeline.
        // Without this, trailing comments from the original method declaration
        // (e.g., "// Error") leak into synthesized comma expressions when methods
        // are hoisted into pending expressions.
        if let Some(on_before_emit_node) = &mut self.print_handlers.on_before_emit_node {
            on_before_emit_node(Some(body));
        }

        self.generate_names(Some(body));

        let statements = body.as_block().statements;

        // !!! Emit with comment after Strada migration
        ////p.emitTokenWithComment(ast.KindOpenBraceToken, body.Pos(), WriteKindPunctuation, body.AsNode())
        self.write_punctuation("{");

        self.increase_indent();
        let detached_state =
            self.emit_detached_comments_before_statement_list(body, statements.loc());
        let statement_offset = self.emit_prologue_directives(statements);
        let pos = self.writer().get_text_pos();
        self.emit_helpers(body);

        if self.should_emit_block_function_body_on_single_line(body)
            && statement_offset == 0
            && pos == self.writer().get_text_pos()
        {
            self.decrease_indent();
            self.emit_list_range(
                Printer::emit_statement,
                Some(body),
                Some(statements),
                ListFormat::SingleLineFunctionBodyStatements,
                statement_offset,
                None,
            );
            self.increase_indent();
        } else {
            self.emit_list_range(
                Printer::emit_statement,
                Some(body),
                Some(statements),
                ListFormat::MultiLineFunctionBodyStatements,
                statement_offset,
                None,
            );
        }

        self.emit_detached_comments_after_statement_list(body, statements.loc(), detached_state);
        self.decrease_indent();

        // !!! Emit comment after Strada migration
        ////p.emitTokenEx(ast.KindCloseBraceToken, body.Statements.End(), WriteKindPunctuation, body.AsNode(), tefNone)
        self.emit_token_ex(
            Kind::CloseBraceToken,
            statements.end(),
            WriteKind::Punctuation,
            body,
            tokenEmitFlags::NoComments,
        );

        if let Some(on_after_emit_node) = &mut self.print_handlers.on_after_emit_node {
            on_after_emit_node(Some(body));
        }
    }

    pub(crate) fn emit_function_body_node(&mut self, node: Option<P<Node>>) {
        let Some(node) = node else {
            self.write_trailing_semicolon();
            return;
        };

        self.write_space();
        self.emit_function_body(node);
    }
}

//
// Type Members
//

impl Printer {
    pub(crate) fn emit_property_signature(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_property_signature_declaration();
        self.emit_modifier_list(node, node.modifiers(), false /*allowDecorators*/);
        self.emit_property_name(Some(n.name()));
        self.emit_token_node(n.postfix_token());
        self.emit_type_annotation(n.type_());
        self.write_trailing_semicolon();
        self.exit_node(node, state);
    }

    pub(crate) fn emit_property_declaration(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_property_declaration();
        self.emit_modifier_list(node, node.modifiers(), true /*allowDecorators*/);
        self.emit_property_name(Some(n.name()));
        self.emit_token_node(n.postfix_token());
        self.emit_type_annotation(n.type_());
        self.emit_initializer(
            n.initializer(),
            greatest_end(n.name().end(), &[&n.type_(), &n.postfix_token()]),
            node,
        );
        self.write_trailing_semicolon();
        self.exit_node(node, state);
    }

    pub(crate) fn emit_method_signature(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_method_signature_declaration();
        self.emit_modifier_list(node, node.modifiers(), false /*allowDecorators*/);
        self.emit_property_name(Some(n.name()));
        self.emit_token_node(n.postfix_token());
        let indented = self.should_emit_indented(node);
        self.increase_indent_if(indented);
        self.push_name_generation_scope(Some(node));
        self.emit_signature(node);
        self.write_trailing_semicolon();
        self.pop_name_generation_scope(Some(node));
        self.decrease_indent_if(indented);
        self.exit_node(node, state);
    }

    pub(crate) fn emit_method_declaration(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_method_declaration();
        self.emit_modifier_list(node, node.modifiers(), true /*allowDecorators*/);
        self.emit_token_node(n.asterisk_token());
        self.emit_property_name(Some(n.name()));
        self.emit_token_node(n.postfix_token());
        let indented = self.should_emit_indented(node);
        self.increase_indent_if(indented);
        self.push_name_generation_scope(Some(node));
        self.emit_signature(node);
        self.emit_function_body_node(n.body());
        self.pop_name_generation_scope(Some(node));
        self.decrease_indent_if(indented);
        self.exit_node(node, state);
    }

    pub(crate) fn emit_class_static_block_declaration(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.write_keyword("static");
        self.push_name_generation_scope(Some(node));
        self.emit_function_body_node(Some(node.as_class_static_block_declaration().body()));
        self.pop_name_generation_scope(Some(node));
        self.exit_node(node, state);
    }

    pub(crate) fn emit_constructor(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_modifier_list(node, node.modifiers(), false /*allowDecorators*/);
        self.write_keyword("constructor");
        let indented = self.should_emit_indented(node);
        self.increase_indent_if(indented);
        self.push_name_generation_scope(Some(node));
        self.emit_signature(node);
        self.emit_function_body_node(node.as_constructor_declaration().body());
        self.pop_name_generation_scope(Some(node));
        self.decrease_indent_if(indented);
        self.exit_node(node, state);
    }

    fn emit_accessor_declaration(&mut self, token: Kind, node: P<Node>) {
        let state = self.enter_node(node);
        let pos = self.emit_modifier_list(node, node.modifiers(), true /*allowDecorators*/);
        self.emit_token(token, pos, WriteKind::Keyword, node);
        self.write_space();
        self.emit_property_name(node.name());
        let indented = self.should_emit_indented(node);
        self.increase_indent_if(indented);
        self.push_name_generation_scope(Some(node));
        self.emit_signature(node);
        self.emit_function_body_node(node.body());
        self.pop_name_generation_scope(Some(node));
        self.decrease_indent_if(indented);
        self.exit_node(node, state);
    }

    pub(crate) fn emit_get_accessor_declaration(&mut self, node: P<Node>) {
        self.emit_accessor_declaration(Kind::GetKeyword, node);
    }

    pub(crate) fn emit_set_accessor_declaration(&mut self, node: P<Node>) {
        self.emit_accessor_declaration(Kind::SetKeyword, node);
    }

    pub(crate) fn emit_call_signature(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let indented = self.should_emit_indented(node);
        self.increase_indent_if(indented);
        self.push_name_generation_scope(Some(node));
        self.emit_signature(node);
        self.write_trailing_semicolon();
        self.pop_name_generation_scope(Some(node));
        self.decrease_indent_if(indented);
        self.exit_node(node, state);
    }

    pub(crate) fn emit_construct_signature(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.write_keyword("new");
        self.write_space();
        let indented = self.should_emit_indented(node);
        self.increase_indent_if(indented);
        self.push_name_generation_scope(Some(node));
        self.emit_signature(node);
        self.write_trailing_semicolon();
        self.pop_name_generation_scope(Some(node));
        self.decrease_indent_if(indented);
        self.exit_node(node, state);
    }

    pub(crate) fn emit_index_signature(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_index_signature_declaration();
        self.emit_modifier_list(node, node.modifiers(), false /*allowDecorators*/);
        let indented = self.should_emit_indented(node);
        self.increase_indent_if(indented);
        self.push_name_generation_scope(Some(node));
        self.emit_parameters_for_index_signature(node, n.parameters());
        self.emit_type_annotation(n.type_());
        self.write_trailing_semicolon();
        self.pop_name_generation_scope(Some(node));
        self.decrease_indent_if(indented);
        self.exit_node(node, state);
    }

    pub(crate) fn emit_class_element(&mut self, node: P<Node>) {
        match node.kind() {
            Kind::PropertyDeclaration => self.emit_property_declaration(node),
            Kind::MethodDeclaration => self.emit_method_declaration(node),
            Kind::ClassStaticBlockDeclaration => self.emit_class_static_block_declaration(node),
            Kind::Constructor => self.emit_constructor(node),
            Kind::GetAccessor => self.emit_get_accessor_declaration(node),
            Kind::SetAccessor => self.emit_set_accessor_declaration(node),
            Kind::IndexSignature => self.emit_index_signature(node),
            Kind::SemicolonClassElement => self.emit_semicolon_class_element(node),
            Kind::NotEmittedStatement => self.emit_not_emitted_statement(node),
            Kind::JSTypeAliasDeclaration => self.emit_type_alias_declaration(node),
            _ => panic!("unexpected ClassElement: {:?}", node.kind()),
        }
    }

    pub(crate) fn emit_type_element(&mut self, node: P<Node>) {
        match node.kind() {
            Kind::PropertySignature => self.emit_property_signature(node),
            Kind::MethodSignature => self.emit_method_signature(node),
            Kind::CallSignature => self.emit_call_signature(node),
            Kind::ConstructSignature => self.emit_construct_signature(node),
            Kind::GetAccessor => self.emit_get_accessor_declaration(node),
            Kind::SetAccessor => self.emit_set_accessor_declaration(node),
            Kind::IndexSignature => self.emit_index_signature(node),
            Kind::NotEmittedTypeElement => self.emit_not_emitted_type_element(node),
            _ => panic!("unexpected TypeElement: {:?}", node.kind()),
        }
    }

    pub(crate) fn emit_object_literal_element(&mut self, node: P<Node>) {
        match node.kind() {
            Kind::PropertyAssignment => self.emit_property_assignment(node),
            Kind::ShorthandPropertyAssignment => self.emit_shorthand_property_assignment(node),
            Kind::SpreadAssignment => self.emit_spread_assignment(node),
            Kind::MethodDeclaration => self.emit_method_declaration(node),
            Kind::GetAccessor => self.emit_get_accessor_declaration(node),
            Kind::SetAccessor => self.emit_set_accessor_declaration(node),
            _ => panic!("unhandled ObjectLiteralElement: {:?}", node.kind()),
        }
    }
}

//
// Types
//

impl Printer {
    fn emit_keyword_type_node(&mut self, node: P<Node>) {
        self.emit_keyword_node(Some(node));
    }

    fn emit_type_predicate_parameter_name(&mut self, node: P<Node>) {
        match node.kind() {
            Kind::Identifier => self.emit_identifier_reference(node),
            Kind::ThisType => self.emit_this_type(node),
            _ => panic!("unexpected TypePredicateParameterName: {:?}", node.kind()),
        }
    }

    fn emit_type_predicate(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_type_predicate_node();
        if n.asserts_modifier().is_some() {
            self.emit_token_node(n.asserts_modifier());
            self.write_space();
        }
        self.emit_type_predicate_parameter_name(n.parameter_name());
        if let Some(type_) = n.type_() {
            self.write_space();
            self.write_keyword("is");
            self.write_space();
            self.emit_type_node_outside_extends(type_);
        }
        self.exit_node(node, state);
    }

    fn emit_type_argument(&mut self, node: P<Node>) {
        self.emit_type_node_outside_extends(node);
    }

    pub(crate) fn emit_type_arguments(&mut self, parent_node: P<Node>, nodes: Option<P<NodeList>>) {
        if nodes.is_none() {
            return;
        }
        self.emit_list(Printer::emit_type_parameter_declaration_node, parent_node, nodes, ListFormat::TypeArguments /*|core.IfElse(p.shouldAllowTrailingComma(parentNode, nodes), LFAllowTrailingComma, LFNone)*/); // TODO: preserve trailing comma after Strada migration
    }

    pub(crate) fn emit_type_reference(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_type_reference_node();
        self.emit_entity_name(n.type_name());
        self.emit_type_arguments(node, n.type_arguments());
        self.exit_node(node, state);
    }

    // Emits the return type of a FunctionTypeNode or ConstructorTypeNode, including the arrow (`=>`)
    fn emit_return_type(&mut self, node: Option<P<Node>>) {
        let Some(node) = node else {
            return;
        };
        self.write_punctuation("=>");
        self.write_space();
        if self.in_extends
            && node.kind() == Kind::InferType
            && node
                .as_infer_type_node()
                .type_parameter()
                .as_type_parameter_declaration()
                .constraint()
                .is_some()
        {
            // if the parent FunctionTypeNode or ConstructorTypeNode is in the `extends` clause of a ConditionalTypeNode,
            // we must parenthesize `infer ... extends ...` so as not to result in an ambiguous parse.
            //
            // `T extends () => infer U extends V ? W : X` would parse the `? W : X` as part of a ConditionalTypeNode in the
            // return type of the FunctionTypeNode, thus we must emit as `T extends () => (infer U extends V) ? W : X`
            self.emit_type_node_preserving_extends(node, TypePrecedence::HIGHEST);
        } else {
            self.emit_type_node_preserving_extends(node, TypePrecedence::LOWEST);
        }
    }

    fn emit_function_type(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_function_type_node();
        let indented = self.should_emit_indented(node);
        self.increase_indent_if(indented);
        self.push_name_generation_scope(Some(node));
        // !!! in the old emitter, quickinfo uses type arguments in place of type parameters for instantiated signatures
        self.emit_type_parameters(node, n.type_parameters());
        self.emit_parameters(node, n.parameters());
        self.write_space();
        self.emit_return_type(n.type_());
        self.pop_name_generation_scope(Some(node));
        self.decrease_indent_if(indented);
        self.exit_node(node, state);
    }

    fn emit_constructor_type(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_constructor_type_node();
        self.emit_modifier_list(node, node.modifiers(), false /*allowDecorators*/);
        self.write_keyword("new");
        self.write_space();
        let indented = self.should_emit_indented(node);
        self.increase_indent_if(indented);
        self.push_name_generation_scope(Some(node));
        // !!! in the old emitter, quickinfo uses type arguments in place of type parameters for instantiated signatures
        self.emit_type_parameters(node, n.type_parameters());
        self.emit_parameters(node, n.parameters());
        self.write_space();
        self.emit_return_type(n.type_());
        self.pop_name_generation_scope(Some(node));
        self.decrease_indent_if(indented);
        self.exit_node(node, state);
    }

    fn emit_type_query(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_type_query_node();
        self.write_keyword("typeof");
        self.write_space();
        self.emit_entity_name(n.expr_name());
        self.emit_type_arguments(node, n.type_arguments());
        self.exit_node(node, state);
    }

    fn emit_type_literal(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let members = node.as_type_literal_node().members();
        self.push_name_generation_scope(Some(node));
        self.generate_all_member_names(Some(members));
        self.write_punctuation("{");
        let flags = if self.should_emit_on_single_line(node) {
            ListFormat::SingleLineTypeLiteralMembers
        } else {
            ListFormat::MultiLineTypeLiteralMembers
        };
        self.emit_list(
            Printer::emit_type_element,
            node,
            Some(members),
            flags | ListFormat::NoSpaceIfEmpty,
        );
        self.write_punctuation("}");
        self.pop_name_generation_scope(Some(node));
        self.exit_node(node, state);
    }

    fn emit_array_type(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_postfix_type_operand(node.as_array_type_node().element_type(), node);
        self.write_punctuation("[");
        self.write_punctuation("]");
        self.exit_node(node, state);
    }

    // emitPostfixTypeOperand emits the operand of a postfix type (ArrayType, IndexedAccessType,
    // OptionalType). It is equivalent to `emitTypeNode(operand, TypePrecedencePostfix)` except
    // that it preserves a parsed `typeof X` operand without adding parentheses (e.g.,
    // `typeof C[K]` instead of `(typeof C)[K]`). TypeScript's `parenthesizeNonArrayTypeOfPostfixType`
    // factory rule wraps `TypeQuery` in `ParenthesizedType` only when a postfix type is constructed
    // via the factory, so parsed postfix types preserve the source as written during round-trip
    // emit while synthesized postfix types (e.g., from declaration emit) still get the parentheses.
    fn emit_postfix_type_operand(&mut self, operand: P<Node>, parent: P<Node>) {
        if is_parse_tree_node(parent) && operand.kind() == Kind::TypeQuery {
            self.emit_type_node(operand, TypePrecedence::TypeOperator);
            return;
        }
        self.emit_type_node(operand, TypePrecedence::Postfix);
    }

    fn emit_tuple_element_type(&mut self, node: P<Node>) {
        self.emit_type_node_outside_extends(node);
    }

    fn emit_tuple_type(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let elements = node.as_tuple_type_node().elements();
        self.emit_token(Kind::OpenBracketToken, node.pos(), WriteKind::Punctuation, node);
        let flags = if self.should_emit_on_single_line(node) {
            ListFormat::SingleLineTupleTypeElements
        } else {
            ListFormat::MultiLineTupleTypeElements
        };
        self.emit_list(
            Printer::emit_tuple_element_type,
            node,
            Some(elements),
            flags | ListFormat::NoSpaceIfEmpty,
        );
        self.emit_token(Kind::CloseBracketToken, elements.end(), WriteKind::Punctuation, node);
        self.exit_node(node, state);
    }

    fn emit_rest_type(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.write_punctuation("...");
        self.emit_type_node_outside_extends(node.as_rest_type_node().type_());
        self.exit_node(node, state);
    }

    fn emit_optional_type(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        // !!! May need extra parenthesization if we also have JSDocNullableType
        self.emit_postfix_type_operand(node.as_optional_type_node().type_(), node);
        self.write_punctuation("?");
        self.exit_node(node, state);
    }

    fn emit_named_tuple_member(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_named_tuple_member();
        self.emit_punctuation_node(n.dot_dot_dot_token());
        self.emit_identifier_name(n.name());
        self.emit_punctuation_node(n.question_token());
        self.emit_token(
            Kind::ColonToken,
            greatest_end(n.name().end(), &[&n.question_token()]),
            WriteKind::Punctuation,
            node,
        );
        self.write_space();
        self.emit_type_node_outside_extends(n.type_());
        self.exit_node(node, state);
    }

    fn emit_union_type_constituent(&mut self, node: P<Node>) {
        self.emit_type_node(node, TypePrecedence::TypeOperator);
    }

    fn emit_union_type(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_list(
            Printer::emit_union_type_constituent,
            node,
            Some(node.as_union_type_node().types()),
            ListFormat::UnionTypeConstituents,
        );
        self.exit_node(node, state);
    }

    fn emit_intersection_type_constituent(&mut self, node: P<Node>) {
        self.emit_type_node(node, TypePrecedence::TypeOperator);
    }

    fn emit_intersection_type(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_list(Printer::emit_intersection_type_constituent, node, Some(node.as_intersection_type_node().types()), ListFormat::IntersectionTypeConstituents /*, parenthesizer.parenthesizeConstituentTypeOfIntersectionType*/); // !!!
        self.exit_node(node, state);
    }

    fn emit_conditional_type(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_conditional_type_node();
        self.emit_type_node(n.check_type(), TypePrecedence::Union);
        self.write_space();
        self.write_keyword("extends");
        self.write_space();
        self.emit_type_node_in_extends(n.extends_type());
        self.write_space();
        self.write_punctuation("?");
        self.write_space();
        self.emit_type_node_outside_extends(n.true_type());
        self.write_space();
        self.write_punctuation(":");
        self.write_space();
        self.emit_type_node_outside_extends(n.false_type());
        self.exit_node(node, state);
    }

    fn emit_infer_type_parameter(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_type_parameter_declaration();
        self.emit_binding_identifier(n.name());
        if let Some(constraint) = n.constraint() {
            self.write_space();
            self.write_keyword("extends");
            self.write_space();
            self.emit_type_node_in_extends(constraint);
        }
        self.exit_node(node, state);
    }

    fn emit_infer_type(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.write_keyword("infer");
        self.write_space();
        self.emit_infer_type_parameter(node.as_infer_type_node().type_parameter());
        self.exit_node(node, state);
    }

    fn emit_parenthesized_type(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.write_punctuation("(");
        self.emit_type_node_outside_extends(node.as_parenthesized_type_node().type_());
        self.write_punctuation(")");
        self.exit_node(node, state);
    }

    fn emit_this_type(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.write_keyword("this");
        self.exit_node(node, state);
    }

    fn emit_type_operator(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_type_operator_node();
        self.emit_token(n.operator, node.pos(), WriteKind::Keyword, node);
        self.write_space();
        self.emit_type_node(
            n.type_(),
            if n.operator == Kind::ReadonlyKeyword {
                TypePrecedence::Postfix
            } else {
                TypePrecedence::TypeOperator
            },
        );
        self.exit_node(node, state);
    }

    fn emit_indexed_access_type(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_indexed_access_type_node();
        self.emit_postfix_type_operand(n.object_type(), node);
        self.write_punctuation("[");
        self.emit_type_node_outside_extends(n.index_type());
        self.write_punctuation("]");
        self.exit_node(node, state);
    }

    fn emit_mapped_type_parameter(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_type_parameter_declaration();
        self.emit_binding_identifier(n.name());
        self.write_space();
        self.write_keyword("in");
        self.write_space();
        self.emit_type_node_outside_extends(n.constraint().unwrap());
        self.exit_node(node, state);
    }

    fn emit_mapped_type(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_mapped_type_node();
        let single_line = self.should_emit_on_single_line(node);
        self.write_punctuation("{");
        if single_line {
            self.write_space();
        } else {
            self.write_line();
            self.increase_indent();
        }
        if let Some(readonly_token) = n.readonly_token() {
            self.emit_token_node(Some(readonly_token));
            if readonly_token.kind() != Kind::ReadonlyKeyword {
                self.write_keyword("readonly");
            }
            self.write_space();
        }
        self.write_punctuation("[");
        self.emit_mapped_type_parameter(n.type_parameter());
        if let Some(name_type) = n.name_type() {
            self.write_space();
            self.write_keyword("as");
            self.write_space();
            self.emit_type_node_outside_extends(name_type);
        }
        self.write_punctuation("]");
        if let Some(question_token) = n.question_token() {
            self.emit_punctuation_node(Some(question_token));
            if question_token.kind() != Kind::QuestionToken {
                self.write_punctuation("?");
            }
        }
        if let Some(type_) = n.type_() {
            self.write_punctuation(":");
            self.write_space();
            self.emit_type_node_outside_extends(type_);
        }
        self.write_trailing_semicolon();
        if let Some(members) = n.members()
            && !members.nodes().is_empty()
        {
            if single_line {
                self.write_space();
            } else {
                self.write_line();
            }
            self.emit_list(
                Printer::emit_type_element,
                node,
                Some(members),
                ListFormat::PreserveLines,
            );
        }
        if single_line {
            self.write_space();
        } else {
            self.write_line();
            self.decrease_indent();
        }
        self.write_punctuation("}");
        self.exit_node(node, state);
    }

    fn emit_literal_type(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_expression(node.as_literal_type_node().literal(), OperatorPrecedence::Comma);
        self.exit_node(node, state);
    }

    fn emit_template_type_span(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_template_literal_type_span();
        self.emit_type_node_outside_extends(n.type_());
        self.emit_template_middle_tail(n.literal());
        self.exit_node(node, state);
    }

    fn emit_template_type_span_node(&mut self, node: P<Node>) {
        self.emit_template_type_span(node);
    }

    fn emit_template_type(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_template_literal_type_node();
        self.emit_template_head(n.head());
        self.emit_list(
            Printer::emit_template_type_span_node,
            node,
            Some(n.template_spans()),
            ListFormat::TemplateExpressionSpans,
        );
        self.exit_node(node, state);
    }

    fn emit_import_type_node_attributes(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_import_attributes();
        self.write_punctuation("{");
        self.write_space();
        self.write_keyword(if n.token == Kind::AssertKeyword { "assert" } else { "with" });
        self.write_punctuation(":");
        self.write_space();
        self.emit_list(
            Printer::emit_import_attribute_node,
            node,
            Some(n.attributes()),
            ListFormat::ImportAttributes,
        );
        self.write_space();
        self.write_punctuation("}");
        self.exit_node(node, state);
    }

    fn emit_import_type_node(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_import_type_node();
        if n.is_type_of {
            self.write_keyword("typeof");
            self.write_space();
        }
        self.write_keyword("import");
        self.write_punctuation("(");
        self.emit_type_node_outside_extends(n.argument());
        if let Some(attributes) = n.attributes() {
            self.write_punctuation(",");
            self.write_space();
            self.emit_import_type_node_attributes(attributes);
        }
        self.write_punctuation(")");
        if let Some(qualifier) = n.qualifier() {
            self.write_punctuation(".");
            self.emit_entity_name(qualifier);
        }
        self.emit_type_arguments(node, n.type_arguments());
        self.exit_node(node, state);
    }

    // emits a Type node in the `extends` clause of a ConditionalType
    fn emit_type_node_in_extends(&mut self, node: P<Node>) {
        let saved_in_extends = self.in_extends;
        self.in_extends = true;
        self.emit_type_node_preserving_extends(node, TypePrecedence::LOWEST);
        self.in_extends = saved_in_extends;
    }

    // emits a Type node not in the `extends` clause of a ConditionalType or InferType
    pub(crate) fn emit_type_node_outside_extends(&mut self, node: P<Node>) {
        let saved_in_extends = self.in_extends;
        self.in_extends = false;
        self.emit_type_node_preserving_extends(node, TypePrecedence::LOWEST);
        self.in_extends = saved_in_extends;
    }

    // emits a Type node preserving whether or not we are currently in the `extends` clause of a ConditionalType or InferType
    fn emit_type_node_preserving_extends(&mut self, node: P<Node>, precedence: TypePrecedence) {
        self.emit_type_node(node, precedence);
    }

    pub(crate) fn emit_type_node(&mut self, node: P<Node>, precedence: TypePrecedence) {
        let mut precedence = precedence;
        if self.in_extends && precedence <= TypePrecedence::Conditional {
            // in the `extends` clause of a ConditionalType or InferType, a ConditionalType must be parenthesized
            precedence = TypePrecedence::Function;
        }

        let saved_in_extends = self.in_extends;
        let parens = get_type_node_precedence(node) < precedence;
        if parens {
            self.in_extends = false;
            self.write_punctuation("(");
        }

        match node.kind() {
            // Keyword Types
            Kind::AnyKeyword
            | Kind::UnknownKeyword
            | Kind::NumberKeyword
            | Kind::BigIntKeyword
            | Kind::ObjectKeyword
            | Kind::BooleanKeyword
            | Kind::StringKeyword
            | Kind::SymbolKeyword
            | Kind::VoidKeyword
            | Kind::UndefinedKeyword
            | Kind::NeverKeyword
            | Kind::IntrinsicKeyword => self.emit_keyword_type_node(node),

            // Types
            Kind::TypePredicate => self.emit_type_predicate(node),
            Kind::TypeReference => self.emit_type_reference(node),
            Kind::FunctionType => self.emit_function_type(node),
            Kind::ConstructorType => self.emit_constructor_type(node),
            Kind::TypeQuery => self.emit_type_query(node),
            Kind::TypeLiteral => self.emit_type_literal(node),
            Kind::ArrayType => self.emit_array_type(node),
            Kind::TupleType => self.emit_tuple_type(node),
            Kind::OptionalType => self.emit_optional_type(node),
            Kind::RestType => self.emit_rest_type(node),
            Kind::UnionType => self.emit_union_type(node),
            Kind::IntersectionType => self.emit_intersection_type(node),
            Kind::ConditionalType => self.emit_conditional_type(node),
            Kind::InferType => self.emit_infer_type(node),
            Kind::ParenthesizedType => self.emit_parenthesized_type(node),
            Kind::ThisType => self.emit_this_type(node),
            Kind::TypeOperator => self.emit_type_operator(node),
            Kind::IndexedAccessType => self.emit_indexed_access_type(node),
            Kind::MappedType => self.emit_mapped_type(node),
            Kind::LiteralType => self.emit_literal_type(node),
            Kind::NamedTupleMember => self.emit_named_tuple_member(node),
            Kind::TemplateLiteralType => self.emit_template_type(node),
            Kind::TemplateLiteralTypeSpan => self.emit_template_type_span(node),
            Kind::ImportType => self.emit_import_type_node(node),

            Kind::PropertyAccessExpression => {
                // Occurs in pseudo-types such as `f<T>.C`, where `f` is a generic function and `C` is a local type
                self.emit_property_access_expression(node);
            }
            Kind::ExpressionWithTypeArguments => {
                // !!! Should this actually be considered a type?
                self.emit_expression_with_type_arguments(node);
            }

            Kind::JSDocAllType => self.emit_jsdoc_all_type(node),
            Kind::JSDocNonNullableType => self.emit_jsdoc_non_nullable_type(node),
            Kind::JSDocNullableType => self.emit_jsdoc_nullable_type(node),
            Kind::JSDocOptionalType => self.emit_jsdoc_optional_type(node),
            Kind::JSDocVariadicType => self.emit_jsdoc_variadic_type(node),

            _ => panic!("unhandled TypeNode: {:?}", node.kind()),
        }

        if parens {
            self.write_punctuation(")");
        }

        self.in_extends = saved_in_extends;
    }
}

//
// Binding patterns
//

impl Printer {
    pub(crate) fn emit_object_binding_pattern(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.write_punctuation("{");
        self.emit_list(
            Printer::emit_binding_element_node,
            node,
            Some(node.as_binding_pattern().elements()),
            ListFormat::ObjectBindingPatternElements,
        );
        self.write_punctuation("}");
        self.exit_node(node, state);
    }

    pub(crate) fn emit_array_binding_pattern(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.write_punctuation("[");
        self.emit_list(
            Printer::emit_binding_element_node,
            node,
            Some(node.as_binding_pattern().elements()),
            ListFormat::ArrayBindingPatternElements,
        );
        self.write_punctuation("]");
        self.exit_node(node, state);
    }

    pub(crate) fn emit_binding_element(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        let n = node.as_binding_element();
        self.emit_token_node(n.dot_dot_dot_token());
        if let Some(property_name) = n.property_name() {
            self.emit_property_name(Some(property_name));
            self.write_punctuation(":");
            self.write_space();
        }
        // Old parser used `OmittedExpression` as a substitute for `Elision`. New parser uses a `BindingElement` with nil members
        if let Some(name) = n.name() {
            self.emit_binding_name(Some(name));
            self.emit_initializer(n.initializer(), name.end(), node);
        }
        self.exit_node(node, state);
    }

    fn emit_binding_element_node(&mut self, node: P<Node>) {
        self.emit_binding_element(node);
    }

    fn emit_jsdoc_all_type(&mut self, node: P<Node>) {
        self.emit_keyword_node(Some(node));
    }

    fn emit_jsdoc_non_nullable_type(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.write_punctuation("!");
        self.emit_type_node(node.as_jsdoc_non_nullable_type().type_(), TypePrecedence::NonArray);
        self.exit_node(node, state);
    }

    fn emit_jsdoc_nullable_type(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.write_punctuation("?");
        self.emit_type_node(node.as_jsdoc_nullable_type().type_(), TypePrecedence::NonArray);
        self.exit_node(node, state);
    }

    fn emit_jsdoc_optional_type(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.emit_type_node(node.as_jsdoc_optional_type().type_(), TypePrecedence::JSDoc);
        self.write_punctuation("=");
        self.exit_node(node, state);
    }

    fn emit_jsdoc_variadic_type(&mut self, node: P<Node>) {
        let state = self.enter_node(node);
        self.write_punctuation("...");
        self.emit_type_node(node.as_jsdoc_variadic_type().type_(), TypePrecedence::JSDoc);
        self.exit_node(node, state);
    }
}
