use super::rangetable::Rune;

// IsUnicodeIdentifierStart reports whether ch may begin an ECMAScript
// identifier, i.e. whether it has the Unicode ID_Start (or Other_ID_Start)
// property. unicode-id-start is pinned to Unicode 15.1.0 to match Go's tables.
pub fn is_unicode_identifier_start(ch: Rune) -> bool {
    char::from_u32(ch).is_some_and(unicode_id_start::is_id_start)
}

// IsUnicodeIdentifierPart reports whether ch may appear after the first
// character of an ECMAScript identifier, i.e. whether it has the Unicode
// ID_Continue (or Other_ID_Continue) property, which also includes ID_Start.
pub fn is_unicode_identifier_part(ch: Rune) -> bool {
    char::from_u32(ch).is_some_and(unicode_id_start::is_id_continue)
}
