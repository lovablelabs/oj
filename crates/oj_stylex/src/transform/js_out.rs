//! Span-splice output: compiled values printed as parseable JS and applied as
//! non-overlapping text edits over the original source (design-core.md §3b/§3c).

use oxc_span::Span;

use crate::eval::value::{EvalValue, JsObjectMap};
use crate::jsrt::{js_number_to_string, write_js_number};

/// One text edit; `start == end` is a pure insertion.
#[derive(Debug, Clone)]
pub struct Edit {
    pub start: u32,
    pub end: u32,
    pub text: String,
}

impl Edit {
    pub fn replace(span: Span, text: String) -> Self {
        Edit {
            start: span.start,
            end: span.end,
            text,
        }
    }

    pub fn insert(offset: u32, text: String) -> Self {
        Edit {
            start: offset,
            end: offset,
            text,
        }
    }
}

/// Applies edits to `source`. Edits must be non-overlapping; same-position
/// insertions apply in their submission order.
pub fn apply_edits(source: &str, edits: &[Edit]) -> String {
    apply_edits_tracked(source, edits, None)
}

/// [`apply_edits`] that also records generated-to-original positions when
/// `map` is supplied; the returned text is byte-identical either way.
pub fn apply_edits_tracked(
    source: &str,
    edits: &[Edit],
    mut map: Option<&mut SpliceMap>,
) -> String {
    let mut ordered: Vec<(usize, &Edit)> = edits.iter().enumerate().collect();
    ordered.sort_by_key(|(i, e)| (e.start, e.end, *i));
    let inserted: usize = edits.iter().map(|e| e.text.len()).sum();
    let replaced: usize = edits.iter().map(|e| (e.end - e.start) as usize).sum();
    let mut out = String::with_capacity(source.len().saturating_sub(replaced) + inserted);
    let mut cursor = 0usize;
    for (_, edit) in ordered {
        let start = edit.start as usize;
        let end = edit.end as usize;
        assert!(start >= cursor, "overlapping stylex output edits");
        if let Some(map) = map.as_mut() {
            map.copy(&source[cursor..start]);
            map.insert(&edit.text);
            map.skip(&source[start..end]);
        }
        out.push_str(&source[cursor..start]);
        out.push_str(&edit.text);
        cursor = end;
    }
    if let Some(map) = map.as_mut() {
        map.copy(&source[cursor..]);
    }
    out.push_str(&source[cursor..]);
    out
}

/// Generated-to-original positions for a splice compile, built by walking the
/// source left to right exactly as [`apply_edits_tracked`] does.
#[derive(Debug, Default)]
pub struct SpliceMap {
    /// (generated line, generated column, source line, source column), all
    /// zero-based; columns are UTF-16 code units, as JS tooling reads them.
    pub tokens: Vec<(u32, u32, u32, u32)>,
    out_line: u32,
    out_col: u32,
    src_line: u32,
    src_col: u32,
}

impl SpliceMap {
    fn token(&mut self) {
        self.tokens
            .push((self.out_line, self.out_col, self.src_line, self.src_col));
    }

    /// Verbatim text: source and output advance together, so one token per
    /// generated line carries the whole run at exact columns.
    fn copy(&mut self, text: &str) {
        let mut at_line_start = true;
        for ch in text.chars() {
            if at_line_start {
                self.token();
                at_line_start = false;
            }
            if ch == '\n' {
                self.out_line += 1;
                self.out_col = 0;
                self.src_line += 1;
                self.src_col = 0;
                at_line_start = true;
            } else {
                let w = ch.len_utf16() as u32;
                self.out_col += w;
                self.src_col += w;
            }
        }
    }

    /// Synthesized text: every generated line it spans points at the start of
    /// the span it replaced, so a frame inside it resolves to the original.
    fn insert(&mut self, text: &str) {
        let mut at_line_start = true;
        for ch in text.chars() {
            if at_line_start {
                self.token();
                at_line_start = false;
            }
            if ch == '\n' {
                self.out_line += 1;
                self.out_col = 0;
                at_line_start = true;
            } else {
                self.out_col += ch.len_utf16() as u32;
            }
        }
    }

    /// Replaced source: consumed without emitting, keeping the source cursor
    /// aligned for the next copied run.
    fn skip(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' {
                self.src_line += 1;
                self.src_col = 0;
            } else {
                self.src_col += ch.len_utf16() as u32;
            }
        }
    }
}

/// Renders `span`'s source text with the given (contained) edits applied.
pub fn render_span(source: &str, span: Span, edits: &[Edit]) -> String {
    let base = span.start as usize;
    let slice = &source[base..span.end as usize];
    let contained: Vec<Edit> = edits
        .iter()
        .filter(|e| e.start >= span.start && e.end <= span.end)
        .map(|e| Edit {
            start: e.start - span.start,
            end: e.end - span.start,
            text: e.text.clone(),
        })
        .collect();
    apply_edits(slice, &contained)
}

// parity: @babel/types isValidIdentifier(name, false) — keywords stay valid
// as object keys; non-ASCII identifiers conservatively print quoted instead.
pub fn is_identifier_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

// parity: @babel/generator via jsesc {quotes: 'double', wrap: true,
// minimal: false} — ASCII-safe output, \xXX below U+0100, \uXXXX above.
pub fn js_string_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    write_js_string_literal(value, &mut out);
    out
}

pub fn write_js_string_literal(value: &str, out: &mut String) {
    use std::fmt::Write;
    out.push('"');
    if value
        .bytes()
        .all(|b| matches!(b, 0x20..=0x7E) && b != b'"' && b != b'\\')
    {
        out.push_str(value);
        out.push('"');
        return;
    }
    let mut units = value.encode_utf16().peekable();
    while let Some(unit) = units.next() {
        match unit {
            0x0000 => {
                let digit_next = units.peek().is_some_and(|n| (0x0030..=0x0039).contains(n));
                out.push_str(if digit_next { "\\x00" } else { "\\0" });
            }
            0x0008 => out.push_str("\\b"),
            0x0009 => out.push_str("\\t"),
            0x000A => out.push_str("\\n"),
            0x000C => out.push_str("\\f"),
            0x000D => out.push_str("\\r"),
            0x0022 => out.push_str("\\\""),
            0x005C => out.push_str("\\\\"),
            0x0020..=0x007E => out.push(unit as u8 as char),
            _ if unit < 0x0100 => {
                let _ = write!(out, "\\x{unit:02X}");
            }
            _ => {
                let _ = write!(out, "\\u{unit:04X}");
            }
        }
    }
    out.push('"');
}

/// JSON string escaping for the canonical-JSON surface (never JS-printed).
fn json_string_literal(value: &str) -> String {
    serde_json::to_string(value).expect("string serialization is infallible")
}

/// Printed-size estimate for pre-sizing buffers: exact for keywords and
/// ASCII strings, a typical width for numbers.
pub fn estimate_len(value: &EvalValue) -> usize {
    match value {
        EvalValue::Null => 4,
        EvalValue::Undefined => 9,
        EvalValue::Bool(_) => 5,
        EvalValue::Num(_) => 8,
        EvalValue::Str(s) => s.len() + 2,
        EvalValue::Arr(items) => {
            2 + items
                .iter()
                .map(|item| estimate_len(item) + 7)
                .sum::<usize>()
        }
        EvalValue::Obj(map) => estimate_object_len(map),
    }
}

pub fn estimate_object_len(map: &JsObjectMap) -> usize {
    2 + map
        .entries()
        .map(|(key, value)| {
            key.len() + if is_identifier_key(key) { 4 } else { 6 } + estimate_len(value)
        })
        .sum::<usize>()
}

/// Prints an `EvalValue` as a JS expression (object/array literal shape).
pub fn print_value(value: &EvalValue) -> String {
    let mut out = String::with_capacity(estimate_len(value));
    write_value(value, &mut out);
    out
}

pub fn write_value(value: &EvalValue, out: &mut String) {
    match value {
        EvalValue::Null => out.push_str("null"),
        EvalValue::Undefined => out.push_str("undefined"),
        EvalValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        EvalValue::Num(n) => write_js_number(*n, out),
        EvalValue::Str(s) => write_js_string_literal(s, out),
        // parity: convertObjectToAST recurses into arrays via Object.entries,
        // printing them as index-keyed objects ({"0": …, "1": …}).
        EvalValue::Arr(items) => {
            use std::fmt::Write;
            out.push('{');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, "\"{i}\"");
                out.push_str(": ");
                write_value(item, out);
            }
            out.push('}');
        }
        EvalValue::Obj(map) => write_object(map, out),
    }
}

pub fn write_object(map: &JsObjectMap, out: &mut String) {
    out.push('{');
    for (i, (key, value)) in map.entries().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        if is_identifier_key(key) {
            out.push_str(key);
        } else {
            write_js_string_literal(key, out);
        }
        out.push_str(": ");
        write_value(value, out);
    }
    out.push('}');
}

/// One runtime-injection statement, `callee({ltr, rtl?, priority, constKey?,
/// constVal?});\n`. parity: state-manager.js addStyleToInject key order.
pub fn inject_call_text(callee: &str, rule: &crate::rules::StylexRule) -> String {
    let mut out = String::with_capacity(
        callee.len() + rule.ltr.len() + rule.rtl.as_ref().map_or(0, |rtl| rtl.len() + 9) + 40,
    );
    out.push_str(callee);
    out.push_str("({ltr: ");
    write_js_string_literal(&rule.ltr, &mut out);
    if let Some(rtl) = &rule.rtl {
        out.push_str(", rtl: ");
        write_js_string_literal(rtl, &mut out);
    }
    out.push_str(", priority: ");
    write_js_number(rule.priority, &mut out);
    if let Some(const_key) = &rule.const_key {
        out.push_str(", constKey: ");
        write_js_string_literal(const_key, &mut out);
    }
    if let Some(const_val) = &rule.const_val {
        out.push_str(", constVal: ");
        out.push_str(&print_const_val(const_val));
    }
    out.push_str("});\n");
    out
}

/// parity: `typeof v === 'number' ? numericLiteral(v) : stringLiteral(String(v))`.
pub fn print_const_val(value: &serde_json::Value) -> String {
    match const_val_number(value) {
        Some(n) => js_number_to_string(n),
        None => js_string_literal(&const_val_string(value)),
    }
}

pub fn const_val_number(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(n) => n.as_f64(),
        other => crate::rules::non_finite_from_tag(other),
    }
}

// JS String(v) for the non-numeric constVal shapes defineConsts can carry.
pub fn const_val_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Array(items) => items
            .iter()
            .map(|item| {
                if item.is_null() {
                    String::new()
                } else {
                    const_val_string(item)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        serde_json::Value::Object(_) => "[object Object]".to_string(),
        serde_json::Value::Number(n) => js_number_to_string(n.as_f64().unwrap_or(f64::NAN)),
    }
}

// parity: upstream builds the class list as a `+` chain of string literals
// and never folds it, even when every operand is static.
fn write_class_segments(segments: &[String], out: &mut String) {
    if segments.is_empty() {
        out.push_str("\"\"");
        return;
    }
    for (i, segment) in segments.iter().enumerate() {
        if i > 0 {
            out.push_str(" + ");
        }
        write_js_string_literal(segment, out);
    }
}

/// Static chunk printer (the object upstream hoists via hoistExpression);
/// its `$$css` key is a string literal, the conditional chunk's an identifier.
pub fn print_static_chunk(compiled: &crate::shared::dynamic::DynamicCompiled) -> String {
    let mut obj = String::from("{");
    for (key, segments) in &compiled.static_props {
        if is_identifier_key(key) {
            obj.push_str(key);
        } else {
            write_js_string_literal(key, &mut obj);
        }
        obj.push_str(": ");
        write_class_segments(segments, &mut obj);
        obj.push_str(", ");
    }
    obj.push_str("\"$$css\": ");
    write_value(&compiled.css_tag, &mut obj);
    obj.push('}');
    obj
}

/// Dynamic-namespace arrow printer: spans splice verbatim (parenthesized),
/// hoisted static chunks splice as `static_ident`. parity: stylex-create.js:294-485
pub fn print_dynamic_arrow(
    compiled: &crate::shared::dynamic::DynamicCompiled,
    source: &str,
    static_ident: Option<&str>,
) -> String {
    use crate::shared::dynamic::ClassPart;
    let expr_text = |span: Span| -> &str { &source[span.start as usize..span.end as usize] };

    let mut vars = String::from("{");
    for (i, var) in compiled.inline_vars.iter().enumerate() {
        if i > 0 {
            vars.push_str(", ");
        }
        vars.push_str(&js_string_literal(&var.var_name));
        vars.push_str(": ");
        let src = expr_text(var.expr);
        if var.unit.is_empty() {
            vars.push_str(&format!("({src}) != null ? ({src}) : undefined"));
        } else {
            vars.push_str(&format!(
                "((val) => typeof val === \"number\" ? val + {unit} : val != null ? val : undefined)(({src}))",
                unit = js_string_literal(&var.unit)
            ));
        }
    }
    vars.push('}');

    let params = compiled.params.join(", ");
    if !compiled.has_container() {
        return format!("({params}) => ({vars})");
    }

    let css_tag = print_value(&compiled.css_tag);
    let write_key = |out: &mut String, key: &str| {
        if is_identifier_key(key) {
            out.push_str(key);
        } else {
            out.push_str(&js_string_literal(key));
        }
    };
    let mut items: Vec<String> = Vec::new();
    if !compiled.static_props.is_empty() {
        items.push(match static_ident {
            Some(ident) => ident.to_string(),
            None => print_static_chunk(compiled),
        });
    }
    if !compiled.conditional_props.is_empty() {
        let mut obj = String::from("{");
        for (key, parts) in &compiled.conditional_props {
            write_key(&mut obj, key);
            obj.push_str(": ");
            let rendered: Vec<String> = parts
                .iter()
                .map(|part| match part {
                    ClassPart::Lit(lit) => js_string_literal(lit),
                    ClassPart::Guarded { lit, expr } => {
                        let src = expr_text(*expr);
                        format!("(({src}) != null ? {} : ({src}))", js_string_literal(lit))
                    }
                })
                .collect();
            if rendered.is_empty() {
                obj.push_str("\"\"");
            } else {
                obj.push_str(&rendered.join(" + "));
            }
            obj.push_str(", ");
        }
        // parity: the conditional chunk's $$css key is an identifier upstream.
        obj.push_str(&format!("$$css: {css_tag}}}"));
        items.push(obj);
    }
    items.push(vars);
    format!("({params}) => [{}]", items.join(", "))
}

/// Canonical JSON (keys in JS ownKeys order) for the extract-objects surface.
pub fn to_canonical_json(value: &EvalValue) -> String {
    let mut out = String::new();
    write_json(value, &mut out);
    out
}

fn write_json(value: &EvalValue, out: &mut String) {
    match value {
        EvalValue::Null | EvalValue::Undefined => out.push_str("null"),
        EvalValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        EvalValue::Num(n) => {
            if n.is_finite() {
                write_js_number(*n, out);
            } else {
                out.push_str(&format!(
                    "{{\"__jsnum\":{}}}",
                    json_string_literal(&js_number_to_string(*n))
                ));
            }
        }
        EvalValue::Str(s) => out.push_str(&json_string_literal(s)),
        EvalValue::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json(item, out);
            }
            out.push(']');
        }
        EvalValue::Obj(map) => {
            out.push('{');
            for (i, (key, item)) in map.entries().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&json_string_literal(key));
                out.push(':');
                write_json(item, out);
            }
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(entries: &[(&str, EvalValue)]) -> EvalValue {
        EvalValue::Obj(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect::<JsObjectMap>()
                .into(),
        )
    }

    #[test]
    fn prints_identifier_and_quoted_keys() {
        let v = obj(&[
            ("kMwMTN", EvalValue::Str("xju2f9n".into())),
            ("$$css", EvalValue::Bool(true)),
            ("data-style-src", EvalValue::Str("f.ts:3".into())),
            ("0", EvalValue::Null),
        ]);
        assert_eq!(
            print_value(&v),
            "{\"0\": null, kMwMTN: \"xju2f9n\", $$css: true, \"data-style-src\": \"f.ts:3\"}"
        );
    }

    #[test]
    fn prints_numbers_via_jsrt() {
        let v = obj(&[
            ("a", EvalValue::Num(12.0)),
            ("b", EvalValue::Num(0.5)),
            ("c", EvalValue::Num(-4.0)),
        ]);
        assert_eq!(print_value(&v), "{a: 12, b: 0.5, c: -4}");
    }

    #[test]
    fn jsesc_escapes_match_babel_generator() {
        // Pinned live against @babel/generator 7.29.8 (probe 2026-08-28).
        assert_eq!(
            js_string_literal("\"a\u{2028}b\u{2029}c\0d\u{000B}e\u{1F600}f\u{00E9}\""),
            "\"\\\"a\\u2028b\\u2029c\\0d\\x0Be\\uD83D\\uDE00f\\xE9\\\"\""
        );
        assert_eq!(js_string_literal("\x001"), r#""\x001""#);
        assert_eq!(js_string_literal("a'`b"), "\"a'`b\"");
        assert_eq!(
            js_string_literal("\\\n\t\r\x08\x0C\x7F"),
            r#""\\\n\t\r\b\f\x7F""#
        );
    }

    #[test]
    fn arrays_print_as_indexed_objects() {
        // parity: convertObjectToAST via Object.entries over the array.
        let v = obj(&[(
            "fontFamily",
            EvalValue::Arr(vec![
                EvalValue::Str("Arial".into()),
                EvalValue::Str("sans-serif".into()),
            ]),
        )]);
        assert_eq!(
            print_value(&v),
            "{fontFamily: {\"0\": \"Arial\", \"1\": \"sans-serif\"}}"
        );
        assert_eq!(print_value(&EvalValue::Arr(vec![])), "{}");
        let nested = EvalValue::Arr(vec![EvalValue::Arr(vec![EvalValue::Num(1.0)])]);
        assert_eq!(print_value(&nested), "{\"0\": {\"0\": 1}}");
    }

    #[test]
    fn edits_apply_in_order_with_insertions() {
        let src = "abcdef";
        let edits = vec![
            Edit::insert(0, "X".into()),
            Edit::replace(Span::new(2, 4), "Y".into()),
        ];
        assert_eq!(apply_edits(src, &edits), "XabYef");
    }

    #[test]
    fn render_span_applies_nested_edits() {
        let src = "foo(bar(baz))";
        let edits = vec![Edit::replace(Span::new(4, 12), "Q".into())];
        assert_eq!(render_span(src, Span::new(0, 13), &edits), "foo(Q)");
    }

    #[test]
    fn canonical_json_preserves_key_order() {
        let v = obj(&[
            ("z", EvalValue::Num(1.0)),
            ("a", EvalValue::Num(f64::INFINITY)),
        ]);
        assert_eq!(
            to_canonical_json(&v),
            "{\"z\":1,\"a\":{\"__jsnum\":\"Infinity\"}}"
        );
    }
}
