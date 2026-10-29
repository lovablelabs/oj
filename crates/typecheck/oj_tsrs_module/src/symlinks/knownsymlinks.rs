use tsrs_ast::SourceFile;
use tsrs_core::collections::{SyncMap, SyncSet};
use tsrs_core::tspath::{self, Path};
use tsrs_core::{P, ResolutionMode};

use crate::types::{ResolvedModule, ResolvedTypeReferenceDirective};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownDirectoryLink {
    /// Original spelling used to reach the symlink, with trailing separator.
    pub symlink: String,
    // Matches the casing returned by `realpath`. Used to compute the `realpath` of children.
    // Always has trailing directory separator
    pub real: String,
    // toPath(real). Stored to avoid repeated recomputation.
    // Always has trailing directory separator
    pub real_path: Path,
}

impl KnownDirectoryLink {
    pub fn resolve_file_name(
        &self,
        file_name: &str,
        use_case_sensitive_file_names: bool,
    ) -> Option<String> {
        let relative =
            tspath::trim_file_path_prefix(file_name, &self.symlink, use_case_sensitive_file_names)?;
        Some(format!("{}{relative}", self.real))
    }
}

#[derive(Debug, Default)]
pub struct KnownSymlinks {
    directories: SyncMap<Path, Option<P<KnownDirectoryLink>>>,
    directories_by_realpath: SyncMap<Path, P<SyncSet<String>>>,
    files: SyncMap<Path, String>,
    files_by_realpath: SyncMap<Path, P<SyncSet<String>>>,
    pub cwd: String,
    pub use_case_sensitive_file_names: bool,
}

impl KnownSymlinks {
    pub fn has_directory(&self, symlink_path: &Path) -> bool {
        self.directories.load(&symlink_path.ensure_trailing_directory_separator()).is_some()
    }

    // Gets a map from symlink to realpath. Keys have trailing directory separators.
    pub fn directories(&self) -> &SyncMap<Path, Option<P<KnownDirectoryLink>>> {
        &self.directories
    }

    pub fn directories_by_realpath(&self) -> &SyncMap<Path, P<SyncSet<String>>> {
        &self.directories_by_realpath
    }

    // Gets a map from symlink to realpath
    pub fn files(&self) -> &SyncMap<Path, String> {
        &self.files
    }

    // Gets a map from realpath to symlinks
    pub fn files_by_realpath(&self) -> &SyncMap<Path, P<SyncSet<String>>> {
        &self.files_by_realpath
    }

    pub fn set_directory(
        &self,
        symlink: &str,
        symlink_path: Path,
        real_directory: Option<P<KnownDirectoryLink>>,
    ) {
        let real_directory = real_directory.map(|real_directory| {
            let mut link = (*real_directory).clone();
            link.symlink = tspath::ensure_trailing_directory_separator(symlink);
            P::new(link)
        });
        if let Some(real_directory) = real_directory
            && self.directories.load(&symlink_path).is_none()
        {
            let (set, _) = self
                .directories_by_realpath
                .load_or_store(real_directory.real_path.clone(), P::new(SyncSet::default()));
            set.add(symlink.to_string());
        }
        self.directories.store(symlink_path, real_directory);
    }

    pub fn set_file(&self, symlink: &str, symlink_path: Path, realpath: &str) {
        if self.files.load(&symlink_path).is_none() {
            let realpath_path =
                tspath::to_path(realpath, &self.cwd, self.use_case_sensitive_file_names);
            let (set, _) =
                self.files_by_realpath.load_or_store(realpath_path, P::new(SyncSet::default()));
            set.add(symlink.to_string());
        }
        self.files.store(symlink_path, realpath.to_string());
    }
}

pub fn new_known_symlink(
    current_directory: &str,
    use_case_sensitive_file_names: bool,
) -> KnownSymlinks {
    KnownSymlinks {
        cwd: current_directory.to_string(),
        use_case_sensitive_file_names,
        ..Default::default()
    }
}

impl KnownSymlinks {
    pub fn set_symlinks_from_resolutions(
        &self,
        for_each_resolved_module: impl FnOnce(
            &mut dyn FnMut(&ResolvedModule, &str, ResolutionMode, &Path),
            Option<P<SourceFile>>,
        ),
        for_each_resolved_type_reference_directive: impl FnOnce(
            &mut dyn FnMut(&ResolvedTypeReferenceDirective, &str, ResolutionMode, &Path),
            Option<P<SourceFile>>,
        ),
    ) {
        for_each_resolved_module(
            &mut |resolution: &ResolvedModule,
                  _module_name: &str,
                  _mode: ResolutionMode,
                  _file_path: &Path| {
                self.process_resolution(resolution.original_path, resolution.resolved_file_name);
            },
            None,
        );
        for_each_resolved_type_reference_directive(
            &mut |resolution: &ResolvedTypeReferenceDirective,
                  _module_name: &str,
                  _mode: ResolutionMode,
                  _file_path: &Path| {
                self.process_resolution(resolution.original_path, resolution.resolved_file_name);
            },
            None,
        );
    }

    pub fn process_resolution(&self, original_path: &str, resolved_file_name: &str) {
        if original_path.is_empty() || resolved_file_name.is_empty() {
            return;
        }
        self.set_file(
            original_path,
            tspath::to_path(original_path, &self.cwd, self.use_case_sensitive_file_names),
            resolved_file_name,
        );
        let (common_resolved, common_original) =
            self.guess_directory_symlink(resolved_file_name, original_path, &self.cwd);
        if !common_resolved.is_empty() && !common_original.is_empty() {
            let symlink_path =
                tspath::to_path(&common_original, &self.cwd, self.use_case_sensitive_file_names);
            if !tspath::contains_ignored_path(&symlink_path) {
                self.set_directory(
                    &common_original,
                    symlink_path.ensure_trailing_directory_separator(),
                    Some(P::new(KnownDirectoryLink {
                        symlink: String::new(),
                        real: tspath::ensure_trailing_directory_separator(&common_resolved),
                        real_path: tspath::to_path(
                            &common_resolved,
                            &self.cwd,
                            self.use_case_sensitive_file_names,
                        )
                        .ensure_trailing_directory_separator(),
                    })),
                );
            }
        }
    }

    pub fn guess_directory_symlink(&self, a: &str, b: &str, cwd: &str) -> (String, String) {
        let mut a_parts =
            tspath::get_path_components(&tspath::get_normalized_absolute_path(a, cwd), "");
        let mut b_parts =
            tspath::get_path_components(&tspath::get_normalized_absolute_path(b, cwd), "");
        let mut is_directory = false;
        while a_parts.len() >= 2
            && b_parts.len() >= 2
            && !self.is_node_modules_or_scoped_package_directory(&a_parts[a_parts.len() - 2])
            && !self.is_node_modules_or_scoped_package_directory(&b_parts[b_parts.len() - 2])
            && tspath::get_canonical_file_name(
                &a_parts[a_parts.len() - 1],
                self.use_case_sensitive_file_names,
            ) == tspath::get_canonical_file_name(
                &b_parts[b_parts.len() - 1],
                self.use_case_sensitive_file_names,
            )
        {
            a_parts.pop();
            b_parts.pop();
            is_directory = true;
        }
        if is_directory {
            return (
                tspath::get_path_from_path_components(&a_parts),
                tspath::get_path_from_path_components(&b_parts),
            );
        }
        (String::new(), String::new())
    }

    pub fn is_node_modules_or_scoped_package_directory(&self, s: &str) -> bool {
        !s.is_empty()
            && (tspath::get_canonical_file_name(s, self.use_case_sensitive_file_names)
                == "node_modules"
                || s.starts_with('@'))
    }
}
