// A small strict JSON (RFC 8259) reader standing in for Go's `encoding/json/v2` decoder as used by
// `packagejson.Parse`. It produces a raw tree that keeps object members in source order, including
// duplicate names; the typed decoding in `packagejson.rs` applies Go's field semantics on top of it.

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    // The first byte of the value's JSON text, which Go's `Expected.UnmarshalJSON` switches on.
    pub(crate) fn first_byte(&self) -> u8 {
        match self {
            Json::Null => b'n',
            Json::Bool(true) => b't',
            Json::Bool(false) => b'f',
            Json::Number(_) => b'0',
            Json::String(_) => b'"',
            Json::Array(_) => b'[',
            Json::Object(_) => b'{',
        }
    }

    // Reports whether any object in this value (recursively) repeats a member name. A plain
    // `json.Unmarshal` without `AllowDuplicateNames(true)` rejects such input.
    pub(crate) fn has_duplicate_names(&self) -> bool {
        match self {
            Json::Array(elements) => elements.iter().any(Json::has_duplicate_names),
            Json::Object(members) => {
                let mut seen = rustc_hash::FxHashSet::default();
                members
                    .iter()
                    .any(|(name, value)| !seen.insert(name.as_str()) || value.has_duplicate_names())
            }
            _ => false,
        }
    }
}

pub fn parse(text: &str) -> Result<Json, String> {
    let mut p = JsonParser { bytes: text.as_bytes(), text, pos: 0 };
    p.skip_whitespace();
    let value = p.parse_value(0)?;
    p.skip_whitespace();
    if p.pos != p.bytes.len() {
        return Err(p.error("invalid character after top-level value"));
    }
    Ok(value)
}

const MAX_DEPTH: usize = 10000;

struct JsonParser<'a> {
    bytes: &'a [u8],
    text: &'a str,
    pos: usize,
}

impl JsonParser<'_> {
    fn error(&self, message: &str) -> String {
        format!("jsontext: {} at offset {}", message, self.pos)
    }

    fn skip_whitespace(&mut self) {
        while self.pos < self.bytes.len() {
            match self.bytes[self.pos] {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > MAX_DEPTH {
            return Err(self.error("exceeded max depth"));
        }
        if self.pos >= self.bytes.len() {
            return Err(self.error("unexpected EOF"));
        }
        match self.bytes[self.pos] {
            b'n' => self.parse_literal("null", Json::Null),
            b't' => self.parse_literal("true", Json::Bool(true)),
            b'f' => self.parse_literal("false", Json::Bool(false)),
            b'"' => Ok(Json::String(self.parse_string()?)),
            b'[' => {
                self.pos += 1;
                let mut elements = Vec::new();
                self.skip_whitespace();
                if self.peek() == Some(b']') {
                    self.pos += 1;
                    return Ok(Json::Array(elements));
                }
                loop {
                    self.skip_whitespace();
                    elements.push(self.parse_value(depth + 1)?);
                    self.skip_whitespace();
                    match self.peek() {
                        Some(b',') => self.pos += 1,
                        Some(b']') => {
                            self.pos += 1;
                            return Ok(Json::Array(elements));
                        }
                        None => return Err(self.error("unexpected EOF")),
                        _ => return Err(self.error("invalid character in array")),
                    }
                }
            }
            b'{' => {
                self.pos += 1;
                let mut members = Vec::new();
                self.skip_whitespace();
                if self.peek() == Some(b'}') {
                    self.pos += 1;
                    return Ok(Json::Object(members));
                }
                loop {
                    self.skip_whitespace();
                    if self.peek() != Some(b'"') {
                        return Err(self.error("invalid character, expected object name"));
                    }
                    let name = self.parse_string()?;
                    self.skip_whitespace();
                    if self.peek() != Some(b':') {
                        return Err(self.error("invalid character, expected ':'"));
                    }
                    self.pos += 1;
                    self.skip_whitespace();
                    let value = self.parse_value(depth + 1)?;
                    members.push((name, value));
                    self.skip_whitespace();
                    match self.peek() {
                        Some(b',') => self.pos += 1,
                        Some(b'}') => {
                            self.pos += 1;
                            return Ok(Json::Object(members));
                        }
                        None => return Err(self.error("unexpected EOF")),
                        _ => return Err(self.error("invalid character in object")),
                    }
                }
            }
            b'-' | b'0'..=b'9' => self.parse_number(),
            _ => Err(self.error("invalid character at start of value")),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn parse_literal(&mut self, literal: &str, value: Json) -> Result<Json, String> {
        if self.bytes[self.pos..].starts_with(literal.as_bytes()) {
            self.pos += literal.len();
            Ok(value)
        } else {
            Err(self.error("invalid literal"))
        }
    }

    fn parse_number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(self.error("invalid number")),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("invalid number"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("invalid number"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let text = &self.text[start..self.pos];
        match text.parse::<f64>() {
            Ok(n) if n.is_finite() => Ok(Json::Number(n)),
            _ => Err(self.error("number out of range")),
        }
    }

    fn parse_hex4(&mut self) -> Result<u32, String> {
        if self.pos + 4 > self.bytes.len() {
            return Err(self.error("unexpected EOF in escape"));
        }
        let mut value = 0u32;
        for _ in 0..4 {
            let digit = match self.bytes[self.pos] {
                c @ b'0'..=b'9' => (c - b'0') as u32,
                c @ b'a'..=b'f' => (c - b'a' + 10) as u32,
                c @ b'A'..=b'F' => (c - b'A' + 10) as u32,
                _ => return Err(self.error("invalid escape sequence")),
            };
            value = value * 16 + digit;
            self.pos += 1;
        }
        Ok(value)
    }

    fn parse_string(&mut self) -> Result<String, String> {
        // Caller has checked the opening quote.
        self.pos += 1;
        let mut result = String::new();
        let mut run_start = self.pos;
        loop {
            let Some(c) = self.peek() else {
                return Err(self.error("unexpected EOF in string"));
            };
            match c {
                b'"' => {
                    result.push_str(&self.text[run_start..self.pos]);
                    self.pos += 1;
                    return Ok(result);
                }
                b'\\' => {
                    result.push_str(&self.text[run_start..self.pos]);
                    self.pos += 1;
                    let Some(e) = self.peek() else {
                        return Err(self.error("unexpected EOF in string"));
                    };
                    self.pos += 1;
                    match e {
                        b'"' => result.push('"'),
                        b'\\' => result.push('\\'),
                        b'/' => result.push('/'),
                        b'b' => result.push('\u{8}'),
                        b'f' => result.push('\u{c}'),
                        b'n' => result.push('\n'),
                        b'r' => result.push('\r'),
                        b't' => result.push('\t'),
                        b'u' => {
                            let first = self.parse_hex4()?;
                            let code = if (0xD800..0xDC00).contains(&first) {
                                if self.bytes[self.pos..].starts_with(b"\\u") {
                                    self.pos += 2;
                                    let second = self.parse_hex4()?;
                                    if !(0xDC00..0xE000).contains(&second) {
                                        return Err(self.error("invalid surrogate pair in string"));
                                    }
                                    0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00)
                                } else {
                                    return Err(self.error("invalid surrogate pair in string"));
                                }
                            } else if (0xDC00..0xE000).contains(&first) {
                                return Err(self.error("invalid surrogate pair in string"));
                            } else {
                                first
                            };
                            match char::from_u32(code) {
                                Some(ch) => result.push(ch),
                                None => return Err(self.error("invalid escape sequence")),
                            }
                        }
                        _ => return Err(self.error("invalid escape sequence")),
                    }
                    run_start = self.pos;
                }
                0x00..=0x1F => return Err(self.error("invalid character in string")),
                _ => self.pos += 1,
            }
        }
    }
}
