//! CSS value tokenizer, behavior-equivalent to postcss-value-parser@4.2.0.
// parity: postcss-value-parser lib/{parse,walk,stringify,unit}.js

use std::borrow::Cow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Word,
    Space,
    Div,
    Str,
    Comment,
    Func,
    UnicodeRange,
}

// Flat node shape mirroring the JS objects; unused fields stay at defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub kind: Kind,
    pub value: String,
    pub source_index: usize,
    pub source_end_index: usize,
    pub before: String,
    pub after: String,
    pub quote: u8,
    pub unclosed: bool,
    pub nodes: Vec<Node>,
}

impl Node {
    fn new(kind: Kind) -> Self {
        Node {
            kind,
            value: String::new(),
            source_index: 0,
            source_end_index: 0,
            before: String::new(),
            after: String::new(),
            quote: 0,
            unclosed: false,
            nodes: Vec::new(),
        }
    }

    fn leaf(kind: Kind, value: String, source_index: usize, source_end_index: usize) -> Self {
        Node {
            value,
            source_index,
            source_end_index,
            ..Node::new(kind)
        }
    }
}

fn utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn find_byte(haystack: &[u8], needle: u8, from: usize) -> Option<usize> {
    if from >= haystack.len() {
        return None;
    }
    memchr::memchr(needle, &haystack[from..]).map(|i| i + from)
}

fn find_subslice(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| i + from)
}

fn tokens<'a>(stack: &'a mut [Node], root: &'a mut Vec<Node>) -> &'a mut Vec<Node> {
    match stack.last_mut() {
        Some(top) => &mut top.nodes,
        None => root,
    }
}

fn is_unicode_range(token: &[u8]) -> bool {
    token.len() > 2
        && (token[0] == b'u' || token[0] == b'U')
        && token[1] == b'+'
        && token[2..]
            .iter()
            .all(|c| c.is_ascii_hexdigit() || *c == b'?' || *c == b'-')
}

pub fn parse(input: &str) -> Vec<Node> {
    // The buffer can grow (unclosed string/url get a synthetic closer); `max`
    // stays at the original length, exactly like upstream's captured `value.length`.
    let mut value: Cow<[u8]> = Cow::Borrowed(input.as_bytes());
    let max = value.len();
    let mut pos: usize = 0;

    let mut root: Vec<Node> = Vec::new();
    let mut stack: Vec<Node> = Vec::new();
    // Upstream's `parent` is undefined until the first function opens and stays
    // the root wrapper afterwards; that changes the whitespace-before-'/' rule.
    let mut entered_function = false;

    let mut name = String::new();
    let mut before_pending = String::new();
    let mut after_pending = String::new();

    while pos < max {
        let code = value[pos];
        let in_calc = stack.last().is_some_and(|f| f.value == "calc");

        if code <= 32 {
            // whitespace run
            let mut next = pos + 1;
            while value.get(next).is_some_and(|c| *c <= 32) {
                next += 1;
            }
            let token = utf8(&value[pos..next]);
            let next_code = value.get(next).copied();
            let in_function = !stack.is_empty();
            let slash_takes_before = match stack.last() {
                Some(f) => f.value != "calc",
                None => !entered_function,
            };
            let cur = tokens(&mut stack, &mut root);
            if next_code == Some(b')') && in_function {
                after_pending = token;
            } else if let Some(prev) = cur.last_mut().filter(|p| p.kind == Kind::Div) {
                prev.source_end_index += token.len();
                prev.after = token;
            } else if next_code == Some(b',')
                || next_code == Some(b':')
                || (next_code == Some(b'/')
                    && value.get(next + 1) != Some(&b'*')
                    && slash_takes_before)
            {
                before_pending = token;
            } else {
                cur.push(Node::leaf(Kind::Space, token, pos, next));
            }
            pos = next;
        } else if code == b'\'' || code == b'"' {
            let quote = code;
            let mut next = pos;
            let mut unclosed = false;
            loop {
                let mut escape = false;
                match find_byte(&value, quote, next + 1) {
                    Some(idx) => {
                        next = idx;
                        let mut escape_pos = next;
                        while escape_pos > 0 && value[escape_pos - 1] == b'\\' {
                            escape_pos -= 1;
                            escape = !escape;
                        }
                    }
                    None => {
                        value.to_mut().push(quote);
                        next = value.len() - 1;
                        unclosed = true;
                    }
                }
                if !escape {
                    break;
                }
            }
            let mut node = Node::leaf(
                Kind::Str,
                utf8(&value[pos + 1..next]),
                pos,
                if unclosed { next } else { next + 1 },
            );
            node.quote = quote;
            node.unclosed = unclosed;
            tokens(&mut stack, &mut root).push(node);
            pos = next + 1;
        } else if code == b'/' && value.get(pos + 1) == Some(&b'*') {
            let mut node = Node::new(Kind::Comment);
            node.source_index = pos;
            let next = match find_subslice(&value, b"*/", pos) {
                Some(idx) => {
                    node.source_end_index = idx + 2;
                    idx
                }
                None => {
                    node.unclosed = true;
                    node.source_end_index = value.len();
                    value.len()
                }
            };
            if next > pos + 2 {
                node.value = utf8(&value[pos + 2..next]);
            }
            tokens(&mut stack, &mut root).push(node);
            pos = next + 2;
        } else if (code == b'/' || code == b'*') && in_calc {
            // operator word directly inside calc()
            let node = Node::leaf(
                Kind::Word,
                (code as char).to_string(),
                pos - before_pending.len(),
                pos + 1,
            );
            tokens(&mut stack, &mut root).push(node);
            pos += 1;
        } else if code == b'/' || code == b',' || code == b':' {
            let mut node = Node::leaf(
                Kind::Div,
                (code as char).to_string(),
                pos - before_pending.len(),
                pos + 1,
            );
            node.before = std::mem::take(&mut before_pending);
            tokens(&mut stack, &mut root).push(node);
            pos += 1;
        } else if code == b'(' {
            let mut next = pos + 1;
            while value.get(next).is_some_and(|c| *c <= 32) {
                next += 1;
            }
            let paren_open = pos;
            let mut func = Node::new(Kind::Func);
            func.source_index = pos - name.len();
            func.value = std::mem::take(&mut name);
            func.before = utf8(&value[paren_open + 1..next]);
            let code_at_next = value.get(next).copied();
            pos = next;

            if func.value == "url" && code_at_next != Some(b'\'') && code_at_next != Some(b'"') {
                // unquoted url(): raw consumption up to the matching ')'
                let mut next2 = next - 1;
                loop {
                    let mut escape = false;
                    match find_byte(&value, b')', next2 + 1) {
                        Some(idx) => {
                            next2 = idx;
                            let mut escape_pos = next2;
                            while escape_pos > 0 && value[escape_pos - 1] == b'\\' {
                                escape_pos -= 1;
                                escape = !escape;
                            }
                        }
                        None => {
                            value.to_mut().push(b')');
                            next2 = value.len() - 1;
                            func.unclosed = true;
                        }
                    }
                    if !escape {
                        break;
                    }
                }
                let mut whitespace_pos = next2;
                loop {
                    whitespace_pos -= 1;
                    if value[whitespace_pos] > 32 {
                        break;
                    }
                }
                if paren_open < whitespace_pos {
                    if pos != whitespace_pos + 1 {
                        func.nodes = vec![Node::leaf(
                            Kind::Word,
                            utf8(&value[pos..whitespace_pos + 1]),
                            pos,
                            whitespace_pos + 1,
                        )];
                    }
                    if func.unclosed && whitespace_pos + 1 != next2 {
                        func.nodes.push(Node::leaf(
                            Kind::Space,
                            utf8(&value[whitespace_pos + 1..next2]),
                            whitespace_pos + 1,
                            next2,
                        ));
                    } else {
                        func.after = utf8(&value[whitespace_pos + 1..next2]);
                    }
                }
                pos = next2 + 1;
                func.source_end_index = if func.unclosed { next2 } else { pos };
                tokens(&mut stack, &mut root).push(func);
            } else {
                entered_function = true;
                func.source_end_index = pos + 1;
                stack.push(func);
            }
        } else if code == b')' && !stack.is_empty() {
            pos += 1;
            let mut func = stack.pop().expect("checked non-empty");
            func.after = std::mem::take(&mut after_pending);
            func.source_end_index = pos;
            tokens(&mut stack, &mut root).push(func);
        } else {
            // word
            let mut next = pos;
            let mut cur = code;
            loop {
                if cur == b'\\' {
                    next += 1;
                }
                next += 1;
                if next >= max {
                    break;
                }
                let c = value[next];
                let is_break = c <= 32
                    || c == b'\''
                    || c == b'"'
                    || c == b','
                    || c == b':'
                    || c == b'/'
                    || c == b'('
                    || (c == b'*' && in_calc)
                    || (c == b')' && !stack.is_empty());
                if is_break {
                    break;
                }
                cur = c;
            }
            let token = utf8(&value[pos..next.min(value.len())]);
            if value.get(next) == Some(&b'(') {
                name = token;
            } else if is_unicode_range(token.as_bytes()) {
                tokens(&mut stack, &mut root).push(Node::leaf(
                    Kind::UnicodeRange,
                    token,
                    pos,
                    next,
                ));
            } else {
                tokens(&mut stack, &mut root).push(Node::leaf(Kind::Word, token, pos, next));
            }
            pos = next;
        }
    }

    while let Some(mut func) = stack.pop() {
        func.unclosed = true;
        func.source_end_index = value.len();
        tokens(&mut stack, &mut root).push(func);
    }
    root
}

pub fn walk<F: FnMut(&mut Node)>(nodes: &mut [Node], f: &mut F) {
    for node in nodes.iter_mut() {
        f(node);
        if node.kind == Kind::Func {
            walk(&mut node.nodes, f);
        }
    }
}

pub fn stringify(nodes: &[Node]) -> String {
    let mut out = String::new();
    for node in nodes {
        stringify_node(node, &mut out);
    }
    out
}

fn stringify_node(node: &Node, out: &mut String) {
    match node.kind {
        Kind::Word | Kind::Space | Kind::UnicodeRange => out.push_str(&node.value),
        Kind::Str => {
            let quote = node.quote as char;
            out.push(quote);
            out.push_str(&node.value);
            if !node.unclosed {
                out.push(quote);
            }
        }
        Kind::Comment => {
            out.push_str("/*");
            out.push_str(&node.value);
            if !node.unclosed {
                out.push_str("*/");
            }
        }
        Kind::Div => {
            out.push_str(&node.before);
            out.push_str(&node.value);
            out.push_str(&node.after);
        }
        Kind::Func => {
            out.push_str(&node.value);
            out.push('(');
            out.push_str(&node.before);
            for child in &node.nodes {
                stringify_node(child, out);
            }
            out.push_str(&node.after);
            if !node.unclosed {
                out.push(')');
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dimension<'a> {
    pub number: &'a str,
    pub unit: &'a str,
}

// https://www.w3.org/TR/css-syntax-3/#starts-with-a-number
fn like_number(b: &[u8]) -> bool {
    match b.first() {
        Some(b'+') | Some(b'-') => match b.get(1) {
            Some(c) if c.is_ascii_digit() => true,
            Some(b'.') => b.get(2).is_some_and(|c| c.is_ascii_digit()),
            _ => false,
        },
        Some(b'.') => b.get(1).is_some_and(|c| c.is_ascii_digit()),
        Some(c) => c.is_ascii_digit(),
        None => false,
    }
}

pub fn unit(value: &str) -> Option<Dimension<'_>> {
    let b = value.as_bytes();
    if b.is_empty() || !like_number(b) {
        return None;
    }
    let mut pos = 0;
    if b[0] == b'+' || b[0] == b'-' {
        pos += 1;
    }
    while b.get(pos).is_some_and(|c| c.is_ascii_digit()) {
        pos += 1;
    }
    if b.get(pos) == Some(&b'.') && b.get(pos + 1).is_some_and(|c| c.is_ascii_digit()) {
        pos += 2;
        while b.get(pos).is_some_and(|c| c.is_ascii_digit()) {
            pos += 1;
        }
    }
    if matches!(b.get(pos), Some(b'e') | Some(b'E')) {
        let c1 = b.get(pos + 1);
        let c2 = b.get(pos + 2);
        let consumed = if c1.is_some_and(|c| c.is_ascii_digit()) {
            Some(2)
        } else if matches!(c1, Some(b'+') | Some(b'-')) && c2.is_some_and(|c| c.is_ascii_digit()) {
            Some(3)
        } else {
            None
        };
        if let Some(n) = consumed {
            pos += n;
            while b.get(pos).is_some_and(|c| c.is_ascii_digit()) {
                pos += 1;
            }
        }
    }
    Some(Dimension {
        number: &value[..pos],
        unit: &value[pos..],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(input: &str) -> String {
        stringify(&parse(input))
    }

    #[test]
    fn roundtrips_preserve_source() {
        for input in [
            "1px solid red",
            "  1px   solid   red  ",
            "calc( 100% - 10px )",
            "calc(100%/3)",
            "url( ./a.png )",
            "url(data:image/png;base64,iVBOR)",
            "url(\"a b.png\")",
            "var(--x, 10px)",
            "\"say \\\"hi\\\"\"",
            "'a' \"b\"",
            "U+0025-00FF",
            "translateX(0px) translateY(0px)",
            "16 / 9",
            "16/9",
            "/* c */ 10px",
            "rgb(255 0 0 / 0.5)",
        ] {
            assert_eq!(roundtrip(input), input, "roundtrip({input:?})");
        }
    }

    #[test]
    fn unclosed_string_and_function() {
        let nodes = parse("\"abc");
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0].unclosed);
        assert_eq!(nodes[0].kind, Kind::Str);
        assert_eq!(stringify(&nodes), "\"abc");

        let nodes = parse("calc(100%");
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0].unclosed);
        assert_eq!(nodes[0].kind, Kind::Func);
        assert_eq!(stringify(&nodes), "calc(100%");
    }

    #[test]
    fn whitespace_before_div_becomes_before() {
        let nodes = parse("a , b");
        let div = nodes.iter().find(|n| n.kind == Kind::Div).unwrap();
        assert_eq!(div.before, " ");
        assert_eq!(div.after, " ");
    }

    // parity: parse.js keeps `parent` pointing at the root wrapper after a
    // function closes, so whitespace before '/' stays a space node there.
    #[test]
    fn slash_whitespace_after_closed_function_stays_space() {
        let nodes = parse("calc(10px) 2px / 3px");
        let kinds: Vec<Kind> = nodes.iter().map(|n| n.kind).collect();
        assert_eq!(
            kinds,
            vec![
                Kind::Func,
                Kind::Space,
                Kind::Word,
                Kind::Space,
                Kind::Div,
                Kind::Word
            ]
        );
        let div = &nodes[4];
        assert_eq!(div.before, "");
    }

    #[test]
    fn unit_matches_reference_behavior() {
        assert_eq!(
            unit("500ms"),
            Some(Dimension {
                number: "500",
                unit: "ms"
            })
        );
        assert_eq!(
            unit(".5.5px"),
            Some(Dimension {
                number: ".5",
                unit: ".5px"
            })
        );
        assert_eq!(
            unit("1e-7px"),
            Some(Dimension {
                number: "1e-7",
                unit: "px"
            })
        );
        assert_eq!(
            unit("-0px"),
            Some(Dimension {
                number: "-0",
                unit: "px"
            })
        );
        assert_eq!(unit("Infinityms"), None);
        assert_eq!(unit("px"), None);
        assert_eq!(unit(""), None);
        assert_eq!(
            unit("5e"),
            Some(Dimension {
                number: "5",
                unit: "e"
            })
        );
    }
}
