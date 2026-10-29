// Go `internal/tspath/extensionrewrite.go`.

use super::{has_ts_file_extension, is_declaration_file_name, path_is_relative};
use crate::stringutil;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionRewrite {
    pub source: String,
    pub target: String,
}

pub fn get_extension_rewrite<'a>(
    path: &str,
    rewrites: &'a [ExtensionRewrite],
    use_case_sensitive_file_names: bool,
) -> Option<&'a ExtensionRewrite> {
    let mut best: Option<&ExtensionRewrite> = None;
    for rewrite in rewrites {
        if rewrite.source.len() <= best.map_or(0, |best| best.source.len())
            || path.len() <= rewrite.source.len()
        {
            continue;
        }
        let Some(suffix) = path.get(path.len() - rewrite.source.len()..) else {
            continue;
        };
        let equal = if use_case_sensitive_file_names {
            suffix == rewrite.source
        } else {
            stringutil::equal_fold(suffix, &rewrite.source)
        };
        if equal {
            best = Some(rewrite);
        }
    }
    best
}

fn is_identity(rewrite: &ExtensionRewrite, use_case_sensitive_file_names: bool) -> bool {
    if use_case_sensitive_file_names {
        rewrite.source == rewrite.target
    } else {
        stringutil::equal_fold(&rewrite.source, &rewrite.target)
    }
}

pub fn rewrite_extension(
    path: &str,
    rewrites: &[ExtensionRewrite],
    use_case_sensitive_file_names: bool,
) -> (String, bool) {
    let Some(rewrite) = get_extension_rewrite(path, rewrites, use_case_sensitive_file_names) else {
        return (path.to_string(), false);
    };
    if is_identity(rewrite, use_case_sensitive_file_names) {
        return (path.to_string(), false);
    }
    let mut result =
        String::with_capacity(path.len() - rewrite.source.len() + rewrite.target.len());
    result.push_str(&path[..path.len() - rewrite.source.len()]);
    result.push_str(&rewrite.target);
    (result, true)
}

pub fn should_rewrite_extension(
    specifier: &str,
    rewrites: &[ExtensionRewrite],
    use_case_sensitive_file_names: bool,
) -> bool {
    if !path_is_relative(specifier) || is_declaration_file_name(specifier) {
        return false;
    }
    if let Some(rewrite) = get_extension_rewrite(specifier, rewrites, use_case_sensitive_file_names)
    {
        return !is_identity(rewrite, use_case_sensitive_file_names);
    }
    has_ts_file_extension(specifier)
}
