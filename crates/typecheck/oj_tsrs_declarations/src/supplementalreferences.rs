use crate::*;

// Non-function declarations in supplementalreferences.go (hand-ported in types.rs):
//   type SupplementalReferencesTransformer (supplementalreferences.go:12)

// supplementalreferences.go:19
pub fn new_supplemental_references_transformer(
    host: &'static dyn DeclarationEmitHost,
    source_file: P<SourceFile>,
    declaration_file_path: &str,
    force_declaration_paths: bool,
) -> P<SupplementalReferencesTransformer> {
    P::new(SupplementalReferencesTransformer {
        host,
        supplemental_files: source_file.supplemental_source_files(),
        declaration_file_path: declaration_file_path.to_string(),
        force_declaration_paths,
    })
}

impl SupplementalReferencesTransformer {
    // supplementalreferences.go:28
    pub fn transform_source_file(&self, source_file: P<SourceFile>) -> P<SourceFile> {
        for &supplemental in self.supplemental_files {
            if !self.host.source_file_may_be_emitted(supplemental, self.force_declaration_paths) {
                continue;
            }
            let output_paths =
                self.host.get_output_paths_for(supplemental, self.force_declaration_paths);
            let declaration_path = output_paths.declaration_file_path();
            if declaration_path.is_empty() {
                continue;
            }
            let mut referenced_files = source_file.referenced_files().to_vec();
            referenced_files.push(P::new(FileReference {
                text_range: tsrs_core::TextRange::synthetic(),
                file_name: tspath::get_relative_path_from_file(
                    &self.declaration_file_path,
                    declaration_path,
                    &tspath::ComparePathsOptions {
                        current_directory: self.host.get_current_directory().to_string(),
                        use_case_sensitive_file_names: self.host.use_case_sensitive_file_names(),
                    },
                ),
                resolution_mode: tsrs_core::ResolutionMode::default(),
                preserve: false,
            }));
            source_file.referenced_files.set(alloc_vec(referenced_files));
        }
        source_file
    }

    // supplementalreferences.go:52
    #[expect(
        clippy::unused_self,
        reason = "Declaration transformers expose diagnostics through the same instance interface."
    )]
    pub fn get_diagnostics(&self) -> Vec<P<Diagnostic>> {
        Vec::new()
    }
}
