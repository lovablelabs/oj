//! Port of the pinned `tsc/internal/api/encoder/stringtable.go`.

use tsrs_ast::Kind;
use tsrs_core::TextPos;

/// String offsets + string data sections.
///
/// Strings that equal the slice of file text at their node's position are stored as a range of the file text;
/// everything else is appended after it. All offsets are UTF-8 (WTF-8) byte offsets into the string data
/// section, exactly as the pinned Go encoder writes them.
pub struct StringTable<'a> {
    file_text: &'a [u8],
    other_strings: Vec<u8>,
    /// pos/end pairs
    offsets: Vec<u32>,
}

impl<'a> StringTable<'a> {
    pub(crate) fn new(file_text: &'a str, string_count: usize) -> StringTable<'a> {
        StringTable {
            file_text: file_text.as_bytes(),
            other_strings: Vec::new(),
            offsets: Vec::with_capacity(string_count * 2),
        }
    }

    /// Go `stringTable.add`. `text` may be WTF-8 (lone surrogates encoded by `encode_js_string_rune`); it is
    /// copied byte for byte.
    pub(crate) fn add(
        &mut self,
        text: &str,
        kind: Kind,
        pos: impl Into<TextPos>,
        end: impl Into<TextPos>,
    ) -> u32 {
        let index = self.offsets.len() as u32;
        let (pos, end) = (pos.into(), end.into());
        if kind == Kind::SourceFile {
            self.offsets.push(pos.expect_source("source files have source positions"));
            self.offsets.push(end.expect_source("source files have source positions"));
            return index;
        }
        let text = text.as_bytes();
        let Some((pos, end)) = pos.source_offset().zip(end.source_offset()) else {
            return self.append(text, index);
        };
        if end > pos && end as usize <= self.file_text.len() {
            // pos includes leading trivia, but we can usually infer the actual start of the
            // string from the kind and end
            let end_offset = usize::from(matches!(
                kind,
                Kind::StringLiteral | Kind::TemplateTail | Kind::NoSubstitutionTemplateLiteral
            ));
            let end = end as usize - end_offset;
            // Go would panic when the text starts before the file (it never happens for parsed nodes); fall back to appending.
            if let Some(start) =
                end.checked_sub(text.len()).filter(|&start| &self.file_text[start..end] == text)
            {
                self.offsets.push(u32::try_from(start).expect("string table offset exceeds u32"));
                self.offsets.push(u32::try_from(end).expect("string table offset exceeds u32"));
                return index;
            }
        }
        // no exact match, so we need to add it to the string table
        self.append(text, index)
    }

    fn append(&mut self, text: &[u8], index: u32) -> u32 {
        let offset = self.file_text.len() + self.other_strings.len();
        self.other_strings.extend_from_slice(text);
        self.offsets.push(offset as u32);
        self.offsets.push((offset + text.len()) as u32);
        index
    }

    /// Number of uint32 entries in the string offsets section.
    pub(crate) fn offsets_len(&self) -> usize {
        self.offsets.len()
    }

    pub(crate) fn string_length(&self) -> usize {
        self.file_text.len() + self.other_strings.len()
    }

    fn encoded_length(&self) -> usize {
        self.offsets.len() * 4 + self.string_length()
    }

    /// Appends the string offsets section followed by the string data section.
    pub(crate) fn encode_into(&self, out: &mut Vec<u8>) {
        out.reserve(self.encoded_length());
        out.extend(self.offsets.iter().flat_map(|&o| o.to_le_bytes()));
        out.extend_from_slice(self.file_text);
        out.extend_from_slice(&self.other_strings);
    }
}
