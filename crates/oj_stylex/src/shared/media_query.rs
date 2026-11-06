// parity: style-value-parser/src/at-queries/media-query.js (parser, normalize,
// toString) and media-query-transform.js (last-media-query-wins), both at 0.19.0.

use crate::errors::StylexError;
use crate::fxhash::FxHashMap;
use crate::jsrt::js_number_to_string;
use std::cell::RefCell;
use std::fmt;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum MediaQueryError {
    #[error("{0}")]
    Syntax(StylexError),
    // Forms our tokenizer cannot faithfully replicate (escapes, non-ASCII):
    // refusing beats guessing a serialization no oracle pin covers.
    #[error("unverified media query form: {input}")]
    Unverified { input: String },
}

impl MediaQueryError {
    fn syntax() -> Self {
        MediaQueryError::Syntax(StylexError::invalid_media_query_syntax())
    }
}

/// The exact key filter `lastMediaQueryWinsTransform` applies to style-object
/// siblings: `'@media\t…'` and bare `'@media'` pass through verbatim.
pub fn is_media_key(key: &str) -> bool {
    key.starts_with("@media ")
}

#[derive(Debug, Clone, PartialEq)]
pub enum MediaRuleValue {
    Length { value: f64, unit: String },
    Number(f64),
    // idents and calc() expressions both live as plain strings upstream
    Str(String),
    Fraction(f64, f64),
}

#[derive(Debug, Clone, PartialEq)]
pub enum MediaQueryRule {
    Keyword { key: String, not: bool, only: bool },
    WordRule(String),
    Pair { key: String, value: MediaRuleValue },
    Not(Box<MediaQueryRule>),
    And(Vec<MediaQueryRule>),
    Or(Vec<MediaQueryRule>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct MediaQuery {
    pub queries: MediaQueryRule,
}

impl MediaQuery {
    pub fn new(rule: MediaQueryRule) -> Self {
        MediaQuery {
            queries: normalize(rule),
        }
    }

    pub fn parse(input: &str) -> Result<Self, MediaQueryError> {
        if input.bytes().any(|b| b >= 0x80 || b == b'\\' || b == b'\0') {
            return Err(MediaQueryError::Unverified {
                input: input.to_string(),
            });
        }
        let tokens = tokenize(input);
        let mut parser = Parser {
            tokens: &tokens,
            pos: 0,
        };
        let Some(rule) = parser.media_query() else {
            return Err(MediaQueryError::syntax());
        };
        if parser.pos != tokens.len() {
            return Err(MediaQueryError::syntax());
        }
        Ok(MediaQuery::new(rule))
    }
}

impl fmt::Display for MediaQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "@media {}", rule_to_string(&self.queries, true))
    }
}

thread_local! {
    // The rewrite is a pure function of the sibling list, and a design-system
    // corpus repeats the same few breakpoint sets in every file.
    static TRANSFORM_MEMO: RefCell<FxHashMap<Vec<String>, Vec<String>>> =
        RefCell::new(FxHashMap::default());
}

const TRANSFORM_MEMO_CAP: usize = 1 << 16;

/// Depth>=1 sibling `@media` rewrite: earlier keys gain ANDed not-clauses of
/// later siblings; every key (last included) re-serializes via normalize.
pub fn last_media_query_wins_transform(
    sibling_keys: &[String],
) -> Result<Vec<String>, MediaQueryError> {
    if let Some(hit) = TRANSFORM_MEMO.with(|memo| memo.borrow().get(sibling_keys).cloned()) {
        return Ok(hit);
    }
    let out = transform_uncached(sibling_keys)?;
    TRANSFORM_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        if memo.len() >= TRANSFORM_MEMO_CAP {
            memo.clear();
        }
        memo.insert(sibling_keys.to_vec(), out.clone());
    });
    Ok(out)
}

fn transform_uncached(sibling_keys: &[String]) -> Result<Vec<String>, MediaQueryError> {
    let queries = sibling_keys
        .iter()
        .map(|key| MediaQuery::parse(key))
        .collect::<Result<Vec<_>, _>>()?;
    let mut out = Vec::with_capacity(queries.len());
    for (i, current) in queries.iter().enumerate() {
        let negations: Vec<MediaQueryRule> = queries[i + 1..]
            .iter()
            .map(|q| MediaQueryRule::Not(Box::new(q.queries.clone())))
            .collect();
        if negations.is_empty() {
            out.push(current.to_string());
            continue;
        }
        let combined = match &current.queries {
            MediaQueryRule::Or(rules) => MediaQueryRule::Or(
                rules
                    .iter()
                    .map(|rule| {
                        let mut branch = vec![rule.clone()];
                        branch.extend(negations.iter().cloned());
                        MediaQueryRule::And(branch)
                    })
                    .collect(),
            ),
            other => {
                let mut rules = vec![other.clone()];
                rules.extend(negations);
                MediaQueryRule::And(rules)
            }
        };
        out.push(MediaQuery::new(combined).to_string());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// serialization

fn rule_to_string(rule: &MediaQueryRule, is_top_level: bool) -> String {
    match rule {
        MediaQueryRule::Keyword { key, not, only } => {
            let prefix = if *not {
                "not "
            } else if *only {
                "only "
            } else {
                ""
            };
            let is_typed = *not || *only;
            if is_top_level || is_typed {
                format!("{prefix}{key}")
            } else {
                format!("({key})")
            }
        }
        MediaQueryRule::WordRule(key) => format!("({key})"),
        MediaQueryRule::Pair { key, value } => match value {
            MediaRuleValue::Fraction(a, b) => {
                format!(
                    "({key}: {} / {})",
                    js_number_to_string(*a),
                    js_number_to_string(*b)
                )
            }
            MediaRuleValue::Str(s) => format!("({key}: {s})"),
            MediaRuleValue::Length { value, unit } => {
                format!("({key}: {}{unit})", js_number_to_string(*value))
            }
            MediaRuleValue::Number(n) => format!("({key}: {})", js_number_to_string(*n)),
        },
        MediaQueryRule::Not(inner) => match **inner {
            MediaQueryRule::And(_) | MediaQueryRule::Or(_) | MediaQueryRule::Not(_) => {
                format!("(not ({}))", rule_to_string(inner, false))
            }
            _ => format!("(not {})", rule_to_string(inner, false)),
        },
        MediaQueryRule::And(rules) => rules
            .iter()
            .map(|r| rule_to_string(r, false))
            .collect::<Vec<_>>()
            .join(" and "),
        MediaQueryRule::Or(rules) => {
            let valid: Vec<&MediaQueryRule> = rules
                .iter()
                .filter(|r| !matches!(r, MediaQueryRule::Or(inner) if inner.is_empty()))
                .collect();
            if valid.is_empty() {
                return "not all".to_string();
            }
            if valid.len() == 1 {
                return rule_to_string(valid[0], is_top_level);
            }
            let formatted: Vec<String> = valid
                .iter()
                .map(|r| match r {
                    MediaQueryRule::And(_) | MediaQueryRule::Or(_) => {
                        let inner = rule_to_string(r, false);
                        if is_top_level {
                            inner
                        } else {
                            format!("({inner})")
                        }
                    }
                    _ => rule_to_string(r, false),
                })
                .collect();
            formatted.join(if is_top_level { ", " } else { " or " })
        }
    }
}

// ---------------------------------------------------------------------------
// normalization

fn normalize(rule: MediaQueryRule) -> MediaQueryRule {
    match rule {
        MediaQueryRule::And(rules) => {
            let mut flattened = Vec::new();
            for r in rules {
                match normalize(r) {
                    MediaQueryRule::And(inner) => flattened.extend(inner),
                    other => flattened.push(other),
                }
            }
            let merged = merge_intervals_for_and(&flattened);
            if merged.is_empty() {
                MediaQueryRule::Keyword {
                    key: "all".to_string(),
                    not: true,
                    only: false,
                }
            } else {
                MediaQueryRule::And(merged)
            }
        }
        MediaQueryRule::Or(rules) => MediaQueryRule::Or(rules.into_iter().map(normalize).collect()),
        MediaQueryRule::Not(inner) => {
            let operand = normalize(*inner);
            if let MediaQueryRule::Keyword { key, not: true, .. } = &operand
                && key == "all"
            {
                return MediaQueryRule::Keyword {
                    key: "all".to_string(),
                    not: false,
                    only: false,
                };
            }
            if let MediaQueryRule::Not(inner2) = operand {
                return normalize(*inner2);
            }
            MediaQueryRule::Not(Box::new(operand))
        }
        other => other,
    }
}

fn as_numeric_minmax(rule: &MediaQueryRule) -> Option<(&str, f64, &str)> {
    if let MediaQueryRule::Pair {
        key,
        value: MediaRuleValue::Length { value, unit },
    } = rule
        && (key == "min-width" || key == "max-width" || key == "min-height" || key == "max-height")
    {
        Some((key.as_str(), *value, unit.as_str()))
    } else {
        None
    }
}

const MERGE_EPSILON: f64 = 0.01;

fn merge_intervals_for_and(rules: &[MediaQueryRule]) -> Vec<MediaQueryRule> {
    // (not (A and B)) with exactly two operands distributes into an OR of the
    // two negated branches before any interval math happens.
    for (idx, rule) in rules.iter().enumerate() {
        if let MediaQueryRule::Not(inner) = rule
            && let MediaQueryRule::And(inner_rules) = &**inner
            && inner_rules.len() == 2
        {
            let others: Vec<MediaQueryRule> = rules
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != idx)
                .map(|(_, r)| r.clone())
                .collect();
            let branches: Vec<Vec<MediaQueryRule>> = inner_rules
                .iter()
                .map(|negated| {
                    let mut input = others.clone();
                    input.push(MediaQueryRule::Not(Box::new(negated.clone())));
                    merge_intervals_for_and(&input)
                })
                .collect();
            return vec![MediaQueryRule::Or(
                branches
                    .into_iter()
                    .filter(|branch| !branch.is_empty())
                    .map(|branch| {
                        if branch.len() == 1 {
                            branch.into_iter().next().expect("len checked")
                        } else {
                            MediaQueryRule::And(branch)
                        }
                    })
                    .collect(),
            )];
        }
    }

    let dims = ["width", "height"];
    let mut intervals: [Vec<(f64, f64)>; 2] = [Vec::new(), Vec::new()];
    let mut units: [Option<String>; 2] = [None, None];
    let mut has_unit_conflict = false;

    for rule in rules {
        let mut matched = false;
        for (d, dim) in dims.iter().enumerate() {
            let min_key = format!("min-{dim}");
            let max_key = format!("max-{dim}");
            if let Some((key, value, unit)) = as_numeric_minmax(rule)
                && (key == min_key || key == max_key)
            {
                match &units[d] {
                    None if intervals[d].is_empty() => units[d] = Some(unit.to_string()),
                    Some(u) if u != unit => has_unit_conflict = true,
                    _ => {}
                }
                intervals[d].push(if key == min_key {
                    (value, f64::INFINITY)
                } else {
                    (f64::NEG_INFINITY, value)
                });
                matched = true;
                break;
            }
            if let MediaQueryRule::Not(inner) = rule
                && let Some((key, value, unit)) = as_numeric_minmax(inner)
                && (key == min_key || key == max_key)
            {
                match &units[d] {
                    None if intervals[d].is_empty() => units[d] = Some(unit.to_string()),
                    Some(u) if u != unit => has_unit_conflict = true,
                    _ => {}
                }
                intervals[d].push(if key == min_key {
                    (f64::NEG_INFINITY, value - MERGE_EPSILON)
                } else {
                    (value + MERGE_EPSILON, f64::INFINITY)
                });
                matched = true;
                break;
            }
        }
        if !matched {
            // any rule that is not a numeric min/max pair (or its negation)
            // disables merging for the whole conjunction
            return rules.to_vec();
        }
    }

    if has_unit_conflict {
        return rules.to_vec();
    }

    let mut result = Vec::new();
    for (d, dim) in dims.iter().enumerate() {
        if intervals[d].is_empty() {
            continue;
        }
        let mut lower = f64::NEG_INFINITY;
        let mut upper = f64::INFINITY;
        for (l, u) in &intervals[d] {
            if *l > lower {
                lower = *l;
            }
            if *u < upper {
                upper = *u;
            }
        }
        if lower > upper {
            return Vec::new();
        }
        let unit = units[d].clone().unwrap_or_default();
        if lower != f64::NEG_INFINITY {
            result.push(MediaQueryRule::Pair {
                key: format!("min-{dim}"),
                value: MediaRuleValue::Length {
                    value: lower,
                    unit: unit.clone(),
                },
            });
        }
        if upper != f64::INFINITY {
            result.push(MediaQueryRule::Pair {
                key: format!("max-{dim}"),
                value: MediaRuleValue::Length { value: upper, unit },
            });
        }
    }
    if result.is_empty() {
        rules.to_vec()
    } else {
        result
    }
}

// ---------------------------------------------------------------------------
// tokenizer (CSS Syntax 3 subset; escapes/non-ASCII are gated in parse())

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Whitespace,
    Ident(String),
    AtKeyword(String),
    Function(String),
    Number(f64),
    Percentage(f64),
    Dimension { value: f64, unit: String },
    Colon,
    Comma,
    OpenParen,
    CloseParen,
    Delim(u8),
    // strings, brackets, semicolons, comments, CDO/CDC: tokens the media query
    // grammar can never match, so one opaque variant preserves the rejection
    Unsupported,
}

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0C)
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_char(b: u8) -> bool {
    is_ident_start(b) || b.is_ascii_digit() || b == b'-'
}

fn would_start_ident(bytes: &[u8], i: usize) -> bool {
    match bytes.get(i) {
        Some(b'-') => matches!(bytes.get(i + 1), Some(&b) if is_ident_start(b) || b == b'-'),
        Some(&b) => is_ident_start(b),
        None => false,
    }
}

fn would_start_number(bytes: &[u8], i: usize) -> bool {
    match bytes.get(i) {
        Some(b'+') | Some(b'-') => match bytes.get(i + 1) {
            Some(b'.') => matches!(bytes.get(i + 2), Some(b) if b.is_ascii_digit()),
            Some(b) => b.is_ascii_digit(),
            None => false,
        },
        Some(b'.') => matches!(bytes.get(i + 1), Some(b) if b.is_ascii_digit()),
        Some(b) => b.is_ascii_digit(),
        None => false,
    }
}

fn consume_ident(bytes: &[u8], i: &mut usize) -> String {
    let start = *i;
    while *i < bytes.len() && is_ident_char(bytes[*i]) {
        *i += 1;
    }
    String::from_utf8_lossy(&bytes[start..*i]).into_owned()
}

fn consume_number(bytes: &[u8], i: &mut usize) -> f64 {
    let start = *i;
    if matches!(bytes.get(*i), Some(b'+') | Some(b'-')) {
        *i += 1;
    }
    while matches!(bytes.get(*i), Some(b) if b.is_ascii_digit()) {
        *i += 1;
    }
    if bytes.get(*i) == Some(&b'.') && matches!(bytes.get(*i + 1), Some(b) if b.is_ascii_digit()) {
        *i += 2;
        while matches!(bytes.get(*i), Some(b) if b.is_ascii_digit()) {
            *i += 1;
        }
    }
    if matches!(bytes.get(*i), Some(b'e') | Some(b'E')) {
        let mut j = *i + 1;
        if matches!(bytes.get(j), Some(b'+') | Some(b'-')) {
            j += 1;
        }
        if matches!(bytes.get(j), Some(b) if b.is_ascii_digit()) {
            *i = j;
            while matches!(bytes.get(*i), Some(b) if b.is_ascii_digit()) {
                *i += 1;
            }
        }
    }
    String::from_utf8_lossy(&bytes[start..*i])
        .parse::<f64>()
        .unwrap_or(f64::NAN)
}

fn tokenize(input: &str) -> Vec<Tok> {
    let bytes = input.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if is_ws(b) {
            while i < bytes.len() && is_ws(bytes[i]) {
                i += 1;
            }
            tokens.push(Tok::Whitespace);
        } else if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            tokens.push(Tok::Unsupported);
        } else if would_start_number(bytes, i) {
            let value = consume_number(bytes, &mut i);
            if would_start_ident(bytes, i) {
                let unit = consume_ident(bytes, &mut i);
                tokens.push(Tok::Dimension { value, unit });
            } else if bytes.get(i) == Some(&b'%') {
                i += 1;
                tokens.push(Tok::Percentage(value));
            } else {
                tokens.push(Tok::Number(value));
            }
        } else if b == b'-' && bytes.get(i..i + 3) == Some(b"-->") {
            i += 3;
            tokens.push(Tok::Unsupported);
        } else if b == b'<' && bytes.get(i..i + 4) == Some(b"<!--") {
            i += 4;
            tokens.push(Tok::Unsupported);
        } else if would_start_ident(bytes, i) {
            let name = consume_ident(bytes, &mut i);
            if bytes.get(i) == Some(&b'(') {
                i += 1;
                tokens.push(Tok::Function(name));
            } else {
                tokens.push(Tok::Ident(name));
            }
        } else if b == b'@' && would_start_ident(bytes, i + 1) {
            i += 1;
            let name = consume_ident(bytes, &mut i);
            tokens.push(Tok::AtKeyword(name));
        } else {
            i += 1;
            tokens.push(match b {
                b'(' => Tok::OpenParen,
                b')' => Tok::CloseParen,
                b':' => Tok::Colon,
                b',' => Tok::Comma,
                b'"' | b'\'' | b'[' | b']' | b'{' | b'}' | b';' => Tok::Unsupported,
                _ => Tok::Delim(b),
            });
        }
    }
    tokens
}

// Parser ports the upstream TokenParser grammar: alternatives in upstream
// order; every helper resets `pos` when it returns None.

struct Parser<'a> {
    tokens: &'a [Tok],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos)
    }

    fn ws(&mut self) -> Option<()> {
        if matches!(self.peek(), Some(Tok::Whitespace)) {
            self.pos += 1;
            Some(())
        } else {
            None
        }
    }

    fn ws_opt(&mut self) {
        if matches!(self.peek(), Some(Tok::Whitespace)) {
            self.pos += 1;
        }
    }

    fn ident(&mut self) -> Option<String> {
        if let Some(Tok::Ident(name)) = self.peek() {
            let name = name.clone();
            self.pos += 1;
            Some(name)
        } else {
            None
        }
    }

    fn ident_eq(&mut self, expected: &str) -> Option<()> {
        if matches!(self.peek(), Some(Tok::Ident(name)) if name == expected) {
            self.pos += 1;
            Some(())
        } else {
            None
        }
    }

    fn delim(&mut self, expected: u8) -> Option<()> {
        if matches!(self.peek(), Some(Tok::Delim(b)) if *b == expected) {
            self.pos += 1;
            Some(())
        } else {
            None
        }
    }

    fn delim_in(&mut self, set: &[u8]) -> Option<u8> {
        if let Some(Tok::Delim(b)) = self.peek()
            && set.contains(b)
        {
            let b = *b;
            self.pos += 1;
            Some(b)
        } else {
            None
        }
    }

    fn open(&mut self) -> Option<()> {
        if matches!(self.peek(), Some(Tok::OpenParen)) {
            self.pos += 1;
            Some(())
        } else {
            None
        }
    }

    fn close(&mut self) -> Option<()> {
        if matches!(self.peek(), Some(Tok::CloseParen)) {
            self.pos += 1;
            Some(())
        } else {
            None
        }
    }

    fn colon(&mut self) -> Option<()> {
        if matches!(self.peek(), Some(Tok::Colon)) {
            self.pos += 1;
            Some(())
        } else {
            None
        }
    }

    fn comma(&mut self) -> Option<()> {
        if matches!(self.peek(), Some(Tok::Comma)) {
            self.pos += 1;
            Some(())
        } else {
            None
        }
    }

    fn number(&mut self) -> Option<f64> {
        if let Some(Tok::Number(v)) = self.peek() {
            let v = *v;
            self.pos += 1;
            Some(v)
        } else {
            None
        }
    }

    fn percentage(&mut self) -> Option<f64> {
        if let Some(Tok::Percentage(v)) = self.peek() {
            let v = *v;
            self.pos += 1;
            Some(v)
        } else {
            None
        }
    }

    fn dimension(&mut self) -> Option<(f64, String)> {
        if let Some(Tok::Dimension { value, unit }) = self.peek() {
            let out = (*value, unit.clone());
            self.pos += 1;
            Some(out)
        } else {
            None
        }
    }

    fn function_eq(&mut self, expected: &str) -> Option<()> {
        if matches!(self.peek(), Some(Tok::Function(name)) if name == expected) {
            self.pos += 1;
            Some(())
        } else {
            None
        }
    }

    // --- grammar rules ---

    fn basic_media_type(&mut self) -> Option<String> {
        let start = self.pos;
        let name = self.ident()?;
        if name == "screen" || name == "print" || name == "all" {
            Some(name)
        } else {
            self.pos = start;
            None
        }
    }

    // sequence(not?, only?, type).separatedBy(Whitespace): a separator is
    // required before an element only once earlier elements consumed input
    fn media_keyword(&mut self) -> Option<MediaQueryRule> {
        let start = self.pos;
        let not = self.ident_eq("not").is_some();
        let only = if self.pos > start {
            let save = self.pos;
            let matched = self.ws().is_some() && self.ident_eq("only").is_some();
            if !matched {
                self.pos = save;
            }
            matched
        } else {
            self.ident_eq("only").is_some()
        };
        if self.pos > start && self.ws().is_none() {
            self.pos = start;
            return None;
        }
        let Some(key) = self.basic_media_type() else {
            self.pos = start;
            return None;
        };
        Some(MediaQueryRule::Keyword { key, not, only })
    }

    fn word_rule(&mut self) -> Option<MediaQueryRule> {
        let start = self.pos;
        let matched = (|| {
            self.open()?;
            let key = self.ident()?;
            self.close()?;
            if key == "color" || key == "monochrome" || key == "grid" || key == "color-index" {
                Some(MediaQueryRule::WordRule(key))
            } else {
                None
            }
        })();
        if matched.is_none() {
            self.pos = start;
        }
        matched
    }

    fn calc_value(&mut self) -> Option<String> {
        for constant in ["pi", "e", "infinity", "-infinity", "NaN"] {
            if self.ident_eq(constant).is_some() {
                return Some(constant.to_string());
            }
        }
        if let Some(n) = self.number() {
            return Some(js_number_to_string(n));
        }
        if let Some((value, unit)) = self.dimension() {
            return Some(format!("{}{unit}", js_number_to_string(value)));
        }
        if let Some(p) = self.percentage() {
            return Some(format!("{}%", js_number_to_string(p)));
        }
        None
    }

    fn calc_group(&mut self) -> Option<String> {
        let start = self.pos;
        let matched = (|| {
            self.open()?;
            self.ws_opt();
            let inner = self.calc_operations()?;
            self.ws_opt();
            self.close()?;
            Some(format!("({inner})"))
        })();
        if matched.is_none() {
            self.pos = start;
        }
        matched
    }

    fn calc_operand(&mut self) -> Option<String> {
        self.calc_value().or_else(|| self.calc_group())
    }

    fn calc_operations(&mut self) -> Option<String> {
        let first = self.calc_operand()?;
        let mut parts = vec![first];
        self.ws_opt();
        let mut first_item = true;
        loop {
            let save = self.pos;
            if !first_item {
                self.ws_opt();
            }
            let item = (|| {
                let op = self.delim_in(b"*/+-")?;
                self.ws_opt();
                let operand = self.calc_operand()?;
                Some((op, operand))
            })();
            match item {
                Some((op, operand)) => {
                    parts.push((op as char).to_string());
                    parts.push(operand);
                    first_item = false;
                }
                None => {
                    self.pos = save;
                    break;
                }
            }
        }
        Some(parts.join(" "))
    }

    fn calc(&mut self) -> Option<String> {
        let start = self.pos;
        let matched = (|| {
            self.function_eq("calc")?;
            self.ws_opt();
            let body = self.calc_operations()?;
            self.ws_opt();
            self.close()?;
            Some(format!("calc({body})"))
        })();
        if matched.is_none() {
            self.pos = start;
        }
        matched
    }

    fn media_rule_value(&mut self) -> Option<MediaRuleValue> {
        if let Some(calc) = self.calc() {
            return Some(MediaRuleValue::Str(calc));
        }
        if let Some((value, unit)) = self.dimension() {
            return Some(MediaRuleValue::Length { value, unit });
        }
        if let Some(name) = self.ident() {
            return Some(MediaRuleValue::Str(name));
        }
        let start = self.pos;
        let fraction = (|| {
            let a = self.number()?;
            self.ws_opt();
            self.delim(b'/')?;
            self.ws_opt();
            let b = self.number()?;
            Some(MediaRuleValue::Fraction(a, b))
        })();
        if fraction.is_some() {
            return fraction;
        }
        self.pos = start;
        self.number().map(MediaRuleValue::Number)
    }

    fn simple_pair(&mut self) -> Option<MediaQueryRule> {
        let start = self.pos;
        let matched = (|| {
            self.open()?;
            self.ws_opt();
            let key = self.ident()?;
            self.ws_opt();
            self.colon()?;
            self.ws_opt();
            let value = self.media_rule_value()?;
            self.ws_opt();
            self.close()?;
            Some(MediaQueryRule::Pair { key, value })
        })();
        if matched.is_none() {
            self.pos = start;
        }
        matched
    }

    fn width_or_height(&mut self) -> Option<String> {
        let start = self.pos;
        let name = self.ident()?;
        if name == "width" || name == "height" {
            Some(name)
        } else {
            self.pos = start;
            None
        }
    }

    fn ineq_op(&mut self) -> Option<u8> {
        self.delim_in(b"><")
    }

    // optional('=') with the sequence's optional-whitespace separator folded in
    fn ineq_eq(&mut self) -> bool {
        let save = self.pos;
        self.ws_opt();
        if self.delim(b'=').is_some() {
            true
        } else {
            self.pos = save;
            false
        }
    }

    fn adjust(value: f64, eq: bool, is_max: bool) -> f64 {
        if eq {
            value
        } else if is_max {
            value - MERGE_EPSILON
        } else {
            value + MERGE_EPSILON
        }
    }

    fn ineq_forward(&mut self) -> Option<MediaQueryRule> {
        let start = self.pos;
        let matched = (|| {
            self.open()?;
            self.ws_opt();
            let key = self.width_or_height()?;
            self.ws_opt();
            let op = self.ineq_op()?;
            let eq = self.ineq_eq();
            self.ws_opt();
            let (value, unit) = self.dimension()?;
            self.ws_opt();
            self.close()?;
            let final_key = if op == b'>' {
                format!("min-{key}")
            } else {
                format!("max-{key}")
            };
            let is_max = final_key.starts_with("max-");
            Some(MediaQueryRule::Pair {
                key: final_key,
                value: MediaRuleValue::Length {
                    value: Self::adjust(value, eq, is_max),
                    unit,
                },
            })
        })();
        if matched.is_none() {
            self.pos = start;
        }
        matched
    }

    fn ineq_reversed(&mut self) -> Option<MediaQueryRule> {
        let start = self.pos;
        let matched = (|| {
            self.open()?;
            self.ws_opt();
            let (value, unit) = self.dimension()?;
            self.ws_opt();
            let op = self.ineq_op()?;
            let eq = self.ineq_eq();
            self.ws_opt();
            let key = self.width_or_height()?;
            self.ws_opt();
            self.close()?;
            let final_key = if op == b'>' {
                format!("max-{key}")
            } else {
                format!("min-{key}")
            };
            let is_max = final_key.starts_with("max-");
            Some(MediaQueryRule::Pair {
                key: final_key,
                value: MediaRuleValue::Length {
                    value: Self::adjust(value, eq, is_max),
                    unit,
                },
            })
        })();
        if matched.is_none() {
            self.pos = start;
        }
        matched
    }

    fn combined_inequality(&mut self) -> Option<MediaQueryRule> {
        self.ineq_forward().or_else(|| self.ineq_reversed())
    }

    fn double_inequality(&mut self) -> Option<MediaQueryRule> {
        let start = self.pos;
        let matched = (|| {
            self.open()?;
            self.ws_opt();
            let (lower, lower_unit) = self.dimension()?;
            self.ws_opt();
            let op = self.ineq_op()?;
            let eq = self.ineq_eq();
            self.ws_opt();
            let key = self.width_or_height()?;
            self.ws_opt();
            let op2 = self.ineq_op()?;
            let eq2 = self.ineq_eq();
            self.ws_opt();
            let (upper, upper_unit) = self.dimension()?;
            self.ws_opt();
            self.close()?;
            let lower_key = if op == b'>' {
                format!("max-{key}")
            } else {
                format!("min-{key}")
            };
            let upper_key = if op2 == b'>' {
                format!("min-{key}")
            } else {
                format!("max-{key}")
            };
            let lower_is_max = lower_key.starts_with("max-");
            let upper_is_max = upper_key.starts_with("max-");
            Some(MediaQueryRule::And(vec![
                MediaQueryRule::Pair {
                    key: lower_key,
                    value: MediaRuleValue::Length {
                        value: Self::adjust(lower, eq, lower_is_max),
                        unit: lower_unit,
                    },
                },
                MediaQueryRule::Pair {
                    key: upper_key,
                    value: MediaRuleValue::Length {
                        value: Self::adjust(upper, eq2, upper_is_max),
                        unit: upper_unit,
                    },
                },
            ]))
        })();
        if matched.is_none() {
            self.pos = start;
        }
        matched
    }

    fn or_in_parens(&mut self) -> Option<MediaQueryRule> {
        let start = self.pos;
        let matched = (|| {
            self.open()?;
            let rule = self.or_rules()?;
            self.close()?;
            Some(rule)
        })();
        if matched.is_none() {
            self.pos = start;
        }
        matched
    }

    fn and_in_parens(&mut self) -> Option<MediaQueryRule> {
        let start = self.pos;
        let matched = (|| {
            self.open()?;
            let rule = self.and_rules()?;
            self.close()?;
            Some(rule)
        })();
        if matched.is_none() {
            self.pos = start;
        }
        matched
    }

    fn and_or_element(&mut self) -> Option<MediaQueryRule> {
        self.media_keyword()
            .or_else(|| self.or_in_parens())
            .or_else(|| self.and_in_parens())
            .or_else(|| self.not_parser())
            .or_else(|| self.double_inequality())
            .or_else(|| self.combined_inequality())
            .or_else(|| self.simple_pair())
            .or_else(|| self.word_rule())
    }

    fn combinator_rules(&mut self, word: &str) -> Option<Vec<MediaQueryRule>> {
        let start = self.pos;
        let Some(first) = self.and_or_element() else {
            self.pos = start;
            return None;
        };
        let mut items = vec![first];
        loop {
            let sep_save = self.pos;
            let sep = (|| {
                self.ws()?;
                self.ident_eq(word)?;
                self.ws()
            })();
            if sep.is_none() {
                self.pos = sep_save;
                break;
            }
            // upstream keeps a consumed separator when the next element fails
            match self.and_or_element() {
                Some(rule) => items.push(rule),
                None => break,
            }
        }
        if items.len() > 1 {
            Some(items)
        } else {
            self.pos = start;
            None
        }
    }

    fn and_rules(&mut self) -> Option<MediaQueryRule> {
        self.combinator_rules("and").map(MediaQueryRule::And)
    }

    fn or_rules(&mut self) -> Option<MediaQueryRule> {
        self.combinator_rules("or").map(MediaQueryRule::Or)
    }

    fn paren_keyword(&mut self) -> Option<MediaQueryRule> {
        let start = self.pos;
        let matched = (|| {
            self.open()?;
            let key = self.basic_media_type()?;
            self.close()?;
            Some(MediaQueryRule::Keyword {
                key,
                not: false,
                only: false,
            })
        })();
        if matched.is_none() {
            self.pos = start;
        }
        matched
    }

    // parity: getNormalRuleParser() — the rule set allowed inside `(not …)`;
    // inequality forms are deliberately absent there upstream
    fn normal_rule_for_not(&mut self) -> Option<MediaQueryRule> {
        let rule = self
            .paren_keyword()
            .or_else(|| self.and_rules())
            .or_else(|| self.or_rules())
            .or_else(|| self.simple_pair())
            .or_else(|| self.word_rule())
            .or_else(|| self.not_parser())
            .or_else(|| self.or_in_parens())
            .or_else(|| self.and_in_parens())?;
        self.ws_opt();
        Some(rule)
    }

    fn not_parser(&mut self) -> Option<MediaQueryRule> {
        let start = self.pos;
        let matched = (|| {
            self.open()?;
            self.ident_eq("not")?;
            self.ws()?;
            let rule = self.normal_rule_for_not()?;
            self.close()?;
            Some(MediaQueryRule::Not(Box::new(rule)))
        })();
        if matched.is_none() {
            self.pos = start;
        }
        matched
    }

    fn leading_not(&mut self) -> Option<MediaQueryRule> {
        let start = self.pos;
        let matched = (|| {
            self.ident_eq("not")?;
            self.ws()?;
            let rule = self
                .or_in_parens()
                .or_else(|| self.and_in_parens())
                .or_else(|| self.not_parser())
                .or_else(|| self.combined_inequality())
                .or_else(|| self.simple_pair())
                .or_else(|| self.word_rule())?;
            Some(MediaQueryRule::Not(Box::new(rule)))
        })();
        if matched.is_none() {
            self.pos = start;
        }
        matched
    }

    fn normal_rule(&mut self) -> Option<MediaQueryRule> {
        self.and_rules()
            .or_else(|| self.or_rules())
            .or_else(|| self.media_keyword())
            .or_else(|| self.not_parser())
            .or_else(|| self.double_inequality())
            .or_else(|| self.combined_inequality())
            .or_else(|| self.simple_pair())
            .or_else(|| self.word_rule())
            .or_else(|| self.or_in_parens())
            .or_else(|| self.and_in_parens())
    }

    fn query(&mut self) -> Option<MediaQueryRule> {
        self.leading_not().or_else(|| self.normal_rule())
    }

    fn media_query(&mut self) -> Option<MediaQueryRule> {
        let start = self.pos;
        let matched = (|| {
            if !matches!(self.peek(), Some(Tok::AtKeyword(name)) if name == "media") {
                return None;
            }
            self.pos += 1;
            self.ws()?;
            let mut sets = vec![self.query()?];
            loop {
                let sep_save = self.pos;
                self.ws_opt();
                if self.comma().is_none() {
                    self.pos = sep_save;
                    break;
                }
                self.ws_opt();
                match self.query() {
                    Some(q) => sets.push(q),
                    None => break,
                }
            }
            Some(if sets.len() > 1 {
                MediaQueryRule::Or(sets)
            } else {
                sets.pop().expect("one query set parsed")
            })
        })();
        if matched.is_none() {
            self.pos = start;
        }
        matched
    }
}
