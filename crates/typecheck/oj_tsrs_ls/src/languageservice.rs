use std::sync::{Arc, Mutex};

use rustc_hash::FxHashMap;
use tsrs_ast::SourceFile;
use tsrs_checker::Checker;
use tsrs_compiler::Program;
use tsrs_core::P;
use tsrs_core::tspath::{self, Path};
use tsrs_lsproto as lsproto;
use tsrs_vfs::vfsmatch;

use crate::autoimport::{self, ProjectID, RegistryExt as _};
use crate::completions::err_needs_auto_imports;
use crate::host::Host;
use crate::lsconv::Converters;
use crate::lsutil::{FormatCodeSettings, UserPreferences};
use crate::sourcemap::{self, DocumentPositionMapper, ECMALineInfo};

// languageservice.go:16
pub struct LanguageService {
    pub(crate) project_id: ProjectID,
    pub(crate) host: Arc<dyn Host>,
    active_config: UserPreferences,
    program: &'static Program,
    pub(crate) converters: Arc<Converters>,
    // Go mutates a plain map through the *LanguageService (one goroutine per service); a Mutex keeps the service
    // Sync so it can be shared like Go's pointer.
    document_position_mappers: Mutex<FxHashMap<String, Option<Arc<DocumentPositionMapper>>>>,
}

// languageservice.go:25
pub fn new_language_service(
    project_id: ProjectID,
    program: &'static Program,
    host: Arc<dyn Host>,
    active_file: &str,
) -> LanguageService {
    LanguageService {
        project_id,
        converters: host.converters(),
        active_config: host.get_preferences(active_file),
        host,
        program,
        document_position_mappers: Mutex::new(FxHashMap::default()),
    }
}

impl LanguageService {
    // languageservice.go:41
    pub(crate) fn to_path(&self, file_name: &str) -> Path {
        tspath::to_path(
            file_name,
            self.program.get_current_directory(),
            self.use_case_sensitive_file_names(),
        )
    }

    // languageservice.go:45
    pub fn get_program(&self) -> &'static Program {
        self.program
    }

    // languageservice.go:49
    pub(crate) fn user_preferences(&self) -> &UserPreferences {
        &self.active_config
    }

    // languageservice.go:53
    pub(crate) fn format_options(&self) -> FormatCodeSettings {
        self.active_config.format_code_settings.clone()
    }

    // languageservice.go:57
    pub(crate) fn try_get_program_and_file(
        &self,
        file_name: &str,
    ) -> (&'static Program, Option<P<SourceFile>>) {
        let program = self.get_program();
        let file = program.get_source_file(file_name);
        (program, file)
    }

    // languageservice.go:63
    pub(crate) fn get_program_and_file(
        &self,
        document_uri: &lsproto::DocumentUri,
    ) -> (&'static Program, P<SourceFile>) {
        let file_name = document_uri.file_name();
        let (program, file) = self.try_get_program_and_file(&file_name);
        let Some(file) = file else {
            panic!("file not found: {file_name}");
        };
        (program, file)
    }

    // languageservice.go:72
    pub(crate) fn get_document_position_mapper(
        &self,
        file_name: &str,
    ) -> Option<Arc<DocumentPositionMapper>> {
        if let Some(d) = self.document_position_mappers.lock().unwrap().get(file_name) {
            return d.clone();
        }
        let d = sourcemap::get_document_position_mapper(self, file_name).map(Arc::new);
        self.document_position_mappers.lock().unwrap().insert(file_name.to_string(), d.clone());
        d
    }

    // languageservice.go:81
    pub(crate) fn read_file(&self, file_name: &str) -> Option<String> {
        self.host.read_file(file_name)
    }

    // languageservice.go:85
    pub(crate) fn use_case_sensitive_file_names(&self) -> bool {
        self.host.use_case_sensitive_file_names()
    }

    // languageservice.go:89
    fn get_ecma_line_info(&self, file_name: &str) -> Option<Arc<ECMALineInfo>> {
        self.host.get_ecma_line_info(file_name)
    }

    // getPreparedAutoImportView returns an auto-import view for the given file if the registry is prepared
    // to provide up-to-date auto-imports for it. If not, it returns ErrNeedsAutoImports.
    // languageservice.go:95
    pub(crate) fn get_prepared_auto_import_view(
        &self,
        from_file: P<SourceFile>,
        type_checker: &mut Checker,
    ) -> Result<autoimport::View, lsproto::Error> {
        let registry = self.host.auto_import_registry();
        let mut registry_file = from_file;
        if let Some(canonical) = from_file.canonical_source_file() {
            registry_file = canonical;
        }
        if !registry.is_prepared_for_importing_file(
            registry_file.file_name(),
            &self.project_id,
            self.user_preferences(),
        ) {
            return Err(err_needs_auto_imports());
        }

        let view = autoimport::new_view(
            registry,
            from_file,
            self.project_id.clone(),
            self.program,
            type_checker,
            self.user_preferences().module_specifier_preferences(),
        );
        Ok(view)
    }

    // getCurrentAutoImportView returns an auto-import view for the given file, based on the current state
    // of the auto-import registry, which may or may not be up-to-date.
    // languageservice.go:111
    pub(crate) fn get_current_auto_import_view(
        &self,
        from_file: P<SourceFile>,
        type_checker: &mut Checker,
    ) -> autoimport::View {
        autoimport::new_view(
            self.host.auto_import_registry(),
            from_file,
            self.project_id.clone(),
            self.program,
            type_checker,
            self.user_preferences().module_specifier_preferences(),
        )
    }

    // Used for module specifier completions.
    // languageservice.go:123
    pub(crate) fn directory_exists(&self, path: &str) -> bool {
        self.host.directory_exists(path)
    }

    // Used for module specifier completions.
    // languageservice.go:128
    pub(crate) fn read_directory(
        &self,
        path: &str,
        extensions: &[String],
        includes: &[String],
    ) -> Vec<String> {
        self.host.read_directory(
            self.program.get_current_directory(),
            path,
            extensions,
            &[], /*excludes*/
            includes,
            vfsmatch::UNLIMITED_DEPTH,
        )
    }

    // languageservice.go:132
    pub(crate) fn get_directories(&self, path: &str) -> Vec<String> {
        self.host.get_directories(path)
    }
}

// LanguageService is the sourcemap host Go passes to `sourcemap.GetDocumentPositionMapper(l, fileName)`.
impl sourcemap::Host for LanguageService {
    fn use_case_sensitive_file_names(&self) -> bool {
        LanguageService::use_case_sensitive_file_names(self)
    }
    fn get_ecma_line_info(&self, file_name: &str) -> Option<Arc<ECMALineInfo>> {
        LanguageService::get_ecma_line_info(self, file_name)
    }
    fn read_file(&self, file_name: &str) -> Option<String> {
        LanguageService::read_file(self, file_name)
    }
}
