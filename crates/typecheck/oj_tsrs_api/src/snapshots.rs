// session.go handleCreateSnapshot / handleUpdateSnapshot / toAPISnapshotRequest / reconcileSnapshotOpens /
// toLanguageServerSnapshotUpdate / handleGetCurrentLanguageServerSnapshot / createSnapshotResponse /
// computeSnapshotChanges / handleRelease / handleGetDefaultProjectForFile, and proto.go NewProjectResponse.

use std::collections::hash_map::Entry;
use std::sync::Arc;

use rustc_hash::FxHashMap;
use tsrs_core::P;
use tsrs_core::collections::Set;
use tsrs_core::json::{JsonMap, Value};
use tsrs_core::tspath::Path;
use tsrs_ls::autoimport::ProjectID as AutoImportProjectID;
use tsrs_ls::lsutil;
use tsrs_project::dirty::Shared;
use tsrs_project::{
    APICreateProgramRequest, APIReconfigureProgramRequest, APISnapshotRequest, FileChangeSummary,
    ID as ProjectID, Project, Snapshot, SyntheticProjectID, parse_configured_project_id,
    parse_synthetic_project_id,
};
use tsrs_tsoptions::gojson;

use crate::config::config_file_response;
use crate::diagnostics::diagnostic_from_response;
use crate::handler::{ApiError, ApiResult};
use crate::requestfs::{self, RequestFileSystem, RequestParams};
use crate::session::{LanguageServerState, OpenState, Session};
use crate::wire::{DocumentIdentifier, Obj, Params, b, s, strings};

fn file_uri(file_name: &str) -> tsrs_lsproto::DocumentUri {
    tsrs_ls::lsconv::file_name_to_document_uri(file_name)
}

impl DocumentIdentifier {
    /// Go `ToURI`.
    fn to_uri(&self, cwd: &str) -> tsrs_lsproto::DocumentUri {
        match self {
            DocumentIdentifier::Uri(u) => tsrs_lsproto::DocumentUri(u.clone()),
            DocumentIdentifier::FileName(_) => file_uri(&self.to_absolute_file_name(cwd)),
        }
    }
}

/// Go `NewProjectResponse`.
fn project_response(p: &Project) -> Value {
    let command_line = p.command_line.expect("project_response called with unloaded project");
    let config_file_name =
        (p.kind == tsrs_project::Kind::Configured).then(|| s(p.config_file_name().to_string()));
    Obj::new()
        .set("id", s(p.id().0))
        .set_opt("configFileName", config_file_name)
        .set("currentDirectory", s(p.current_directory()))
        .set("dirty", b(p.is_dirty()))
        .set("parsedCommandLine", config_file_response(&command_line))
        .build()
}

fn same_project(a: &Shared<Project>, b: &Shared<Project>) -> bool {
    std::ptr::eq::<Project>(&raw const **a, &raw const **b)
}

fn same_program(a: &Project, b: &Project) -> bool {
    match (a.get_program(), b.get_program()) {
        (Some(x), Some(y)) => std::ptr::eq(x, y),
        (None, None) => true,
        _ => false,
    }
}

/// Go `computeSnapshotChanges`.
fn compute_snapshot_changes(prev: &Snapshot, next: &Snapshot) -> Value {
    let prev_projects = prev.project_collection.projects_by_id();
    let next_projects = next.project_collection.projects_by_id();
    let mut removed = Vec::new();
    let mut changed = JsonMap::new();
    for (id, old) in &prev_projects {
        match next_projects.get(id) {
            None => removed.push(s(old.id().0)),
            Some(new) => {
                if same_project(old, new) || same_program(old, new) {
                    continue;
                }
                let empty = FxHashMap::default();
                let old_files =
                    old.get_program().map_or(&empty, tsrs_compiler::Program::files_by_path);
                let new_files =
                    new.get_program().map_or(&empty, tsrs_compiler::Program::files_by_path);
                let mut changed_files: Vec<&Path> = Vec::new();
                let mut deleted_files: Vec<&Path> = Vec::new();
                #[expect(
                    clippy::iter_over_hash_type,
                    reason = "both lists are sorted before they are serialized"
                )]
                for (path, old_file) in old_files {
                    match new_files.get(path) {
                        None => deleted_files.push(path),
                        Some(new_file)
                            if !std::ptr::eq::<tsrs_ast::SourceFile>(
                                &raw const **old_file,
                                &raw const **new_file,
                            ) =>
                        {
                            changed_files.push(path);
                        }
                        _ => {}
                    }
                }
                if !changed_files.is_empty() || !deleted_files.is_empty() {
                    // Go map iteration order is random; sort for deterministic output.
                    changed_files.sort();
                    deleted_files.sort();
                    let o = Obj::new()
                        .set_opt(
                            "changedFiles",
                            (!changed_files.is_empty())
                                .then(|| strings(changed_files.iter().map(|p| p.as_str()))),
                        )
                        .set_opt(
                            "deletedFiles",
                            (!deleted_files.is_empty())
                                .then(|| strings(deleted_files.iter().map(|p| p.as_str()))),
                        );
                    changed.insert(new.id().0, o.build());
                }
            }
        }
    }
    Obj::new()
        .set_opt("changedProjects", (!changed.is_empty()).then(|| Value::Object(changed)))
        .set_opt("removedProjects", (!removed.is_empty()).then(|| Value::Array(removed)))
        .build()
}

/// Parsed `SnapshotRequestChangesParams` pieces the response needs after the request is consumed.
struct RequestEcho {
    has_create_programs: bool,
    open_files: Option<Vec<DocumentIdentifier>>,
}

/// Go `languageServerSnapshotUpdate`.
struct LanguageServerSnapshotUpdate {
    request: Arc<APISnapshotRequest>,
    open_state: OpenState,
}

impl LanguageServerSnapshotUpdate {
    /// Go `languageServerSnapshotUpdate.commit`.
    fn commit(&self, state: &mut LanguageServerState, snapshot: &Snapshot) {
        state.open_state = self.open_state.clone();
        if let Some(remove_programs) = &self.request.remove_programs {
            #[expect(
                clippy::iter_over_hash_type,
                reason = "removals of distinct keys commute; Go ranges the set too"
            )]
            for program_id in remove_programs.keys() {
                state.created_programs.delete(program_id);
            }
        }
        for program in snapshot.created_programs() {
            let program = program.as_ref().expect("created program result missing");
            let id = program.id();
            let Some(program_id) = id.synthetic() else {
                panic!("created program has non-synthetic project ID: {}", id.0);
            };
            state.created_programs.add(program_id);
        }
    }
}

impl Session {
    fn create_programs_request(
        &self,
        item: &Value,
        what: &str,
        i: usize,
    ) -> ApiResult<APICreateProgramRequest> {
        if matches!(item, Value::Null) {
            return Err(ApiError::client(format!("{what}[{i}] must not be null")));
        }
        let p = Params(item);
        p.object()?;
        let cwd = self.current_directory();
        let root_file_names = DocumentIdentifier::parse_list(p.array("rootFiles")?, "rootFiles")?
            .iter()
            .map(|d| d.to_absolute_file_name(cwd))
            .collect();
        let options = crate::raw_options::compiler_options_from_raw(p.get("compilerOptions"), cwd)?;
        let mut request = APICreateProgramRequest {
            root_file_names,
            compiler_options: Some(P::new(options)),
            project_references: Vec::new(),
            config_file_parsing_diagnostics: Vec::new(),
            module_resolver_factory: None,
            module_resolver_id: 0,
        };
        let opts = Params(p.get("options"));
        if opts.has("projectReferences") {
            request.project_references = opts
                .array("projectReferences")?
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let reference = gojson::project_reference_from_go_json(r).map_err(ApiError::invalid_request)?;
                    // Go resolves references without making them absolute and crashes on a relative, empty or
                    // null path; reject it instead.
                    if !tsrs_core::tspath::is_rooted_disk_path(&reference.path) {
                        return Err(ApiError::client(format!(
                            "projectReferences[{i}].path must be an absolute path to a project directory or tsconfig file, got {:?}",
                            reference.path
                        )));
                    }
                    Ok(reference)
                })
                .collect::<ApiResult<_>>()?;
        }
        request.config_file_parsing_diagnostics = opts
            .array("configFileParsingDiagnostics")?
            .iter()
            .map(diagnostic_from_response)
            .collect::<ApiResult<_>>()?;
        let resolver = opts.u64("moduleResolver")?;
        if resolver != 0 {
            let (factory, id) = self.module_resolver_factory(resolver)?;
            request.module_resolver_factory = Some(factory);
            request.module_resolver_id = id;
        }
        Ok(request)
    }

    /// Go `toAPISnapshotRequest`.
    fn to_api_snapshot_request(&self, p: Params) -> ApiResult<(APISnapshotRequest, RequestEcho)> {
        let cwd = self.current_directory().to_string();
        let mut req = APISnapshotRequest::default();
        for d in DocumentIdentifier::parse_list(p.array("openProjects")?, "openProjects")? {
            let config_file_name = d.to_absolute_file_name(&cwd);
            let id =
                parse_configured_project_id(&self.to_path(&config_file_name)).ok_or_else(|| {
                    ApiError::client(format!("invalid configured project ID: {config_file_name}"))
                })?;
            req.ensure_programs.get_or_insert_with(Set::new).add(id.as_id());
            req.open_projects.get_or_insert_with(Set::new).add(config_file_name);
        }
        for d in DocumentIdentifier::parse_list(p.array("closeProjects")?, "closeProjects")? {
            req.close_projects
                .get_or_insert_with(Set::new)
                .add(self.to_path(&d.to_absolute_file_name(&cwd)));
        }
        let open_files_list = if p.has("openFiles") {
            Some(DocumentIdentifier::parse_list(p.array("openFiles")?, "openFiles")?)
        } else {
            None
        };
        for d in open_files_list.iter().flatten() {
            let file_name = d.to_absolute_file_name(&cwd);
            let path = self.to_path(&file_name);
            let open = req.open_files.get_or_insert_with(FxHashMap::default);
            if let Entry::Vacant(entry) = open.entry(path) {
                let path = entry.key().clone();
                entry.insert(file_name.clone());
                req.ensure_files.insert(path, file_name);
            }
        }
        for d in DocumentIdentifier::parse_list(p.array("closeFiles")?, "closeFiles")? {
            req.close_files
                .get_or_insert_with(Set::new)
                .add(self.to_path(&d.to_uri(&cwd).file_name()));
        }
        let create = p.array("createPrograms")?;
        req.create_programs = create
            .iter()
            .enumerate()
            .map(|(i, item)| self.create_programs_request(item, "createPrograms", i))
            .collect::<Result<_, _>>()?;
        let mut reconfigured: Set<SyntheticProjectID> = Set::new();
        for (i, item) in p.array("reconfigurePrograms")?.iter().enumerate() {
            if matches!(item, Value::Null) {
                return Err(ApiError::client(format!("reconfigurePrograms[{i}] must not be null")));
            }
            let id_text = Params(item).str("id")?.to_string();
            let program_id = parse_synthetic_project_id(&id_text).ok_or_else(|| {
                ApiError::client(format!("invalid synthetic project handle: {id_text}"))
            })?;
            if reconfigured.has(&program_id) {
                return Err(ApiError::client(format!(
                    "synthetic program reconfigured more than once: {}",
                    program_id.0
                )));
            }
            reconfigured.add(program_id.clone());
            let request = self.create_programs_request(item, "reconfigurePrograms", i)?;
            req.reconfigure_programs.push(APIReconfigureProgramRequest { program_id, request });
        }
        let remove = p.strings("removePrograms")?;
        for id in remove {
            // Decoded (and validated by `predecode`) like Go's SyntheticProjectID, which normalizes the number.
            let program_id = parse_synthetic_project_id(&id).unwrap_or(SyntheticProjectID(id));
            if reconfigured.has(&program_id) {
                return Err(ApiError::client(format!(
                    "synthetic program cannot be reconfigured and removed: {}",
                    program_id.0
                )));
            }
            req.remove_programs.get_or_insert_with(Set::new).add(program_id);
        }
        match p.get("ensurePrograms") {
            Value::Null => {}
            Value::Bool(true) => req.ensure_all_programs = true,
            Value::Array(items) => {
                for item in items {
                    match item {
                        Value::String(id) => req
                            .ensure_programs
                            .get_or_insert_with(Set::new)
                            .add(ProjectID(id.clone())),
                        // Go decodes into []project.ID: null is the zero ID.
                        Value::Null => req
                            .ensure_programs
                            .get_or_insert_with(Set::new)
                            .add(ProjectID(String::new())),
                        _ => {
                            return Err(ApiError::invalid_request(
                                "ensurePrograms must be true or an array of project IDs",
                            ));
                        }
                    }
                }
            }
            _ => {
                return Err(ApiError::invalid_request(
                    "ensurePrograms must be true or an array of project IDs",
                ));
            }
        }
        Ok((
            req,
            RequestEcho {
                has_create_programs: p.has("createPrograms"),
                open_files: open_files_list,
            },
        ))
    }

    /// Go `reconcileSnapshotOpens`.
    fn reconcile_snapshot_opens(
        &self,
        req: &mut APISnapshotRequest,
        base: &OpenState,
    ) -> OpenState {
        let mut state = base.clone();
        if let Some(close) = &mut req.close_projects {
            let keys: Vec<Path> = close.keys().iter().cloned().collect();
            for path in keys {
                if state.open_projects.has(&path) {
                    state.open_projects.delete(&path);
                } else {
                    close.delete(&path);
                }
            }
        }
        if let Some(open) = &mut req.open_projects {
            let keys: Vec<String> = open.keys().iter().cloned().collect();
            for name in keys {
                let path = self.to_path(&name);
                if state.open_projects.has(&path) {
                    open.delete(&name);
                } else {
                    state.open_projects.add(path);
                }
            }
        }
        if let Some(close) = &mut req.close_files {
            let keys: Vec<Path> = close.keys().iter().cloned().collect();
            for path in keys {
                if state.open_files.has(&path) {
                    state.open_files.delete(&path);
                } else {
                    close.delete(&path);
                }
            }
        }
        if let Some(open) = &mut req.open_files {
            let keys: Vec<Path> = open.keys().cloned().collect();
            for path in keys {
                if state.open_files.has(&path) {
                    open.remove(&path);
                } else {
                    state.open_files.add(path);
                }
            }
        }
        state
    }

    /// Go `toLanguageServerSnapshotUpdate`.
    fn to_language_server_snapshot_update(
        &self,
        changes: Params,
        state: &LanguageServerState,
    ) -> ApiResult<(LanguageServerSnapshotUpdate, RequestEcho)> {
        let (mut request, echo) = self.to_api_snapshot_request(changes)?;
        let open_state = self.reconcile_snapshot_opens(&mut request, &state.open_state);

        if let Some(remove_programs) = &mut request.remove_programs {
            let keys: Vec<SyntheticProjectID> = remove_programs.keys().iter().cloned().collect();
            for program_id in keys {
                if !state.created_programs.has(&program_id) {
                    remove_programs.delete(&program_id);
                }
            }
        }
        for reconfigure in &request.reconfigure_programs {
            if !state.created_programs.has(&reconfigure.program_id) {
                return Err(ApiError::client(format!(
                    "synthetic program is not owned by this API session: {}",
                    reconfigure.program_id.0
                )));
            }
        }
        Ok((LanguageServerSnapshotUpdate { request: Arc::new(request), open_state }, echo))
    }

    /// Go `toFileChangeSummary`.
    fn to_file_change_summary(&self, v: &Value) -> ApiResult<FileChangeSummary> {
        let mut summary = FileChangeSummary::default();
        if matches!(v, Value::Null) {
            return Ok(summary);
        }
        let p = Params(v);
        p.object()?;
        if p.bool("invalidateAll")? {
            summary.invalidate_all = true;
            summary.includes_watch_change_outside_node_modules = true;
            return Ok(summary);
        }
        let cwd = self.current_directory();
        summary.changed.m.extend(
            (DocumentIdentifier::parse_list(p.array("changed")?, "changed")?)
                .into_iter()
                .map(|d| d.to_uri(cwd)),
        );
        summary.created.m.extend(
            (DocumentIdentifier::parse_list(p.array("created")?, "created")?)
                .into_iter()
                .map(|d| d.to_uri(cwd)),
        );
        summary.deleted.m.extend(
            (DocumentIdentifier::parse_list(p.array("deleted")?, "deleted")?)
                .into_iter()
                .map(|d| d.to_uri(cwd)),
        );
        if summary.changed.len() + summary.created.len() + summary.deleted.len() > 0 {
            summary.includes_watch_change_outside_node_modules = true;
        }
        Ok(summary)
    }

    fn snapshot_response(
        &self,
        snapshot: &Snapshot,
        base: Option<&Snapshot>,
        echo: &RequestEcho,
    ) -> ApiResult<Value> {
        let cwd = self.current_directory();
        // createSnapshotOperationResponse
        let mut operation = Obj::new();
        if echo.has_create_programs {
            let mut ids = Vec::new();
            ids = snapshot.created_programs().iter().try_fold(ids, |mut ids, created| {
                let created = created
                    .as_ref()
                    .ok_or_else(|| ApiError::internal("created program result missing"))?;
                let id = created.id();
                let synthetic = id.synthetic().ok_or_else(|| {
                    ApiError::internal("created program has non-synthetic project ID")
                })?;
                ids.push(s(synthetic.0));
                Ok::<_, ApiError>(ids)
            })?;
            operation = operation.set("createdPrograms", Value::Array(ids));
        }
        if let Some(open_files) = &echo.open_files {
            let mut results = Vec::new();
            results = open_files.iter().try_fold(results, |mut results, f| {
                let project = snapshot.get_default_project(&f.to_uri(cwd)).ok_or_else(|| {
                    ApiError::internal(format!(
                        "no project found for opened file {}",
                        f.to_absolute_file_name(cwd)
                    ))
                })?;
                results.push(Obj::new().set("project", s(project.id().0)).build());
                Ok::<_, ApiError>(results)
            })?;
            operation = operation.set("openedFiles", Value::Array(results));
        }
        let mut projects = Vec::new();
        let mut out = Obj::new().set("snapshot", Value::Number(snapshot.id() as f64));
        match base {
            None => {
                projects.extend(snapshot.project_collection.projects().into_iter().filter_map(
                    |proj| {
                        if proj.command_line.is_some() {
                            Some(project_response(&proj))
                        } else {
                            None
                        }
                    },
                ));
                out = out.set("projects", Value::Array(projects));
            }
            Some(base) => {
                let old = base.project_collection.projects_by_id();
                projects.extend(snapshot.project_collection.projects_by_id().iter().filter_map(
                    |(id, new)| {
                        let include = match old.get(id) {
                            None => true,
                            Some(old) => !same_project(old, new),
                        };
                        if include && new.command_line.is_some() {
                            Some(project_response(new))
                        } else {
                            None
                        }
                    },
                ));
                out = out.set("projects", Value::Array(projects));
                // `json:"changes,omitempty"` (json/v2): a SnapshotChanges that encodes as `{}` is omitted.
                let changes = compute_snapshot_changes(base, snapshot);
                if !matches!(&changes, Value::Object(o) if o.is_empty()) {
                    out = out.set("changes", changes);
                }
            }
        }
        Ok(out.set("operation", operation.build()).build())
    }

    fn module_resolution_error(snapshot: &Snapshot) -> ApiResult<()> {
        if let Some(result) =
            snapshot.project_collection.projects().into_iter().find_map(|project| {
                if let Some(program) = project.get_program()
                    && let Some(err) = program.module_resolution_error()
                {
                    return Some(Err(ApiError::internal(err.to_string())));
                }

                None
            })
        {
            return result;
        }
        Ok(())
    }

    /// Go `validatePreparedAutoImports`: a prepared snapshot is returned only if its
    /// registry can serve auto-imports for the requested file and default project.
    fn validate_prepared_auto_imports(
        &self,
        snapshot: &Snapshot,
        file: Option<&DocumentIdentifier>,
    ) -> ApiResult<()> {
        let Some(file) = file else { return Ok(()) };
        let uri = file.to_uri(self.current_directory());
        let prepared = snapshot.get_default_project(&uri).is_some_and(|project| {
            snapshot.auto_import_registry().is_some_and(|registry| {
                registry.is_prepared_for_importing_file(
                    &uri.file_name(),
                    &AutoImportProjectID(project.id().0),
                    snapshot.user_preferences(),
                )
            })
        });
        if prepared {
            Ok(())
        } else {
            Err(ApiError::client(format!("could not prepare auto-imports for {file}")))
        }
    }

    /// Go's two snapshot handlers copy the extra fields after `toAPISnapshotRequest`.
    fn snapshot_preferences_and_auto_imports(
        &self,
        p: Params,
        req: &mut APISnapshotRequest,
    ) -> ApiResult<Option<DocumentIdentifier>> {
        if let Value::Object(config) = p.get("userPreferences") {
            req.user_preferences = Some(lsutil::parse_api_user_preferences(config));
        }
        let file = if p.has("prepareAutoImports") {
            Some(p.document("prepareAutoImports")?)
        } else {
            None
        };
        if let Some(file) = &file {
            req.prepare_auto_imports = file.to_uri(self.current_directory());
        }
        Ok(file)
    }

    /// Go `requestfilesystem.NewForUpdate` for the request's optional `fileSystem`.
    fn request_file_system(
        &self,
        p: Params,
        base: Option<&RequestFileSystem>,
        file_changes: &mut FileChangeSummary,
    ) -> ApiResult<Option<(Arc<RequestFileSystem>, bool)>> {
        if !p.has("fileSystem") {
            return Ok(None);
        }
        let params = RequestParams::parse(p.get("fileSystem")).map_err(ApiError::client)?;
        let fs = requestfs::new_for_update(
            &params,
            ScopedFs::wrap(self.fs()),
            base,
            self.current_directory(),
            file_changes,
        )
        .map_err(ApiError::client)?;
        // Go sets ReplaceFileSystem from the *request's* kind, not the compacted result's.
        Ok(Some((Arc::new(fs), params.kind == requestfs::Kind::Full)))
    }

    pub(crate) fn handle_create_snapshot(&self, p: Params) -> ApiResult<Value> {
        if !matches!(p.0, Value::Null) {
            p.object()?;
        }
        let (mut req, echo) = self.to_api_snapshot_request(p)?;
        let prepare_auto_imports = self.snapshot_preferences_and_auto_imports(p, &mut req)?;
        let open_state = self.reconcile_snapshot_opens(&mut req, &OpenState::default());
        let mut file_changes = self.to_file_change_summary(p.get("fileNotifications"))?;
        let snapshot_fs =
            if let Some((fs, replace)) = self.request_file_system(p, None, &mut file_changes)? {
                req.layered_file_system =
                    Some(Arc::clone(&fs) as Arc<dyn tsrs_project::LayeredFileSystem>);
                req.replace_file_system = replace;
                Some(fs)
            } else {
                // Attribution only: same host filesystem, never a replacement.
                req.file_system = Some(ScopedFs::wrap(self.snapshot_host_fs()));
                None
            };
        let ctx = tsrs_core::context::Context::background();
        let root = self.snapshot_host.new_root_snapshot();
        // The root reference is released even if building a program unwinds (see `Snapshot::clone_snapshot`).
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.snapshot_host.clone_snapshot(&ctx, &root, file_changes, Some(Arc::new(req)))
        }));
        root.deref();
        let result = result.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        let snapshot = match result {
            Ok(s) => s,
            Err((s, err)) => {
                s.deref();
                return Err(ApiError::client(format!(
                    "failed to create snapshot: {}",
                    err.message
                )));
            }
        };
        if let Err(e) =
            self.validate_prepared_auto_imports(&snapshot, prepare_auto_imports.as_ref())
        {
            snapshot.deref();
            return Err(e);
        }
        if let Err(e) = Self::module_resolution_error(&snapshot) {
            snapshot.deref();
            return Err(e);
        }
        let response = match self.snapshot_response(&snapshot, None, &echo) {
            Ok(r) => r,
            Err(e) => {
                snapshot.deref();
                return Err(e);
            }
        };
        self.register_snapshot(snapshot, open_state, snapshot_fs);
        Ok(response)
    }

    pub(crate) fn handle_update_snapshot(&self, p: Params) -> ApiResult<Value> {
        p.object()?;
        let base_handle = p.u64("snapshot")?;
        // retainSnapshotData: the Arc pins the base for the duration of the update.
        let base = self.snapshot_data(base_handle)?;
        let changes = Params(p.get("changes"));
        if !matches!(changes.0, Value::Null) {
            changes.object()?;
        }
        let (mut req, echo) = self.to_api_snapshot_request(changes)?;
        let prepare_auto_imports = self.snapshot_preferences_and_auto_imports(changes, &mut req)?;
        let open_state = self.reconcile_snapshot_opens(&mut req, &base.open_state);
        let mut file_changes = self.to_file_change_summary(changes.get("fileNotifications"))?;
        let new_fs =
            self.request_file_system(changes, base.file_system.as_deref(), &mut file_changes)?;
        let replaced = new_fs.as_ref().is_some_and(|(_, replace)| *replace);
        let snapshot_fs = new_fs.map(|(fs, _)| fs).or_else(|| base.file_system.clone());
        if let Some(fs) = &snapshot_fs {
            req.layered_file_system =
                Some(Arc::clone(fs) as Arc<dyn tsrs_project::LayeredFileSystem>);
            req.replace_file_system = replaced;
        } else {
            // Attribution only: same host filesystem, never a replacement.
            req.file_system = Some(ScopedFs::wrap(self.snapshot_host_fs()));
        }
        let ctx = tsrs_core::context::Context::background();
        let snapshot = match self.snapshot_host.clone_snapshot(
            &ctx,
            &base.snapshot,
            file_changes,
            Some(Arc::new(req)),
        ) {
            Ok(s) => s,
            Err((s, err)) => {
                s.deref();
                return Err(ApiError::client(format!(
                    "failed to update snapshot: {}",
                    err.message
                )));
            }
        };
        if let Err(e) =
            self.validate_prepared_auto_imports(&snapshot, prepare_auto_imports.as_ref())
        {
            snapshot.deref();
            return Err(e);
        }
        if let Err(e) = Self::module_resolution_error(&snapshot) {
            snapshot.deref();
            return Err(e);
        }
        let response = match self.snapshot_response(&snapshot, Some(&base.snapshot), &echo) {
            Ok(r) => r,
            Err(e) => {
                snapshot.deref();
                return Err(e);
            }
        };
        self.register_snapshot(snapshot, open_state, snapshot_fs);
        Ok(response)
    }

    /// Go `handleGetCurrentLanguageServerSnapshot`.
    pub(crate) fn handle_get_current_language_server_snapshot(
        &self,
        p: Params,
    ) -> ApiResult<Value> {
        let Some(project_session) = &self.project_session else {
            return Err(ApiError::client(
                "getCurrentLanguageServerSnapshot requires an LSP-connected API session",
            ));
        };
        let base_handle = p.u64("baseSnapshot")?;
        // retainSnapshotData: the Arc pins the base for the duration of the update.
        let base = if base_handle != 0 { Some(self.snapshot_data(base_handle)?) } else { None };

        let mut state = self.language_server.lock().unwrap();

        let changes = Params(p.get("changes"));
        if !matches!(changes.0, Value::Null) {
            changes.object()?;
        }
        let (update, echo) = self.to_language_server_snapshot_update(changes, &state)?;

        let ctx = project_session.with_current_locale(&tsrs_core::context::Context::background());
        let snapshot = project_session
            .api_update(&ctx, &FileChangeSummary::default(), Some(Arc::clone(&update.request)))
            .map_err(|err| {
                ApiError::client(format!(
                    "failed to update language server snapshot: {}",
                    err.message
                ))
            })?;

        update.commit(&mut state, &snapshot);
        let response =
            match self.snapshot_response(&snapshot, base.as_ref().map(|b| &*b.snapshot), &echo) {
                Ok(r) => r,
                Err(e) => {
                    snapshot.deref();
                    return Err(e);
                }
            };
        self.register_snapshot(snapshot, state.open_state.clone(), None);
        Ok(response)
    }

    pub(crate) fn handle_release(&self, p: Params) -> ApiResult<Value> {
        let handle = p.u64("snapshot")?;
        if handle == 0 {
            return Err(ApiError::client("empty handle"));
        }
        self.release_snapshot(handle)?;
        Ok(Value::Bool(true))
    }

    pub(crate) fn handle_get_default_project_for_file(&self, p: Params) -> ApiResult<Value> {
        // Go decodes all params before looking anything up.
        let file = p.document("file")?;
        let sd = self.snapshot_data(p.u64("snapshot")?)?;
        Ok(match sd.snapshot.get_default_project(&file.to_uri(self.current_directory())) {
            Some(proj) => project_response(&proj),
            None => Value::Null,
        })
    }
}

/// Runs every filesystem call inside the request that created the snapshot (`RequestScope`), so client
/// callbacks made from compiler worker threads while the program is built are attributed to that request by
/// the transport's re-entrancy checks (`crates/tsrs_api_transport/INTEGRATION.md`). A snapshot may keep the
/// wrapper after its request ended; later lazy reads then count against a finished request, which holds
/// nothing.
pub(crate) struct ScopedFs {
    inner: Arc<dyn tsrs_vfs::FS>,
    scope: tsrs_api_transport::reentrancy::RequestScope,
}

impl ScopedFs {
    /// Wraps `inner` when the current thread serves a transport request; otherwise returns `inner`.
    fn wrap(inner: Arc<dyn tsrs_vfs::FS>) -> Arc<dyn tsrs_vfs::FS> {
        match tsrs_api_transport::reentrancy::RequestScope::current() {
            Some(scope) => Arc::new(ScopedFs { inner, scope }),
            None => inner,
        }
    }
}

impl tsrs_vfs::FS for ScopedFs {
    fn use_case_sensitive_file_names(&self) -> bool {
        self.inner.use_case_sensitive_file_names()
    }
    fn file_exists(&self, path: &str) -> bool {
        self.scope.enter(|| self.inner.file_exists(path))
    }
    fn read_file(&self, path: &str) -> Option<String> {
        self.scope.enter(|| self.inner.read_file(path))
    }
    fn write_file(&self, path: &str, data: &str) -> Result<(), String> {
        self.scope.enter(|| self.inner.write_file(path, data))
    }
    fn append_file(&self, path: &str, data: &str) -> Result<(), String> {
        self.scope.enter(|| self.inner.append_file(path, data))
    }
    fn remove(&self, path: &str) -> Result<(), String> {
        self.scope.enter(|| self.inner.remove(path))
    }
    fn chtimes(
        &self,
        path: &str,
        a: std::time::SystemTime,
        m: std::time::SystemTime,
    ) -> Result<(), String> {
        self.scope.enter(|| self.inner.chtimes(path, a, m))
    }
    fn directory_exists(&self, path: &str) -> bool {
        self.scope.enter(|| self.inner.directory_exists(path))
    }
    fn get_accessible_entries(&self, path: &str) -> tsrs_vfs::Entries {
        self.scope.enter(|| self.inner.get_accessible_entries(path))
    }
    fn stat(&self, path: &str) -> Option<tsrs_vfs::FileInfo> {
        self.scope.enter(|| self.inner.stat(path))
    }
    fn realpath(&self, path: &str) -> String {
        self.scope.enter(|| self.inner.realpath(path))
    }
}
