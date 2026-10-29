use tsrs_ast::{self as ast, Diagnostic, DiagnosticExt as _, SourceFile};
use tsrs_compiler::Program;
use tsrs_core::collections::OrderedMap;
use tsrs_core::context::Context;
use tsrs_core::{P, TextRange};
use tsrs_diagnostics::{self as diagnostics, Category};
use tsrs_lsproto as lsproto;

use crate::languageservice::LanguageService;
use crate::lsconv::{self, Script};
use crate::spanmap::Fidelity;

// getAllDiagnostics collects all diagnostics for a file: syntactic, semantic,
// suggestion, and (when declarations are emitted) declaration diagnostics.
// diagnostics.go:18
pub fn get_all_diagnostics(
    ctx: &Context,
    program: &'static Program,
    file: P<SourceFile>,
) -> Vec<P<Diagnostic>> {
    let mut diags: Vec<P<Diagnostic>> = Vec::new();
    let mut files = vec![file];
    files.extend_from_slice(file.supplemental_source_files());
    for source_file in files {
        diags.extend(program.get_syntactic_diagnostics(ctx, Some(source_file)));
        diags.extend(program.get_semantic_diagnostics(ctx, Some(source_file)));
        diags.extend(program.get_suggestion_diagnostics(ctx, Some(source_file)));
        if program.options().get_emit_declarations() {
            diags.extend(program.get_declaration_diagnostics(ctx, Some(source_file)));
        }
    }
    diags
}

impl LanguageService {
    // diagnostics.go:32
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Language service operations share the fallible response signature used by LSP and cross-project dispatch."
    )]
    pub fn provide_diagnostics(
        &self,
        ctx: &Context,
        uri: &lsproto::DocumentUri,
    ) -> Result<lsproto::DocumentDiagnosticResponse, lsproto::Error> {
        let (program, file) = self.get_program_and_file(uri);

        if self.user_preferences().enable_validation.is_false() {
            let diagnostics: Vec<lsproto::Diagnostic> = Vec::new();
            return Ok(
                lsproto::RelatedFullDocumentDiagnosticReportOrUnchangedDocumentDiagnosticReport {
                    full_document_diagnostic_report: Some(
                        lsproto::RelatedFullDocumentDiagnosticReport {
                            items: diagnostics,
                            ..Default::default()
                        },
                    ),
                    ..Default::default()
                },
            );
        }

        let diagnostics = get_all_diagnostics(ctx, program, file);

        Ok(lsproto::RelatedFullDocumentDiagnosticReportOrUnchangedDocumentDiagnosticReport {
            full_document_diagnostic_report: Some(lsproto::RelatedFullDocumentDiagnosticReport {
                items: self.to_lsp_diagnostics(ctx, &[&diagnostics]),
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    // diagnostics.go:53
    fn to_lsp_diagnostics(
        &self,
        ctx: &Context,
        diagnostics: &[&[P<Diagnostic>]],
    ) -> Vec<lsproto::Diagnostic> {
        let report_style_checks_as_warnings =
            self.user_preferences().report_style_checks_as_warnings.is_true();
        let mut size = 0;
        size = diagnostics.iter().fold(size, |mut size, diag_slice| {
            size += diag_slice.len();

            size
        });
        let mut lsp_diagnostics: Vec<lsproto::Diagnostic> = Vec::with_capacity(size);
        // Compiler diagnostics located entirely in a content-mapped file's synthesized code have no location
        // in the original file. Collect them per file and surface them through a single aggregate at the top
        // of the file (with the real messages as related information) rather than dropping them or scattering
        // them at position 0.
        let mut synthesized_by_file: OrderedMap<P<SourceFile>, Vec<P<Diagnostic>>> =
            OrderedMap::default();
        for diag_slice in diagnostics {
            for &diag in *diag_slice {
                if is_synthesized_content_mapped_diagnostic(diag) {
                    synthesized_by_file.entry(diag.file().unwrap()).or_default().push(diag);
                    continue;
                }
                lsp_diagnostics.push(lsconv::diagnostic_to_lsp_pull(
                    ctx,
                    &self.converters,
                    diag,
                    report_style_checks_as_warnings,
                ));
            }
        }
        lsp_diagnostics.extend(synthesized_by_file.into_iter().map(|(file, diags)| {
            let aggregate = aggregate_synthesized_diagnostics(file, &diags);
            lsconv::diagnostic_to_lsp_pull(
                ctx,
                &self.converters,
                aggregate,
                report_style_checks_as_warnings,
            )
        }));
        lsp_diagnostics
    }
}

// isSynthesizedContentMappedDiagnostic reports whether diag is a compiler diagnostic on a content-mapped
// file whose location lies entirely in synthesized virtual code with no counterpart in the original
// file, and so has no meaningful position to report against the original file.
// diagnostics.go:84
fn is_synthesized_content_mapped_diagnostic(diag: P<Diagnostic>) -> bool {
    let Some(file) = diag.file() else {
        return false;
    };
    let Some(span_map) = Script::span_map(file.get()) else {
        return false;
    };
    if !diag.source().is_empty() {
        return false;
    }
    let Some(loc) = diag.loc() else { return false };
    let (_, fidelity) = span_map.virtual_to_original_span(loc);
    fidelity == Fidelity::None
}

// aggregateSynthesizedDiagnostics builds a single diagnostic at the top of a content-mapped file standing
// in for compiler diagnostics located in synthesized code with no original location. The originals are
// attached as related information so their messages are surfaced rather than silently dropped. (A later
// change will point the related locations at a read-only view of the file's virtual TypeScript.)
// diagnostics.go:97
fn aggregate_synthesized_diagnostics(
    file: P<SourceFile>,
    diags: &[P<Diagnostic>],
) -> P<Diagnostic> {
    let aggregate = ast::new_diagnostic(
        Some(file),
        TextRange::new(0, 0),
        &diagnostics::VIRTUAL_CODE_PRODUCED_BY_THE_CONTENT_MAPPER_0_HAS_PROBLEMS_WITH_NO_CORRESPONDING_LOCATION_IN_THIS_FILE,
        &[&file.content_mapper()],
    );
    let aggregate = aggregate.set_related_info(diags);
    aggregate.set_category(worst_category(diags));
    aggregate
}

// diagnostics.go:109
fn worst_category(diags: &[P<Diagnostic>]) -> Category {
    let mut worst = diags[0].category();
    for diag in diags {
        match diag.category() {
            Category::Error => return Category::Error,
            Category::Warning => worst = Category::Warning,
            _ => {}
        }
    }
    worst
}
