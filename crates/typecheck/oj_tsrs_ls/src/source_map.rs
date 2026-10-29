use tsrs_ast::SourceFile;
use tsrs_core::{P, TextPos, TextRange, tspath};
use tsrs_lsproto as lsproto;
use tsrs_tsoptions::outputpaths;

use crate::languageservice::LanguageService;
use crate::lsconv::{self, Script};
use crate::sourcemap::DocumentPosition;
use crate::spanmap::{Feature, Fidelity, SpanMap};

impl LanguageService {
    // sourceFileRangeToLSPLocation maps a range from an arbitrary program SourceFile to an LSP location,
    // composing content-mapper span maps and declaration source maps as needed. LS features should use this
    // for cross-file results instead of calling getMappedLocation or lsconv.ToLSPLocation directly.
    // This unfiltered form is appropriate for diagnostics and text edits.
    // source_map.go:17
    pub(crate) fn source_file_range_to_lsp_location(
        &self,
        file: P<SourceFile>,
        file_range: TextRange,
    ) -> (lsproto::Location, Fidelity) {
        if !file.content_mapper().is_empty() {
            return self.converters.to_lsp_location(&file, file_range);
        }
        self.get_mapped_location(file.file_name(), file_range)
    }

    // sourceFileRangeToLSPLocationForFeature is the preferred conversion for visible LS results that may
    // come from another file. It applies content-mapper feature filtering and follows declaration source maps.
    // Do not use it for diagnostics or text edits.
    // source_map.go:27
    pub(crate) fn source_file_range_to_lsp_location_for_feature(
        &self,
        file: P<SourceFile>,
        file_range: TextRange,
        feature: Feature,
    ) -> (lsproto::Location, Fidelity) {
        if !file.content_mapper().is_empty() {
            return self.converters.to_lsp_location_for_feature(&file, file_range, feature);
        }
        self.get_mapped_location(file.file_name(), file_range)
    }

    // getMappedLocation follows declaration source maps from a .d.ts range to its source location.
    // It is an implementation detail of sourceFileRangeToLSPLocation; LS features should not call it directly,
    // because it does not preserve a content-mapper projection or apply span-map feature filtering.
    // source_map.go:38
    fn get_mapped_location(
        &self,
        file_name: &str,
        file_range: TextRange,
    ) -> (lsproto::Location, Fidelity) {
        let Some(start_pos) = self.try_get_source_position(file_name, file_range.pos()) else {
            // Go passes the nil *script to the converters, which dereference it.
            let script = self.get_script(file_name).expect("nil script");
            let (lsp_range, fidelity) = self.create_lsp_range_from_range(file_range, &script);
            return (
                lsproto::Location {
                    uri: lsconv::file_name_to_document_uri(file_name),
                    range: lsp_range,
                },
                fidelity,
            );
        };
        let mut end_pos = self.try_get_source_position(file_name, file_range.end());
        if end_pos
            .as_ref()
            .is_none_or(|e| e.file_name != start_pos.file_name || e.pos < start_pos.pos)
        {
            // When end doesn't map, maps to a different source file (e.g. in a .d.ts with a
            // multi-source source map from --outFile compilation), or maps to a position before
            // start (non-monotonic source map mappings), approximate the end position.
            end_pos = Some(DocumentPosition {
                file_name: start_pos.file_name.clone(),
                pos: start_pos.pos
                    + file_range.len().expect("mapped source ranges have source positions"),
            });
        }
        let end_pos = end_pos.unwrap();
        let new_range = TextRange::new(start_pos.pos, end_pos.pos);
        let script = self.get_script(&start_pos.file_name).expect("nil script");
        let (lsp_range, fidelity) = self.create_lsp_range_from_range(new_range, &script);
        (
            lsproto::Location {
                uri: lsconv::file_name_to_document_uri(&start_pos.file_name),
                range: lsp_range,
            },
            fidelity,
        )
    }
}

// source_map.go:65
pub struct SourceMapScript {
    file_name: String,
    text: String,
}

impl Script for SourceMapScript {
    // source_map.go:70
    fn file_name(&self) -> &str {
        &self.file_name
    }

    // source_map.go:74
    fn original_file_name(&self) -> &str {
        &self.file_name
    }

    // source_map.go:76
    fn text(&self) -> &str {
        &self.text
    }

    // source_map.go:80
    fn original_text(&self) -> &str {
        &self.text
    }

    // source_map.go:81
    fn span_map(&self) -> Option<&SpanMap> {
        None
    }
}

impl LanguageService {
    // source_map.go:85
    pub(crate) fn get_script(&self, file_name: &str) -> Option<SourceMapScript> {
        let text = self.read_file(file_name)?;
        Some(SourceMapScript { file_name: file_name.to_string(), text })
    }

    // source_map.go:93
    pub(crate) fn try_get_source_position(
        &self,
        file_name: &str,
        position: TextPos,
    ) -> Option<DocumentPosition> {
        let new_pos = self.try_get_source_position_worker(file_name, position);
        if let Some(new_pos) = &new_pos {
            self.read_file(&new_pos.file_name)?;
        }
        new_pos
    }

    // source_map.go:106
    fn try_get_source_position_worker(
        &self,
        file_name: &str,
        position: TextPos,
    ) -> Option<DocumentPosition> {
        if !tspath::is_declaration_file_name(file_name) {
            return None;
        }

        let position = position.source_offset()?;
        let position_mapper = self.get_document_position_mapper(file_name);
        let document_pos = position_mapper.as_ref()?.get_source_position(&DocumentPosition {
            file_name: file_name.to_string(),
            pos: position,
        })?;
        if let Some(new_pos) =
            self.try_get_source_position_worker(&document_pos.file_name, document_pos.pos.into())
        {
            return Some(new_pos);
        }
        Some(document_pos)
    }

    // source_map.go:124
    pub(crate) fn try_get_generated_position(
        &self,
        file_name: &str,
        position: TextPos,
    ) -> Option<DocumentPosition> {
        let new_pos = self.try_get_generated_position_worker(file_name, position);
        if let Some(new_pos) = &new_pos {
            self.read_file(&new_pos.file_name)?;
        }
        new_pos
    }

    // source_map.go:137
    fn try_get_generated_position_worker(
        &self,
        file_name: &str,
        position: TextPos,
    ) -> Option<DocumentPosition> {
        if tspath::is_declaration_file_name(file_name) {
            return None;
        }

        let program = self.get_program();
        program.get_source_file(file_name)?;

        let path = self.to_path(file_name);
        // If this is source file of project reference source (instead of redirect) there is no generated position
        if program.is_source_from_project_reference(&path) {
            return None;
        }

        let position = position.source_offset()?;
        let declaration_file_name = outputpaths::get_output_declaration_file_name_worker(
            file_name,
            &program.options(),
            program,
        );
        let position_mapper = self.get_document_position_mapper(&declaration_file_name);
        let document_pos = position_mapper.as_ref()?.get_generated_position(&DocumentPosition {
            file_name: file_name.to_string(),
            pos: position,
        })?;
        if let Some(new_pos) =
            self.try_get_generated_position_worker(&document_pos.file_name, document_pos.pos.into())
        {
            return Some(new_pos);
        }
        Some(document_pos)
    }
}
