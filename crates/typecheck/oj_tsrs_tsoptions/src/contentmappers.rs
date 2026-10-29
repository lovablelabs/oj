// The definitions half of Go `internal/contentmapper` (contentmapper.go, and OptionPathSegment from host.go) and
// tsoptions/contentmappers.go. Go's tsoptions imports contentmapper; here the definitions live in tsoptions and the
// mapper host crate imports them (notes/contentmappers.md, "Crate layout").
//
// A mapper is declared in tsconfig (Definition), its implementation is described by fields in its npm
// package's package.json (Manifest), and the two are combined once the package is resolved (Mapper).

use rustc_hash::FxHashMap;
use tsrs_ast::{Diagnostic, new_compiler_diagnostic};
use tsrs_core::collections::{OrderedMap, new_ordered_map_with_size_hint};
use tsrs_core::json;
use tsrs_core::tspath;
use tsrs_core::{CompilerOptions, ModuleResolutionKind, P, RESOLUTION_MODE_NONE};
use tsrs_diagnostics as diagnostics;
use tsrs_module::packagejson;

use crate::gojson;
use crate::tsconfigparsing::{ParseConfigHost, ResolverHost};

// Definition is a content mapper as declared in a tsconfig's "contentMappers": the npm package that
// implements the mapper and the otherwise unsupported file extensions it registers.
// contentmapper.go:30
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Definition {
    pub package: String,
    pub extensions: Vec<String>,
    pub output_extensions: FxHashMap<String, String>,
    // Go distinguishes a nil map from an explicitly empty map in Mapper.Equals.
    pub output_extensions_present: bool,
    // json.Value: the raw JSON text, empty when absent.
    pub options: String,
}

// Manifest is the content-mapper information read from a package's package.json: its name and version
// (which form the mapper's identity), the argv used to run it, and the compiler options it declares it
// depends on.
// contentmapper.go:39
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub exec: Vec<String>,
    pub compiler_options: Vec<String>,
    pub dynamic_config: bool,
    pub default_output_extensions: FxHashMap<String, String>,
    pub default_output_extensions_present: bool,
}

// Mapper is a resolved content mapper: its tsconfig Definition combined with the Manifest resolved from
// the package's package.json, plus the package directory used as the mapper's working directory.
// contentmapper.go:49
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mapper {
    pub definition: Definition,
    pub manifest: Manifest,
    // PackageDirectory is the real path directory returned by package resolution for package-based mappers.
    pub package_directory: String,
    // ContributionID is provided by an LSP client extension for inferred project content mappers.
    pub contribution_id: String,
}

// contentmapper.go:58
const SUPPORTED_VIRTUAL_EXTENSIONS: [&str; 9] =
    [".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts", ".json"];

// contentmapper.go:62
pub fn is_supported_virtual_extension(extension: &str) -> bool {
    SUPPORTED_VIRTUAL_EXTENSIONS.contains(&extension)
}

pub fn is_valid_extension(extension: &str) -> bool {
    extension.len() > 1
        && extension.starts_with('.')
        && !extension.contains('/')
        && !extension.contains('\\')
}

pub fn has_reserved_source_extension(extension: &str) -> bool {
    tspath::ALL_SUPPORTED_EXTENSIONS_WITH_JSON.iter().flat_map(|group| group.iter().copied()).any(
        |native| {
            extension.len() >= native.len()
                && extension
                    .get(extension.len() - native.len()..)
                    .is_some_and(|suffix| tsrs_core::stringutil::equal_fold(suffix, native))
        },
    )
}

impl Mapper {
    // DiagnosticName returns the best available user-facing name, including when manifest resolution failed.
    // contentmapper.go:67
    pub fn diagnostic_name(&self) -> &str {
        if !self.manifest.name.is_empty() {
            return &self.manifest.name;
        }
        if !self.definition.package.is_empty() {
            return &self.definition.package;
        }
        &self.contribution_id
    }

    // Identity returns the mapper's "name@version" identity, or just the name when it declares no version,
    // or an empty string when the mapper has not been resolved to a name.
    // contentmapper.go:80
    pub fn identity(&self) -> String {
        if !self.contribution_id.is_empty() {
            return format!("{} ({})", self.contribution_id, self.manifest_identity());
        }
        self.manifest_identity()
    }

    // contentmapper.go:87
    fn manifest_identity(&self) -> String {
        if self.manifest.name.is_empty() {
            return String::new();
        }
        if self.manifest.version.is_empty() {
            return self.manifest.name.clone();
        }
        format!("{}@{}", self.manifest.name, self.manifest.version)
    }

    // TransformIdentity fingerprints the mapper's identity, declared compiler options, and runtime output
    // contract so both transformed-file caches and incremental emit are invalidated when these change.
    // contentmapper.go:104 (Go `xxh3.Uint128`: `Hi` is the high 64 bits, so `Bytes()` is `to_be_bytes()`.)
    pub fn transform_identity(&self, options: Option<&CompilerOptions>) -> u128 {
        // Go discards the error, which marshaling these option types never returns.
        let declared = self.marshal_declared_options(options).unwrap_or_default();
        // Go `json.Marshal(declared)`: an object in declared order holding the raw JSON values as marshaled.
        let mut options_json = String::from("{");
        for (i, (name, raw)) in declared.iter().enumerate() {
            if i > 0 {
                options_json.push(',');
            }
            json::write_compact_string(&mut options_json, name);
            options_json.push(':');
            options_json.push_str(raw);
        }
        options_json.push('}');
        let mut output_extensions_json = String::new();
        if !self.definition.output_extensions.is_empty() {
            let mut entries: Vec<_> = self.definition.output_extensions.iter().collect();
            entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
            output_extensions_json.push('{');
            for (i, (source, target)) in entries.into_iter().enumerate() {
                if i > 0 {
                    output_extensions_json.push(',');
                }
                json::write_compact_string(&mut output_extensions_json, source);
                output_extensions_json.push(':');
                json::write_compact_string(&mut output_extensions_json, target);
            }
            output_extensions_json.push('}');
        }
        let identity = self.identity();
        let mut buf = Vec::with_capacity(
            identity.len()
                + 3
                + self.definition.options.len()
                + options_json.len()
                + output_extensions_json.len(),
        );
        buf.extend_from_slice(identity.as_bytes());
        buf.push(0);
        buf.extend_from_slice(self.definition.options.as_bytes());
        buf.push(0);
        buf.extend_from_slice(options_json.as_bytes());
        if !output_extensions_json.is_empty() {
            buf.push(0);
            buf.extend_from_slice(output_extensions_json.as_bytes());
        }
        xxhash_rust::xxh3::xxh3_128(&buf)
    }

    // MarshalDeclaredOptions marshals just the compiler options this mapper declared it depends on, in the
    // declared order, skipping any that are unset. Marshaling only the declared fields avoids serializing the
    // whole CompilerOptions when a mapper depends on few options (or none).
    // contentmapper.go:119 (Go finds each field by its json tag through reflection, `compilerOptionFields`;
    // `gojson::compiler_option_to_go_json` does that lookup and the marshaling. The values are raw JSON text.)
    pub fn marshal_declared_options(
        &self,
        options: Option<&CompilerOptions>,
    ) -> Result<OrderedMap<String, String>, String> {
        let mut out = new_ordered_map_with_size_hint(self.manifest.compiler_options.len());
        let Some(options) = options else {
            return Ok(out);
        };
        if self.manifest.compiler_options.is_empty() {
            return Ok(out);
        }
        out = self.manifest.compiler_options.iter().try_fold(out, |mut out, name| {
            let Some(field) = gojson::compiler_option_to_go_json(options, name) else {
                return Ok(out);
            };
            let Some(value) = field else {
                return Ok(out);
            };
            let raw = json::marshal(&value)?;
            out.insert(name.clone(), raw);
            Ok::<_, String>(out)
        })?;
        Ok(out)
    }
}

// host.go:209
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OptionPathSegment {
    pub property: String,
    pub index: usize,
    pub is_index: bool,
}

// resolveContentMapperManifest locates packageName in node_modules (walking up from the directory of
// containingFile via node module resolution) and reads its package.json to produce the mapper's manifest
// and package directory. It never executes the package. On failure it returns a diagnostic describing why
// the mapper could not be resolved; on success the diagnostic is nil.
// contentmappers.go:17
pub fn resolve_content_mapper_manifest(
    host: &dyn ParseConfigHost,
    containing_file: &str,
    package_name: &str,
) -> (Manifest, String, Option<P<Diagnostic>>) {
    let resolver_host = ResolverHost { host };
    let resolver = tsrs_module::new_resolver(tsrs_module::ResolverOptions::new(
        &resolver_host,
        P::new(CompilerOptions {
            module_resolution: ModuleResolutionKind::BUNDLER,
            ..Default::default()
        }),
    ));
    let resolved = resolver.resolve_package_directory(
        package_name,
        containing_file,
        RESOLUTION_MODE_NONE,
        None,
    );
    let Some(resolved) = resolved.filter(|resolved| !resolved.resolved_file_name.is_empty()) else {
        return (
            Manifest::default(),
            String::new(),
            Some(new_compiler_diagnostic(
                &diagnostics::THE_CONTENT_MAPPER_PACKAGE_0_COULD_NOT_BE_RESOLVED,
                &[&package_name],
            )),
        );
    };
    let package_directory = resolved.resolved_file_name.to_string();

    let package_json_path = tspath::combine_paths(&package_directory, &["package.json"]);
    let Some(contents) = host.fs().read_file(&package_json_path) else {
        return (
            Manifest::default(),
            package_directory,
            Some(new_compiler_diagnostic(
                &diagnostics::THE_CONTENT_MAPPER_PACKAGE_0_COULD_NOT_BE_RESOLVED,
                &[&package_name],
            )),
        );
    };
    let Ok(fields) = packagejson::parse(&contents) else {
        return (
            Manifest::default(),
            package_directory,
            Some(new_compiler_diagnostic(
                &diagnostics::THE_PACKAGE_JSON_OF_THE_CONTENT_MAPPER_PACKAGE_0_COULD_NOT_BE_PARSED,
                &[&package_name],
            )),
        );
    };
    // Go `name, _ := fields.Name.GetValue()`: the decoded value whether or not it was valid.
    let name = fields.name.value.clone();
    if name.is_empty() {
        return (
            Manifest::default(),
            package_directory,
            Some(new_compiler_diagnostic(
                &diagnostics::THE_PACKAGE_JSON_OF_THE_CONTENT_MAPPER_PACKAGE_0_DOES_NOT_SPECIFY_A_NAME,
                &[&package_name],
            )),
        );
    }
    let version = fields.version.value.clone();

    // A content mapper package must declare how to run it: a "typescript.contentMapper" object with a non-empty
    // "exec" array of strings.
    let Some(cm) = fields.content_mapper.get_value() else {
        return (
            Manifest::default(),
            package_directory,
            Some(new_compiler_diagnostic(
                &diagnostics::THE_PACKAGE_JSON_OF_THE_CONTENT_MAPPER_PACKAGE_0_DOES_NOT_DECLARE_A_TYPESCRIPT_CONTENT_MAPPER_OBJECT,
                &[&package_name],
            )),
        );
    };
    let exec = match cm.exec.get_value() {
        Some(exec) if !exec.is_empty() => exec.clone(),
        _ => {
            return (
                Manifest::default(),
                package_directory,
                Some(new_compiler_diagnostic(
                    &diagnostics::THE_TYPESCRIPT_CONTENT_MAPPER_EXEC_OF_THE_CONTENT_MAPPER_PACKAGE_0_MUST_BE_A_NON_EMPTY_ARRAY_OF_STRINGS,
                    &[&package_name],
                )),
            );
        }
    };
    let compiler_options = cm.compiler_options.value.clone();
    let dynamic_config = cm.dynamic_config.value;
    if cm.output_extensions.is_present() && !cm.output_extensions.is_valid() {
        return (
            Manifest::default(), package_directory,
            Some(new_compiler_diagnostic(
                &diagnostics::THE_TYPESCRIPT_CONTENT_MAPPER_OUTPUT_EXTENSIONS_OF_THE_CONTENT_MAPPER_PACKAGE_0_MUST_BE_AN_OBJECT_WITH_STRING_VALUES,
                &[&package_name],
            )),
        );
    }
    let output_extensions = cm.output_extensions.value.clone();
    let default_output_extensions_present = cm.output_extensions.is_present();
    let mut entries: Vec<_> = output_extensions.iter().collect();
    entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
    for (source, target) in entries {
        for extension in [source, target] {
            if extension.contains('/') || extension.contains('\\') {
                return (
                    Manifest::default(),
                    package_directory,
                    Some(new_compiler_diagnostic(
                        &diagnostics::CONTENT_MAPPER_EXTENSION_0_MUST_NOT_CONTAIN_PATH_SEPARATORS,
                        &[extension],
                    )),
                );
            }
        }
        if !is_valid_extension(source) || !is_valid_extension(target) {
            return (
                Manifest::default(), package_directory,
                Some(new_compiler_diagnostic(
                    &diagnostics::THE_TYPESCRIPT_CONTENT_MAPPER_OUTPUT_EXTENSIONS_OF_THE_CONTENT_MAPPER_PACKAGE_0_CONTAINS_AN_INVALID_MAPPING_FROM_1_TO_2_EXTENSIONS_MUST_BE_NON_EMPTY_AND_BEGIN_WITH_A,
                    &[&package_name, source, target],
                )),
            );
        }
    }
    (
        Manifest {
            name,
            version,
            exec,
            compiler_options,
            dynamic_config,
            default_output_extensions: output_extensions,
            default_output_extensions_present,
        },
        package_directory,
        None,
    )
}
