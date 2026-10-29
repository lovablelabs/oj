use super::js_case_generated::{
    SPECIAL_CASING_MAPPINGS, SpecialCasingCondition, SpecialCasingMapping,
    UNICODE_CASE_IGNORABLE_RANGES, UNICODE_CASED_RANGES,
};
use super::rangetable::{Rune, push_rune, unicode_is};
use super::util::{decode_js_string_rune_bytes, encode_js_string_rune, is_surrogate};

fn special_casing_mapping(r: Rune) -> Option<&'static SpecialCasingMapping> {
    SPECIAL_CASING_MAPPINGS
        .binary_search_by_key(&r, |(k, _)| *k)
        .ok()
        .map(|i| &SPECIAL_CASING_MAPPINGS[i].1)
}

/// Appends raw bytes that are known to be (possibly WTF-8) slices of a string.
fn push_bytes(builder: &mut String, bytes: &[u8]) {
    // SAFETY: callers pass whole encoded runes taken from a string (or WTF-8 sentinels), matching Go byte strings.
    unsafe { builder.as_mut_vec().extend_from_slice(bytes) }
}

pub fn to_lower_js(str: &str) -> String {
    if let Some(ascii) = to_lower_ascii(str) {
        return ascii;
    }

    let bytes = str.as_bytes();
    let mut builder = String::with_capacity(str.len());
    // casedBefore tracks whether the most recent non-Case_Ignorable code point is
    // "cased", which is the backward half of the Final_Sigma context. We
    // accumulate it as we stream so we never have to scan (or decode) backwards.
    let mut cased_before = false;
    let mut i = 0;
    while i < bytes.len() {
        let (r, size) = decode_js_string_rune_bytes(&bytes[i..]);
        i += size;
        if is_surrogate(r) {
            // A lone surrogate has no case mapping; preserve it verbatim, matching
            // String.prototype.toLowerCase. EncodeJSStringRune restores the sentinel
            // bytes because WriteRune would re-encode the surrogate as U+FFFD.
            push_bytes(&mut builder, encode_js_string_rune(r).as_bytes());
        } else if let Some(mapping) = special_casing_mapping(r) {
            if mapping.condition == SpecialCasingCondition::FinalSigma
                && is_final_sigma_context(cased_before, bytes, i)
            {
                push_bytes(&mut builder, mapping.conditional_lower());
            } else {
                push_bytes(&mut builder, mapping.lower());
            }
        } else {
            push_rune(&mut builder, r);
        }
        if !is_unicode_case_ignorable(r) {
            cased_before = is_sigma_cased(r);
        }
    }
    builder
}

pub fn to_upper_js(str: &str) -> String {
    if let Some(ascii) = to_upper_ascii(str) {
        return ascii;
    }

    let bytes = str.as_bytes();
    let mut builder = String::with_capacity(str.len());
    let mut i = 0;
    while i < bytes.len() {
        let (r, size) = decode_js_string_rune_bytes(&bytes[i..]);
        if is_surrogate(r) {
            // A lone surrogate has no case mapping; preserve it verbatim, matching
            // String.prototype.toUpperCase. Copy the sentinel bytes directly because
            // WriteRune would re-encode the surrogate as U+FFFD.
            push_bytes(&mut builder, &bytes[i..i + size]);
        } else if let Some(mapping) = special_casing_mapping(r) {
            push_bytes(&mut builder, mapping.upper());
        } else {
            push_rune(&mut builder, r);
        }
        i += size;
    }

    builder
}

fn to_lower_ascii(str: &str) -> Option<String> {
    let mut needs_mapping = false;
    needs_mapping = str.bytes().try_fold(needs_mapping, |needs_mapping, ch| {
        if ch >= 0x80 { None } else { Some(needs_mapping || ch.is_ascii_uppercase()) }
    })?;
    if !needs_mapping {
        return Some(str.to_string());
    }
    Some(str.to_ascii_lowercase())
}

fn to_upper_ascii(str: &str) -> Option<String> {
    let mut needs_mapping = false;
    needs_mapping = str.bytes().try_fold(needs_mapping, |needs_mapping, ch| {
        if ch >= 0x80 { None } else { Some(needs_mapping || ch.is_ascii_lowercase()) }
    })?;
    if !needs_mapping {
        return Some(str.to_string());
    }
    Some(str.to_ascii_uppercase())
}

// isFinalSigmaContext reports whether a sigma at the current position is in
// Final_Sigma context: it is preceded by a cased code point and not followed by
// one. casedBefore carries the backward half (tracked incrementally by the
// caller so we never scan backwards); afterOffset is the byte offset just past
// the sigma, from which we scan forward.
fn is_final_sigma_context(cased_before: bool, str: &[u8], after_offset: usize) -> bool {
    cased_before && !has_sigma_cased_after(str, after_offset)
}

fn has_sigma_cased_after(str: &[u8], start: usize) -> bool {
    let mut i = start;
    while i < str.len() {
        let (r, size) = decode_js_string_rune_bytes(&str[i..]);
        i += size;
        if is_unicode_case_ignorable(r) {
            continue;
        }
        return is_sigma_cased(r);
    }
    false
}

fn is_sigma_cased(r: Rune) -> bool {
    unicode_is(&UNICODE_CASED_RANGES, r)
}

fn is_unicode_case_ignorable(r: Rune) -> bool {
    unicode_is(&UNICODE_CASE_IGNORABLE_RANGES, r)
}
