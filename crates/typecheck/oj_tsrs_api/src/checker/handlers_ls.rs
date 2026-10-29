// Language-service backed handlers (Go tsc/internal/api/session.go: handleGetSignatureUsages,
// handleGetCompletionsAtPosition, handleGetReferencedSymbolsForNode, handleGetImportAdderEdits).
//
// Like Go these do not take the persistent API checker for the whole request: the language service
// acquires its own checkers (find-all-references acquires several, which would deadlock on the
// single-slot API checker). Only completions with `includeSymbol` use the API checker lifetime, so the
// returned symbols stay resolvable by later checker requests (Go does the same).

use std::sync::Arc;

use tsrs_ast::Symbol;
use tsrs_compiler::Program;
use tsrs_core::P;
use tsrs_core::context::{CheckerLifetime, Context, with_checker_lifetime};
use tsrs_core::json::Value;
use tsrs_ls::autoimport::ProjectID as LsProjectID;
use tsrs_project::Snapshot;

use super::host::{CheckerError, CheckerErrorKind, CheckerHost, CheckerResult};
use super::json::obj;
use super::params::{Params, SymbolReference};
use super::setup::{SnapshotCtx, resolve_symbol_for_program};

fn snapshot_ctx<'h>(host: &'h dyn CheckerHost, p: &Params) -> CheckerResult<SnapshotCtx<'h>> {
    SnapshotCtx::new(host, p.u64("snapshot")?, p.project()?)
}

fn program_of(snapshot: &Snapshot, project: &str) -> CheckerResult<&'static Program> {
    let proj = snapshot
        .project_collection
        .get_project(&tsrs_project::ID(project.to_string()))
        .ok_or_else(|| CheckerError::client(format!("project {project} not found")))?;
    proj.get_program().ok_or_else(|| CheckerError::client("project has no program"))
}

/// Go `setupLanguageService`.
fn language_service(
    snapshot: &Arc<Snapshot>,
    program: &'static Program,
    project: &str,
    active_file: &str,
) -> CheckerResult<tsrs_ls::LanguageService> {
    if snapshot.project_collection.get_project(&tsrs_project::ID(project.to_string())).is_none() {
        return Err(CheckerError::client(format!("project {project} not found")));
    }
    let host: Arc<dyn tsrs_ls::Host> = Arc::<Snapshot>::clone(snapshot);
    Ok(tsrs_ls::new_language_service(LsProjectID(project.to_string()), program, host, active_file))
}

pub fn get_signature_usages(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let sd = snapshot_ctx(host, p)?;
    let program = sd.program()?;
    let decl = host.resolve_node_handle(program, p.string("signatureDecl")?)?;
    let ls = language_service(&sd.scope.snapshot, program, &sd.project, "")?;
    let usages = ls.get_signature_usages(&host.context(), decl);
    if usages.is_empty() {
        return Ok(Value::Array(Vec::new())); // Go nil slice: json/v2 encodes []
    }
    let mut out = Vec::with_capacity(usages.len());
    out = usages.into_iter().try_fold(out, |mut out, usage| {
        let mut o = obj();
        o.set("name", Value::String(host.node_handle(usage.name)?));
        if let Some(call) = usage.call {
            o.set("call", Value::String(host.node_handle(call)?));
        }
        out.push(o.build());

        Ok::<_, CheckerError>(out)
    })?;
    Ok(Value::Array(out))
}

pub fn get_referenced_symbols_for_node(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let sd = snapshot_ctx(host, p)?;
    let program = sd.program()?;
    let node = host.resolve_node_handle(program, p.string("node")?)?;
    let position = p.u32("position")?;
    let ls = language_service(&sd.scope.snapshot, program, &sd.project, "")?;
    let entries = ls.get_referenced_symbols_for_node_exported(
        &host.context(),
        position,
        node,
        program.get_source_files(),
    );
    if entries.is_empty() {
        return Ok(Value::Array(Vec::new())); // Go nil slice: json/v2 encodes []
    }
    let mut out = Vec::new();
    for entry in &entries {
        let Some(def) = entry.definition_node() else {
            continue;
        };
        let mut refs = Vec::new();
        refs = entry.references().iter().try_fold(refs, |mut refs, r| {
            if let Some(n) = r.node() {
                refs.push(Value::String(host.node_handle(n)?));
            }

            Ok::<_, CheckerError>(refs)
        })?;
        let mut o = obj();
        o.set("definition", Value::String(host.node_handle(def)?));
        if let Some(sym) = entry.definition_symbol() {
            o.set("symbol", sd.symbol_response(sym, &sd.project)?);
        }
        o.set("references", Value::Array(refs));
        out.push(o.build());
    }
    if out.is_empty() {
        // Go: `var result []ReferencedSymbolEntry` stays nil when every entry lacks a definition (encodes as []).
        return Ok(Value::Array(Vec::new())); // Go nil slice: json/v2 encodes []
    }
    Ok(Value::Array(out))
}

pub fn get_completions_at_position(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    let include_symbol = p.bool("includeSymbol")?;
    let mut ctx = host.context();
    if include_symbol {
        ctx = with_checker_lifetime(&ctx, CheckerLifetime::API);
    }
    let sd = snapshot_ctx(host, p)?;
    let file = p.document("file")?;
    let file_name = file.to_file_name();
    let position = p.u32("position")?;
    let trigger = match p.raw("triggerCharacter") {
        None => None,
        Some(Value::String(s)) => Some(s.as_str()),
        Some(_) => {
            return Err(CheckerError::invalid("field \"triggerCharacter\": expected a string"));
        }
    };
    let run = |snapshot: &Arc<Snapshot>,
               program: &'static Program,
               ctx: &Context|
     -> CheckerResult<Result<Option<tsrs_ls::CompletionList>, tsrs_lsproto::Error>> {
        let Some(source_file) = program.get_source_file(&file_name) else {
            return Ok(Ok(None));
        };
        let ls = language_service(snapshot, program, &sd.project, source_file.file_name())?;
        let internal = source_file.get_position_map().utf16_to_utf8(position);
        // With includeSymbol the language service takes the API checker itself (Go: same).
        let _lease = if include_symbol { Some(super::lease::acquire(program)?) } else { None };
        Ok(ls.get_completions_at_position(ctx, source_file, internal, trigger, include_symbol))
    };

    let program = sd.program()?;
    let mut prepared: Option<Arc<Snapshot>> = None;
    let mut result = run(&sd.scope.snapshot, program, &ctx)?;
    if matches!(&result, Err(e) if tsrs_ls::is_err_needs_auto_imports(e)) {
        if include_symbol {
            return Err(CheckerError::client(format!(
                "snapshot is not prepared for auto-imports for {}",
                file.display()
            )));
        }
        let Some(source_file) = program.get_source_file(&file_name) else {
            return Ok(Value::Null);
        };
        let snapshot =
            host.clone_snapshot_with_auto_imports(&sd.scope.snapshot, source_file.file_name())?;
        host.try_adopt_snapshot_in_background(&sd.scope.snapshot, &snapshot);
        let outcome =
            program_of(&snapshot, &sd.project).and_then(|program| run(&snapshot, program, &ctx));
        prepared = Some(snapshot);
        result = match outcome {
            Ok(r) => r,
            Err(e) => {
                prepared.take().unwrap().deref();
                return Err(e);
            }
        };
    }
    let release_prepared = |prepared: Option<Arc<Snapshot>>| {
        if let Some(s) = prepared {
            s.deref();
        }
    };
    let list = match result {
        Ok(Some(list)) => list,
        Ok(None) => {
            release_prepared(prepared);
            return Ok(Value::Null);
        }
        Err(e) => {
            release_prepared(prepared);
            // Go returns the language service's error as is (ErrNeedsAutoImports, a cancelled context).
            return Err(CheckerError { kind: CheckerErrorKind::Internal, message: e.message });
        }
    };
    let mut entries = Vec::with_capacity(list.items.len());
    let mut uses_prepared_symbols = false;
    for item in &list.items {
        let ci = &item.completion_item;
        let mut o = obj();
        o.set("name", Value::String(ci.label.clone()));
        // encoding/json v2 `omitempty`: a uint32 0 is still emitted; `*string` pointing at "" and an
        // all-empty labelDetails object ({}) are omitted.
        o.num("kind", ci.kind.as_ref().map_or(0, |k| k.0) as f64);
        for (key, value) in [
            ("sortText", &ci.sort_text),
            ("insertText", &ci.insert_text),
            ("filterText", &ci.filter_text),
            ("detail", &ci.detail),
        ] {
            if let Some(v) = value {
                o.str_nonempty(key, v);
            }
        }
        if let Some(details) = &ci.label_details {
            let mut d = obj();
            if let Some(v) = &details.detail {
                d.str_nonempty("detail", v);
            }
            if let Some(v) = &details.description {
                d.str_nonempty("description", v);
            }
            let d = d.build();
            if !matches!(&d, Value::Object(m) if m.is_empty()) {
                o.set("labelDetails", d);
            }
        }
        if let Some(symbol) = item.symbol {
            uses_prepared_symbols |= prepared.is_some();
            o.set("symbol", sd.symbol_response(symbol, &sd.project)?);
        }
        entries.push(o.build());
    }
    match prepared {
        // Symbols from the auto-import snapshot were handed out: keep it alive with the registry.
        Some(s) if uses_prepared_symbols => sd.scope.registry.retain_snapshot(s)?,
        other => release_prepared(other),
    }
    let mut o = obj();
    o.set("isIncomplete", Value::Bool(list.is_incomplete));
    o.set("entries", Value::Array(entries));
    Ok(o.build())
}

/// Go `handleGetImportAdderEdits`.
pub fn get_import_adder_edits(host: &dyn CheckerHost, p: &Params) -> CheckerResult<Value> {
    // Decode actions like Go's unmarshalling of `[]ImportAdderAction` (before the handler runs, so a decode
    // error consumes no snapshot): wrong JSON kinds are invalid requests; `null` elements and absent fields
    // are zero values. Semantic validation (unknown kind, missing symbol, unresolvable symbol) is a client
    // error raised later, after the auto-import snapshot clone, in Go's order (it consumes a snapshot ID).
    struct Action<'a> {
        kind: String,
        symbol: Option<SymbolReference<'a>>,
        valid: bool,
    }

    // Release the auto-import snapshot on every exit, including a panic in the import adder (Go: defer).
    struct Release(Option<Arc<Snapshot>>);
    impl Drop for Release {
        fn drop(&mut self) {
            if let Some(s) = self.0.take() {
                s.deref();
            }
        }
    }

    let sd = snapshot_ctx(host, p)?;
    let file = p.document("file")?;
    let file_name = file.to_file_name();
    let mut program = sd.program()?;
    let mut source_file = program.get_source_file(&file_name).ok_or_else(|| {
        CheckerError::client(format!("source file not found: {}", file.display()))
    })?;
    let project_id = LsProjectID(sd.project.clone());

    let mut actions: Vec<Action> = Vec::new();
    actions = (p.array("actions")?).iter().try_fold(actions, |mut actions, action| {
        if matches!(action, Value::Null) {
            actions.push(Action { kind: String::new(), symbol: None, valid: true });
            return Ok::<_, CheckerError>(actions);
        }
        let a = Params::new(action, "getImportAdderEdits")?;
        let valid = match a.raw("isValidTypeOnlyUseSite") {
            None => true,
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                return Err(CheckerError::invalid(
                    "field \"isValidTypeOnlyUseSite\": expected a boolean",
                ));
            }
        };
        let symbol = match a.raw("symbol") {
            None => None,
            Some(_) => Some(a.symbol_ref("symbol")?),
        };
        actions.push(Action { kind: a.string("kind")?.to_string(), symbol, valid });

        Ok::<_, CheckerError>(actions)
    })?;
    // Go `handleGetImportAdderEdits`: the per-action checks run inside the action loop, after the working
    // snapshot (possibly a fresh auto-import clone) exists.
    let validate =
        |sd: &SnapshotCtx, program: &'static Program| -> CheckerResult<Vec<(P<Symbol>, bool)>> {
            let mut out = Vec::with_capacity(actions.len());
            out = actions.iter().enumerate().try_fold(out, |mut out, (i, action)| {
                match action.kind.as_str() {
                    "importSymbol" => {
                        let Some(r) = &action.symbol else {
                            return Err(CheckerError::client(format!(
                                "import adder action {i} missing symbol"
                            )));
                        };
                        out.push((resolve_symbol_for_program(sd, program, r)?, action.valid));
                    }
                    kind => {
                        return Err(CheckerError::client(format!(
                            "unknown import adder action kind {kind:?}"
                        )));
                    }
                }

                Ok::<_, CheckerError>(out)
            })?;
            Ok(out)
        };

    let mut working = Arc::clone(&sd.scope.snapshot);
    let mut prepared: Option<Arc<Snapshot>> = None;
    let prepared_for_file = working.auto_import_registry().is_some_and(|r| {
        r.is_prepared_for_importing_file(
            source_file.file_name(),
            &project_id,
            working.user_preferences(),
        )
    });
    if !prepared_for_file {
        let snapshot = host.clone_snapshot_with_auto_imports(&working, source_file.file_name())?;
        host.try_adopt_snapshot_in_background(&working, &snapshot);
        prepared = Some(Arc::clone(&snapshot));
        working = snapshot;
        let resolved = program_of(&working, &sd.project).and_then(|prog| {
            let sf = prog.get_source_file(&file_name).ok_or_else(|| {
                CheckerError::client(format!("source file not found: {}", file.display()))
            })?;
            Ok((prog, sf))
        });
        match resolved {
            Ok((prog, sf)) => {
                program = prog;
                source_file = sf;
            }
            Err(e) => {
                working.deref();
                return Err(e);
            }
        }
    }
    let _release = Release(prepared);
    import_adder_edits(host, &sd, &working, program, source_file, project_id, &validate)
}

fn import_adder_edits(
    host: &dyn CheckerHost,
    sd: &SnapshotCtx,
    working: &Arc<Snapshot>,
    program: &'static Program,
    source_file: P<tsrs_ast::SourceFile>,
    project_id: LsProjectID,
    validate: &dyn Fn(&SnapshotCtx, &'static Program) -> CheckerResult<Vec<(P<Symbol>, bool)>>,
) -> CheckerResult<Value> {
    let Some(registry) = working.auto_import_registry() else {
        // Go returns before looking at the actions.
        return Ok(Value::Array(Vec::new()));
    };
    // Resolve symbols against the working program (Go resolves through a checker-less checkerSetup).
    let symbols = validate(sd, program)?;
    let ctx = host.context();
    let mut checker = program.get_type_checker(&ctx);
    let preferences = working.user_preferences().clone();
    let view = tsrs_ls::autoimport::new_view(
        Some(registry),
        source_file,
        project_id,
        program,
        &mut checker,
        preferences.module_specifier_preferences(),
    );
    let format = tsrs_ls::Host::get_preferences(working.as_ref(), source_file.file_name())
        .format_code_settings;
    let mut adder = tsrs_ls::autoimport::new_import_adder(
        &ctx,
        program,
        &mut checker,
        source_file,
        view,
        format,
        working.converters(),
        preferences,
    );
    for (symbol, valid) in symbols {
        adder.add_import_from_exported_symbol(symbol, valid);
    }
    if !adder.has_fixes() {
        return Ok(Value::Array(Vec::new()));
    }
    let edits = adder.edits();
    drop(adder);
    drop(checker);
    Ok(to_api_text_edits(source_file, &edits))
}

/// Go `toAPITextEdits` (+ `originalTextOffset`): LSP line/character → original-text offsets → UTF-16.
fn to_api_text_edits(
    source_file: P<tsrs_ast::SourceFile>,
    edits: &[tsrs_lsproto::TextEdit],
) -> Value {
    let original = source_file.original_text();
    let line_map = tsrs_ls::lsconv::compute_lsp_line_starts(original);
    let position_map = tsrs_ast::compute_position_map(original);
    let offset = |pos: &tsrs_lsproto::Position| -> Option<i64> {
        let line = pos.line as usize;
        let start = *line_map.line_starts.get(line)? as i64;
        let off = start + pos.character as i64;
        (off >= start && off <= original.len() as i64).then_some(off)
    };
    let mut out = Vec::with_capacity(edits.len());
    out = match edits.iter().try_fold(out, |mut out, edit| {
        let (Some(start), Some(end)) = (offset(&edit.range.start), offset(&edit.range.end)) else {
            return std::ops::ControlFlow::Break(Value::Null);
        };
        let mut o = obj();
        o.num(
            "pos",
            f64::from(
                position_map
                    .utf8_to_utf16(u32::try_from(start).expect("edit position exceeds u32")),
            ),
        );
        o.num(
            "end",
            f64::from(
                position_map.utf8_to_utf16(u32::try_from(end).expect("edit position exceeds u32")),
            ),
        );
        o.set("newText", Value::String(edit.new_text.clone()));
        out.push(o.build());

        std::ops::ControlFlow::Continue(out)
    }) {
        std::ops::ControlFlow::Continue(value) => value,
        std::ops::ControlFlow::Break(value) => return value,
    };
    Value::Array(out)
}
