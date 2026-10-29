use super::path::{file_extension_is, get_any_extension_from_path, get_base_file_name_cow};

pub const EXTENSION_TS: &str = ".ts";
pub const EXTENSION_TSX: &str = ".tsx";
pub const EXTENSION_DTS: &str = ".d.ts";
pub const EXTENSION_JS: &str = ".js";
pub const EXTENSION_JSX: &str = ".jsx";
pub const EXTENSION_JSON: &str = ".json";
pub const EXTENSION_TS_BUILD_INFO: &str = ".tsbuildinfo";
pub const EXTENSION_MJS: &str = ".mjs";
pub const EXTENSION_MTS: &str = ".mts";
pub const EXTENSION_DMTS: &str = ".d.mts";
pub const EXTENSION_CJS: &str = ".cjs";
pub const EXTENSION_CTS: &str = ".cts";
pub const EXTENSION_DCTS: &str = ".d.cts";

pub static SUPPORTED_DECLARATION_EXTENSIONS: &[&str] =
    &[EXTENSION_DTS, EXTENSION_DCTS, EXTENSION_DMTS];
pub static SUPPORTED_TS_IMPLEMENTATION_EXTENSIONS: &[&str] =
    &[EXTENSION_TS, EXTENSION_TSX, EXTENSION_MTS, EXTENSION_CTS];
static SUPPORTED_TS_EXTENSIONS_FOR_EXTRACT_EXTENSION: &[&str] = &[
    EXTENSION_DTS,
    EXTENSION_DCTS,
    EXTENSION_DMTS,
    EXTENSION_TS,
    EXTENSION_TSX,
    EXTENSION_MTS,
    EXTENSION_CTS,
];
pub static ALL_SUPPORTED_EXTENSIONS: &[&[&str]] = &[
    &[EXTENSION_TS, EXTENSION_TSX, EXTENSION_DTS, EXTENSION_JS, EXTENSION_JSX],
    &[EXTENSION_CTS, EXTENSION_DCTS, EXTENSION_CJS],
    &[EXTENSION_MTS, EXTENSION_DMTS, EXTENSION_MJS],
];
pub static SUPPORTED_TS_EXTENSIONS: &[&[&str]] = &[
    &[EXTENSION_TS, EXTENSION_TSX, EXTENSION_DTS],
    &[EXTENSION_CTS, EXTENSION_DCTS],
    &[EXTENSION_MTS, EXTENSION_DMTS],
];
pub static SUPPORTED_TS_EXTENSIONS_FLAT: &[&str] = &[
    EXTENSION_TS,
    EXTENSION_TSX,
    EXTENSION_DTS,
    EXTENSION_CTS,
    EXTENSION_DCTS,
    EXTENSION_MTS,
    EXTENSION_DMTS,
];

pub static SUPPORTED_JS_EXTENSIONS_FLAT: &[&str] =
    &[EXTENSION_JS, EXTENSION_JSX, EXTENSION_MJS, EXTENSION_CJS];
pub static ALL_SUPPORTED_EXTENSIONS_WITH_JSON: &[&[&str]] = &[
    &[EXTENSION_TS, EXTENSION_TSX, EXTENSION_DTS, EXTENSION_JS, EXTENSION_JSX],
    &[EXTENSION_CTS, EXTENSION_DCTS, EXTENSION_CJS],
    &[EXTENSION_MTS, EXTENSION_DMTS, EXTENSION_MJS],
    &[EXTENSION_JSON],
];
pub static SUPPORTED_TS_EXTENSIONS_WITH_JSON: &[&[&str]] = &[
    &[EXTENSION_TS, EXTENSION_TSX, EXTENSION_DTS],
    &[EXTENSION_CTS, EXTENSION_DCTS],
    &[EXTENSION_MTS, EXTENSION_DMTS],
    &[EXTENSION_JSON],
];
pub static SUPPORTED_TS_EXTENSIONS_WITH_JSON_FLAT: &[&str] = &[
    EXTENSION_TS,
    EXTENSION_TSX,
    EXTENSION_DTS,
    EXTENSION_CTS,
    EXTENSION_DCTS,
    EXTENSION_MTS,
    EXTENSION_DMTS,
    EXTENSION_JSON,
];
pub static EXTENSIONS_NOT_SUPPORTING_EXTENSIONLESS_RESOLUTION: &[&str] =
    &[EXTENSION_MTS, EXTENSION_DMTS, EXTENSION_MJS, EXTENSION_CTS, EXTENSION_DCTS, EXTENSION_CJS];

pub fn extension_is_ts(ext: &str) -> bool {
    ext == EXTENSION_TS
        || ext == EXTENSION_TSX
        || ext == EXTENSION_DTS
        || ext == EXTENSION_MTS
        || ext == EXTENSION_DMTS
        || ext == EXTENSION_CTS
        || ext == EXTENSION_DCTS
        || ext.len() >= 7 && ext.as_bytes().starts_with(b".d.") && ext.as_bytes().ends_with(b".ts")
}

static EXTENSIONS_TO_REMOVE: &[&str] = &[
    EXTENSION_DTS,
    EXTENSION_DMTS,
    EXTENSION_DCTS,
    EXTENSION_MJS,
    EXTENSION_MTS,
    EXTENSION_CJS,
    EXTENSION_CTS,
    EXTENSION_TS,
    EXTENSION_JS,
    EXTENSION_TSX,
    EXTENSION_JSX,
    EXTENSION_JSON,
];

pub fn remove_file_extension(path: &str) -> &str {
    // Remove any known extension even if it has more than one dot
    if let Some(result) = EXTENSIONS_TO_REMOVE.iter().find_map(|&ext| path.strip_suffix(ext)) {
        return result;
    }

    path
}

pub fn remove_any_file_extension(path: &str) -> &str {
    let without_extension = remove_file_extension(path);
    if without_extension.len() != path.len() {
        return without_extension;
    }
    let extension = get_any_extension_from_path(path, &[], false);
    if !extension.is_empty() {
        return remove_extension(path, &extension);
    }
    path
}

pub fn try_get_extension_from_path(p: &str) -> &'static str {
    if let Some(result) = EXTENSIONS_TO_REMOVE.iter().copied().find(|ext| file_extension_is(p, ext))
    {
        return result;
    }
    ""
}

pub fn remove_extension<'a>(path: &'a str, extension: &str) -> &'a str {
    &path[..path.len() - extension.len()]
}

pub fn file_extension_is_one_of(path: &str, extensions: &[&str]) -> bool {
    extensions.iter().any(|ext| file_extension_is(path, ext))
}

pub fn try_extract_ts_extension(file_name: &str) -> &'static str {
    if let Some(result) = SUPPORTED_TS_EXTENSIONS_FOR_EXTRACT_EXTENSION
        .iter()
        .copied()
        .find(|ext| file_extension_is(file_name, ext))
    {
        return result;
    }
    ""
}

pub fn has_ts_file_extension(path: &str) -> bool {
    file_extension_is_one_of(path, SUPPORTED_TS_EXTENSIONS_FLAT)
}

pub fn has_implementation_ts_file_extension(path: &str) -> bool {
    file_extension_is_one_of(path, SUPPORTED_TS_IMPLEMENTATION_EXTENSIONS)
        && !is_declaration_file_name(path)
}

pub fn has_js_file_extension(path: &str) -> bool {
    file_extension_is_one_of(path, SUPPORTED_JS_EXTENSIONS_FLAT)
}

pub fn has_json_file_extension(path: &str) -> bool {
    file_extension_is(path, EXTENSION_JSON)
}

pub fn is_declaration_file_name(file_name: &str) -> bool {
    !get_declaration_file_extension(file_name).is_empty()
}

pub fn extension_is_one_of(ext: &str, extensions: &[&str]) -> bool {
    extensions.contains(&ext)
}

/// The declaration extension at the end of `file_name`, as a slice of it; empty when there is none.
///
/// `.d.ts`, `.d.mts`, `.d.cts` or a custom `.d.x.ts`. Borrowed because every module-resolution probe asks
/// `is_declaration_file_name` (notes/perf-reparse-cloner.md).
pub fn get_declaration_file_extension(file_name: &str) -> &str {
    let base = get_base_file_name_cow(file_name);
    let len = if let Some(ext) =
        SUPPORTED_DECLARATION_EXTENSIONS.iter().find(|ext| base.ends_with(*ext))
    {
        ext.len()
    } else if base.ends_with(EXTENSION_TS) {
        base.find(".d.").map_or(0, |index| base.len() - index)
    } else {
        0
    };
    if len == 0 {
        return "";
    }
    // The base name is the last segment of the path with at most one trailing separator removed, so the extension
    // is the tail of the path before that separator (an extension has no separator in it).
    let trimmed = file_name.trim_end_matches(['/', '\\']);
    &trimmed[trimmed.len() - len..]
}

pub fn get_declaration_emit_extension_for_path(path: &str) -> String {
    if file_extension_is_one_of(path, &[EXTENSION_MJS, EXTENSION_MTS]) {
        EXTENSION_DMTS.to_string()
    } else if file_extension_is_one_of(path, &[EXTENSION_CJS, EXTENSION_CTS]) {
        EXTENSION_DCTS.to_string()
    } else if file_extension_is_one_of(
        path,
        &[EXTENSION_TS, EXTENSION_TSX, EXTENSION_JS, EXTENSION_JSX],
    ) {
        EXTENSION_DTS.to_string()
    } else {
        let ext = get_any_extension_from_path(path, &[], false);
        if !ext.is_empty() {
            return format!(".d{ext}.ts");
        }
        EXTENSION_DTS.to_string()
    }
}

// ChangeAnyExtension changes the extension of a path to the provided extension if it has one of the provided extensions.
//
// ChangeAnyExtension("/path/to/file.ext", ".js", ".ext") === "/path/to/file.js"
// ChangeAnyExtension("/path/to/file.ext", ".js", ".ts") === "/path/to/file.ext"
// ChangeAnyExtension("/path/to/file.ext", ".js", [".ext", ".ts"]) === "/path/to/file.js"
pub fn change_any_extension(
    path: &str,
    ext: &str,
    extensions: &[&str],
    ignore_case: bool,
) -> String {
    let pathext = get_any_extension_from_path(path, extensions, ignore_case);
    if !pathext.is_empty() {
        let result = &path[..path.len() - pathext.len()];
        if ext.is_empty() {
            return result.to_string();
        }
        if ext.starts_with('.') {
            return format!("{result}{ext}");
        }
        return format!("{result}.{ext}");
    }
    path.to_string()
}

pub fn change_extension(path: &str, new_extension: &str) -> String {
    change_any_extension(path, new_extension, EXTENSIONS_TO_REMOVE, false /*ignoreCase*/)
}

// Like `changeAnyExtension`, but declaration file extensions are recognized
// and replaced starting from the `.d`.
//
//	changeAnyExtension("file.d.ts", ".js") === "file.d.js"
//	changeFullExtension("file.d.ts", ".js") === "file.js"
pub fn change_full_extension(path: &str, new_extension: &str) -> String {
    let declaration_extension = get_declaration_file_extension(path);
    if !declaration_extension.is_empty() {
        let ext = if new_extension.starts_with('.') {
            new_extension.to_string()
        } else {
            format!(".{new_extension}")
        };
        return format!("{}{}", &path[..path.len() - declaration_extension.len()], ext);
    }
    change_extension(path, new_extension)
}

pub fn get_possible_original_input_extension_for_extension(path: &str) -> Vec<String> {
    if file_extension_is_one_of(path, &[EXTENSION_DMTS, EXTENSION_MJS, EXTENSION_MTS]) {
        return vec![EXTENSION_MTS.to_string(), EXTENSION_MJS.to_string()];
    }
    if file_extension_is_one_of(path, &[EXTENSION_DCTS, EXTENSION_CJS, EXTENSION_CTS]) {
        return vec![EXTENSION_CTS.to_string(), EXTENSION_CJS.to_string()];
    }
    // Handle any custom .d.x.ts extension (e.g., .d.json.ts -> .json, .d.css.ts -> .css)
    let ext = get_declaration_file_extension(path);
    if !ext.is_empty() && ext != EXTENSION_DTS {
        let inner = &ext[".d.".len()..ext.len() - ".ts".len()];
        return vec![format!(".{inner}")];
    }
    vec![
        EXTENSION_TSX.to_string(),
        EXTENSION_TS.to_string(),
        EXTENSION_JSX.to_string(),
        EXTENSION_JS.to_string(),
    ]
}
