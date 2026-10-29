use rustc_hash::FxHashMap;
use tsrs_core::stringutil::{self, Rune};

use super::util::word_indices;

// index.go:12
// Named is a constraint for types that can provide their name.
pub trait Named {
    fn name(&self) -> &str;
}

// index.go:19
// Index stores entries with an index mapping uppercase letters to entries whose name
// starts with that letter, and lowercase letters to entries whose name contains a
// word starting with that letter.
#[derive(Clone)]
pub struct Index<T: Named + Clone> {
    pub entries: Vec<T>,
    index: FxHashMap<Rune, Vec<usize>>,
}

impl<T: Named + Clone> Default for Index<T> {
    fn default() -> Index<T> {
        Index { entries: Vec::new(), index: FxHashMap::default() }
    }
}

const RUNE_ERROR: Rune = 0xFFFD;

// Go `utf8.DecodeRuneInString(s)` reduced to the rune (RuneError for an empty string).
fn first_rune(s: &str) -> Rune {
    match s.chars().next() {
        Some(c) => c as Rune,
        None => RUNE_ERROR,
    }
}

// Go `unicode.ToUpper` / `unicode.ToLower` (simple case mapping).
fn to_upper(r: Rune) -> Rune {
    stringutil::unicode_to_upper(r)
}

fn to_lower(r: Rune) -> Rune {
    stringutil::unicode_to_lower(r)
}

impl<T: Named + Clone> Index<T> {
    // index.go:24
    pub fn find(&self, name: &str, case_sensitive: bool) -> Vec<T> {
        if self.entries.is_empty() || name.is_empty() {
            return Vec::new();
        }
        let first = first_rune(name);
        if first == RUNE_ERROR {
            return Vec::new();
        }
        let first_rune_upper = to_upper(first);
        let Some(candidates) = self.index.get(&first_rune_upper) else {
            return Vec::new();
        };

        let mut results = Vec::new();
        results.extend(candidates.iter().filter_map(|&entry_index| {
            let entry = &self.entries[entry_index];
            let entry_name = entry.name();
            if (case_sensitive && entry_name == name)
                || (!case_sensitive && stringutil::equal_fold(entry_name, name))
            {
                Some(entry.clone())
            } else {
                None
            }
        }));

        results
    }

    // index.go:54
    // SearchWordPrefix returns each entry whose name contains a word beginning with
    // the first character of 'prefix', and whose name contains all characters
    // of 'prefix' in order (case-insensitive). If 'filter' is provided, only entries
    // for which filter(entry) returns true are included.
    pub fn search_word_prefix(&self, prefix: &str) -> Vec<T> {
        if self.entries.is_empty() {
            return Vec::new();
        }

        if prefix.is_empty() {
            return self.entries.clone();
        }

        let prefix = stringutil::go_strings_to_lower(prefix);
        let first = first_rune(&prefix);
        if first == RUNE_ERROR {
            return Vec::new();
        }

        let first_rune_upper = to_upper(first);
        let first_rune_lower = to_lower(first);

        // Look up entries that have words starting with this letter
        let empty: Vec<usize> = Vec::new();
        let mut word_starts: &Vec<usize> = &empty;
        let name_starts = self.index.get(&first_rune_upper).unwrap_or(&empty);
        if first_rune_upper != first_rune_lower {
            word_starts = self.index.get(&first_rune_lower).unwrap_or(&empty);
        }
        let count = name_starts.len() + word_starts.len();
        if count == 0 {
            return Vec::new();
        }

        // Filter entries by checking if they contain all characters in order
        let mut results = Vec::with_capacity(count);
        results.extend([name_starts, word_starts].into_iter().flatten().filter_map(|&i| {
            let entry = &self.entries[i];
            contains_chars_in_order(entry.name(), &prefix).then(|| entry.clone())
        }));
        results
    }

    pub fn index_len(&self) -> usize {
        self.index.len()
    }

    // index.go:122
    // insertAsWords adds a value to the index keyed by the first letter of each word in its name.
    pub fn insert_as_words(&mut self, value: T) {
        let name = value.name().to_string();
        assert!(!name.is_empty(), "Cannot index entry with empty name");
        let entry_index = self.entries.len();
        self.entries.push(value);

        let indices = word_indices(&name);
        let mut seen_runes: FxHashMap<Rune, bool> = FxHashMap::default();

        for (i, &start) in indices.iter().enumerate() {
            let substr = &name[start..];
            let mut first = first_rune(substr);
            if first == RUNE_ERROR {
                continue;
            }
            if i == 0 {
                // Name start keyed by uppercase
                first = to_upper(first);
                self.index.entry(first).or_default().push(entry_index);
                seen_runes.insert(first, true); // (Still set seenRunes in case first character is non-alphabetic)
            } else {
                // Subsequent word starts keyed by lowercase
                first = to_lower(first);
                let seen = seen_runes.entry(first).or_default();
                if !*seen {
                    self.index.entry(first).or_default().push(entry_index);
                    *seen = true;
                }
            }
        }
    }

    // index.go:156
    // Clone creates a new Index containing only entries for which filter returns true.
    #[must_use]
    pub fn clone_filtered(&self, filter: impl Fn(&T) -> bool) -> Index<T> {
        let mut new_idx =
            Index { entries: Vec::with_capacity(self.entries.len()), index: FxHashMap::default() };

        // Build mapping from old index to new index for filtered entries
        let mut old_to_new: FxHashMap<usize, usize> = FxHashMap::default();
        for (old_index, entry) in self.entries.iter().enumerate() {
            if filter(entry) {
                let new_index = new_idx.entries.len();
                new_idx.entries.push(entry.clone());
                old_to_new.insert(old_index, new_index);
            }
        }

        // Rebuild the index with remapped indices
        // each key is re-inserted into a new map with its Vec order kept; Go ranges the map too
        new_idx.index.extend(self.index.iter().filter_map(|(&r, old_indices)| {
            let mut new_indices = Vec::with_capacity(old_indices.len());
            new_indices.extend(old_indices.iter().filter_map(|old_index| {
                if let Some(&new_index) = old_to_new.get(old_index) {
                    Some(new_index)
                } else {
                    None
                }
            }));
            if new_indices.is_empty() { None } else { Some((r, new_indices)) }
        }));

        new_idx
    }
}

// index.go:105
// containsCharsInOrder checks if str contains all characters from pattern in order (case-insensitive).
fn contains_chars_in_order(s: &str, pattern: &str) -> bool {
    let s = stringutil::go_strings_to_lower(s);
    let pattern = stringutil::go_strings_to_lower(pattern);

    let mut pattern_idx = 0;
    for ch in s.chars() {
        if pattern_idx < pattern.len() {
            let pattern_rune = pattern[pattern_idx..].chars().next().unwrap();
            if ch == pattern_rune {
                pattern_idx += pattern_rune.len_utf8();
            }
        }
    }
    pattern_idx == pattern.len()
}
