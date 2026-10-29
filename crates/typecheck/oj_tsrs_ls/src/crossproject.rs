use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use indexmap::map::Entry;
use rustc_hash::FxHashMap;
use tsrs_compiler::Program;
use tsrs_core::collections::{OrderedMap, Set};
use tsrs_core::context::Context;
use tsrs_core::tspath::Path;
use tsrs_lsproto::{self as lsproto, HasLocation, HasLocations};

use crate::findallreferences::{
    NonLocalDefinition, SymbolAndEntriesData, SymbolEntryTransformOptions,
};
use crate::languageservice::LanguageService;

// crossproject.go:17
pub trait Project: Send + Sync {
    fn id(&self) -> String;
    fn get_program(&self) -> Option<&'static Program>;
    fn has_file(&self, file_name: &str) -> bool;
}

// The default project's item carries the default language service (`ls`); other items fetch theirs from the
// orchestrator.
// crossproject.go:23
struct ProjectAndTextDocumentPosition<'a> {
    project: Arc<dyn Project>,
    ls: Option<&'a LanguageService>,
    uri: lsproto::DocumentUri,
    position: lsproto::Position,
    symbol_data: Option<SymbolAndEntriesData>,
    for_original_location: bool,
}

// crossproject.go:32
#[derive(Default)]
struct Response<Resp> {
    complete: bool,
    result: Resp,
    for_original_location: bool,
}

// crossproject.go:38
pub trait CrossProjectOrchestrator: Send + Sync {
    fn get_default_project(&self) -> Arc<dyn Project>;
    fn get_all_projects_for_initial_request(&self) -> Vec<Arc<dyn Project>>;
    fn get_language_service_for_project_with_file(
        &self,
        ctx: &Context,
        project: &Arc<dyn Project>,
        uri: &lsproto::DocumentUri,
    ) -> Option<Arc<LanguageService>>;
    fn get_projects_for_file(
        &self,
        ctx: &Context,
        uri: &lsproto::DocumentUri,
    ) -> Result<Vec<Arc<dyn Project>>, lsproto::Error>;
    fn get_projects_loading_project_tree(
        &self,
        ctx: &Context,
        requested_project_trees: &Set<Path>,
    ) -> Box<dyn Iterator<Item = Arc<dyn Project>> + '_>;
}

impl LanguageService {
    // Go runs the per-project searches as goroutines of a `core.WorkGroup`; they run sequentially here, last-queued
    // first like tsrs_core's WorkGroup (the order only affects which search runs first: results are ordered by
    // getResultsIterator). Go's `iter.Seq[Resp]` of results is a Vec handed to `combine_results`.
    // crossproject.go:46
    pub(crate) fn handle_cross_project<Req: lsproto::HasTextDocumentPosition, Resp: Default>(
        &self,
        ctx: &Context,
        params: &Req,
        orchestrator: Option<&dyn CrossProjectOrchestrator>,
        symbol_and_entries_to_resp: fn(
            &LanguageService,
            &Context,
            &Req,
            &SymbolAndEntriesData,
            SymbolEntryTransformOptions,
        ) -> Result<Resp, lsproto::Error>,
        combine_results: fn(Vec<Resp>) -> Resp,
        is_rename: bool,
        implementations: bool,
        options: SymbolEntryTransformOptions,
        default_project_data: Option<&SymbolAndEntriesData>,
    ) -> Result<Resp, lsproto::Error> {
        fn can_search_project<Resp>(
            results: &OrderedMap<String, Response<Resp>>,
            project: &Arc<dyn Project>,
        ) -> bool {
            !results.contains_key(&project.id())
        }
        fn enqueue_item<'a, Resp: Default>(
            results: &mut OrderedMap<String, Response<Resp>>,
            queue: &mut Vec<ProjectAndTextDocumentPosition<'a>>,
            item: ProjectAndTextDocumentPosition<'a>,
        ) {
            let id = item.project.id();
            if let Entry::Vacant(entry) = results.entry(id) {
                entry.insert(Response::default());
                queue.push(item);
            }
        }

        let default_ls = self;

        // Single project
        let Some(orchestrator) = orchestrator else {
            let computed;
            let data = if let Some(data) = default_project_data {
                data
            } else {
                computed = default_ls
                    .provide_symbols_and_entries(
                        ctx,
                        params.text_document_uri(),
                        params.text_document_position(),
                        is_rename,
                        implementations,
                    )
                    .unwrap_or_default();
                &computed
            };
            return symbol_and_entries_to_resp(default_ls, ctx, params, data, options);
        };

        let default_project = orchestrator.get_default_project();
        let all_projects = orchestrator.get_all_projects_for_initial_request();
        let mut results: OrderedMap<String, Response<Resp>> = OrderedMap::default();
        let mut default_definition: Option<NonLocalDefinition> = None;
        let mut queue: Vec<ProjectAndTextDocumentPosition> = Vec::new();
        let mut err: Option<lsproto::Error> = None;
        let mut panics_occurred: Vec<String> = Vec::new();

        // Initial set of projects and locations in the queue, starting with default project
        let initial_item = ProjectAndTextDocumentPosition {
            project: Arc::clone(&default_project),
            ls: Some(default_ls),
            uri: params.text_document_uri().clone(),
            position: params.text_document_position(),
            symbol_data: default_project_data.cloned(),
            for_original_location: false,
        };
        enqueue_item(&mut results, &mut queue, initial_item);
        for project in &all_projects {
            if !Arc::ptr_eq(project, &default_project) {
                enqueue_item(
                    &mut results,
                    &mut queue,
                    ProjectAndTextDocumentPosition {
                        project: Arc::clone(project),
                        ls: None,
                        // TODO!! symlinks need to change the URI
                        uri: params.text_document_uri().clone(),
                        position: params.text_document_position(),
                        symbol_data: None,
                        for_original_location: false,
                    },
                );
            }
        }

        // Outer loop - to complete work if more is added after completing existing queue
        loop {
            // Process existing known projects first
            while let Some(item) = queue.pop() {
                if ctx.err().is_some() {
                    continue;
                }
                let outcome =
                    catch_unwind(AssertUnwindSafe(|| -> Option<Result<Resp, lsproto::Error>> {
                        // Process the item
                        let fetched: Arc<LanguageService>;
                        let ls: &LanguageService = if let Some(ls) = item.ls {
                            ls
                        } else {
                            // Get it now
                            fetched = orchestrator.get_language_service_for_project_with_file(
                                ctx,
                                &item.project,
                                &item.uri,
                            )?;
                            &fetched
                        };
                        let (data, ok) = match item.symbol_data {
                            Some(data) => (data, true),
                            None => match ls.provide_symbols_and_entries(
                                ctx,
                                &item.uri,
                                item.position,
                                is_rename,
                                implementations,
                            ) {
                                Some(data) => (data, true),
                                None => (SymbolAndEntriesData::default(), false),
                            },
                        };
                        if ctx.err().is_some() {
                            return None;
                        }
                        if ok {
                            for entry in &data.symbols_and_entries {
                                // Find the default definition that can be in another project
                                // Later we will use this load ancestor tree that references this location and expand search
                                if Arc::ptr_eq(&item.project, &default_project)
                                    && default_definition.is_none()
                                {
                                    default_definition = ls.get_non_local_definition(ctx, entry);
                                }
                                ls.for_each_original_definition_location(
                                    ctx,
                                    entry,
                                    &mut |uri, position| {
                                        // Get default configured project for this file
                                        let Ok(def_projects) =
                                            orchestrator.get_projects_for_file(ctx, &uri)
                                        else {
                                            return;
                                        };
                                        for def_project in def_projects {
                                            // Optimization: don't enqueue if will be discarded
                                            if can_search_project(&results, &def_project) {
                                                enqueue_item(
                                                    &mut results,
                                                    &mut queue,
                                                    ProjectAndTextDocumentPosition {
                                                        project: def_project,
                                                        ls: None,
                                                        uri: uri.clone(),
                                                        position,
                                                        symbol_data: None,
                                                        for_original_location: true,
                                                    },
                                                );
                                            }
                                        }
                                    },
                                );
                            }
                        }

                        Some(symbol_and_entries_to_resp(ls, ctx, params, &data, options))
                    }));
                match outcome {
                    Ok(Some(Ok(result))) => {
                        let response = results.get_mut(&item.project.id()).unwrap();
                        response.complete = true;
                        response.result = result;
                        response.for_original_location = item.for_original_location;
                    }
                    Ok(Some(Err(err_search))) => {
                        if err.is_none() {
                            err = Some(err_search);
                        }
                    }
                    Ok(None) => {}
                    Err(payload) => {
                        let r = payload
                            .downcast_ref::<String>()
                            .cloned()
                            .or_else(|| {
                                payload.downcast_ref::<&str>().map(std::string::ToString::to_string)
                            })
                            .unwrap_or_default();
                        let stack = std::backtrace::Backtrace::force_capture();
                        panics_occurred.push(format!("panic handling request: {r}\n{stack}"));
                    }
                }
            }
            assert!(
                panics_occurred.is_empty(),
                "Panics occurred during cross-project handling: {panics_occurred:?}"
            );
            if let Some(e) = ctx.err() {
                return Err(e.into());
            }
            if let Some(err) = err {
                return Err(err);
            }

            let mut has_more_work = false;
            if let Some(default_definition) = &default_definition {
                let mut requested_project_trees: Set<Path> = Set::new();
                requested_project_trees.m.extend(results.iter().filter_map(|(key, response)| {
                    if response.complete { Some(Path::new(key.clone())) } else { None }
                }));

                // Load more projects based on default definition found
                for loaded_project in
                    orchestrator.get_projects_loading_project_tree(ctx, &requested_project_trees)
                {
                    if let Some(e) = ctx.err() {
                        return Err(e.into());
                    }

                    // Can loop forever without this (enqueue here, dequeue above, repeat)
                    if !can_search_project(&results, &loaded_project)
                        || loaded_project.get_program().is_none()
                    {
                        continue;
                    }

                    // Enqueue the project and location for further processing
                    let mut target: Option<(lsproto::DocumentUri, lsproto::Position)> = None;
                    if loaded_project.has_file(&default_definition.position.uri.file_name()) {
                        target = Some((
                            default_definition.position.uri.clone(),
                            default_definition.position.pos,
                        ));
                    } else if let Some(source_pos) = default_definition
                        .get_source_position(default_ls)
                        .filter(|p| loaded_project.has_file(&p.uri.file_name()))
                    {
                        target = Some((source_pos.uri.clone(), source_pos.pos));
                    } else if let Some(generated_pos) = default_definition
                        .get_generated_position(default_ls)
                        .filter(|p| loaded_project.has_file(&p.uri.file_name()))
                    {
                        target = Some((generated_pos.uri.clone(), generated_pos.pos));
                    }
                    if let Some((uri, position)) = target {
                        enqueue_item(
                            &mut results,
                            &mut queue,
                            ProjectAndTextDocumentPosition {
                                project: loaded_project,
                                ls: None,
                                uri,
                                position,
                                symbol_data: None,
                                for_original_location: false,
                            },
                        );
                        has_more_work = true;
                    }
                }
            }
            if !has_more_work {
                break;
            }
        }

        // getResultsIterator (crossproject.go:184)
        let size = results.len();
        let mut ordered: Vec<Resp> = Vec::new();
        let mut seen_projects: Set<String> = Set::new();
        let default_id = default_project.id();
        if results.get(&default_id).is_some_and(|r| r.complete) {
            ordered.push(std::mem::take(&mut results.get_mut(&default_id).unwrap().result));
        }
        seen_projects.add(default_id);
        ordered.extend(all_projects.iter().filter_map(|project| {
            let id = project.id();
            if seen_projects.add_if_absent(id.clone()) {
                if let Some(response) = results.get_mut(&id) {
                    if response.complete {
                        Some(std::mem::take(&mut response.result))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        }));
        // Prefer the searches from locations for default definition
        ordered.extend(results.iter_mut().filter_map(|(key, response)| {
            if !response.for_original_location
                && seen_projects.add_if_absent(key.clone())
                && response.complete
            {
                Some(std::mem::take(&mut response.result))
            } else {
                None
            }
        }));
        // Then the searches from original locations
        ordered.extend(results.iter_mut().filter_map(|(key, response)| {
            if response.for_original_location
                && seen_projects.add_if_absent(key.clone())
                && response.complete
            {
                Some(std::mem::take(&mut response.result))
            } else {
                None
            }
        }));

        let resp = if size > 1 {
            combine_results(ordered)
        } else {
            // Single result, return that directly
            ordered.into_iter().next().unwrap_or_default()
        };
        Ok(resp)
    }
}

// crossproject.go:298
pub(crate) fn combine_location_array<T: HasLocation + Clone>(
    mut combined: Vec<T>,
    locations: &[T],
    seen: &mut Set<lsproto::Location>,
) -> Vec<T> {
    combined.extend(locations.iter().filter_map(|loc| {
        if seen.add_if_absent(loc.get_location()) { Some(loc.clone()) } else { None }
    }));
    combined
}

// crossproject.go:311
fn combine_response_locations<T: HasLocations>(results: &[T]) -> Vec<lsproto::Location> {
    let mut combined: Vec<lsproto::Location> = Vec::new();
    let mut seen_locations: Set<lsproto::Location> = Set::new();
    combined = results.iter().fold(combined, |mut combined, resp| {
        if let Some(locations) = resp.get_locations() {
            combined = combine_location_array(combined, locations, &mut seen_locations);
        }

        combined
    });
    combined
}

// crossproject.go:322
pub(crate) fn combine_references(
    results: &[lsproto::ReferencesResponse],
) -> lsproto::ReferencesResponse {
    lsproto::LocationsOrNull { locations: Some(combine_response_locations(results)) }
}

// crossproject.go:326
pub(crate) fn combine_vs_references(
    results: Vec<lsproto::VSReferencesResponse>,
) -> lsproto::VSReferencesResponse {
    let mut combined: Vec<lsproto::VSReferenceItem> = Vec::new();
    // Re-number IDs across projects to maintain unique IDs and correct definition references
    let mut next_id: usize = 0;
    for resp in results {
        let Some(items) = resp.vs_reference_items else {
            continue;
        };
        // Map old IDs to new IDs for this batch
        let mut id_map: FxHashMap<i32, usize> = FxHashMap::default();
        combined.extend(items.into_iter().map(|item| {
            let old_id = item.vs_id;
            let new_id = i32::try_from(next_id).expect("reference ID exceeds protocol i32");
            id_map.insert(old_id, next_id);
            next_id += 1;

            let mut new_item = item.clone();
            new_item.vs_id = new_id;
            if let Some(definition_id) = item.vs_definition_id {
                let new_def_id = i32::try_from(id_map.get(&definition_id).copied().unwrap_or(0))
                    .expect("reference ID exceeds protocol i32");
                new_item.vs_definition_id = Some(new_def_id);
            }
            new_item
        }));
    }
    lsproto::VSReferenceItemsOrNull { vs_reference_items: Some(combined) }
}

// Go's `return ...combineResponseLocations(results)` restarts the iteration over all results; so does the port.
// crossproject.go:354
pub(crate) fn combine_implementations(
    results: &[lsproto::ImplementationResponse],
) -> lsproto::ImplementationResponse {
    let mut combined: Vec<lsproto::LocationLink> = Vec::new();
    let mut seen_locations: Set<lsproto::Location> = Set::new();
    for resp in results {
        if let Some(definition_links) = &resp.definition_links {
            combined = combine_location_array(combined, definition_links, &mut seen_locations);
        } else if resp.locations.is_some() {
            return lsproto::LocationOrLocationsOrDefinitionLinksOrNull {
                locations: Some(combine_response_locations(results)),
                ..Default::default()
            };
        }
    }
    lsproto::LocationOrLocationsOrDefinitionLinksOrNull {
        definition_links: Some(combined),
        ..Default::default()
    }
}

// Go builds `map`s (random iteration order) keyed by document URI; the port keeps first-seen order.
// crossproject.go:367
pub(crate) fn combine_rename_response(
    results: Vec<lsproto::RenameResponse>,
) -> lsproto::RenameResponse {
    let mut combined: OrderedMap<lsproto::DocumentUri, Vec<lsproto::TextEdit>> =
        OrderedMap::default();
    let mut seen_changes: FxHashMap<lsproto::DocumentUri, Set<lsproto::Range>> =
        FxHashMap::default();
    let mut document_changes: Vec<lsproto::TextDocumentEditOrCreateFileOrRenameFileOrDeleteFile> =
        Vec::new();
    let mut seen_renames: Set<[lsproto::DocumentUri; 2]> = Set::new();

    for resp in results {
        let Some(workspace_edit) = resp.workspace_edit else {
            continue;
        };
        if let Some(changes) = workspace_edit.document_changes {
            document_changes.extend(changes.into_iter().filter_map(|change| {
                match &change.rename_file {
                    Some(rename_file) => {
                        let key = [rename_file.old_uri.clone(), rename_file.new_uri.clone()];
                        if seen_renames.add_if_absent(key) { Some(change) } else { None }
                    }
                    None => Some(change),
                }
            }));
        }
        if let Some(changes) = workspace_edit.changes {
            for (doc, changes) in changes {
                let seen_set = seen_changes.entry(doc.clone()).or_default();
                let changes_for_doc = combined.entry(doc).or_default();
                changes_for_doc.extend(changes.into_iter().filter_map(|change| {
                    if seen_set.has(&change.range) {
                        None
                    } else {
                        seen_set.add(change.range);
                        Some(change)
                    }
                }));
            }
        }
    }
    if !document_changes.is_empty() || !combined.is_empty() {
        let mut workspace_edit = lsproto::WorkspaceEdit::default();
        if !document_changes.is_empty() {
            workspace_edit.document_changes = Some(document_changes);
        }
        if !combined.is_empty() {
            workspace_edit.changes = Some(combined);
        }
        return lsproto::WorkspaceEditOrNull { workspace_edit: Some(workspace_edit) };
    }
    lsproto::WorkspaceEditOrNull::default()
}

// crossproject.go:423
pub(crate) fn combine_incoming_calls(
    results: Vec<lsproto::CallHierarchyIncomingCallsResponse>,
) -> lsproto::CallHierarchyIncomingCallsResponse {
    let mut combined: Vec<lsproto::CallHierarchyIncomingCall> = Vec::new();
    let mut seen_calls: Set<lsproto::Location> = Set::new();
    combined.extend(
        results
            .into_iter()
            .filter_map(|resp| resp.call_hierarchy_incoming_calls)
            .flatten()
            .filter(|call| seen_calls.add_if_absent(call.from.get_location())),
    );
    lsproto::CallHierarchyIncomingCallsOrNull { call_hierarchy_incoming_calls: Some(combined) }
}
