// The emit surface of Go's compiler package that the incremental program uses: `ProgramLike`, `HandleNoEmitOptions`
// and `GetDiagnosticsOfAnyProgram` over it live in tsrs_compiler (programlike.rs, program_emit.rs; emit/core-2).

use tsrs_compiler::{Context, Program as CompilerProgram};

pub use tsrs_compiler::{
    EmitOnly, EmitOptions, EmitResult, ProgramLike, WriteFile, WriteFileData, combine_emit_results,
    get_diagnostics_of_any_program_like as get_diagnostics_of_any_program, handle_no_emit_options,
};

// Go `(*compiler.Program).Emit` (program.go:1875).
#[expect(
    clippy::unnecessary_wraps,
    reason = "The incremental emit interface shares an optional result with paths that have no pending emit."
)]
pub(crate) fn compiler_program_emit(
    program: &'static CompilerProgram,
    ctx: &Context,
    options: &EmitOptions,
) -> Option<EmitResult> {
    Some(program.emit(ctx, options))
}
