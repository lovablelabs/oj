// session.go program-level handlers: diagnostics (getDiagnostics + the eight diagnostic methods),
// getSourceFileNames, and emit / emitToString / getJavaScriptEmit / getDeclarationEmit.
//
// Emit follows the program's compiler options (noEmit, outDir, declaration, ...) exactly like tsgo; there is
// no TSRS_EMIT opt-in on this path. Write-through emit writes with the session's host filesystem.

use std::sync::Mutex;

use tsrs_ast::{Diagnostic, SourceFile};
use tsrs_compiler::{EmitOnly, EmitOptions, EmitResult, Program, WriteFileData};
use tsrs_core::P;
use tsrs_core::context::{CheckerLifetime, Context, with_checker_lifetime};
use tsrs_core::json::Value;
use tsrs_project::ID as ProjectID;

use crate::diagnostics::diagnostic_responses;
use crate::handler::{ApiError, ApiResult};
use crate::session::Session;
use crate::wire::{DocumentIdentifier, Obj, Params, b, s, strings};

/// Go `NewDiagnosticResponses`: a nil slice, which encoding/json/v2 writes as `[]` (the TS type is
/// nullable, but the pinned server never sends `null` here).
fn nullable_diagnostics(diags: &[P<Diagnostic>]) -> Value {
    diagnostic_responses(diags)
}

/// Go `resolveOptionalSourceFile` for a present identifier.
pub fn resolve_source_file(
    program: &Program,
    file: &DocumentIdentifier,
) -> ApiResult<P<SourceFile>> {
    program
        .get_source_file(&file.to_file_name())
        .ok_or_else(|| ApiError::client(format!("source file not found: {file}")))
}

#[derive(Clone, Copy)]
pub enum DiagnosticKind {
    Syntactic,
    Bind,
    Semantic,
    Suggestion,
    Declaration,
}

impl Session {
    fn program_of(
        &self,
        p: Params,
    ) -> ApiResult<(std::sync::Arc<crate::session::SnapshotData>, &'static Program)> {
        let sd = self.snapshot_data(p.u64("snapshot")?)?;
        let program = sd.get_program(&ProjectID(p.str("project")?.to_string()))?;
        Ok((sd, program))
    }

    /// Go `getDiagnostics` with the per-kind getter.
    pub(crate) fn handle_get_diagnostics(
        &self,
        p: Params,
        kind: DiagnosticKind,
    ) -> ApiResult<Value> {
        let (_sd, program) = self.program_of(p)?;
        let ctx = with_checker_lifetime(&Context::background(), CheckerLifetime::Diagnostics);
        let get = |file: Option<P<SourceFile>>| -> Vec<P<Diagnostic>> {
            match kind {
                DiagnosticKind::Syntactic => program.get_syntactic_diagnostics(&ctx, file),
                DiagnosticKind::Bind => program.get_bind_diagnostics(&ctx, file),
                DiagnosticKind::Semantic => program.get_semantic_diagnostics(&ctx, file),
                DiagnosticKind::Suggestion => program.get_suggestion_diagnostics(&ctx, file),
                DiagnosticKind::Declaration => program.get_declaration_diagnostics(&ctx, file),
            }
        };
        if p.has("files") {
            let mut all = Vec::new();
            all = (DocumentIdentifier::parse_list(p.array("files")?, "files")?)
                .into_iter()
                .try_fold(all, |mut all, file| {
                    let source_file = resolve_source_file(program, &file)?;
                    all.extend(get(Some(source_file)));
                    Ok::<_, ApiError>(all)
                })?;
            return Ok(nullable_diagnostics(&all));
        }
        Ok(nullable_diagnostics(&get(None)))
    }

    pub(crate) fn handle_get_config_file_parsing_diagnostics(&self, p: Params) -> ApiResult<Value> {
        let (_sd, program) = self.program_of(p)?;
        Ok(nullable_diagnostics(&program.get_config_file_parsing_diagnostics()))
    }

    pub(crate) fn handle_get_program_diagnostics(&self, p: Params) -> ApiResult<Value> {
        let (_sd, program) = self.program_of(p)?;
        Ok(nullable_diagnostics(&program.get_program_diagnostics()))
    }

    pub(crate) fn handle_get_global_diagnostics(&self, p: Params) -> ApiResult<Value> {
        let sd = self.snapshot_data(p.u64("snapshot")?)?;
        let id = ProjectID(p.str("project")?.to_string());
        let proj = sd
            .snapshot
            .project_collection
            .get_project(&id)
            .ok_or_else(|| ApiError::client(format!("project {} not found", id.0)))?;
        let program =
            proj.get_program().ok_or_else(|| ApiError::client("project has no program"))?;
        let ctx = with_checker_lifetime(&Context::background(), CheckerLifetime::Diagnostics);
        // Force a full semantic pass so the external checker pool accumulates global diagnostics.
        program.get_semantic_diagnostics(&ctx, None);
        let diags: Vec<P<Diagnostic>> =
            proj.get_project_diagnostics(&ctx).into_iter().filter(|d| d.file().is_none()).collect();
        Ok(nullable_diagnostics(&diags))
    }

    pub(crate) fn handle_get_source_file_names(&self, p: Params) -> ApiResult<Value> {
        let (_sd, program) = self.program_of(p)?;
        Ok(strings(program.get_source_files().iter().map(|f| f.file_name().to_string())))
    }

    fn emit_only(p: Params) -> ApiResult<EmitOnly> {
        match p.opt_u64("emitOnly")? {
            None | Some(0) => Ok(EmitOnly::EmitAll),
            Some(1) => Ok(EmitOnly::EmitOnlyJs),
            Some(2) => Ok(EmitOnly::EmitOnlyDts),
            Some(v) => Err(ApiError::client(format!("invalid emitOnly value: {v}"))),
        }
    }

    /// Go `handleEmit`: outputs are captured in memory for snapshots with a full request filesystem and
    /// written through the session host filesystem otherwise.
    pub(crate) fn handle_emit(&self, p: Params) -> ApiResult<Value> {
        let (sd, program) = self.program_of(p)?;
        let emit_only = Self::emit_only(p)?;
        let capture = sd.file_system.as_ref().is_some_and(|fs| fs.is_full());
        let outputs: Mutex<rustc_hash::FxHashMap<String, String>> =
            Mutex::new(rustc_hash::FxHashMap::default());
        let fs = self.base_fs();
        let write =
            |file_name: &str, text: &str, _data: &mut WriteFileData| -> Result<(), String> {
                if capture {
                    outputs.lock().unwrap().insert(file_name.to_string(), text.to_string());
                    Ok(())
                } else {
                    fs.write_file(file_name, text)
                }
            };
        let ctx = Context::background();
        let result = program.emit(
            &ctx,
            &EmitOptions {
                target_source_files: None,
                emit_only,
                force_emit: false,
                write_file: Some(&write),
            },
        );
        let outputs = outputs.into_inner().unwrap();
        let contents = if capture {
            result
                .emitted_files
                .iter()
                .map(|f| s(outputs.get(f).cloned().unwrap_or_default()))
                .collect()
        } else {
            Vec::new()
        };
        Ok(Obj::new()
            .set("emitSkipped", b(result.emit_skipped))
            .set("diagnostics", diagnostic_responses(&result.diagnostics))
            .set("emittedFiles", strings(result.emitted_files.iter().cloned()))
            .set("emittedFilesContents", Value::Array(contents))
            .build())
    }

    pub(crate) fn handle_emit_to_string(&self, p: Params) -> ApiResult<Value> {
        let (_sd, program) = self.program_of(p)?;
        let emit_only = Self::emit_only(p)?;
        Ok(emit_to_output(program, None, emit_only, false))
    }

    pub(crate) fn handle_selected_files_emit(
        &self,
        p: Params,
        emit_only: EmitOnly,
    ) -> ApiResult<Value> {
        let (_sd, program) = self.program_of(p)?;
        if !p.has("files") {
            return Err(ApiError::client("files is required"));
        }
        let mut targets = Vec::new();
        targets = (DocumentIdentifier::parse_list(p.array("files")?, "files")?)
            .into_iter()
            .try_fold(targets, |mut targets, file| {
                targets.push(resolve_source_file(program, &file)?);
                Ok::<_, ApiError>(targets)
            })?;
        Ok(emit_to_output(program, Some(targets), emit_only, true))
    }
}

/// Go `emitToOutput`: captures outputs in memory, sorted by file name.
fn emit_to_output(
    program: &'static Program,
    targets: Option<Vec<P<SourceFile>>>,
    emit_only: EmitOnly,
    force_emit: bool,
) -> Value {
    let outputs: Mutex<Vec<(String, String, Option<String>)>> = Mutex::new(Vec::new());
    let write = |file_name: &str, text: &str, data: &mut WriteFileData| -> Result<(), String> {
        let source = data.source_file.map(|f| f.file_name().to_string());
        outputs.lock().unwrap().push((file_name.to_string(), text.to_string(), source));
        Ok(())
    };
    let ctx = Context::background();
    let result: EmitResult = program.emit(
        &ctx,
        &EmitOptions {
            target_source_files: targets,
            emit_only,
            force_emit,
            write_file: Some(&write),
        },
    );
    let mut outputs = outputs.into_inner().unwrap();
    outputs.sort_by(|a, b| a.0.cmp(&b.0));
    let files = outputs
        .into_iter()
        .map(|(name, text, source)| {
            Obj::new()
                .set("fileName", s(name))
                .set("text", s(text))
                .set_opt("sourceFileName", source.map(s))
                .build()
        })
        .collect();
    Obj::new()
        .set("emitSkipped", b(result.emit_skipped))
        .set("diagnostics", diagnostic_responses(&result.diagnostics))
        .set("outputFiles", Value::Array(files))
        .build()
}

impl Session {
    /// Go `handleGetConfigFileNames` (`[]` when the program has no tsconfig).
    pub(crate) fn handle_get_config_file_names(&self, p: Params) -> ApiResult<Value> {
        let (_sd, program) = self.program_of(p)?;
        let command_line = program.command_line();
        // Go returns a nil []string, written as `[]` by encoding/json/v2.
        let Some(config) = command_line.config_file else {
            return Ok(Value::Array(Vec::new()));
        };
        let mut names = vec![config.source_file.file_name().to_string()];
        names.extend(command_line.extended_source_files());
        Ok(strings(names))
    }

    /// Go `handleGetSourceFileMetadata` (`null` when the file is not in the program).
    pub(crate) fn handle_get_source_file_metadata(&self, p: Params) -> ApiResult<Value> {
        let (_sd, program) = self.program_of(p)?;
        let file = p.document("file")?;
        let Some(source_file) = program.get_source_file(&file.to_file_name()) else {
            return Ok(Value::Null);
        };
        let path = source_file.path();
        let meta = program.get_source_file_meta_data(path).clone();
        Ok(Obj::new()
            .set("isDefaultLibrary", b(program.is_source_file_default_library(path)))
            .set(
                "isFromExternalLibrary",
                b(program.is_source_file_from_external_library(source_file)),
            )
            .set("packageJsonType", s(meta.package_json_type))
            .set("packageJsonDirectory", s(meta.package_json_directory))
            .set("impliedNodeFormat", Value::Number(meta.implied_node_format.value() as f64))
            .build())
    }
}
