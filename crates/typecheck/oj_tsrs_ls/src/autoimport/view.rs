use std::cell::OnceCell;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::Arc;

use rustc_hash::FxHashMap;
use tsrs_ast::SourceFile;
use tsrs_checker::Checker;
use tsrs_compiler::Program;
use tsrs_core::collections::{Set, new_set_from_items};
use tsrs_core::tspath::Path;
use tsrs_core::{LanguageVariant, P, Tristate};
use tsrs_lsproto as lsproto;
use tsrs_modulespecifiers::{self as modulespecifiers, ModuleSpecifierEnding};
use tsrs_scanner as scanner;

use super::export::{Export, ExportID, ModuleID};
use super::fix::{ExistingImport, Fix};
use super::registry::{ProjectID, Registry, RegistryBucket};
use super::util::add_package_json_dependencies;
use crate::lsutil;

// view.go:21
pub struct View {
    registry: Option<Arc<Registry>>,
    pub(crate) importing_file: P<SourceFile>,
    pub(crate) importing_file_path: Path,
    pub(crate) program: &'static Program,
    // Go shares the `*checker.Checker` between the caller, the view and the import adder built from it. The caller
    // holds the checker's pool lease for the whole request and keeps using it while the view is alive, so the view
    // keeps the checker's address (see `checker()`).
    checker: NonNull<Checker>,
    pub(crate) preferences: modulespecifiers::UserPreferences,
    project_id: ProjectID,

    allowed_endings: OnceCell<Vec<ModuleSpecifierEnding>>,
    pub(crate) conditions: Set<String>,
    pub(crate) should_use_uri_style_node_core_modules: Tristate,
    existing_imports: OnceCell<Rc<FxHashMap<ModuleID, Vec<ExistingImport>>>>,
    should_use_require_for_fixes: OnceCell<bool>,
}

// view.go:37
pub fn new_view(
    registry: Option<Arc<Registry>>,
    importing_file: P<SourceFile>,
    project_id: ProjectID,
    program: &'static Program,
    type_checker: &mut Checker,
    preferences: modulespecifiers::UserPreferences,
) -> View {
    let mut importing_file_path = importing_file.path().clone();
    if let Some(canonical) = importing_file.canonical_source_file() {
        importing_file_path = canonical.path().clone();
    }
    View {
        registry,
        importing_file,
        importing_file_path,
        program,
        checker: NonNull::from(type_checker),
        project_id,
        preferences,
        conditions: new_set_from_items(tsrs_module::get_conditions(
            &program.options(),
            program.get_default_resolution_mode_for_file(importing_file),
        )),
        should_use_uri_style_node_core_modules: lsutil::should_use_uri_style_node_core_modules(
            importing_file,
            program,
        ),
        allowed_endings: OnceCell::new(),
        existing_imports: OnceCell::new(),
        should_use_require_for_fixes: OnceCell::new(),
    }
}

impl View {
    // Go's `v.checker`.
    #[expect(
        clippy::mut_from_ref,
        reason = "Go shares the checker pointer; the SAFETY comment states the one-user-at-a-time contract"
    )]
    pub(crate) fn checker(&self) -> &mut Checker {
        // SAFETY: the checker outlives the view (the caller's pool lease covers the request that built the view),
        // and the view, the import adder and the caller use it on one thread, one call at a time, exactly as Go
        // shares the pointer: no other reference to the checker is in use while the returned one is.
        unsafe { &mut *self.checker.as_ptr() }
    }

    pub(crate) fn registry(&self) -> &Registry {
        // Go dereferences the registry pointer; a view without a registry is never searched.
        self.registry.as_deref().expect("nil registry")
    }

    // view.go:60
    pub(crate) fn get_allowed_endings(&self) -> &[ModuleSpecifierEnding] {
        self.allowed_endings.get_or_init(|| {
            let resolution_mode =
                self.program.get_default_resolution_mode_for_file(self.importing_file);
            modulespecifiers::get_allowed_endings_in_preferred_order(
                &self.preferences,
                self.program,
                &self.program.options(),
                self.importing_file,
                "",
                resolution_mode,
            )
        })
    }

    // view.go:83
    pub(crate) fn search(&self, query: &str, kind: QueryKind) -> Vec<Arc<Export>> {
        let search_fn = |bucket: &RegistryBucket| -> Vec<Arc<Export>> {
            let Some(index) = &bucket.index else {
                return Vec::new();
            };
            match kind {
                QueryKind::WordPrefix => index.search_word_prefix(query),
                QueryKind::ExactMatch => index.find(query, true),
                QueryKind::CaseInsensitiveMatch => index.find(query, false),
            }
        };

        self.search_worker(&search_fn)
    }

    // view.go:100
    pub(crate) fn search_by_export_id(&self, id: &ExportID) -> Vec<Arc<Export>> {
        let search = |bucket: &RegistryBucket| -> Vec<Arc<Export>> {
            let Some(index) = &bucket.index else {
                return Vec::new();
            };
            index.entries.iter().filter(|e| e.export_id == *id).cloned().collect()
        };

        self.search_worker(&search)
    }

    // view.go:110
    fn search_worker(
        &self,
        search_fn: &dyn Fn(&RegistryBucket) -> Vec<Arc<Export>>,
    ) -> Vec<Arc<Export>> {
        let mut results: Vec<Arc<Export>> = Vec::new();
        let registry = self.registry();

        if let Some(bucket) = registry.projects.get(&self.project_id) {
            let exports = search_fn(bucket);
            results.reserve(exports.len());
            results.extend(exports.into_iter().filter_map(|e| {
                if e.module_id.as_file() == Some(self.importing_file.path().as_str()) {
                    // Don't auto-import from the importing file itself
                    return None;
                }
                Some(e)
            }));
        }

        // Compute the set of packages accessible to the importing file.
        // This includes packages from package.json dependencies (aggregated from ancestor directories)
        // plus packages that are directly imported by the project's program files.
        // If no package.json is found, allowedPackages remains nil and all packages are allowed.
        let mut allowed_packages: Option<Set<String>> = None;
        self.importing_file.path().get_directory_path().for_each_ancestor_directory(
            |dir_path| -> Option<()> {
                if let Some(dir) = registry.directories.get(dir_path) {
                    let pj = dir.package_json;
                    if pj.exists() && pj.contents.unwrap().parseable {
                        // Initialize to empty set if this is the first package.json we've seen
                        let allowed = allowed_packages.get_or_insert_with(Set::new);
                        add_package_json_dependencies(&pj.contents.unwrap(), allowed);
                    }
                }
                None
            },
        );
        // If we found at least one package.json, also include packages directly imported by the project
        if let Some(allowed) = &mut allowed_packages
            && let Some(bucket) = registry.projects.get(&self.project_id)
            && let Some(resolved) = &bucket.resolved_package_names
        {
            allowed.union(resolved);
        }

        let mut exclude_packages: Set<String> = Set::new();
        self.importing_file.path().get_directory_path().for_each_ancestor_directory(
            |dir_path| -> Option<()> {
                if let Some(node_modules_bucket) = registry.node_modules.get(dir_path) {
                    let exports = search_fn(node_modules_bucket);
                    results.reserve(exports.len());
                    results.extend(exports.into_iter().filter_map(|e| {
                        // Exclude packages found in lower node_modules (shadowing)
                        if exclude_packages.has(&e.package_name) {
                            return None;
                        }
                        // If allowedPackages is nil, no package.json was found, so include all packages.
                        // Otherwise, only include packages that are dependencies or directly imported.
                        if let Some(allowed) = &allowed_packages
                            && !allowed.has(&e.package_name)
                        {
                            return None;
                        }
                        Some(e)
                    }));

                    // As we go up the directory tree, exclude packages found in lower node_modules
                    if let Some(package_files) = &node_modules_bucket.package_files {
                        // pure set inserts; Go ranges the map too
                        exclude_packages
                            .m
                            .extend(package_files.keys().map(std::clone::Clone::clone));
                    }
                }
                None
            },
        );
        results
    }

    // view.go:180
    pub(crate) fn get_completions(
        &self,
        prefix: &str,
        position: lsproto::Position,
        for_jsx: bool,
        is_type_only_location: bool,
    ) -> Vec<FixAndExport> {
        #[derive(Clone, PartialEq, Eq, Hash)]
        struct ExportGroupKey {
            target: ExportID,
            name: String,
            ambient_module_or_package_name: String,
        }

        let results = self.search(prefix, QueryKind::WordPrefix);

        // Go map (random iteration order); groups are kept in first-seen order.
        let mut grouped: indexmap::IndexMap<
            ExportGroupKey,
            Vec<Arc<Export>>,
            rustc_hash::FxBuildHasher,
        > = indexmap::IndexMap::default();
        'outer: for e in results {
            let name = e.name().to_string();
            if !scanner::is_identifier_text(&name, LanguageVariant::Standard) {
                continue;
            }
            if for_jsx
                && !(tsrs_core::stringutil::unicode_is_upper(u32::from(name.as_bytes()[0]))
                    || e.is_renameable())
            {
                continue;
            }
            let mut target = e.export_id.clone();
            if e.target != ExportID::default() {
                target = e.target.clone();
            }
            let ambient = e.ambient_module_name();
            let mut key = ExportGroupKey {
                target,
                name,
                ambient_module_or_package_name: if ambient.is_empty() {
                    e.package_name.clone()
                } else {
                    ambient.to_string()
                },
            };
            if (e.package_name == "@types/node" || e.path.contains("/node_modules/@types/node/"))
                && tsrs_core::is_unprefixed_node_core_module(&key.ambient_module_or_package_name)
            {
                // Group URI-style and non-URI style node core modules together so the ranking logic
                // is allowed to drop one if an explicit preference is detected.
                key.ambient_module_or_package_name =
                    format!("node:{}", key.ambient_module_or_package_name);
            }
            if let Some(existing) = grouped.get_mut(&key) {
                for element in existing.iter_mut() {
                    let ex = Arc::clone(&(*element));
                    if e.export_id == ex.export_id {
                        (*element) = Arc::new(Export {
                            export_id: e.export_id.clone(),
                            module_file_name: e.module_file_name.clone(),
                            unresolved_module_specifier: e.unresolved_module_specifier.clone(),
                            package_name: e.package_name.clone(),
                            is_type_only: e.is_type_only || ex.is_type_only,
                            syntax: e.syntax.min(ex.syntax),
                            flags: e.flags | ex.flags,
                            script_element_kind: e.script_element_kind.min(ex.script_element_kind),
                            script_element_kind_modifiers: e.script_element_kind_modifiers
                                | ex.script_element_kind_modifiers,
                            local_name: e.local_name.clone(),
                            target: e.target.clone(),
                            path: e.path.clone(),
                            ..Default::default()
                        });
                        continue 'outer;
                    }
                }
            }
            grouped.entry(key).or_default().push(e);
        }

        let mut fixes: Vec<FixAndExport> = Vec::new();
        let compare_fixes =
            |a: &FixAndExport, b: &FixAndExport| self.compare_fixes_for_ranking(&a.fix, &b.fix);

        for exps in grouped.values() {
            let mut fixes_for_group: Vec<FixAndExport> = Vec::with_capacity(exps.len());
            fixes_for_group.extend(exps.iter().flat_map(|e| {
                self.get_fixes(e, for_jsx, is_type_only_location, Some(position))
                    .into_iter()
                    .map(move |fix| FixAndExport { fix, export: Arc::clone(e) })
            }));
            fixes.extend(tsrs_core::min_all_func(&fixes_for_group, compare_fixes));
        }

        // The client will do additional sorting by SortText and Label, so we don't
        // need to consider the name in our sorting here; we only need to produce a
        // stable relative ordering between completions that the client will consider
        // equivalent.
        tsrs_core::goslices::sort_func(&mut fixes, |a, b| {
            self.compare_fixes_for_sorting(&a.fix, &b.fix)
        });

        fixes
    }

    pub(crate) fn existing_imports_cell(
        &self,
    ) -> &OnceCell<Rc<FxHashMap<ModuleID, Vec<ExistingImport>>>> {
        &self.existing_imports
    }

    pub(crate) fn should_use_require_cell(&self) -> &OnceCell<bool> {
        &self.should_use_require_for_fixes
    }
}

// view.go:74
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QueryKind {
    WordPrefix,
    ExactMatch,
    CaseInsensitiveMatch,
}

// view.go:175
#[derive(Clone)]
pub struct FixAndExport {
    pub(crate) fix: Arc<Fix>,
    pub(crate) export: Arc<Export>,
}
