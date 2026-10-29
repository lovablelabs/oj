use std::cell::{Cell, RefCell};

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::{
    self as ast, CheckFlags, Kind, Node, NodeFlags, SemanticMeaning, SourceFile, Symbol,
    SymbolFlags,
};
use tsrs_checker::{
    self as checker, Checker, ContextFlags, Flags, InternalFlags, Signature, SignatureFlags,
    SignatureKind, SymbolFormatFlags, Type, TypeFlags, TypeFormatFlags, VerbosityContext,
};
use tsrs_core::context::Context;
use tsrs_core::{NewLineKind, P, TextRange};
use tsrs_lsproto as lsproto;
use tsrs_printer::{self as printer, EmitTextWriter};
use tsrs_scanner as scanner;

use crate::astnav;
use crate::definition::get_declarations_from_location;
use crate::displaypartswriter::{DisplayPartsWriter, new_display_parts_writer};
use crate::findallreferences::get_range_of_node;
use crate::hovericon::{build_vs_hover_raw_content, get_vs_hover_image_id};
use crate::jsdoc::get_jsdoc_or_tag;
use crate::languageservice::LanguageService;
use crate::lsutil::{self, ScriptElementKind, ScriptElementKindModifier};
use crate::spanmap::{Feature, Fidelity};
use crate::utilities::{
    create_range_from_node, get_container_node, get_containing_object_literal_element,
    get_meaning_from_location, is_in_comment,
};

// hover.go:22
const SYMBOL_FORMAT_FLAGS: SymbolFormatFlags = SymbolFormatFlags::WriteTypeParametersOrArguments
    .union(SymbolFormatFlags::UseOnlyExternalAliasing)
    .union(SymbolFormatFlags::AllowAnyNodeKind)
    .union(SymbolFormatFlags::UseAliasDefinedOutsideCurrentScope);
const TYPE_FORMAT_FLAGS: TypeFormatFlags = TypeFormatFlags::UseAliasDefinedOutsideCurrentScope
    .union(TypeFormatFlags::UseInstantiationExpressions);

fn new_verbosity_context(level: usize, max_truncation_length: usize) -> P<VerbosityContext> {
    P::new(VerbosityContext {
        level: Cell::new(level),
        max_truncation_length: Cell::new(max_truncation_length),
        can_increase_verbosity: Cell::new(false),
        truncated: Cell::new(false),
    })
}

impl LanguageService {
    // hover.go:27
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Language service operations share the fallible response signature used by LSP and cross-project dispatch."
    )]
    pub fn provide_hover(
        &self,
        ctx: &Context,
        params: &lsproto::HoverParams,
    ) -> Result<lsproto::HoverResponse, lsproto::Error> {
        let caps = lsproto::get_client_capabilities(ctx);
        let content_format =
            lsproto::preferred_markup_kind(&caps.text_document.hover.content_format);

        let verbosity_level =
            params.verbosity_level.and_then(|level| usize::try_from(level).ok()).unwrap_or(0);

        let (program, mut file) = self.get_program_and_file(&params.text_document.uri);
        let positions = self.converters.from_lsp_position_for_source_file(
            file,
            params.position,
            Feature::Hover,
        );
        let mut hovers: Vec<lsproto::Hover> = Vec::new();
        for projection in positions {
            if !projection.fidelity.is_single_segment() {
                continue;
            }
            file = projection.script;
            let position = projection.position;
            let node = astnav::get_touching_property_name(file, position);
            if ast::is_source_file(node)
                || ast::is_property_access_or_qualified_name(node)
                    && is_in_comment(file, position, node).is_none()
            {
                // Avoid giving quickInfo for the sourceFile as a whole or inside the comment of a/**/.b
                continue;
            }
            let mut c = program.get_type_checker_for_file(ctx, file);
            let range_node = get_node_for_quick_info(node);
            let symbol = get_symbol_at_location_for_quick_info(&mut c, range_node);

            // Always create VerbosityContext for hover so that canExpandSymbol can signal
            // canIncreaseVerbosity even at Level 0. The nodebuilder also detects expandable
            // types at Level 0 via shouldExpandType (maxExpansionDepth = 0).
            let max_trunc_len = usize::try_from(self.user_preferences().maximum_hover_length)
                .ok()
                .filter(|&length| length > 0)
                .unwrap_or(500);
            let vc = new_verbosity_context(verbosity_level, max_trunc_len);

            let vs_capability = caps.vs_supports_visual_studio_extensions;
            let (quick_info, documentation, vs_documentation, quick_info_runs) = self
                .get_quick_info_and_documentation_for_symbol(
                    &mut c,
                    symbol,
                    range_node,
                    content_format,
                    Some(vc),
                    vs_capability,
                );
            if quick_info.is_empty() {
                drop(c);
                continue;
            }
            let range_file = ast::get_source_file_of_node(range_node).unwrap();
            let text_range = get_range_of_node(range_node, Some(range_file), None /*endNode*/);
            let (hover_range, hover_fidelity) =
                self.converters.to_lsp_range_for_feature(&range_file, text_range, Feature::Hover);

            let content = if content_format == lsproto::MarkupKind::MARKDOWN {
                format_quick_info(&quick_info) + &documentation
            } else {
                quick_info + &documentation
            };

            let mut hover = lsproto::Hover {
                contents: lsproto::MarkupContentOrStringOrMarkedStringWithLanguageOrMarkedStrings {
                    markup_content: Some(lsproto::MarkupContent {
                        kind: content_format,
                        value: content,
                    }),
                    ..Default::default()
                },
                ..Default::default()
            };
            if hover_fidelity.is_single_segment() {
                hover.range = Some(hover_range);
            }

            if caps.experimental.hover_verbosity_level {
                hover.can_increase_verbosity =
                    vc.can_increase_verbosity.get() && !vc.truncated.get();
            }

            // Clients that support Visual Studio extensions (e.g. VS itself, when Corsa/Native TS Preview is
            // enabled) render `_vs_rawContent` in place of `contents`. Without it, VS shows plain markdown
            // with no symbol icon and no syntax coloring, unlike the legacy TSServer-backed hover path.
            if vs_capability && !quick_info_runs.is_empty() {
                let mut kind = ScriptElementKind::Keyword;
                let mut modifiers = ScriptElementKindModifier::None;
                if let Some(symbol) = symbol {
                    // Resolve aliases to their target before computing the icon kind, so e.g. `import { x }`
                    // shows the icon for whatever `x` actually is (const, function, ...) rather than a
                    // generic alias icon. GetSymbolModifiers already accounts for the alias target itself.
                    let mut icon_symbol = symbol;
                    if symbol.flags().intersects(SymbolFlags::Alias) {
                        let resolved = c.get_aliased_symbol(symbol);
                        if resolved != symbol {
                            icon_symbol = resolved;
                        }
                    }
                    kind = lsutil::get_symbol_kind(Some(&mut c), icon_symbol, range_node);
                    modifiers = lsutil::get_symbol_modifiers(Some(&mut c), Some(symbol));
                }
                let image_id = get_vs_hover_image_id(kind, modifiers);
                let mut documentation_runs: Vec<lsproto::VSClassifiedTextRun> = Vec::new();
                let doc_text = vs_documentation.trim_start_matches('\n');
                if !doc_text.is_empty() {
                    documentation_runs = vec![lsproto::VSClassifiedTextRun {
                        classification_type_name: lsproto::ClassificationTypeName::TEXT
                            .0
                            .to_string(),
                        text: doc_text.to_string(),
                        ..Default::default()
                    }];
                }
                hover.vs_raw_content =
                    build_vs_hover_raw_content(image_id, quick_info_runs, documentation_runs);
            }

            drop(c);
            hovers.push(hover);
        }
        if hovers.is_empty() {
            return Ok(lsproto::HoverOrNull::default());
        }
        if hovers.len() == 1 {
            return Ok(lsproto::HoverOrNull { hover: hovers.pop() });
        }

        let mut contents: Vec<String> = Vec::with_capacity(hovers.len());
        let mut seen_contents: FxHashSet<String> = FxHashSet::default();
        let mut raw_contents: Vec<
            lsproto::VSImageElementOrClassifiedTextElementOrContainerElement,
        > = Vec::new();
        let mut common_range = hovers[0].range;
        let mut can_increase_verbosity = hovers[0].can_increase_verbosity;
        for hover in &hovers {
            let content = hover
                .contents
                .markup_content
                .as_ref()
                .unwrap()
                .value
                .trim_end_matches('\n')
                .to_string();
            if seen_contents.insert(content.clone()) {
                contents.push(content);
                if let Some(raw) = &hover.vs_raw_content {
                    raw_contents.push(
                        lsproto::VSImageElementOrClassifiedTextElementOrContainerElement {
                            container_element: Some(raw.clone()),
                            ..Default::default()
                        },
                    );
                }
            }
            can_increase_verbosity = can_increase_verbosity || hover.can_increase_verbosity;
            if common_range.is_none() || hover.range.is_none() || common_range != hover.range {
                common_range = None;
            }
        }
        let mut combined = hovers.swap_remove(0);
        combined.can_increase_verbosity = can_increase_verbosity;
        let separator =
            if content_format == lsproto::MarkupKind::MARKDOWN { "\n\n---\n\n" } else { "\n\n" };
        combined.contents.markup_content.as_mut().unwrap().value = contents.join(separator);
        combined.range = common_range;
        match raw_contents.len() {
            0 => combined.vs_raw_content = None,
            1 => combined.vs_raw_content = raw_contents.pop().unwrap().container_element,
            _ => {
                combined.vs_raw_content = Some(lsproto::VSContainerElement {
                    style: lsproto::VSContainerElementStyle::STACKED,
                    elements: raw_contents,
                    ..Default::default()
                });
            }
        }
        Ok(lsproto::HoverOrNull { hover: Some(combined) })
    }

    // hover.go:174
    pub(crate) fn get_quick_info_and_documentation_for_symbol(
        &self,
        c: &mut Checker,
        symbol: Option<P<Symbol>>,
        node: P<Node>,
        content_format: lsproto::MarkupKind,
        vc: Option<P<VerbosityContext>>,
        vs_capability: bool,
    ) -> (String, String, String, Vec<lsproto::VSClassifiedTextRun>) {
        let info = get_quick_info_and_declaration_at_location(
            c,
            symbol,
            node,
            vc,
            vs_capability,
            get_meaning_from_location(node),
        );
        let quick_info = info.display_parts.as_str().to_string();
        if quick_info.is_empty() {
            return (String::new(), String::new(), String::new(), Vec::new());
        }
        let quick_info_runs = info.display_parts.get_runs().to_vec();

        let mapper = self.documentation_location_mapper(Feature::Hover);
        let documentation = get_documentation_for_symbol(
            &mapper,
            c,
            symbol,
            node,
            info.declaration,
            content_format,
            false, /*commentOnly*/
        );

        // VS's rich hover (_vs_rawContent) renders documentation as plain colorized text with no Markdown
        // parser, so it can't use the tag section (@param/@returns/@example/@see, etc.) that
        // getDocumentationFromDeclaration renders with '*@tag*' bolding and ```-fenced @example blocks --
        // those would show up as literal asterisks/backticks. This also matches the legacy TSServer-backed
        // VS hover (TypeScript-VS's HoverService.cs), which only ever surfaced the JSDoc summary
        // (TSServer's quickinfo `documentation`) and never included the tag section at all (TSServer
        // exposes tags via a separate `tags` field that legacy VS hover never read). So request
        // comment-only, plain-text documentation for the VS path instead of reusing `documentation`.
        let mut vs_documentation = String::new();
        if vs_capability {
            let mapper = self.documentation_location_mapper(Feature::Hover);
            vs_documentation = get_documentation_for_symbol(
                &mapper,
                c,
                symbol,
                node,
                info.declaration,
                lsproto::MarkupKind::PLAIN_TEXT,
                true, /*commentOnly*/
            );
        }

        (quick_info, documentation, vs_documentation, quick_info_runs)
    }

    // hover.go:202
    pub(crate) fn documentation_location_mapper(
        &self,
        feature: Feature,
    ) -> impl Fn(P<SourceFile>, TextRange) -> (lsproto::Location, Fidelity) + '_ {
        move |file, file_range| {
            self.source_file_range_to_lsp_location_for_feature(file, file_range, feature)
        }
    }
}

// hover.go:200
pub type DocumentationLocationMapper<'a> =
    &'a dyn Fn(P<SourceFile>, TextRange) -> (lsproto::Location, Fidelity);

// hover.go:212
// getDocumentationForSymbol tries each documentation source in turn (call-signature documentation,
// declaration JSDoc, root-symbol JSDoc, alias target JSDoc) and returns the first non-empty result,
// formatted for contentFormat. commentOnly restricts the result to the JSDoc summary, excluding the
// @tag section.
fn get_documentation_for_symbol(
    get_mapped_location: DocumentationLocationMapper,
    c: &mut Checker,
    symbol: Option<P<Symbol>>,
    node: P<Node>,
    declaration: Option<P<Node>>,
    content_format: lsproto::MarkupKind,
    comment_only: bool,
) -> String {
    let mut documentation = documentation_from_signature(
        get_mapped_location,
        c,
        symbol,
        get_call_or_new_expression(node),
        node,
        content_format,
        comment_only,
    );
    if !documentation.is_empty() {
        return documentation;
    }

    documentation = documentation_from_root_symbols(
        get_mapped_location,
        c,
        symbol,
        node,
        content_format,
        comment_only,
    );
    if !documentation.is_empty() {
        return documentation;
    }

    documentation = get_documentation_from_declaration(
        get_mapped_location,
        c,
        symbol,
        declaration,
        Some(node),
        content_format,
        comment_only,
    );
    if !documentation.is_empty() {
        return documentation;
    }

    documentation_from_alias(get_mapped_location, c, symbol, node, content_format, comment_only)
}

// hover.go:231
fn documentation_from_signature(
    get_mapped_location: DocumentationLocationMapper,
    c: &mut Checker,
    symbol: Option<P<Symbol>>,
    node: Option<P<Node>>,
    location: P<Node>,
    content_format: lsproto::MarkupKind,
    comment_only: bool,
) -> String {
    let Some(node) = node else {
        return String::new();
    };
    let signature = c.get_resolved_signature_exported(node);
    let Some(declaration) = signature.declaration() else {
        return String::new();
    };
    if ast::is_call_signature_declaration(declaration)
        || ast::is_construct_signature_declaration(declaration)
    {
        return get_documentation_from_declaration(
            get_mapped_location,
            c,
            symbol,
            Some(declaration),
            Some(location),
            content_format,
            comment_only,
        );
    }
    String::new()
}

// hover.go:249
fn documentation_from_alias(
    get_mapped_location: DocumentationLocationMapper,
    c: &mut Checker,
    symbol: Option<P<Symbol>>,
    node: P<Node>,
    content_format: lsproto::MarkupKind,
    comment_only: bool,
) -> String {
    let Some(symbol) = symbol else {
        return String::new();
    };
    if !symbol.flags().intersects(SymbolFlags::Alias) {
        return String::new();
    }

    let aliased_symbol = c.get_aliased_symbol(symbol);
    if aliased_symbol == c.get_unknown_symbol() {
        return String::new();
    }

    let mut candidates = vec![aliased_symbol];
    if let Some(export_symbol) = aliased_symbol.export_symbol() {
        candidates.push(export_symbol);
    }

    if let Some(result) = candidates.into_iter().find_map(|candidate| {
        let aliased_declaration =
            candidate.value_declaration().or_else(|| candidate.declarations().first().copied());
        aliased_declaration?;

        let documentation = get_documentation_from_declaration(
            get_mapped_location,
            c,
            Some(candidate),
            aliased_declaration,
            Some(node),
            content_format,
            comment_only,
        );
        if !documentation.is_empty() {
            return Some(documentation);
        }

        None
    }) {
        return result;
    }

    String::new()
}

// hover.go:278
fn documentation_from_root_symbols(
    get_mapped_location: DocumentationLocationMapper,
    c: &mut Checker,
    symbol: Option<P<Symbol>>,
    node: P<Node>,
    content_format: lsproto::MarkupKind,
    comment_only: bool,
) -> String {
    let Some(symbol) = symbol else {
        return String::new();
    };

    let root_symbols = c.get_root_symbols(symbol);
    if root_symbols.len() <= 1 {
        return String::new();
    }

    let mut docs: Vec<String> = Vec::new();
    for root_symbol in root_symbols {
        let mut declarations: Vec<P<Node>> = root_symbol.declarations().to_vec();
        if declarations.is_empty()
            && let Some(value_declaration) = root_symbol.value_declaration()
        {
            declarations = vec![value_declaration];
        }
        for declaration in declarations {
            let documentation = get_documentation_from_declaration(
                get_mapped_location,
                c,
                Some(root_symbol),
                Some(declaration),
                Some(node),
                content_format,
                comment_only,
            );
            if !documentation.is_empty() && !docs.contains(&documentation) {
                docs.push(documentation);
            }
        }
    }
    docs.join("\n")
}

// hover.go:306
pub fn get_documentation_from_declaration(
    get_mapped_location: DocumentationLocationMapper,
    c: &mut Checker,
    _symbol: Option<P<Symbol>>,
    declaration: Option<P<Node>>,
    _location: Option<P<Node>>,
    content_format: lsproto::MarkupKind,
    comment_only: bool,
) -> String {
    let Some(declaration) = declaration else {
        return String::new();
    };
    let is_markdown = content_format == lsproto::MarkupKind::MARKDOWN;
    let mut b = String::new();
    if let Some(jsdoc) = get_jsdoc_or_tag(c, Some(declaration), &mut FxHashSet::default())
        && (declaration.flags().intersects(NodeFlags::Reparsed) || !contains_typedef_tag(jsdoc))
    {
        write_comments(get_mapped_location, &mut b, c, jsdoc.comments(), is_markdown);
        if jsdoc.kind() == Kind::JSDoc
            && !comment_only
            && let Some(tags) = jsdoc.as_jsdoc().tags
        {
            for &tag in tags.nodes() {
                if tag.kind() == Kind::JSDocTypeTag
                    || tag.kind() == Kind::JSDocTypedefTag
                    || tag.kind() == Kind::JSDocCallbackTag
                {
                    continue;
                }
                b.push_str("\n\n");
                if is_markdown {
                    b.push_str("*@");
                    b.push_str(tag.tag_name().text());
                    b.push('*');
                } else {
                    b.push('@');
                    b.push_str(tag.tag_name().text());
                }
                match tag.kind() {
                    Kind::JSDocParameterTag | Kind::JSDocPropertyTag => {
                        write_optional_entity_name(&mut b, tag.name());
                    }
                    Kind::JSDocAugmentsTag => {
                        write_optional_entity_name(&mut b, Some(tag.class_name()));
                    }
                    Kind::JSDocTemplateTag => {
                        for (i, &tp) in tag.type_parameters().iter().enumerate() {
                            if i != 0 {
                                b.push(',');
                            }
                            write_optional_entity_name(&mut b, tp.name());
                        }
                    }
                    _ => {}
                }
                let comments = tag.comments();
                if tag.kind() == Kind::JSDocUnknownTag && tag.tag_name().text() == "example" {
                    let mut comment_text = scanner::get_text_of_jsdoc_comment(tag.comment_list());
                    if comment_text.starts_with("<caption>")
                        && let Some(caption_end) =
                            comment_text.find("</caption>").filter(|&i| i > 0)
                    {
                        b.push_str(" — ");
                        b.push_str(&comment_text["<caption>".len()..caption_end]);
                        comment_text = comment_text[caption_end + "</caption>".len()..].to_string();
                        // Trim leading blank lines from commentText
                        loop {
                            let s1 = comment_text.trim_start_matches([' ', '\t']);
                            let s2 = s1.trim_start_matches(['\r', '\n']);
                            if s1.len() == s2.len() {
                                break;
                            }
                            comment_text = s2.to_string();
                        }
                    }
                    b.push('\n');
                    if comment_text.len() > 6
                        && comment_text.starts_with("```")
                        && comment_text.ends_with("```")
                        && comment_text.contains('\n')
                    {
                        b.push_str(&comment_text);
                        b.push('\n');
                    } else {
                        write_code(&mut b, "tsx", &comment_text);
                    }
                } else if tag.kind() == Kind::JSDocSeeTag
                    && tag.as_jsdoc_see_tag().name_expression.is_some()
                {
                    b.push_str(" — ");
                    write_name_link(
                        get_mapped_location,
                        &mut b,
                        c,
                        tag.as_jsdoc_see_tag().name_expression.unwrap().name().unwrap(),
                        "",
                        false, /*quote*/
                        is_markdown,
                    );
                    if !comments.is_empty() {
                        b.push(' ');
                        write_comments(get_mapped_location, &mut b, c, comments, is_markdown);
                    }
                } else if tag.kind() == Kind::JSDocThrowsTag
                    && tag.as_jsdoc_throws_tag().type_expression.is_some()
                {
                    b.push_str(" — ");
                    b.push_str(&scanner::get_text_of_node(
                        tag.as_jsdoc_throws_tag().type_expression.unwrap(),
                    ));
                    if !comments.is_empty() {
                        b.push(' ');
                        write_comments(get_mapped_location, &mut b, c, comments, is_markdown);
                    }
                } else if !comments.is_empty() {
                    b.push(' ');
                    if comments[0].kind() != Kind::JSDocText || !comments[0].text().starts_with('-')
                    {
                        b.push_str("— ");
                    }
                    write_comments(get_mapped_location, &mut b, c, comments, is_markdown);
                }
            }
        }
    }
    b
}

// hover.go:396
fn format_quick_info(quick_info: &str) -> String {
    let mut b = String::with_capacity(32);
    write_code(&mut b, "typescript", quick_info);
    b
}

// hover.go:403
fn should_get_type(node: P<Node>) -> bool {
    match node.kind() {
        Kind::Identifier => {
            // If we're in a JSDoc node with no associated symbol, no binding has taken place for the node and
            // we can't answer questions about types of declaration nodes (such as property declarations).
            let is_unbound_jsdoc_declaration =
                node.flags().intersects(NodeFlags::JSDoc) && ast::is_declaration_name(node);
            !is_unbound_jsdoc_declaration
                && !ast::is_label_name(node)
                && !ast::is_tag_name(node)
                && !ast::is_const_type_reference(node.parent().unwrap())
        }
        Kind::ThisKeyword | Kind::ThisType | Kind::SuperKeyword | Kind::NamedTupleMember => true,
        Kind::MetaProperty => ast::is_import_meta(node),
        _ => false,
    }
}

// symbolDisplayInfo holds the result of getSymbolDisplayPartsDocumentationAndSymbolKind.
// hover.go:420
pub struct SymbolDisplayInfo {
    pub(crate) display_parts: DisplayPartsWriter,
    declaration: Option<P<Node>>,
}

// nodeBuilderFlags for classified output (same as signatureHelpNodeBuilderFlags)
// hover.go:440
const CLASSIFIED_NODE_BUILDER_FLAGS: Flags = Flags::IgnoreErrors
    .union(Flags::UseAliasDefinedOutsideCurrentScope)
    .union(Flags::WriteTypeParametersInQualifiedName);

// The closures of getQuickInfoAndDeclarationAtLocation (hover.go:426) share this state; each closure is a method that
// receives the checker first (PORTING.md callback rule).
struct QuickInfoWriter {
    dpw: DisplayPartsWriter,
    display_emit_context: Cell<Option<P<printer::EmitContext>>>,
    vs_capability: bool,
    vc: P<VerbosityContext>,
    source_file: Option<P<SourceFile>>,
    container: Option<P<Node>>,
    node: P<Node>,
    meaning: SemanticMeaning,
    visited_aliases: FxHashSet<P<Symbol>>,
    alias_level: usize,
    first_declaration: Option<P<Node>>,
    symbol_was_expanded: bool,
}

fn node_builder_flags(flags: TypeFormatFlags) -> Flags {
    Flags::from_bits_retain((flags & TypeFormatFlags::NodeBuilderFlagsMask).bits())
        | CLASSIFIED_NODE_BUILDER_FLAGS
}

impl QuickInfoWriter {
    fn emit_context(&self) -> P<printer::EmitContext> {
        if let Some(context) = self.display_emit_context.get() {
            return context;
        }
        let context = printer::new_emit_context();
        self.display_emit_context.set(Some(context));
        context
    }

    // writeTypeClassified writes a type to dpw with proper classification (punctuation, symbols, keywords).
    // Falls back to flat text when vsCapability is false or when TypeToTypeNode fails.
    // hover.go:444
    fn write_type_classified(
        &mut self,
        c: &mut Checker,
        t: P<Type>,
        enclosing: Option<P<Node>>,
        mut flags: TypeFormatFlags,
    ) {
        flags |= TypeFormatFlags::MultilineObjectLiterals;
        if !self.vs_capability {
            let s = c.type_to_string_ex(t, enclosing, flags, Some(self.vc));
            self.dpw.write(&s);
            return;
        }
        let emit_context = self.emit_context();
        let id_to_symbol: P<RefCell<FxHashMap<P<Node>, P<Symbol>>>> =
            P::new(RefCell::new(FxHashMap::default()));
        let nb = checker::new_node_builder_ex(c, emit_context, Some(id_to_symbol));
        let combined_flags = node_builder_flags(flags);
        let Some(type_node) =
            nb.type_to_type_node(c, t, enclosing, combined_flags, InternalFlags::None, None)
        else {
            let s = c.type_to_string_ex(t, enclosing, flags, Some(self.vc));
            self.dpw.write(&s);
            return;
        };
        let mut p = printer::new_printer(
            printer::PrinterOptions { new_line: NewLineKind::LF, ..Default::default() },
            printer::PrintHandlers::default(),
            Some(emit_context),
        );
        p.id_to_symbol = Some(id_to_symbol.borrow().clone());
        let mut temp_dpw = new_display_parts_writer(true);
        p.write(type_node, self.source_file, &mut temp_dpw, None);
        self.dpw.write_from(&temp_dpw);
    }

    // writeSignatureClassified writes a signature to dpw with proper classification.
    // hover.go:466
    fn write_signature_classified(
        &mut self,
        c: &mut Checker,
        sig: P<Signature>,
        enclosing: Option<P<Node>>,
        mut flags: TypeFormatFlags,
    ) {
        flags |= TypeFormatFlags::MultilineObjectLiterals;
        if !self.vs_capability {
            let s = c.signature_to_string_ex(sig, enclosing, flags, Some(self.vc));
            self.dpw.write(&s);
            return;
        }
        let is_constructor = sig.flags().intersects(SignatureFlags::Construct)
            && !flags.intersects(TypeFormatFlags::WriteCallStyleSignature);
        let sig_output = if flags.intersects(TypeFormatFlags::WriteArrowStyleSignature) {
            if is_constructor { Kind::ConstructorType } else { Kind::FunctionType }
        } else if is_constructor {
            Kind::ConstructSignature
        } else {
            Kind::CallSignature
        };
        let emit_context = self.emit_context();
        let id_to_symbol: P<RefCell<FxHashMap<P<Node>, P<Symbol>>>> =
            P::new(RefCell::new(FxHashMap::default()));
        let nb = checker::new_node_builder_ex(c, emit_context, Some(id_to_symbol));
        let combined_flags = node_builder_flags(flags);
        let Some(sig_node) = nb.signature_to_signature_declaration(
            c,
            sig,
            sig_output,
            enclosing,
            combined_flags,
            InternalFlags::None,
            None,
        ) else {
            let s = c.signature_to_string_ex(sig, enclosing, flags, Some(self.vc));
            self.dpw.write(&s);
            return;
        };
        let mut p = printer::new_printer(
            printer::PrinterOptions { new_line: NewLineKind::LF, ..Default::default() },
            printer::PrintHandlers::default(),
            Some(emit_context),
        );
        p.id_to_symbol = Some(id_to_symbol.borrow().clone());
        let mut temp_dpw = new_display_parts_writer(true);
        p.write(sig_node, self.source_file, &mut temp_dpw, None);
        self.dpw.write_from(&temp_dpw);
    }

    // writeSymbolClassified writes a symbol name to dpw with proper classification based on symbol flags.
    // hover.go:506
    fn write_symbol_classified(
        &mut self,
        c: &mut Checker,
        symbol: P<Symbol>,
        enclosing: Option<P<Node>>,
        meaning: SymbolFlags,
        flags: SymbolFormatFlags,
    ) {
        if !self.vs_capability {
            let s = c.symbol_to_string_ex(symbol, enclosing, meaning, flags);
            self.dpw.write(&s);
            return;
        }
        // Use WriteSymbol which calls classificationForSymbol to determine the correct classification
        let text = c.symbol_to_string_ex(symbol, enclosing, meaning, flags);
        self.dpw.write_symbol(&text, symbol);
    }

    // hover.go:515
    fn write_module_import_attributes(&mut self, symbol: P<Symbol>) {
        let declaration = symbol.declarations().iter().copied().find(|&declaration| {
            ast::is_module_declaration(declaration)
                && declaration.as_module_declaration().attributes.is_some()
        });
        let Some(declaration) = declaration else {
            return;
        };
        let attributes = declaration.as_module_declaration().attributes.unwrap();
        let emit_context = self.emit_context();
        emit_context.set_emit_flags(attributes, printer::EmitFlags::SingleLine);
        let mut p = printer::new_printer(
            printer::PrinterOptions { new_line: NewLineKind::LF, ..Default::default() },
            printer::PrintHandlers::default(),
            Some(emit_context),
        );
        let mut temp_dpw = new_display_parts_writer(self.vs_capability);
        p.write(attributes, ast::get_source_file_of_node(declaration), &mut temp_dpw, None);
        self.dpw.write_keyword(" with ");
        self.dpw.write_from(&temp_dpw);
    }

    // hover.go:550
    fn set_declaration(&mut self, declaration: Option<P<Node>>) {
        if self.first_declaration.is_none() {
            self.first_declaration = declaration;
        }
    }

    // hover.go:555
    fn write_new_line(&mut self) {
        if !self.dpw.as_str().is_empty() {
            self.dpw.write("\n");
        }
        if self.alias_level != 0 {
            self.dpw.write_punctuation("(");
            self.dpw.write("alias");
            self.dpw.write_punctuation(") ");
        }
    }

    // hover.go:565
    fn write_signatures(
        &mut self,
        c: &mut Checker,
        signatures: &[P<Signature>],
        prefix: &str,
        parenthesized: bool,
        symbol: P<Symbol>,
    ) {
        for (i, &sig) in signatures.iter().enumerate() {
            self.write_new_line();
            if i == 3 && signatures.len() >= 5 {
                self.dpw.write_comment(&format!("// +{} more overloads", signatures.len() - 3));
                break;
            }
            if parenthesized {
                self.dpw.write_punctuation("(");
                self.dpw.write(prefix);
                self.dpw.write_punctuation(") ");
            } else {
                self.dpw.write_keyword(prefix);
            }
            self.write_symbol_classified(
                c,
                symbol,
                self.container,
                SymbolFlags::None,
                SYMBOL_FORMAT_FLAGS,
            );
            if symbol.flags().intersects(SymbolFlags::Optional) {
                self.dpw.write_punctuation("?");
            }
            self.write_signature_classified(
                c,
                sig,
                self.container,
                TYPE_FORMAT_FLAGS
                    | TypeFormatFlags::WriteCallStyleSignature
                    | TypeFormatFlags::WriteTypeArgumentsOfSignature,
            );
        }
    }

    // hover.go:586
    fn write_type_params(&mut self, c: &mut Checker, params: &[P<Type>]) {
        if !params.is_empty() {
            self.dpw.write_punctuation("<");
            for (i, &tp) in params.iter().enumerate() {
                if i != 0 {
                    self.dpw.write_punctuation(", ");
                }
                self.write_symbol_classified(
                    c,
                    tp.symbol().unwrap(),
                    None,
                    SymbolFlags::None,
                    SYMBOL_FORMAT_FLAGS,
                );
                if let Some(cons) = c.get_constraint_of_type_parameter_exported(tp) {
                    self.dpw.write_keyword(" extends ");
                    self.write_type_classified(c, cons, None, TYPE_FORMAT_FLAGS);
                }
                if let Some(def) = c.get_default_from_type_parameter_exported(tp) {
                    self.dpw.write_operator(" = ");
                    self.write_type_classified(c, def, None, TYPE_FORMAT_FLAGS);
                }
            }
            self.dpw.write_punctuation(">");
        }
    }

    // hover.go:609
    fn can_expand_symbol(&self, c: &mut Checker, symbol: P<Symbol>) -> bool {
        // (Go returns false for a nil vc; hover always passes one, and a nil vc was replaced above.)
        // Only offer symbol-level expansion for types that tryExpandSymbol handles:
        // class, interface, enum, namespace/module. For functions/variables/properties,
        // the node builder's probeTypeExpandability detects expandable type components.
        if !symbol
            .flags()
            .intersects(SymbolFlags::Class | SymbolFlags::Interface | SymbolFlags::Namespace)
        {
            return false;
        }
        let t = if symbol.flags().intersects(SymbolFlags::Class | SymbolFlags::Interface) {
            Some(c.get_declared_type_of_symbol_exported(symbol))
        } else {
            c.get_type_of_symbol_at_location(symbol, Some(self.node))
        };
        let Some(t) = t else { return false };
        if c.is_lib_type_for_hover_verbosity(t) {
            return false;
        }
        if self.vc.level.get() > 0 {
            return true;
        }
        // At level 0, signal that expansion is possible but don't expand
        self.vc.can_increase_verbosity.set(true);
        false
    }

    // tryExpandSymbol checks if a symbol can be expanded at the current verbosity level.
    // hover.go:637
    fn try_expand_symbol(
        &mut self,
        c: &mut Checker,
        symbol: P<Symbol>,
        meaning: SymbolFlags,
    ) -> bool {
        if self.symbol_was_expanded {
            return true;
        }
        if self.can_expand_symbol(c, symbol) {
            let expand_vc =
                new_verbosity_context(self.vc.level.get() - 1, self.vc.max_truncation_length.get());
            let expanded = c.expand_symbol_for_hover(symbol, meaning, Some(expand_vc));
            if !expanded.is_empty() {
                self.vc.can_increase_verbosity.set(
                    self.vc.can_increase_verbosity.get() || expand_vc.can_increase_verbosity.get(),
                );
                self.vc.truncated.set(self.vc.truncated.get() || expand_vc.truncated.get());
                self.dpw.write(&expanded);
                self.symbol_was_expanded = true;
                return true;
            }
        }
        false
    }

    // hover.go:658
    fn write_symbol(&mut self, c: &mut Checker, symbol: P<Symbol>) {
        let node = self.node;
        let container = self.container;
        // Recursively write all meanings of alias
        if symbol.flags().intersects(SymbolFlags::Alias) && self.visited_aliases.insert(symbol) {
            let aliased_symbol = c.get_aliased_symbol(symbol);
            if aliased_symbol != c.get_unknown_symbol() {
                self.alias_level += 1;
                self.write_symbol(c, aliased_symbol);
                self.alias_level -= 1;
            }
        }
        let mut flags = if self.meaning == SemanticMeaning::Value {
            symbol.flags() & (SymbolFlags::Value | SymbolFlags::Signature)
        } else if self.meaning == SemanticMeaning::Type {
            symbol.flags() & SymbolFlags::Type
        } else if self.meaning == SemanticMeaning::Namespace {
            symbol.flags() & SymbolFlags::Namespace
        } else {
            symbol.flags()
                & (SymbolFlags::Value
                    | SymbolFlags::Signature
                    | SymbolFlags::Type
                    | SymbolFlags::Namespace)
        };
        if flags.is_empty() {
            if self.alias_level != 0 || !self.dpw.as_str().is_empty() {
                return;
            }
            flags = symbol.flags()
                & (SymbolFlags::Value
                    | SymbolFlags::Signature
                    | SymbolFlags::Type
                    | SymbolFlags::Namespace);
            if flags.is_empty() {
                return;
            }
        }
        if flags.intersects(SymbolFlags::Property)
            && symbol.value_declaration().is_some_and(ast::is_method_declaration)
        {
            flags = SymbolFlags::Method;
        }
        if flags.intersects(SymbolFlags::Variable | SymbolFlags::Property | SymbolFlags::Accessor) {
            self.write_new_line();
            if !symbol.check_flags().intersects(CheckFlags::IndexSymbol) {
                if flags.intersects(SymbolFlags::Property) {
                    self.dpw.write_punctuation("(");
                    self.dpw.write("property");
                    self.dpw.write_punctuation(") ");
                } else if flags.intersects(SymbolFlags::Accessor) {
                    self.dpw.write_punctuation("(");
                    self.dpw.write("accessor");
                    self.dpw.write_punctuation(") ");
                } else if let Some(decl) = symbol.value_declaration() {
                    let decl = ast::get_root_declaration(decl);
                    if ast::is_parameter_declaration(decl) {
                        self.dpw.write_punctuation("(");
                        self.dpw.write("parameter");
                        self.dpw.write_punctuation(") ");
                    } else if ast::is_var_let(decl) {
                        self.dpw.write_keyword("let ");
                    } else if ast::is_var_const(decl) {
                        self.dpw.write_keyword("const ");
                    } else if ast::is_var_using(decl) {
                        self.dpw.write_keyword("using ");
                    } else if ast::is_var_await_using(decl) {
                        self.dpw.write_keyword("await ");
                        self.dpw.write_keyword("using ");
                    } else {
                        self.dpw.write_keyword("var ");
                    }
                }
                if symbol.name() == ast::INTERNAL_SYMBOL_NAME_EXPORT_EQUALS
                    && symbol.parent().is_some_and(|p| p.flags().intersects(SymbolFlags::Module))
                {
                    self.dpw.write("exports");
                } else {
                    self.write_symbol_classified(
                        c,
                        symbol,
                        container,
                        SymbolFlags::None,
                        SYMBOL_FORMAT_FLAGS,
                    );
                }
                if symbol.flags().intersects(SymbolFlags::Optional) {
                    self.dpw.write_punctuation("?");
                }
                self.dpw.write_punctuation(": ");
            }
            if let Some(call_node) = get_call_or_new_expression(node) {
                let mut flags = TYPE_FORMAT_FLAGS
                    | TypeFormatFlags::WriteTypeArgumentsOfSignature
                    | TypeFormatFlags::WriteArrowStyleSignature;
                if ast::is_call_expression(call_node) {
                    flags |= TypeFormatFlags::WriteCallStyleSignature;
                }
                let sig = c.get_resolved_signature_exported(call_node);
                self.write_signature_classified(c, sig, container, flags);
            } else {
                let t = c.get_type_of_symbol_at_location(symbol, Some(node)).unwrap();
                // If the type is a constrained type parameter, support expansion:
                // Level 0: show just "T", signal canIncreaseVerbosity
                // Level 1+: show "T extends Constraint" with the constraint expanded at level-1
                if t.symbol().is_some_and(|s| s.flags().intersects(SymbolFlags::TypeParameter))
                    && c.get_constraint_of_type_parameter_exported(t).is_some()
                {
                    if self.vc.level.get() > 0 {
                        let expand_vc = new_verbosity_context(
                            self.vc.level.get() - 1,
                            self.vc.max_truncation_length.get(),
                        );
                        let s = type_parameter_to_string(c, t, container, Some(expand_vc));
                        self.dpw.write(&s);
                        self.vc.can_increase_verbosity.set(
                            self.vc.can_increase_verbosity.get()
                                || expand_vc.can_increase_verbosity.get(),
                        );
                        self.vc.truncated.set(self.vc.truncated.get() || expand_vc.truncated.get());
                    } else {
                        self.write_type_classified(c, t, container, TYPE_FORMAT_FLAGS);
                        self.vc.can_increase_verbosity.set(true);
                    }
                } else {
                    self.write_type_classified(c, t, container, TYPE_FORMAT_FLAGS);
                }
            }
            self.set_declaration(
                symbol.value_declaration().or_else(|| symbol.declarations().first().copied()),
            );
        }
        if flags.intersects(SymbolFlags::EnumMember) {
            self.write_new_line();
            self.dpw.write_punctuation("(");
            self.dpw.write("enum member");
            self.dpw.write_punctuation(") ");
            let t = c.get_type_of_symbol_exported(symbol);
            self.write_type_classified(c, t, container, TYPE_FORMAT_FLAGS);
            if t.flags().intersects(TypeFlags::Literal) {
                self.dpw.write_operator(" = ");
                self.dpw
                    .write_literal(&checker::value_to_string(t.as_literal_type().value().unwrap()));
            }
            self.set_declaration(symbol.value_declaration());
        }
        if flags.intersects(SymbolFlags::Function | SymbolFlags::Method) {
            let is_method = flags.intersects(SymbolFlags::Method);
            let prefix = if is_method { "method" } else { "function " };
            let node_parent = node.parent();
            if ast::is_identifier(node)
                && node_parent.is_some_and(|p| {
                    ast::is_function_like_declaration(p) || ast::is_method_signature_declaration(p)
                })
                && node_parent.unwrap().name() == Some(node)
                && symbol.declarations().contains(&node_parent.unwrap())
            {
                let parent = node_parent.unwrap();
                self.set_declaration(Some(parent));
                let signatures = vec![c.get_signature_from_declaration_exported(parent)];
                self.write_signatures(c, &signatures, prefix, is_method, symbol);
            } else {
                let signatures = get_signatures_at_location(c, symbol, SignatureKind::Call, node);
                if signatures.len() == 1
                    && let Some(d) = signatures[0].declaration()
                    && !d.flags().intersects(NodeFlags::JSDoc)
                {
                    self.set_declaration(Some(d));
                }
                self.write_signatures(c, &signatures, prefix, is_method, symbol);
            }
            self.set_declaration(symbol.value_declaration());
        }
        if flags.intersects(SymbolFlags::Class | SymbolFlags::Interface) {
            let node_parent = node.parent();
            if node.kind() == Kind::ThisKeyword || ast::is_this_in_type_query(node) {
                self.write_new_line();
                self.dpw.write_keyword("this");
            } else if node.kind() == Kind::ConstructorKeyword
                && node_parent.is_some_and(|p| {
                    ast::is_constructor_declaration(p) || ast::is_construct_signature_declaration(p)
                })
            {
                let parent = node_parent.unwrap();
                self.set_declaration(Some(parent));
                let signatures = vec![c.get_signature_from_declaration_exported(parent)];
                self.write_signatures(c, &signatures, "constructor ", false, symbol);
            } else {
                let mut signatures: Vec<P<Signature>> = Vec::new();
                if flags.intersects(SymbolFlags::Class)
                    && get_call_or_new_expression(node).is_some()
                {
                    signatures =
                        get_signatures_at_location(c, symbol, SignatureKind::Construct, node);
                }
                if signatures.len() == 1 {
                    if let Some(d) = signatures[0].declaration()
                        && !d.flags().intersects(NodeFlags::JSDoc)
                    {
                        self.set_declaration(Some(d));
                    }
                    self.write_signatures(c, &signatures, "constructor ", false, symbol);
                } else {
                    self.write_new_line();
                    if flags.intersects(SymbolFlags::Class) {
                        let class_expression =
                            ast::get_declaration_of_kind(symbol, Kind::ClassExpression);
                        if class_expression.is_some() {
                            // Local class expression: show "(local class)" prefix
                            self.dpw.write_punctuation("(");
                            self.dpw.write("local class");
                            self.dpw.write_punctuation(") ");
                        }
                        if !self.try_expand_symbol(c, symbol, flags) {
                            if class_expression.is_none() {
                                if symbol.declarations().iter().any(|&d| {
                                    ast::is_class_declaration(d) && ast::has_abstract_modifier(d)
                                }) {
                                    self.dpw.write_keyword("abstract ");
                                }
                                self.dpw.write_keyword("class ");
                            }
                            self.write_symbol_classified(
                                c,
                                symbol,
                                container,
                                SymbolFlags::None,
                                SYMBOL_FORMAT_FLAGS,
                            );
                            let params = c
                                .get_declared_type_of_symbol_exported(symbol)
                                .as_interface_type()
                                .local_type_parameters();
                            self.write_type_params(c, params);
                        }
                    } else if !self.try_expand_symbol(c, symbol, flags) {
                        self.dpw.write_keyword("interface ");
                        self.write_symbol_classified(
                            c,
                            symbol,
                            container,
                            SymbolFlags::None,
                            SYMBOL_FORMAT_FLAGS,
                        );
                        let params = c
                            .get_declared_type_of_symbol_exported(symbol)
                            .as_interface_type()
                            .local_type_parameters();
                        self.write_type_params(c, params);
                    }
                }
            }
            if flags.intersects(SymbolFlags::Class) {
                self.set_declaration(symbol.value_declaration());
            } else {
                self.set_declaration(
                    symbol
                        .declarations()
                        .iter()
                        .copied()
                        .find(|&d| ast::is_interface_declaration(d)),
                );
            }
        }
        if flags.intersects(SymbolFlags::Enum) {
            self.write_new_line();
            if !self.try_expand_symbol(c, symbol, flags) {
                if symbol
                    .declarations()
                    .iter()
                    .any(|&d| ast::is_enum_declaration(d) && ast::is_enum_const(d))
                {
                    self.dpw.write_keyword("const ");
                }
                self.dpw.write_keyword("enum ");
                self.write_symbol_classified(
                    c,
                    symbol,
                    container,
                    SymbolFlags::None,
                    SYMBOL_FORMAT_FLAGS,
                );
            }
            self.set_declaration(
                symbol.declarations().iter().copied().find(|&d| ast::is_enum_declaration(d)),
            );
        }
        if flags.intersects(SymbolFlags::Module) {
            self.write_new_line();
            if !self.try_expand_symbol(c, symbol, flags) {
                let is_module = symbol
                    .value_declaration()
                    .is_some_and(|d| ast::is_source_file(d) || ast::is_ambient_module(d));
                self.dpw.write_keyword(if is_module { "module " } else { "namespace " });
                self.write_symbol_classified(
                    c,
                    symbol,
                    container,
                    SymbolFlags::None,
                    SYMBOL_FORMAT_FLAGS,
                );
                self.write_module_import_attributes(symbol);
            }
            self.set_declaration(
                symbol.declarations().iter().copied().find(|&d| ast::is_module_declaration(d)),
            );
        }
        if flags.intersects(SymbolFlags::TypeParameter) {
            self.write_new_line();
            self.dpw.write_punctuation("(");
            self.dpw.write("type parameter");
            self.dpw.write_punctuation(") ");
            if ast::is_identifier(node) && ast::is_type_reference_node(node.parent().unwrap()) {
                let t = c.get_type_at_location(node.parent().unwrap());
                if checker::is_distributed_type_parameter(t) {
                    self.dpw.write_punctuation("(");
                    self.dpw.write("distributed");
                    self.dpw.write_punctuation(") ");
                }
            }
            let tp = c.get_declared_type_of_symbol_exported(symbol);
            self.write_symbol_classified(
                c,
                symbol,
                container,
                SymbolFlags::None,
                SYMBOL_FORMAT_FLAGS,
            );
            if let Some(cons) = c.get_constraint_of_type_parameter_exported(tp) {
                self.dpw.write_keyword(" extends ");
                self.write_type_classified(c, cons, container, TYPE_FORMAT_FLAGS);
            }
            // Show context: "in ClassName<T>" or "in funcName<T>(...)"
            if let Some(symbol_parent) = symbol.parent() {
                // Class/Interface type parameter
                self.dpw.write_keyword(" in ");
                self.write_symbol_classified(
                    c,
                    symbol_parent,
                    container,
                    SymbolFlags::None,
                    SYMBOL_FORMAT_FLAGS,
                );
                let parent_type = c.get_declared_type_of_symbol_exported(symbol_parent);
                if let Some(parent_interface) = parent_type.try_as_interface_type() {
                    let parent_params = parent_interface.local_type_parameters();
                    self.write_type_params(c, parent_params);
                }
            } else {
                // Method/function type parameter
                let decl = ast::get_declaration_of_kind(symbol, Kind::TypeParameter);
                if let Some(declaration) = decl.and_then(|d| d.parent()) {
                    if ast::is_function_like(Some(declaration)) {
                        self.dpw.write_keyword(" in ");
                        if declaration.kind() == Kind::ConstructSignature {
                            self.dpw.write_keyword("new ");
                        } else if declaration.kind() != Kind::CallSignature
                            && declaration.name().is_some()
                        {
                            self.write_symbol_classified(
                                c,
                                declaration.symbol().unwrap(),
                                container,
                                SymbolFlags::None,
                                SYMBOL_FORMAT_FLAGS,
                            );
                        }
                        let sig = c.get_signature_from_declaration_exported(declaration);
                        self.write_signature_classified(
                            c,
                            sig,
                            container,
                            TYPE_FORMAT_FLAGS | TypeFormatFlags::WriteTypeArgumentsOfSignature,
                        );
                    } else if ast::is_type_alias_declaration(declaration) {
                        self.dpw.write_keyword(" in ");
                        self.dpw.write_keyword("type ");
                        self.write_symbol_classified(
                            c,
                            declaration.symbol().unwrap(),
                            container,
                            SymbolFlags::None,
                            SYMBOL_FORMAT_FLAGS,
                        );
                        if let Some(decl_symbol) = declaration.symbol() {
                            let ta_params = c.get_type_alias_type_parameters(decl_symbol);
                            self.write_type_params(c, &ta_params);
                        }
                    }
                }
            }
            self.set_declaration(
                symbol
                    .declarations()
                    .iter()
                    .copied()
                    .find(|&d| ast::is_type_parameter_declaration(d)),
            );
        }
        if flags.intersects(SymbolFlags::TypeAlias) {
            self.write_new_line();
            self.dpw.write_keyword("type ");
            self.write_symbol_classified(
                c,
                symbol,
                container,
                SymbolFlags::None,
                SYMBOL_FORMAT_FLAGS,
            );
            let ta_params = c.get_type_alias_type_parameters(symbol);
            self.write_type_params(c, &ta_params);
            self.dpw.write_operator(" = ");
            let type_alias_type = match node.parent() {
                Some(parent) if ast::is_const_type_reference(parent) => {
                    c.get_type_at_location(parent)
                }
                _ => c.get_declared_type_of_symbol_exported(symbol),
            };
            self.write_type_classified(
                c,
                type_alias_type,
                container,
                TYPE_FORMAT_FLAGS | TypeFormatFlags::InTypeAlias,
            );
            self.set_declaration(
                symbol
                    .declarations()
                    .iter()
                    .copied()
                    .find(|&d| ast::is_type_or_js_type_alias_declaration(d)),
            );
        }
        if flags.intersects(SymbolFlags::Signature) {
            self.write_new_line();
            let t = c.get_type_of_symbol_exported(symbol);
            self.write_type_classified(c, t, container, TYPE_FORMAT_FLAGS);
        }
    }
}

// getQuickInfoAndDeclarationAtLocation builds classified display parts using displayPartsWriter when vsCapability is true.
// When vsCapability is false, it still builds the plain text string but skips classification runs.
// hover.go:426
pub fn get_quick_info_and_declaration_at_location(
    c: &mut Checker,
    symbol: Option<P<Symbol>>,
    node: P<Node>,
    vc: Option<P<VerbosityContext>>,
    vs_capability: bool,
    meaning: SemanticMeaning,
) -> SymbolDisplayInfo {
    let container = get_container_node(node);
    let vc = vc.unwrap_or_else(|| P::new(VerbosityContext::default()));
    let dpw = new_display_parts_writer(vs_capability);

    // Source file for printer context
    let source_file = ast::get_source_file_of_node(node);

    let mut w = QuickInfoWriter {
        display_emit_context: Cell::new(None),
        dpw,
        vs_capability,
        vc,
        source_file,
        container,
        node,
        meaning,
        visited_aliases: FxHashSet::default(),
        alias_level: 0,
        first_declaration: None,
        symbol_was_expanded: false,
    };

    if node.kind() == Kind::ThisKeyword && ast::is_in_expression_context(node)
        || ast::is_this_in_type_query(node)
    {
        w.dpw.write_keyword("this");
        w.dpw.write_punctuation(": ");
        let t = c.get_type_at_location(node);
        w.write_type_classified(c, t, container, TYPE_FORMAT_FLAGS);
        return SymbolDisplayInfo { display_parts: w.dpw, declaration: None };
    }
    let Some(symbol) = symbol else {
        if should_get_type(node) {
            let t = c.get_type_at_location(node);
            w.write_type_classified(c, t, container, TYPE_FORMAT_FLAGS);
        }
        return SymbolDisplayInfo { display_parts: w.dpw, declaration: None };
    };
    w.write_symbol(c, symbol);

    SymbolDisplayInfo { display_parts: w.dpw, declaration: w.first_declaration }
}

// typeParameterToString renders a type parameter declaration (e.g., "T extends FooType").
// hover.go:951
fn type_parameter_to_string(
    c: &mut Checker,
    t: P<Type>,
    enclosing_declaration: Option<P<Node>>,
    vc: Option<P<VerbosityContext>>,
) -> String {
    c.type_parameter_to_string_ex(t, enclosing_declaration, vc)
}

// hover.go:955
fn get_node_for_quick_info(node: P<Node>) -> P<Node> {
    let Some(parent) = node.parent() else {
        return node;
    };
    if ast::is_new_expression(parent) && node.pos() == parent.pos() {
        return parent.expression().unwrap();
    }
    if ast::is_named_tuple_member(parent) && node.pos() == parent.pos() {
        return parent;
    }
    if ast::is_import_meta(parent) && parent.name() == Some(node) {
        return parent;
    }
    if ast::is_jsx_namespaced_name(parent) {
        return parent;
    }
    node
}

// hover.go:974
fn get_symbol_at_location_for_quick_info(c: &mut Checker, node: P<Node>) -> Option<P<Symbol>> {
    if let Some(object_element) = get_containing_object_literal_element(node)
        && let Some(contextual_type) =
            c.get_contextual_type_exported(object_element.parent().unwrap(), ContextFlags::None)
    {
        let properties = c.get_property_symbols_from_contextual_type(
            object_element,
            contextual_type,
            false, /*unionSymbolOk*/
        );
        if properties.len() == 1 {
            return Some(properties[0]);
        }
    }
    c.get_symbol_at_location_exported(node)
}

// hover.go:985
fn get_signatures_at_location(
    c: &mut Checker,
    symbol: P<Symbol>,
    kind: SignatureKind,
    node: P<Node>,
) -> Vec<P<Signature>> {
    let t = c.get_type_of_symbol_exported(symbol);
    let t = c.remove_missing_or_undefined_type_exported(t);
    let signatures = c.get_signatures_of_type_exported(t, kind);
    if (signatures.len() > 1
        || signatures.len() == 1 && !signatures[0].type_parameters().is_empty())
        && let Some(call_node) = get_call_or_new_expression(node)
    {
        // We have a call or new expression, return the resolved signature
        return vec![c.get_resolved_signature_exported(call_node)];
    }
    signatures.to_vec()
}

// hover.go:996
fn get_call_or_new_expression(mut node: P<Node>) -> Option<P<Node>> {
    if ast::is_source_file(node) {
        return None;
    }
    let parent = node.parent().unwrap();
    if ast::is_property_access_expression(parent) && parent.name() == Some(node) {
        node = parent;
    }
    let parent = node.parent().unwrap();
    if (ast::is_call_expression(parent) || ast::is_new_expression(parent))
        && parent.expression() == Some(node)
    {
        return Some(parent);
    }
    None
}

// hover.go:1009
fn contains_typedef_tag(jsdoc: P<Node>) -> bool {
    if jsdoc.kind() == Kind::JSDoc
        && let Some(tags) = jsdoc.as_jsdoc().tags
        && tags
            .nodes()
            .iter()
            .any(|&tag| tag.kind() == Kind::JSDocTypedefTag || tag.kind() == Kind::JSDocCallbackTag)
    {
        return true;
    }
    false
}

// hover.go:1022
fn write_code(b: &mut String, lang: &str, code: &str) {
    if code.is_empty() {
        return;
    }
    let mut ticks = 3;
    while code.contains(&"`".repeat(ticks)) {
        ticks += 1;
    }
    b.push_str(&"`".repeat(ticks));
    b.push_str(lang);
    b.push('\n');
    b.push_str(code);
    b.push('\n');
    b.push_str(&"`".repeat(ticks));
    b.push('\n');
}

// hover.go:1043
fn write_comments(
    get_mapped_location: DocumentationLocationMapper,
    b: &mut String,
    c: &mut Checker,
    comments: &[P<Node>],
    is_markdown: bool,
) {
    for &comment in comments {
        match comment.kind() {
            Kind::JSDocText => b.push_str(comment.text()),
            Kind::JSDocLink | Kind::JSDocLinkPlain => write_jsdoc_link(
                get_mapped_location,
                b,
                c,
                comment,
                false, /*quote*/
                is_markdown,
            ),
            Kind::JSDocLinkCode => write_jsdoc_link(
                get_mapped_location,
                b,
                c,
                comment,
                true, /*quote*/
                is_markdown,
            ),
            _ => {}
        }
    }
}

// hover.go:1056
fn write_jsdoc_link(
    get_mapped_location: DocumentationLocationMapper,
    b: &mut String,
    c: &mut Checker,
    link: P<Node>,
    quote: bool,
    is_markdown: bool,
) {
    let name = link.name();
    let text = link.text().trim_matches(' ');
    let Some(name) = name else {
        write_quoted_string(b, text, quote && is_markdown);
        return;
    };
    if ast::is_identifier(name)
        && (name.text() == "http" || name.text() == "https")
        && text.starts_with("://")
    {
        let mut link_text = format!("{}{}", name.text(), text);
        let mut link_uri = link_text.clone();
        if let Some(comment_pos) = link_text.find([' ', '|']) {
            link_uri = link_text[..comment_pos].to_string();
            link_text = trim_comment_prefix(&link_text[comment_pos..]).to_string();
            if link_text.is_empty() {
                link_text.clone_from(&link_uri);
            }
        }
        if is_markdown {
            write_markdown_link(b, &link_text, &link_uri, quote);
        } else {
            write_quoted_string(b, &link_text, false);
            if link_text != link_uri {
                b.push_str(" (");
                b.push_str(&link_uri);
                b.push(')');
            }
        }
        return;
    }
    write_name_link(get_mapped_location, b, c, name, text, quote, is_markdown);
}

// hover.go:1088
fn write_name_link(
    get_mapped_location: DocumentationLocationMapper,
    b: &mut String,
    c: &mut Checker,
    name: P<Node>,
    text: &str,
    quote: bool,
    is_markdown: bool,
) {
    let declarations = get_declarations_from_location(c, name);
    if !declarations.is_empty() {
        let declaration = declarations[0];
        let file = ast::get_source_file_of_node(declaration).unwrap();
        let node = ast::get_name_of_declaration(declaration).unwrap_or(declaration);
        let (loc, fidelity) = get_mapped_location(file, create_range_from_node(node, file));
        let prefix_len = if text.starts_with("()") { 2 } else { 0 };
        let mut link_text = trim_comment_prefix(&text[prefix_len..]).to_string();
        if link_text.is_empty() {
            link_text = get_entity_name_string(name) + &text[..prefix_len];
        }
        if is_markdown && fidelity.is_single_segment() {
            let link_uri = format!(
                "{}#{},{}-{},{}",
                loc.uri,
                loc.range.start.line + 1,
                loc.range.start.character + 1,
                loc.range.end.line + 1,
                loc.range.end.character + 1
            );
            write_markdown_link(b, &link_text, &link_uri, quote);
        } else {
            write_quoted_string(b, &link_text, false);
        }
        return;
    }
    write_quoted_string(
        b,
        &(get_entity_name_string(name) + if text.is_empty() { "" } else { " " } + text),
        quote && is_markdown,
    );
}

// hover.go:1111
fn trim_comment_prefix(text: &str) -> &str {
    let t = text.trim_start_matches(' ');
    let t = t.strip_prefix('|').unwrap_or(t);
    t.trim_start_matches(' ')
}

// hover.go:1115
fn write_markdown_link(b: &mut String, text: &str, uri: &str, quote: bool) {
    b.push('[');
    write_quoted_string(b, text, quote);
    b.push_str("](");
    b.push_str(uri);
    b.push(')');
}

// hover.go:1123
fn write_optional_entity_name(b: &mut String, name: Option<P<Node>>) {
    if let Some(name) = name {
        b.push(' ');
        write_quoted_string(b, &get_entity_name_string(name), true /*quote*/);
    }
}

// hover.go:1130
fn write_quoted_string(b: &mut String, str: &str, quote: bool) {
    if quote && !str.contains('`') {
        b.push('`');
        b.push_str(str);
        b.push('`');
    } else {
        b.push_str(str);
    }
}

// hover.go:1140
fn get_entity_name_string(name: P<Node>) -> String {
    let mut b = String::new();
    write_entity_name_parts(&mut b, name);
    b
}

// hover.go:1146
fn write_entity_name_parts(b: &mut String, node: P<Node>) {
    match node.kind() {
        Kind::Identifier => b.push_str(node.text()),
        Kind::QualifiedName => {
            write_entity_name_parts(b, node.as_qualified_name().left);
            b.push('.');
            write_entity_name_parts(b, node.as_qualified_name().right);
        }
        Kind::PropertyAccessExpression => {
            write_entity_name_parts(b, node.expression().unwrap());
            b.push('.');
            write_entity_name_parts(b, node.name().unwrap());
        }
        Kind::ParenthesizedExpression | Kind::ExpressionWithTypeArguments => {
            write_entity_name_parts(b, node.expression().unwrap());
        }
        Kind::JSDocNameReference => write_entity_name_parts(b, node.name().unwrap()),
        _ => {}
    }
}
