use tsrs_ast::{self as ast, CheckFlags, Kind, Node, NodeList, SourceFile, Symbol, SymbolFlags};
use tsrs_checker::{
    self as checker, Checker, ContextFlags, ElementFlags, Flags, InternalFlags, Signature,
    SymbolFormatFlags, Type,
};
use tsrs_compiler::Program;
use tsrs_core::context::Context;
use tsrs_core::{NewLineKind, P, TextRange};
use tsrs_lsproto as lsproto;
use tsrs_printer::{self as printer, EmitTextWriter, Printer, PrinterOptions};
use tsrs_scanner as scanner;

use crate::astnav;
use crate::displaypartswriter::{DisplayPartsWriter, new_display_parts_writer};
use crate::hover::get_documentation_from_declaration;
use crate::languageservice::LanguageService;
use crate::spanmap::Feature;
use crate::utilities::{
    find_containing_list, get_children_from_non_jsdoc_node, get_possible_generic_signatures,
    get_possible_type_arguments_info, is_in_comment, is_in_string, is_inside_template_literal,
    is_no_substitution_template_literal, is_tagged_template_expression, is_template_head,
    is_template_tail, range_contains_range,
};

// SignatureHelpTriggerCharacters and SignatureHelpRetriggerCharacters are the characters that trigger and
// re-trigger signature help. They are advertised both in the static server capabilities and in the dynamic
// content-mapper registration, so they live here to keep those two declarations in sync.
// signaturehelp.go:24
pub const SIGNATURE_HELP_TRIGGER_CHARACTERS: [&str; 3] = ["(", ",", "<"];
pub const SIGNATURE_HELP_RETRIGGER_CHARACTERS: [&str; 1] = [")"];

// signaturehelp.go:29
#[derive(Clone, Copy)]
pub struct CallInvocation {
    pub(crate) node: P<Node>,
}

// signaturehelp.go:33
#[derive(Clone, Copy)]
pub struct TypeArgsInvocation {
    called: P<Node>,
}

// signaturehelp.go:37
#[derive(Clone, Copy)]
pub struct ContextualInvocation {
    signature: P<Signature>,
    node: P<Node>, // Just for enclosingDeclaration for printing types
    symbol: P<Symbol>,
}

// signaturehelp.go:43
#[derive(Clone, Copy, Default)]
pub struct Invocation {
    pub(crate) call_invocation: Option<CallInvocation>,
    type_args_invocation: Option<TypeArgsInvocation>,
    contextual_invocation: Option<ContextualInvocation>,
}

impl LanguageService {
    // signaturehelp.go:49
    pub fn provide_signature_help(
        &self,
        ctx: &Context,
        document_uri: &lsproto::DocumentUri,
        position: lsproto::Position,
        context: Option<&lsproto::SignatureHelpContext>,
    ) -> Result<lsproto::SignatureHelpResponse, lsproto::Error> {
        let (program, source_file) = self.get_program_and_file(document_uri);
        let positions = self.converters.from_lsp_position_for_source_file(
            source_file,
            position,
            Feature::SignatureHelp,
        );
        if let Some(result) = positions.into_iter().find_map(|projection| {
            if !projection.fidelity.is_single_segment() {
                return None;
            }
            let items = self.get_signature_help_items(
                ctx,
                projection.position,
                program,
                projection.script,
                context,
            );
            if let Some(items) = items {
                return Some(Ok(lsproto::SignatureHelpOrNull { signature_help: Some(items) }));
            }

            None
        }) {
            return result;
        }
        Ok(lsproto::SignatureHelpOrNull::default())
    }

    // signaturehelp.go:75
    fn get_signature_help_items(
        &self,
        ctx: &Context,
        position: u32,
        program: &'static Program,
        source_file: P<SourceFile>,
        context: Option<&lsproto::SignatureHelpContext>,
    ) -> Option<lsproto::SignatureHelp> {
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum SignatureHelpTriggerReasonKind {
            None,           // was undefined
            Invoked,        // was "invoked"
            CharacterTyped, // was "characterTyped"
            Retriggered,    // was "retrigger"
        }

        let mut checker_handle = program.get_type_checker_for_file(ctx, source_file);
        let type_checker: &mut Checker = &mut checker_handle;

        // Decide whether to show signature help
        // We are at the beginning of the file
        let starting_token = astnav::find_preceding_token(source_file, position)?;

        // Emulate VS Code's toTsTriggerReason.
        let mut trigger_reason_kind = SignatureHelpTriggerReasonKind::None;
        if let Some(context) = context {
            trigger_reason_kind = match context.trigger_kind {
                lsproto::SignatureHelpTriggerKind::TRIGGER_CHARACTER => {
                    if context.trigger_character.is_some() {
                        if context.is_retrigger {
                            SignatureHelpTriggerReasonKind::Retriggered
                        } else {
                            SignatureHelpTriggerReasonKind::CharacterTyped
                        }
                    } else {
                        SignatureHelpTriggerReasonKind::Invoked
                    }
                }
                lsproto::SignatureHelpTriggerKind::CONTENT_CHANGE => {
                    if context.is_retrigger {
                        SignatureHelpTriggerReasonKind::Retriggered
                    } else {
                        SignatureHelpTriggerReasonKind::CharacterTyped
                    }
                }
                lsproto::SignatureHelpTriggerKind::INVOKED => {
                    SignatureHelpTriggerReasonKind::Invoked
                }
                _ => SignatureHelpTriggerReasonKind::Invoked,
            };
        }

        // Only need to be careful if the user typed a character and signature help wasn't showing.
        let only_use_syntactic_owners =
            trigger_reason_kind == SignatureHelpTriggerReasonKind::CharacterTyped;

        // Bail out quickly in the middle of a string or comment, don't provide signature help unless the user explicitly requested it.
        if only_use_syntactic_owners
            && (is_in_string(source_file, position, Some(starting_token))
                || is_in_comment(source_file, position, starting_token).is_some())
        {
            return None;
        }

        let is_manually_invoked = trigger_reason_kind == SignatureHelpTriggerReasonKind::Invoked;
        let argument_info = get_containing_argument_info(
            starting_token,
            source_file,
            type_checker,
            is_manually_invoked,
            position,
        )?;

        if ctx.err().is_some() {
            return None;
        }

        // Extra syntactic and semantic filtering of signature help
        let candidate_info = get_candidate_or_type_info(
            &argument_info,
            type_checker,
            source_file,
            starting_token,
            only_use_syntactic_owners,
        );

        if ctx.err().is_some() {
            return None;
        }

        let Some(candidate_info) = candidate_info else {
            // For JS files, try a fallback that searches all source files for declarations
            // with matching names that have call signatures. This is a heuristic for untyped JS code.
            if ast::is_source_file_js(source_file) {
                return self.create_js_signature_help_items(
                    ctx,
                    &argument_info,
                    program,
                    type_checker,
                );
            }
            return None;
        };

        // return typeChecker.runWithCancellationToken(cancellationToken, typeChecker =>
        if let Some(candidate_info) = candidate_info.candidate_info {
            return self.create_signature_help_items(
                ctx,
                &candidate_info.candidates,
                candidate_info.resolved_signature,
                &argument_info,
                source_file,
                type_checker,
                only_use_syntactic_owners,
            );
        }
        create_type_help_items(
            ctx,
            candidate_info.type_info.unwrap(),
            &argument_info,
            source_file,
            type_checker,
        )
    }
}

// signaturehelp.go:169
fn create_type_help_items(
    ctx: &Context,
    symbol: P<Symbol>,
    argument_info: &ArgumentListInfo,
    source_file: P<SourceFile>,
    c: &mut Checker,
) -> Option<lsproto::SignatureHelp> {
    let type_parameters =
        c.get_local_type_parameters_of_class_or_interface_or_type_alias_exported(symbol);
    if type_parameters.is_empty() {
        return None;
    }
    let item = get_type_help_item(
        symbol,
        &type_parameters,
        get_enclosing_declaration_from_invocation(&argument_info.invocation),
        source_file,
        c,
    );

    // Check client capabilities for activeParameter handling
    let caps = lsproto::get_client_capabilities(ctx);
    let sig_info_caps = &caps.text_document.signature_help.signature_information;
    let supports_per_signature_active_param = sig_info_caps.active_parameter_support;

    // Converting signatureHelpParameter to *lsproto.ParameterInformation
    let parameters: Vec<lsproto::ParameterInformation> =
        item.parameters.iter().map(|param| param.parameter_info.clone()).collect();

    let mut sig_info = lsproto::SignatureInformation {
        label: item.label.clone(),
        documentation: None,
        parameters: Some(parameters),
        ..Default::default()
    };

    // If client supports per-signature activeParameter, set it on SignatureInformation
    if supports_per_signature_active_param && !item.parameters.is_empty() {
        sig_info.active_parameter =
            Some(lsproto::UintegerOrNull { uinteger: Some(argument_info.argument_index as u32) });
    }

    let mut help = lsproto::SignatureHelp {
        signatures: vec![sig_info],
        active_signature: Some(0),
        ..Default::default()
    };

    // If client doesn't support per-signature activeParameter, set it on the top-level SignatureHelp
    if !supports_per_signature_active_param && !item.parameters.is_empty() {
        help.active_parameter =
            Some(lsproto::UintegerOrNull { uinteger: Some(argument_info.argument_index as u32) });
    }

    Some(help)
}

// signaturehelp.go:211
fn get_type_help_item(
    symbol: P<Symbol>,
    type_parameter: &[P<Type>],
    enclosing_declaration: P<Node>,
    source_file: P<SourceFile>,
    c: &mut Checker,
) -> SignatureInformationBuilder {
    let emit_context = printer::new_emit_context();
    let builder = checker::new_node_builder(c, emit_context);
    let mut printer = printer::new_printer(
        PrinterOptions { new_line: NewLineKind::LF, ..Default::default() },
        printer::PrintHandlers::default(),
        Some(emit_context),
    );

    let mut parameters: Vec<SignatureHelpParameter> = Vec::with_capacity(type_parameter.len());
    parameters.extend(type_parameter.iter().map(|&type_param| {
        create_signature_help_parameter_for_type_parameter(
            type_param,
            source_file,
            enclosing_declaration,
            builder,
            c,
            &mut printer,
        )
    }));

    // Creating display label
    let mut display_parts = String::new();
    display_parts.push_str(&c.symbol_to_string_exported(symbol));
    if !parameters.is_empty() {
        display_parts.push_str(scanner::token_to_string(Kind::LessThanToken));
        display_parts.extend(parameters.iter().enumerate().flat_map(|(i, type_parameter)| {
            [
                if i > 0 { ", " } else { "" },
                type_parameter.parameter_info.label.string.as_deref().unwrap(),
            ]
        }));
        display_parts.push_str(scanner::token_to_string(Kind::GreaterThanToken));
    }

    SignatureInformationBuilder {
        label: display_parts,
        documentation: None,
        parameters,
        is_variadic: false,
        colorized_runs: Vec::new(),
    }
}

impl LanguageService {
    // createJSSignatureHelpItems is a fallback for JavaScript files when normal signature help
    // doesn't produce results. It searches all source files for declarations with matching names
    // that have call signatures.
    // signaturehelp.go:244
    fn create_js_signature_help_items(
        &self,
        ctx: &Context,
        argument_info: &ArgumentListInfo,
        program: &'static Program,
        c: &mut Checker,
    ) -> Option<lsproto::SignatureHelp> {
        if argument_info.invocation.contextual_invocation.is_some() {
            return None;
        }
        // See if we can find some symbol with the call expression name that has call signatures.
        let expression = get_expression_from_invocation(argument_info);
        if !ast::is_property_access_expression(expression) {
            return None;
        }
        let name = expression.name().unwrap().text();
        if name.is_empty() {
            return None;
        }

        if let Some(result) = program.get_source_files().iter().find_map(|&sf| {
            let result =
                self.find_signature_help_from_named_declarations(ctx, sf, name, argument_info, c);
            if result.is_some() {
                return Some(result);
            }

            None
        }) {
            return result;
        }
        None
    }

    // signaturehelp.go:267
    fn find_signature_help_from_named_declarations(
        &self,
        ctx: &Context,
        source_file: P<SourceFile>,
        name: &str,
        argument_info: &ArgumentListInfo,
        c: &mut Checker,
    ) -> Option<lsproto::SignatureHelp> {
        fn visit(
            l: &LanguageService,
            ctx: &Context,
            node: P<Node>,
            source_file: P<SourceFile>,
            name: &str,
            argument_info: &ArgumentListInfo,
            c: &mut Checker,
            result: &mut Option<lsproto::SignatureHelp>,
        ) -> bool {
            if result.is_some() {
                return true;
            }
            if ast::get_declaration_name(node) == name
                && let Some(symbol) = node.symbol()
                && let Some(t) = c.get_type_of_symbol_at_location(symbol, Some(node))
            {
                let call_signatures = c.get_call_signatures(t);
                if !call_signatures.is_empty() {
                    *result = l.create_signature_help_items(
                        ctx,
                        call_signatures,
                        Some(call_signatures[0]),
                        argument_info,
                        source_file,
                        c,
                        true, /*useFullPrefix*/
                    );
                    if result.is_some() {
                        return true;
                    }
                }
            }
            node.for_each_child(&mut |child| {
                visit(l, ctx, child, source_file, name, argument_info, c, result)
            });
            result.is_some()
        }
        let mut result: Option<lsproto::SignatureHelp> = None;
        visit(self, ctx, source_file.as_node(), source_file, name, argument_info, c, &mut result);
        result
    }

    // signaturehelp.go:295
    fn create_signature_help_items(
        &self,
        ctx: &Context,
        candidates: &[P<Signature>],
        resolved_signature: Option<P<Signature>>,
        argument_info: &ArgumentListInfo,
        source_file: P<SourceFile>,
        c: &mut Checker,
        use_full_prefix: bool,
    ) -> Option<lsproto::SignatureHelp> {
        let caps = lsproto::get_client_capabilities(ctx);
        let doc_format = lsproto::preferred_markup_kind(
            &caps.text_document.signature_help.signature_information.documentation_format,
        );
        let vs_capability = caps.vs_supports_visual_studio_extensions;

        // Go checks the enclosing declaration for nil; every invocation kind has a node.
        let enclosing_declaration =
            get_enclosing_declaration_from_invocation(&argument_info.invocation);
        let mut call_target_symbol: Option<P<Symbol>>;
        if let Some(contextual_invocation) = argument_info.invocation.contextual_invocation {
            call_target_symbol = Some(contextual_invocation.symbol);
        } else {
            call_target_symbol =
                c.get_symbol_at_location_exported(get_expression_from_invocation(argument_info));
            if call_target_symbol.is_none()
                && use_full_prefix
                && let Some(declaration) = resolved_signature.unwrap().declaration()
            {
                call_target_symbol = declaration.symbol();
            }
        }

        let mut call_target_display_parts = String::new();
        // A contextual signature for an anonymous inline function type (e.g. a callback
        // argument) has a synthetic symbol whose name is an internal marker such as
        // "\xFEtype". There is no meaningful name to show, so render the signature with
        // no prefix (as we already do when there is no call target symbol) rather than
        // leaking the internal name.
        if let Some(call_target_symbol) = call_target_symbol
            && !call_target_symbol.name().starts_with(ast::INTERNAL_SYMBOL_NAME_PREFIX)
        {
            if use_full_prefix {
                call_target_display_parts.push_str(&c.symbol_to_string_ex_exported(
                    call_target_symbol,
                    Some(source_file.as_node()),
                    SymbolFlags::None,
                    SymbolFormatFlags::UseAliasDefinedOutsideCurrentScope,
                ));
            } else {
                call_target_display_parts
                    .push_str(&c.symbol_to_string_exported(call_target_symbol));
            }
        }
        let mut items: Vec<Vec<SignatureInformationBuilder>> = Vec::with_capacity(candidates.len());
        items.extend(candidates.iter().map(|&candidate_signature| {
            self.get_signature_help_item(
                candidate_signature,
                argument_info.is_type_parameter_list,
                &call_target_display_parts,
                call_target_symbol,
                enclosing_declaration,
                source_file,
                c,
                doc_format,
                vs_capability,
            )
        }));

        let mut selected_item_index = 0;
        let mut item_seen = 0;
        for (i, element) in items.iter().enumerate() {
            let item = element;
            if Some(candidates[i]) == resolved_signature {
                selected_item_index = item_seen;
                if item.len() > 1
                    && let Some(count) = item.iter().position(|j| {
                        j.is_variadic || j.parameters.len() >= argument_info.argument_count
                    })
                {
                    selected_item_index = item_seen + count;
                }
            }
            item_seen += item.len();
        }

        let flattened_signatures: Vec<SignatureInformationBuilder> =
            items.into_iter().flatten().collect();
        if flattened_signatures.is_empty() {
            return None;
        }

        // Check client capabilities for activeParameter handling
        let sig_info_caps = &caps.text_document.signature_help.signature_information;
        let supports_per_signature_active_param = sig_info_caps.active_parameter_support;
        let supports_null_active_param = sig_info_caps.no_active_parameter_support;

        // Converting []signatureInformation to []*lsproto.SignatureInformation
        let mut signature_information: Vec<lsproto::SignatureInformation> =
            Vec::with_capacity(flattened_signatures.len());
        for item in &flattened_signatures {
            let parameters: Vec<lsproto::ParameterInformation> =
                item.parameters.iter().map(|param| param.parameter_info.clone()).collect();
            let mut documentation: Option<lsproto::StringOrMarkupContent> = None;
            if let Some(doc) = &item.documentation {
                documentation = Some(lsproto::StringOrMarkupContent {
                    markup_content: Some(lsproto::MarkupContent {
                        kind: doc_format,
                        value: doc.clone(),
                    }),
                    ..Default::default()
                });
            }
            let mut sig_info = lsproto::SignatureInformation {
                label: item.label.clone(),
                documentation,
                parameters: Some(parameters),
                ..Default::default()
            };

            // Set VS-specific colorized label if we have classified runs
            if !item.colorized_runs.is_empty() {
                sig_info.vs_colorized_label = Some(lsproto::VSClassifiedTextElement {
                    runs: item.colorized_runs.clone(),
                    ..Default::default()
                });
            }

            // If client supports per-signature activeParameter, set it on each SignatureInformation
            if supports_per_signature_active_param {
                sig_info.active_parameter = Self::compute_active_parameter(
                    item,
                    argument_info.argument_index,
                    supports_null_active_param,
                );
            }

            signature_information.push(sig_info);
        }

        let mut help = lsproto::SignatureHelp {
            signatures: signature_information,
            active_signature: Some(selected_item_index as u32),
            ..Default::default()
        };

        // If client doesn't support per-signature activeParameter, set it on the top-level SignatureHelp
        if !supports_per_signature_active_param {
            let active_signature = &flattened_signatures[selected_item_index];
            help.active_parameter = Self::compute_active_parameter(
                active_signature,
                argument_info.argument_index,
                supports_null_active_param,
            );
        }

        Some(help)
    }

    // computeActiveParameter calculates the active parameter index for a signature,
    // handling variadic signatures and null support appropriately.
    // signaturehelp.go:419
    fn compute_active_parameter(
        sig: &SignatureInformationBuilder,
        argument_index: usize,
        supports_null: bool,
    ) -> Option<lsproto::UintegerOrNull> {
        let param_count = sig.parameters.len();
        if param_count == 0 {
            // No parameters, return nil (omit the field)
            return None;
        }

        let mut active_param = u32::try_from(argument_index).expect("argument index exceeds u32");

        if sig.is_variadic {
            let first_rest = sig.parameters.iter().position(|p| p.is_rest);
            if first_rest.is_some_and(|first_rest| first_rest < param_count - 1) {
                // Middle rest parameter - we can't accurately highlight, so indicate "no active parameter"
                if supports_null {
                    return Some(lsproto::UintegerOrNull::default()); // null means "no parameter is active"
                }
                // Client doesn't support null, use out-of-range index (defaults to 0 per LSP spec)
                return Some(lsproto::UintegerOrNull {
                    uinteger: Some(
                        u32::try_from(param_count).expect("parameter count exceeds u32"),
                    ),
                });
            }
            // Clamp to last parameter for trailing rest parameters
            if active_param > (param_count - 1) as u32 {
                active_param = (param_count - 1) as u32;
            }
        }

        Some(lsproto::UintegerOrNull { uinteger: Some(active_param) })
    }

    // signaturehelp.go:449
    fn get_signature_help_item(
        &self,
        candidate: P<Signature>,
        is_type_parameter_list: bool,
        call_target_symbol: &str,
        call_target_sym: Option<P<Symbol>>,
        enclosing_declaration: P<Node>,
        source_file: P<SourceFile>,
        c: &mut Checker,
        doc_format: lsproto::MarkupKind,
        vs_capability: bool,
    ) -> Vec<SignatureInformationBuilder> {
        let emit_context = printer::new_emit_context();
        let infos = if is_type_parameter_list {
            Self::item_info_for_type_parameters(
                candidate,
                c,
                enclosing_declaration,
                source_file,
                doc_format,
                vs_capability,
                emit_context,
            )
        } else {
            self.item_info_for_parameters(
                candidate,
                c,
                enclosing_declaration,
                source_file,
                doc_format,
                vs_capability,
                emit_context,
            )
        };

        let suffix_dpw = return_type_to_display_parts(
            candidate,
            c,
            enclosing_declaration,
            source_file,
            vs_capability,
            emit_context,
        );

        // Generate documentation from the signature's declaration
        let mut documentation: Option<String> = None;
        if let Some(declaration) = candidate.declaration() {
            let mapper = self.documentation_location_mapper(Feature::SignatureHelp);
            let doc = get_documentation_from_declaration(
                &mapper,
                c,
                None,
                Some(declaration),
                None,
                doc_format,
                true, /*commentOnly*/
            );
            if !doc.is_empty() {
                documentation = Some(doc);
            }
        }

        let mut result: Vec<SignatureInformationBuilder> = Vec::with_capacity(infos.len());
        result.extend(infos.into_iter().map(|info| {
            let mut label_dpw = new_display_parts_writer(vs_capability);
            if !call_target_symbol.is_empty() {
                label_dpw.write_symbol(call_target_symbol, call_target_sym.unwrap());
            }
            label_dpw.write_from(&info.writer);
            label_dpw.write_from(&suffix_dpw);

            SignatureInformationBuilder {
                label: label_dpw.string(),
                documentation: documentation.clone(),
                parameters: info.parameters,
                is_variadic: info.is_variadic,
                colorized_runs: label_dpw.get_runs().to_vec(),
            }
        }));
        result
    }
}

// signaturehelp.go:488
fn return_type_to_display_parts(
    candidate_signature: P<Signature>,
    c: &mut Checker,
    enclosing_declaration: P<Node>,
    source_file: P<SourceFile>,
    vs_capability: bool,
    emit_context: P<printer::EmitContext>,
) -> DisplayPartsWriter {
    let mut dpw = new_display_parts_writer(vs_capability);

    // Add ": " prefix
    dpw.write_punctuation(": ");

    let predicate = c.get_type_predicate_of_signature_exported(candidate_signature);
    if let Some(predicate) = predicate {
        dpw.write(&c.type_predicate_to_string_exported(predicate));
    } else {
        let return_type = c.get_return_type_of_signature_exported(candidate_signature);
        let type_node = c.type_to_type_node(
            return_type,
            Some(enclosing_declaration),
            SIGNATURE_HELP_NODE_BUILDER_FLAGS,
            None,
        );
        if let Some(type_node) = type_node {
            let mut p = printer::new_printer(
                PrinterOptions { new_line: NewLineKind::LF, ..Default::default() },
                printer::PrintHandlers::default(),
                Some(emit_context),
            );
            // Use a temporary writer for p.Write since the printer calls Clear() on its writer
            let mut temp_dpw = new_display_parts_writer(vs_capability);
            p.write(type_node, Some(source_file), &mut temp_dpw, None);
            dpw.write_from(&temp_dpw);
        } else {
            dpw.write(&c.type_to_string_exported(return_type));
        }
    }
    dpw
}

impl LanguageService {
    // signaturehelp.go:513
    fn item_info_for_type_parameters(
        candidate_signature: P<Signature>,
        c: &mut Checker,
        enclosing_declaration: P<Node>,
        source_file: P<SourceFile>,
        _doc_format: lsproto::MarkupKind,
        vs_capability: bool,
        emit_context: P<printer::EmitContext>,
    ) -> Vec<SignatureHelpItemInfo> {
        let builder = checker::new_node_builder(c, emit_context);
        let mut p = printer::new_printer(
            PrinterOptions { new_line: NewLineKind::LF, ..Default::default() },
            printer::PrintHandlers::default(),
            Some(emit_context),
        );

        let type_parameters: &[P<Type>] = if let Some(target) = candidate_signature.target() {
            target.type_parameters()
        } else {
            candidate_signature.type_parameters()
        };
        let mut signature_help_type_parameters: Vec<SignatureHelpParameter> =
            Vec::with_capacity(type_parameters.len());
        signature_help_type_parameters.extend(type_parameters.iter().map(|&type_parameter| {
            create_signature_help_parameter_for_type_parameter(
                type_parameter,
                source_file,
                enclosing_declaration,
                builder,
                c,
                &mut p,
            )
        }));

        // Creating type parameter display label
        let mut dpw = new_display_parts_writer(vs_capability);

        let less_than_token = scanner::token_to_string(Kind::LessThanToken);
        dpw.write_punctuation(less_than_token);
        for (i, type_parameter) in signature_help_type_parameters.iter().enumerate() {
            if i > 0 {
                dpw.write_punctuation(", ");
            }
            let label = type_parameter.parameter_info.label.string.as_ref().unwrap();
            dpw.write_classified(label, lsproto::ClassificationTypeName::TYPE_PARAMETER_NAME);
        }
        let greater_than_token = scanner::token_to_string(Kind::GreaterThanToken);
        dpw.write_punctuation(greater_than_token);

        // Creating display label for parameters like, (a: string, b: number)
        let lists = c.get_expanded_parameters_exported(candidate_signature, false);
        if !lists.is_empty() {
            let open_paren = scanner::token_to_string(Kind::OpenParenToken);
            dpw.write_punctuation(open_paren);
        }

        let mut result: Vec<SignatureHelpItemInfo> = Vec::with_capacity(lists.len());
        for parameter_list in &lists {
            let mut param_dpw = new_display_parts_writer(vs_capability);
            param_dpw.write_from(&dpw);

            for (j, &param) in parameter_list.iter().enumerate() {
                let param_node = builder
                    .symbol_to_parameter_declaration(
                        c,
                        param,
                        Some(enclosing_declaration),
                        SIGNATURE_HELP_NODE_BUILDER_FLAGS,
                        InternalFlags::None,
                        None,
                    )
                    .unwrap();

                if j > 0 {
                    param_dpw.write_punctuation(", ");
                }
                // Use a temporary writer for p.Write since the printer calls Clear() on its writer
                let mut temp_dpw = new_display_parts_writer(vs_capability);
                p.write(param_node, Some(source_file), &mut temp_dpw, None);
                param_dpw.write_from(&temp_dpw);
            }
            let close_paren = scanner::token_to_string(Kind::CloseParenToken);
            param_dpw.write_punctuation(close_paren);

            result.push(SignatureHelpItemInfo {
                is_variadic: false,
                parameters: signature_help_type_parameters.clone(),
                writer: param_dpw,
            });
        }
        result
    }

    // signaturehelp.go:588
    fn item_info_for_parameters(
        &self,
        candidate_signature: P<Signature>,
        c: &mut Checker,
        enclosing_declaratipn: P<Node>,
        source_file: P<SourceFile>,
        doc_format: lsproto::MarkupKind,
        vs_capability: bool,
        emit_context: P<printer::EmitContext>,
    ) -> Vec<SignatureHelpItemInfo> {
        let builder = checker::new_node_builder(c, emit_context);
        let mut p = printer::new_printer(
            PrinterOptions { new_line: NewLineKind::LF, ..Default::default() },
            printer::PrintHandlers::default(),
            Some(emit_context),
        );

        let mut signature_help_type_parameters: Vec<SignatureHelpParameter> =
            Vec::with_capacity(candidate_signature.type_parameters().len());
        if !candidate_signature.type_parameters().is_empty() {
            signature_help_type_parameters.extend(
                candidate_signature.type_parameters().iter().map(|&type_parameter| {
                    create_signature_help_parameter_for_type_parameter(
                        type_parameter,
                        source_file,
                        enclosing_declaratipn,
                        builder,
                        c,
                        &mut p,
                    )
                }),
            );
        }

        // Creating display label for type parameters like, <T, U>
        let mut dpw = new_display_parts_writer(vs_capability);

        if !signature_help_type_parameters.is_empty() {
            let less_than_token = scanner::token_to_string(Kind::LessThanToken);
            dpw.write_punctuation(less_than_token);
            for (i, type_parameter) in signature_help_type_parameters.iter().enumerate() {
                if i > 0 {
                    dpw.write_punctuation(", ");
                }
                let label = type_parameter.parameter_info.label.string.as_ref().unwrap();
                dpw.write_classified(label, lsproto::ClassificationTypeName::TYPE_PARAMETER_NAME);
            }
            let greater_than_token = scanner::token_to_string(Kind::GreaterThanToken);
            dpw.write_punctuation(greater_than_token);
        }

        // Creating display parts for parameters. For example, (a: string, b: number)
        let lists = c.get_expanded_parameters_exported(candidate_signature, false);
        if !lists.is_empty() {
            let open_paren = scanner::token_to_string(Kind::OpenParenToken);
            dpw.write_punctuation(open_paren);
        }

        let lists_len = lists.len();
        let is_variadic = |c: &mut Checker, parameter_list: &[P<Symbol>]| -> bool {
            if !c.has_effective_rest_parameter_exported(candidate_signature) {
                return false;
            }
            if lists_len == 1 {
                return true;
            }
            !parameter_list.is_empty()
                && parameter_list[parameter_list.len() - 1]
                    .check_flags()
                    .intersects(CheckFlags::RestParameter)
        };

        let mut result: Vec<SignatureHelpItemInfo> = Vec::with_capacity(lists.len());
        for parameter_list in &lists {
            let mut parameters: Vec<SignatureHelpParameter> =
                Vec::with_capacity(parameter_list.len());
            let mut param_dpw = new_display_parts_writer(vs_capability);
            param_dpw.write_from(&dpw);

            for (j, &param) in parameter_list.iter().enumerate() {
                let param_node = builder
                    .symbol_to_parameter_declaration(
                        c,
                        param,
                        Some(enclosing_declaratipn),
                        SIGNATURE_HELP_NODE_BUILDER_FLAGS,
                        InternalFlags::None,
                        None,
                    )
                    .unwrap();

                if j > 0 {
                    param_dpw.write_punctuation(", ");
                }
                // Use a temporary writer for p.Write since the printer calls Clear() on its writer
                let mut temp_dpw = new_display_parts_writer(vs_capability);
                p.write(param_node, Some(source_file), &mut temp_dpw, None);
                let param_label = temp_dpw.string();
                param_dpw.write_from(&temp_dpw);

                let parameter = self.create_signature_help_parameter_from_label(
                    param,
                    &param_label,
                    c,
                    doc_format,
                );
                parameters.push(parameter);
            }
            let close_paren = scanner::token_to_string(Kind::CloseParenToken);
            param_dpw.write_punctuation(close_paren);

            result.push(SignatureHelpItemInfo {
                is_variadic: is_variadic(c, parameter_list),
                parameters,
                writer: param_dpw,
            });
        }
        result
    }
}

// signaturehelp.go:666
const SIGNATURE_HELP_NODE_BUILDER_FLAGS: Flags = Flags::OmitParameterModifiers
    .union(Flags::IgnoreErrors)
    .union(Flags::UseAliasDefinedOutsideCurrentScope);

impl LanguageService {
    // createSignatureHelpParameterFromLabel creates a signatureHelpParameter from a pre-computed label string.
    // signaturehelp.go:669
    fn create_signature_help_parameter_from_label(
        &self,
        parameter: P<Symbol>,
        label: &str,
        c: &mut Checker,
        doc_format: lsproto::MarkupKind,
    ) -> SignatureHelpParameter {
        let is_rest = parameter.check_flags().intersects(CheckFlags::RestParameter);
        let mut documentation: Option<lsproto::StringOrMarkupContent> = None;
        if let Some(value_declaration) = parameter.value_declaration() {
            let mapper = self.documentation_location_mapper(Feature::SignatureHelp);
            let doc = get_documentation_from_declaration(
                &mapper,
                c,
                None,
                Some(value_declaration),
                None,
                doc_format,
                true, /*commentOnly*/
            );
            if !doc.is_empty() {
                documentation = Some(lsproto::StringOrMarkupContent {
                    markup_content: Some(lsproto::MarkupContent { kind: doc_format, value: doc }),
                    ..Default::default()
                });
            }
        }
        SignatureHelpParameter {
            parameter_info: lsproto::ParameterInformation {
                label: lsproto::StringOrTuple {
                    string: Some(label.to_string()),
                    ..Default::default()
                },
                documentation,
            },
            is_rest,
        }
    }
}

// signaturehelp.go:699
fn create_signature_help_parameter_for_type_parameter(
    t: P<Type>,
    source_file: P<SourceFile>,
    enclosing_declaration: P<Node>,
    builder: P<checker::NodeBuilder>,
    c: &mut Checker,
    p: &mut Printer,
) -> SignatureHelpParameter {
    let node = builder
        .type_parameter_to_declaration(
            c,
            t,
            Some(enclosing_declaration),
            SIGNATURE_HELP_NODE_BUILDER_FLAGS,
            InternalFlags::None,
            None,
        )
        .unwrap();
    let display = p.emit(node, Some(source_file));
    SignatureHelpParameter {
        parameter_info: lsproto::ParameterInformation {
            label: lsproto::StringOrTuple { string: Some(display), ..Default::default() },
            ..Default::default()
        },
        is_rest: false,
    }
}

// Represents the signature of something callable. A signature
// can have a label, like a function-name, a doc-comment, and
// a set of parameters.
// signaturehelp.go:713
struct SignatureInformationBuilder {
    // The Label of this signature. Will be shown in
    // the UI.
    label: String,
    // The human-readable doc-comment of this signature. Will be shown
    // in the UI but can be omitted.
    documentation: Option<String>,
    // The Parameters of this signature.
    parameters: Vec<SignatureHelpParameter>,
    // Needed only here, not in lsp
    is_variadic: bool,
    // Classified text runs for VS colorized label
    colorized_runs: Vec<lsproto::VSClassifiedTextRun>,
}

// signaturehelp.go:728
struct SignatureHelpItemInfo {
    is_variadic: bool,
    parameters: Vec<SignatureHelpParameter>,
    writer: DisplayPartsWriter,
}

// signaturehelp.go:734 (Go's isOptional field is never read)
#[derive(Clone)]
struct SignatureHelpParameter {
    parameter_info: lsproto::ParameterInformation,
    is_rest: bool,
}

// signaturehelp.go:740
fn get_enclosing_declaration_from_invocation(invocation: &Invocation) -> P<Node> {
    if let Some(call_invocation) = invocation.call_invocation {
        call_invocation.node
    } else if let Some(type_args_invocation) = invocation.type_args_invocation {
        type_args_invocation.called
    } else {
        invocation.contextual_invocation.unwrap().node
    }
}

// signaturehelp.go:750
fn get_expression_from_invocation(argument_info: &ArgumentListInfo) -> P<Node> {
    if let Some(call_invocation) = argument_info.invocation.call_invocation {
        return ast::get_invoked_expression(call_invocation.node);
    }
    argument_info.invocation.type_args_invocation.unwrap().called
}

// signaturehelp.go:757
struct CandidateInfo {
    candidates: Vec<P<Signature>>,
    resolved_signature: Option<P<Signature>>,
}

// signaturehelp.go:762
struct CandidateOrTypeInfo {
    candidate_info: Option<CandidateInfo>,
    type_info: Option<P<Symbol>>,
}

// signaturehelp.go:767
fn get_candidate_or_type_info(
    info: &ArgumentListInfo,
    c: &mut Checker,
    source_file: P<SourceFile>,
    starting_token: P<Node>,
    only_use_syntactic_owners: bool,
) -> Option<CandidateOrTypeInfo> {
    if let Some(call_invocation) = info.invocation.call_invocation {
        if only_use_syntactic_owners
            && !is_syntactic_owner(starting_token, call_invocation.node, source_file)
        {
            return None;
        }

        let (resolved_signature, candidates) = checker::get_resolved_signature_for_signature_help(
            call_invocation.node,
            info.argument_count,
            c,
        );
        if candidates.is_empty() {
            return None;
        }

        return Some(CandidateOrTypeInfo {
            candidate_info: Some(CandidateInfo { candidates, resolved_signature }),
            type_info: None,
        });
    }
    if let Some(type_args_invocation) = info.invocation.type_args_invocation {
        let called = type_args_invocation.called;
        let mut container = called;
        if ast::is_identifier(called) {
            container = called.parent().unwrap();
        }

        if only_use_syntactic_owners
            && !contains_preceding_token(starting_token, source_file, container)
        {
            return None;
        }

        let candidates = get_possible_generic_signatures(called, info.argument_count, c);
        if !candidates.is_empty() {
            let resolved_signature = Some(candidates[0]);
            return Some(CandidateOrTypeInfo {
                candidate_info: Some(CandidateInfo { candidates, resolved_signature }),
                type_info: None,
            });
        }

        if let Some(symbol) = c.get_symbol_at_location_exported(called) {
            return Some(CandidateOrTypeInfo { candidate_info: None, type_info: Some(symbol) });
        }

        // This can happen in the case of an unresolved symbol.
        return None;
    }

    if let Some(contextual_invocation) = info.invocation.contextual_invocation {
        return Some(CandidateOrTypeInfo {
            candidate_info: Some(CandidateInfo {
                candidates: vec![contextual_invocation.signature],
                resolved_signature: Some(contextual_invocation.signature),
            }),
            type_info: None,
        });
    }
    panic!("Debug Failure. Illegal value: invocation");
}

// signaturehelp.go:828
fn is_syntactic_owner(starting_token: P<Node>, node: P<Node>, source_file: P<SourceFile>) -> bool {
    if !ast::is_call_or_new_expression(node) {
        return false;
    }
    let invocation_children = get_children_from_non_jsdoc_node(node, source_file);
    match starting_token.kind() {
        Kind::OpenParenToken | Kind::CommaToken => invocation_children.contains(&starting_token),
        Kind::LessThanToken => {
            contains_preceding_token(starting_token, source_file, node.expression().unwrap())
        }
        _ => false,
    }
}

// signaturehelp.go:843
fn contains_preceding_token(
    starting_token: P<Node>,
    source_file: P<SourceFile>,
    container: P<Node>,
) -> bool {
    let pos = starting_token.pos().expect_source("parsed tokens have source positions");
    // There's a possibility that `startingToken.parent` contains only `startingToken` and
    // missing nodes, none of which are valid to be returned by `findPrecedingToken`. In that
    // case, the preceding token we want is actually higher up the tree—almost definitely the
    // next parent, but theoretically the situation with missing nodes might be happening on
    // multiple nested levels.
    let mut current_parent = starting_token.parent();
    while let Some(parent) = current_parent {
        let preceding_token = astnav::find_preceding_token_ex(
            source_file,
            pos,
            Some(parent),
            true, /*excludeJSDoc*/
        );
        if let Some(preceding_token) = preceding_token {
            return range_contains_range(container.loc(), preceding_token.loc());
        }
        current_parent = parent.parent();
    }
    false
}

// signaturehelp.go:861
fn get_containing_argument_info(
    node: P<Node>,
    source_file: P<SourceFile>,
    checker: &mut Checker,
    is_manually_invoked: bool,
    position: u32,
) -> Option<ArgumentListInfo> {
    let mut first_argument_info: Option<ArgumentListInfo> = None;
    let mut n = node;
    while !ast::is_source_file(n) && (is_manually_invoked || !ast::is_block(n)) {
        // If the node is not a subspan of its parent, this is a big problem.
        // There have been crashes that might be caused by this violation.
        assert!(
            range_contains_range(n.parent().unwrap().loc(), n.loc()),
            "Not a subspan. Child: {}, parent: {}",
            n.kind_string(),
            n.parent().unwrap().kind_string()
        );
        let argument_info = get_immediately_containing_argument_or_contextual_parameter_info(
            n,
            position,
            source_file,
            checker,
        );
        if let Some(argument_info) = argument_info {
            // For contextual invocations (e.g., arrow functions with contextual types),
            // always return immediately without checking the position.
            // This ensures that when inside a callback's parameter list, we show the callback's
            // signature, not the outer call's signature.
            if argument_info.invocation.contextual_invocation.is_some() {
                return Some(argument_info);
            }

            // Remember the first (innermost) argument info we find
            if first_argument_info.is_none() {
                first_argument_info = Some(argument_info);
            }

            // If the position is at the end boundary of an argument list, keep the
            // innermost call. This covers cases like foo(bar("x"|)) where the cursor is
            // still inside the inner invocation, just before its closing paren.
            if argument_info.arguments_span.end() == position.into() {
                return Some(argument_info);
            }

            // If any call's span contains the position, return it.
            // We walk from inner to outer, so this naturally prefers the innermost call
            // when multiple calls contain the position.
            if argument_info.arguments_span.contains(position.into()) {
                return Some(argument_info);
            }
        }
        n = n.parent().unwrap();
    }

    // No call's span contains the position. Fall back to the innermost call we found.
    // This covers boundary positions that are still syntactically associated with that
    // invocation, such as being at the end of the argument list or on the close paren.
    first_argument_info
}

// signaturehelp.go:904
fn get_immediately_containing_argument_or_contextual_parameter_info(
    node: P<Node>,
    position: u32,
    source_file: P<SourceFile>,
    checker: &mut Checker,
) -> Option<ArgumentListInfo> {
    let result = try_get_parameter_info(node, source_file, checker);
    if result.is_none() {
        return get_immediately_containing_argument_info(node, position, source_file, checker);
    }
    result
}

// signaturehelp.go:912
#[derive(Clone, Copy)]
pub struct ArgumentListInfo {
    pub(crate) is_type_parameter_list: bool,
    pub(crate) invocation: Invocation,
    arguments_span: TextRange,
    pub(crate) argument_index: usize,
    // argumentCount is the *apparent* number of arguments.
    pub(crate) argument_count: usize,
}

// Returns relevant information for the argument list and the current argument if we are
// in the argument of an invocation; returns undefined otherwise.
// signaturehelp.go:923
pub fn get_immediately_containing_argument_info(
    node: P<Node>,
    position: u32,
    source_file: P<SourceFile>,
    c: &mut Checker,
) -> Option<ArgumentListInfo> {
    let parent = node.parent().unwrap();
    if ast::is_call_or_new_expression(parent) {
        // There are 3 cases to handle:
        //   1. The token introduces a list, and should begin a signature help session
        //   2. The token is either not associated with a list, or ends a list, so the session should end
        //   3. The token is buried inside a list, and should give signature help
        //
        // The following are examples of each:
        //
        //    Case 1:
        //          foo<#T, U>(#a, b)    -> The token introduces a list, and should begin a signature help session
        //    Case 2:
        //          fo#o<T, U>#(a, b)#   -> The token is either not associated with a list, or ends a list, so the session should end
        //    Case 3:
        //          foo<T#, U#>(a#, #b#) -> The token is buried inside a list, and should give signature help
        // Find out if 'node' is an argument, a type argument, or neither
        let info = get_argument_or_parameter_list_info(node, source_file, c)?;
        let list = info.list;
        let argument_index = info.argument_index;
        let argument_count = info.argument_count;
        let arguments_span = info.arguments_span;
        let mut is_type_parameter_list = false;
        let parent_type_argument_list = parent.type_argument_list();
        if let Some(parent_type_argument_list) = parent_type_argument_list {
            // Go dereferences `list` here; it is nil only for `foo(` / `foo<` with a missing list.
            if parent_type_argument_list.pos() == list.unwrap().pos() {
                is_type_parameter_list = true;
            }
        }
        return Some(ArgumentListInfo {
            is_type_parameter_list,
            invocation: Invocation {
                call_invocation: Some(CallInvocation { node: parent }),
                ..Default::default()
            },
            arguments_span,
            argument_index,
            argument_count,
        });
    } else if is_no_substitution_template_literal(node) && is_tagged_template_expression(parent) {
        // Check if we're actually inside the template;
        // otherwise we'll fall out and return undefined.
        if is_inside_template_literal(node, position, source_file) {
            return Some(get_argument_list_info_for_template(parent, 0, source_file));
        }
        return None;
    } else if is_template_head(node)
        && parent.parent().unwrap().kind() == Kind::TaggedTemplateExpression
    {
        let template_expression = parent;
        let tag_expression = template_expression.parent().unwrap();

        let mut argument_index = 1;
        if is_inside_template_literal(node, position, source_file) {
            argument_index = 0;
        }
        return Some(get_argument_list_info_for_template(
            tag_expression,
            argument_index,
            source_file,
        ));
    } else if ast::is_template_span(parent)
        && is_tagged_template_expression(parent.parent().unwrap().parent().unwrap())
    {
        let template_span = parent;
        let tag_expression = parent.parent().unwrap().parent().unwrap();

        // If we're just after a template tail, don't show signature help.
        if is_template_tail(node) && !is_inside_template_literal(node, position, source_file) {
            return None;
        }

        let span_index = ast::index_of_node(
            template_span.parent().unwrap().as_template_expression().template_spans.nodes(),
            template_span,
        )?;
        let argument_index =
            get_argument_index_for_template_piece(span_index, node, position, source_file);

        return Some(get_argument_list_info_for_template(
            tag_expression,
            argument_index,
            source_file,
        ));
    } else if ast::is_jsx_opening_like_element(parent) {
        // Provide a signature help for JSX opening element or JSX self-closing element.
        // This is not guarantee that JSX tag-name is resolved into stateless function component. (that is done in "getSignatureHelpItems")
        // i.e
        //      export function MainButton(props: ButtonProps, context: any): JSX.Element { ... }
        //      <MainButton /*signatureHelp*/
        let attribute_span_start = parent
            .attributes()
            .unwrap()
            .loc()
            .pos()
            .expect_source("parsed JSX attributes have source positions");
        let attribute_span_end = scanner::skip_trivia(
            source_file.text(),
            parent
                .attributes()
                .unwrap()
                .end()
                .expect_source("parsed JSX attributes have source positions"),
        );
        return Some(ArgumentListInfo {
            is_type_parameter_list: false,
            invocation: Invocation {
                call_invocation: Some(CallInvocation { node: parent }),
                ..Default::default()
            },
            arguments_span: TextRange::new(attribute_span_start, attribute_span_end),
            argument_index: 0,
            argument_count: 1,
        });
    }
    let type_arg_info = get_possible_type_arguments_info(Some(node), source_file);
    if let Some(type_arg_info) = type_arg_info {
        let called = type_arg_info.called;
        let n_type_arguments = type_arg_info.n_type_arguments;
        let invoc = TypeArgsInvocation { called };
        let argument_range = TextRange::from_text_pos(called.loc().pos(), node.end());
        return Some(ArgumentListInfo {
            is_type_parameter_list: true,
            invocation: Invocation { type_args_invocation: Some(invoc), ..Default::default() },
            arguments_span: argument_range,
            argument_index: n_type_arguments,
            argument_count: n_type_arguments + 1,
        });
    }
    None
}

// spanIndex is either the index for a given template span.
// This does not give appropriate results for a NoSubstitutionTemplateLiteral
// signaturehelp.go:1029
fn get_argument_index_for_template_piece(
    span_index: usize,
    node: P<Node>,
    position: u32,
    source_file: P<SourceFile>,
) -> usize {
    // Because the TemplateStringsArray is the first argument, we have to offset each substitution expression by 1.
    // There are three cases we can encounter:
    //      1. We are precisely in the template literal (argIndex = 0).
    //      2. We are in or to the right of the substitution expression (argIndex = spanIndex + 1).
    //      3. We are directly to the right of the template literal, but because we look for the token on the left,
    //          not enough to put us in the substitution expression; we should consider ourselves part of
    //          the *next* span's expression by offsetting the index (argIndex = (spanIndex + 1) + 1).
    //
    // Example: f  `# abcd $#{#  1 + 1#  }# efghi ${ #"#hello"#  }  #  `
    //              ^       ^ ^       ^   ^          ^ ^      ^     ^
    // Case:        1       1 3       2   1          3 2      2     1
    assert!(
        position >= node.loc().pos().expect_source("parsed template nodes have source positions"),
        "Assumed 'position' could not occur before node."
    );
    if ast::is_template_literal_token(node) {
        if is_inside_template_literal(node, position, source_file) {
            return 0;
        }
        return span_index + 2;
    }
    span_index + 1
}

// signaturehelp.go:1051
fn get_adjusted_node(node: P<Node>) -> Option<P<Node>> {
    match node.kind() {
        Kind::OpenParenToken | Kind::CommaToken => Some(node),
        _ => ast::find_ancestor(node.parent(), |n| {
            if ast::is_parameter_declaration(n) {
                return true;
            } else if ast::is_binding_element(n)
                || ast::is_object_binding_pattern(n)
                || ast::is_array_binding_pattern(n)
            {
                return false;
            }
            false
        }),
    }
}

// signaturehelp.go:1067
struct ContextualSignatureLocationInfo {
    contextual_type: P<Type>,
    argument_index: usize,
    argument_count: usize,
    arguments_span: TextRange,
}

// signaturehelp.go:1074
fn get_spread_element_count(node: P<Node>, c: &mut Checker) -> usize {
    let spread_type = c.get_type_at_location(node.expression().unwrap());
    if checker::is_tuple_type_exported(spread_type) {
        let tuple_type = spread_type.target().unwrap().as_tuple_type();
        let element_flags = tuple_type.element_flags();
        let fixed_length = tuple_type.fixed_length();
        if fixed_length == 0 {
            return 0;
        }

        let first_optional_index =
            element_flags.iter().position(|f| !f.intersects(ElementFlags::Required));
        let Some(first_optional_index) = first_optional_index else {
            return fixed_length;
        };
        return first_optional_index;
    }
    0
}

// signaturehelp.go:1098
fn get_argument_index(
    node: P<Node>,
    arguments: Option<P<NodeList>>,
    source_file: P<SourceFile>,
    c: &mut Checker,
) -> usize {
    get_argument_index_or_count(
        &get_token_from_node_list(arguments, node.parent(), source_file),
        Some(node),
        c,
    )
}

// signaturehelp.go:1102
fn get_argument_count(
    node: P<Node>,
    arguments: Option<P<NodeList>>,
    source_file: P<SourceFile>,
    c: &mut Checker,
) -> usize {
    get_argument_index_or_count(
        &get_token_from_node_list(arguments, node.parent(), source_file),
        None,
        c,
    )
}

// signaturehelp.go:1106
fn get_argument_index_or_count(
    arguments: &[P<Node>],
    node: Option<P<Node>>,
    c: &mut Checker,
) -> usize {
    let mut argument_index = 0;
    let mut skip_comma = false;
    for &arg in arguments {
        if node.is_some() && Some(arg) == node {
            if !skip_comma && arg.kind() == Kind::CommaToken {
                argument_index += 1;
            }
            return argument_index;
        }
        if ast::is_spread_element(arg) {
            argument_index += get_spread_element_count(arg, c);
            skip_comma = true;
            continue;
        }
        if arg.kind() != Kind::CommaToken {
            argument_index += 1;
            skip_comma = true;
            continue;
        }
        if skip_comma {
            skip_comma = false;
            continue;
        }
        argument_index += 1;
    }
    if node.is_some() {
        return argument_index;
    }
    // The argument count for a list is normally the number of non-comma children it has.
    // For example, if you have "Foo(a,b)" then there will be three children of the arg
    // list 'a' '<comma>' 'b'. So, in this case the arg count will be 2. However, there
    // is a small subtlety. If you have "Foo(a,)", then the child list will just have
    // 'a' '<comma>'. So, in the case where the last child is a comma, we increase the
    // arg count by one to compensate.
    let mut argument_count = argument_index;
    if !arguments.is_empty() && arguments[arguments.len() - 1].kind() == Kind::CommaToken {
        argument_count = argument_index + 1;
    }
    argument_count
}

// signaturehelp.go:1148
struct ArgumentOrParameterListInfo {
    list: Option<P<NodeList>>,
    argument_index: usize,
    argument_count: usize,
    arguments_span: TextRange,
}

// signaturehelp.go:1155
fn get_argument_or_parameter_list_info(
    node: P<Node>,
    source_file: P<SourceFile>,
    c: &mut Checker,
) -> Option<ArgumentOrParameterListInfo> {
    let info = get_argument_or_parameter_list_and_index(node, source_file, c)?;
    let list = info.list;
    let argument_index = info.argument_index;
    let argument_count = get_argument_count(node, list, source_file, c);
    let arguments_span = get_applicable_span_for_arguments(list, Some(node), source_file);
    Some(ArgumentOrParameterListInfo { list, argument_index, argument_count, arguments_span })
}

// signaturehelp.go:1172
fn get_applicable_span_for_arguments(
    argument_list: Option<P<NodeList>>,
    node: Option<P<Node>>,
    source_file: P<SourceFile>,
) -> TextRange {
    // We use full start and skip trivia on the end because we want to include trivia on
    // both sides. For example,
    //
    //    foo(   /*comment */     a, b, c      /*comment*/     )
    //        |                                               |
    //
    // The applicable span is from the first bar to the second bar (inclusive,
    // but not including parentheses).
    if argument_list.is_none()
        && let Some(node) = node
    {
        // If the user has just opened a list, and there are no arguments.
        // For example, foo(    )
        //                  |  |
        // The span should include positions inside the parentheses.
        let span_start = node.end().expect_source("parsed nodes have source positions");
        let mut span_end = scanner::skip_trivia(source_file.text(), span_start);
        span_end = ensure_minimum_span_size(span_start, span_end);
        return TextRange::new(span_start, span_end);
    }
    let argument_list = argument_list.unwrap();
    let applicable_span_start =
        argument_list.loc.get().pos().expect_source("parsed argument lists have source positions");
    let mut applicable_span_end = scanner::skip_trivia(
        source_file.text(),
        argument_list.loc.get().end().expect_source("parsed argument lists have source positions"),
    );

    // If the argument list is empty (Pos == End), extend the span to include at least
    // one position. This handles foo(|) where the cursor is right after the opening paren.
    applicable_span_end = ensure_minimum_span_size(applicable_span_start, applicable_span_end);

    TextRange::new(applicable_span_start, applicable_span_end)
}

// ensureMinimumSpanSize ensures that a span includes at least one position.
// TextRange.Contains uses a half-open interval, so an empty span would not contain
// the cursor immediately after typing an opening paren in a call like foo(bar(|)).
// signaturehelp.go:1204
fn ensure_minimum_span_size(start: u32, end: u32) -> u32 {
    if end <= start {
        return start + 1;
    }
    end
}

// signaturehelp.go:1211
struct ArgumentOrParameterListAndIndex {
    list: Option<P<NodeList>>,
    argument_index: usize,
}

// signaturehelp.go:1216
fn get_argument_or_parameter_list_and_index(
    node: P<Node>,
    source_file: P<SourceFile>,
    c: &mut Checker,
) -> Option<ArgumentOrParameterListAndIndex> {
    if node.kind() == Kind::LessThanToken || node.kind() == Kind::OpenParenToken {
        // Find the list that starts right *after* the < or ( token.
        // If the user has just opened a list, consider this item 0.
        let list = get_child_list_that_starts_with_opener_token(node.parent().unwrap(), node);
        Some(ArgumentOrParameterListAndIndex { list, argument_index: 0 })
    } else {
        // findListItemInfo can return undefined if we are not in parent's argument list
        // or type argument list. This includes cases where the cursor is:
        //   - To the right of the closing parenthesis, non-substitution template, or template tail.
        //   - Between the type arguments and the arguments (greater than token)
        //   - On the target of the call (parent.func)
        //   - On the 'new' keyword in a 'new' expression
        let list = find_containing_list(node, source_file)?;
        Some(ArgumentOrParameterListAndIndex {
            list: Some(list),
            // Find the index of the argument that contains the node.
            argument_index: get_argument_index(node, Some(list), source_file, c),
        })
    }
}

// signaturehelp.go:1244
fn get_child_list_that_starts_with_opener_token(
    parent: P<Node>,
    opener_token: P<Node>,
) -> Option<P<NodeList>> {
    if (ast::is_call_expression(parent)) || (ast::is_new_expression(parent)) {
        if opener_token.kind() == Kind::LessThanToken {
            return parent.type_argument_list();
        }
        return parent.argument_list();
    }
    None
}

// signaturehelp.go:1261
fn try_get_parameter_info(
    starting_token: P<Node>,
    source_file: P<SourceFile>,
    c: &mut Checker,
) -> Option<ArgumentListInfo> {
    let node = get_adjusted_node(starting_token)?;
    let info = get_contextual_signature_location_info(node, source_file, c)?;

    // for optional function condition
    // Go checks the non-nullable type for nil; GetNonNullableType never returns nil.
    let non_nullable_contextual_type = c.get_non_nullable_type(info.contextual_type);

    let symbol = non_nullable_contextual_type.symbol()?;

    let signatures = c.get_signatures_of_type_exported(
        non_nullable_contextual_type,
        checker::SignatureKind::Call,
    );
    if signatures.is_empty() {
        return None;
    }
    let signature = signatures[signatures.len() - 1];

    let contextual_invocation = ContextualInvocation {
        signature,
        node: starting_token,
        symbol: choose_better_symbol(symbol),
    };
    Some(ArgumentListInfo {
        is_type_parameter_list: false,
        invocation: Invocation {
            contextual_invocation: Some(contextual_invocation),
            ..Default::default()
        },
        arguments_span: info.arguments_span,
        argument_index: info.argument_index,
        argument_count: info.argument_count,
    })
}

// signaturehelp.go:1302
fn choose_better_symbol(s: P<Symbol>) -> P<Symbol> {
    if s.name() == ast::INTERNAL_SYMBOL_NAME_TYPE
        && let Some(result) = s.declarations().iter().find_map(|&d| {
            if ast::is_function_type_node(d) && ast::can_have_symbol(d.parent().unwrap()) {
                // Go returns `d.Parent.Symbol()` (nil-able) where a symbol is expected.
                return Some(d.parent().unwrap().symbol().unwrap());
            }

            None
        })
    {
        return result;
    }
    s
}

// signaturehelp.go:1313
fn get_contextual_signature_location_info(
    node: P<Node>,
    source_file: P<SourceFile>,
    c: &mut Checker,
) -> Option<ContextualSignatureLocationInfo> {
    let parent = node.parent().unwrap();
    match parent.kind() {
        Kind::ParenthesizedExpression
        | Kind::MethodDeclaration
        | Kind::FunctionExpression
        | Kind::ArrowFunction => {
            let info = get_argument_or_parameter_list_info(node, source_file, c)?;
            let argument_index = info.argument_index;
            let argument_count = info.argument_count;
            let arguments_span = info.arguments_span;

            let contextual_type = if ast::is_method_declaration(parent) {
                c.get_contextual_type_for_object_literal_element_exported(
                    parent,
                    ContextFlags::None,
                )
            } else {
                c.get_contextual_type_exported(parent, ContextFlags::None)
            };
            if let Some(contextual_type) = contextual_type {
                return Some(ContextualSignatureLocationInfo {
                    contextual_type,
                    argument_index,
                    argument_count,
                    arguments_span,
                });
            }
            None
        }
        Kind::BinaryExpression => {
            let highest_binary = get_highest_binary(parent);
            let contextual_type =
                c.get_contextual_type_exported(highest_binary, ContextFlags::None);
            if node.kind() != Kind::OpenParenToken {
                let argument_index = count_binary_expression_parameters(parent) - 1;
                let argument_count = count_binary_expression_parameters(highest_binary);
                if let Some(contextual_type) = contextual_type {
                    return Some(ContextualSignatureLocationInfo {
                        contextual_type,
                        argument_index,
                        argument_count,
                        arguments_span: TextRange::from_text_pos(parent.pos(), parent.end()),
                    });
                }
                return None;
            }
            None
        }
        _ => None,
    }
}

// signaturehelp.go:1361
fn get_highest_binary(b: P<Node>) -> P<Node> {
    let parent = b.parent().unwrap();
    if ast::is_binary_expression(parent) {
        return get_highest_binary(parent);
    }
    b
}

// signaturehelp.go:1368
fn count_binary_expression_parameters(b: P<Node>) -> usize {
    let left = b.as_binary_expression().left;
    if ast::is_binary_expression(left) {
        return count_binary_expression_parameters(left) + 1;
    }
    2
}

// signaturehelp.go:1375
fn get_token_from_node_list(
    node_list: Option<P<NodeList>>,
    node_list_parent: Option<P<Node>>,
    source_file: P<SourceFile>,
) -> Vec<P<Node>> {
    let (Some(node_list), Some(node_list_parent)) = (node_list, node_list_parent) else {
        return Vec::new();
    };
    let mut left =
        node_list.loc.get().pos().expect_source("parsed node lists have source positions");
    let mut node_list_index = 0;
    let mut tokens = Vec::new();
    let nodes = node_list.nodes();
    let node_list_end =
        node_list.loc.get().end().expect_source("parsed node lists have source positions");
    while left < node_list_end {
        if nodes.len() > node_list_index
            && left
                == nodes[node_list_index].pos().expect_source("parsed nodes have source positions")
        {
            tokens.push(nodes[node_list_index]);
            left = nodes[node_list_index].end().expect_source("parsed nodes have source positions");
            node_list_index += 1;
        } else {
            let scanner = scanner::get_scanner_for_source_file(source_file, left);
            let token = scanner.token();
            let token_full_start = scanner.token_full_start();
            let token_end = scanner.token_end();
            tokens.push(source_file.get_or_create_token(
                token,
                token_full_start,
                token_end,
                node_list_parent,
                scanner.token_flags(),
            ));
            left = token_end;
        }
    }
    tokens
}

// signaturehelp.go:1399
fn get_argument_list_info_for_template(
    tag_expression: P<Node>,
    argument_index: usize,
    source_file: P<SourceFile>,
) -> ArgumentListInfo {
    // argumentCount is either 1 or (numSpans + 1) to account for the template strings array argument.
    let template = tag_expression.as_tagged_template_expression().template;
    let mut argument_count = 1;
    if !is_no_substitution_template_literal(template) {
        argument_count = template.as_template_expression().template_spans.nodes().len() + 1;
    }
    if argument_index != 0 {
        assert!(argument_index < argument_count);
    }
    ArgumentListInfo {
        is_type_parameter_list: false,
        invocation: Invocation {
            call_invocation: Some(CallInvocation { node: tag_expression }),
            ..Default::default()
        },
        argument_index,
        argument_count,
        arguments_span: get_applicable_range_for_tagged_template(tag_expression, source_file),
    }
}

// signaturehelp.go:1417
fn get_applicable_range_for_tagged_template(
    tagged_template: P<Node>,
    source_file: P<SourceFile>,
) -> TextRange {
    let template = tagged_template.as_tagged_template_expression().template;
    let applicable_span_start = scanner::get_token_pos_of_node(template, source_file, false);
    let mut applicable_span_end =
        template.end().expect_source("parsed templates have source positions");

    // We need to adjust the end position for the case where the template does not have a tail.
    // Otherwise, we will not show signature help past the expression.
    // For example,
    //
    //      ` ${ 1 + 1 foo(10)
    //       |       |
    // This is because a Missing node has no width. However, what we actually want is to include trivia
    // leading up to the next token in case the user is about to type in a TemplateMiddle or TemplateTail.
    if template.kind() == Kind::TemplateExpression {
        let template_spans = template.as_template_expression().template_spans;
        let last_span = template_spans.nodes()[template_spans.nodes().len() - 1];
        if last_span.as_template_span().literal.end() == last_span.as_template_span().literal.pos()
        {
            applicable_span_end = scanner::skip_trivia(source_file.text(), applicable_span_end);
        }
    }

    TextRange::new(applicable_span_start, applicable_span_end)
}
