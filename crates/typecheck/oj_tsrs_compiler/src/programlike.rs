// compiler.ProgramLike (compiler/program.go:1965-2073): the interface the incremental program also implements, and
// `GetDiagnosticsOfAnyProgram` over it. Taken from the emit/incremental wave's port (tsrs_incremental/src/emit.rs),
// moved here as its TODO(emit/core) asked. `HandleNoEmitOptions` (program_emit.rs) takes a `&dyn ProgramLike`.

use tsrs_ast::{Diagnostic, SourceFile};
use tsrs_core::{CompilerOptions, P};

use crate::checkerpool::Context;
use crate::program::Program as CompilerProgram;
use crate::program_emit::{EmitOptions, EmitResult};

// compiler.ProgramLike (program.go:1965).
pub trait ProgramLike {
    fn options(&self) -> P<CompilerOptions>;
    fn get_source_file(&self, path: &str) -> Option<P<SourceFile>>;
    fn get_source_files(&self) -> &'static [P<SourceFile>];
    fn get_config_file_parsing_diagnostics(&self) -> Vec<P<Diagnostic>>;
    fn get_syntactic_diagnostics(
        &self,
        ctx: &Context,
        file: Option<P<SourceFile>>,
    ) -> Vec<P<Diagnostic>>;
    fn get_bind_diagnostics(
        &self,
        ctx: &Context,
        file: Option<P<SourceFile>>,
    ) -> Vec<P<Diagnostic>>;
    fn get_program_diagnostics(&self) -> Vec<P<Diagnostic>>;
    fn get_global_diagnostics(&self, ctx: &Context) -> Vec<P<Diagnostic>>;
    fn get_semantic_diagnostics(
        &self,
        ctx: &Context,
        file: Option<P<SourceFile>>,
    ) -> Vec<P<Diagnostic>>;
    fn get_declaration_diagnostics(
        &self,
        ctx: &Context,
        file: Option<P<SourceFile>>,
    ) -> Vec<P<Diagnostic>>;
    fn get_suggestion_diagnostics(
        &self,
        ctx: &Context,
        file: Option<P<SourceFile>>,
    ) -> Vec<P<Diagnostic>>;
    fn emit(&self, ctx: &Context, options: EmitOptions) -> Option<EmitResult>;
    fn common_source_directory(&self) -> String;
    fn is_source_file_default_library(&self, path: &tsrs_core::tspath::Path) -> bool;
    fn program(&self) -> &'static CompilerProgram;
    // Go's `program.(*compiler.Program)` type assertion in GetDiagnosticsOfAnyProgram.
    fn is_compiler_program(&self) -> bool;
}

impl ProgramLike for &'static CompilerProgram {
    fn options(&self) -> P<CompilerOptions> {
        CompilerProgram::options(self)
    }
    fn get_source_file(&self, path: &str) -> Option<P<SourceFile>> {
        CompilerProgram::get_source_file(self, path)
    }
    fn get_source_files(&self) -> &'static [P<SourceFile>] {
        CompilerProgram::get_source_files(self)
    }
    fn get_config_file_parsing_diagnostics(&self) -> Vec<P<Diagnostic>> {
        CompilerProgram::get_config_file_parsing_diagnostics(self)
    }
    fn get_syntactic_diagnostics(
        &self,
        ctx: &Context,
        file: Option<P<SourceFile>>,
    ) -> Vec<P<Diagnostic>> {
        CompilerProgram::get_syntactic_diagnostics(self, ctx, file)
    }
    fn get_bind_diagnostics(
        &self,
        ctx: &Context,
        file: Option<P<SourceFile>>,
    ) -> Vec<P<Diagnostic>> {
        CompilerProgram::get_bind_diagnostics(self, ctx, file)
    }
    fn get_program_diagnostics(&self) -> Vec<P<Diagnostic>> {
        CompilerProgram::get_program_diagnostics(self)
    }
    fn get_global_diagnostics(&self, ctx: &Context) -> Vec<P<Diagnostic>> {
        CompilerProgram::get_global_diagnostics(self, ctx)
    }
    fn get_semantic_diagnostics(
        &self,
        ctx: &Context,
        file: Option<P<SourceFile>>,
    ) -> Vec<P<Diagnostic>> {
        CompilerProgram::get_semantic_diagnostics(self, ctx, file)
    }
    fn get_declaration_diagnostics(
        &self,
        ctx: &Context,
        file: Option<P<SourceFile>>,
    ) -> Vec<P<Diagnostic>> {
        CompilerProgram::get_declaration_diagnostics(self, ctx, file)
    }
    fn get_suggestion_diagnostics(
        &self,
        ctx: &Context,
        file: Option<P<SourceFile>>,
    ) -> Vec<P<Diagnostic>> {
        CompilerProgram::get_suggestion_diagnostics(self, ctx, file)
    }
    fn emit(&self, ctx: &Context, options: EmitOptions) -> Option<EmitResult> {
        Some(CompilerProgram::emit(self, ctx, &options))
    }
    fn common_source_directory(&self) -> String {
        CompilerProgram::common_source_directory(self).to_string()
    }
    fn is_source_file_default_library(&self, path: &tsrs_core::tspath::Path) -> bool {
        CompilerProgram::is_source_file_default_library(self, path)
    }
    fn program(&self) -> &'static CompilerProgram {
        self
    }
    fn is_compiler_program(&self) -> bool {
        true
    }
}

// program.go:2018
pub fn get_diagnostics_of_any_program_like(
    ctx: &Context,
    program: &dyn ProgramLike,
    files: Option<&[P<SourceFile>]>,
    skip_no_emit_check_for_dts_diagnostics: bool,
    get_bind_diagnostics: &mut dyn FnMut(&Context, Option<P<SourceFile>>) -> Vec<P<Diagnostic>>,
    get_semantic_diagnostics: &mut dyn FnMut(&Context, Option<P<SourceFile>>) -> Vec<P<Diagnostic>>,
) -> Vec<P<Diagnostic>> {
    // Go's `program.(*Program)` assertion below; for the compiler program, the concrete version (program.rs) runs the
    // same steps (plus tsrs's phase timers and checker cost bookkeeping).
    if program.is_compiler_program() {
        return crate::program::get_diagnostics_of_any_program(
            ctx,
            program.program(),
            files,
            skip_no_emit_check_for_dts_diagnostics,
            get_bind_diagnostics,
            get_semantic_diagnostics,
        );
    }
    let mut all_diagnostics = program.get_config_file_parsing_diagnostics();
    let config_file_parsing_diagnostics_length = all_diagnostics.len();

    let append_diagnostics_for_all_files = |diagnostics: &mut Vec<P<Diagnostic>>,
                                            get_diagnostics: &mut dyn FnMut(
        &Context,
        Option<P<SourceFile>>,
    ) -> Vec<
        P<Diagnostic>,
    >| match files {
        None => diagnostics.extend(get_diagnostics(ctx, None)),
        Some(files) => {
            diagnostics.extend(files.iter().flat_map(|&file| get_diagnostics(ctx, Some(file))));
        }
    };

    let mut syntactic_diagnostics = Vec::new();
    append_diagnostics_for_all_files(&mut syntactic_diagnostics, &mut |ctx, f| {
        program.get_syntactic_diagnostics(ctx, f)
    });
    if !syntactic_diagnostics.is_empty() {
        // Per-file content mapper failures are syntactic diagnostics, but the locationless diagnostic
        // that disables a repeatedly failing mapper must still be reported.
        all_diagnostics.extend_from_slice(&program.program().content_mapper_diagnostics);
    }
    all_diagnostics.extend(syntactic_diagnostics);

    // If we didn't have any syntactic errors, then also try getting the program (options),
    // global and semantic errors.
    if all_diagnostics.len() == config_file_parsing_diagnostics_length {
        all_diagnostics.extend(program.get_program_diagnostics());

        // Do binding early so we can track the time.
        append_diagnostics_for_all_files(&mut Vec::new(), get_bind_diagnostics);

        if program.options().list_files_only.is_false_or_unknown() {
            all_diagnostics.extend(program.get_global_diagnostics(ctx));

            if all_diagnostics.len() == config_file_parsing_diagnostics_length {
                append_diagnostics_for_all_files(&mut all_diagnostics, get_semantic_diagnostics);
                if program.is_compiler_program() {
                    // Incremental programs cache checking globals with file diagnostics;
                    // a late sweep would also collect incidental signature-generation globals.
                    all_diagnostics.extend(program.get_global_diagnostics(ctx));
                }
            }

            if (skip_no_emit_check_for_dts_diagnostics || program.options().no_emit.is_true())
                && program.options().get_emit_declarations()
                && all_diagnostics.len() == config_file_parsing_diagnostics_length
            {
                append_diagnostics_for_all_files(&mut all_diagnostics, &mut |ctx, f| {
                    program.get_declaration_diagnostics(ctx, f)
                });
            }
        }
    }
    all_diagnostics
}
