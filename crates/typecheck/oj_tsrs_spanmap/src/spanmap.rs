// Package spanmap provides bidirectional span-aware mapping between a content mapper's virtual text
// and its original, untransformed source. Unlike a source map, which records
// point correspondences and leaves spans and "no origin" implicit, a SpanMap records explicit segments
// for the parts of the virtual text that correspond to the original; positions not covered by any
// segment are synthesized (virtual content with no original counterpart). All positions are absolute
// byte offsets, matching the compiler's source `TextRange` model.
//
// Go calls the SpanMap methods on a possibly-nil `*SpanMap` and treats nil as the identity map. Here the methods
// take `&SpanMap`, and each method with a nil branch also has a free function of the same name taking
// `Option<&SpanMap>` (Rust cannot give an inherent method and an associated function one name).

use std::fmt;
use std::sync::OnceLock;

use tsrs_core::goslices;
use tsrs_core::json::{self, Value};
use tsrs_core::{P, TextRange, new_text_range};

// Kind describes how positions inside a segment relate the virtual span to the original span.
// spanmap.go:22 (Go `type Kind int32`: a decoded span map can carry any int32, which Validate reports. Named
// values are associated constants, as for `tsrs_core::ModuleKind`.)
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct Kind(pub i32);

impl Kind {
    // KindVerbatim segments are length-preserving: the virtual and original spans have the same
    // length and interior positions map 1:1 (OriginalPos = pos - VirtualStart + OriginalStart). A virtual span
    // fully within a verbatim segment maps to an exact original span.
    pub const VERBATIM: Kind = Kind(0);
    // KindAtom segments map a virtual span to an original span as a whole; interior positions are not
    // interpolatable (the lengths may differ), so positions within clamp to the segment's endpoints.
    // Used for renamed identifiers or short expressions.
    pub const ATOM: Kind = Kind(1);
    // KindAlias has atom geometry, but additionally asserts that the virtual and original texts are
    // names for the same logical entity. Diagnostic presentation may substitute the original name.
    pub const ALIAS: Kind = Kind(2);
}

bitflags::bitflags! {
    // Feature selects which language-service operations may use a segment. Diagnostics are intentionally not
    // represented: diagnostics on virtual text may not opt out of reporting. Text edits additionally require exact
    // verbatim geometry regardless of feature participation.
    // spanmap.go:41
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
    pub struct Feature: u32 {
        const Hover = 1 << 0;
        const SignatureHelp = 1 << 1;
        const Completion = 1 << 2;
        const Definition = 1 << 3;
        const TypeDefinition = 1 << 4;
        const Implementation = 1 << 5;
        const References = 1 << 6;
        const DocumentHighlights = 1 << 7;
        const Rename = 1 << 8;
        const CallHierarchy = 1 << 9;
        const CodeActions = 1 << 10;
        const Formatting = 1 << 11;
        const InlayHints = 1 << 12;
        const SemanticTokens = 1 << 13;
        const FoldingRanges = 1 << 14;
        const SelectionRanges = 1 << 15;
        const LinkedEditing = 1 << 16;
        const AutoInsert = 1 << 17;
        const DocumentSymbols = 1 << 18;
        const CodeLens = 1 << 19;
        const All = (1 << 20) - 1;
    }
}

impl Feature {
    #[expect(non_upper_case_globals, reason = "Keep the upstream bitflags constant name.")]
    pub const None: Feature = Feature::empty();
}

// spanmap.go:68
const FEATURE_MASK: Feature = Feature::All;

// Fidelity describes how faithfully a mapped span reflects the original.
// spanmap.go:71
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum Fidelity {
    // FidelityExact means the span fell entirely within a single verbatim segment and maps precisely.
    #[default]
    Exact,
    // FidelityAtom means the span fell within a single atom segment and maps to that atom's span.
    Atom,
    // FidelityApproximate means the span crossed segment boundaries; its endpoints were mapped and clamped.
    Approximate,
    // FidelityNone means the span had no original counterpart (it was entirely synthesized).
    None,
}

impl Fidelity {
    // IsExact reports whether the mapping was fully faithful — the input fell within a single verbatim span —
    // so the result maps 1:1 and can host a text edit written back to the original.
    // spanmap.go:86
    pub fn is_exact(self) -> bool {
        self == Fidelity::Exact
    }

    // IsSingleSegment reports whether the input fell within one segment, verbatim or atom, so the result is a
    // concrete location rather than a best-effort approximation across boundaries or a synthesized gap.
    // spanmap.go:92
    pub fn is_single_segment(self) -> bool {
        self == Fidelity::Exact || self == Fidelity::Atom
    }

    // IsNone reports whether the input had no original counterpart, meaning the mapped result is a synthesized
    // gap that does not correspond to any location in the original text.
    // spanmap.go:98
    pub fn is_none(self) -> bool {
        self == Fidelity::None
    }
}

// Segment maps the half-open virtual range [VirtualStart, VirtualEnd) to the half-open original range
// [OriginalStart, OriginalEnd). Features controls language-service participation; diagnostics and exact edit mapping
// deliberately bypass it.
// spanmap.go:105
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Segment {
    pub virtual_start: u32,
    pub virtual_end: u32,
    pub original_start: u32,
    pub original_end: u32,
    pub kind: Kind,
    pub features: Feature,
}

// MappedPosition is one virtual projection of an original position and its mapping fidelity.
// spanmap.go:115
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MappedPosition {
    pub position: u32,
    pub fidelity: Fidelity,
}

// MappedSpan is one virtual projection of an original range and its mapping fidelity.
// spanmap.go:121
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MappedSpan {
    pub span: TextRange,
    pub fidelity: Fidelity,
}

// SpanMap is a sparse, ordered set of segments over a content mapper's virtual text. Segments do not
// need to cover the whole text: any virtual position not inside a segment is synthesized (it has no
// original counterpart). An empty SpanMap therefore describes fully synthesized virtual text.
// spanmap.go:129
#[derive(Debug)]
pub struct SpanMap {
    segments: Vec<Segment>,

    // Go `origOnce` + `originalIndex`: the interval index used for original-to-virtual lookups, built on first use.
    original_index: OnceLock<OriginalIndex>,
}

// Validation failures. A content mapper is required to provide a valid span map; these describe the
// ways a map can be malformed, so the compiler can attribute the failure to the mapper precisely and
// point the mapper's author at the offending location.
// spanmap.go:140
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum MappingErrorKind {
    // MappingErrorKindOverlap means the segments overlap, run backwards, or extend past the end of the
    // virtual text (they must be ordered and disjoint in virtual space).
    Overlap,
    // MappingErrorKindOutOfBounds means a segment's original span lies outside the original text.
    OutOfBounds,
    // MappingErrorKindVerbatimMismatch means a verbatim segment's virtual and original text differ.
    VerbatimMismatch,
    // MappingErrorKindKind means a segment uses an unsupported mapping kind.
    Kind,
    // MappingErrorKindFeature means a feature annotation contains unsupported flags.
    Feature,
}

// MappingError describes a single span map validation failure, including the offsets involved so the mapper's
// author can locate it. VirtualPos is an offset into the virtual text; OriginalPos is an offset into the
// original content. Either may be unused (zero) depending on Kind.
// spanmap.go:159
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct MappingError {
    pub kind: MappingErrorKind,
    pub virtual_pos: u32,
    pub original_pos: u32,
}

impl MappingError {
    // Error describes the invalid mapping and the coordinate at which it was detected.
    // spanmap.go:166 (Go's default case, "content mapper produced an invalid position mapping", is for a kind
    // outside the named constants, which the Rust enum cannot hold.)
    pub fn error(&self) -> String {
        match self.kind {
            MappingErrorKind::Overlap => {
                format!(
                    "content mapper position mappings overlap or are out of order near virtual offset {}",
                    self.virtual_pos
                )
            }
            MappingErrorKind::OutOfBounds => {
                format!(
                    "content mapper position mapping points outside the original content at original offset {}",
                    self.original_pos
                )
            }
            MappingErrorKind::VerbatimMismatch => format!(
                "content mapper verbatim mapping does not match the original content at virtual offset {}, original offset {}",
                self.virtual_pos, self.original_pos
            ),
            MappingErrorKind::Kind => {
                format!(
                    "content mapper position mapping has an invalid kind at virtual offset {}",
                    self.virtual_pos
                )
            }
            MappingErrorKind::Feature => {
                format!(
                    "content mapper position mappings have invalid features near original offset {}",
                    self.original_pos
                )
            }
        }
    }
}

impl fmt::Display for MappingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.error())
    }
}

impl std::error::Error for MappingError {}

impl SpanMap {
    // Validate enforces the content-mapper span map contract against the virtual and original text: the
    // segments must be ordered and disjoint in virtual space and stay within the virtual text, every
    // original span must lie within the original text, and every verbatim segment's text must match the
    // original exactly. Gaps are allowed (they map as synthesized) and an empty map is valid. It returns the
    // first violation found, or nil if the map is valid.
    // spanmap.go:188
    pub fn validate(&self, virtual_: &str, original: &str) -> Option<MappingError> {
        let virtual_len = virtual_.len() as u32;
        let orig_len = original.len() as u32;
        let mut previous_virtual_end: u32 = 0;
        for s in &self.segments {
            if s.virtual_start < previous_virtual_end
                || s.virtual_end < s.virtual_start
                || s.virtual_end > virtual_len
            {
                return Some(MappingError {
                    kind: MappingErrorKind::Overlap,
                    virtual_pos: s.virtual_start,
                    original_pos: 0,
                });
            }
            previous_virtual_end = s.virtual_end;
            if s.original_end < s.original_start || s.original_end > orig_len {
                return Some(MappingError {
                    kind: MappingErrorKind::OutOfBounds,
                    virtual_pos: s.virtual_start,
                    original_pos: s.original_end,
                });
            }
            if s.kind != Kind::VERBATIM && s.kind != Kind::ATOM && s.kind != Kind::ALIAS {
                return Some(MappingError {
                    kind: MappingErrorKind::Kind,
                    virtual_pos: s.virtual_start,
                    original_pos: s.original_start,
                });
            }
            if s.kind == Kind::VERBATIM {
                // Go compares the byte ranges, which need not fall on character boundaries.
                if s.virtual_end - s.virtual_start != s.original_end - s.original_start
                    || virtual_.as_bytes()[s.virtual_start as usize..s.virtual_end as usize]
                        != original.as_bytes()[s.original_start as usize..s.original_end as usize]
                {
                    return Some(MappingError {
                        kind: MappingErrorKind::VerbatimMismatch,
                        virtual_pos: s.virtual_start,
                        original_pos: s.original_start,
                    });
                }
            }
            if s.features.bits() & !FEATURE_MASK.bits() != 0 {
                return Some(MappingError {
                    kind: MappingErrorKind::Feature,
                    virtual_pos: s.virtual_start,
                    original_pos: s.original_start,
                });
            }
        }
        None
    }
}

// spanmap.go:188 with a nil receiver.
pub fn validate(m: Option<&SpanMap>, virtual_: &str, original: &str) -> Option<MappingError> {
    let m = m?;
    m.validate(virtual_, original)
}

// New builds a SpanMap from segments, sorted by virtual start. Segments describe only the parts of the
// virtual text that correspond to the original; anything not covered maps as synthesized.
// spanmap.go:222 (Go's pdqsort, so segments with equal virtual starts keep Go's order.)
pub fn new(segments: &[Segment]) -> P<SpanMap> {
    let mut sorted = segments.to_vec();
    goslices::sort_func(&mut sorted, |a, b| a.virtual_start.cmp(&b.virtual_start));
    P::new(SpanMap { segments: sorted, original_index: OnceLock::new() })
}

impl SpanMap {
    // Segments returns the map's segments ordered by virtual start.
    // spanmap.go:231
    pub fn segments(&self) -> Vec<Segment> {
        self.segments.clone()
    }
}

// spanmap.go:231 with a nil receiver.

impl SpanMap {
    // VirtualToOriginalSpan maps a virtual range to an original range, along with the fidelity of the result. A virtual
    // range that lies entirely in a gap between segments (or in an empty map) is synthesized: it maps to the
    // insertion point in the original with FidelityNone.
    // spanmap.go:241
    pub fn virtual_to_original_span(&self, r: TextRange) -> (TextRange, Fidelity) {
        let (virtual_start, range_end) = source_range(r);
        let virtual_end = range_end.max(virtual_start);
        if virtual_start == virtual_end {
            let (position, fidelity) = self.virtual_to_original_position(virtual_start);
            return (new_text_range(position, position), fidelity);
        }

        let (start_idx, start_in) = self.segment_index_at(virtual_start);
        let end_probe = virtual_end - 1;
        let (end_idx, end_in) = self.segment_index_at(end_probe);

        if start_idx == end_idx && start_in == end_in {
            if start_in {
                let seg = &self.segments[start_idx.expect("an inside lookup has an index")];
                if seg.kind == Kind::VERBATIM {
                    let orig_start = clamp(
                        seg.original_start + (virtual_start - seg.virtual_start),
                        seg.original_start,
                        seg.original_end,
                    );
                    let orig_end = clamp(
                        seg.original_start + (virtual_end - seg.virtual_start),
                        orig_start,
                        seg.original_end,
                    );
                    return (new_text_range(orig_start, orig_end), Fidelity::Exact);
                }
                return (new_text_range(seg.original_start, seg.original_end), Fidelity::Atom);
            }
            // Entirely within a single synthesized gap.
            let pos = self.insertion_point(start_idx);
            return (new_text_range(pos, pos), Fidelity::None);
        }

        let orig_start = self.map_low(virtual_start, start_idx, start_in);
        let orig_end = self.map_high(virtual_end, end_idx, end_in).max(orig_start);
        (new_text_range(orig_start, orig_end), Fidelity::Approximate)
    }
}

// spanmap.go:241 with a nil receiver: a nil SpanMap maps identically.
pub fn virtual_to_original_span(m: Option<&SpanMap>, r: TextRange) -> (TextRange, Fidelity) {
    let Some(m) = m else {
        return (r, Fidelity::Exact);
    };
    m.virtual_to_original_span(r)
}

impl SpanMap {
    // VirtualToOriginalSpanForFeature maps r only when every virtual position in the non-empty range is
    // covered by contiguous segments participating in feature. A zero-length range requires its containing
    // segment to participate. Diagnostics and edit write-back intentionally use VirtualToOriginalSpan instead.
    // spanmap.go:279
    pub fn virtual_to_original_span_for_feature(
        &self,
        r: TextRange,
        feature: Feature,
    ) -> (TextRange, Fidelity) {
        let (mapped, fidelity) = self.virtual_to_original_span(r);
        if self.virtual_span_supports_feature(r, feature) {
            return (mapped, fidelity);
        }
        (mapped, Fidelity::None)
    }
}

// spanmap.go:279 with a nil receiver.

impl SpanMap {
    // spanmap.go:287
    fn virtual_span_supports_feature(&self, r: TextRange, feature: Feature) -> bool {
        let (start, range_end) = source_range(r);
        let end = range_end.max(start);
        if start == end {
            let (index, inside) = self.segment_index_at(start);
            return inside
                && supports_feature(
                    self.segments[index.expect("an inside lookup has an index")],
                    feature,
                );
        }
        let (index, inside) = self.segment_index_at(start);
        if !inside {
            return false;
        }
        let mut index = index.expect("an inside lookup has an index");
        let mut covered_through = start;
        while index < self.segments.len() && covered_through < end {
            let segment = self.segments[index];
            if segment.virtual_start > covered_through
                || segment.virtual_end <= covered_through
                || !supports_feature(segment, feature)
            {
                return false;
            }
            covered_through = segment.virtual_end;
            index += 1;
        }
        covered_through >= end
    }

    // VirtualToOriginalPosition maps a single virtual position to the corresponding original position, along with the
    // fidelity of the result. It is the single-position analog of VirtualToOriginalSpan: a position in a gap (or in an empty
    // map) is synthesized and maps to the insertion point with FidelityNone.
    // spanmap.go:313
    pub fn virtual_to_original_position(&self, pos: u32) -> (u32, Fidelity) {
        let (idx, in_) = self.segment_index_at(pos);
        if !in_ {
            return (self.insertion_point(idx), Fidelity::None);
        }
        let seg = &self.segments[idx.expect("an inside lookup has an index")];
        if seg.kind == Kind::VERBATIM {
            return (
                clamp(
                    seg.original_start + (pos - seg.virtual_start),
                    seg.original_start,
                    seg.original_end,
                ),
                Fidelity::Exact,
            );
        }
        (seg.original_start, Fidelity::Atom)
    }
}

// spanmap.go:313 with a nil receiver: a nil SpanMap maps identically.
pub fn virtual_to_original_position(m: Option<&SpanMap>, pos: u32) -> (u32, Fidelity) {
    let Some(m) = m else {
        return (pos, Fidelity::Exact);
    };
    m.virtual_to_original_position(pos)
}

impl SpanMap {
    // VirtualToOriginalPositionExact maps a position only when it is unambiguously in verbatim content.
    // A boundary touching an atom is rejected because the same virtual position can describe either side.
    // spanmap.go:330 (not a comma-ok result: the mapped position is returned with false too, as in Go.)
    pub fn virtual_to_original_position_exact(&self, pos: u32) -> (u32, bool) {
        let (mapped, fidelity) = self.virtual_to_original_position(pos);
        if fidelity != Fidelity::Exact {
            return (mapped, false);
        }
        let (index, inside) = self.segment_index_at(pos);
        let Some(index) = index.filter(|_| inside) else {
            return (mapped, false);
        };
        if self.segments[index].kind != Kind::VERBATIM {
            return (mapped, false);
        }
        if index > 0 {
            let previous = self.segments[index - 1];
            if previous.virtual_end == pos
                && (previous.kind != Kind::VERBATIM
                    || previous.original_end != self.segments[index].original_start)
            {
                return (mapped, false);
            }
        }
        (mapped, true)
    }
}

// spanmap.go:330 with a nil receiver.

impl SpanMap {
    // VirtualToOriginalPositionForFeature maps pos only when its virtual segment participates in feature.
    // Diagnostics and edit write-back intentionally use VirtualToOriginalPosition instead.
    // spanmap.go:350
    pub fn virtual_to_original_position_for_feature(
        &self,
        pos: u32,
        feature: Feature,
    ) -> (u32, Fidelity) {
        let (mapped, fidelity) = self.virtual_to_original_position(pos);
        let (index, inside) = self.segment_index_at(pos);
        if !inside
            || !supports_feature(
                self.segments[index.expect("an inside lookup has an index")],
                feature,
            )
        {
            return (mapped, Fidelity::None);
        }
        (mapped, fidelity)
    }
}

// spanmap.go:350 with a nil receiver.

impl SpanMap {
    // AliasForVirtualSpan returns the alias segment exactly covering r. Partial overlap does not qualify:
    // diagnostic text may be substituted only when the diagnostic identifies the complete virtual alias.
    // spanmap.go:364
    pub fn alias_for_virtual_span(&self, r: TextRange) -> Option<Segment> {
        let (start, end) = source_range(r);
        let (index, inside) = self.segment_index_at(start);
        if !inside {
            return None;
        }
        let segment = self.segments[index.expect("an inside lookup has an index")];
        (segment.kind == Kind::ALIAS
            && start == segment.virtual_start
            && end == segment.virtual_end)
            .then_some(segment)
    }
}

// spanmap.go:364 with a nil receiver.
pub fn alias_for_virtual_span(m: Option<&SpanMap>, r: TextRange) -> Option<Segment> {
    let m = m?;
    m.alias_for_virtual_span(r)
}

impl SpanMap {
    // segmentIndexAt returns the index of the segment containing pos and true, or, when pos lies in a gap,
    // the index of the segment immediately before pos (`None` if none) and false.
    // spanmap.go:378
    fn segment_index_at(&self, pos: u32) -> (Option<usize>, bool) {
        let idx = self.segments.partition_point(|s| s.virtual_start < pos);
        if self.segments.get(idx).is_some_and(|s| s.virtual_start == pos) {
            return (Some(idx), true);
        }
        let prev = idx.checked_sub(1);
        if let Some(prev) = prev
            && (pos < self.segments[prev].virtual_end
                || prev + 1 == self.segments.len() && pos == self.segments[prev].virtual_end)
        {
            return (Some(prev), true);
        }
        (prev, false)
    }

    // insertionPoint returns the original offset where synthesized content following segment prev sits: the
    // original end of that segment, or 0 before the first segment.
    // spanmap.go:394
    fn insertion_point(&self, prev: Option<usize>) -> u32 {
        prev.map_or(0, |prev| self.segments[prev].original_end)
    }

    // mapLow maps a virtual lower range boundary to original coordinates. A boundary in a synthesized
    // gap uses that gap's insertion point; an atom uses its original start.
    // spanmap.go:403
    fn map_low(&self, pos: u32, idx: Option<usize>, in_: bool) -> u32 {
        if !in_ {
            return self.insertion_point(idx);
        }
        let seg = &self.segments[idx.expect("an inside lookup has an index")];
        if seg.kind == Kind::VERBATIM {
            return clamp(
                seg.original_start + (pos - seg.virtual_start),
                seg.original_start,
                seg.original_end,
            );
        }
        seg.original_start
    }

    // mapHigh maps a virtual upper range boundary to original coordinates. A boundary in a synthesized
    // gap uses that gap's insertion point; an atom uses its original end.
    // spanmap.go:416
    fn map_high(&self, pos: u32, idx: Option<usize>, in_: bool) -> u32 {
        if !in_ {
            return self.insertion_point(idx);
        }
        let seg = &self.segments[idx.expect("an inside lookup has an index")];
        if seg.kind == Kind::VERBATIM {
            return clamp(
                seg.original_start + (pos - seg.virtual_start),
                seg.original_start,
                seg.original_end,
            );
        }
        seg.original_end
    }

    // OriginalToVirtualPositions returns every virtual projection of an original position whose segment
    // participates in feature. Segment ends are inclusive for point mapping, so a position shared by adjacent
    // original spans returns projections from both sides. Results are ordered by virtual position. It returns
    // no results for an uncovered position or when all touching segments reject feature.
    // spanmap.go:431
    pub fn original_to_virtual_positions(&self, pos: u32, feature: Feature) -> Vec<MappedPosition> {
        let groups = self.orig_index().segment_groups_at_original_position(pos);
        if groups.is_empty() {
            return Vec::new();
        }
        let mut results: Vec<MappedPosition> = Vec::new();
        for group in &groups {
            for &segment in &group.segments {
                if !supports_feature(segment, feature) {
                    continue;
                }
                let mut mapped = MappedPosition { position: 0, fidelity: Fidelity::Atom };
                if segment.kind == Kind::VERBATIM {
                    mapped.position = clamp(
                        segment.virtual_start + (pos - segment.original_start),
                        segment.virtual_start,
                        segment.virtual_end,
                    );
                    mapped.fidelity = Fidelity::Exact;
                } else if group.at_end {
                    mapped.position = segment.virtual_end;
                } else {
                    mapped.position = segment.virtual_start;
                }
                if !results.contains(&mapped) {
                    results.push(mapped);
                }
            }
        }
        goslices::sort_func(&mut results, |a, b| a.position.cmp(&b.position));
        results
    }
}

// spanmap.go:431 with a nil receiver: a nil SpanMap maps identically.

impl SpanMap {
    // OriginalToVirtualSpans returns every feature-compatible virtual projection of an original range.
    // A range contained by one or more segments produces one exact or atom result per matching segment.
    //
    // A range that starts in one group and ends in another can have several possible virtual ranges. For
    // example, suppose two original segments are each copied twice into the virtual text:
    //
    //  original:   [ A ][ B ]
    //                 [---)       range from inside A to inside B
    //
    //  virtual:    [ A ][ B ]      [ A ][ B ]
    //                 ^   ^          ^   ^
    //               start end      start end
    //                 1   3          11  13
    //
    // The map says that the range may start at 1 or 11 and end at 3 or 13, but it does not say which copy of A
    // belongs with which copy of B. We choose the smallest range around each possible location, producing [1,3)
    // and [11,13). We do not return [1,13), because it contains both smaller candidates and would include code
    // that may be unrelated to the original range. These cross-group results have approximate fidelity.
    // If either boundary is uncovered or disabled for feature, there are no results.
    // spanmap.go:482
    pub fn original_to_virtual_spans(&self, r: TextRange, feature: Feature) -> Vec<MappedSpan> {
        let (start, range_end) = source_range(r);
        let end = range_end.max(start);
        if start == end {
            return self
                .original_to_virtual_positions(start, feature)
                .into_iter()
                .map(|position| MappedSpan {
                    span: new_text_range(position.position, position.position),
                    fidelity: position.fidelity,
                })
                .collect();
        }
        let last_character = end - 1;
        let original_index = self.orig_index();
        let (start_segments, start_inside) = original_index.segments_at_original_position(start);
        let (end_segments, end_inside) =
            original_index.segments_at_original_position(last_character);
        if !start_inside || !end_inside {
            return Vec::new();
        }
        let mut containing: Vec<Segment> = Vec::new();
        containing.extend(
            start_segments.iter().filter_map(|&segment| {
                if end <= segment.original_end { Some(segment) } else { None }
            }),
        );
        if !containing.is_empty() {
            let mut results =
                original_to_virtual_spans_in_segments(&containing, start, end, feature);
            if !results.is_empty() {
                goslices::sort_func(&mut results, |a, b| {
                    a.span
                        .source_pos()
                        .expect("mapped spans have source positions")
                        .cmp(&b.span.source_pos().expect("mapped spans have source positions"))
                });
                return results;
            }
        }
        let mut starts = original_start_projections(&start_segments, start, feature);
        let mut ends = original_end_projections(&end_segments, end, feature);
        if starts.is_empty() || ends.is_empty() {
            return Vec::new();
        }
        starts.sort_unstable();
        ends.sort_unstable();
        let mut results = Vec::with_capacity(starts.len().min(ends.len()));
        results.extend(starts.iter().enumerate().filter_map(|(i, &virtual_start)| {
            // Go `slices.BinarySearch(ends, virtualStart)`.
            let end_index = ends.partition_point(|&e| e < virtual_start);
            if end_index == ends.len() || i + 1 < starts.len() && starts[i + 1] <= ends[end_index] {
                return None;
            }
            Some(MappedSpan {
                span: new_text_range(virtual_start, ends[end_index]),
                fidelity: Fidelity::Approximate,
            })
        }));
        results
    }
}

// spanmap.go:482 with a nil receiver: a nil SpanMap maps identically.
pub fn original_to_virtual_spans(
    m: Option<&SpanMap>,
    r: TextRange,
    feature: Feature,
) -> Vec<MappedSpan> {
    let Some(m) = m else {
        return vec![MappedSpan { span: r, fidelity: Fidelity::Exact }];
    };
    m.original_to_virtual_spans(r, feature)
}

impl SpanMap {
    // OriginalToVirtualIntersectingSpans maps every feature-enabled segment intersection with r.
    // Unlike OriginalToVirtualSpans, uncovered range endpoints do not suppress covered interior segments.
    // spanmap.go:539
    pub fn original_to_virtual_intersecting_spans(
        &self,
        r: TextRange,
        feature: Feature,
    ) -> Vec<MappedSpan> {
        let (range_start, range_end) = source_range(r);
        if range_start == range_end {
            return self.original_to_virtual_spans(r, feature);
        }
        let mut results = Vec::new();
        results.extend(self.segments.iter().filter_map(|&segment| {
            if !supports_feature(segment, feature) {
                return None;
            }
            let start = range_start.max(segment.original_start);
            let end = range_end.min(segment.original_end);
            if start >= end {
                return None;
            }
            if segment.kind == Kind::VERBATIM {
                Some(MappedSpan {
                    span: new_text_range(
                        segment.virtual_start + (start - segment.original_start),
                        segment.virtual_start + (end - segment.original_start),
                    ),
                    fidelity: Fidelity::Exact,
                })
            } else {
                Some(MappedSpan {
                    span: new_text_range(segment.virtual_start, segment.virtual_end),
                    fidelity: Fidelity::Atom,
                })
            }
        }));
        results
    }
}

// spanmap.go:539 with a nil receiver: a nil SpanMap maps identically.

// originalStartProjections maps the inclusive start of an original range through every matching segment.
// Verbatim segments preserve the offset within the segment; atoms map to their virtual start.
//
// For duplicate verbatim segments, the start keeps the same relative offset in every copy:
//
//  original:       [---------)
//                     ^ start
//
//  virtual:    [---------)   [---------)
//                 ^             ^
//               result        result
// spanmap.go:585
fn original_start_projections(segments: &[Segment], start: u32, feature: Feature) -> Vec<u32> {
    let mut results = Vec::with_capacity(segments.len());
    results.extend(segments.iter().filter_map(|&segment| {
        if !supports_feature(segment, feature) {
            return None;
        }
        if segment.kind == Kind::VERBATIM {
            Some(clamp(
                segment.virtual_start + (start - segment.original_start),
                segment.virtual_start,
                segment.virtual_end,
            ))
        } else {
            Some(segment.virtual_start)
        }
    }));
    results
}

// originalEndProjections maps the exclusive end of an original range through every matching segment.
// The caller uses end-1 to find the segment containing the final character, while this helper maps the end
// boundary itself. Verbatim segments preserve that boundary; atoms map to their virtual end.
//
// The lookup uses end-1 so an end at a segment boundary selects the segment on its left, not the next one:
//
//  original:       [---------)[ next segment )
//                           ^`-- end
//                           `--- end-1
//
//  virtual:    [---------)   [---------)
//                        ^             ^
//                      result        result
// spanmap.go:613
fn original_end_projections(segments: &[Segment], end: u32, feature: Feature) -> Vec<u32> {
    let mut results = Vec::with_capacity(segments.len());
    results.extend(segments.iter().filter_map(|&segment| {
        if !supports_feature(segment, feature) {
            return None;
        }
        if segment.kind == Kind::VERBATIM {
            Some(clamp(
                segment.virtual_start + (end - segment.original_start),
                segment.virtual_start,
                segment.virtual_end,
            ))
        } else {
            Some(segment.virtual_end)
        }
    }));
    results
}

// originalToVirtualSpansInSegments maps a range fully contained by each segment.
// spanmap.go:629
fn original_to_virtual_spans_in_segments(
    segments: &[Segment],
    start: u32,
    end: u32,
    feature: Feature,
) -> Vec<MappedSpan> {
    let mut results = Vec::with_capacity(segments.len());
    results.extend(segments.iter().filter_map(|&segment| {
        if !supports_feature(segment, feature) {
            return None;
        }
        if segment.kind == Kind::VERBATIM {
            let virtual_start = clamp(
                segment.virtual_start + (start - segment.original_start),
                segment.virtual_start,
                segment.virtual_end,
            );
            let virtual_end = clamp(
                segment.virtual_start + (end - segment.original_start),
                virtual_start,
                segment.virtual_end,
            );
            Some(MappedSpan {
                span: new_text_range(virtual_start, virtual_end),
                fidelity: Fidelity::Exact,
            })
        } else {
            Some(MappedSpan {
                span: new_text_range(segment.virtual_start, segment.virtual_end),
                fidelity: Fidelity::Atom,
            })
        }
    }));
    results
}

// sameOriginalRange reports whether two segments belong to the same duplicate group.
// spanmap.go:647
fn same_original_range(left: Segment, right: Segment) -> bool {
    left.original_start == right.original_start && left.original_end == right.original_end
}

// originalIndex stores segments in original-text order and a complete binary tree whose leaves correspond
// to those segments. Each internal node stores the maximum OriginalEnd below it, allowing point lookups to
// discard a whole subtree when none of its segments can reach the queried position.
// spanmap.go:654
#[derive(Debug)]
pub struct OriginalIndex {
    segments: Vec<Segment>,
    leaf_count: usize,
    max_ends: Vec<u32>,
}

impl SpanMap {
    // origIndex builds the immutable original-text interval index on first use. Sorting dominates the O(n) tree
    // construction, so the first lookup remains O(n log n); later point lookups visit only tree branches that can
    // contain a match.
    // spanmap.go:663
    fn orig_index(&self) -> &OriginalIndex {
        self.original_index.get_or_init(|| {
            let mut segments = self.segments.clone();
            goslices::sort_func(&mut segments, |a, b| {
                a.original_start
                    .cmp(&b.original_start)
                    .then_with(|| a.original_end.cmp(&b.original_end))
                    .then_with(|| a.virtual_start.cmp(&b.virtual_start))
            });
            let mut leaf_count = 1;
            while leaf_count < segments.len() {
                leaf_count *= 2;
            }
            let mut max_ends: Vec<u32> = vec![0; 2 * leaf_count];
            for (i, segment) in segments.iter().enumerate() {
                max_ends[leaf_count + i] = segment.original_end;
            }
            for i in (1..leaf_count).rev() {
                max_ends[i] = max_ends[2 * i].max(max_ends[2 * i + 1]);
            }
            OriginalIndex { segments, leaf_count, max_ends }
        })
    }
}

impl OriginalIndex {
    // segmentsAtOriginalPosition returns every mapping segment containing the original-text position pos.
    // Segment ends are exclusive; a segment start, including a zero-length segment, is considered contained.
    // spanmap.go:693
    fn segments_at_original_position(&self, pos: u32) -> (Vec<Segment>, bool) {
        // Query intervals that contain pos strictly before their exclusive end. Segments starting exactly at pos
        // are appended separately so zero-length segments are included without preventing maxEnd <= pos pruning.
        let start = self.segments.partition_point(|s| s.original_start < pos);
        let mut results = self.segments_ending_after_position(start, pos);
        let end = self.segments.partition_point(|s| s.original_start <= pos);
        results.extend_from_slice(&self.segments[start..end]);
        let found = !results.is_empty();
        (results, found)
    }

    // segmentsEndingAfterPosition returns segments among [0, limit) whose OriginalEnd is greater than pos.
    // spanmap.go:704
    fn segments_ending_after_position(&self, limit: usize, pos: u32) -> Vec<Segment> {
        let mut results = Vec::new();
        self.collect_segments_ending_at_or_after(
            1,
            0,
            self.leaf_count,
            limit,
            pos,
            false,
            &mut results,
        );
        results
    }

    // collectSegmentsEndingAtOrAfter walks the flat max-end tree left-to-right, preserving original-text order.
    // Nodes beyond limit or whose maximum end cannot reach pos are discarded without visiting their leaves.
    // spanmap.go:712
    fn collect_segments_ending_at_or_after(
        &self,
        node: usize,
        start: usize,
        end: usize,
        limit: usize,
        pos: u32,
        include_end: bool,
        results: &mut Vec<Segment>,
    ) {
        if start >= limit || self.max_ends[node] < pos || !include_end && self.max_ends[node] == pos
        {
            return;
        }
        if end - start == 1 {
            results.push(self.segments[start]);
            return;
        }
        let middle = start + (end - start) / 2;
        self.collect_segments_ending_at_or_after(
            2 * node,
            start,
            middle,
            limit,
            pos,
            include_end,
            results,
        );
        self.collect_segments_ending_at_or_after(
            2 * node + 1,
            middle,
            end,
            limit,
            pos,
            include_end,
            results,
        );
    }
}

// spanmap.go:725 (Go keeps a subslice of the collected segments; this owns a copy.)
struct SegmentGroupAtOriginalPosition {
    segments: Vec<Segment>,
    at_end: bool,
}

impl OriginalIndex {
    // segmentGroupsAtOriginalPosition returns every group of equal-range mapping segments containing or touching
    // the original-text position pos. Segment ends are included for point mapping.
    //
    // At a shared boundary, segments ending at pos and segments starting there form separate groups:
    //
    //  original:  [--- A ---)[--- B ---)
    //                        ^ pos
    //
    //  virtual:   [ A1 ) [ A2 )    [ B1 ) [ B2 )
    //               left group       right group
    //               atEnd: true      atEnd: false
    // spanmap.go:741
    fn segment_groups_at_original_position(&self, pos: u32) -> Vec<SegmentGroupAtOriginalPosition> {
        let limit = self.segments.partition_point(|s| s.original_start <= pos);
        let mut segments = Vec::new();
        self.collect_segments_ending_at_or_after(
            1,
            0,
            self.leaf_count,
            limit,
            pos,
            true,
            &mut segments,
        );
        let mut groups = Vec::new();
        groups.extend(segments.chunk_by(|a, b| same_original_range(*a, *b)).filter_map(|group| {
            let segment = group[0];
            (pos <= segment.original_end).then(|| SegmentGroupAtOriginalPosition {
                segments: group.to_vec(),
                at_end: pos == segment.original_end && pos != segment.original_start,
            })
        }));
        groups
    }
}

// supportsFeature reports whether segment participates in feature.
// spanmap.go:764
fn supports_feature(segment: Segment, feature: Feature) -> bool {
    segment.features.intersects(feature)
}

// clamp confines v to the inclusive interval [lo, hi].
// spanmap.go:769 (not `Ord::clamp`, which panics when lo > hi.)
fn clamp(v: u32, lo: u32, hi: u32) -> u32 {
    lo.max(v.min(hi))
}

fn source_range(range: TextRange) -> (u32, u32) {
    (
        range.pos().expect_source("span maps require source ranges"),
        range.end().expect_source("span maps require source ranges"),
    )
}

// Unmarshal decodes a SpanMap from the JSON tuple form produced by an out-of-process content mapper.
// Five-element tuples omit features and are normalized to FeatureAll; six-element tuples preserve the
// explicit feature mask, including FeatureNone.
// spanmap.go:776
pub fn unmarshal(data: &[u8]) -> Result<P<SpanMap>, String> {
    let tuples = unmarshal_int32_tuples(data)?;
    let mut segments = Vec::with_capacity(tuples.len());
    segments = tuples.iter().enumerate().try_fold(segments, |mut segments, (i, t)| {
        if t.len() != 5 && t.len() != 6 {
            return Err(format!("span map segment {}: expected 5 or 6 values, got {}", i, t.len()));
        }
        let virtual_start = u32::try_from(t[0])
            .map_err(|_| format!("span map segment {i}: negative virtual start"))?;
        let virtual_len = u32::try_from(t[1])
            .map_err(|_| format!("span map segment {i}: negative virtual length"))?;
        let original_start = u32::try_from(t[2])
            .map_err(|_| format!("span map segment {i}: negative original start"))?;
        let original_len = u32::try_from(t[3])
            .map_err(|_| format!("span map segment {i}: negative original length"))?;
        let mut segment = Segment {
            virtual_start,
            virtual_end: virtual_start.wrapping_add(virtual_len),
            original_start,
            original_end: original_start.wrapping_add(original_len),
            kind: Kind(t[4]),
            features: Feature::All,
        };
        if t.len() == 6 {
            segment.features = Feature::from_bits_retain(t[5] as u32);
        }
        segments.push(segment);
        Ok::<_, String>(segments)
    })?;
    Ok(new(&segments))
}

// Go `json.Unmarshal(data, &tuples)` with `tuples [][]int32` (encoding/json/v2): null decodes to an empty slice, a
// null element of an inner slice to 0, and a number must be an integer literal in int32 range.
fn unmarshal_int32_tuples(data: &[u8]) -> Result<Vec<Vec<i32>>, String> {
    let text = tsrs_core::utf8::compat::from_utf8(data)
        .map_err(|err| format!("jsontext: invalid UTF-8: {err}"))?;
    // `json::unmarshal` keeps numbers as f64, so `1.0` and `1e2` would pass as integers. Go rejects a fraction or
    // exponent; any '.', 'e' or 'E' in an input that decodes into [][]int32 at all belongs to such a number
    // (strings and booleans are rejected anyway).
    if text.bytes().any(|b| matches!(b, b'.' | b'e' | b'E')) {
        return Err("json: cannot unmarshal a JSON value into Go [][]int32".to_string());
    }
    let int32 = |value: &Value| match value {
        Value::Null => Ok(0),
        Value::Number(n) if *n >= i32::MIN as f64 && *n <= i32::MAX as f64 => Ok(*n as i32),
        _ => Err(format!("json: cannot unmarshal JSON {} into Go int32", json_kind(value))),
    };
    match json::unmarshal(text)? {
        Value::Null => Ok(Vec::new()),
        Value::Array(tuples) => tuples
            .iter()
            .map(|tuple| match tuple {
                Value::Null => Ok(Vec::new()),
                Value::Array(values) => values.iter().map(int32).collect(),
                value => {
                    Err(format!("json: cannot unmarshal JSON {} into Go []int32", json_kind(value)))
                }
            })
            .collect(),
        value => {
            Err(format!("json: cannot unmarshal JSON {} into Go [][]int32", json_kind(&value)))
        }
    }
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) | Value::Integer(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

impl SpanMap {
    // Marshal encodes a SpanMap into the JSON tuple form. FeatureAll uses the backward-compatible five-element
    // tuple; every other feature mask is emitted as a sixth element.
    // spanmap.go:803
    pub fn marshal(&self) -> Result<Vec<u8>, String> {
        let mut tuples = Vec::with_capacity(self.segments.len());
        tuples.extend(self.segments.iter().map(|s| {
            let mut tuple = vec![
                Value::Number(s.virtual_start as f64),
                Value::Number(s.virtual_end.wrapping_sub(s.virtual_start) as f64),
                Value::Number(s.original_start as f64),
                Value::Number(s.original_end.wrapping_sub(s.original_start) as f64),
                Value::Number(s.kind.0 as f64),
            ];
            if s.features != Feature::All {
                tuple.push(Value::Number(s.features.bits() as f64));
            }
            Value::Array(tuple)
        }));
        json::marshal(&Value::Array(tuples)).map(String::into_bytes)
    }
}
