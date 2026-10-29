use std::ops::Deref;
use std::sync::Arc;

use rustc_hash::FxHashSet;
use std::sync::LazyLock;
use tsrs_ast::{Diagnostic, SourceFile};
use tsrs_core::context::Context;
use tsrs_core::debug;
use tsrs_core::stringutil;
use tsrs_core::tspath;
use tsrs_core::{P, ScriptKind, TextPos, TextRange, new_text_range};
use tsrs_diagnostics::{self as diagnostics, Category};
use tsrs_lsproto as lsproto;
use tsrs_vfs::bundled;

use super::LSPLineMap;
use crate::spanmap::{self, Feature, Fidelity, SpanMap};

pub type GetLineMap = dyn Fn(&str) -> Option<Arc<LSPLineMap>> + Send + Sync;

// converters.go:25
pub struct Converters {
    get_line_map: Box<GetLineMap>,
    position_encoding: lsproto::PositionEncodingKind,
}

// converters.go:30
#[derive(Clone, Debug)]
pub struct MappedSpan<T> {
    pub(crate) script: T,
    mapped_span: spanmap::MappedSpan,
}

impl<T> Deref for MappedSpan<T> {
    type Target = spanmap::MappedSpan;

    fn deref(&self) -> &spanmap::MappedSpan {
        &self.mapped_span
    }
}

// converters.go:35
#[derive(Clone, Debug)]
pub struct MappedPosition<T> {
    pub(crate) script: T,
    mapped_position: spanmap::MappedPosition,
}

impl<T> Deref for MappedPosition<T> {
    type Target = spanmap::MappedPosition;

    fn deref(&self) -> &spanmap::MappedPosition {
        &self.mapped_position
    }
}

// Script is a source text the converters operate over. For a content-mapped file, Text() is the content
// mapper's virtual output and SpanMap() returns the map from that output back to the original text
// (OriginalText()); virtual ranges are then automatically converted to original coordinates (see
// ToLSPRange). For an ordinary file SpanMap() is nil and OriginalText() equals Text().
// converters.go:44
pub trait Script {
    fn file_name(&self) -> &str;
    fn original_file_name(&self) -> &str;
    fn text(&self) -> &str;
    fn span_map(&self) -> Option<&SpanMap>;
    fn original_text(&self) -> &str;
}

// The language server does not support content mappers yet (phase 2 of notes/contentmappers.md): it never loads a
// content-mapped file, and its scripts report no span map.
impl Script for SourceFile {
    fn file_name(&self) -> &str {
        SourceFile::file_name(self)
    }
    fn original_file_name(&self) -> &str {
        SourceFile::original_file_name(self)
    }
    fn text(&self) -> &str {
        SourceFile::text(self)
    }
    fn span_map(&self) -> Option<&SpanMap> {
        None
    }
    fn original_text(&self) -> &str {
        SourceFile::original_text(self)
    }
}

impl Script for P<SourceFile> {
    fn file_name(&self) -> &str {
        self.get().file_name()
    }
    fn original_file_name(&self) -> &str {
        self.get().original_file_name()
    }
    fn text(&self) -> &str {
        self.get().text()
    }
    fn span_map(&self) -> Option<&SpanMap> {
        Script::span_map(self.get())
    }
    fn original_text(&self) -> &str {
        self.get().original_text()
    }
}

// The Go functions that return the `Script` interface return either the script they were given or an
// `originalTextScript`.
pub enum AnyScript<'a> {
    Script(&'a dyn Script),
    Original(OriginalTextScript<'a>),
}

impl Script for AnyScript<'_> {
    fn file_name(&self) -> &str {
        match self {
            AnyScript::Script(s) => s.file_name(),
            AnyScript::Original(s) => s.file_name(),
        }
    }
    fn original_file_name(&self) -> &str {
        match self {
            AnyScript::Script(s) => s.original_file_name(),
            AnyScript::Original(s) => s.original_file_name(),
        }
    }
    fn text(&self) -> &str {
        match self {
            AnyScript::Script(s) => s.text(),
            AnyScript::Original(s) => s.text(),
        }
    }
    fn span_map(&self) -> Option<&SpanMap> {
        match self {
            AnyScript::Script(s) => s.span_map(),
            AnyScript::Original(s) => s.span_map(),
        }
    }
    fn original_text(&self) -> &str {
        match self {
            AnyScript::Script(s) => s.original_text(),
            AnyScript::Original(s) => s.original_text(),
        }
    }
}

// converters.go:52
pub fn new_converters(
    position_encoding: lsproto::PositionEncodingKind,
    get_line_map: impl Fn(&str) -> Option<Arc<LSPLineMap>> + Send + Sync + 'static,
) -> Arc<Converters> {
    Arc::new(Converters { get_line_map: Box::new(get_line_map), position_encoding })
}

impl Converters {
    // ToLSPRange converts a range in a SourceFile (or a script read from the file system after declaration
    // mapping) to an lsproto.Range. If the file is a content-mapped virtual SourceFile, the range is mapped
    // through the file's span map and the fidelity of that mapping is returned. For normal files, the second
    // return value is FidelityExact.
    // converters.go:66
    pub(crate) fn to_lsp_range(
        &self,
        script: &dyn Script,
        text_range: TextRange,
    ) -> (lsproto::Range, Fidelity) {
        let (script, text_range, fidelity) = virtual_range_to_original(script, text_range, None);
        (
            lsproto::Range {
                start: self.position_to_line_and_character(&script, text_range.pos()),
                end: self.position_to_line_and_character(&script, text_range.end()),
            },
            fidelity,
        )
    }

    // ToLSPRangeForFeature is [Converters.ToLSPRange] for an LS feature. For a content-mapped file, it returns
    // FidelityNone unless the entire virtual range is covered by contiguous segments that participate in
    // feature; the returned range is still the best-effort mapped range. For normal files, it behaves like
    // [Converters.ToLSPRange].
    // converters.go:78
    pub(crate) fn to_lsp_range_for_feature(
        &self,
        script: &dyn Script,
        text_range: TextRange,
        feature: Feature,
    ) -> (lsproto::Range, Fidelity) {
        let (script, text_range, fidelity) =
            virtual_range_to_original(script, text_range, Some(feature));
        (
            lsproto::Range {
                start: self.position_to_line_and_character(&script, text_range.pos()),
                end: self.position_to_line_and_character(&script, text_range.end()),
            },
            fidelity,
        )
    }

    // ToLSPPosition converts a position in a SourceFile (or a script read from the file system after
    // declaration mapping) to an lsproto.Position. Positions in content-mapped files are mapped through
    // the file's span map; positions in normal files return FidelityExact.
    // converters.go:89
    pub fn to_lsp_position(
        &self,
        script: &dyn Script,
        position: TextPos,
    ) -> (lsproto::Position, Fidelity) {
        let (script, position, fidelity) = virtual_position_to_original(script, position, None);
        (self.position_to_line_and_character(&script, position), fidelity)
    }

    // ToLSPPositionForFeature is [Converters.ToLSPPosition] for an LS feature. For a content-mapped file, it returns
    // FidelityNone when the virtual position is not in a segment that participates in feature; the
    // returned position is still the best-effort mapped position. For normal files, it behaves like
    // [Converters.ToLSPPosition].
    // converters.go:98
    pub(crate) fn to_lsp_position_for_feature(
        &self,
        script: &dyn Script,
        position: TextPos,
        feature: Feature,
    ) -> (lsproto::Position, Fidelity) {
        let (script, position, fidelity) =
            virtual_position_to_original(script, position, Some(feature));
        (self.position_to_line_and_character(&script, position), fidelity)
    }

    // ToLSPLocation converts a range in a SourceFile or script to an lsproto.Location. If the file is a content-mapped
    // virtual SourceFile, the range is mapped through the file's span map and the fidelity of that mapping is returned.
    // For normal files, the second return value is FidelityExact. If the file is a supplemental output of a content mapper,
    // the file's original file name is used for the URI (e.g. App.astro.0.ts -> App.astro).
    // converters.go:107
    pub(crate) fn to_lsp_location(
        &self,
        script: &dyn Script,
        rng: TextRange,
    ) -> (lsproto::Location, Fidelity) {
        let (lsp_range, fidelity) = self.to_lsp_range(script, rng);
        (
            lsproto::Location {
                uri: file_name_to_document_uri(script.original_file_name()),
                range: lsp_range,
            },
            fidelity,
        )
    }

    // ToLSPLocationForFeature is [Converters.ToLSPLocation] for an LS feature. For a content-mapped file, it returns
    // FidelityNone when the virtual position is not in a segment that participates in feature; the
    // returned position is still the best-effort mapped position. For normal files, it behaves like
    // [Converters.ToLSPLocation].
    // converters.go:119
    pub(crate) fn to_lsp_location_for_feature(
        &self,
        script: &dyn Script,
        rng: TextRange,
        feature: Feature,
    ) -> (lsproto::Location, Fidelity) {
        let (lsp_range, fidelity) = self.to_lsp_range_for_feature(script, rng, feature);
        (
            lsproto::Location {
                uri: file_name_to_document_uri(script.original_file_name()),
                range: lsp_range,
            },
            fidelity,
        )
    }

    // FromLSPRange converts an lsproto.Range to offsets in one Script. For a content-mapped script, results
    // include each virtual projection covered by segments that participate in feature; it returns no
    // results when no projection qualifies. Normal scripts return one exact span.
    // converters.go:127
    #[expect(
        clippy::wrong_self_convention,
        reason = "The method name and mutable receiver match the upstream Go operation."
    )]
    pub fn from_lsp_range<T: Script + Clone>(
        &self,
        script: T,
        text_range: lsproto::Range,
        feature: Feature,
    ) -> Vec<MappedSpan<T>> {
        self.lsp_range_to_virtual_for_scripts(&[script], text_range, feature)
    }

    // FromLSPRangeForSourceFile converts an lsproto.Range to offsets in a SourceFile. When the file has
    // supplemental content-mapper outputs, results include every qualifying virtual projection across the
    // canonical and supplemental files. Projections not participating in feature are omitted.
    // converters.go:134
    #[expect(
        clippy::wrong_self_convention,
        reason = "The method name and mutable receiver match the upstream Go operation."
    )]
    pub(crate) fn from_lsp_range_for_source_file(
        &self,
        file: P<SourceFile>,
        text_range: lsproto::Range,
        feature: Feature,
    ) -> Vec<MappedSpan<P<SourceFile>>> {
        let files = source_file_projections(file);
        self.lsp_range_to_virtual_for_scripts(&files, text_range, feature)
    }

    // FromLSPRangeIntersectingForSourceFile projects every feature-enabled intersection with textRange
    // across the canonical and supplemental virtual files. Unlike FromLSPRangeForSourceFile, the original
    // range endpoints need not be mapped. This is intended for read-only range requests such as semantic
    // tokens and inlay hints, where an editor commonly asks for a viewport spanning host markup:
    //
    //  original: <template>...</template><script>const x = 1</script><style>...</style>
    //            [---------------- requested viewport ---------------------------------)
    //                                            [----------) mapped script
    //
    // The result contains the script intersection even though both viewport endpoints are outside it.
    // converters.go:149
    #[expect(
        clippy::wrong_self_convention,
        reason = "The method name and mutable receiver match the upstream Go operation."
    )]
    pub(crate) fn from_lsp_range_intersecting_for_source_file(
        &self,
        file: P<SourceFile>,
        text_range: lsproto::Range,
        feature: Feature,
    ) -> Vec<MappedSpan<P<SourceFile>>> {
        let files = source_file_projections(file);
        let mut result = Vec::with_capacity(files.len());
        for script in files {
            let Some(spans) = Script::span_map(&script) else {
                result.push(MappedSpan {
                    script,
                    mapped_span: spanmap::MappedSpan {
                        span: new_text_range(
                            self.line_and_character_to_position(&script, text_range.start),
                            self.line_and_character_to_position(&script, text_range.end),
                        ),
                        fidelity: Fidelity::Exact,
                    },
                });
                continue;
            };
            let original = OriginalTextScript {
                file_name: script.original_file_name(),
                text: script.original_text(),
            };
            let original_range = new_text_range(
                self.line_and_character_to_position(&original, text_range.start),
                self.line_and_character_to_position(&original, text_range.end),
            );
            result.extend(
                spans
                    .original_to_virtual_intersecting_spans(original_range, feature)
                    .into_iter()
                    .map(|mapped| MappedSpan { script, mapped_span: mapped }),
            );
        }
        result
    }

    // converters.go:177
    fn lsp_range_to_virtual_for_scripts<T: Script + Clone>(
        &self,
        scripts: &[T],
        text_range: lsproto::Range,
        feature: Feature,
    ) -> Vec<MappedSpan<T>> {
        let mut result = Vec::with_capacity(scripts.len());
        result.extend(scripts.iter().flat_map(|script| {
            self.lsp_range_to_virtual(script, text_range, feature)
                .into_iter()
                .map(|mapped| MappedSpan { script: script.clone(), mapped_span: mapped })
        }));
        result
    }

    // converters.go:187
    fn lsp_range_to_virtual(
        &self,
        script: &dyn Script,
        text_range: lsproto::Range,
        feature: Feature,
    ) -> Vec<spanmap::MappedSpan> {
        let Some(spans) = script.span_map() else {
            return vec![spanmap::MappedSpan {
                span: new_text_range(
                    self.line_and_character_to_position(script, text_range.start),
                    self.line_and_character_to_position(script, text_range.end),
                ),
                fidelity: Fidelity::Exact,
            }];
        };
        // A content-mapped script's line map is its original text's, so convert against that text and then map
        // the resulting original range forward into the virtual text.
        let original = OriginalTextScript {
            file_name: script.original_file_name(),
            text: script.original_text(),
        };
        let orig_range = new_text_range(
            self.line_and_character_to_position(&original, text_range.start),
            self.line_and_character_to_position(&original, text_range.end),
        );
        spans.original_to_virtual_spans(orig_range, feature)
    }

    // FromLSPPosition converts an lsproto.Position to offsets in one Script. For a content-mapped script,
    // results include each virtual projection whose segment participates in feature; it returns no results
    // when no projection qualifies. Normal scripts return one exact position.
    // converters.go:211
    #[expect(
        clippy::wrong_self_convention,
        reason = "The method name and mutable receiver match the upstream Go operation."
    )]
    pub fn from_lsp_position<T: Script + Clone>(
        &self,
        script: T,
        position: lsproto::Position,
        feature: Feature,
    ) -> Vec<MappedPosition<T>> {
        self.lsp_position_to_virtual_for_scripts(&[script], position, feature)
    }

    // FromLSPPositionForSourceFile converts an lsproto.Position to offsets in a SourceFile. When the file has
    // supplemental content-mapper outputs, results include every qualifying virtual projection across the
    // canonical and supplemental files. Projections not participating in feature are omitted.
    // converters.go:218
    #[expect(
        clippy::wrong_self_convention,
        reason = "The method name and mutable receiver match the upstream Go operation."
    )]
    pub(crate) fn from_lsp_position_for_source_file(
        &self,
        file: P<SourceFile>,
        position: lsproto::Position,
        feature: Feature,
    ) -> Vec<MappedPosition<P<SourceFile>>> {
        let files = source_file_projections(file);
        self.lsp_position_to_virtual_for_scripts(&files, position, feature)
    }

    // FromLSPRangeToOriginal converts an LSP range in a content-mapped document directly to original-text offsets.
    // converters.go:224
    #[expect(
        clippy::wrong_self_convention,
        reason = "The method name and mutable receiver match the upstream Go operation."
    )]
    pub(crate) fn from_lsp_range_to_original(
        &self,
        script: &dyn Script,
        text_range: lsproto::Range,
    ) -> TextRange {
        let original = OriginalTextScript {
            file_name: script.original_file_name(),
            text: script.original_text(),
        };
        new_text_range(
            self.line_and_character_to_position(&original, text_range.start),
            self.line_and_character_to_position(&original, text_range.end),
        )
    }
}

// converters.go:232
fn source_file_projections(file: P<SourceFile>) -> Vec<P<SourceFile>> {
    let supplemental = file.supplemental_source_files();
    let mut files = Vec::with_capacity(1 + supplemental.len());
    files.push(file);
    files.extend_from_slice(supplemental);
    files
}

impl Converters {
    // converters.go:239
    fn lsp_position_to_virtual_for_scripts<T: Script + Clone>(
        &self,
        scripts: &[T],
        position: lsproto::Position,
        feature: Feature,
    ) -> Vec<MappedPosition<T>> {
        let mut result = Vec::with_capacity(scripts.len());
        result.extend(scripts.iter().flat_map(|script| {
            self.lsp_position_to_virtual(script, position, feature)
                .into_iter()
                .map(|mapped| MappedPosition { script: script.clone(), mapped_position: mapped })
        }));
        result
    }

    // converters.go:249
    fn lsp_position_to_virtual(
        &self,
        script: &dyn Script,
        position: lsproto::Position,
        feature: Feature,
    ) -> Vec<spanmap::MappedPosition> {
        let Some(spans) = script.span_map() else {
            return vec![spanmap::MappedPosition {
                position: self.line_and_character_to_position(script, position),
                fidelity: Fidelity::Exact,
            }];
        };
        let original = OriginalTextScript {
            file_name: script.original_file_name(),
            text: script.original_text(),
        };
        let orig_offset = self.line_and_character_to_position(&original, position);
        spans.original_to_virtual_positions(orig_offset, feature)
    }
}

// virtualRangeToOriginal maps a content mapper's virtual range back to its original text.
// A nil feature bypasses feature filtering for diagnostics and edits.
// converters.go:261
fn virtual_range_to_original(
    script: &dyn Script,
    text_range: TextRange,
    feature: Option<Feature>,
) -> (AnyScript<'_>, TextRange, Fidelity) {
    let Some(span_map) = script.span_map() else {
        return (AnyScript::Script(script), text_range, Fidelity::Exact);
    };
    let (mapped, fidelity) = match feature {
        None => span_map.virtual_to_original_span(text_range),
        Some(feature) => span_map.virtual_to_original_span_for_feature(text_range, feature),
    };
    (
        AnyScript::Original(OriginalTextScript {
            file_name: script.original_file_name(),
            text: script.original_text(),
        }),
        mapped,
        fidelity,
    )
}

// virtualPositionToOriginal is the single-position analog of virtualRangeToOriginal.
// converters.go:276
fn virtual_position_to_original(
    script: &dyn Script,
    position: TextPos,
    feature: Option<Feature>,
) -> (AnyScript<'_>, TextPos, Fidelity) {
    let Some(span_map) = script.span_map() else {
        return (AnyScript::Script(script), position, Fidelity::Exact);
    };
    let Some(position) = position.source_offset() else {
        return (
            AnyScript::Original(OriginalTextScript {
                file_name: script.original_file_name(),
                text: script.original_text(),
            }),
            TextPos::source(0),
            Fidelity::None,
        );
    };
    let (mapped, fidelity) = match feature {
        None => span_map.virtual_to_original_position(position),
        Some(feature) => span_map.virtual_to_original_position_for_feature(position, feature),
    };
    (
        AnyScript::Original(OriginalTextScript {
            file_name: script.original_file_name(),
            text: script.original_text(),
        }),
        TextPos::source(mapped),
        fidelity,
    )
}

// converters.go:290
pub fn language_kind_to_script_kind(language_id: lsproto::LanguageKind) -> ScriptKind {
    match language_id.0 {
        "typescript" => ScriptKind::TS,
        "typescriptreact" => ScriptKind::TSX,
        "javascript" => ScriptKind::JS,
        "javascriptreact" => ScriptKind::JSX,
        "json" => ScriptKind::JSON,
        _ => ScriptKind::Unknown,
    }
}

// https://github.com/microsoft/vscode-uri/blob/edfdccd976efaf4bb8fdeca87e97c47257721729/src/uri.ts#L455
// converters.go:308 (Go strings.NewReplacer with single-byte keys: one pass, byte by byte)
fn extra_escape_replace(s: &str) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    out = s.as_bytes().iter().fold(out, |mut out, &b| {
        let replacement = match b {
            b':' => "%3A",
            b'/' => "%2F",
            b'?' => "%3F",
            b'#' => "%23",
            b'[' => "%5B",
            b']' => "%5D",
            b'@' => "%40",

            b'!' => "%21",
            b'$' => "%24",
            b'&' => "%26",
            b'\'' => "%27",
            b'(' => "%28",
            b')' => "%29",
            b'*' => "%2A",
            b'+' => "%2B",
            b',' => "%2C",
            b';' => "%3B",
            b'=' => "%3D",

            b' ' => "%20",
            _ => {
                out.push(b);
                return out;
            }
        };
        out.extend_from_slice(replacement.as_bytes());

        out
    });
    // Only ASCII bytes are replaced (by ASCII), so the result is valid UTF-8 when the input is.
    tsrs_core::utf8::into_string(out).unwrap()
}

// converters.go:332
pub fn file_name_to_document_uri(file_name: &str) -> lsproto::DocumentUri {
    if bundled::is_bundled(file_name) {
        return lsproto::DocumentUri(file_name.to_string());
    }
    if tspath::is_dynamic_file_name(file_name) {
        return lsproto::DocumentUri::dynamic_file_name_to_document_uri(file_name);
    }

    let (mut volume, file_name) = match tspath::split_volume_path(file_name) {
        Some((volume, rest)) => (volume, rest),
        None => (String::new(), file_name),
    };
    if !volume.is_empty() {
        volume = format!("/{}", extra_escape_replace(&volume));
    }

    let file_name = file_name.strip_prefix("//").unwrap_or(file_name);

    let parts: Vec<String> =
        file_name.split('/').map(|part| extra_escape_replace(&url_path_escape(part))).collect();

    lsproto::DocumentUri(format!("file://{}{}", volume, parts.join("/")))
}

// Go net/url `PathEscape` (`escape(s, encodePathSegment)`).
fn url_path_escape(s: &str) -> String {
    fn should_escape(c: u8) -> bool {
        // §2.3 Unreserved characters (alphanum)
        if c.is_ascii_alphanumeric() {
            return false;
        }
        match c {
            // §2.3 Unreserved characters (mark)
            b'-' | b'_' | b'.' | b'~' => false,
            // §2.2 Reserved characters (reserved); §3.3 for a path segment
            b'$' | b'&' | b'+' | b',' | b'/' | b':' | b';' | b'=' | b'?' | b'@' => {
                c == b'/' || c == b';' || c == b',' || c == b'?'
            }
            // Everything else must be escaped.
            _ => true,
        }
    }
    const UPPERHEX: &[u8; 16] = b"0123456789ABCDEF";
    let bytes = s.as_bytes();
    if !bytes.iter().any(|&c| should_escape(c)) {
        return s.to_string();
    }
    let mut t: Vec<u8> = Vec::with_capacity(bytes.len() * 3);
    t.extend(bytes.iter().flat_map(|&c| {
        {
            if should_escape(c) {
                [Some(b'%'), Some(UPPERHEX[(c >> 4) as usize]), Some(UPPERHEX[(c & 15) as usize])]
            } else {
                [Some(c), None, None]
            }
        }
        .into_iter()
        .flatten()
    }));
    tsrs_core::utf8::into_string(t).unwrap()
}

impl Converters {
    // converters.go:366
    pub fn line_and_character_to_position(
        &self,
        script: &dyn Script,
        line_and_character: lsproto::Position,
    ) -> u32 {
        // UTF-8/16 0-indexed line and character to UTF-8 offset
        debug::assert(
            script.span_map().is_none(),
            &[&"raw coordinate conversion requires a non-content-mapped script"],
        );

        let line_map = (self.get_line_map)(script.file_name()).expect("nil line map");

        let line = line_and_character.line as usize;
        let character = line_and_character.character;

        let text_len =
            u32::try_from(script.text().len()).expect("source text exceeds u32 positions");

        // Clamp line to valid range.
        if line >= line_map.line_starts.len() {
            return text_len;
        }

        let start = line_map.line_starts[line];

        // Determine the end of this line (start of next line, or end of text).
        let line_end = if line + 1 < line_map.line_starts.len() {
            line_map.line_starts[line + 1]
        } else {
            text_len
        };

        if line_map.ascii_only || self.position_encoding == lsproto::PositionEncodingKind::UTF8 {
            // Go keeps a UTF-8 character offset that lands inside a multi-byte character and slices strings there;
            // snap it back to the start of that character, as the UTF-16 scan below does.
            return snap_to_char_boundary(
                script.text(),
                start.saturating_add(character).min(line_end),
            );
        }

        // Scan from line start counting UTF-16 code units to find the byte position.
        // Uses DecodeRuneInString (not range + RuneLen) so that invalid UTF-8 bytes
        // advance by their actual size (1) rather than RuneLen(RuneError) == 3.
        // This matches the approach in scanner.ComputePositionOfLineAndUTF16Character.
        let mut utf16_character = 0_u32;
        let mut pos = start as usize;
        let end = line_end as usize;
        let text = script.text().as_bytes();
        while pos < end {
            let (r, size) = stringutil::decode_rune(&text[pos..]);
            let utf16_len = utf16_rune_len(r).expect("decoded rune has a UTF-16 length");
            if utf16_character + utf16_len > character {
                break;
            }
            utf16_character += utf16_len;
            pos += size;
        }

        u32::try_from(pos).expect("source position exceeds u32")
    }

    // converters.go:417
    fn position_to_line_and_character(
        &self,
        script: &dyn Script,
        position: TextPos,
    ) -> lsproto::Position {
        // UTF-8 offset to UTF-8/16 0-indexed line and character
        debug::assert(
            script.span_map().is_none(),
            &[&"raw coordinate conversion requires a non-content-mapped script"],
        );

        let text_len =
            u32::try_from(script.text().len()).expect("source text exceeds u32 positions");
        let position = position.source_offset().unwrap_or(0).min(text_len);

        let line_map = (self.get_line_map)(script.file_name()).expect("nil line map");

        let mut line: i64 = match line_map.line_starts.binary_search(&position) {
            Ok(i) => i as i64,
            Err(i) => i as i64 - 1,
        };
        line = 0.max(line.min(line_map.line_starts.len() as i64 - 1));
        let line = line as usize;

        // The current line ranges from lineMap.LineStarts[line] (or 0) to lineMap.LineStarts[line+1] (or len(text)).

        let start = line_map.line_starts[line];

        let mut character = 0_u32;
        if line_map.ascii_only || self.position_encoding == lsproto::PositionEncodingKind::UTF8 {
            character = position - start;
        } else {
            // We need to rescan the text as UTF-16 to find the character offset.
            let mut s = &script.text().as_bytes()[start as usize..position as usize];
            while !s.is_empty() {
                let (r, size) = stringutil::decode_rune(s);
                character += utf16_rune_len(r).expect("decoded rune has a UTF-16 length");
                s = &s[size..];
            }
        }

        lsproto::Position { line: line as u32, character }
    }
}

// Not in Go. Snaps a byte offset inside a multi-byte character back to the character's start and leaves any other
// offset, including one past the end of the text, as it is. Offsets a client sends outside line/character positions
// (completion item data, API positions, where the UTF-16 map sends an offset between the halves of a surrogate pair
// into the character) skip the conversion above; Go slices its strings at whatever byte they name, Rust panics.
pub(crate) fn snap_to_char_boundary(text: &str, position: u32) -> u32 {
    if position as usize >= text.len() {
        return position;
    }
    u32::try_from(text.floor_char_boundary(position as usize)).expect("source position exceeds u32")
}

// Go unicode/utf16 `RuneLen`.
fn utf16_rune_len(r: stringutil::Rune) -> Option<u32> {
    if (0..0xd800).contains(&r) || (0xe000..0x10000).contains(&r) {
        Some(1)
    } else if (0x1_0000..=0x10_ffff).contains(&r) {
        Some(2)
    } else {
        None
    }
}

// converters.go:451
struct DiagnosticOptions {
    report_style_checks_as_warnings: bool,
    related_information: bool,
    tag_value_set: Vec<lsproto::DiagnosticTag>,
    visual_studio: bool,
}

// DiagnosticToLSPPull converts a diagnostic for pull diagnostics (textDocument/diagnostic)
// converters.go:459
pub(crate) fn diagnostic_to_lsp_pull(
    ctx: &Context,
    converters: &Converters,
    diagnostic: P<Diagnostic>,
    report_style_checks_as_warnings: bool,
) -> lsproto::Diagnostic {
    let client_caps = lsproto::get_client_capabilities(ctx);
    let client_diagnostic_caps = &client_caps.text_document.diagnostic;
    diagnostic_to_lsp(
        ctx,
        converters,
        diagnostic,
        &DiagnosticOptions {
            report_style_checks_as_warnings, // !!! get through context UserPreferences
            related_information: client_diagnostic_caps.related_information,
            tag_value_set: client_diagnostic_caps.tag_support.value_set.clone(),
            visual_studio: client_caps.vs_supports_visual_studio_extensions,
        },
    )
}

// DiagnosticToLSPPush converts a diagnostic for push diagnostics (textDocument/publishDiagnostics)
// converters.go:471
pub fn diagnostic_to_lsp_push(
    ctx: &Context,
    converters: &Converters,
    diagnostic: P<Diagnostic>,
) -> lsproto::Diagnostic {
    let client_caps = lsproto::get_client_capabilities(ctx);
    let client_diagnostic_caps = &client_caps.text_document.publish_diagnostics;
    diagnostic_to_lsp(
        ctx,
        converters,
        diagnostic,
        &DiagnosticOptions {
            report_style_checks_as_warnings: false,
            related_information: client_diagnostic_caps.related_information,
            tag_value_set: client_diagnostic_caps.tag_support.value_set.clone(),
            visual_studio: client_caps.vs_supports_visual_studio_extensions,
        },
    )
}

// https://github.com/microsoft/vscode/blob/93e08afe0469712706ca4e268f778cfadf1a43ef/extensions/typescript-language-features/src/typeScriptServiceClientHost.ts#L40C7-L40C29
// converters.go:482
static STYLE_CHECK_DIAGNOSTICS: LazyLock<FxHashSet<i32>> = LazyLock::new(|| {
    [
        diagnostics::X_0_IS_DECLARED_BUT_NEVER_USED.code(),
        diagnostics::X_0_IS_DECLARED_BUT_ITS_VALUE_IS_NEVER_READ.code(),
        diagnostics::PROPERTY_0_IS_DECLARED_BUT_ITS_VALUE_IS_NEVER_READ.code(),
        diagnostics::ALL_IMPORTS_IN_IMPORT_DECLARATION_ARE_UNUSED.code(),
        diagnostics::UNREACHABLE_CODE_DETECTED.code(),
        diagnostics::UNUSED_LABEL.code(),
        diagnostics::FALLTHROUGH_CASE_IN_SWITCH.code(),
        diagnostics::NOT_ALL_CODE_PATHS_RETURN_A_VALUE.code(),
    ]
    .into_iter()
    .collect()
});

// converters.go:493
fn diagnostic_to_lsp(
    _ctx: &Context,
    converters: &Converters,
    diagnostic: P<Diagnostic>,
    opts: &DiagnosticOptions,
) -> lsproto::Diagnostic {
    // Go reads locale.FromContext(ctx) here; only English messages are ported.
    let mut severity = diagnostic_severity(diagnostic.category());

    if opts.report_style_checks_as_warnings
        && severity == lsproto::DiagnosticSeverity::ERROR
        && STYLE_CHECK_DIAGNOSTICS.contains(&diagnostic.code())
    {
        severity = lsproto::DiagnosticSeverity::WARNING;
    }

    let mut related_information: Vec<lsproto::DiagnosticRelatedInformation> = Vec::new();
    if opts.related_information {
        related_information.reserve(diagnostic.related_information().len());
        related_information.extend(diagnostic.related_information().iter().map(|&related| {
            let related_file = related.file().expect("related information without a file");
            let (script, loc) = diagnostic_script_and_range(
                related_file,
                related.loc().unwrap_or_else(|| new_text_range(0, 0)),
                related.source(),
            );
            let (mut related_range, fidelity) = converters.to_lsp_range(&script, loc);
            if fidelity.is_none() {
                // Related diagnostic information cannot omit its location. Use an explicit file-level
                // location instead of presenting the synthesized span's insertion point as related source.
                related_range = lsproto::Range::default();
            }
            lsproto::DiagnosticRelatedInformation {
                location: lsproto::Location {
                    uri: file_name_to_document_uri(related_file.original_file_name()),
                    range: related_range,
                },
                message: related.localize(),
            }
        }));
    }

    let mut tags: Vec<lsproto::DiagnosticTag> = Vec::new();
    if !opts.tag_value_set.is_empty()
        && (diagnostic.reports_unnecessary() || diagnostic.reports_deprecated())
    {
        tags.reserve(2);
        if diagnostic.reports_unnecessary()
            && opts.tag_value_set.contains(&lsproto::DiagnosticTag::UNNECESSARY)
        {
            tags.push(lsproto::DiagnosticTag::UNNECESSARY);
        }
        if diagnostic.reports_deprecated()
            && opts.tag_value_set.contains(&lsproto::DiagnosticTag::DEPRECATED)
        {
            tags.push(lsproto::DiagnosticTag::DEPRECATED);
        }
    }

    // For diagnostics without a file (e.g., program diagnostics), use a zero range
    let mut lsp_range = lsproto::Range::default();
    if let Some(file) = diagnostic.file() {
        let (script, loc) = diagnostic_script_and_range(
            file,
            diagnostic.loc().unwrap_or_else(|| new_text_range(0, 0)),
            diagnostic.source(),
        );
        let fidelity;
        (lsp_range, fidelity) = converters.to_lsp_range(&script, loc);
        if fidelity.is_none() {
            // Diagnostics must carry a range. A zero range honestly means "this file" when the
            // diagnostic arose entirely in synthesized code and has no original source span.
            lsp_range = lsproto::Range::default();
        }
    }

    let mut source_text = diagnostic.source().to_string();
    if source_text.is_empty() {
        source_text = "ts".to_string();
    }
    let code = if opts.visual_studio {
        lsproto::IntegerOrString {
            string: Some(format!("TS{}", diagnostic.code())),
            ..Default::default()
        }
    } else {
        lsproto::IntegerOrString { integer: Some(diagnostic.code()), ..Default::default() }
    };

    lsproto::Diagnostic {
        range: lsp_range,
        code: Some(code),
        severity: Some(severity),
        message: lsproto::StringOrMarkupContent {
            string: Some(message_chain_to_string(diagnostic)),
            ..Default::default()
        },
        source: Some(source_text),
        related_information: ptr_to_slice_if_non_empty(related_information),
        tags: ptr_to_slice_if_non_empty(tags),
        ..Default::default()
    }
}

// diagnosticScriptAndRange resolves the text basis and range to report a diagnostic against. For a
// content-mapped file it maps the diagnostic's virtual range back to the original text so
// the range lines up with what the editor shows; the original text's line map is already what
// getLineMap returns for the file. A range in synthesized code has no original counterpart, so it is
// surfaced at the top of the file. Non-mapped files are returned unchanged.
// converters.go:577 (Go also accepts a nil file and returns it; both callers pass a non-nil file or dereference
// it right after)
fn diagnostic_script_and_range(
    file: P<SourceFile>,
    loc: TextRange,
    source: &str,
) -> (AnyScript<'static>, TextRange) {
    let file: &'static SourceFile = file.get();
    let Some(span_map) = Script::span_map(file) else {
        return (AnyScript::Script(file), loc);
    };
    let original =
        OriginalTextScript { file_name: file.original_file_name(), text: file.original_text() };
    if !source.is_empty() {
        // A content mapper's own diagnostics already carry original-text ranges.
        return (AnyScript::Original(original), loc);
    }
    let (mapped, fidelity) = span_map.virtual_to_original_span(loc);
    if fidelity == Fidelity::None {
        // Entirely synthesized code has no original location; surface it at the top of the file.
        return (AnyScript::Original(original), new_text_range(0, 0));
    }
    (AnyScript::Original(original), mapped)
}

// originalTextScript presents a content-mapped file's original (untransformed) text as a Script, so that
// ranges already mapped into that text convert to the correct line/character positions.
// converters.go:596
pub struct OriginalTextScript<'a> {
    file_name: &'a str,
    text: &'a str,
}

impl Script for OriginalTextScript<'_> {
    // converters.go:601
    fn file_name(&self) -> &str {
        self.file_name
    }
    // converters.go:602
    fn original_file_name(&self) -> &str {
        self.file_name
    }
    // converters.go:603
    fn text(&self) -> &str {
        self.text
    }
    // converters.go:604
    fn original_text(&self) -> &str {
        self.text
    }
    // converters.go:605
    fn span_map(&self) -> Option<&SpanMap> {
        None
    }
}

// diagnosticSeverity maps a diagnostic category to its LSP severity.
// converters.go:608
fn diagnostic_severity(category: Category) -> lsproto::DiagnosticSeverity {
    match category {
        Category::Suggestion => lsproto::DiagnosticSeverity::HINT,
        Category::Message => lsproto::DiagnosticSeverity::INFORMATION,
        Category::Warning => lsproto::DiagnosticSeverity::WARNING,
        Category::Error => lsproto::DiagnosticSeverity::ERROR,
    }
}

// converters.go:621
fn message_chain_to_string(diagnostic: P<Diagnostic>) -> String {
    if diagnostic.message_chain().is_empty() {
        return diagnostic.localize();
    }
    let mut b: Vec<u8> = Vec::new();
    tsrs_compiler::diagnosticwriter::write_flattened_diagnostic_message(&mut b, diagnostic, "\n");
    tsrs_core::utf8::into_string(b).unwrap()
}

// converters.go:630
fn ptr_to_slice_if_non_empty<T>(s: Vec<T>) -> Option<Vec<T>> {
    if s.is_empty() {
        return None;
    }
    Some(s)
}
