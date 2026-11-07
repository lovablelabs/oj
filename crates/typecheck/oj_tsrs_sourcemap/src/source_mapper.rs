use std::sync::Arc;

use rustc_hash::FxHashMap;
use tsrs_core::goslices;
use tsrs_core::json;
use tsrs_core::json::Value;
use tsrs_core::stringutil;
use tsrs_core::tspath;

use super::*;

// source_mapper.go:16
pub trait Host {
    fn use_case_sensitive_file_names(&self) -> bool;
    fn get_ecma_line_info(&self, file_name: &str) -> Option<Arc<ECMALineInfo>>;
    fn read_file(&self, file_name: &str) -> Option<String>;
}

// source_mapper.go:23
// Similar to `Mapping`, but position-based.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MappedPosition {
    generated_position: Option<u32>,
    source_position: Option<u32>,
    source_index: Option<u32>,
    name_index: Option<u32>,
}

impl MappedPosition {
    // source_mapper.go:34
    fn is_source_mapped_position(&self) -> bool {
        self.source_index.is_some() && self.source_position.is_some()
    }
}

pub type SourceMappedPosition = MappedPosition;

// source_mapper.go:41
// Maps source positions to generated positions and vice versa.
#[derive(Clone, Debug)]
pub struct DocumentPositionMapper {
    use_case_sensitive_file_names: bool,

    source_file_absolute_paths: Vec<String>,
    source_mappings_by_path: FxHashMap<String, Vec<SourceMappedPosition>>,
    generated_absolute_file_path: String,

    generated_mappings: Vec<MappedPosition>,
}

// source_mapper.go:52
fn create_document_position_mapper(
    host: &dyn Host,
    source_map: &RawSourceMap,
    source_root_field: Option<&str>,
    null_sources: &[bool],
    map_path: &str,
) -> DocumentPositionMapper {
    let map_directory = tspath::get_directory_path(map_path);
    // An explicit empty root is treated as absent for compatibility with established consumers.
    let source_url_prefix =
        source_root_field.filter(|root| !root.is_empty()).map_or_else(String::new, |root| {
            if root.ends_with('/') { root.to_string() } else { format!("{root}/") }
        });
    let generated_absolute_file_path =
        tspath::get_normalized_absolute_path(&source_map.file, &map_directory);
    let source_file_absolute_paths: Vec<String> = source_map
        .sources
        .iter()
        .enumerate()
        .map(|(i, source)| {
            if null_sources.get(i) == Some(&true) {
                return String::new();
            }
            let with_prefix = format!("{source_url_prefix}{source}");
            if with_prefix.is_empty() {
                map_path.to_string()
            } else {
                tspath::get_normalized_absolute_path(&with_prefix, &map_directory)
            }
        })
        .collect();
    let use_case_sensitive_file_names = host.use_case_sensitive_file_names();
    let mut source_to_source_index_map: FxHashMap<String, Vec<u32>> =
        FxHashMap::with_capacity_and_hasher(
            source_file_absolute_paths.len(),
            rustc_hash::FxBuildHasher,
        );
    for (i, source) in source_file_absolute_paths.iter().enumerate() {
        if null_sources.get(i) == Some(&true) {
            continue;
        }
        source_to_source_index_map
            .entry(tspath::get_canonical_file_name(source, use_case_sensitive_file_names))
            .or_default()
            .push(i as u32);
    }

    let mut decoded_mappings: Vec<MappedPosition> = Vec::new();
    let mut source_mappings: FxHashMap<u32, Vec<SourceMappedPosition>> = FxHashMap::default();

    // getDecodedMappings()
    let mut decoder = decode_mappings(&source_map.mappings);
    decoded_mappings.extend(decoder.values().map(|mapping| {
        // processMapping()
        let mut generated_position = None;
        let line_info = host.get_ecma_line_info(&generated_absolute_file_path);
        if let Some(line_info) = &line_info {
            generated_position = Some(tsrs_scanner::compute_position_of_line_and_utf16_character(
                &line_info.line_starts,
                mapping.generated_line as usize,
                mapping.generated_character,
                &line_info.text,
                true, /*allowEdits*/
            ));
        }

        let mut source_position = None;
        let source_index = mapping.source_index.get();
        if let (Some(source_index), Some(source_line), Some(source_character)) =
            (source_index, mapping.source_line, mapping.source_character)
            && source_file_absolute_paths.get(source_index as usize).is_some()
            && null_sources.get(source_index as usize) != Some(&true)
        {
            let line_info =
                host.get_ecma_line_info(&source_file_absolute_paths[source_index as usize]);
            if let Some(line_info) = &line_info {
                let pos = tsrs_scanner::compute_position_of_line_and_utf16_character(
                    &line_info.line_starts,
                    source_line as usize,
                    source_character,
                    &line_info.text,
                    true, /*allowEdits*/
                );
                source_position = Some(pos);
            }
        }

        MappedPosition {
            generated_position,
            source_index,
            source_position,
            name_index: mapping.name_index.get(),
        }
    }));
    if decoder.error().is_some() {
        decoded_mappings = Vec::new();
    }

    // getSourceMappings()
    for mapping in &decoded_mappings {
        if !mapping.is_source_mapped_position() {
            continue;
        }
        let source_index = mapping.source_index.unwrap();
        let list = source_mappings.entry(source_index).or_default();
        list.push(SourceMappedPosition {
            generated_position: mapping.generated_position,
            source_index: Some(source_index),
            source_position: mapping.source_position,
            name_index: mapping.name_index,
        });
    }
    #[expect(clippy::iter_over_hash_type, reason = "sorts each list on its own")]
    for list in source_mappings.values_mut() {
        goslices::sort_func(list, |a, b| {
            assert!(
                a.source_index == b.source_index,
                "All source mappings should have the same source index"
            );
            a.source_position.cmp(&b.source_position)
        });
        *list = tsrs_core::deduplicate_sorted(list, |a, b| {
            a.generated_position == b.generated_position
                && a.source_index == b.source_index
                && a.source_position == b.source_position
        });
    }
    let mut source_mappings_by_path = FxHashMap::default();
    #[expect(clippy::iter_over_hash_type, reason = "each path's mappings are sorted independently")]
    for (path, source_indices) in source_to_source_index_map {
        let mut mappings = Vec::new();
        for index in source_indices {
            if let Some(list) = source_mappings.get(&index) {
                mappings.extend(list.iter().copied());
            }
        }
        goslices::sort_func(&mut mappings, |a, b| a.source_position.cmp(&b.source_position));
        source_mappings_by_path.insert(path, mappings);
    }

    // getGeneratedMappings()
    let mut generated_mappings = decoded_mappings;
    goslices::sort_func(&mut generated_mappings, |a, b| {
        a.generated_position.cmp(&b.generated_position)
    });
    let generated_mappings = tsrs_core::deduplicate_sorted(&generated_mappings, |a, b| {
        a.generated_position == b.generated_position
            && a.source_index == b.source_index
            && a.source_position == b.source_position
    });

    DocumentPositionMapper {
        use_case_sensitive_file_names,
        source_file_absolute_paths,
        source_mappings_by_path,
        generated_absolute_file_path,
        generated_mappings,
    }
}

// source_mapper.go:164
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct DocumentPosition {
    pub file_name: String,
    pub pos: u32,
}

// Go's methods accept a nil receiver and return nil; callers holding an Option<DocumentPositionMapper> take that path
// themselves.
impl DocumentPositionMapper {
    // source_mapper.go:169
    pub fn get_source_position(&self, loc: &DocumentPosition) -> Option<DocumentPosition> {
        if self.generated_mappings.is_empty() {
            return None;
        }

        let target_index =
            self.generated_mappings.partition_point(|m| m.generated_position < Some(loc.pos));

        if target_index >= self.generated_mappings.len() {
            return None;
        }

        let mapping = &self.generated_mappings[target_index];
        if !mapping.is_source_mapped_position() {
            return None;
        }

        // Closest position
        Some(DocumentPosition {
            file_name: self.source_file_absolute_paths[mapping.source_index? as usize].clone(),
            pos: mapping.source_position?,
        })
    }

    // source_mapper.go:197
    pub fn get_generated_position(&self, loc: &DocumentPosition) -> Option<DocumentPosition> {
        let source_mappings = self.source_mappings_by_path.get(
            &tspath::get_canonical_file_name(&loc.file_name, self.use_case_sensitive_file_names),
        )?;
        if source_mappings.is_empty() {
            return None;
        }
        let target_index = source_mappings.partition_point(|m| m.source_position < Some(loc.pos));

        if target_index >= source_mappings.len() {
            return None;
        }

        let mapping = &source_mappings[target_index];
        // Closest position
        Some(DocumentPosition {
            file_name: self.generated_absolute_file_path.clone(),
            pos: mapping.generated_position?,
        })
    }
}

// source_mapper.go:229
pub fn get_document_position_mapper(
    host: &dyn Host,
    generated_file_name: &str,
) -> Option<DocumentPositionMapper> {
    let mut map_file_name = try_get_source_mapping_url_(host, generated_file_name);
    if !map_file_name.is_empty() {
        let (base64_object, matched) = try_parse_base64_url(&map_file_name);
        if matched {
            if !base64_object.is_empty()
                && let Some(decoded) = base64_std_decode_string(base64_object)
            {
                // Go converts the bytes to a string and json.Unmarshal rejects invalid UTF-8, so invalid UTF-8
                // ends in the same nil result.
                return match tsrs_core::utf8::into_string(decoded) {
                    Ok(decoded) => {
                        convert_document_to_source_mapper(host, &decoded, generated_file_name)
                    }
                    Err(_) => None,
                };
            }
            // Not a data URL we can parse, skip it
            map_file_name = String::new();
        }
    }

    let mut possible_map_locations: Vec<String> = Vec::new();
    if !map_file_name.is_empty() {
        possible_map_locations.push(map_file_name);
    }
    possible_map_locations.push(format!("{generated_file_name}.map"));
    if let Some(result) = possible_map_locations.iter().find_map(|location| {
        let map_file_name = tspath::get_normalized_absolute_path(
            location,
            &tspath::get_directory_path(generated_file_name),
        );
        if let Some(map_file_contents) = host.read_file(&map_file_name) {
            return Some(convert_document_to_source_mapper(
                host,
                &map_file_contents,
                &map_file_name,
            ));
        }

        None
    }) {
        return result;
    }
    None
}

// source_mapper.go:257
pub fn convert_document_to_source_mapper(
    host: &dyn Host,
    contents: &str,
    map_file_name: &str,
) -> Option<DocumentPositionMapper> {
    let parsed = try_parse_raw_source_map(contents);
    let parsed = match parsed {
        Some(parsed)
            if !parsed.source_map.sources.is_empty()
                && !parsed.source_map.file.is_empty()
                && !parsed.source_map.mappings.is_empty() =>
        {
            parsed
        }
        // invalid map
        _ => return None,
    };

    // Don't support source maps that contain inlined sources
    if parsed
        .source_map
        .sources_content
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .any(std::option::Option::is_some)
    {
        return None;
    }

    Some(create_document_position_mapper(
        host,
        &parsed.source_map,
        parsed.source_root.as_deref(),
        &parsed.null_sources,
        map_file_name,
    ))
}

// source_mapper.go:305
struct ParsedRawSourceMap {
    source_map: RawSourceMap,
    source_root: Option<String>,
    null_sources: Vec<bool>,
}

fn try_parse_raw_source_map(contents: &str) -> Option<ParsedRawSourceMap> {
    let value = json::unmarshal(contents).ok()?;
    let Value::Object(members) = &value else { return None };
    let source_root = match members.get("sourceRoot") {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    };
    let null_sources = match members.get("sources") {
        Some(Value::Array(sources)) => sources.iter().map(|s| matches!(s, Value::Null)).collect(),
        _ => Vec::new(),
    };
    let source_map = RawSourceMap::from_json(&value).ok()?;
    if source_map.version != 3 {
        return None;
    }
    Some(ParsedRawSourceMap { source_map, source_root, null_sources })
}

// source_mapper.go:284
// Go `tryGetSourceMappingURL`; the trailing `_` keeps it apart from util.go's exported `TryGetSourceMappingURL`.
fn try_get_source_mapping_url_(host: &dyn Host, file_name: &str) -> String {
    let line_info = host.get_ecma_line_info(file_name);
    try_get_source_mapping_url(line_info.as_deref())
}

// source_mapper.go:290
// Equivalent to /^data:(?:application\/json;(?:charset=[uU][tT][fF]-8;)?base64,([A-Za-z0-9+/=]+)$)?/
pub fn try_parse_base64_url(url: &str) -> (&str, bool) {
    let Some(url) = url.strip_prefix("data:") else {
        return ("", false);
    };
    let Some(mut url) = url.strip_prefix("application/json;") else {
        return ("", true);
    };
    if let Some(rest) = url.strip_prefix("charset=") {
        // Go slices `url[:len("utf-8;")]` without a length check (and panics on a shorter string).
        if !rest.as_bytes()[.."utf-8;".len()].eq_ignore_ascii_case(b"utf-8;") {
            return ("", true);
        }
        url = &rest["utf-8;".len()..];
    }
    let Some(url) = url.strip_prefix("base64,") else {
        return ("", true);
    };
    if let Some(result) = url.chars().into_iter().find_map(|r| {
        if stringutil::is_ascii_letter(r as u32)
            || stringutil::is_digit(r as u32)
            || r == '+'
            || r == '/'
            || r == '='
        {
            None
        } else {
            Some(("", true))
        }
    }) {
        return result;
    }
    (url, true)
}

// Go encoding/base64 StdEncoding.DecodeString for input already restricted to [A-Za-z0-9+/=] (so no newlines to
// skip): padded quanta of four, '=' only as "xx==" or "xxx=" in the last quantum, nonzero trailing bits accepted.
pub fn base64_std_decode_string(s: &str) -> Option<Vec<u8>> {
    let src = s.as_bytes();
    if !src.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(src.len() / 4 * 3);
    let quanta = src.len() / 4;
    for (q, chunk) in src.chunks(4).enumerate() {
        let is_last = q + 1 == quanta;
        let padding = match (chunk[2], chunk[3]) {
            (b'=', b'=') => 2,
            (_, b'=') => 1,
            _ => 0,
        };
        if padding > 0 && !is_last {
            return None;
        }
        let mut v: u32 = 0;
        v = chunk.iter().enumerate().try_fold(v, |mut v, (i, &c)| {
            let d = if i >= 4 - padding { 0 } else { u32::from(base64_format_decode(c)?) };
            v = (v << 6) | d;
            Some(v)
        })?;
        out.push((v >> 16) as u8);
        if padding < 2 {
            out.push((v >> 8) as u8);
        }
        if padding < 1 {
            out.push(v as u8);
        }
    }
    Some(out)
}
