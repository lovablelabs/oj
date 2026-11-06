// Go's emitter.go: first the parts type checking depends on (sourceFileMayBeEmitted, declaration diagnostics), then
// the emitter itself (`mod emit`).

use tsrs_ast::{self as ast, SourceFile};
use tsrs_core::P;
use tsrs_core::tspath::{self, ComparePathsOptions};
use tsrs_tsoptions::outputpaths;

use crate::program::Program;

pub fn source_file_may_be_emitted(
    source_file: P<SourceFile>,
    host: &Program,
    force_dts_emit: bool,
    force_js_emit: bool,
) -> bool {
    // TODO: move this to outputpaths?
    let options = host.options();
    // Js files are emitted only if option is enabled
    if !force_js_emit
        && options.no_emit_for_js_files.is_true()
        && ast::is_source_file_js(source_file)
    {
        return false;
    }

    // Declaration files are not emitted
    if source_file.is_declaration_file.get() {
        return false;
    }

    // Runtime output for content-mapped files is owned by the external content mapper or build tool. Only
    // include them in the emit set when their transformed TypeScript can produce declarations.
    // emitter.go:508
    if !source_file.content_mapper().is_empty()
        && !force_dts_emit
        && !options.get_emit_declarations()
    {
        return false;
    }

    // Source file from node_modules are not emitted
    if host.is_source_file_from_external_library(source_file) {
        return false;
    }

    // forcing dts emit => file needs to be emitted
    if force_dts_emit || force_js_emit {
        return true;
    }

    // Check other conditions for file emit
    // Source files from referenced projects are not emitted
    if host.get_project_reference_from_source(source_file.path()).is_some() {
        return false;
    }

    // Any non json file should be emitted
    if !ast::is_json_source_file(source_file) {
        return true;
    }

    // Json file is not emitted if outDir is not specified
    if options.out_dir.is_empty() {
        return false;
    }

    // Otherwise, if rootDir is specified or a config file exists, we know the common source directory and can check if the file would be emitted in the same location
    if !options.root_dir.is_empty() || !options.config_file_path.is_empty() {
        let common_dir = tspath::get_normalized_absolute_path(
            &outputpaths::get_common_source_directory(
                &options,
                Vec::new,
                host.get_current_directory(),
                host.use_case_sensitive_file_names(),
                None,
            ),
            host.get_current_directory(),
        );
        let output_path = outputpaths::get_source_file_path_in_new_dir_worker(
            source_file.file_name(),
            &options.out_dir,
            host.get_current_directory(),
            &common_dir,
            host.use_case_sensitive_file_names(),
        );
        if tspath::compare_paths(
            source_file.file_name(),
            &output_path,
            &ComparePathsOptions {
                use_case_sensitive_file_names: host.use_case_sensitive_file_names(),
                current_directory: host.get_current_directory().to_string(),
            },
        ) == std::cmp::Ordering::Equal
        {
            return false;
        }
    }

    true
}

pub fn get_source_files_to_emit(
    host: &Program,
    target_source_files: Option<&[P<SourceFile>]>,
    force_dts_emit: bool,
    force_js_emit: bool,
) -> Vec<P<SourceFile>> {
    let target_source_files = target_source_files.unwrap_or_else(|| host.source_files());
    target_source_files
        .iter()
        .copied()
        .filter(|&f| source_file_may_be_emitted(f, host, force_dts_emit, force_js_emit))
        .collect()
}

fn is_source_file_not_json(file: P<SourceFile>) -> bool {
    !ast::is_json_source_file(file)
}

// emitter.go:566. Runs inside the caller's `CheckerSlot::lend` (the host's resolver borrows the checker from it).
pub fn get_declaration_diagnostics(
    host: &'static crate::emithost::EmitHost,
    program: &Program,
    file: P<SourceFile>,
) -> Vec<P<tsrs_ast::Diagnostic>> {
    // TODO: use p.getSourceFilesToEmit cache
    // Go passes the emit host as the SourceFileMayBeEmittedHost; its methods forward to the program.
    let full_files: Vec<P<SourceFile>> =
        get_source_files_to_emit(program, Some(&[file]), false, false)
            .into_iter()
            .filter(|&f| is_source_file_not_json(f))
            .collect();
    if !full_files.contains(&file) {
        return Vec::new();
    }
    let options = program.options();
    // The caller creates the resolver and context together in the file's scratch region.
    let transform = tsrs_declarations::new_declaration_transformer(
        host,
        tsrs_transformers::EmitHost::get_emit_resolver(host),
        options,
        "",
    );
    transform.base.transform_source_file(file);
    transform.get_diagnostics()
}

// ---------------------------------------------------------------------------------------------------------------
// The emitter (emitter.go:23 onwards, minus the parts above). Only built with the checker: emit needs the emit
// resolver. Nothing here runs unless a caller invokes `Program::emit` (the CLI does unless the options disable emit).
// ---------------------------------------------------------------------------------------------------------------

pub use self::emit::*;

mod emit {
    use tsrs_ast::{
        self as ast, Diagnostic, DiagnosticsCollection, SourceFile, new_compiler_diagnostic,
    };
    use tsrs_core::tspath::{self, ComparePathsOptions};
    use tsrs_core::{
        CompilerOptions, LanguageVariant, ModuleKind, NewLineKind, P, Tristate, stringutil,
    };
    use tsrs_diagnostics as diagnostics;
    use tsrs_printer::{
        self as printer, EmitContext, EmitTextWriter, PrintHandlers, Printer, PrinterOptions,
        SourceMapGenerator,
    };
    use tsrs_transformers::{
        EmitHost as _, ReferenceResolverRef, TransformOptions, Transformer, estransforms, inliners,
        jsxtransforms, moduletransforms, tstransforms,
    };
    use tsrs_tsoptions::outputpaths::{self, OutputPaths};

    use crate::emithost::EmitHost;
    use crate::program_emit::{
        EmitPhase, EmitResult, EmitTimes, SourceMapEmitResult, WriteFile, WriteFileData,
    };

    // emitter.go:24
    #[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
    #[expect(
        clippy::enum_variant_names,
        reason = "The variant names preserve the upstream Go enum names."
    )]
    pub enum EmitOnly {
        #[default]
        EmitAll,
        EmitOnlyJs,
        EmitOnlyDts,
        EmitOnlyBuilderSignature,
    }

    // emitter.go:33
    pub struct Emitter<'a> {
        pub(crate) host: &'static EmitHost,
        pub(crate) emit_only: EmitOnly,
        pub(crate) emitter_diagnostics: DiagnosticsCollection,
        pub(crate) paths: OutputPaths,
        pub(crate) source_file: P<SourceFile>,
        pub(crate) emit_result: EmitResult,
        pub(crate) force_emit: bool,
        pub(crate) write_file: Option<WriteFile<'a>>,
        pub(crate) times: &'a EmitTimes,
        pub(crate) pending_js: Option<PendingPrint>,
        pub(crate) declaration_diagnostics: Vec<P<Diagnostic>>,
        pub(crate) pending_declaration: Option<PendingPrint>,
        pub(crate) emit_context: P<EmitContext>,
    }

    /// `TSRS_EMIT_MEM=1`: per file, the arena bytes its emit used in its region and outside it (stderr, `emitmem`).
    pub fn emit_mem_log() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("TSRS_EMIT_MEM").is_some_and(|v| v == "1"))
    }

    // tsrs-only: what printing a transformed file needs. Go prints and writes each file while it still holds the
    // file's checker; tsrs splits `emit` into `transform` (needs the checker) and `print` (does not), so that
    // `Program::emit` can print and write on another thread while the checker transforms the next file.
    pub struct PendingPrint {
        source_file: P<SourceFile>,
        emit_context: P<EmitContext>,
        printer_options: PrinterOptions,
        file_path: String,
        source_map_file_path: String,
        // `None`: the program's options (JS file); `Some`: the declaration map options
        map_options: Option<CompilerOptions>,
        should_emit_source_maps: bool,
        // Go `PrintHandlers.MapSourcePosition` (emitDeclarationFile, for a content-mapped file).
        map_source_position: Option<MapSourcePosition>,
    }

    // `Send`: the pending print may run on the thread that writes the files (program_emit.rs).
    type MapSourcePosition = Box<
        dyn Fn(printer::SourceMapSource, u32) -> Option<(printer::SourceMapSource, u32)> + Send,
    >;

    impl Emitter<'_> {
        // emitter.go:46 `emit` is `transform` followed by `print`. This part runs the transformers (and so needs the
        // file's checker).
        pub(crate) fn transform(&mut self) {
            tsrs_ast::with_subtree_facts_cache(|| self.transform_worker());
        }

        fn transform_worker(&mut self) {
            let js_file_path = self.paths.js_file_path().to_string();
            let source_map_file_path = self.paths.source_map_file_path().to_string();
            self.emit_js_file(Some(self.source_file), &js_file_path, &source_map_file_path);
            let declaration_file_path = self.paths.declaration_file_path().to_string();
            let declaration_map_path = self.paths.declaration_map_path().to_string();
            self.emit_declaration_file(
                Some(self.source_file),
                &declaration_file_path,
                &declaration_map_path,
            );
        }

        // The rest of `emit`, in Go's order: print and write the JS file, add the declaration transform's
        // diagnostics, print and write the declaration file.
        pub(crate) fn print(&mut self, writer: &mut (dyn EmitTextWriter + 'static)) {
            if let Some(pending) = self.pending_js.take() {
                self.print_pending(pending, writer);
            }
            for elem in std::mem::take(&mut self.declaration_diagnostics) {
                // Add declaration transform diagnostics to emit diagnostics
                self.emitter_diagnostics.add(elem);
            }
            if let Some(pending) = self.pending_declaration.take() {
                self.print_pending(pending, writer);
            }
            self.emit_context.reset();
            self.emit_result.diagnostics = self.emitter_diagnostics.get_diagnostics();
        }

        fn print_pending(
            &mut self,
            mut pending: PendingPrint,
            writer: &mut (dyn EmitTextWriter + 'static),
        ) {
            // create a printer to print the nodes
            let map_source_position = pending.map_source_position.take().map(
                |handler| -> Box<
                    dyn Fn(
                        printer::SourceMapSource,
                        u32,
                    ) -> Option<(printer::SourceMapSource, u32)>,
                > { handler },
            );
            let print_handlers = PrintHandlers { map_source_position, ..Default::default() };
            let printer = printer::new_printer(
                pending.printer_options,
                print_handlers,
                Some(pending.emit_context),
            );
            let options = self.host.options();
            let map_options = pending.map_options.as_ref().unwrap_or(&options);
            self.print_source_file(
                &pending.file_path,
                &pending.source_map_file_path,
                pending.source_file,
                printer,
                map_options,
                pending.should_emit_source_maps,
                writer,
            );
        }

        // emitter.go:62. Go returns a slice of the `declarationTransformer` interface; the two transformers run in
        // `run_declaration_transformers` in the same order.
        fn run_declaration_transformers(
            &self,
            source_file: P<SourceFile>,
            declaration_file_path: &str,
        ) -> (P<SourceFile>, Vec<P<Diagnostic>>) {
            let force_dts_emit = self.emit_only == EmitOnly::EmitOnlyBuilderSignature
                || self.force_emit && self.emit_only == EmitOnly::EmitOnlyDts;
            let mut diags = Vec::new();
            // getDeclarationTransformers (emitter.go:62)
            let declaration = tsrs_declarations::new_declaration_transformer(
                self.host,
                self.host.get_emit_resolver(),
                self.host.options(),
                declaration_file_path,
            );
            let supplemental = tsrs_declarations::new_supplemental_references_transformer(
                self.host,
                source_file,
                declaration_file_path,
                force_dts_emit,
            );
            // emitter.go:81
            let source_file = self.times.time(EmitPhase::DeclarationTransform, || {
                let source_file = declaration.base.transform_source_file(source_file);
                diags.extend(declaration.get_diagnostics());
                let source_file = supplemental.transform_source_file(source_file);
                diags.extend(supplemental.get_diagnostics());
                source_file
            });
            (source_file, diags)
        }

        // emitter.go:71
        fn run_script_transformers(
            &self,
            emit_context: P<EmitContext>,
            mut source_file: P<SourceFile>,
        ) -> P<SourceFile> {
            let host = self.host;
            self.times.time(EmitPhase::ScriptTransform, || {
                source_file = get_script_transformers(emit_context, host, source_file)
                    .into_iter()
                    .fold(source_file, |mut source_file, transformer| {
                        source_file = transformer.transform_source_file(source_file);

                        source_file
                    });
                source_file
            })
        }

        // emitter.go:192
        fn emit_js_file(
            &mut self,
            source_file: Option<P<SourceFile>>,
            js_file_path: &str,
            source_map_file_path: &str,
        ) {
            let options = self.host.options();

            let Some(source_file) = source_file else {
                return;
            };
            if self.emit_only != EmitOnly::EmitAll && self.emit_only != EmitOnly::EmitOnlyJs
                || js_file_path.is_empty()
            {
                return;
            }

            if !self.force_emit
                && (options.no_emit == Tristate::True || self.host.is_emit_blocked(js_file_path))
            {
                self.emit_result.emit_skipped = true;
                return;
            }

            let emit_context = self.emit_context;

            let source_file = self.run_script_transformers(emit_context, source_file);

            let printer_options = PrinterOptions {
                remove_comments: options.remove_comments.is_true(),
                new_line: options.new_line,
                no_emit_helpers: options.no_emit_helpers.is_true(),
                source_map: options.source_map.is_true(),
                inline_source_map: options.inline_source_map.is_true(),
                inline_sources: options.inline_sources.is_true(),
                target: options.target,
                // !!!
                ..Default::default()
            };

            // the printer is created, and the file printed, in `print`
            self.pending_js = Some(PendingPrint {
                source_file,
                emit_context,
                printer_options,
                file_path: js_file_path.to_string(),
                source_map_file_path: source_map_file_path.to_string(),
                map_options: None,
                should_emit_source_maps: should_emit_source_maps(&options, source_file),
                map_source_position: None,
            });
        }

        // emitter.go:239
        fn emit_declaration_file(
            &mut self,
            source_file: Option<P<SourceFile>>,
            declaration_file_path: &str,
            declaration_map_path: &str,
        ) {
            let options = self.host.options();

            let Some(source_file) = source_file else {
                return;
            };
            if self.emit_only == EmitOnly::EmitOnlyJs || declaration_file_path.is_empty() {
                return;
            }
            let emit_declaration_map = self.emit_only != EmitOnly::EmitOnlyBuilderSignature
                && options.declaration_map.is_true();
            let content_mapped_source = source_file;

            let emit_context = self.emit_context;
            let (source_file, diags) =
                self.run_declaration_transformers(source_file, declaration_file_path);

            // added to the emit diagnostics in `print`, after the JS file is written
            let decl_blocked = !diags.is_empty()
                && !self.force_emit
                && self.emit_only != EmitOnly::EmitOnlyBuilderSignature;
            self.declaration_diagnostics = diags;

            if !self.force_emit
                && self.emit_only != EmitOnly::EmitOnlyBuilderSignature
                && (options.no_emit == Tristate::True
                    || self.host.is_emit_blocked(declaration_file_path))
            {
                self.emit_result.emit_skipped = true;
                return;
            }

            if decl_blocked {
                self.emit_result.emit_skipped = true;
                return;
            }

            let printer_options = PrinterOptions {
                remove_comments: options.remove_comments.is_true(),
                new_line: options.new_line,
                no_emit_helpers: true,
                // Module: 			   options.Module, // NYI
                // ModuleResolution:   options.ModuleResolution, // NYI
                target: options.get_emit_script_target(),
                source_map: emit_declaration_map,
                inline_source_map: options.inline_source_map.is_true(),
                // InlineSources:       options.InlineSources.IsTrue(), // ignored, per strada
                // ExtendedDiagnostics: options.ExtendedDiagnostics.IsTrue(), // NYI
                only_print_js_doc_style: true,
                omit_brace_source_map_positions: true,
                ..Default::default()
            };

            // create a printer to print the nodes (`print_pending`, with these handlers)
            let mut map_source_position: Option<MapSourcePosition> = None;
            if let Some(span_map) =
                content_mapped_source.span_map().filter(|_| emit_declaration_map)
            {
                let original_source: &'static DeclarationMapSource =
                    new_declaration_map_source(content_mapped_source);
                let content_mapped_file_name = content_mapped_source.get().file_name();
                map_source_position =
                    Some(Box::new(move |source: printer::SourceMapSource, pos: u32| {
                        if source.file_name() != content_mapped_file_name {
                            return Some((source, pos));
                        }
                        let (mapped, ok) = span_map.virtual_to_original_position_exact(pos);
                        if !ok {
                            return None;
                        }
                        Some((original_source, mapped))
                    }));
            }

            let declaration_map_options = CompilerOptions {
                source_map: if emit_declaration_map { Tristate::True } else { Tristate::False },
                source_root: options.source_root.clone(),
                map_root: options.map_root.clone(),
                // Explicitly do not pass through either inline option.
                ..Default::default()
            };
            let should_emit = should_emit_source_maps(&declaration_map_options, source_file);
            self.pending_declaration = Some(PendingPrint {
                source_file,
                emit_context,
                printer_options,
                file_path: declaration_file_path.to_string(),
                source_map_file_path: declaration_map_path.to_string(),
                map_options: Some(declaration_map_options),
                should_emit_source_maps: should_emit,
                map_source_position,
            });
        }

        // emitter.go:345
        fn print_source_file(
            &mut self,
            js_file_path: &str,
            source_map_file_path: &str,
            source_file: P<SourceFile>,
            mut printer_: Printer,
            map_options: &CompilerOptions,
            should_emit_source_maps: bool,
            writer: &mut (dyn EmitTextWriter + 'static),
        ) {
            // !!! sourceMapGenerator
            let options = self.host.options();
            let mut source_map_generator: Option<SourceMapGenerator> = None;
            if should_emit_source_maps {
                source_map_generator = Some(tsrs_sourcemap::new_generator(
                    &tspath::get_base_file_name(&tspath::normalize_slashes(js_file_path)),
                    &get_source_root(map_options),
                    &self.get_source_map_directory(map_options, js_file_path, Some(source_file)),
                    ComparePathsOptions {
                        use_case_sensitive_file_names: self.host.use_case_sensitive_file_names(),
                        current_directory: self.host.get_current_directory().to_string(),
                    },
                ));
            }

            let print_start = self.times.start();
            printer_.write(
                source_file.as_node(),
                Some(source_file),
                writer,
                source_map_generator.as_mut(),
            );
            self.times.add(EmitPhase::Print, print_start);

            let mut source_map_url_pos = None;
            if let Some(source_map_generator) = &mut source_map_generator {
                let source_map_start = self.times.start();
                if map_options.source_map.is_true() || map_options.inline_source_map.is_true() {
                    self.emit_result.source_maps.push(SourceMapEmitResult {
                        input_source_file_names: source_map_generator.sources().to_vec(),
                        source_map: source_map_generator.raw_source_map(),
                        generated_file: js_file_path.to_string(),
                    });
                }

                let source_mapping_url = self.get_source_mapping_url(
                    map_options,
                    source_map_generator,
                    js_file_path,
                    source_map_file_path,
                    Some(source_file),
                );

                if !source_mapping_url.is_empty() {
                    if !writer.is_at_start_of_line() {
                        writer.raw_write(if options.new_line == NewLineKind::CRLF {
                            "\r\n"
                        } else {
                            "\n"
                        });
                    }
                    source_map_url_pos = Some(writer.get_text_pos());
                    writer.write_comment("//# sourceMappingURL=");
                    writer.write_comment(&source_mapping_url);
                }

                // Write the source map
                if source_map_file_path.is_empty() {
                    self.times.add(EmitPhase::SourceMap, source_map_start);
                } else {
                    let source_map = source_map_generator.string();
                    self.times.add(EmitPhase::SourceMap, source_map_start);
                    let err = self.write_text(
                        source_map_file_path,
                        &source_map,
                        &mut WriteFileData {
                            source_file: Some(self.source_file),
                            ..Default::default()
                        },
                    );
                    match err {
                        Err(err) => {
                            self.emitter_diagnostics.add(new_compiler_diagnostic(
                                &diagnostics::COULD_NOT_WRITE_FILE_0_COLON_1,
                                &[&js_file_path, &err],
                            ));
                        }
                        Ok(()) => {
                            self.emit_result.emitted_files.push(source_map_file_path.to_string());
                        }
                    }
                }
            } else {
                writer.write_line();
            }

            // Write the output file
            let mut text = writer.take_string();
            if options.emit_bom.is_true() {
                text = stringutil::add_utf8_byte_order_mark(&text);
            }
            let mut data = WriteFileData {
                source_map_url_pos,
                diagnostics: self.emitter_diagnostics.get_diagnostics(),
                source_file: Some(self.source_file),
                ..Default::default()
            };
            let err = self.write_text(js_file_path, &text, &mut data);
            let skipped_dts_write = data.skipped_dts_write;
            match err {
                Err(err) => {
                    self.emitter_diagnostics.add(new_compiler_diagnostic(
                        &diagnostics::COULD_NOT_WRITE_FILE_0_COLON_1,
                        &[&js_file_path, &err],
                    ));
                }
                Ok(()) if !skipped_dts_write => {
                    self.emit_result.emitted_files.push(js_file_path.to_string());
                }
                Ok(()) => {}
            }
        }

        // emitter.go:436
        fn write_text(
            &self,
            file_name: &str,
            text: &str,
            data: &mut WriteFileData,
        ) -> Result<(), String> {
            let _permit = self.times.write_permit();
            self.times.time(EmitPhase::Write, || {
                if let Some(write_file) = self.write_file {
                    return write_file(file_name, text, data);
                }
                self.host.write_file(file_name, text)
            })
        }

        // emitter.go:460
        fn get_source_map_directory(
            &self,
            map_options: &CompilerOptions,
            file_path: &str,
            source_file: Option<P<SourceFile>>,
        ) -> String {
            if !map_options.source_root.is_empty() {
                return self.host.common_source_directory();
            }
            if !map_options.map_root.is_empty() {
                let mut source_map_dir = tspath::normalize_slashes(&map_options.map_root);
                if let Some(source_file) = source_file {
                    // For modules or multiple emit files the mapRoot will have directory structure like the sources
                    // So if src\a.ts and src\lib\b.ts are compiled together user would be moving the maps into mapRoot\a.js.map and mapRoot\lib\b.js.map
                    source_map_dir =
                        tspath::get_directory_path(&outputpaths::get_source_file_path_in_new_dir(
                            source_file.file_name(),
                            &source_map_dir,
                            self.host.get_current_directory(),
                            &self.host.common_source_directory(),
                            self.host.use_case_sensitive_file_names(),
                        ));
                }
                if tspath::get_root_length(&source_map_dir) == 0 {
                    // The relative paths are relative to the common directory
                    source_map_dir = tspath::combine_paths(
                        &self.host.common_source_directory(),
                        &[&source_map_dir],
                    );
                }
                return source_map_dir;
            }
            tspath::get_directory_path(&tspath::normalize_path(file_path))
        }

        // emitter.go:443
        fn get_source_mapping_url(
            &self,
            map_options: &CompilerOptions,
            source_map_generator: &mut SourceMapGenerator,
            file_path: &str,
            source_map_file_path: &str,
            source_file: Option<P<SourceFile>>,
        ) -> String {
            if map_options.inline_source_map.is_true() {
                // Encode the sourceMap into the sourceMap url
                return source_map_generator.base64_data_url();
            }

            let source_map_file =
                tspath::get_base_file_name(&tspath::normalize_slashes(source_map_file_path));
            if !map_options.map_root.is_empty() {
                let mut source_map_dir = tspath::normalize_slashes(&map_options.map_root);
                if let Some(source_file) = source_file {
                    // For modules or multiple emit files the mapRoot will have directory structure like the sources
                    // So if src\a.ts and src\lib\b.ts are compiled together user would be moving the maps into mapRoot\a.js.map and mapRoot\lib\b.js.map
                    source_map_dir =
                        tspath::get_directory_path(&outputpaths::get_source_file_path_in_new_dir(
                            source_file.file_name(),
                            &source_map_dir,
                            self.host.get_current_directory(),
                            &self.host.common_source_directory(),
                            self.host.use_case_sensitive_file_names(),
                        ));
                }
                if tspath::get_root_length(&source_map_dir) == 0 {
                    // The relative paths are relative to the common directory
                    source_map_dir = tspath::combine_paths(
                        &self.host.common_source_directory(),
                        &[&source_map_dir],
                    );
                    return stringutil::encode_uri(&tspath::get_relative_path_to_directory_or_url(
                        &tspath::get_directory_path(&tspath::normalize_path(file_path)), // get the relative sourceMapDir path based on jsFilePath
                        &tspath::combine_paths(&source_map_dir, &[&source_map_file]), // this is where user expects to see sourceMap
                        /*isAbsolutePathAnUrl*/ true,
                        &ComparePathsOptions {
                            use_case_sensitive_file_names: self
                                .host
                                .use_case_sensitive_file_names(),
                            current_directory: self.host.get_current_directory().to_string(),
                        },
                    ));
                }
                return stringutil::encode_uri(&tspath::combine_paths(
                    &source_map_dir,
                    &[&source_map_file],
                ));
            }
            stringutil::encode_uri(&source_map_file)
        }
    }

    // emitter.go:295
    // Go `declarationMapSource`: the original (content-mapped) source a declaration map points at.
    pub struct DeclarationMapSource {
        file_name: String,
        text: &'static str,
        line_map: Vec<u32>,
    }

    // emitter.go:301
    pub fn new_declaration_map_source(source_file: P<SourceFile>) -> &'static DeclarationMapSource {
        let text = source_file.original_text();
        P::new(DeclarationMapSource {
            file_name: source_file.original_file_name().to_string(),
            text,
            line_map: tsrs_core::compute_ecma_line_starts(text),
        })
        .get()
    }

    impl tsrs_sourcemap::Source for DeclarationMapSource {
        // emitter.go:310
        fn file_name(&self) -> &str {
            &self.file_name
        }
        // emitter.go:311
        fn text(&self) -> &str {
            self.text
        }
        // emitter.go:312
        fn ecma_line_map(&self) -> &[u32] {
            &self.line_map
        }
    }

    // emitter.go:93
    fn get_module_transformer(opts: &TransformOptions) -> Option<P<Transformer>> {
        match opts.compiler_options.get_emit_module_kind() {
            ModuleKind::PRESERVE => {
                // `ESModuleTransformer` contains logic for preserving CJS input syntax in `--module preserve`
                moduletransforms::new_es_module_transformer(opts)
            }
            ModuleKind::ES_NEXT
            | ModuleKind::ES2022
            | ModuleKind::ES2020
            | ModuleKind::ES2015
            | ModuleKind::NODE20
            | ModuleKind::NODE18
            | ModuleKind::NODE16
            | ModuleKind::NODE_NEXT
            | ModuleKind::COMMON_JS => moduletransforms::new_implied_module_transformer(opts),
            _ => moduletransforms::new_common_js_module_transformer(opts),
        }
    }

    // emitter.go:116
    pub fn get_script_transformers(
        emit_context: P<EmitContext>,
        host: &'static EmitHost,
        source_file: P<SourceFile>,
    ) -> Vec<P<Transformer>> {
        let mut tx: Vec<P<Transformer>> = Vec::new();
        let options = host.options();

        // JS files don't use reference calculations as they don't do import elision, no need to calculate it
        let import_elision_enabled = !options.verbatim_module_syntax.is_true()
            && !ast::is_in_js_file(Some(source_file.as_node()));
        let jsx_transform_enabled = options.get_jsx_transform_enabled()
            && source_file.language_variant.get() == LanguageVariant::JSX;

        let emit_resolver = host.get_emit_resolver();

        let reference_resolver = if import_elision_enabled
            || jsx_transform_enabled
            || !options.get_isolated_modules()
            || options.emit_decorator_metadata.is_true()
        {
            ReferenceResolverRef::Emit(emit_resolver)
        } else {
            ReferenceResolverRef::Plain(tsrs_binder::new_reference_resolver(
                options,
                tsrs_binder::ReferenceResolverHooks::<()>::default(),
            ))
        };

        let opts = TransformOptions {
            context: emit_context,
            compiler_options: options,
            resolver: reference_resolver,
            emit_resolver: Some(emit_resolver),
            get_emit_module_format_of_file: std::rc::Rc::new(move |file| {
                host.get_emit_module_format_of_file(file)
            }),
            content_mapper_extension_rewrites: host.content_mapper_extension_rewrites(),
            use_case_sensitive_file_names: host.use_case_sensitive_file_names(),
        };

        // transform TypeScript syntax
        {
            // use type nodes to add metadata decorators
            if options.emit_decorator_metadata.is_true() {
                tx.extend(tstransforms::new_metadata_transformer(&opts));
            }

            // erase types
            tx.extend(tstransforms::new_type_eraser_transformer(&opts));

            // elide imports
            if import_elision_enabled {
                tx.extend(tstransforms::new_import_elision_transformer(&opts));
            }

            // transform `enum`, `namespace`, and parameter properties
            tx.extend(tstransforms::new_runtime_syntax_transformer(&opts));

            if options.experimental_decorators.is_true() {
                tx.extend(tstransforms::new_legacy_decorators_transformer(&opts));
            }
        }

        if jsx_transform_enabled {
            tx.extend(jsxtransforms::new_jsx_transformer(&opts));
        }

        let downleveler = estransforms::get_es_transformer(&opts);
        if let Some(downleveler) = downleveler {
            tx.push(downleveler);
        }

        tx.extend(estransforms::new_use_strict_transformer(&opts));

        // transform module syntax
        tx.extend(get_module_transformer(&opts));

        // inlining (formerly done via substitutions)
        if !options.get_isolated_modules() {
            tx.extend(inliners::new_const_enum_inlining_transformer(&opts));
        }
        tx
    }

    // emitter.go:444
    pub fn should_emit_source_maps(
        map_options: &CompilerOptions,
        source_file: P<SourceFile>,
    ) -> bool {
        (map_options.source_map.is_true() || map_options.inline_source_map.is_true())
            && !tspath::file_extension_is(source_file.file_name(), tspath::EXTENSION_JSON)
    }

    // emitter.go:449
    pub fn get_source_root(map_options: &CompilerOptions) -> String {
        // Normalize source root and make sure it has trailing "/" so that it can be used to combine paths with the
        // relative paths of the sources list in the sourcemap
        let mut source_root = tspath::normalize_slashes(&map_options.source_root);
        if !source_root.is_empty() {
            source_root = tspath::ensure_trailing_directory_separator(&source_root);
        }
        source_root
    }
}
