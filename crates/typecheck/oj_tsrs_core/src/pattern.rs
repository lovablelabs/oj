#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PatternKind {
    #[default]
    Invalid,
    Exact,
    Wildcard(usize),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pattern {
    pub text: String,
    pub kind: PatternKind,
}

pub fn try_parse_pattern(pattern: &str) -> Pattern {
    let star_index = pattern.find('*');
    match star_index {
        None => Pattern { text: pattern.to_string(), kind: PatternKind::Exact },
        Some(i) if !pattern[i + 1..].contains('*') => {
            Pattern { text: pattern.to_string(), kind: PatternKind::Wildcard(i) }
        }
        _ => Pattern::default(),
    }
}

impl Pattern {
    pub fn is_valid(&self) -> bool {
        match self.kind {
            PatternKind::Invalid => false,
            PatternKind::Exact => true,
            PatternKind::Wildcard(star) => star < self.text.len(),
        }
    }

    pub fn matches(&self, candidate: &str) -> bool {
        match self.kind {
            PatternKind::Invalid => false,
            PatternKind::Exact => self.text == candidate,
            PatternKind::Wildcard(star) => {
                candidate.len() + 1 >= self.text.len()
                    && candidate.as_bytes().starts_with(&self.text.as_bytes()[..star])
                    && candidate.as_bytes().ends_with(&self.text.as_bytes()[star + 1..])
            }
        }
    }

    pub fn matched_text<'a>(&self, candidate: &'a str) -> &'a str {
        assert!(self.matches(candidate), "candidate does not match pattern");
        match self.kind {
            PatternKind::Exact => "",
            PatternKind::Wildcard(star) => {
                &candidate[star..candidate.len() + star + 1 - self.text.len()]
            }
            PatternKind::Invalid => unreachable!("matches() rejected an invalid pattern"),
        }
    }
}

pub fn find_best_pattern_match<T: Clone>(
    values: &[T],
    get_pattern: impl Fn(&T) -> &Pattern,
    candidate: &str,
) -> Option<T> {
    let mut best_pattern = None;
    let mut longest_match_prefix_length: Option<usize> = None;
    for value in values {
        let pattern = get_pattern(value);
        let prefix_length = match pattern.kind {
            PatternKind::Invalid => continue,
            PatternKind::Exact => None,
            PatternKind::Wildcard(star) => Some(star),
        };
        if pattern.matches(candidate)
            && match prefix_length {
                None => true,
                Some(prefix_length) => {
                    longest_match_prefix_length.is_none_or(|longest| prefix_length > longest)
                }
            }
        {
            best_pattern = Some(value.clone());
            longest_match_prefix_length = prefix_length;
        }
    }
    best_pattern
}
