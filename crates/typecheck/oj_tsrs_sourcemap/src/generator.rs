use std::collections::hash_map::Entry;

use rustc_hash::FxHashMap;
use tsrs_core::UTF16Offset;
use tsrs_core::json::{self, JsonMap, Value};
use tsrs_core::tspath::{self, ComparePathsOptions};

// generator.go:14
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SourceIndex {
    #[default]
    Missing,
    Index(u32),
}

impl SourceIndex {
    pub const fn get(self) -> Option<u32> {
        match self {
            SourceIndex::Missing => None,
            SourceIndex::Index(index) => Some(index),
        }
    }
}

impl std::fmt::Display for SourceIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceIndex::Missing => f.write_str("-1"),
            SourceIndex::Index(index) => index.fmt(f),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum NameIndex {
    #[default]
    Missing,
    Index(u32),
}

impl NameIndex {
    pub const fn get(self) -> Option<u32> {
        match self {
            NameIndex::Missing => None,
            NameIndex::Index(index) => Some(index),
        }
    }
}

impl std::fmt::Display for NameIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NameIndex::Missing => f.write_str("-1"),
            NameIndex::Index(index) => index.fmt(f),
        }
    }
}

pub const MISSING_SOURCE: SourceIndex = SourceIndex::Missing;
pub const MISSING_NAME: NameIndex = NameIndex::Missing;

// generator.go:26
#[derive(Clone, Debug, Default)]
pub struct Generator {
    path_options: ComparePathsOptions,
    file: String,
    source_root: String,
    sources_directory_path: String,
    raw_sources: Vec<String>,
    sources: Vec<String>,
    source_to_source_index_map: FxHashMap<String, SourceIndex>,
    sources_content: Option<Vec<Option<String>>>,
    names: Vec<String>,
    name_to_name_index_map: FxHashMap<String, NameIndex>,
    mappings: String,
    last_generated_line: u32,
    last_generated_character: UTF16Offset,
    last_source_index: u32,
    last_source_line: u32,
    last_source_character: UTF16Offset,
    last_name_index: u32,
    has_last: bool,
    pending_generated_line: u32,
    pending_generated_character: UTF16Offset,
    pending_source_index: u32,
    pending_source_line: u32,
    pending_source_character: UTF16Offset,
    pending_name_index: u32,
    has_pending: bool,
    has_pending_source: bool,
    has_pending_name: bool,
}

// generator.go:56
// `sources_content` is Go's `[]*string` with `omitzero`: None (nil) is omitted from the JSON, Some(vec![]) is not.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct RawSourceMap {
    pub version: u32,
    pub file: String,
    pub source_root: String,
    pub sources: Vec<String>,
    pub names: Vec<String>,
    pub mappings: String,
    pub sources_content: Option<Vec<Option<String>>>,
}

// Go marshals and unmarshals RawSourceMap through its struct tags with encoding/json/v2; these two functions do the
// same for its seven fields (field order, omitzero, case-sensitive names, unknown names ignored, null -> zero value).
impl RawSourceMap {
    fn to_json(&self) -> Value {
        let mut m = JsonMap::new();
        m.insert("version".to_string(), Value::Number(self.version as f64));
        m.insert("file".to_string(), Value::String(self.file.clone()));
        if !self.source_root.is_empty() {
            m.insert("sourceRoot".to_string(), Value::String(self.source_root.clone()));
        }
        m.insert(
            "sources".to_string(),
            Value::Array(self.sources.iter().map(|s| Value::String(s.clone())).collect()),
        );
        m.insert(
            "names".to_string(),
            Value::Array(self.names.iter().map(|s| Value::String(s.clone())).collect()),
        );
        m.insert("mappings".to_string(), Value::String(self.mappings.clone()));
        if let Some(sources_content) = &self.sources_content {
            m.insert(
                "sourcesContent".to_string(),
                Value::Array(
                    sources_content
                        .iter()
                        .map(|s| match s {
                            Some(s) => Value::String(s.clone()),
                            None => Value::Null,
                        })
                        .collect(),
                ),
            );
        }
        Value::Object(m)
    }

    pub fn from_json(value: &Value) -> Result<RawSourceMap, String> {
        fn string(name: &str, v: &Value) -> Result<String, String> {
            match v {
                Value::Null => Ok(String::new()),
                Value::String(s) => Ok(s.clone()),
                _ => Err(format!(
                    "json: cannot unmarshal into Go RawSourceMap.{name} of type string"
                )),
            }
        }
        fn strings(name: &str, v: &Value) -> Result<Vec<String>, String> {
            match v {
                Value::Null => Ok(Vec::new()),
                Value::Array(items) => items.iter().map(|item| string(name, item)).collect(),
                _ => Err(format!(
                    "json: cannot unmarshal into Go RawSourceMap.{name} of type []string"
                )),
            }
        }

        let mut sm = RawSourceMap::default();
        let members = match value {
            Value::Null => return Ok(sm),
            Value::Object(members) => members,
            _ => {
                return Err("json: cannot unmarshal into Go value of type sourcemap.RawSourceMap"
                    .to_string());
            }
        };
        for (name, v) in members {
            match name {
                "version" => {
                    sm.version = match v {
                        Value::Null => 0,
                        Value::Number(n) if n.fract() == 0.0 && *n >= 0.0 && *n <= u32::MAX as f64 => *n as u32,
                        _ => return Err("json: cannot unmarshal into Go RawSourceMap.version of type int".to_string()),
                    }
                }
                "file" => sm.file = string(name, v)?,
                "sourceRoot" => sm.source_root = string(name, v)?,
                "sources" => sm.sources = strings(name, v)?,
                "names" => sm.names = strings(name, v)?,
                "mappings" => sm.mappings = string(name, v)?,
                "sourcesContent" => {
                    sm.sources_content = match v {
                        Value::Null => None,
                        Value::Array(items) => Some(
                            items
                                .iter()
                                .map(|item| match item {
                                    Value::Null => Ok(None),
                                    Value::String(s) => Ok(Some(s.clone())),
                                    _ => Err("json: cannot unmarshal into Go RawSourceMap.sourcesContent of type *string".to_string()),
                                })
                                .collect::<Result<Vec<_>, String>>()?,
                        ),
                        _ => return Err("json: cannot unmarshal into Go RawSourceMap.sourcesContent of type []*string".to_string()),
                    }
                }
                _ => {}
            }
        }
        Ok(sm)
    }
}

// generator.go:66
pub fn new_generator(
    file: &str,
    source_root: &str,
    sources_directory_path: &str,
    options: ComparePathsOptions,
) -> Generator {
    Generator {
        file: file.to_string(),
        source_root: source_root.to_string(),
        sources_directory_path: sources_directory_path.to_string(),
        path_options: options,
        ..Default::default()
    }
}

impl Generator {
    // generator.go:75
    #[expect(clippy::misnamed_getters, reason = "Go's Sources() returns rawSources")]
    pub fn sources(&self) -> &[String] {
        &self.raw_sources
    }

    // generator.go:78
    // Adds a source to the source map
    pub fn add_source(&mut self, file_name: &str) -> SourceIndex {
        let source = tspath::get_relative_path_to_directory_or_url(
            &self.sources_directory_path,
            file_name,
            true, /*isAbsolutePathAnUrl*/
            &self.path_options,
        );

        match self.source_to_source_index_map.entry(source) {
            Entry::Occupied(entry) => *entry.get(),
            Entry::Vacant(entry) => {
                let source_index = SourceIndex::Index(self.sources.len() as u32);
                self.sources.push(entry.key().clone());
                self.raw_sources.push(file_name.to_string());
                entry.insert(source_index);
                source_index
            }
        }
    }

    // generator.go:101
    // Sets the content for a source
    pub fn set_source_content(
        &mut self,
        source_index: SourceIndex,
        content: &str,
    ) -> Result<(), String> {
        let Some(source_index) = source_index.get().map(|index| index as usize) else {
            return Err("sourceIndex is out of range".to_string());
        };
        if source_index >= self.sources.len() {
            return Err("sourceIndex is out of range".to_string());
        }
        let sources_content = self.sources_content.get_or_insert_with(Vec::new);
        sources_content.resize(sources_content.len().max(source_index + 1), None);
        sources_content[source_index] = Some(content.to_string());
        Ok(())
    }

    // generator.go:113
    // Declares a name in the source map, returning the index of the name
    pub fn add_name(&mut self, name: &str) -> NameIndex {
        if let Some(&name_index) = self.name_to_name_index_map.get(name) {
            name_index
        } else {
            let name_index = NameIndex::Index(self.names.len() as u32);
            self.names.push(name.to_string());
            self.name_to_name_index_map.insert(name.to_string(), name_index);
            name_index
        }
    }

    // generator.go:126
    fn is_new_generated_position(
        &self,
        generated_line: u32,
        generated_character: UTF16Offset,
    ) -> bool {
        !self.has_pending
            || self.pending_generated_line != generated_line
            || self.pending_generated_character != generated_character
    }

    // generator.go:132
    fn is_backtracking_source_position(
        &self,
        source_index: u32,
        source_line: u32,
        source_character: UTF16Offset,
    ) -> bool {
        self.pending_source_index == source_index
            && (self.pending_source_line > source_line
                || self.pending_source_line == source_line
                    && self.pending_source_character > source_character)
    }

    // generator.go:141
    fn should_commit_mapping(&self) -> bool {
        self.has_pending
            && (!self.has_last
                || self.last_generated_line != self.pending_generated_line
                || self.last_generated_character != self.pending_generated_character
                || self.last_source_index != self.pending_source_index
                || self.last_source_line != self.pending_source_line
                || self.last_source_character != self.pending_source_character
                || self.last_name_index != self.pending_name_index)
    }

    // generator.go:151
    fn append_mapping_char_code(&mut self, char_code: char) {
        self.mappings.push(char_code);
    }

    // generator.go:155
    fn append_base64_vlq(&mut self, in_value: i64) {
        // Go's int is 64-bit.
        let mut in_value = in_value;
        // Add a new least significant bit that has the sign of the value.
        // if negative number the least significant bit that gets added to the number has value 1
        // else least significant bit value that gets added is 0
        // eg. -1 changes to binary : 01 [1] => 3
        //     +1 changes to binary : 01 [0] => 2
        if in_value < 0 {
            in_value = ((-in_value) << 1) + 1;
        } else {
            in_value <<= 1;
        }

        // Encode 5 bits at a time starting from least significant bits
        loop {
            let mut current_digit = in_value & 31; // 11111
            in_value >>= 5;
            if in_value > 0 {
                // There are still more digits to decode, set the msb (6th bit)
                current_digit |= 32;
            }
            self.append_mapping_char_code(base64_format_encode(current_digit as u8));
            if in_value <= 0 {
                break;
            }
        }
    }

    // generator.go:182
    fn commit_pending_mapping(&mut self) {
        if !self.should_commit_mapping() {
            return;
        }

        // Line/Comma delimiters
        if self.last_generated_line < self.pending_generated_line {
            // Emit line delimiters
            loop {
                self.append_mapping_char_code(';');
                self.last_generated_line += 1;
                if self.last_generated_line >= self.pending_generated_line {
                    break;
                }
            }
            // Only need to set this once
            self.last_generated_character = 0;
        } else {
            // panic rather than error as an invariant has been violated
            assert!(
                self.last_generated_line == self.pending_generated_line,
                "generatedLine cannot backtrack"
            );
            // Emit comma to separate the entry
            if self.has_last {
                self.append_mapping_char_code(',');
            }
        }

        // 1. Relative generated character
        self.append_base64_vlq(
            i64::from(self.pending_generated_character) - i64::from(self.last_generated_character),
        );
        self.last_generated_character = self.pending_generated_character;

        if self.has_pending_source {
            // 2. Relative sourceIndex
            self.append_base64_vlq(
                i64::from(self.pending_source_index) - i64::from(self.last_source_index),
            );
            self.last_source_index = self.pending_source_index;

            // 3. Relative source line
            self.append_base64_vlq(
                i64::from(self.pending_source_line) - i64::from(self.last_source_line),
            );
            self.last_source_line = self.pending_source_line;

            // 4. Relative source character
            self.append_base64_vlq(
                i64::from(self.pending_source_character) - i64::from(self.last_source_character),
            );
            self.last_source_character = self.pending_source_character;

            if self.has_pending_name {
                // 5. Relative nameIndex
                self.append_base64_vlq(
                    i64::from(self.pending_name_index) - i64::from(self.last_name_index),
                );
                self.last_name_index = self.pending_name_index;
            }
        }

        self.has_last = true;
    }

    // generator.go:237
    fn add_mapping(
        &mut self,
        generated_line: u32,
        generated_character: UTF16Offset,
        source: Option<(u32, u32, UTF16Offset, Option<u32>)>,
    ) {
        if self.is_new_generated_position(generated_line, generated_character)
            || source.is_some_and(|(source_index, source_line, source_character, _)| {
                self.is_backtracking_source_position(source_index, source_line, source_character)
            })
        {
            self.commit_pending_mapping();
            self.pending_generated_line = generated_line;
            self.pending_generated_character = generated_character;
            self.has_pending_source = false;
            self.has_pending_name = false;
            self.has_pending = true;
        }

        if let Some((source_index, source_line, source_character, name_index)) = source {
            self.pending_source_index = source_index;
            self.pending_source_line = source_line;
            self.pending_source_character = source_character;
            self.has_pending_source = true;
            if let Some(name_index) = name_index {
                self.pending_name_index = name_index;
                self.has_pending_name = true;
            }
        }
    }

    // generator.go:261
    // Adds a mapping without source information
    pub fn add_generated_mapping(
        &mut self,
        generated_line: u32,
        generated_character: UTF16Offset,
    ) -> Result<(), String> {
        if generated_line < self.pending_generated_line {
            return Err("generatedLine cannot backtrack".to_string());
        }
        self.add_mapping(generated_line, generated_character, None);
        self.has_pending_source = false;
        self.has_pending_name = false;
        Ok(())
    }

    // generator.go:275
    // Adds a mapping with source information
    pub fn add_source_mapping(
        &mut self,
        generated_line: u32,
        generated_character: UTF16Offset,
        source_index: SourceIndex,
        source_line: u32,
        source_character: UTF16Offset,
    ) -> Result<(), String> {
        if generated_line < self.pending_generated_line {
            return Err("generatedLine cannot backtrack".to_string());
        }
        let Some(source_index) = source_index.get() else {
            return Err("sourceIndex is out of range".to_string());
        };
        if source_index as usize >= self.sources.len() {
            return Err("sourceIndex is out of range".to_string());
        }
        if self.has_pending
            && !self.is_new_generated_position(generated_line, generated_character)
            && !self.has_pending_source
        {
            return Ok(());
        }
        self.add_mapping(
            generated_line,
            generated_character,
            Some((source_index, source_line, source_character, None)),
        );
        Ok(())
    }

    // generator.go:299
    // Adds a mapping with source and name information
    pub fn add_named_source_mapping(
        &mut self,
        generated_line: u32,
        generated_character: UTF16Offset,
        source_index: SourceIndex,
        source_line: u32,
        source_character: UTF16Offset,
        name_index: NameIndex,
    ) -> Result<(), String> {
        if generated_line < self.pending_generated_line {
            return Err("generatedLine cannot backtrack".to_string());
        }
        let Some(source_index) = source_index.get() else {
            return Err("sourceIndex is out of range".to_string());
        };
        if source_index as usize >= self.sources.len() {
            return Err("sourceIndex is out of range".to_string());
        }
        let Some(name_index) = name_index.get() else {
            return Err("nameIndex is out of range".to_string());
        };
        if name_index as usize >= self.names.len() {
            return Err("nameIndex is out of range".to_string());
        }
        if self.has_pending
            && !self.is_new_generated_position(generated_line, generated_character)
            && !self.has_pending_source
        {
            return Ok(());
        }
        self.add_mapping(
            generated_line,
            generated_character,
            Some((source_index, source_line, source_character, Some(name_index))),
        );
        Ok(())
    }

    // generator.go:326
    // Gets the source map as a `RawSourceMap` object
    pub fn raw_source_map(&mut self) -> RawSourceMap {
        self.commit_pending_mapping();
        RawSourceMap {
            version: 3,
            file: self.file.clone(),
            source_root: self.source_root.clone(),
            sources: self.sources.clone(),
            names: self.names.clone(),
            mappings: self.mappings.clone(),
            sources_content: self.sources_content.clone(),
        }
    }

    // generator.go:347
    fn bytes(&mut self) -> Vec<u8> {
        match json::marshal(&self.raw_source_map().to_json()) {
            Ok(buf) => buf.into_bytes(),
            Err(err) => panic!("{}", err),
        }
    }

    // generator.go:356
    // Gets the string representation of the source map
    pub fn string(&mut self) -> String {
        tsrs_core::utf8::into_string(self.bytes()).unwrap()
    }

    // generator.go:360
    pub fn base64_data_url(&mut self) -> String {
        const PREFIX: &str = "data:application/json;base64,";
        let data = self.bytes();
        let mut sb = String::with_capacity(PREFIX.len() + data.len().div_ceil(3) * 4);
        sb.push_str(PREFIX);
        base64_std_encode(&mut sb, &data);
        sb
    }
}

// Go encoding/base64 StdEncoding (padded) encoder.
fn base64_std_encode(out: &mut String, data: &[u8]) {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    out.extend(data.chunks(3).flat_map(|chunk| {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let v = (b0 << 16) | (b1 << 8) | b2;
        [
            ALPHABET[((v >> 18) & 0x3f) as usize] as char,
            ALPHABET[((v >> 12) & 0x3f) as usize] as char,
            if chunk.len() > 1 { ALPHABET[((v >> 6) & 0x3f) as usize] as char } else { '=' },
            if chunk.len() > 2 { ALPHABET[(v & 0x3f) as usize] as char } else { '=' },
        ]
    }));
}

// generator.go:372
fn base64_format_encode(value: u8) -> char {
    match value {
        0..=25 => (b'A' + value) as char,
        26..=51 => (b'a' + value - 26) as char,
        52..=61 => (b'0' + value - 52) as char,
        62 => '+',
        63 => '/',
        _ => panic!("not a base64 value"),
    }
}
