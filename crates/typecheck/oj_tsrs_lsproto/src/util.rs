use std::cmp::Ordering;

use crate::{Diagnostic, IntegerOrString, Position, Range, StringOrMarkupContent};

// util.go:11
// Implements a cmp.Compare like function for two Position
// ComparePositions(pos, other) == cmp.Compare(pos, other)
pub fn compare_positions(pos: Position, other: Position) -> Ordering {
    let line_comp = pos.line.cmp(&other.line);
    if !line_comp.is_eq() {
        return line_comp;
    }
    pos.character.cmp(&other.character)
}

// util.go:22
// Implements a cmp.Compare like function for two Range
// CompareRanges(lsRange, other) == cmp.Compare(lsRange, other)
//
//	Range.Start is compared before Range.End
pub fn compare_ranges(ls_range: Range, other: Range) -> Ordering {
    let start_comp = compare_positions(ls_range.start, other.start);
    if !start_comp.is_eq() {
        return start_comp;
    }
    compare_positions(ls_range.end, other.end)
}

impl StringOrMarkupContent {
    // util.go:31
    // AsString returns the plain text of a StringOrMarkupContent, reading the
    // MarkupContent value when the message is not a plain string.
    pub fn as_string(&self) -> String {
        if let Some(s) = &self.string {
            return s.clone();
        }
        if let Some(markup_content) = &self.markup_content {
            return markup_content.value.clone();
        }
        String::new()
    }
}

impl IntegerOrString {
    // util.go:41
    fn as_string(&self) -> String {
        if let Some(s) = &self.string {
            s.clone()
        } else if let Some(i) = self.integer {
            i.to_string()
        } else {
            "-1".to_string()
        }
    }
}

// util.go:53
fn diagnostic_exists_in_slice(elem: &Diagnostic, diags: &[Diagnostic]) -> bool {
    diags.iter().any(|diag| diagnostics_equal(elem, diag))
}

// util.go:62
fn diagnostics_equal(diag1: &Diagnostic, diag2: &Diagnostic) -> bool {
    diagnostic_codes_equal(diag1.code.as_ref(), diag2.code.as_ref())
        && diagnostic_messages_equal(&diag1.message, &diag2.message)
        && compare_ranges(diag1.range, diag2.range).is_eq()
}

// util.go:69
fn diagnostic_codes_equal(
    code1: Option<&IntegerOrString>,
    code2: Option<&IntegerOrString>,
) -> bool {
    // Go dereferences both codes.
    let (code1, code2) = (code1.unwrap(), code2.unwrap());
    if let (Some(s1), Some(s2)) = (&code1.string, &code2.string) {
        return s1 == s2;
    }
    if let (Some(i1), Some(i2)) = (code1.integer, code2.integer) {
        return i1 == i2;
    }
    false
}

// util.go:79
fn diagnostic_messages_equal(
    message1: &StringOrMarkupContent,
    message2: &StringOrMarkupContent,
) -> bool {
    if let (Some(s1), Some(s2)) = (&message1.string, &message2.string) {
        return s1 == s2;
    }
    if let (Some(m1), Some(m2)) = (&message1.markup_content, &message2.markup_content) {
        return m1.kind == m2.kind && m1.value == m2.value;
    }
    false
}

// util.go:89
pub fn compare_diagnostics<'a>(
    list1: &'a [Diagnostic],
    list2: &'a [Diagnostic],
) -> (Vec<&'a Diagnostic>, Vec<&'a Diagnostic>) {
    let mut missing_from_list1 = Vec::new();
    let mut missing_from_list2 = Vec::new();
    missing_from_list2.extend(list1.iter().filter(|elem| !diagnostic_exists_in_slice(elem, list2)));
    missing_from_list1.extend(list2.iter().filter(|elem| !diagnostic_exists_in_slice(elem, list1)));
    (missing_from_list1, missing_from_list2)
}

impl Diagnostic {
    // util.go:105
    pub fn as_string(&self) -> String {
        format!(
            "{} ({}:{}-{}:{}): {}",
            self.code.as_ref().unwrap().as_string(),
            self.range.start.line,
            self.range.start.character,
            self.range.end.line,
            self.range.end.character,
            self.message.as_string()
        )
    }

    // util.go:109
    pub fn code_as_string(&self) -> String {
        format!("Code({})", self.code.as_ref().unwrap().as_string())
    }
}
