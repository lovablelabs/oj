use tsrs_ast::{ContentMapperSourceFileInfo, SourceFile, SourceFileParseOptions};
use tsrs_core::{P, alloc_slice, alloc_str, get_script_kind_from_file_name, tspath};
use tsrs_parser::parse_source_file_owned;
use tsrs_tsoptions::{Mapper, is_supported_virtual_extension};

use crate::host::{
    Error, Project, Request, SupplementalFileCollisionError, TransformErrorKind,
    TransformResultFiles, new_transform_error,
};

// SourceFiles is the canonical output and its unnamed supplemental compiler inputs.
// transform.go:14 (Go's zero SourceFiles, which a host returns for a file it cannot read, has a nil Canonical.)
#[derive(Clone, Debug, Default)]
pub struct SourceFiles {
    pub canonical: Option<P<SourceFile>>,
    pub supplemental: Vec<P<SourceFile>>,
}

// TransformAndParse runs the given content mapper's transform for a content-mapped source file and
// parses the resulting TypeScript, preserving the original file name and retaining the untransformed text
// on the source file. The mapper is supplied by the caller (which also owns the failure accounting) so it
// is neither re-resolved nor substituted here. It returns an error if the transform fails or the mapper
// produces invalid position mappings (a *spanmap.MappingError); the caller decides how to report the failure
// and what placeholder file to substitute. It is the shared implementation behind
// CompilerHost.GetContentMappedSourceFile.
// transform.go:26
pub fn transform_and_parse(
    parse_options: SourceFileParseOptions,
    content: &str,
    mapper: &Mapper,
    project: &dyn Project,
) -> Result<SourceFiles, Error> {
    let transform_identity = match project.identity(mapper) {
        Ok(transform_identity) => transform_identity,
        Err(err) => return Err(new_transform_error(TransformErrorKind::Project, Some(err)).into()),
    };
    let result =
        project.transform(mapper, Request { file_name: &parse_options.file_name, content })?;
    parse_result(parse_options, content, mapper, &transform_identity, result)
}

// ParseResult validates and parses one mapper result and all its supplemental outputs.
// transform.go:47 (The source files' content-mapper strings and slices are arena data, like the files.)
pub fn parse_result(
    parse_options: SourceFileParseOptions,
    content: &str,
    mapper: &Mapper,
    transform_identity: &str,
    result: TransformResultFiles,
) -> Result<SourceFiles, Error> {
    let Some(mappings) = result.mappings else {
        return Err(new_transform_error(TransformErrorKind::Mappings, None).into());
    };
    if let Some(problem) = mappings.validate(&result.text, content) {
        return Err(problem.into());
    }
    let virtual_extension = result.virtual_extension;
    if !is_supported_virtual_extension(&virtual_extension) {
        return Err(new_transform_error(TransformErrorKind::Response, None).into());
    }
    let base_parse_options = parse_options;
    let virtual_file_name = format!("{}{}", base_parse_options.file_name, virtual_extension);
    let mut parse_options = base_parse_options.clone();
    if is_module_virtual_extension(&virtual_extension) {
        parse_options.external_module_indicator_options.force = true;
    }
    let canonical_path = parse_options.path.clone();
    let source_file = parse_source_file_owned(
        parse_options,
        result.text,
        get_script_kind_from_file_name(&virtual_file_name),
    );
    if !result.diagnostics.is_empty() {
        // The runner produces diagnostics without a source file (it doesn't have one yet); associate
        // them with the file now so they are reported against it.
        for diagnostic in &result.diagnostics {
            diagnostic.set_file(Some(source_file));
        }
        let mut diagnostics = source_file.diagnostics().to_vec();
        diagnostics.extend_from_slice(&result.diagnostics);
        source_file.set_diagnostics(&diagnostics);
    }
    let mut files = SourceFiles {
        canonical: Some(source_file),
        supplemental: Vec::with_capacity(result.supplemental.len()),
    };
    // Go reads result.Supplemental[i] again below; the texts went to the parser, so the rest is kept here.
    let mut supplemental_mappings = Vec::with_capacity(result.supplemental.len());
    for (i, supplemental) in result.supplemental.into_iter().enumerate() {
        let Some(mappings) = supplemental.mappings else {
            return Err(new_transform_error(TransformErrorKind::Mappings, None).into());
        };
        if let Some(problem) = mappings.validate(&supplemental.text, content) {
            return Err(problem.into());
        }
        let mut supplemental_options = base_parse_options.clone();
        if !is_supported_virtual_extension(&supplemental.virtual_extension) {
            return Err(new_transform_error(TransformErrorKind::Response, None).into());
        }
        let suffix = format!(".{i}{}", supplemental.virtual_extension);
        supplemental_options.file_name.push_str(&suffix);
        supplemental_options.path = tspath::Path::new(format!("{}{suffix}", &*canonical_path));
        if is_module_virtual_extension(&supplemental.virtual_extension) {
            supplemental_options.external_module_indicator_options.force = true;
        }

        let script_kind = get_script_kind_from_file_name(&supplemental_options.file_name);
        let file = parse_source_file_owned(supplemental_options, supplemental.text, script_kind);

        files.supplemental.push(file);
        supplemental_mappings.push((mappings, supplemental.diagnostic_directives));
    }
    let mapper_identity = alloc_str(&mapper.identity());
    let transform_identity = alloc_str(transform_identity);
    let original_text = alloc_str(content);
    source_file.set_content_mapper_info(ContentMapperSourceFileInfo {
        content_mapper: mapper_identity,
        transform_identity,
        parse_options: base_parse_options.clone(),
        virtual_file_name: alloc_str(&virtual_file_name),
        original_text,
        span_map: Some(mappings),
        diagnostic_directives: alloc_slice(&result.diagnostic_directives),
        supplemental_source_files: alloc_slice(&files.supplemental),
        canonical_source_file: None,
    });
    for (&file, (span_map, diagnostic_directives)) in
        files.supplemental.iter().zip(supplemental_mappings)
    {
        file.set_content_mapper_info(ContentMapperSourceFileInfo {
            content_mapper: mapper_identity,
            transform_identity,
            parse_options: base_parse_options.clone(),
            virtual_file_name: file.get().file_name(),
            original_text,
            span_map: Some(span_map),
            diagnostic_directives: alloc_slice(&diagnostic_directives),
            supplemental_source_files: &[],
            canonical_source_file: Some(source_file),
        });
    }
    Ok(files)
}

// transform.go:123
fn is_module_virtual_extension(extension: &str) -> bool {
    [tspath::EXTENSION_MTS, tspath::EXTENSION_CTS, tspath::EXTENSION_MJS, tspath::EXTENSION_CJS]
        .contains(&extension)
}

// CheckSupplementalFileNameCollisions rejects compiler-assigned virtual filenames that name physical files.
// transform.go:128
pub fn check_supplemental_file_name_collisions(
    files: &SourceFiles,
    mut file_exists: impl FnMut(&str) -> bool,
) -> Result<(), Error> {
    if let Some(result) = files.supplemental.iter().find_map(|file| {
        if file_exists(file.file_name()) {
            Some(Err(
                SupplementalFileCollisionError { file_name: file.file_name().to_string() }.into()
            ))
        } else {
            None
        }
    }) {
        return result;
    }
    Ok(())
}
