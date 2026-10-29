use std::cell::RefCell;

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::{
    self as ast, FileReference, JSDeclarationKind, Kind, ModifierFlags, Node, SourceFile,
    SourceFileParseOptions,
};
use tsrs_checker::{self as checker, Checker};
use tsrs_compiler::{CompilerHost, Program};
use tsrs_core::context::Context;
use tsrs_core::{
    CompilerOptions, ModuleKind, P, ResolutionMode, TextPos, TextRange, Tristate, tspath,
};
use tsrs_lsproto as lsproto;
use tsrs_module::packagejson::InfoCacheEntryExt as _;
use tsrs_module::{self as module, ResolutionHost};
use tsrs_vfs::FS;

use crate::astnav;
use crate::definition::{
    combine_definition_responses, get_declarations_from_location, try_get_signature_declaration,
};
use crate::findallreferences::get_range_of_node;
use crate::languageservice::LanguageService;
use crate::spanmap::Feature;
use crate::utilities::{get_container_node, get_reference_at_position};

impl LanguageService {
    // sourcedefinition.go:25
    pub fn provide_source_definition(
        &self,
        ctx: &Context,
        document_uri: &lsproto::DocumentUri,
        position: lsproto::Position,
    ) -> Result<lsproto::DefinitionResponse, lsproto::Error> {
        let (program, file) = self.get_program_and_file(document_uri);
        let positions =
            self.converters.from_lsp_position_for_source_file(file, position, Feature::Definition);
        let mut results: Vec<lsproto::DefinitionResponse> = Vec::with_capacity(positions.len());
        for mapped in positions {
            if mapped.fidelity.is_single_segment() {
                let result = self.provide_source_definition_at_position(
                    ctx,
                    program,
                    mapped.script,
                    mapped.position,
                )?;
                results.push(result);
            }
        }
        Ok(combine_definition_responses(
            results,
            lsproto::get_client_capabilities(ctx).text_document.definition.link_support,
        ))
    }

    // sourcedefinition.go:45
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Language service operations share the fallible response signature used by LSP and cross-project dispatch."
    )]
    fn provide_source_definition_at_position(
        &self,
        ctx: &Context,
        program: &'static Program,
        file: P<SourceFile>,
        text_pos: u32,
    ) -> Result<lsproto::DefinitionResponse, lsproto::Error> {
        let caps = lsproto::get_client_capabilities(ctx);
        let client_supports_link = caps.text_document.definition.link_support;

        let pos = text_pos;
        let resolver = self.new_source_def_resolver(program, file.file_name());
        let node = astnav::get_touching_property_name(file, pos);

        if node.kind() == Kind::SourceFile {
            // Triple-slash directives are comments, not AST nodes, so
            // GetTouchingPropertyName returns the SourceFile node.
            let (declarations, ref_) = resolver.resolve_triple_slash_reference(file, pos, program);
            if !declarations.is_empty() {
                let ref_ = ref_.unwrap();
                let (origin_selection_range, _) = self.create_lsp_range_from_bounds(
                    ref_.text_range
                        .pos()
                        .expect_source("triple-slash references have source positions"),
                    ref_.text_range
                        .end()
                        .expect_source("triple-slash references have source positions"),
                    file,
                );
                return Ok(self.create_definition_locations(
                    origin_selection_range,
                    client_supports_link,
                    &declarations,
                    None, /*reference*/
                    Feature::Definition,
                ));
            }
            return Ok(lsproto::LocationOrLocationsOrDefinitionLinksOrNull::default());
        }

        let (origin_selection_range, _) = self.create_lsp_range_from_node(node, file);

        let containing_module_specifier = find_containing_module_specifier(node);
        if containing_module_specifier
            .is_some_and(|specifier| specifier.parent().is_some_and(ast::is_source_phase_import))
        {
            return Ok(self.provide_definition_at_position(
                ctx,
                program,
                file,
                text_pos,
                client_supports_link,
            ));
        }

        // If the cursor is directly on a module specifier string, resolve to the
        // implementation file's entry point.
        if Some(node) == containing_module_specifier {
            let containing_module_specifier = containing_module_specifier.unwrap();
            let specifier_mode =
                program.get_mode_for_usage_location(file, containing_module_specifier);
            let implementation_file =
                resolver.resolve_implementation(containing_module_specifier.text(), specifier_mode);
            if !implementation_file.is_empty()
                && let Some(source_file) = resolver.get_or_parse_source_file(&implementation_file)
            {
                return Ok(self.create_definition_locations(
                    origin_selection_range,
                    client_supports_link,
                    &get_source_definition_entry_declarations(source_file),
                    None,
                    Feature::Definition,
                ));
            }
            return Ok(self.provide_definition_at_position(
                ctx,
                program,
                file,
                text_pos,
                client_supports_link,
            ));
        }

        // Phase 1: Syntactic fast path — when the cursor is inside an
        // import/require/export, forward-resolve the module specifier to an
        // implementation file and search it directly. This avoids acquiring
        // the type checker entirely when the fast path succeeds.
        let mut resolved_impl_file = String::new();
        if let Some(containing_module_specifier) = containing_module_specifier {
            let specifier_mode =
                program.get_mode_for_usage_location(file, containing_module_specifier);
            resolved_impl_file =
                resolver.resolve_implementation(containing_module_specifier.text(), specifier_mode);
        }

        if !resolved_impl_file.is_empty() {
            let names = get_candidate_source_declaration_names(Some(node), None);
            let module_results =
                resolver.search_implementation_file(Some(node), &resolved_impl_file, &names);
            if !module_results.is_empty()
                && (!ast::is_part_of_type_node(node)
                    && !ast::is_part_of_type_only_import_or_export_declaration(node)
                    || has_concrete_source_declarations(&module_results))
            {
                return Ok(self.create_definition_locations(
                    origin_selection_range,
                    client_supports_link,
                    &unique_declaration_nodes(&module_results),
                    None,
                    Feature::Definition,
                ));
            }
        }

        // Phase 2: Type checker path — acquire the checker for the original file
        // and use its declarations and module specifier to map to source
        // implementations. This is the only point where the checker is used;
        // after this, only the NoDts module resolver and file parsing are needed.
        let (checker_declarations, module_specifier) =
            get_source_def_checker_info(ctx, program, file, node);

        // Phase 3: Map checker results to source definitions.
        let declarations = resolver.resolve_from_checker_info(
            node,
            &resolved_impl_file,
            &checker_declarations,
            &module_specifier,
        );
        if declarations.is_empty() {
            // If we resolved an implementation file from an import/export but
            // couldn't find specific declarations, fall back to the file entry
            // point rather than the standard definition provider — unless the
            // checker found declarations that are all type-only (e.g. interfaces),
            // in which case the .d.ts definition is more appropriate.
            if containing_module_specifier.is_some()
                && !resolved_impl_file.is_empty()
                && !has_concrete_source_declarations(&checker_declarations)
                && let Some(source_file) = resolver.get_or_parse_source_file(&resolved_impl_file)
            {
                return Ok(self.create_definition_locations(
                    origin_selection_range,
                    client_supports_link,
                    &get_source_definition_entry_declarations(source_file),
                    None,
                    Feature::Definition,
                ));
            }
            return Ok(self.provide_definition_at_position(
                ctx,
                program,
                file,
                text_pos,
                client_supports_link,
            ));
        }
        Ok(self.create_definition_locations(
            origin_selection_range,
            client_supports_link,
            &declarations,
            None, /*reference*/
            Feature::Definition,
        ))
    }
}

// Go passes `program.Host()` (a compiler.CompilerHost, which has the FS/GetCurrentDirectory methods of
// module.ResolutionHost) to module.NewResolver; Rust needs the explicit adapter.
struct CompilerHostResolutionHost(&'static dyn CompilerHost);

impl ResolutionHost for CompilerHostResolutionHost {
    fn fs(&self) -> &dyn FS {
        self.0.fs()
    }
    fn get_current_directory(&self) -> &str {
        self.0.get_current_directory()
    }
}

// sourceDefResolver resolves source definitions by mapping .d.ts declarations
// to their implementation files (.js/.ts). It uses the NoDts module resolver
// and file parsing for resolution, but never acquires the type checker or
// the original program; all checker-dependent work is done before results
// are passed in.
// sourcedefinition.go:132
struct SourceDefResolver<'a> {
    ls: &'a LanguageService,
    fs: &'static dyn FS,
    options: P<CompilerOptions>,
    program: &'static Program, // Go `getSourceFile func(string) *ast.SourceFile` (program.GetSourceFile)
    resolve_from: String,
    resolver: module::DefaultResolver<'static>,
    parsed_files: RefCell<FxHashMap<String, Option<P<SourceFile>>>>,
}

impl LanguageService {
    // sourcedefinition.go:142
    fn new_source_def_resolver(
        &self,
        program: &'static Program,
        resolve_from: &str,
    ) -> SourceDefResolver<'_> {
        let options = program.options();
        let mut no_dts_options: CompilerOptions = (*options).clone();
        no_dts_options.no_dts_resolution = Tristate::True;
        let host: &'static dyn CompilerHost = &**program.host();
        let resolution_host: &'static CompilerHostResolutionHost =
            tsrs_core::alloc(CompilerHostResolutionHost(host));
        SourceDefResolver {
            ls: self,
            fs: host.fs(),
            options,
            program,
            resolve_from: resolve_from.to_string(),
            resolver: module::new_resolver(module::ResolverOptions {
                host: resolution_host,
                compiler_options: P::new(no_dts_options),
                typings_location: program.get_global_typings_cache_location().to_string(),
                project_name: String::new(),
                extra_extensions: program.command_line().content_mapper_extensions(),
                package_json_cache: None,
            }),
            parsed_files: RefCell::new(FxHashMap::default()),
        }
    }
}

impl SourceDefResolver<'_> {
    // resolveFromCheckerInfo maps type-checker declarations to source
    // implementations. It uses only the NoDts module resolver and file parsing;
    // the type checker and original request file are not needed.
    // sourcedefinition.go:167
    fn resolve_from_checker_info(
        &self,
        node: P<Node>,
        resolved_impl_file: &str,
        checker_declarations: &[P<Node>],
        module_specifier: &str,
    ) -> Vec<P<Node>> {
        let mut resolved_impl_file = resolved_impl_file.to_string();
        // If we don't yet have a forward-resolved implementation file, try to
        // recover a module specifier from the checker (e.g. from the import that
        // brought the symbol into scope, or from the root of an access expression).
        if resolved_impl_file.is_empty() && !module_specifier.is_empty() {
            resolved_impl_file = self.resolve_implementation(
                module_specifier,
                self.infer_implied_node_format(&self.resolve_from),
            );
        }

        // For property access where the checker found no declarations (e.g.
        // mapped types), search the implementation file for the property name.
        if checker_declarations.is_empty() && !resolved_impl_file.is_empty() {
            let names = get_candidate_source_declaration_names(Some(node), None);
            let results = self.search_implementation_file(Some(node), &resolved_impl_file, &names);
            // Go checks `results != nil`; searchImplementationFile returns nil exactly when nothing matched.
            if !results.is_empty() {
                return unique_declaration_nodes(&results);
            }
        }

        let mut declarations: Vec<P<Node>> = Vec::new();
        declarations.extend(checker_declarations.iter().flat_map(|&declaration| {
            self.map_declaration_to_source(node, declaration, &resolved_impl_file)
        }));
        let declarations = unique_declaration_nodes(&declarations);
        if has_concrete_source_declarations(&declarations) {
            return declarations;
        }
        Vec::new()
    }
}

// getSourceDefCheckerInfo acquires the type checker for the given file and
// returns the definition declarations for node along with the module specifier
// of the import that brought the symbol into scope (empty if not applicable).
// sourcedefinition.go:203
fn get_source_def_checker_info(
    ctx: &Context,
    program: &'static Program,
    file: P<SourceFile>,
    node: P<Node>,
) -> (Vec<P<Node>>, String) {
    let mut c = program.get_type_checker_for_file(ctx, file);
    let c: &mut Checker = &mut c;

    let mut declarations = get_declarations_from_location(c, node);
    let is_property_name =
        node.parent().is_some_and(|p| ast::is_access_expression(p) && p.name() == Some(node));
    if declarations.is_empty()
        && is_property_name
        && let Some(left) = node.parent().unwrap().expression()
    {
        let t = c.get_type_at_location(left);
        if let Some(prop) = c.get_property_of_type_exported(t, node.text()) {
            declarations = prop.declarations().to_vec();
        }
    }
    if let Some(called_declaration) = try_get_signature_declaration(c, node) {
        let mut non_function_declarations: Vec<P<Node>> =
            declarations.into_iter().filter(|&node| !ast::is_function_like(Some(node))).collect();
        non_function_declarations.push(called_declaration);
        declarations = non_function_declarations;
    }

    // Extract module specifier from the import that brought this symbol into
    // scope. For property access (obj.prop), walk up the access chain to the
    // root expression's symbol.
    let mut module_specifier = String::new();
    let mut resolve_node = node;
    if is_property_name {
        let mut expr = node.parent().unwrap().expression();
        while let Some(e) = expr {
            if !ast::is_access_expression(e) {
                break;
            }
            expr = e.expression();
        }
        if let Some(expr) = expr {
            resolve_node = expr;
        }
    }
    if let Some(sym) = c.get_symbol_at_location_exported(resolve_node) {
        for &d in sym.declarations() {
            if !ast::is_import_specifier(d)
                && !ast::is_import_clause(d)
                && !ast::is_namespace_import(d)
                && !ast::is_import_equals_declaration(d)
            {
                continue;
            }
            if let Some(spec) = checker::try_get_module_specifier_from_declaration(d) {
                module_specifier = spec.text().to_string();
                break;
            }
        }
    }

    (declarations, module_specifier)
}

impl SourceDefResolver<'_> {
    // resolveTripleSlashReference handles /// <reference path/types="..."/> directives.
    // For path references to .js files, it returns the entry declarations directly.
    // For path references to .d.ts files or type references, it uses the NoDts
    // resolver to find the corresponding implementation file.
    // sourcedefinition.go:259
    fn resolve_triple_slash_reference(
        &self,
        file: P<SourceFile>,
        pos: u32,
        program: &'static Program,
    ) -> (Vec<P<Node>>, Option<P<FileReference>>) {
        let Some(ref_) = get_reference_at_position(file, pos, program) else {
            return (Vec::new(), None);
        };
        let Some(ref_file) = ref_.file else {
            return (Vec::new(), None);
        };

        // If the referenced file is already an implementation file, return it directly.
        if !ref_file.is_declaration_file() {
            return (get_source_definition_entry_declarations(ref_file), ref_.reference);
        }

        // The referenced file is a .d.ts. Try to find the implementation file
        // using the NoDts module resolver via findImplementationFileFromDtsFileName.
        let dts_file_name = ref_file.file_name();
        let preferred_mode = self.infer_implied_node_format(dts_file_name);
        let implementation_file =
            self.find_implementation_file_from_dts_file_name(dts_file_name, preferred_mode);
        if implementation_file.is_empty() {
            return (Vec::new(), None);
        }

        let Some(source_file) = self.get_or_parse_source_file(&implementation_file) else {
            return (Vec::new(), None);
        };
        (get_source_definition_entry_declarations(source_file), ref_.reference)
    }

    // searchImplementationFile searches an implementation file for declarations
    // matching the given names. Returns nil when no declarations matched; callers
    // fall through to the checker path or to the standard definition provider.
    // sourcedefinition.go:289
    fn search_implementation_file(
        &self,
        original_node: Option<P<Node>>,
        implementation_file: &str,
        names: &[String],
    ) -> Vec<P<Node>> {
        if implementation_file.is_empty() {
            return Vec::new();
        }
        let Some(source_file) = self.get_or_parse_source_file(implementation_file) else {
            return Vec::new();
        };
        if is_default_import_name(original_node) {
            // For default imports, only search for "default" declarations to avoid
            // matching unrelated declarations with the same identifier name.
            let default_declarations = self.find_declarations_in_file(
                implementation_file,
                &["default".to_string()],
                &mut FxHashSet::default(),
            );
            if !default_declarations.is_empty() {
                return filter_preferred_source_declarations(original_node, default_declarations);
            }
            return get_source_definition_entry_declarations(source_file);
        }
        let declarations =
            self.find_declarations_in_file(implementation_file, names, &mut FxHashSet::default());
        if !declarations.is_empty() {
            return filter_preferred_source_declarations(original_node, declarations);
        }
        Vec::new()
    }
}

// sourcedefinition.go:317
fn is_default_import_name(node: Option<P<Node>>) -> bool {
    let Some(node) = node else { return false };
    let Some(parent) = node.parent() else {
        return false;
    };
    if !ast::is_import_clause(parent) || parent.name() != Some(node) {
        return false;
    }
    let Some(grand_parent) = parent.parent() else {
        return false;
    };
    ast::is_default_import(grand_parent)
}

// sourcedefinition.go:324
fn get_source_definition_entry_node(source_file: P<SourceFile>) -> P<Node> {
    let statements = source_file.statements.nodes();
    if !statements.is_empty() {
        return statements[0];
    }
    source_file.as_node()
}

// sourcedefinition.go:331
fn get_source_definition_entry_declarations(source_file: P<SourceFile>) -> Vec<P<Node>> {
    vec![get_source_definition_entry_node(source_file)]
}

// findallreferences.go:496 (getFileAndStartPosFromDeclaration)
fn get_file_and_start_pos_from_declaration(declaration: P<Node>) -> (P<SourceFile>, TextPos) {
    let file = ast::get_source_file_of_node(declaration).unwrap();
    // A name without a position (the `this` parameter of a JSDoc `@this` tag) starts at -1 in Go; use the
    // declaration's position instead (definition.rs create_definition_locations).
    let name = ast::get_name_of_declaration(declaration)
        .filter(|name| !name.pos().is_synthetic())
        .unwrap_or(declaration);
    let text_range = get_range_of_node(name, Some(file), None /*endNode*/);
    (file, text_range.pos())
}

impl SourceDefResolver<'_> {
    // sourcedefinition.go:335
    fn map_declaration_to_source(
        &self,
        original_node: P<Node>,
        declaration: P<Node>,
        resolved_impl_file: &str,
    ) -> Vec<P<Node>> {
        let (file, start_pos) = get_file_and_start_pos_from_declaration(declaration);
        let file_name = file.file_name();

        if let Some(mapped) = self.ls.try_get_source_position(file_name, start_pos)
            && let Some(source_file) = self.get_or_parse_source_file(&mapped.file_name)
        {
            return vec![find_closest_declaration_node(source_file, mapped.pos)];
        }

        if !tspath::is_declaration_file_name(file_name) {
            return vec![declaration];
        }

        let mut implementation_file = resolved_impl_file.to_string();
        if implementation_file.is_empty() {
            // Reverse-resolve .d.ts path to implementation file. This path is only
            // reached for declarations with no associated module specifier (e.g.
            // globals, ambient declarations, or when forward resolution failed).
            let dts_file_name =
                ast::get_source_file_of_node(declaration).unwrap().file_name().to_string();
            let preferred_mode = self.infer_implied_node_format(&dts_file_name);
            implementation_file =
                self.find_implementation_file_from_dts_file_name(&dts_file_name, preferred_mode);
        }

        self.search_implementation_file(
            Some(original_node),
            &implementation_file,
            &get_candidate_source_declaration_names(Some(original_node), Some(declaration)),
        )
    }

    // sourcedefinition.go:366
    fn find_implementation_file_from_dts_file_name(
        &self,
        dts_file_name: &str,
        preferred_mode: ResolutionMode,
    ) -> String {
        let js_ext = module::try_get_js_extension_for_file(dts_file_name, &self.options);
        if !js_ext.is_empty() {
            let candidate = tspath::change_extension(dts_file_name, js_ext);
            if self.fs.file_exists(&candidate) {
                return candidate;
            }
        }

        let Some(parts) = tsrs_modulespecifiers::get_node_module_path_parts(dts_file_name) else {
            return String::new();
        };

        // Ensure the file only contains one /node_modules/ segment. If there's more
        // than one, the package name extraction may be incorrect, so bail out.
        if dts_file_name.rfind("/node_modules/") != Some(parts.top_level_node_modules_index) {
            return String::new();
        }

        let Some(package_root_index) = parts.package_root_index else {
            return String::new();
        };
        let package_name_path_part =
            &dts_file_name[parts.top_level_package_name_index + 1..package_root_index];
        let package_name = module::get_package_name_from_types_package_name(
            &module::unmangle_scoped_package_name(package_name_path_part),
        );
        if package_name.is_empty() {
            return String::new();
        }

        let path_to_file_in_package = &dts_file_name[package_root_index + 1..];

        // Try resolving as a package subpath first (e.g. "pkg/dist/utils"), then
        // fall back to the bare package name (e.g. "pkg"). This covers both main
        // entrypoints and deep imports without needing to inspect package.json
        // entrypoints.
        if !path_to_file_in_package.is_empty() {
            let specifier = format!(
                "{}/{}",
                package_name,
                tspath::remove_file_extension(path_to_file_in_package)
            );
            let implementation_file = self.resolve_implementation(&specifier, preferred_mode);
            if !implementation_file.is_empty() {
                return implementation_file;
            }
        }
        self.resolve_implementation(&package_name, preferred_mode)
    }

    // sourcedefinition.go:409
    fn resolve_implementation(&self, module_name: &str, preferred_mode: ResolutionMode) -> String {
        self.resolve_implementation_from(module_name, &self.resolve_from, preferred_mode)
    }

    // sourcedefinition.go:416
    fn resolve_implementation_from(
        &self,
        module_name: &str,
        resolve_from_file: &str,
        preferred_mode: ResolutionMode,
    ) -> String {
        let mut modes = vec![preferred_mode];
        if preferred_mode != ModuleKind::ES_NEXT {
            modes.push(ModuleKind::ES_NEXT);
        }
        if preferred_mode != ModuleKind::COMMON_JS {
            modes.push(ModuleKind::COMMON_JS);
        }

        if let Some(result) = modes.into_iter().find_map(|mode| {
            if let Ok((resolved, _)) =
                self.resolver.resolve_module_name(module_name, resolve_from_file, mode, None)
                && resolved.is_resolved()
                && !tspath::is_declaration_file_name(resolved.resolved_file_name)
            {
                return Some(resolved.resolved_file_name.to_string());
            }

            None
        }) {
            return result;
        }
        String::new()
    }

    // sourcedefinition.go:438
    fn get_or_parse_source_file(&self, file_name: &str) -> Option<P<SourceFile>> {
        if let Some(source_file) = self.program.get_source_file(file_name) {
            return Some(source_file);
        }
        if let Some(&source_file) = self.parsed_files.borrow().get(file_name) {
            return source_file;
        }
        let mut source_file: Option<P<SourceFile>> = None;
        if let Some(text) = self.ls.read_file(file_name) {
            let sf = tsrs_parser::parse_source_file_owned(
                SourceFileParseOptions {
                    file_name: file_name.to_string(),
                    path: self.ls.to_path(file_name),
                    ..Default::default()
                },
                text,
                // A declaration map's `sources` entries are arbitrary strings, so the
                // file name here may not have a recognized extension.
                tsrs_core::ensure_script_kind_from_file_name(file_name),
            );
            tsrs_binder::bind_source_file(sf);
            source_file = Some(sf);
        }
        self.parsed_files.borrow_mut().insert(file_name.to_string(), source_file);
        source_file
    }

    // inferImpliedNodeFormat determines the module format for a source file that may not be
    // in the program, using the file extension and nearest package.json "type" field.
    // sourcedefinition.go:465
    fn infer_implied_node_format(&self, file_name: &str) -> ResolutionMode {
        let mut package_json_type = String::new();
        let scope =
            self.resolver.get_package_scope_for_path(&tspath::get_directory_path(file_name));
        if scope.exists()
            && let Some(value) = scope.unwrap().contents.unwrap().fields.type_.get_value()
        {
            package_json_type.clone_from(value);
        }
        ast::get_implied_node_format_for_file(file_name, &package_json_type)
    }
}

// sourcedefinition.go:475
fn find_containing_module_specifier(node: P<Node>) -> Option<P<Node>> {
    let current = Some(node);
    if let Some(result) = std::iter::successors(current, |cur| cur.parent()).find_map(|cur| {
        if (ast::is_any_import_or_re_export(cur)
            || ast::is_require_call(cur, true /*requireStringLiteralLikeArgument*/)
            || ast::is_import_call(cur))
            && let Some(module_specifier) = ast::get_external_module_name(cur)
            && ast::is_string_literal_like(module_specifier)
        {
            return Some(Some(module_specifier));
        }

        None
    }) {
        return result;
    }
    None
}

impl SourceDefResolver<'_> {
    // sourcedefinition.go:486
    fn find_declarations_in_file(
        &self,
        file_name: &str,
        names: &[String],
        seen: &mut FxHashSet<String>,
    ) -> Vec<P<Node>> {
        if file_name.is_empty() || names.is_empty() {
            return Vec::new();
        }
        if !seen.insert(file_name.to_string()) {
            return Vec::new();
        }

        let Some(source_file) = self.get_or_parse_source_file(file_name) else {
            return Vec::new();
        };

        let declarations = find_declaration_nodes_by_name(source_file, names);
        if !declarations.is_empty() && has_concrete_source_declarations(&declarations) {
            return declarations;
        }

        let mut forwarded: Vec<P<Node>> = Vec::new();
        forwarded.extend(
            self.get_forwarded_implementation_files(source_file).into_iter().flat_map(
                |forwarded_file| self.find_declarations_in_file(&forwarded_file, names, seen),
            ),
        );
        if !forwarded.is_empty() {
            if has_concrete_source_declarations(&forwarded) {
                return unique_declaration_nodes(&forwarded);
            }
            let mut all = declarations;
            all.extend(forwarded);
            return unique_declaration_nodes(&all);
        }
        declarations
    }

    // sourcedefinition.go:521
    fn get_forwarded_implementation_files(&self, source_file: P<SourceFile>) -> Vec<String> {
        let preferred_mode = self.infer_implied_node_format(source_file.file_name());

        let mut files: Vec<String> = Vec::new();
        files.extend(source_file.imports().iter().filter_map(|&imp| {
            let module_name = imp.text();
            let implementation_file = self.resolve_implementation_from(
                module_name,
                source_file.file_name(),
                preferred_mode,
            );
            if implementation_file.is_empty() { None } else { Some(implementation_file) }
        }));
        deduplicate(files)
    }
}

// Go core.Deduplicate (first occurrence wins, order kept).
fn deduplicate<T: PartialEq>(slice: Vec<T>) -> Vec<T> {
    let mut result: Vec<T> = Vec::with_capacity(slice.len());
    for value in slice {
        if !result.contains(&value) {
            result.push(value);
        }
    }
    result
}

// sourcedefinition.go:534
fn get_candidate_source_declaration_names(
    original_node: Option<P<Node>>,
    declaration: Option<P<Node>>,
) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if let Some(declaration) = declaration {
        if let Some(name) = ast::get_name_of_declaration(declaration) {
            let text = ast::get_text_of_property_name(name);
            if !text.is_empty() {
                names.push(text);
            }
        }
        if declaration.kind() == Kind::ExportAssignment {
            names.push("default".to_string());
        }
        if (ast::is_function_declaration(declaration) || ast::is_class_declaration(declaration))
            && declaration.modifier_flags() & ModifierFlags::ExportDefault
                == ModifierFlags::ExportDefault
        {
            names.push("default".to_string());
        }
        if (ast::is_import_specifier(declaration) || ast::is_export_specifier(declaration))
            && let Some(prop_name) = declaration.property_name()
        {
            names.push(prop_name.text().to_string());
        }
    }
    if let Some(original_node) = original_node {
        if ast::is_identifier(original_node) || ast::is_private_identifier(original_node) {
            names.push(original_node.text().to_string());
        }
        if is_default_import_name(Some(original_node)) {
            names.push("default".to_string());
        }
        if let Some(parent) = original_node.parent()
            && (ast::is_import_specifier(parent) || ast::is_export_specifier(parent))
            && let Some(prop_name) = parent.property_name()
        {
            names.push(prop_name.text().to_string());
        }
    }
    names
}

// sourcedefinition.go:572
fn find_declaration_nodes_by_name(source_file: P<SourceFile>, names: &[String]) -> Vec<P<Node>> {
    struct Candidate {
        node: P<Node>,
        depth: usize,
    }

    fn visit(
        node: P<Node>,
        wanted: &FxHashSet<String>,
        want_default: bool,
        candidates: &mut Vec<Candidate>,
        min_depth: &mut Option<usize>,
    ) -> bool {
        let mut matched = false;
        if let Some(name) = ast::get_name_of_declaration(node) {
            let text = ast::get_text_of_property_name(name);
            if !text.is_empty() && wanted.contains(&text) {
                matched = true;
            }
        }
        if want_default && node.kind() == Kind::ExportAssignment {
            matched = true;
        }
        if want_default
            && (ast::is_function_declaration(node) || ast::is_class_declaration(node))
            && node.modifier_flags() & ModifierFlags::ExportDefault == ModifierFlags::ExportDefault
        {
            matched = true;
        }
        if matched {
            let depth = get_container_depth(node);
            candidates.push(Candidate { node, depth });
            if min_depth.is_none_or(|minimum| depth < minimum) {
                *min_depth = Some(depth);
            }
        }
        node.for_each_child(&mut |child| visit(child, wanted, want_default, candidates, min_depth))
    }

    let names = deduplicate(names.iter().filter(|name| !name.is_empty()).cloned().collect());
    if names.is_empty() {
        return Vec::new();
    }

    let mut wanted: FxHashSet<String> = FxHashSet::default();
    let mut want_default = false;
    for name in names {
        if name == "default" {
            want_default = true;
            continue;
        }
        wanted.insert(name);
    }

    let mut candidates: Vec<Candidate> = Vec::new();
    let mut min_depth: Option<usize> = None;

    source_file.as_node().for_each_child(&mut |child| {
        visit(child, &wanted, want_default, &mut candidates, &mut min_depth)
    });

    // Only keep declarations at the shallowest depth, like getTopMostDeclarationNamesInFile.
    let declarations: Vec<P<Node>> =
        candidates.iter().filter(|c| Some(c.depth) == min_depth).map(|c| c.node).collect();
    unique_declaration_nodes(&declarations)
}

// getContainerDepth counts the number of container nodes above a declaration,
// matching the behavior of getDepth in getTopMostDeclarationNamesInFile.
// sourcedefinition.go:634
fn get_container_depth(node: P<Node>) -> usize {
    let mut depth = 0;
    let mut current = Some(node);
    while let Some(cur) = current {
        current = get_container_node(cur);
        depth += 1;
    }
    depth
}

// sourcedefinition.go:644
fn filter_preferred_source_declarations(
    original_node: Option<P<Node>>,
    declarations: Vec<P<Node>>,
) -> Vec<P<Node>> {
    let Some(original_node) = original_node else {
        return declarations;
    };
    if declarations.len() <= 1 {
        return declarations;
    }
    let preferred = get_property_like_source_declarations(original_node, &declarations);
    if !preferred.is_empty() {
        return preferred;
    }
    let preferred: Vec<P<Node>> =
        declarations.iter().copied().filter(|&d| is_concrete_source_declaration(d)).collect();
    if !preferred.is_empty() {
        return preferred;
    }
    declarations
}

// sourcedefinition.go:657
fn get_property_like_source_declarations(
    original_node: P<Node>,
    declarations: &[P<Node>],
) -> Vec<P<Node>> {
    match original_node.parent() {
        Some(parent)
            if ast::is_access_expression(parent) && parent.name() == Some(original_node) => {}
        _ => return Vec::new(),
    }
    declarations
        .iter()
        .copied()
        .filter(|node| {
            matches!(
                node.kind(),
                Kind::PropertyAssignment
                    | Kind::ShorthandPropertyAssignment
                    | Kind::PropertyDeclaration
                    | Kind::PropertySignature
                    | Kind::MethodDeclaration
                    | Kind::MethodSignature
                    | Kind::GetAccessor
                    | Kind::SetAccessor
                    | Kind::EnumMember
            )
        })
        .collect()
}

// sourcedefinition.go:679
fn has_concrete_source_declarations(declarations: &[P<Node>]) -> bool {
    declarations.iter().any(|&d| is_concrete_source_declaration(d))
}

// sourcedefinition.go:683
fn is_concrete_source_declaration(node: P<Node>) -> bool {
    if !ast::is_declaration(node) || node.kind() == Kind::ExportAssignment {
        return false;
    }
    if (ast::is_binary_expression(node) || ast::is_call_expression(node))
        && ast::get_assignment_declaration_kind(node) != JSDeclarationKind::None
    {
        return false;
    }
    !matches!(
        node.kind(),
        Kind::Parameter
            | Kind::TypeParameter
            | Kind::BindingElement
            | Kind::ImportClause
            | Kind::ImportSpecifier
            | Kind::NamespaceImport
            | Kind::ExportSpecifier
            | Kind::PropertyAccessExpression
            | Kind::ElementAccessExpression
    )
}

// sourcedefinition.go:706
fn unique_declaration_nodes(nodes: &[P<Node>]) -> Vec<P<Node>> {
    let mut seen: FxHashSet<(String, TextRange)> = FxHashSet::default();
    let mut result: Vec<P<Node>> = Vec::with_capacity(nodes.len());
    result.extend(nodes.iter().filter_map(|&node| {
        let file_name = ast::get_source_file_of_node(node).unwrap().file_name().to_string();
        let key = (file_name, node.loc());
        if !seen.insert(key) {
            return None;
        }
        Some(node)
    }));
    result
}

// sourcedefinition.go:727
fn find_closest_declaration_node(source_file: P<SourceFile>, pos: u32) -> P<Node> {
    let node = astnav::get_touching_property_name(source_file, pos);
    let current = Some(node);
    if let Some(result) = std::iter::successors(current, |cur| cur.parent())
        .find(|&cur| ast::is_declaration(cur) || cur.kind() == Kind::ExportAssignment)
    {
        return result;
    }
    get_source_definition_entry_node(source_file)
}
