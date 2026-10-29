// session.go handleGetModeForUsageLocation / handleGetModeForResolutionAtIndex / handleGetResolvedModule /
// handleGetResolvedModuleFromModuleSpecifier / handleGetResolvedTypeReferenceDirective(FromReference).

use tsrs_core::json::Value;
use tsrs_core::{ModuleKind, ResolutionMode};
use tsrs_module::ResolvedTypeReferenceDirective;
use tsrs_project::ID as ProjectID;

use crate::handler::{ApiError, ApiResult};
use crate::module_resolution::{package_id_response, resolved_module_response};
use crate::program::resolve_source_file;
use crate::session::Session;
use crate::wire::{Obj, Params, b, s};

fn mode_param(p: Params, key: &str) -> ApiResult<ResolutionMode> {
    Ok(match p.get(key) {
        Value::Null => ModuleKind::NONE,
        Value::Number(n) => match *n as i64 {
            0 => ModuleKind::NONE,
            1 => ModuleKind::COMMON_JS,
            99 => ModuleKind::ES_NEXT,
            // Go accepts any core.ModuleKind value here; other kinds simply find no resolution.
            x => tsrs_tsoptions::gojson::GoJson::from_go_json(&Value::Number(x as f64), key)
                .map_err(ApiError::invalid_request)?,
        },
        _ => return Err(ApiError::invalid_request(format!("{key} must be a number"))),
    })
}

fn mode_value(m: ResolutionMode) -> Value {
    Value::Number(m.value() as f64)
}

/// Go `newResolvedTypeReferenceDirectiveResponse`.
fn type_ref_response(r: Option<tsrs_core::P<ResolvedTypeReferenceDirective>>) -> Value {
    let Some(r) = r.filter(|r| r.is_resolved()) else {
        return Value::Null;
    };
    Obj::new()
        .set("primary", b(r.primary))
        .set("resolvedFileName", s(r.resolved_file_name))
        .set_omitempty("originalPath", s(r.original_path))
        .set_opt("packageId", package_id_response(&r.package_id))
        .set("isExternalLibraryImport", b(r.is_external_library_import))
        .build()
}

impl Session {
    fn program_for(
        &self,
        p: Params,
    ) -> ApiResult<(std::sync::Arc<crate::session::SnapshotData>, &'static tsrs_compiler::Program)>
    {
        let sd = self.snapshot_data(p.u64("snapshot")?)?;
        let program = sd.get_program(&ProjectID(p.str("project")?.to_string()))?;
        Ok((sd, program))
    }

    pub(crate) fn handle_get_mode_for_usage_location(&self, p: Params) -> ApiResult<Value> {
        let (_sd, program) = self.program_for(p)?;
        let file = resolve_source_file(program, &p.document("file")?)?;
        let usage = self.resolve_node_handle(program, p.str("usage")?)?;
        if !tsrs_ast::is_string_literal_like(usage) {
            return Err(ApiError::client("usage must be a StringLiteralLike node"));
        }
        Ok(mode_value(program.get_mode_for_usage_location(file, usage)))
    }

    pub(crate) fn handle_get_mode_for_resolution_at_index(&self, p: Params) -> ApiResult<Value> {
        let (_sd, program) = self.program_for(p)?;
        let file = resolve_source_file(program, &p.document("file")?)?;
        let index = match p.get("index") {
            Value::Number(n) if n.fract() == 0.0 => *n as i64,
            Value::Null => 0,
            _ => return Err(ApiError::invalid_request("index must be an integer")),
        };
        let count = file.imports().len()
            + file
                .module_augmentations()
                .iter()
                .filter(|a| a.kind() == tsrs_ast::Kind::StringLiteral)
                .count();
        if index < 0 || index as usize >= count {
            return Err(ApiError::client("invalid resolution index"));
        }
        Ok(mode_value(program.get_mode_for_resolution_at_index(file, index as usize)))
    }

    pub(crate) fn handle_get_resolved_module(&self, p: Params) -> ApiResult<Value> {
        let (_sd, program) = self.program_for(p)?;
        let file = resolve_source_file(program, &p.document("file")?)?;
        let mode = mode_param(p, "mode")?;
        Ok(program
            .get_resolved_module(file, p.str("moduleName")?, mode)
            .map_or(Value::Null, |r| resolved_module_response(&r)))
    }

    pub(crate) fn handle_get_resolved_module_from_module_specifier(
        &self,
        p: Params,
    ) -> ApiResult<Value> {
        let (_sd, program) = self.program_for(p)?;
        let node = self.resolve_node_handle(program, p.str("moduleSpecifier")?)?;
        if !tsrs_ast::is_string_literal_like(node) {
            return Err(ApiError::client("moduleSpecifier must be a StringLiteralLike node"));
        }
        let mut file = tsrs_ast::get_source_file_of_node(node);
        if p.has("sourceFile") {
            file = Some(resolve_source_file(program, &p.document("sourceFile")?)?);
        }
        let file = file.ok_or_else(|| {
            ApiError::client(
                "moduleSpecifier must have a SourceFile ancestor or sourceFile must be provided",
            )
        })?;
        Ok(program
            .get_resolved_module_from_module_specifier(file, node)
            .map_or(Value::Null, |r| resolved_module_response(&r)))
    }

    pub(crate) fn handle_get_resolved_type_reference_directive(
        &self,
        p: Params,
    ) -> ApiResult<Value> {
        let (_sd, program) = self.program_for(p)?;
        let file = resolve_source_file(program, &p.document("file")?)?;
        let mode = mode_param(p, "mode")?;
        Ok(type_ref_response(program.get_resolved_type_reference_directive(
            file,
            p.str("typeDirectiveName")?,
            mode,
        )))
    }

    pub(crate) fn handle_get_resolved_type_reference_directive_from_reference(
        &self,
        p: Params,
    ) -> ApiResult<Value> {
        let (_sd, program) = self.program_for(p)?;
        let file = resolve_source_file(program, &p.document("sourceFile")?)?;
        let mut mode = mode_param(p, "resolutionMode")?;
        if mode == ModuleKind::NONE {
            mode = program.get_default_resolution_mode_for_file(file);
        }
        Ok(type_ref_response(program.get_resolved_type_reference_directive(
            file,
            p.str("typeDirectiveName")?,
            mode,
        )))
    }
}
