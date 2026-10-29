//! Encoded file names for non-file document URIs. Go: tspath/dynamic.go.

use super::{
    get_any_extension_from_path, get_declaration_file_extension, get_root_length,
    has_trailing_directory_separator, path_is_absolute, remove_trailing_directory_separator,
};

pub const DYNAMIC_URI_FILE_NAME_PREFIX: &str = "^/~ts-uri~/";
const PATH_ESCAPE: &str = "~ts-uri-escape~";
const SPEC_ESCAPE: &str = "~ts-uri-spec~";
const NO_PATH_ESCAPE: &str = "~ts-uri-no-path~";

pub fn is_encoded_dynamic_file_name(path: &str) -> bool {
    path.starts_with(DYNAMIC_URI_FILE_NAME_PREFIX)
}

pub(super) fn canonical_dynamic_uri_path(path: &str) -> String {
    if is_encoded_dynamic_file_name(path)
        && get_root_length(path) == path.len()
        && !has_trailing_directory_separator(path)
    {
        format!("{path}/")
    } else {
        path.to_string()
    }
}

pub fn encode_dynamic_uri_path(path: &str) -> String {
    encode_dynamic_uri_path_root_aware(path, true)
}

pub fn encode_dynamic_uri_directory_path(path: &str) -> String {
    encode_dynamic_uri_path_root_aware(path, false)
}

pub fn encode_dynamic_relative_uri_path(path: &str) -> String {
    encode_dynamic_uri_path_root_aware(path, true)
}

pub fn encode_dynamic_relative_uri_directory_path(path: &str) -> String {
    encode_dynamic_uri_path_root_aware(path, false)
}

fn encode_dynamic_uri_path_root_aware(path: &str, preserve_final_extension: bool) -> String {
    let encoded = encode_dynamic_uri_path_segments(path, preserve_final_extension);
    if !path_is_absolute(&encoded) {
        return encoded;
    }
    match encoded.split_once('/') {
        Some((first, rest)) => {
            format!("{}/{}", force_encode_dynamic_uri_path_segment(first, false), rest)
        }
        None => force_encode_dynamic_uri_path_segment(&encoded, preserve_final_extension),
    }
}

pub fn encode_dynamic_uri_path_with_suffix(path: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        return encode_dynamic_uri_path(path);
    }
    let (before, last) = match path.rsplit_once('/') {
        Some((before, last)) => (format!("{}/", encode_dynamic_uri_directory_path(before)), last),
        None => (String::new(), path),
    };
    format!("{before}{}", force_encode_dynamic_uri_path_segment_with_suffix(last, suffix))
}

fn encode_dynamic_uri_path_segments(path: &str, preserve_final_extension: bool) -> String {
    if !path.split('/').any(dynamic_uri_path_segment_needs_encoding) {
        return path.to_string();
    }
    let mut out = String::with_capacity(path.len() + PATH_ESCAPE.len());
    let mut segments = path.split('/').peekable();
    while let Some(segment) = segments.next() {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(&encode_dynamic_uri_path_segment(
            segment,
            preserve_final_extension && segments.peek().is_none(),
        ));
    }
    out
}

pub fn force_encode_dynamic_uri_path_segment(segment: &str, preserve_extension: bool) -> String {
    let (base, extension) = if preserve_extension && segment != "." && segment != ".." {
        split_dynamic_uri_file_extension(segment)
    } else {
        (segment, "")
    };
    format!("{PATH_ESCAPE}{}~{extension}", hex_encode(base.as_bytes()))
}

pub fn encode_dynamic_module_specifier(specifier: &str) -> String {
    encode_dynamic_module_specifier_worker(specifier, true)
}

pub fn encode_dynamic_directory_specifier(specifier: &str) -> String {
    encode_dynamic_module_specifier_worker(specifier, false)
}

fn encode_dynamic_module_specifier_worker(
    specifier: &str,
    preserve_final_extension: bool,
) -> String {
    if !specifier.contains(PATH_ESCAPE) && !specifier.contains(SPEC_ESCAPE) {
        return specifier.to_string();
    }
    let mut out = String::with_capacity(specifier.len() + PATH_ESCAPE.len());
    let mut rest = specifier;
    loop {
        if let Some(index) = rest.find(['/', '\\']) {
            out.push_str(&encode_dynamic_module_specifier_segment(&rest[..index], false));
            out.push(rest.as_bytes()[index] as char);
            rest = &rest[index + 1..];
        } else {
            out.push_str(&encode_dynamic_module_specifier_segment(rest, preserve_final_extension));
            return out;
        }
    }
}

fn encode_dynamic_module_specifier_segment(segment: &str, preserve_extension: bool) -> String {
    if let Some(encoded) = segment.strip_prefix(SPEC_ESCAPE) {
        let physical = format!("{PATH_ESCAPE}{encoded}");
        if decode_dynamic_uri_path_segment(&physical) != physical {
            return physical;
        }
    }
    if segment.starts_with(PATH_ESCAPE) {
        return force_encode_dynamic_uri_path_segment(segment, preserve_extension);
    }
    segment.to_string()
}

pub fn dynamic_uri_path_to_module_specifier(path: &str) -> String {
    if !path.contains(PATH_ESCAPE) {
        return path.to_string();
    }
    path.split('/')
        .map(|segment| {
            if segment.starts_with(PATH_ESCAPE)
                && decode_dynamic_uri_path_segment(segment) != segment
            {
                format!("{SPEC_ESCAPE}{}", &segment[PATH_ESCAPE.len()..])
            } else {
                segment.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

pub fn encode_dynamic_logical_module_specifier(specifier: &str) -> String {
    if specifier.is_empty() {
        return String::new();
    }
    let trailing = has_trailing_directory_separator(specifier);
    let body = if trailing { remove_trailing_directory_separator(specifier) } else { specifier };
    let mut encoded = dynamic_uri_path_to_module_specifier(&encode_dynamic_relative_uri_path(body));
    if trailing {
        encoded.push('/');
    }
    encoded
}

fn encode_dynamic_uri_path_segment(segment: &str, preserve_extension: bool) -> String {
    if dynamic_uri_path_segment_needs_encoding(segment) {
        force_encode_dynamic_uri_path_segment(segment, preserve_extension)
    } else {
        segment.to_string()
    }
}

fn dynamic_uri_path_segment_needs_encoding(segment: &str) -> bool {
    segment.is_empty()
        || segment == "."
        || segment == ".."
        || segment.starts_with(PATH_ESCAPE)
        || segment.starts_with(SPEC_ESCAPE)
        || segment.starts_with(NO_PATH_ESCAPE)
        || segment.contains('\\')
}

pub fn encode_dynamic_uri_no_path(suffix: &str) -> String {
    format!("{NO_PATH_ESCAPE}{}~", hex_encode(suffix.as_bytes()))
}

pub fn decode_dynamic_uri_no_path(path: &str) -> Option<String> {
    let encoded = path.strip_prefix(NO_PATH_ESCAPE)?.strip_suffix('~')?;
    hex_decode(encoded)
}

fn force_encode_dynamic_uri_path_segment_with_suffix(segment: &str, suffix: &str) -> String {
    let (base, extension) = split_dynamic_uri_file_extension(segment);
    let mut payload = String::from(base);
    payload.push('\0');
    payload.push_str(suffix);
    format!("{PATH_ESCAPE}{}~{extension}", hex_encode(payload.as_bytes()))
}

fn split_dynamic_uri_file_extension(segment: &str) -> (&str, &str) {
    let base_name = segment.rsplit('\\').next().unwrap_or(segment);
    let declaration_extension = get_declaration_file_extension(base_name);
    let any_extension;
    let extension: &str = if declaration_extension.is_empty() {
        any_extension = get_any_extension_from_path(base_name, &[], true);
        &any_extension
    } else {
        declaration_extension
    };
    if extension.is_empty() {
        (segment, "")
    } else {
        let split = segment.len() - extension.len();
        (&segment[..split], &segment[split..])
    }
}

pub fn decode_dynamic_uri_path(path: &str) -> String {
    if !path.contains(PATH_ESCAPE) {
        return path.to_string();
    }
    path.split('/').map(decode_dynamic_uri_path_segment).collect::<Vec<_>>().join("/")
}

pub fn try_decode_dynamic_uri_path(path: &str) -> Option<String> {
    path.split('/')
        .map(try_decode_dynamic_uri_path_segment)
        .collect::<Option<Vec<_>>>()
        .map(|segments| segments.join("/"))
}

pub fn decode_dynamic_uri_path_for_disk(path: &str) -> Option<String> {
    let decoded = decode_dynamic_uri_path(path);
    if path_is_absolute(&decoded) {
        return None;
    }
    let mut segments = decoded.split('/').peekable();
    while let Some(segment) = segments.next() {
        if segment.is_empty() {
            if segments.peek().is_none() {
                return Some(decoded);
            }
            return None;
        }
        if segment == "." || segment == ".." || segment.contains('\\') {
            return None;
        }
    }
    Some(decoded)
}

pub fn decode_dynamic_uri_path_segment(segment: &str) -> String {
    try_decode_dynamic_uri_path_segment(segment).unwrap_or_else(|| segment.to_string())
}

pub fn try_decode_dynamic_uri_path_segment(segment: &str) -> Option<String> {
    let Some(encoded) = segment.strip_prefix(PATH_ESCAPE) else {
        return (!dynamic_uri_path_segment_needs_encoding(segment)).then(|| segment.to_string());
    };
    let (encoded, extension) = encoded.split_once('~')?;
    let decoded = hex_decode(encoded)?;
    match decoded.split_once('\0') {
        Some((base, suffix)) => Some(format!("{base}{extension}{suffix}")),
        None => Some(format!("{decoded}{extension}")),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 15) as usize] as char);
    }
    out
}

fn hex_decode(encoded: &str) -> Option<String> {
    if !encoded.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(encoded.len() / 2);
    for pair in encoded.as_bytes().as_chunks::<2>().0 {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        bytes.push(((hi << 4) | lo) as u8);
    }
    String::from_utf8(bytes).ok()
}
