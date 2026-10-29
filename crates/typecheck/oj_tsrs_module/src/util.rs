use std::sync::LazyLock;

use tsrs_ast::SourceFile;
use tsrs_core::semver;
use tsrs_core::tspath;
use tsrs_core::{CompilerOptions, JsxEmit, P};
use tsrs_diagnostics as diagnostics;
use tsrs_diagnostics::Message;

use crate::resolver::move_to_next_directory_separator_if_available;
use crate::types::ResolvedModule;

static TYPE_SCRIPT_VERSION: LazyLock<semver::Version> =
    LazyLock::new(|| semver::must_parse(tsrs_core::version()));

pub const INFERRED_TYPES_CONTAINING_FILE: &str = "__inferred type names__.ts";

pub fn is_applicable_versioned_types_key(key: &str) -> bool {
    let Some(rest) = key.strip_prefix("types@") else {
        return false;
    };
    let Some(range_) = semver::try_parse_version_range(rest) else {
        return false;
    };
    range_.test(&TYPE_SCRIPT_VERSION)
}

pub fn node_module_package_root_for_file(resolved: &str) -> String {
    parse_node_module_package_root(resolved, false)
}

pub fn node_module_package_root_for_directory(resolved: &str) -> String {
    parse_node_module_package_root(resolved, true)
}

// Kept for call sites outside the ported module surface.
pub fn parse_node_module_from_path(resolved: &str, is_folder: bool) -> String {
    parse_node_module_package_root(resolved, is_folder)
}

fn parse_node_module_package_root(resolved: &str, is_directory: bool) -> String {
    let path = tspath::normalize_path(resolved);
    let Some(idx) = path.rfind("/node_modules/") else {
        return String::new();
    };

    let index_after_node_modules = idx + "/node_modules/".len();
    let mut index_after_package_name = move_to_next_directory_separator_if_available(
        &path,
        index_after_node_modules,
        is_directory,
    );
    if path.as_bytes()[index_after_node_modules] == b'@' {
        index_after_package_name = move_to_next_directory_separator_if_available(
            &path,
            index_after_package_name,
            is_directory,
        );
    }
    path[..index_after_package_name].to_string()
}

pub fn parse_package_name(module_name: &str) -> (&str, &str) {
    let mut idx = module_name.find('/');
    if !module_name.is_empty() && module_name.as_bytes()[0] == b'@' {
        // Go computes `offset := idx + 1` with `idx == -1` when there is no slash.
        let offset = match idx {
            Some(i) => i + 1,
            None => 0,
        };
        idx = module_name[offset..].find('/').map(|i| i + offset);
    }
    match idx {
        None => (module_name, ""),
        Some(idx) => (&module_name[..idx], &module_name[idx + 1..]),
    }
}

pub fn mangle_scoped_package_name(package_name: &str) -> String {
    if !package_name.is_empty() && package_name.as_bytes()[0] == b'@' {
        let Some(idx) = package_name.find('/') else {
            return package_name.to_string();
        };
        return format!("{}__{}", &package_name[1..idx], &package_name[idx + 1..]);
    }
    package_name.to_string()
}

pub fn unmangle_scoped_package_name(package_name: &str) -> String {
    if let Some((before, after)) = package_name.split_once("__") {
        return format!("@{before}/{after}");
    }
    package_name.to_string()
}

pub fn get_types_package_name(package_name: &str) -> String {
    format!("@types/{}", mangle_scoped_package_name(package_name))
}

pub fn get_package_name_from_types_package_name(mangled_name: &str) -> String {
    if let Some(without_at_type_prefix) = mangled_name.strip_prefix("@types/") {
        return unmangle_scoped_package_name(without_at_type_prefix);
    }
    mangled_name.to_string()
}

pub fn compare_pattern_keys(a: &str, b: &str) -> std::cmp::Ordering {
    let a_pattern_index = a.find('*');
    let b_pattern_index = b.find('*');
    let base_len_a = match a_pattern_index {
        Some(i) => i + 1,
        None => a.len(),
    };
    let base_len_b = match b_pattern_index {
        Some(i) => i + 1,
        None => b.len(),
    };

    if base_len_a > base_len_b {
        return std::cmp::Ordering::Less;
    }
    if base_len_b > base_len_a {
        return std::cmp::Ordering::Greater;
    }
    if a_pattern_index.is_none() {
        return std::cmp::Ordering::Greater;
    }
    if b_pattern_index.is_none() {
        return std::cmp::Ordering::Less;
    }
    if a.len() > b.len() {
        return std::cmp::Ordering::Less;
    }
    if b.len() > a.len() {
        return std::cmp::Ordering::Greater;
    }
    std::cmp::Ordering::Equal
}

// Returns a DiagnosticMessage if we won't include a resolved module due to its extension.
// The DiagnosticMessage's parameters are the imported module name, and the filename it resolved to.
// This returns a diagnostic even if the module will be an untyped module.
pub fn get_resolution_diagnostic(
    options: &CompilerOptions,
    resolved_module: &ResolvedModule,
    file: P<SourceFile>,
) -> Option<&'static Message> {
    let need_jsx = || -> Option<&'static Message> {
        if options.jsx != JsxEmit::NONE {
            return None;
        }
        Some(&diagnostics::MODULE_0_WAS_RESOLVED_TO_1_BUT_JSX_IS_NOT_SET)
    };

    let need_allow_js = || -> Option<&'static Message> {
        if options.get_allow_js()
            || !options.no_implicit_any.default_if_unknown(options.strict).is_true()
        {
            return None;
        }
        Some(&diagnostics::COULD_NOT_FIND_A_DECLARATION_FILE_FOR_MODULE_0_1_IMPLICITLY_HAS_AN_ANY_TYPE)
    };

    let need_resolve_json_module = || -> Option<&'static Message> {
        if options.get_resolve_json_module() {
            return None;
        }
        Some(&diagnostics::MODULE_0_WAS_RESOLVED_TO_1_BUT_RESOLVE_JSON_MODULE_IS_NOT_USED)
    };

    let need_allow_arbitrary_extensions = || -> Option<&'static Message> {
        if file.is_declaration_file.get() || options.allow_arbitrary_extensions.is_true() {
            return None;
        }
        Some(&diagnostics::MODULE_0_WAS_RESOLVED_TO_1_BUT_ALLOW_ARBITRARY_EXTENSIONS_IS_NOT_SET)
    };

    if resolved_module.resolved_using_extra_extensions {
        return None;
    }

    match resolved_module.extension {
        tspath::EXTENSION_TS
        | tspath::EXTENSION_DTS
        | tspath::EXTENSION_MTS
        | tspath::EXTENSION_DMTS
        | tspath::EXTENSION_CTS
        | tspath::EXTENSION_DCTS => {
            // These are always allowed.
            None
        }
        tspath::EXTENSION_TSX => need_jsx(),
        tspath::EXTENSION_JSX => {
            if let Some(message) = need_jsx() {
                return Some(message);
            }
            need_allow_js()
        }
        tspath::EXTENSION_JS | tspath::EXTENSION_MJS | tspath::EXTENSION_CJS => need_allow_js(),
        tspath::EXTENSION_JSON => need_resolve_json_module(),
        _ => need_allow_arbitrary_extensions(),
    }
}

// TryGetJSExtensionForFile maps TS/JS/DTS extensions to the output JS-side extension.
// Returns an empty string if the extension is unsupported.
pub fn try_get_js_extension_for_file(file_name: &str, options: &CompilerOptions) -> &'static str {
    let ext = tspath::try_get_extension_from_path(file_name);
    match ext {
        tspath::EXTENSION_TS | tspath::EXTENSION_DTS => tspath::EXTENSION_JS,
        tspath::EXTENSION_TSX => {
            if options.jsx == JsxEmit::PRESERVE {
                return tspath::EXTENSION_JSX;
            }
            tspath::EXTENSION_JS
        }
        tspath::EXTENSION_JS | tspath::EXTENSION_JSX | tspath::EXTENSION_JSON => ext,
        tspath::EXTENSION_DMTS | tspath::EXTENSION_MTS | tspath::EXTENSION_MJS => {
            tspath::EXTENSION_MJS
        }
        tspath::EXTENSION_DCTS | tspath::EXTENSION_CTS | tspath::EXTENSION_CJS => {
            tspath::EXTENSION_CJS
        }
        _ => "",
    }
}
