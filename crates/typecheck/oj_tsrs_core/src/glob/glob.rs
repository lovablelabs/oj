// Copyright 2023 The Go Authors. All rights reserved.
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use std::fmt;

use crate::stringutil::{Rune, decode_rune, push_rune};

// A Glob is an LSP-compliant glob pattern, as defined by the spec:
// https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#documentFilter
//
// NOTE: this implementation is currently only intended for testing. In order
// to make it production ready, we'd need to:
//   - verify it against the VS Code implementation
//   - add more tests
//   - microbenchmark, likely avoiding the element interface
//   - resolve the question of what is meant by "character". If it's a UTF-16
//     code (as we suspect) it'll be a bit more work.
//
// Quoting from the spec:
// Glob patterns can have the following syntax:
//   - `*` to match one or more characters in a path segment
//   - `?` to match on one character in a path segment
//   - `**` to match any number of path segments, including none
//   - `{}` to group sub patterns into an OR expression. (e.g. `**/*.{ts,js}`
//     matches all TypeScript and JavaScript files)
//   - `[]` to declare a range of characters to match in a path segment
//     (e.g., `example.[0-9]` to match on `example.0`, `example.1`, …)
//   - `[!...]` to negate a range of characters to match in a path segment
//     (e.g., `example.[!0-9]` to match on `example.a`, `example.b`, but
//     not `example.0`)
//
// Expanding on this:
//   - '/' matches one or more literal slashes.
//   - any other character matches itself literally.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Glob {
    elems: Vec<Element>, // pattern elements
}

// Parse builds a Glob for the given pattern, returning an error if the pattern
// is invalid.
pub fn parse(pattern: &str) -> Result<Glob, String> {
    let (g, _) = parse_nested(pattern, false)?;
    Ok(g)
}

fn parse_nested(mut pattern: &str, nested: bool) -> Result<(Glob, &str), String> {
    let mut g = Glob::default();
    while !pattern.is_empty() {
        match pattern.as_bytes()[0] {
            b'/' => {
                pattern = &pattern[1..];
                g.elems.push(Element::Slash);
            }

            b'*' => {
                if pattern.len() > 1 && pattern.as_bytes()[1] == b'*' {
                    if (!g.elems.is_empty() && g.elems[g.elems.len() - 1] != Element::Slash)
                        || (pattern.len() > 2 && pattern.as_bytes()[2] != b'/')
                    {
                        return Err("** may only be adjacent to '/'".to_string());
                    }
                    pattern = &pattern[2..];
                    g.elems.push(Element::StarStar);
                    continue;
                }
                pattern = &pattern[1..];
                g.elems.push(Element::Star);
            }

            b'?' => {
                pattern = &pattern[1..];
                g.elems.push(Element::AnyChar);
            }

            b'{' => {
                let mut gs: Vec<Glob> = Vec::new();
                while pattern.as_bytes()[0] != b'}' {
                    pattern = &pattern[1..];
                    let (group_g, pat) = parse_nested(pattern, true)?;
                    if pat.is_empty() {
                        return Err("unmatched '{'".to_string());
                    }
                    pattern = pat;
                    gs.push(group_g);
                }
                pattern = &pattern[1..];
                g.elems.push(Element::Group(gs));
            }

            b'}' | b',' => {
                if nested {
                    return Ok((g, pattern));
                }
                pattern = g.parse_literal(pattern, false);
            }

            b'[' => {
                pattern = &pattern[1..];
                if pattern.is_empty() {
                    return Err(ERR_BAD_RANGE.to_string());
                }
                let mut negate = false;
                if pattern.as_bytes()[0] == b'!' {
                    pattern = &pattern[1..];
                    negate = true;
                }
                let (low, sz) = read_range_rune(pattern)?;
                pattern = &pattern[sz..];
                if pattern.is_empty() || pattern.as_bytes()[0] != b'-' {
                    return Err(ERR_BAD_RANGE.to_string());
                }
                pattern = &pattern[1..];
                let (high, sz) = read_range_rune(pattern)?;
                pattern = &pattern[sz..];
                if pattern.is_empty() || pattern.as_bytes()[0] != b']' {
                    return Err(ERR_BAD_RANGE.to_string());
                }
                pattern = &pattern[1..];
                g.elems.push(Element::CharRange { negate, low, high });
            }

            _ => {
                pattern = g.parse_literal(pattern, nested);
            }
        }
    }
    Ok((g, ""))
}

// helper for decoding a rune in range elements, e.g. [a-z]
fn read_range_rune(input: &str) -> Result<(Rune, usize), String> {
    let (r, sz) = decode_rune(input.as_bytes());
    if r == 0xFFFD {
        // See the documentation for DecodeRuneInString.
        match sz {
            0 => return Err(ERR_BAD_RANGE.to_string()),
            1 => return Err(ERR_INVALID_UTF8.to_string()),
            _ => {}
        }
    }
    Ok((r, sz))
}

pub const ERR_BAD_RANGE: &str = "'[' patterns must be of the form [x-y]";
const ERR_INVALID_UTF8: &str = "invalid UTF-8 encoding";

impl Glob {
    fn parse_literal<'a>(&mut self, pattern: &'a str, nested: bool) -> &'a str {
        let special_chars: &[u8] = if nested { b"*?{[/}," } else { b"*?{[/" };
        let end = pattern.bytes().position(|b| special_chars.contains(&b)).unwrap_or(pattern.len());
        self.elems.push(Element::Literal(pattern[..end].to_string()));
        &pattern[end..]
    }

    pub fn string(&self) -> String {
        let mut b = String::new();
        b.extend(self.elems.iter().map(Element::string));
        b
    }
}

impl fmt::Display for Glob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.string())
    }
}

// element holds a glob pattern element, as defined below.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Element {
    Slash,            // One or more '/' separators
    Literal(String),  // string literal, not containing /, *, ?, {}, or []
    Star,             // *
    AnyChar,          // ?
    StarStar,         // **
    Group(Vec<Glob>), // {foo, bar, ...} grouping
    CharRange {
        // [a-z] character range
        negate: bool,
        low: Rune,
        high: Rune,
    },
}

impl Element {
    fn string(&self) -> String {
        match self {
            Element::Slash => "/".to_string(),
            Element::Literal(l) => l.clone(),
            Element::Star => "*".to_string(),
            Element::AnyChar => "?".to_string(),
            Element::StarStar => "**".to_string(),
            Element::Group(g) => {
                let parts: Vec<String> = g.iter().map(Glob::string).collect();
                "{".to_string() + &parts.join(",") + "}"
            }
            Element::CharRange { low, high, .. } => {
                let mut s = "[".to_string();
                push_rune(&mut s, *low);
                s.push('-');
                push_rune(&mut s, *high);
                s.push(']');
                s
            }
        }
    }
}

impl fmt::Display for Element {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.string())
    }
}

impl Glob {
    // Match reports whether the input string matches the glob pattern.
    pub fn match_(&self, input: &str) -> bool {
        let elems: Vec<&Element> = self.elems.iter().collect();
        match_(&elems, input.as_bytes())
    }
}

fn match_(mut elems: &[&Element], mut input: &[u8]) -> bool {
    while !elems.is_empty() {
        let elem = elems[0];
        elems = &elems[1..];
        match elem {
            Element::Slash => {
                if input.is_empty() || input[0] != b'/' {
                    return false;
                }
                while input[0] == b'/' {
                    input = &input[1..];
                }
            }

            Element::StarStar => {
                // Special cases:
                //  - **/a matches "a"
                //  - **/ matches everything
                //
                // Note that if ** is followed by anything, it must be '/' (this is
                // enforced by Parse).
                if !elems.is_empty() {
                    elems = &elems[1..];
                }

                // A trailing ** matches anything.
                if elems.is_empty() {
                    return true;
                }

                // Backtracking: advance pattern segments until the remaining pattern
                // elements match.
                while !input.is_empty() {
                    if match_(elems, input) {
                        return true;
                    }
                    (_, input) = split(input);
                }
                return false;
            }

            Element::Literal(l) => {
                if !input.starts_with(l.as_bytes()) {
                    return false;
                }
                input = &input[l.len()..];
            }

            Element::Star => {
                let seg_input;
                (seg_input, input) = split(input);

                let mut elem_end = elems.len();
                if let Some((i, _e)) = elems
                    .iter()
                    .enumerate()
                    .find_map(|(i, e)| if **e == Element::Slash { Some((i, e)) } else { None })
                {
                    elem_end = i;
                }
                let seg_elems = &elems[..elem_end];
                elems = &elems[elem_end..];

                // A trailing * matches the entire segment.
                if seg_elems.is_empty() {
                    continue;
                }

                // Backtracking: advance characters until remaining subpattern elements
                // match.
                let matched = (0..seg_input.len()).any(|i| match_(seg_elems, &seg_input[i..]));
                if !matched {
                    return false;
                }
            }

            Element::AnyChar => {
                if input.is_empty() || input[0] == b'/' {
                    return false;
                }
                input = &input[1..];
            }

            Element::Group(g) => {
                // Append remaining pattern elements to each group member looking for a
                // match.
                let mut branch: Vec<&Element> = Vec::new();
                for m in g {
                    branch.clear();
                    branch.extend(m.elems.iter());
                    branch.extend_from_slice(elems);
                    if match_(&branch, input) {
                        return true;
                    }
                }
                return false;
            }

            Element::CharRange { low, high, .. } => {
                if input.is_empty() || input[0] == b'/' {
                    return false;
                }
                let (c, sz) = decode_rune(input);
                if c < *low || c > *high {
                    return false;
                }
                input = &input[sz..];
            }
        }
    }

    input.is_empty()
}

// split returns the portion before and after the first slash
// (or sequence of consecutive slashes). If there is no slash
// it returns (input, nil).
fn split(input: &[u8]) -> (&[u8], &[u8]) {
    let Some(i) = memchr::memchr(b'/', input) else {
        return (input, b"");
    };
    let first = &input[..i];
    if let Some((j, _)) = input.iter().enumerate().skip(i).find(|&(_, &byte)| byte != b'/') {
        return (first, &input[j..]);
    }
    (first, b"")
}
