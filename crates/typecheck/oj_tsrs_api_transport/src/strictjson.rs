// The input checks that the pinned server's JSON decoder (tsc/internal/json -> encoding/json/v2,
// jsontext) applies to every document it unmarshals, which serde_json does not: duplicate object
// member names are rejected at any depth (including inside ignored/unknown fields), strings must be
// valid UTF-8 with correctly paired `\uD800-\uDFFF` escapes (lone surrogates are rejected, never
// replaced), and the grammar is strict. Error texts reproduce jsontext's, including the JSON-pointer
// and offset rules (verified against a Go binary built at the pinned commit:
// tests/fixtures/go_json_oracle.json and go_syntax_oracle.json).
//
// The scan is iterative (no recursion), allocates at most one key set per open object, and rejects
// nesting deeper than jsontext's limit (10000). `decode` runs the same scan and also builds the value, so a
// request is read once.

use rustc_hash::{FxHashMap, FxHashSet};
use std::fmt::Write as _;
use tsrs_core::json::{JsonMap, Key, Value, exact_integer_value};

const MAX_DEPTH: usize = 10000;

enum Frame {
    Object { keys: FxHashSet<String>, current: Option<String> },
    Array { index: usize },
}

#[derive(Clone, Copy, PartialEq)]
enum Expect {
    /// A value (top level, after `:`, or as an array element).
    Value,
    /// A member name or `}` right after `{`.
    FirstName,
    /// A member name after `,`.
    Name,
    /// `:` after a member name.
    Colon,
    /// `,` or the closer after a value inside a container; at top level, only whitespace.
    AfterValue,
}

/// Which JSON pointer an error inside a string reports.
#[derive(Clone, Copy)]
enum At {
    Value,
    Container,
}

struct Scanner<'a> {
    b: &'a [u8],
    i: usize,
    stack: Vec<Frame>,
    /// Offset of the last `,` while a name or value is expected after it.
    comma: Option<usize>,
    /// Set by `decode`: the value being built.
    build: Option<Build>,
}

/// A strictly valid JSON document read by `decode`.
pub struct Decoded {
    pub value: Value,
    /// The literal of each number whose f64 may differ from it (a fraction, an exponent or more than 15
    /// digits), by JSON pointer. Every other number is a plain integer that its f64 holds exactly.
    pub lexemes: FxHashMap<String, String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// What `validate` reports.
    Invalid(String),
    /// The document is valid but a number does not fit an f64: `json::unmarshal`'s error.
    OutOfRange(String),
}

#[derive(Default)]
struct Build {
    /// The open containers, parallel to `Scanner::stack`.
    open: Vec<Partial>,
    root: Option<Value>,
    lexemes: FxHashMap<String, String>,
    /// End offset of the first number out of f64 range.
    out_of_range: Option<usize>,
}

enum Partial {
    Object(JsonMap),
    Array(Vec<Value>),
}

fn escape_token(t: &str) -> String {
    t.replace('~', "~0").replace('/', "~1")
}

/// Go's %q for a rune, as jsontext prints offending characters.
fn quote_char(c: char) -> String {
    match c {
        '\'' => "'\\''".to_string(),
        '\\' => "'\\\\'".to_string(),
        '\n' => "'\\n'".to_string(),
        '\r' => "'\\r'".to_string(),
        '\t' => "'\\t'".to_string(),
        '\u{7}' => "'\\a'".to_string(),
        '\u{8}' => "'\\b'".to_string(),
        '\u{c}' => "'\\f'".to_string(),
        '\u{b}' => "'\\v'".to_string(),
        c if (c as u32) < 0x20 || c as u32 == 0x7f => format!("'\\x{:02x}'", c as u32),
        c => format!("'{c}'"),
    }
}

fn is_value_start(c: u8) -> bool {
    matches!(c, b'"' | b'{' | b'[' | b'-' | b'0'..=b'9' | b't' | b'f' | b'n')
}

impl Scanner<'_> {
    /// Pointer tokens of the frames; `skip_last` drops that many trailing frame tokens.
    fn pointer_upto(&self, frames: usize, include_last_token: bool) -> String {
        let mut out = String::new();
        for (depth, frame) in self.stack.iter().take(frames).enumerate() {
            if depth + 1 == frames && !include_last_token {
                break;
            }
            match frame {
                Frame::Object { current: Some(k), .. } => {
                    out.push('/');
                    out.push_str(&escape_token(k));
                }
                Frame::Object { current: None, .. } => {}
                Frame::Array { index } => {
                    out.push('/');
                    out.push_str(&index.to_string());
                }
            }
        }
        out
    }

    /// Pointer of the value being read (includes the current member name / element index).
    fn value_ptr(&self) -> String {
        self.pointer_upto(self.stack.len(), true)
    }

    /// Pointer of the innermost container itself.
    fn container_ptr(&self) -> String {
        self.pointer_upto(self.stack.len(), false)
    }

    fn pointer(&self, at: At) -> String {
        match at {
            At::Value => self.value_ptr(),
            At::Container => self.container_ptr(),
        }
    }

    /// Adds a finished value to the innermost open container (or makes it the document) when decoding.
    fn emit(&mut self, value: impl FnOnce() -> Value) {
        let Some(build) = &mut self.build else { return };
        match (self.stack.last_mut(), build.open.last_mut()) {
            (None, _) => build.root = Some(value()),
            (Some(Frame::Object { current, .. }), Some(Partial::Object(members))) => {
                members.insert(current.take().unwrap_or_default(), value());
            }
            (Some(Frame::Array { .. }), Some(Partial::Array(items))) => items.push(value()),
            _ => unreachable!("decode containers follow the scan stack"),
        }
    }

    fn open(&mut self, frame: Frame) {
        if let Some(build) = &mut self.build {
            build.open.push(match frame {
                Frame::Object { .. } => Partial::Object(JsonMap::new()),
                Frame::Array { .. } => Partial::Array(Vec::new()),
            });
        }
        self.stack.push(frame);
    }

    fn close(&mut self) {
        self.stack.pop();
        let Some(partial) = self.build.as_mut().and_then(|b| b.open.pop()) else { return };
        self.emit(|| match partial {
            Partial::Object(members) => Value::Object(members),
            Partial::Array(items) => Value::Array(items),
        });
    }

    /// Pointer of the innermost container's parent container.
    fn parent_ptr(&self) -> String {
        self.pointer_upto(self.stack.len().saturating_sub(1), false)
    }

    fn err(what: &str, pointer: &str, offset: usize) -> String {
        let mut s = format!("jsontext: {what}");
        if !pointer.is_empty() {
            let _ = write!(s, " within {}", serde_json::to_string(pointer).unwrap());
        }
        if offset > 0 {
            let _ = write!(s, " after offset {offset}");
        }
        s
    }

    fn char_at(&self, at: usize) -> char {
        std::str::from_utf8(&self.b[at..(at + 4).min(self.b.len())])
            .ok()
            .or_else(|| self.b[at..].utf8_chunks().next().map(|c| c.valid()))
            .and_then(|s| s.chars().next())
            .unwrap_or(self.b[at] as char)
    }

    fn invalid_char(&self, at: usize, context: &str, pointer: &str) -> String {
        Self::err(
            &format!("invalid character {} {context}", quote_char(self.char_at(at))),
            pointer,
            at,
        )
    }

    fn eof(pointer: &str, offset: usize) -> String {
        Self::err("unexpected EOF", pointer, offset)
    }

    fn skip_ws(&mut self) {
        self.i += (self.i..self.b.len())
            .take_while(|&index| matches!(self.b[index], b' ' | b'\t' | b'\n' | b'\r'))
            .count();
    }

    fn hex4(&self, at: usize) -> Option<u32> {
        let s = self.b.get(at..at + 4)?;
        if !s.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        u32::from_str_radix(std::str::from_utf8(s).ok()?, 16).ok()
    }

    /// Reads a string starting at the opening quote; returns its decoded value. `at` locates it in errors.
    fn string(&mut self, at: At) -> Result<String, String> {
        self.i += 1;
        // Fast path: printable ASCII up to the closing quote is the string itself.
        let rest = &self.b[self.i..];
        let plain =
            rest.iter().position(|&c| c == b'"' || c == b'\\' || !(0x20..0x80).contains(&c));
        if let Some(len) = plain.filter(|&len| rest[len] == b'"') {
            let text = std::str::from_utf8(&rest[..len]).unwrap_or_default().to_string();
            self.i += len + 1;
            return Ok(text);
        }
        let mut out = String::with_capacity(plain.unwrap_or(0));
        loop {
            let Some(&c) = self.b.get(self.i) else {
                return Err(Self::eof(&self.pointer(at), self.b.len()));
            };
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    let start = self.i;
                    let window = |s: &Self, n: usize| {
                        String::from_utf8_lossy(&s.b[start..(start + n).min(s.b.len())])
                            .into_owned()
                    };
                    let Some(&e) = self.b.get(self.i + 1) else {
                        return Err(Self::eof(&self.pointer(at), start));
                    };
                    let simple = match e {
                        b'"' => Some('"'),
                        b'\\' => Some('\\'),
                        b'/' => Some('/'),
                        b'b' => Some('\u{8}'),
                        b'f' => Some('\u{c}'),
                        b'n' => Some('\n'),
                        b'r' => Some('\r'),
                        b't' => Some('\t'),
                        _ => None,
                    };
                    if let Some(ch) = simple {
                        out.push(ch);
                        self.i += 2;
                        continue;
                    }
                    if e != b'u' {
                        return Err(Self::err(
                            &format!("invalid escape sequence `{}` in string", window(self, 2)),
                            &self.pointer(at),
                            start,
                        ));
                    }
                    if self.b.len() < start + 6
                        && self.b[start + 2..].iter().all(u8::is_ascii_hexdigit)
                    {
                        return Err(Self::eof(&self.pointer(at), start));
                    }
                    let Some(u) = self.hex4(start + 2) else {
                        return Err(Self::err(
                            &format!("invalid escape sequence `{}` in string", window(self, 6)),
                            &self.pointer(at),
                            start,
                        ));
                    };
                    let surrogate = |s: &Self| {
                        Scanner::<'_>::err(
                            &format!("invalid surrogate pair `{}` in string", window(s, 12)),
                            &s.pointer(at),
                            start,
                        )
                    };
                    if (0xDC00..0xE000).contains(&u) {
                        return Err(surrogate(self));
                    }
                    if (0xD800..0xDC00).contains(&u) {
                        let lo = (self.b.get(start + 6) == Some(&b'\\')
                            && self.b.get(start + 7) == Some(&b'u'))
                        .then(|| self.hex4(start + 8))
                        .flatten()
                        .filter(|lo| (0xDC00..0xE000).contains(lo));
                        let Some(lo) = lo else {
                            return Err(surrogate(self));
                        };
                        out.push(
                            char::from_u32(0x10000 + ((u - 0xD800) << 10) + (lo - 0xDC00)).unwrap(),
                        );
                        self.i += 12;
                        continue;
                    }
                    out.push(char::from_u32(u).unwrap());
                    self.i += 6;
                }
                c if c < 0x20 => {
                    return Err(self.invalid_char(
                        self.i,
                        "in string (expecting non-control character)",
                        &self.pointer(at),
                    ));
                }
                c if c < 0x80 => {
                    out.push(c as char);
                    self.i += 1;
                }
                _ => {
                    let len = match c {
                        0xC2..=0xDF => 2,
                        0xE0..=0xEF => 3,
                        0xF0..=0xF4 => 4,
                        _ => 0,
                    };
                    match self
                        .b
                        .get(self.i..self.i + len)
                        .filter(|_| len > 0)
                        .and_then(|s| std::str::from_utf8(s).ok())
                    {
                        Some(s) => {
                            out.push_str(s);
                            self.i += len;
                        }
                        None => return Err(Self::err("invalid UTF-8", &self.pointer(at), self.i)),
                    }
                }
            }
        }
    }

    fn literal(&mut self, lit: &'static [u8], name: &str) -> Result<(), String> {
        if let Some(result) = lit.iter().enumerate().find_map(|(k, &want)| {
            match self.b.get(self.i + k) {
                None => return Some(Err(Self::eof(&self.value_ptr(), self.b.len()))),
                Some(&got) if got != want => {
                    return Some(Err(self.invalid_char(
                        self.i + k,
                        &format!("in literal {name} (expecting {})", quote_char(want as char)),
                        &self.value_ptr(),
                    )));
                }
                _ => {}
            }

            None
        }) {
            return result;
        }
        self.i += lit.len();
        Ok(())
    }

    /// -?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?
    fn number(&mut self) -> Result<(), String> {
        let start = self.i;
        let b = self.b;
        let mut j = self.i;
        let digit_expected = |s: &Self, j: usize| -> String {
            match b.get(j) {
                None => Scanner::<'_>::eof(&s.value_ptr(), start),
                Some(_) => s.invalid_char(j, "in number (expecting digit)", &s.value_ptr()),
            }
        };
        if b.get(j) == Some(&b'-') {
            j += 1;
        }
        match b.get(j) {
            Some(b'0') => j += 1,
            Some(b'1'..=b'9') => {
                j += b.iter().skip(j).take_while(|b| b.is_ascii_digit()).count();
            }
            _ => return Err(digit_expected(self, j)),
        }
        if b.get(j) == Some(&b'.') {
            j += 1;
            if !b.get(j).is_some_and(u8::is_ascii_digit) {
                return Err(digit_expected(self, j));
            }
            j += b.iter().skip(j).take_while(|b| b.is_ascii_digit()).count();
        }
        if matches!(b.get(j), Some(b'e' | b'E')) {
            j += 1;
            if matches!(b.get(j), Some(b'+' | b'-')) {
                j += 1;
            }
            if !b.get(j).is_some_and(u8::is_ascii_digit) {
                return Err(digit_expected(self, j));
            }
            j += b.iter().skip(j).take_while(|b| b.is_ascii_digit()).count();
        }
        self.i = j;
        if self.build.is_some() {
            self.emit_number(start, j);
        }
        Ok(())
    }

    fn emit_number(&mut self, start: usize, end: usize) {
        // Valid number syntax is ASCII.
        let text = std::str::from_utf8(&self.b[start..end]).unwrap_or_default();
        if let Some(n) = exact_integer_value(text) {
            self.emit(|| Value::Number(n));
            return;
        }
        let pointer = self.value_ptr();
        let n = text.parse::<f64>().ok().filter(|f| f.is_finite());
        if let Some(build) = &mut self.build {
            build.lexemes.insert(pointer, text.to_string());
            if n.is_none() {
                build.out_of_range.get_or_insert(end);
            }
        }
        self.emit(|| n.map_or(Value::Null, Value::Number));
    }

    /// Reads one value at `self.i` (already past whitespace). Containers are pushed and left open.
    fn value(&mut self) -> Result<Expect, String> {
        let c = self.b[self.i];
        match c {
            b'{' | b'[' => {
                if self.stack.len() >= MAX_DEPTH {
                    return Err(Self::err("exceeded max depth", &self.value_ptr(), self.i));
                }
                self.i += 1;
                if c == b'{' {
                    self.open(Frame::Object { keys: FxHashSet::default(), current: None });
                    Ok(Expect::FirstName)
                } else {
                    self.open(Frame::Array { index: 0 });
                    self.skip_ws();
                    if self.b.get(self.i) == Some(&b']') {
                        self.i += 1;
                        self.close();
                        return Ok(Expect::AfterValue);
                    }
                    Ok(Expect::Value)
                }
            }
            b'"' => {
                let text = self.string(At::Value)?;
                self.emit(|| Value::String(text));
                Ok(Expect::AfterValue)
            }
            b't' => self.literal(b"true", "true").map(|()| {
                self.emit(|| Value::Bool(true));
                Expect::AfterValue
            }),
            b'f' => self.literal(b"false", "false").map(|()| {
                self.emit(|| Value::Bool(false));
                Expect::AfterValue
            }),
            b'n' => self.literal(b"null", "null").map(|()| {
                self.emit(|| Value::Null);
                Expect::AfterValue
            }),
            b'-' | b'0'..=b'9' => self.number().map(|()| Expect::AfterValue),
            _ => Err(self.invalid_char(self.i, "at start of value", &self.value_ptr())),
        }
    }

    fn run(&mut self) -> Result<(), String> {
        if self.b.is_empty() {
            return Err("jsontext: unexpected EOF".to_string());
        }
        let mut expect = Expect::Value;
        loop {
            self.skip_ws();
            let at_end = self.i >= self.b.len();
            match expect {
                Expect::Value => {
                    if at_end {
                        // jsontext locates a missing member value at the member, a missing element at the array.
                        let ptr = match self.stack.last() {
                            Some(Frame::Object { .. }) => self.value_ptr(),
                            _ => self.container_ptr(),
                        };
                        return Err(Self::eof(&ptr, self.i));
                    }
                    let c = self.b[self.i];
                    if let (Some(comma), b']' | b'}') = (self.comma, c) {
                        // Trailing comma: jsontext blames the comma, located in the container.
                        return Err(self.invalid_char(
                            comma,
                            "at start of value",
                            &self.container_ptr(),
                        ));
                    }
                    self.comma = None;
                    expect = self.value()?;
                }
                Expect::FirstName | Expect::Name => {
                    if at_end {
                        return Err(Self::eof(&self.container_ptr(), self.i));
                    }
                    let c = self.b[self.i];
                    if let (Some(comma), b'}' | b']') = (self.comma, c) {
                        return Err(self.invalid_char(
                            comma,
                            "at start of value",
                            &self.container_ptr(),
                        ));
                    }
                    if c == b'}' {
                        self.i += 1;
                        self.close();
                        expect = Expect::AfterValue;
                        continue;
                    }
                    self.comma = None;
                    if c != b'"' {
                        let ptr = self.container_ptr();
                        if is_value_start(c) {
                            return Err(Self::err(
                                "object member name must be a string",
                                &ptr,
                                self.i,
                            ));
                        }
                        return Err(self.invalid_char(self.i, "at start of value", &ptr));
                    }
                    let name = self.string(At::Container)?;
                    // Decoding finds earlier members in the object being built; validating keeps a key set.
                    let decoded_duplicate = match self.build.as_ref().and_then(|b| b.open.last()) {
                        Some(Partial::Object(members)) => Some(members.contains_key(&name)),
                        _ => None,
                    };
                    let Some(Frame::Object { keys, current }) = self.stack.last_mut() else {
                        unreachable!()
                    };
                    if decoded_duplicate.unwrap_or_else(|| !keys.insert(name.clone())) {
                        return Err(Self::err(
                            &format!(
                                "duplicate object member name {}",
                                serde_json::to_string(&name).unwrap()
                            ),
                            &self.container_ptr(),
                            0,
                        ));
                    }
                    *current = Some(name);
                    expect = Expect::Colon;
                }
                Expect::Colon => {
                    if at_end {
                        return Err(Self::eof(&self.value_ptr(), self.i));
                    }
                    if self.b[self.i] != b':' {
                        return Err(self.invalid_char(
                            self.i,
                            "after object name (expecting ':')",
                            &self.value_ptr(),
                        ));
                    }
                    self.i += 1;
                    expect = Expect::Value;
                }
                Expect::AfterValue => {
                    let Some(frame) = self.stack.last_mut() else {
                        if at_end {
                            return Ok(());
                        }
                        return Err(self.invalid_char(self.i, "after top-level value", ""));
                    };
                    if at_end {
                        return Err(Self::eof(&self.container_ptr(), self.i));
                    }
                    let c = self.b[self.i];
                    match (frame, c) {
                        (Frame::Object { .. }, b',') => {
                            self.comma = Some(self.i);
                            self.i += 1;
                            expect = Expect::Name;
                        }
                        (Frame::Array { index }, b',') => {
                            *index += 1;
                            self.comma = Some(self.i);
                            self.i += 1;
                            expect = Expect::Value;
                        }
                        (Frame::Object { .. }, b'}') | (Frame::Array { .. }, b']') => {
                            self.i += 1;
                            self.close();
                        }
                        (Frame::Object { .. }, b']') => {
                            return Err(self.invalid_char(
                                self.i,
                                "after object value (expecting ',' or '}')",
                                &self.parent_ptr(),
                            ));
                        }
                        (Frame::Object { .. }, _) => {
                            return Err(self.invalid_char(
                                self.i,
                                "after object value (expecting ',' or '}')",
                                &self.container_ptr(),
                            ));
                        }
                        (Frame::Array { .. }, _) => {
                            return Err(self.invalid_char(
                                self.i,
                                "after array element (expecting ',' or ']')",
                                &self.container_ptr(),
                            ));
                        }
                    }
                }
            }
        }
    }
}

/// Validates one JSON document with jsontext's strictness (see module docs).
pub fn validate(input: &[u8]) -> Result<(), String> {
    Scanner { b: input, i: 0, stack: Vec::new(), comma: None, build: None }.run()
}

/// `validate`, then `json::unmarshal`, in one scan: the same errors, in the same order.
pub fn decode(input: &[u8]) -> Result<Decoded, DecodeError> {
    decode_with_names(input, |_| None)
}

/// `decode`, taking member names from `names` where it has them (a request's protocol field names), so
/// those members borrow their names instead of allocating them.
pub fn decode_with_names(
    input: &[u8],
    names: fn(&str) -> Option<&'static str>,
) -> Result<Decoded, DecodeError> {
    if let Some(value) = fast_decode(input, names) {
        return Ok(Decoded { value, lexemes: FxHashMap::default() });
    }
    decode_scan(input)
}

/// `decode` by the full scan.
pub fn decode_scan(input: &[u8]) -> Result<Decoded, DecodeError> {
    let mut scanner =
        Scanner { b: input, i: 0, stack: Vec::new(), comma: None, build: Some(Build::default()) };
    scanner.run().map_err(DecodeError::Invalid)?;
    let build = scanner.build.unwrap_or_default();
    if let Some(offset) = build.out_of_range {
        return Err(DecodeError::OutOfRange(format!(
            "jsontext: number out of range at offset {offset}"
        )));
    }
    Ok(Decoded { value: build.root.unwrap_or_default(), lexemes: build.lexemes })
}

/// Containers nested deeper than this leave the document to the full scan.
const FAST_MAX_DEPTH: usize = 64;

/// `decode` for the common request document, or `None` to leave it to the full scan: every string is
/// printable ASCII without escapes, every number is an integer of at most 15 digits (exact in an f64, so it
/// needs no literal), and containers nest at most `FAST_MAX_DEPTH` deep. Whatever it accepts the full scan
/// accepts with the same value; it never reports an error itself, so error texts all come from the scan.
#[expect(
    clippy::too_long_first_doc_paragraph,
    reason = "Public for the integration tests; the comment keeps its upstream wording."
)]
pub fn fast_decode(input: &[u8], names: fn(&str) -> Option<&'static str>) -> Option<Value> {
    let text = std::str::from_utf8(input).ok()?;
    let mut parser = FastParser {
        s: text,
        b: input,
        i: 0,
        names,
        // Room for a request's members, so the stack does not grow on every request.
        members: Vec::with_capacity(16),
        items: Vec::new(),
    };
    parser.ws();
    let value = parser.value(0)?;
    parser.ws();
    (parser.i == input.len()).then_some(value)
}

struct FastParser<'a> {
    s: &'a str,
    b: &'a [u8],
    i: usize,
    names: fn(&str) -> Option<&'static str>,
    /// The members of the objects being read, innermost last: each container is collected here and moved out
    /// at its exact size when it closes.
    members: Vec<(Key, Value)>,
    /// The same for arrays.
    items: Vec<Value>,
}

impl FastParser<'_> {
    fn ws(&mut self) {
        while matches!(self.b.get(self.i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> Option<()> {
        (self.b.get(self.i) == Some(&c)).then(|| self.i += 1)
    }

    fn value(&mut self, depth: usize) -> Option<Value> {
        match *self.b.get(self.i)? {
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'"' => self.string().map(Value::String),
            b't' => self.literal("true", Value::Bool(true)),
            b'f' => self.literal("false", Value::Bool(false)),
            b'n' => self.literal("null", Value::Null),
            b'-' | b'0'..=b'9' => self.number(),
            _ => None,
        }
    }

    fn object(&mut self, depth: usize) -> Option<Value> {
        if depth >= FAST_MAX_DEPTH {
            return None;
        }
        self.i += 1;
        self.ws();
        if self.eat(b'}').is_some() {
            return Some(Value::Object(JsonMap::new()));
        }
        let base = self.members.len();
        loop {
            self.ws();
            let key = self.member_name()?;
            self.ws();
            self.eat(b':')?;
            self.ws();
            let value = self.value(depth + 1)?;
            self.members.push((key, value));
            self.ws();
            match *self.b.get(self.i)? {
                b',' => self.i += 1,
                b'}' => {
                    self.i += 1;
                    // A duplicate member name is an error: the full scan reports it.
                    return JsonMap::from_distinct(self.members.split_off(base)).map(Value::Object);
                }
                _ => return None,
            }
        }
    }

    fn array(&mut self, depth: usize) -> Option<Value> {
        if depth >= FAST_MAX_DEPTH {
            return None;
        }
        self.i += 1;
        self.ws();
        if self.eat(b']').is_some() {
            return Some(Value::Array(Vec::new()));
        }
        let base = self.items.len();
        loop {
            self.ws();
            let item = self.value(depth + 1)?;
            self.items.push(item);
            self.ws();
            match *self.b.get(self.i)? {
                b',' => self.i += 1,
                b']' => {
                    self.i += 1;
                    return Some(Value::Array(self.items.split_off(base)));
                }
                _ => return None,
            }
        }
    }

    /// A string of printable ASCII without escapes.
    fn string(&mut self) -> Option<String> {
        self.text().map(str::to_string)
    }

    fn member_name(&mut self) -> Option<Key> {
        let names = self.names;
        let name = self.text()?;
        Some(names(name).map_or_else(|| Key::Owned(name.to_string()), Key::Borrowed))
    }

    /// The contents of a string of printable ASCII without escapes.
    fn text(&mut self) -> Option<&str> {
        if self.b.get(self.i) != Some(&b'"') {
            return None;
        }
        let start = self.i + 1;
        let len = tsrs_core::json::plain_ascii_len(&self.b[start..]);
        if self.b.get(start + len) != Some(&b'"') {
            return None;
        }
        self.i = start + len + 1;
        self.s.get(start..start + len)
    }

    fn literal(&mut self, word: &str, value: Value) -> Option<Value> {
        if !self.b[self.i..].starts_with(word.as_bytes()) {
            return None;
        }
        self.i += word.len();
        Some(value)
    }

    /// `-?(0|[1-9][0-9]*)` of at most 15 digits, not followed by a fraction or an exponent.
    fn number(&mut self) -> Option<Value> {
        let start = self.i;
        if self.b[self.i] == b'-' {
            self.i += 1;
        }
        let digits = self.b[self.i..].iter().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 || (digits > 1 && self.b[self.i] == b'0') {
            return None;
        }
        self.i += digits;
        if matches!(self.b.get(self.i), Some(b'.' | b'e' | b'E')) {
            return None;
        }
        exact_integer_value(&self.s[start..self.i]).map(Value::Number)
    }
}
