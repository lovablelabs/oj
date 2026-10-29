use std::ops::Deref;
use std::sync::{Arc, LazyLock, OnceLock};

use tsrs_core::P;
use tsrs_core::collections::{OrderedMap, SyncMap, new_ordered_map_with_size_hint};
use tsrs_core::semver;
use tsrs_core::tspath::{self, Path};
use tsrs_diagnostics as diagnostics;
use tsrs_diagnostics::Message;

use super::jsonvalue::JSONValueType;
use super::packagejson::Fields;

static TYPE_SCRIPT_VERSION: LazyLock<semver::Version> =
    LazyLock::new(|| semver::must_parse(tsrs_core::version()));

#[derive(Debug)]
pub struct PackageJson {
    pub fields: Fields,
    pub parseable: bool,
    version_paths: OnceLock<(VersionPaths, Vec<DiagnosticAndArgs>)>,
}

#[derive(Clone, Debug)]
struct DiagnosticAndArgs {
    message: &'static Message,
    args: Vec<String>,
}

impl Deref for PackageJson {
    type Target = Fields;
    fn deref(&self) -> &Fields {
        &self.fields
    }
}

impl PackageJson {
    pub fn new(fields: Fields, parseable: bool) -> PackageJson {
        PackageJson { fields, parseable, version_paths: OnceLock::new() }
    }

    // `trace` is Go's `func(m *diagnostics.Message, args ...any)`; arguments arrive pre-formatted.
    pub fn get_version_paths(
        &self,
        trace: Option<&mut dyn FnMut(&'static Message, &[String])>,
    ) -> &VersionPaths {
        let (version_paths, version_traces) =
            self.version_paths.get_or_init(|| self.compute_version_paths());
        if let Some(trace) = trace {
            for msg in version_traces {
                trace(msg.message, &msg.args);
            }
        }
        version_paths
    }

    fn compute_version_paths(&self) -> (VersionPaths, Vec<DiagnosticAndArgs>) {
        let mut version_traces = Vec::new();
        let mut trace = |message: &'static Message, args: Vec<String>| {
            version_traces.push(DiagnosticAndArgs { message, args });
        };
        let types_versions = &self.fields.types_versions;
        if types_versions.type_() == JSONValueType::NotPresent {
            trace(
                &diagnostics::X_PACKAGE_JSON_DOES_NOT_HAVE_A_0_FIELD,
                vec!["typesVersions".to_string()],
            );
            return (VersionPaths::default(), version_traces);
        }
        if types_versions.type_() != JSONValueType::Object {
            trace(
                &diagnostics::EXPECTED_TYPE_OF_0_FIELD_IN_PACKAGE_JSON_TO_BE_1_GOT_2,
                vec![
                    "typesVersions".to_string(),
                    "object".to_string(),
                    types_versions.type_().to_string(),
                ],
            );
            return (VersionPaths::default(), version_traces);
        }

        trace(
            &diagnostics::X_PACKAGE_JSON_HAS_A_TYPES_VERSIONS_FIELD_WITH_VERSION_SPECIFIC_PATH_MAPPINGS,
            vec!["typesVersions".to_string()],
        );

        for (key, value) in types_versions.as_object() {
            let Some(key_range) = semver::try_parse_version_range(key) else {
                trace(&diagnostics::X_PACKAGE_JSON_HAS_A_TYPES_VERSIONS_ENTRY_0_THAT_IS_NOT_A_VALID_SEMVER_RANGE, vec![key.clone()]);
                continue;
            };
            if key_range.test(&TYPE_SCRIPT_VERSION) {
                if value.type_() != JSONValueType::Object {
                    trace(
                        &diagnostics::EXPECTED_TYPE_OF_0_FIELD_IN_PACKAGE_JSON_TO_BE_1_GOT_2,
                        vec![
                            format!("typesVersions['{}']", key),
                            "object".to_string(),
                            value.type_().to_string(),
                        ],
                    );
                    return (VersionPaths::default(), version_traces);
                }
                let paths_json = value.as_object();
                let mut paths = new_ordered_map_with_size_hint(paths_json.len());
                for (key, value) in paths_json {
                    if value.type_() != JSONValueType::Array {
                        continue;
                    }
                    let mut slice = vec![String::new(); value.as_array().len()];
                    for (i, path) in value.as_array().iter().enumerate() {
                        if path.type_() != JSONValueType::String {
                            continue;
                        }
                        slice[i] = path.as_string().to_string();
                    }
                    paths.insert(key.clone(), slice);
                }
                return (VersionPaths { version: key.clone(), paths: Some(paths) }, version_traces);
            }
        }

        trace(
            &diagnostics::X_PACKAGE_JSON_DOES_NOT_HAVE_A_TYPES_VERSIONS_ENTRY_THAT_MATCHES_VERSION_0,
            vec![tsrs_core::version_major_minor().to_string()],
        );
        (VersionPaths::default(), version_traces)
    }
}

// Go keeps the raw `pathsJSON` object and converts it on the first `GetPaths` call; the conversion
// is done up front here.
#[derive(Clone, Debug, Default)]
pub struct VersionPaths {
    pub(crate) version: String,
    paths: Option<OrderedMap<String, Vec<String>>>,
}

impl VersionPaths {
    pub(crate) fn exists(&self) -> bool {
        !self.version.is_empty() && self.paths.is_some()
    }

    pub fn get_paths(&self) -> Option<&OrderedMap<String, Vec<String>>> {
        if !self.exists() {
            return None;
        }
        self.paths.as_ref()
    }
}

#[derive(Debug)]
pub struct InfoCacheEntry {
    pub package_directory: &'static str,
    pub directory_exists: bool,
    pub contents: Option<P<PackageJson>>,
}

// Go's nil-receiver methods on `*InfoCacheEntry`.
pub trait InfoCacheEntryExt {
    fn exists(&self) -> bool;
    fn get_contents(&self) -> Option<P<PackageJson>>;
    fn get_directory(&self) -> &'static str;
}

impl InfoCacheEntryExt for Option<P<InfoCacheEntry>> {
    fn exists(&self) -> bool {
        matches!(self, Some(p) if p.contents.is_some())
    }

    fn get_contents(&self) -> Option<P<PackageJson>> {
        self.and_then(|p| p.contents)
    }

    fn get_directory(&self) -> &'static str {
        match self {
            Some(p) => p.package_directory,
            None => "",
        }
    }
}

impl InfoCacheEntry {
    pub fn exists(&self) -> bool {
        self.contents.is_some()
    }

    pub fn get_contents(&self) -> Option<P<PackageJson>> {
        self.contents
    }
}

// WithPackageDirectory returns an entry whose PackageDirectory matches the
// caller's value. The package.json info cache is keyed by the canonical
// path of the package.json file, but multiple callers may look up the same
// package.json using directory paths that differ only by a trailing
// separator (e.g. "node_modules/preact/compat" vs
// "node_modules/preact/compat/"). Because the cache uses first-writer-wins
// semantics, a later caller may receive an entry whose PackageDirectory
// doesn't match its own candidate path. Downstream code compares the
// candidate against PackageDirectory, so we must return a corrected
// shallow copy when they diverge.
// See https://github.com/microsoft/TypeScript/pull/50740.
pub(crate) fn with_package_directory(
    p: P<InfoCacheEntry>,
    package_directory: &str,
) -> P<InfoCacheEntry> {
    if p.package_directory == package_directory {
        return p;
    }
    P::new(InfoCacheEntry {
        package_directory: tsrs_core::alloc_str(package_directory),
        directory_exists: p.directory_exists,
        contents: p.contents,
    })
}

#[derive(Debug)]
pub struct InfoCache {
    cache: SyncMap<Path, P<InfoCacheEntry>>,
    current_directory: String,
    use_case_sensitive_file_names: bool,
    // Clones copy entry pointers, so they keep the entries' allocation region alive together. The region is entered
    // only to allocate an entry, never while reading files or waiting for loader jobs on another thread.
    entries_region: Arc<OnceLock<tsrs_core::arena::Region>>,
}

pub(crate) fn new_info_cache(
    current_directory: &str,
    use_case_sensitive_file_names: bool,
) -> InfoCache {
    InfoCache {
        cache: SyncMap::default(),
        current_directory: current_directory.to_string(),
        use_case_sensitive_file_names,
        entries_region: Arc::default(),
    }
}

impl InfoCache {
    pub(crate) fn get_or_insert_with(
        &self,
        package_json_path: &str,
        create: impl FnOnce() -> InfoCacheEntry,
    ) -> P<InfoCacheEntry> {
        let region = self.entries_region.get_or_init(|| tsrs_core::arena::Region::new(4 << 10));
        let _scope = region.enter();
        // The file was read and parsed before taking the region lock. Another worker may have filled the cache;
        // discard that worker's losing parsed value before allocating it in the arena.
        if let Some(existing) = self.get(package_json_path) {
            return existing;
        }
        self.set(package_json_path, P::new(create()))
    }

    #[must_use]
    pub(crate) fn clone_cache(&self) -> InfoCache {
        let mut clone = new_info_cache(&self.current_directory, self.use_case_sensitive_file_names);
        clone.entries_region = Arc::clone(&self.entries_region);
        self.cache.range(|key, value| {
            clone.cache.store(key.clone(), *value);
            true
        });
        clone
    }

    pub(crate) fn get(&self, package_json_path: &str) -> Option<P<InfoCacheEntry>> {
        // The key's text without the `Path` (an `Arc` per lookup; most lookups hit): borrowed from the argument
        // on a case-sensitive host, the lowercase copy elsewhere.
        let key = tspath::to_path_cow(
            package_json_path,
            &self.current_directory,
            self.use_case_sensitive_file_names,
        );
        self.cache.load::<str>(&key)
    }

    fn set(&self, package_json_path: &str, info: P<InfoCacheEntry>) -> P<InfoCacheEntry> {
        let key = tspath::to_path(
            package_json_path,
            &self.current_directory,
            self.use_case_sensitive_file_names,
        );
        let (actual, _) = self.cache.load_or_store(key, info);
        actual
    }

    pub(crate) fn range(&self, f: impl FnMut(&Path, &P<InfoCacheEntry>) -> bool) {
        self.cache.range(f);
    }
}
