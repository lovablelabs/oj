use crate::ScriptKind;
use crate::stringutil;
use crate::tspath;
use rustc_hash::FxHashMap;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::hash::Hash;

// Go helpers that return their input slice unchanged when nothing changes return `Cow::Borrowed`
// so callers can preserve slice identity (see `same`).

pub fn filter<T: Clone>(slice: &[T], mut f: impl FnMut(&T) -> bool) -> Cow<'_, [T]> {
    for (i, value) in slice.iter().enumerate() {
        if !f(value) {
            let mut result = slice[..i].to_vec();
            result.extend(
                slice[i + 1..]
                    .iter()
                    .filter_map(|value| if f(value) { Some(value.clone()) } else { None }),
            );
            return Cow::Owned(result);
        }
    }
    Cow::Borrowed(slice)
}

/// Go `MapNonNil`: the callback returns `None` where Go returns the zero value.
pub fn map_non_nil<T, U>(slice: &[T], f: impl FnMut(&T) -> Option<U>) -> Vec<U> {
    slice.iter().filter_map(f).collect()
}

pub fn same_map<T: Clone + PartialEq>(slice: &[T], mut f: impl FnMut(&T) -> T) -> Cow<'_, [T]> {
    for (i, value) in slice.iter().enumerate() {
        let mapped = f(value);
        if mapped != *value {
            let mut result = Vec::with_capacity(slice.len());
            result.extend_from_slice(&slice[..i]);
            result.push(mapped);
            result.extend(slice[i + 1..].iter().map(&mut f));
            return Cow::Owned(result);
        }
    }
    Cow::Borrowed(slice)
}

/// Go `Same`: slices are identical (same length and same backing storage).
pub fn same<T>(s1: &[T], s2: &[T]) -> bool {
    if s1.len() == s2.len() {
        return s1.is_empty() || std::ptr::eq(s1.as_ptr(), s2.as_ptr());
    }
    false
}

pub fn every<T>(slice: &[T], f: impl FnMut(&T) -> bool) -> bool {
    slice.iter().all(f)
}

pub fn find<T: Clone>(slice: &[T], mut f: impl FnMut(&T) -> bool) -> Option<T> {
    slice.iter().find(|v| f(v)).cloned()
}

pub fn find_last<T: Clone>(slice: &[T], mut f: impl FnMut(&T) -> bool) -> Option<T> {
    slice.iter().rev().find(|v| f(v)).cloned()
}

pub fn find_index<T>(slice: &[T], f: impl FnMut(&T) -> bool) -> Option<usize> {
    slice.iter().position(f)
}

pub fn find_last_index<T>(slice: &[T], f: impl FnMut(&T) -> bool) -> Option<usize> {
    slice.iter().rposition(f)
}

pub fn first_or_nil<T: Clone>(slice: &[T]) -> Option<T> {
    slice.first().cloned()
}

pub fn last_or_nil<T: Clone>(slice: &[T]) -> Option<T> {
    slice.last().cloned()
}

pub fn element_or_nil<T: Clone>(slice: &[T], index: usize) -> Option<T> {
    slice.get(index).cloned()
}

pub fn first_non_nil<T, U>(slice: &[T], f: impl FnMut(&T) -> Option<U>) -> Option<U> {
    slice.iter().find_map(f)
}

pub fn concatenate<'a, T: Clone>(s1: &'a [T], s2: &'a [T]) -> Cow<'a, [T]> {
    if s2.is_empty() {
        return Cow::Borrowed(s1);
    }
    if s1.is_empty() {
        return Cow::Borrowed(s2);
    }
    let mut result = Vec::with_capacity(s1.len() + s2.len());
    result.extend_from_slice(s1);
    result.extend_from_slice(s2);
    Cow::Owned(result)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpliceStart {
    FromStart(usize),
    FromEnd(usize),
}

pub fn splice<'a, T: Clone>(
    s1: &'a [T],
    start: SpliceStart,
    delete_count: usize,
    items: &[T],
) -> Cow<'a, [T]> {
    let start = match start {
        SpliceStart::FromStart(start) => start.min(s1.len()),
        SpliceStart::FromEnd(distance) => s1.len().saturating_sub(distance),
    };
    let end = start + delete_count.min(s1.len() - start);
    if start == end && items.is_empty() {
        return Cow::Borrowed(s1);
    }
    let mut result = Vec::with_capacity(s1.len() - (end - start) + items.len());
    result.extend_from_slice(&s1[..start]);
    result.extend_from_slice(items);
    result.extend_from_slice(&s1[end..]);
    Cow::Owned(result)
}

pub fn count_where<T>(slice: &[T], mut f: impl FnMut(&T) -> bool) -> usize {
    slice.iter().filter(|v| f(v)).count()
}

pub fn replace_element<T: Clone>(slice: &[T], i: usize, t: T) -> Vec<T> {
    let mut result = slice.to_vec();
    result[i] = t;
    result
}

pub fn insert_sorted<T>(slice: &mut Vec<T>, element: T, mut cmp: impl FnMut(&T, &T) -> Ordering) {
    let i = match slice.binary_search_by(|probe| cmp(probe, &element)) {
        Ok(i) => {
            // slices.BinarySearchFunc returns the first matching position.
            let mut i = i;
            i -= (0..i).rev().take_while(|&i| cmp(&slice[i], &element) == Ordering::Equal).count();
            i
        }
        Err(i) => i,
    };
    slice.insert(i, element);
}

// MinAllFunc returns all minimum elements from xs according to the comparison function cmp.
pub fn min_all_func<T: Clone>(xs: &[T], mut cmp: impl FnMut(&T, &T) -> Ordering) -> Vec<T> {
    if xs.is_empty() {
        return Vec::new();
    }

    let mut m = xs[0].clone();
    let mut mins = vec![m.clone()];

    for x in &xs[1..] {
        let c = cmp(x, &m);
        if c == Ordering::Less {
            m = x.clone();
            mins.clear();
            mins.push(x.clone());
        } else if c == Ordering::Equal {
            mins.push(x.clone());
        }
    }

    mins
}

pub fn append_if_unique<T: PartialEq>(slice: &mut Vec<T>, element: T) {
    if slice.contains(&element) {
        return;
    }
    slice.push(element);
}

// Returns whenTrue if b is true; otherwise, returns whenFalse. IfElse should only be used when branches are either
// constant or precomputed as both branches will be evaluated regardless as to the value of b.
#[inline]
pub fn if_else<T>(b: bool, when_true: T, when_false: T) -> T {
    if b { when_true } else { when_false }
}

// Returns value if value is not the zero value of T; Otherwise, returns defaultValue. OrElse should only be used when
// defaultValue is constant or precomputed as its argument will be evaluated regardless as to the content of value.
pub fn or_else<T: Default + PartialEq>(value: T, default_value: T) -> T {
    if value != T::default() {
        return value;
    }
    default_value
}

// Returns `a` if `a` is not `nil`; Otherwise, returns `b`. Coalesce is roughly analogous to `??` in JS, except that it
// non-shortcutting, so it is advised to only use a constant or precomputed value for `b`
#[inline]
pub fn coalesce<T>(a: Option<T>, b: Option<T>) -> Option<T> {
    a.or(b)
}

pub type ECMALineStarts = Vec<u32>;

pub fn compute_ecma_line_starts(text: &str) -> ECMALineStarts {
    let mut result = Vec::with_capacity(memchr::memchr_iter(b'\n', text.as_bytes()).count() + 1);
    compute_ecma_line_starts_seq(text, |p| {
        result.push(p);
        true
    });
    result
}

/// Go returns an `iter.Seq`; here the positions are pushed to `yield_` until it returns false.
pub fn compute_ecma_line_starts_seq(text: &str, mut yield_: impl FnMut(u32) -> bool) {
    let bytes = text.as_bytes();
    let text_len = bytes.len();
    let mut pos = 0usize;
    let mut line_start = 0usize;
    while pos < text_len {
        let b = bytes[pos];
        if b < 0x80 {
            pos += 1;
            match b {
                b'\r' | b'\n' => {
                    if b == b'\r' && pos < text_len && bytes[pos] == b'\n' {
                        pos += 1;
                    }
                    if !yield_(line_start as u32) {
                        return;
                    }
                    line_start = pos;
                }
                _ => {}
            }
        } else {
            let (ch, size) = stringutil::decode_rune(&bytes[pos..]);
            pos += size;
            if stringutil::is_line_break(ch) {
                if !yield_(line_start as u32) {
                    return;
                }
                line_start = pos;
            }
        }
    }
    yield_(line_start as u32);
}

/// Counts line starts with the same terminator rules as the line-map builder.
///
/// tsrs-only: the number of positions `compute_ecma_line_starts_seq` yields (one per line terminator, with CR
/// LF as one, plus the last line), counted with vectorized byte searches instead of the per-byte loop.
pub fn count_ecma_line_starts(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut n = 1 + memchr::memchr_iter(b'\n', bytes).count();
    n += memchr::memchr_iter(b'\r', bytes).filter(|&i| bytes.get(i + 1) != Some(&b'\n')).count();
    if !text.is_ascii() {
        // U+2028 and U+2029 are E2 80 A8 and E2 80 A9; in valid UTF-8, 0xE2 is always a lead byte.
        n += memchr::memchr_iter(0xE2, bytes)
            .filter(|&i| {
                bytes.get(i + 1) == Some(&0x80) && matches!(bytes.get(i + 2), Some(0xA8 | 0xA9))
            })
            .count();
    }
    n
}

// PositionToLineAndByteOffset returns the 0-based line and byte offset from the
// start of that line for the given byte position, using the provided line starts.
// The byte offset is a raw UTF-8 byte offset from the line start, not a UTF-16 code unit count.
pub fn position_to_line_and_byte_offset(position: u32, line_starts: &[u32]) -> (usize, u32) {
    let idx = line_starts.partition_point(|&s| s <= position);
    let line = if idx == 0 { 0 } else { idx - 1 };
    (line, position - line_starts[line])
}

// UTF16Offset represents a character offset measured in UTF-16 code units.
pub type UTF16Offset = u32;

// UTF16Len returns the number of UTF-16 code units needed to
// represent the given UTF-8 encoded string.
pub fn utf16_len(s: &str) -> UTF16Offset {
    if s.is_ascii() {
        return s.len() as UTF16Offset;
    }
    let bytes = s.as_bytes();
    for (i, &byte) in bytes.iter().enumerate() {
        if byte >= 0x80 {
            // Found non-ASCII; count the ASCII prefix, then decode the rest.
            let mut n = i as UTF16Offset;
            let mut j = i;
            while j < bytes.len() {
                let (r, size) = stringutil::decode_rune(&bytes[j..]);
                j += size;
                n += if r >= 0x10000 { 2 } else { 1 };
            }
            return n;
        }
    }
    bytes.len() as UTF16Offset
}

pub fn get_script_kind_from_file_name(file_name: &str) -> ScriptKind {
    if let Some(dot_pos) = file_name.rfind('.') {
        let ext = stringutil::go_strings_to_lower(&file_name[dot_pos..]);
        match ext.as_str() {
            tspath::EXTENSION_JS | tspath::EXTENSION_CJS | tspath::EXTENSION_MJS => {
                return ScriptKind::JS;
            }
            tspath::EXTENSION_JSX => return ScriptKind::JSX,
            tspath::EXTENSION_TS | tspath::EXTENSION_CTS | tspath::EXTENSION_MTS => {
                return ScriptKind::TS;
            }
            tspath::EXTENSION_TSX => return ScriptKind::TSX,
            tspath::EXTENSION_JSON => return ScriptKind::JSON,
            _ => {}
        }
    }
    ScriptKind::Unknown
}

// EnsureScriptKindFromFileName is like GetScriptKindFromFileName, but defaults to
// ScriptKindTS when the file name has no recognized extension (e.g. files included
// with allowNonTsExtensions), so the result is always safe to hand to the parser.
pub fn ensure_script_kind_from_file_name(file_name: &str) -> ScriptKind {
    let kind = get_script_kind_from_file_name(file_name);
    if kind != ScriptKind::Unknown {
        return kind;
    }
    ScriptKind::TS
}

// Given a name and a list of names that are *not* equal to the name, return a spelling suggestion if there is one that is close enough.
// Names less than length 3 only check for case-insensitive equality.
//
// find the candidate with the smallest Levenshtein distance,
//
//	except for candidates:
//	  * With no name
//	  * Whose length differs from the target name by more than 0.34 of the length of the name.
//	  * Whose levenshtein distance is more than 0.4 of the length of the name
//	    (0.4 allows 1 substitution/transposition for every 5 characters,
//	     and 1 insertion/deletion at 3 characters)
pub fn get_spelling_suggestion<T, S: AsRef<str>>(
    name: &str,
    candidates: impl IntoIterator<Item = T>,
    get_name: impl FnMut(&T) -> S,
    compare: impl FnMut(&T, &T) -> Ordering,
) -> Option<T> {
    get_spelling_suggestion_worker(name, candidates, get_name, compare, 0 /*maxCandidates*/)
}

pub fn get_spelling_suggestion_with_max_candidate_count<T, S: AsRef<str>>(
    name: &str,
    candidates: impl IntoIterator<Item = T>,
    get_name: impl FnMut(&T) -> S,
    compare: impl FnMut(&T, &T) -> Ordering,
    max_candidates: usize,
) -> Option<T> {
    get_spelling_suggestion_worker(name, candidates, get_name, compare, max_candidates)
}

fn to_runes(s: &str) -> Vec<stringutil::Rune> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let (r, size) = stringutil::decode_rune(&b[i..]);
        out.push(r);
        i += size;
    }
    out
}

// Go: unexported getSpellingSuggestion (renamed: collides with GetSpellingSuggestion after snake-casing).
fn get_spelling_suggestion_worker<T, S: AsRef<str>>(
    name: &str,
    candidates: impl IntoIterator<Item = T>,
    mut get_name: impl FnMut(&T) -> S,
    mut compare: impl FnMut(&T, &T) -> Ordering,
    max_candidates: usize,
) -> Option<T> {
    let target_is_ascii = name.is_ascii();
    let mut target_runes = (!target_is_ascii).then(|| to_runes(name));
    let target_rune_len = target_runes.as_ref().map_or(name.len(), Vec::len);
    let maximum_length_difference = 2usize.max((target_rune_len as f64 * 0.34) as usize);
    let mut best_distance = (target_rune_len as f64 * 0.4).floor() + 0.9; // If the best result is worse than this, don't bother.
    let mut buffers = LevenshteinBuffers::default();
    let mut best_candidate: Option<T> = None;
    let mut checked_candidates = 0;
    for candidate in candidates {
        checked_candidates += 1;
        if max_candidates > 0 && checked_candidates > max_candidates {
            return None;
        }
        let candidate_name_s = get_name(&candidate);
        let candidate_name = candidate_name_s.as_ref();
        // Go compares the candidate's byte length with the name's rune count here.
        let max_len = candidate_name.len().max(target_rune_len);
        let min_len = candidate_name.len().min(target_rune_len);
        if !candidate_name.is_empty() && max_len - min_len <= maximum_length_difference {
            if candidate_name == name {
                continue;
            }
            // Only consider candidates less than 3 characters long when they differ by case.
            // Otherwise, don't bother, since a user would usually notice differences of a 2-character name.
            if candidate_name.len() < 3 && !stringutil::equal_fold(candidate_name, name) {
                continue;
            }
            let distance = if target_is_ascii && candidate_name.is_ascii() {
                levenshtein_with_max(
                    &mut buffers,
                    name.as_bytes(),
                    candidate_name.as_bytes(),
                    best_distance,
                    |c| c.to_ascii_lowercase(),
                )
            } else {
                let target_runes = target_runes.get_or_insert_with(|| to_runes(name));
                levenshtein_with_max(
                    &mut buffers,
                    target_runes,
                    &to_runes(candidate_name),
                    best_distance,
                    stringutil::unicode_to_lower,
                )
            };
            let Some(distance) = distance else {
                continue;
            };
            assert!(distance <= best_distance); // Else `levenshteinWithMax` should return undefined
            if distance < best_distance {
                best_distance = distance;
                best_candidate = Some(candidate);
            } else if best_candidate
                .as_ref()
                .is_none_or(|best| compare(&candidate, best) == Ordering::Less)
            {
                best_candidate = Some(candidate);
            }
        }
    }
    best_candidate
}

pub fn get_spelling_suggestion_for_strings<S: AsRef<str>>(
    name: &str,
    candidates: impl IntoIterator<Item = S>,
) -> Option<S> {
    get_spelling_suggestion(
        name,
        candidates,
        |s| s.as_ref().to_string(),
        |a, b| stringutil::compare_strings_case_sensitive(a.as_ref(), b.as_ref()),
    )
}

#[derive(Default)]
pub struct LevenshteinBuffers {
    previous: Vec<f64>,
    current: Vec<f64>,
}

pub fn levenshtein_with_max<T: Copy + Eq>(
    buffers: &mut LevenshteinBuffers,
    s1: &[T],
    s2: &[T],
    max_value: f64,
    to_lowercase: impl Fn(T) -> T,
) -> Option<f64> {
    if s1.len().abs_diff(s2.len()) as f64 > max_value {
        return None;
    }
    let buffer_size = s2.len() + 1;
    buffers.previous.resize(buffer_size, 0.0);
    buffers.current.resize(buffer_size, 0.0);

    let mut previous = std::mem::take(&mut buffers.previous);
    let mut current = std::mem::take(&mut buffers.current);

    let big = max_value + 0.01;
    for (i, p) in previous.iter_mut().enumerate() {
        *p = i as f64;
    }
    for i in 1..=s1.len() {
        let c1 = s1[i - 1];
        let c1_lower = to_lowercase(c1);
        let min_j = ((i as f64 - max_value).ceil() as i64).max(1) as usize;
        let max_j = ((max_value + i as f64).floor() as i64).min(s2.len() as i64).max(0) as usize;
        let mut col_min = i as f64;
        current[0] = col_min;
        // Adjacent bands shift by one cell, so later rows can only observe the
        // sentinel immediately outside either edge of this row's band.
        if min_j > 1 && min_j <= s2.len() {
            current[min_j - 1] = big;
        }
        for j in min_j..=max_j {
            let substitution_distance = if c1_lower == to_lowercase(s2[j - 1]) {
                previous[j - 1] + 0.1
            } else {
                previous[j - 1] + 2.0
            };
            let dist = if c1 == s2[j - 1] {
                previous[j - 1]
            } else {
                (previous[j] + 1.0).min((current[j - 1] + 1.0).min(substitution_distance))
            };
            current[j] = dist;
            col_min = col_min.min(dist);
        }
        if max_j < s2.len() {
            current[max_j + 1] = big;
        }
        if col_min > max_value {
            // Give up -- everything in this column is > max and it can't get better in future columns.
            buffers.previous = previous;
            buffers.current = current;
            return None;
        }
        std::mem::swap(&mut previous, &mut current);
    }
    let res = previous[s2.len()];
    buffers.previous = previous;
    buffers.current = current;
    if res > max_value {
        return None;
    }
    Some(res)
}

pub fn index_after(s: &str, pattern: &str, start_index: usize) -> Option<usize> {
    s[start_index..].find(pattern).map(|matched| matched + start_index)
}

/// Go `strings.Index(haystack, needle)` on bytes.
///
/// Searched the way Go does: candidates by the needle's first byte and a compare, then `memmem` once too many
/// candidates fail. Building a searcher first (`str::find`, `memmem::find`) costs time linear in the needle on every
/// call, which adds up when a caller searches every file of a program for one name; a naive scan costs a compare per
/// byte.
pub fn index_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let Some((&first, _)) = needle.split_first() else {
        return Some(0);
    };
    let last_start = haystack.len().checked_sub(needle.len())?;
    let mut i = 0;
    let mut fails = 0;
    while let Some(offset) = memchr::memchr(first, &haystack[i..=last_start]) {
        let candidate = i + offset;
        if &haystack[candidate..candidate + needle.len()] == needle {
            return Some(candidate);
        }
        fails += 1;
        i = candidate + 1;
        // Go's cutover to Rabin-Karp.
        if fails > 4 + (i >> 4) {
            return memchr::memmem::find(&haystack[i..], needle).map(|found| found + i);
        }
    }
    None
}

#[expect(
    clippy::iter_over_hash_type,
    reason = "Go's DiffMapsFunc ranges over the maps too; the one caller (tsrs_project update_watches) collects watch changes, whose order is not output"
)]
#[expect(
    clippy::implicit_hasher,
    reason = "The API deliberately accepts only FxHashMap, as required by the workspace hashing policy."
)]
pub fn diff_maps_func<K: Hash + Eq, V1, V2>(
    m1: &FxHashMap<K, V1>,
    m2: &FxHashMap<K, V2>,
    mut equal_values: impl FnMut(&V1, &V2) -> bool,
    on_added: Option<impl FnMut(&K, &V2)>,
    on_removed: Option<impl FnMut(&K, &V1)>,
    on_changed: Option<impl FnMut(&K, &V1, &V2)>,
) {
    if let Some(mut on_added) = on_added {
        for (k, v2) in m2 {
            if !m1.contains_key(k) {
                on_added(k, v2);
            }
        }
    }
    if on_changed.is_none() && on_removed.is_none() {
        return;
    }
    let mut on_changed = on_changed;
    let mut on_removed = on_removed;
    for (k, v1) in m1 {
        if let Some(v2) = m2.get(k) {
            if let Some(on_changed) = on_changed.as_mut()
                && !equal_values(v1, v2)
            {
                on_changed(k, v1, v2);
            }
        } else if let Some(on_removed) = on_removed.as_mut() {
            on_removed(k, v1);
        }
    }
}

// UnorderedEqual returns true if s1 and s2 contain the same elements, regardless of order.
pub fn unordered_equal<T: Hash + Eq>(s1: &[T], s2: &[T]) -> bool {
    if s1.len() != s2.len() {
        return false;
    }
    let mut counts: FxHashMap<&T, usize> = FxHashMap::default();
    for v in s1 {
        *counts.entry(v).or_default() += 1;
    }
    for v in s2 {
        let Some(c) = counts.get_mut(v) else {
            return false;
        };
        if *c == 0 {
            return false;
        }
        *c -= 1;
    }
    true
}

pub fn deduplicate<T: Clone + PartialEq>(slice: &[T]) -> Cow<'_, [T]> {
    if slice.len() > 1 {
        for (i, value) in slice.iter().enumerate() {
            if slice[..i].contains(value) {
                let mut result = slice[..i].to_vec();
                for value in &slice[i + 1..] {
                    if !result.contains(value) {
                        result.push(value.clone());
                    }
                }
                return Cow::Owned(result);
            }
        }
    }
    Cow::Borrowed(slice)
}

pub fn deduplicate_sorted<T: Clone>(
    slice: &[T],
    mut is_equal: impl FnMut(&T, &T) -> bool,
) -> Vec<T> {
    if slice.is_empty() {
        return Vec::new();
    }
    let mut last = &slice[0];
    let mut deduplicated = vec![slice[0].clone()];
    deduplicated = slice[1..].iter().fold(deduplicated, |mut deduplicated, next| {
        if is_equal(last, next) {
            return deduplicated;
        }

        deduplicated.push(next.clone());
        last = next;

        deduplicated
    });

    deduplicated
}

// CompareBooleans treats true as greater than false.
pub fn compare_booleans(a: bool, b: bool) -> Ordering {
    a.cmp(&b)
}
