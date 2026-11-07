use rustc_hash::FxHashMap;
use tsrs_core::collections::OrderedMap;
use tsrs_core::tspath::{self, ComparePathsOptions};
use tsrs_vfs::vfsmatch;

// Go returns a map with random iteration order; an insertion-ordered map keeps the Rust port deterministic.
pub fn get_wildcard_directories(
    include: &[String],
    exclude: &[String],
    compare_paths_options: &ComparePathsOptions,
) -> Option<OrderedMap<String, bool>> {
    // We watch a directory recursively if it contains a wildcard anywhere in a directory segment
    // of the pattern:
    //
    //  /a/b/**/d   - Watch /a/b recursively to catch changes to any d in any subfolder recursively
    //  /a/b/*/d    - Watch /a/b recursively to catch any d in any immediate subfolder, even if a new subfolder is added
    //  /a/b        - Watch /a/b recursively to catch changes to anything in any recursive subfoler
    //
    // We watch a directory without recursion if it contains a wildcard in the file segment of
    // the pattern:
    //
    //  /a/b/*      - Watch /a/b directly to catch any new file
    //  /a/b/a?z    - Watch /a/b directly to catch any new file matching a?z

    if include.is_empty() {
        return None;
    }

    let exclude_matcher = vfsmatch::new_spec_matcher(
        exclude,
        &compare_paths_options.current_directory,
        vfsmatch::Usage::Exclude,
        compare_paths_options.use_case_sensitive_file_names,
    );

    let mut wildcard_directories: OrderedMap<String, bool> = OrderedMap::default();
    let mut wild_card_key_to_path: FxHashMap<String, String> = FxHashMap::default();

    let mut recursive_keys: Vec<String> = Vec::new();

    for file in include {
        let spec = tspath::normalize_path(&tspath::combine_paths(
            &compare_paths_options.current_directory,
            &[file],
        ));
        if let Some(exclude_matcher) = &exclude_matcher
            && exclude_matcher.match_string(&spec)
        {
            continue;
        }

        let match_ = get_wildcard_directory_from_spec(
            &spec,
            compare_paths_options.use_case_sensitive_file_names,
        );
        if let Some(match_) = match_ {
            let key = match_.key;
            let path = match_.path;
            let recursive = match_.recursive;

            let existing_path = wild_card_key_to_path.get(&key).cloned();
            let mut existing_recursive = false;

            if let Some(existing_path) = &existing_path {
                existing_recursive =
                    wildcard_directories.get(existing_path).copied().unwrap_or(false);
            }

            if existing_path.is_none() || (!existing_recursive && recursive) {
                let path_to_use = match &existing_path {
                    Some(existing_path) => existing_path.clone(),
                    None => path.clone(),
                };
                wildcard_directories.insert(path_to_use, recursive);

                if existing_path.is_none() {
                    wild_card_key_to_path.insert(key.clone(), path);
                }

                if recursive {
                    recursive_keys.push(key);
                }
            }
        }

        // Remove any subpaths under an existing recursively watched directory
        let paths: Vec<String> = wildcard_directories.keys().cloned().collect();
        for path in paths {
            for recursive_key in &recursive_keys {
                let key =
                    to_canonical_key(&path, compare_paths_options.use_case_sensitive_file_names);
                if key != *recursive_key
                    && tspath::contains_path(recursive_key, &key, compare_paths_options)
                {
                    wildcard_directories.shift_remove(&path);
                }
            }
        }
    }

    Some(wildcard_directories)
}

fn to_canonical_key(path: &str, use_case_sensitive_file_names: bool) -> String {
    if use_case_sensitive_file_names {
        return path.to_string();
    }
    tsrs_core::stringutil::go_strings_to_lower(path)
}

// wildcardDirectoryMatch represents the result of a wildcard directory match
struct WildcardDirectoryMatch {
    key: String,
    path: String,
    recursive: bool,
}

fn get_wildcard_directory_from_spec(
    spec: &str,
    use_case_sensitive_file_names: bool,
) -> Option<WildcardDirectoryMatch> {
    // Find the first occurrence of a wildcard character
    if let Some(first_wildcard) = spec.find(['*', '?']) {
        // Find the last directory separator before the wildcard
        if let Some(last_sep_before_wildcard) =
            spec[..first_wildcard].rfind(tspath::DIRECTORY_SEPARATOR as char)
        {
            let path = &spec[..last_sep_before_wildcard];
            // Determine if this should be watched recursively:
            // recursive if the wildcard appears in a directory segment (not just the final file segment)
            let recursive = spec
                .rfind(tspath::DIRECTORY_SEPARATOR as char)
                .is_some_and(|last_separator| first_wildcard < last_separator);

            return Some(WildcardDirectoryMatch {
                key: to_canonical_key(path, use_case_sensitive_file_names),
                path: path.to_string(),
                recursive,
            });
        }
    }

    if let Some((_, last_segment)) = spec.rsplit_once(tspath::DIRECTORY_SEPARATOR as char)
        && vfsmatch::is_implicit_glob(last_segment)
    {
        let path = tspath::remove_trailing_directory_separator(spec);
        return Some(WildcardDirectoryMatch {
            key: to_canonical_key(path, use_case_sensitive_file_names),
            path: path.to_string(),
            recursive: true,
        });
    }

    None
}
