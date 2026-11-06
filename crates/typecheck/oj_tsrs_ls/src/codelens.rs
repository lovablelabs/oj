// Port of ls/codelens.go.

use rustc_hash::FxHashSet;
use tsrs_ast::{self as ast, Kind, ModifierFlags, Node, SourceFile, Symbol};
use tsrs_core::context::Context;
use tsrs_core::{P, TextRange};
use tsrs_diagnostics as diagnostics;
use tsrs_lsproto as lsproto;
use tsrs_lsproto::Json as _;
use tsrs_scanner as scanner;

use crate::completions::{source_file_for_supplemental_file_index, supplemental_file_index};
use crate::crossproject::CrossProjectOrchestrator;
use crate::findallreferences::SymbolEntryTransformOptions;
use crate::languageservice::LanguageService;
use crate::lsutil::CodeLensUserPreferences;
use crate::spanmap::Feature;

// The `visit` closure of ProvideCodeLenses and the state it captures.
struct CodeLensVisitor<'a> {
    l: &'a LanguageService,
    ctx: &'a Context,
    document_uri: &'a lsproto::DocumentUri,
    projection: P<SourceFile>,
    user_prefs: &'a CodeLensUserPreferences,
    result: &'a mut Vec<lsproto::CodeLens>,
    seen: &'a mut FxHashSet<CodeLensKey>,
    // Keeps track of the last symbol to avoid duplicating code lenses across overloads.
    last_symbol: Option<P<Symbol>>,
}

impl CodeLensVisitor<'_> {
    fn visit(&mut self, node: P<Node>) -> bool {
        if self.ctx.err().is_some() {
            return true;
        }

        let current_symbol = node.symbol();
        if self.last_symbol != current_symbol {
            self.last_symbol = current_symbol;

            if self.user_prefs.references_code_lens_enabled.is_true()
                && is_valid_reference_lens_node(node, self.user_prefs)
                && let Some(code_lens) = self.l.new_code_lens_for_node(
                    self.document_uri,
                    self.projection,
                    node,
                    lsproto::CodeLensKind::REFERENCES,
                )
                && self.seen.insert(key_for_code_lens(&code_lens))
            {
                self.result.push(code_lens);
            }

            if self.user_prefs.implementations_code_lens_enabled.is_true()
                && is_valid_implementations_code_lens_node(node, self.user_prefs)
                && let Some(code_lens) = self.l.new_code_lens_for_node(
                    self.document_uri,
                    self.projection,
                    node,
                    lsproto::CodeLensKind::IMPLEMENTATIONS,
                )
                && self.seen.insert(key_for_code_lens(&code_lens))
            {
                self.result.push(code_lens);
            }
        }

        let saved_last_symbol = self.last_symbol;
        node.for_each_child(&mut |child| self.visit(child));
        self.last_symbol = saved_last_symbol;
        false
    }
}

impl LanguageService {
    // codelens.go:18
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Language service operations share the fallible response signature used by LSP and cross-project dispatch."
    )]
    pub fn provide_code_lenses(
        &self,
        ctx: &Context,
        document_uri: &lsproto::DocumentUri,
    ) -> Result<lsproto::CodeLensResponse, lsproto::Error> {
        let (_, file) = self.get_program_and_file(document_uri);

        let user_prefs = &self.user_preferences().code_lens;
        if !user_prefs.references_code_lens_enabled.is_true()
            && !user_prefs.implementations_code_lens_enabled.is_true()
        {
            return Ok(lsproto::CodeLensResponse::default());
        }

        let mut result: Vec<lsproto::CodeLens> = Vec::new();
        let mut seen: FxHashSet<CodeLensKey> = FxHashSet::default();
        let mut projections = vec![file];
        projections.extend_from_slice(file.supplemental_source_files());
        for projection in projections {
            let mut v = CodeLensVisitor {
                l: self,
                ctx,
                document_uri,
                projection,
                user_prefs,
                result: &mut result,
                seen: &mut seen,
                last_symbol: None,
            };
            v.visit(projection.as_node());
        }

        Ok(lsproto::CodeLensesOrNull { code_lenses: Some(result) })
    }
}

// codelens.go:66
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct CodeLensKey {
    kind: lsproto::CodeLensKind,
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
}

// codelens.go:71
fn key_for_code_lens(code_lens: &lsproto::CodeLens) -> CodeLensKey {
    CodeLensKey {
        kind: code_lens.data.as_ref().unwrap().kind,
        start_line: code_lens.range.start.line,
        start_character: code_lens.range.start.character,
        end_line: code_lens.range.end.line,
        end_character: code_lens.range.end.character,
    }
}

impl LanguageService {
    // codelens.go:81
    pub fn resolve_code_lens(
        &self,
        ctx: &Context,
        code_lens: lsproto::CodeLens,
        show_locations_command_name: Option<&String>,
        orchestrator: Option<&dyn CrossProjectOrchestrator>,
    ) -> Result<lsproto::CodeLens, lsproto::Error> {
        let mut code_lens = code_lens;
        let data = code_lens.data.clone().unwrap();
        let uri = data.uri.clone();
        let text_doc = lsproto::TextDocumentIdentifier { uri: uri.clone() };
        let (program, file) = self.get_program_and_file(&uri);
        let Some(file) =
            source_file_for_supplemental_file_index(file, data.supplemental_file_index)
        else {
            return Err(lsproto::Error::new(format!(
                "supplemental source file index not found: {}",
                data.supplemental_file_index.unwrap()
            )));
        };
        let position = u32::try_from(data.position)
            .map_err(|_| lsproto::Error::new("code lens position must be non-negative"))?;
        // (Go reads `locale.FromContext(ctx)`; only English is ported.)
        let mut locs: Vec<lsproto::Location> = Vec::new();
        let mut lens_title = String::new();
        match data.kind {
            lsproto::CodeLensKind::REFERENCES => {
                let symbols_data = self
                    .provide_symbols_and_entries_at_position(
                        ctx, program, file, position, false, false,
                    )
                    .unwrap_or_default();
                let references_resp = self.provide_references_from_data(
                    ctx,
                    &lsproto::ReferenceParams {
                        text_document: text_doc,
                        position: code_lens.range.start,
                        context: lsproto::ReferenceContext {
                            // Don't include the declaration in the references count.
                            include_declaration: false,
                        },
                        ..Default::default()
                    },
                    orchestrator,
                    &symbols_data,
                )?;
                if let Some(locations) = references_resp.locations {
                    locs = locations;
                }

                if locs.len() == 1 {
                    lens_title = diagnostics::X_1_REFERENCE.localize(&[]);
                } else {
                    lens_title = diagnostics::X_0_REFERENCES.localize(&[&locs.len()]);
                }
            }
            lsproto::CodeLensKind::IMPLEMENTATIONS => {
                let symbols_data = self
                    .provide_symbols_and_entries_at_position(
                        ctx, program, file, position, false, true,
                    )
                    .unwrap_or_default();
                let implementations = self.provide_implementations_from_data(
                    ctx,
                    &lsproto::ImplementationParams {
                        text_document: text_doc,
                        position: code_lens.range.start,
                        ..Default::default()
                    },
                    // "Force" link support to be false so that we only get `Locations` back,
                    // and don't include the "current" node in the results.
                    SymbolEntryTransformOptions {
                        require_locations_result: true,
                        drop_origin_nodes: true,
                    },
                    orchestrator,
                    &symbols_data,
                )?;

                if let Some(locations) = implementations.locations {
                    locs = locations;
                }

                if locs.len() == 1 {
                    lens_title = diagnostics::X_1_IMPLEMENTATION.localize(&[]);
                } else {
                    lens_title = diagnostics::X_0_IMPLEMENTATIONS.localize(&[&locs.len()]);
                }
            }
            _ => {}
        }

        let mut cmd = lsproto::Command { title: lens_title, ..Default::default() };
        if let Some(show_locations_command_name) =
            show_locations_command_name.filter(|_| !locs.is_empty())
        {
            cmd.command.clone_from(show_locations_command_name);
            cmd.arguments =
                Some(vec![uri.to_json(), code_lens.range.start.to_json(), locs.to_json()]);
        }

        code_lens.command = Some(cmd);
        Ok(code_lens)
    }

    // codelens.go:175
    fn new_code_lens_for_node(
        &self,
        file_uri: &lsproto::DocumentUri,
        file: P<SourceFile>,
        node: P<Node>,
        kind: lsproto::CodeLensKind,
    ) -> Option<lsproto::CodeLens> {
        let node_for_range = node.name().unwrap_or(node);
        let pos = scanner::skip_trivia(
            file.text(),
            node_for_range.pos().expect_source("parsed nodes have source positions"),
        );
        let (lsp_range, fidelity) = self.converters.to_lsp_range_for_feature(
            &file,
            TextRange::new(pos, node.end().expect_source("parsed nodes have source positions")),
            Feature::CodeLens,
        );
        if fidelity.is_none() {
            return None;
        }

        Some(lsproto::CodeLens {
            range: lsp_range,
            data: Some(lsproto::CodeLensData {
                kind,
                uri: file_uri.clone(),
                position: i32::try_from(pos).expect("code lens position exceeds protocol i32"),
                supplemental_file_index: supplemental_file_index(file).map(|index| {
                    i32::try_from(index).expect("supplemental file index exceeds protocol i32")
                }),
            }),
            ..Default::default()
        })
    }
}

// codelens.go:197
fn is_valid_implementations_code_lens_node(
    node: P<Node>,
    user_prefs: &CodeLensUserPreferences,
) -> bool {
    match node.kind() {
        // Always show on interfaces
        Kind::InterfaceDeclaration => {
            // TODO: ast.KindTypeAliasDeclaration?
            true
        }

        // If configured, show on interface methods
        Kind::MethodSignature => {
            user_prefs.implementations_code_lens_show_on_interface_methods.is_true()
                && node.parent().unwrap().kind() == Kind::InterfaceDeclaration
        }

        // If configured, show on all class methods - but not private ones.
        Kind::MethodDeclaration => {
            if user_prefs.implementations_code_lens_show_on_all_class_methods.is_true()
                && node.parent().unwrap().kind() == Kind::ClassDeclaration
            {
                return !ast::has_modifier(node, ModifierFlags::Private)
                    && node.name().unwrap().kind() != Kind::PrivateIdentifier;
            }
            // fallthrough
            ast::has_modifier(node, ModifierFlags::Abstract)
        }

        // Always show on abstract classes/properties/methods
        Kind::ClassDeclaration
        | Kind::Constructor
        | Kind::GetAccessor
        | Kind::SetAccessor
        | Kind::PropertyDeclaration => ast::has_modifier(node, ModifierFlags::Abstract),

        _ => false,
    }
}

// codelens.go:224
fn is_valid_reference_lens_node(node: P<Node>, user_prefs: &CodeLensUserPreferences) -> bool {
    match node.kind() {
        Kind::FunctionDeclaration | Kind::VariableDeclaration => {
            if node.kind() == Kind::FunctionDeclaration
                && user_prefs.references_code_lens_show_on_all_functions.is_true()
            {
                return true;
            }
            // fallthrough
            ast::get_combined_modifier_flags(node).intersects(ModifierFlags::Export)
        }

        Kind::ClassDeclaration
        | Kind::InterfaceDeclaration
        | Kind::TypeAliasDeclaration
        | Kind::EnumDeclaration
        | Kind::EnumMember => true,

        Kind::MethodDeclaration
        | Kind::MethodSignature
        | Kind::Constructor
        | Kind::GetAccessor
        | Kind::SetAccessor
        | Kind::PropertyDeclaration
        | Kind::PropertySignature => {
            // Don't show if child and parent have same start
            // For https://github.com/microsoft/vscode/issues/90396
            // !!!

            matches!(
                node.parent().unwrap().kind(),
                Kind::ClassDeclaration | Kind::InterfaceDeclaration | Kind::TypeLiteral
            )
        }

        _ => false,
    }
}
