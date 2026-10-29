use tsrs_core::UTF16Offset;

use super::*;

// decoder.go:10
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Mapping {
    pub generated_line: u32,
    pub generated_character: UTF16Offset,
    pub source_index: SourceIndex,
    pub source_line: Option<u32>,
    pub source_character: Option<UTF16Offset>,
    pub name_index: NameIndex,
}

impl Mapping {
    // decoder.go:19
    pub fn equals(&self, other: &Mapping) -> bool {
        std::ptr::eq(self, other)
            || self.generated_line == other.generated_line
                && self.generated_character == other.generated_character
                && self.source_index == other.source_index
                && self.source_line == other.source_line
                && self.source_character == other.source_character
                && self.name_index == other.name_index
    }

    // decoder.go:28
    pub fn is_source_mapping(&self) -> bool {
        self.source_index != MISSING_SOURCE
            && self.source_line.is_some()
            && self.source_character.is_some()
    }
}

pub const MISSING_LINE_OR_COLUMN: Option<u32> = None;
pub const MISSING_UTF16_COLUMN: Option<UTF16Offset> = None;

// decoder.go:41
// Go allocates the returned mappings in a per-decoder arena (`mappingArena`); Mapping is a small value type here.
#[derive(Clone, Debug, Default)]
pub struct MappingsDecoder {
    mappings: String,
    done: bool,
    pos: usize,
    generated_line: u32,
    generated_character: UTF16Offset,
    source_index: u32,
    source_line: u32,
    source_character: UTF16Offset,
    name_index: u32,
    error: Option<String>,
}

// decoder.go:55
pub fn decode_mappings(mappings: &str) -> MappingsDecoder {
    MappingsDecoder { mappings: mappings.to_string(), ..Default::default() }
}

impl MappingsDecoder {
    // decoder.go:63
    pub fn pos(&self) -> usize {
        self.pos
    }

    // decoder.go:67
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    // decoder.go:71
    pub fn state(&self) -> Mapping {
        self.capture_mapping(true /*hasSource*/, true /*hasName*/)
    }

    // decoder.go:75
    pub fn values(&mut self) -> impl Iterator<Item = Mapping> + '_ {
        std::iter::from_fn(move || self.next())
    }

    // decoder.go:85
    // Go returns (value, done); None is done.
    #[expect(
        clippy::should_implement_trait,
        reason = "this is the Go source-map decoder's stateful Next API, not a general Rust iterator"
    )]
    pub fn next(&mut self) -> Option<Mapping> {
        while !self.done && self.pos < self.mappings.len() {
            let ch = self.mappings.as_bytes()[self.pos];
            if ch == b';' {
                // new line
                self.generated_line += 1;
                self.generated_character = 0;
                self.pos += 1;
                continue;
            }

            if ch == b',' {
                // Next entry is on same line - no action needed
                self.pos += 1;
                continue;
            }

            let mut has_source = false;
            let mut has_name = false;
            let delta = self.base64_vlq_format_decode()?;
            self.generated_character = match apply_delta(self.generated_character, delta) {
                Some(value) => value,
                None => {
                    return self.set_error_and_stop_iterating("Invalid generatedCharacter found");
                }
            };

            if !self.is_source_mapping_segment_end() {
                has_source = true;

                let delta = self.base64_vlq_format_decode()?;
                self.source_index = match apply_delta(self.source_index, delta) {
                    Some(value) => value,
                    None => return self.set_error_and_stop_iterating("Invalid sourceIndex found"),
                };
                if self.is_source_mapping_segment_end() {
                    return self.set_error_and_stop_iterating(
                        "Unsupported Format: No entries after sourceIndex",
                    );
                }

                let delta = self.base64_vlq_format_decode()?;
                self.source_line = match apply_delta(self.source_line, delta) {
                    Some(value) => value,
                    None => return self.set_error_and_stop_iterating("Invalid sourceLine found"),
                };
                if self.is_source_mapping_segment_end() {
                    return self.set_error_and_stop_iterating(
                        "Unsupported Format: No entries after sourceLine",
                    );
                }

                let delta = self.base64_vlq_format_decode()?;
                self.source_character = match apply_delta(self.source_character, delta) {
                    Some(value) => value,
                    None => {
                        return self.set_error_and_stop_iterating("Invalid sourceCharacter found");
                    }
                };

                if !self.is_source_mapping_segment_end() {
                    has_name = true;
                    let delta = self.base64_vlq_format_decode()?;
                    self.name_index = match apply_delta(self.name_index, delta) {
                        Some(value) => value,
                        None => {
                            return self.set_error_and_stop_iterating("Invalid nameIndex found");
                        }
                    };

                    if !self.is_source_mapping_segment_end() {
                        return self.set_error_and_stop_iterating(
                            "Unsupported Error Format: Entries after nameIndex",
                        );
                    }
                }
            }

            return Some(self.capture_mapping(has_source, has_name));
        }

        self.stop_iterating()
    }

    // decoder.go:167
    fn capture_mapping(&self, has_source: bool, has_name: bool) -> Mapping {
        Mapping {
            generated_line: self.generated_line,
            generated_character: self.generated_character,
            source_index: if has_source {
                SourceIndex::Index(self.source_index)
            } else {
                MISSING_SOURCE
            },
            source_line: has_source.then_some(self.source_line),
            source_character: has_source.then_some(self.source_character),
            name_index: if has_name { NameIndex::Index(self.name_index) } else { MISSING_NAME },
        }
    }

    // decoder.go:178
    fn stop_iterating(&mut self) -> Option<Mapping> {
        self.done = true;
        None
    }

    // decoder.go:183
    fn set_error(&mut self, err: &str) {
        self.error = Some(err.to_string());
    }

    // decoder.go:187
    fn set_error_and_stop_iterating(&mut self, err: &str) -> Option<Mapping> {
        self.set_error(err);
        self.stop_iterating()
    }

    // decoder.go:192
    // decoder.go:196
    fn is_source_mapping_segment_end(&self) -> bool {
        self.pos == self.mappings.len()
            || self.mappings.as_bytes()[self.pos] == b','
            || self.mappings.as_bytes()[self.pos] == b';'
    }

    // decoder.go:200
    // Returns Go's (64-bit) int.
    fn base64_vlq_format_decode(&mut self) -> Option<i64> {
        let mut more_digits = true;
        let mut shift_count: u32 = 0;
        let mut value: i64 = 0;
        while more_digits {
            if self.pos >= self.mappings.len() {
                self.set_error("Error in decoding base64VLQFormatDecode, past the mapping string");
                return None;
            }

            // 6 digit number
            let Some(current_byte) = base64_format_decode(self.mappings.as_bytes()[self.pos])
            else {
                self.set_error("Invalid character in VLQ");
                return None;
            };

            // If msb is set, we still have more bits to continue
            more_digits = (current_byte & 32) != 0;

            // least significant 5 bits are the next msbs in the final value.
            // (A Go shift by 64 or more yields 0.)
            value |= ((current_byte & 31) as i64).checked_shl(shift_count).unwrap_or(0);
            shift_count = shift_count.saturating_add(5);
            self.pos += 1;
        }

        // Least significant bit if 1 represents negative and rest of the msb is actual absolute value
        if (value & 1) == 0 {
            // + number
            value >>= 1;
        } else {
            // - number
            value >>= 1;
            value = value.wrapping_neg();
        }

        Some(value)
    }
}

// decoder.go:238
#[expect(clippy::redundant_pub_crate, reason = "Shared with the generator through the crate root.")]
pub(crate) fn base64_format_decode(ch: u8) -> Option<u8> {
    match ch {
        b'A'..=b'Z' => Some(ch - b'A'),
        b'a'..=b'z' => Some(ch - b'a' + 26),
        b'0'..=b'9' => Some(ch - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn apply_delta(value: u32, delta: i64) -> Option<u32> {
    i64::from(value).checked_add(delta)?.try_into().ok()
}
