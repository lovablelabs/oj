use std::cmp::Ordering;
use std::hash::{Hash, Hasher};
use std::num::NonZeroU32;

/// A byte offset in source text.
///
/// The stored value is one greater than the offset so that [`TextPos`] can use
/// zero as the niche for its synthetic variant while remaining four bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourcePos(NonZeroU32);

impl SourcePos {
    pub const MAX: u32 = u32::MAX - 1;

    #[inline]
    pub const fn new(offset: u32) -> SourcePos {
        match NonZeroU32::new(offset.wrapping_add(1)) {
            Some(encoded) => SourcePos(encoded),
            None => source_position_overflow(),
        }
    }

    #[inline]
    pub const fn get(self) -> u32 {
        self.0.get() - 1
    }
}

impl Default for SourcePos {
    fn default() -> Self {
        SourcePos::new(0)
    }
}

impl std::fmt::Debug for SourcePos {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.get().fmt(f)
    }
}

impl From<SourcePos> for u32 {
    fn from(value: SourcePos) -> Self {
        value.get()
    }
}

/// A location in source text, or a location belonging to a synthesized node.
///
/// Keep `Synthetic` first: it preserves the useful ordering of the old `-1`
/// sentinel before every real source offset.
#[derive(Clone, Copy, Debug)]
pub enum TextPos {
    Synthetic,
    Source(SourcePos),
}

impl TextPos {
    // The niche encoding already has the required order: synthetic = 0,
    // source = offset + 1. Comparing enum discriminants separately adds work
    // to every position comparison.
    #[inline]
    const fn order_key(self) -> u32 {
        match self {
            Self::Synthetic => 0,
            Self::Source(pos) => pos.0.get(),
        }
    }

    #[inline]
    pub const fn source(offset: u32) -> TextPos {
        TextPos::Source(SourcePos::new(offset))
    }

    #[inline]
    pub const fn source_offset(self) -> Option<u32> {
        match self {
            TextPos::Synthetic => None,
            TextPos::Source(pos) => Some(pos.get()),
        }
    }

    #[inline]
    pub fn expect_source(self, message: &str) -> u32 {
        match self {
            TextPos::Synthetic => synthetic_source_position(message),
            TextPos::Source(pos) => pos.get(),
        }
    }

    #[inline]
    pub const fn is_synthetic(self) -> bool {
        matches!(self, TextPos::Synthetic)
    }

    #[inline]
    #[must_use]
    pub const fn offset_by(self, offset: u32) -> TextPos {
        match self {
            TextPos::Synthetic => TextPos::Synthetic,
            TextPos::Source(pos) => {
                TextPos::source(pos.get().checked_add(offset).expect("source position overflow"))
            }
        }
    }
}

impl Ord for TextPos {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        self.order_key().cmp(&other.order_key())
    }
}

impl PartialEq for TextPos {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.order_key() == other.order_key()
    }
}

impl Eq for TextPos {}

impl Hash for TextPos {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.order_key().hash(state);
    }
}

impl PartialOrd for TextPos {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cold]
fn synthetic_source_position(message: &str) -> ! {
    panic!("{message}")
}

#[cold]
const fn source_position_overflow() -> ! {
    panic!("source position exceeds the supported range")
}

impl std::fmt::Display for TextPos {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Synthetic => f.write_str("-1"),
            Self::Source(pos) => pos.get().fmt(f),
        }
    }
}

impl Default for TextPos {
    fn default() -> Self {
        TextPos::source(0)
    }
}

impl From<SourcePos> for TextPos {
    fn from(value: SourcePos) -> Self {
        TextPos::Source(value)
    }
}

impl From<u32> for TextPos {
    fn from(value: u32) -> Self {
        TextPos::source(value)
    }
}

const _: () = assert!(std::mem::size_of::<SourcePos>() == 4);
const _: () = assert!(std::mem::size_of::<TextPos>() == 4);

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct TextRange {
    pos: TextPos,
    end: TextPos,
}

impl TextRange {
    #[inline]
    pub const fn new(pos: u32, end: u32) -> TextRange {
        TextRange { pos: TextPos::source(pos), end: TextPos::source(end) }
    }

    #[inline]
    pub const fn from_text_pos(pos: TextPos, end: TextPos) -> TextRange {
        TextRange { pos, end }
    }

    #[inline]
    pub const fn synthetic() -> TextRange {
        TextRange { pos: TextPos::Synthetic, end: TextPos::Synthetic }
    }

    #[inline]
    pub const fn pos(self) -> TextPos {
        self.pos
    }

    #[inline]
    pub const fn end(self) -> TextPos {
        self.end
    }

    #[inline]
    pub const fn source_pos(self) -> Option<u32> {
        self.pos.source_offset()
    }

    #[inline]
    pub const fn source_end(self) -> Option<u32> {
        self.end.source_offset()
    }

    #[inline]
    #[expect(
        clippy::len_without_is_empty,
        reason = "This accessor reports a position or optional diagnostic length, not a container size."
    )]
    pub fn len(self) -> Option<u32> {
        let TextPos::Source(pos) = self.pos else {
            return None;
        };
        let TextPos::Source(end) = self.end else {
            return None;
        };
        // The +1 encoding cancels in the difference; decoding both positions
        // first needlessly adjusts the operands of the checked subtraction.
        Some(end.0.get().checked_sub(pos.0.get()).expect("text range ends before it starts"))
    }

    pub const fn is_valid(self) -> bool {
        !(self.pos.is_synthetic() && self.end.is_synthetic())
    }

    pub fn contains(self, pos: TextPos) -> bool {
        pos >= self.pos && pos < self.end
    }

    pub fn contains_inclusive(self, pos: TextPos) -> bool {
        pos >= self.pos && pos <= self.end
    }

    pub fn contains_exclusive(self, pos: TextPos) -> bool {
        self.pos < pos && pos < self.end
    }

    #[must_use]
    pub const fn with_pos(self, pos: TextPos) -> TextRange {
        TextRange { pos, end: self.end }
    }

    #[must_use]
    pub const fn with_end(self, end: TextPos) -> TextRange {
        TextRange { pos: self.pos, end }
    }

    pub fn contained_by(self, t2: TextRange) -> bool {
        t2.pos <= self.pos && t2.end >= self.end
    }

    pub fn overlaps(self, t2: TextRange) -> bool {
        let start = self.pos.max(t2.pos);
        let end = self.end.min(t2.end);
        start < end
    }

    // Similar to Overlaps, but treats touching ranges as intersecting.
    // For example, [0, 5) intersects [5, 10).
    pub fn intersects(self, t2: TextRange) -> bool {
        let start = self.pos.max(t2.pos);
        let end = self.end.min(t2.end);
        start <= end
    }
}

const _: () = assert!(std::mem::size_of::<TextRange>() == 8);

#[inline]
pub const fn new_text_range(pos: u32, end: u32) -> TextRange {
    TextRange::new(pos, end)
}

#[inline]
pub const fn undefined_text_range() -> TextRange {
    TextRange::synthetic()
}

pub fn compare_text_ranges(r1: TextRange, r2: TextRange) -> Ordering {
    r1.pos.cmp(&r2.pos).then_with(|| r1.end.cmp(&r2.end))
}
