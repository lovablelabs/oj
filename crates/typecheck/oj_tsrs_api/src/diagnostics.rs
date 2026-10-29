// proto.go DiagnosticResponse / NewDiagnosticResponse / ToDiagnostic. Positions on the wire are UTF-16
// offsets (pos/end) plus ECMA line / UTF-16 character pairs, converted from tsrs' UTF-8 offsets.

use tsrs_ast::{Diagnostic, SourceFile};
use tsrs_core::json::Value;
use tsrs_core::{P, TextRange};
use tsrs_diagnostics::Category;

use crate::handler::{ApiError, ApiResult};
use crate::wire::{Obj, Params, b, n, s};

fn category_from(v: i64) -> Option<Category> {
    Some(match v {
        0 => Category::Warning,
        1 => Category::Error,
        2 => Category::Suggestion,
        3 => Category::Message,
        _ => return None,
    })
}

fn category_number(c: Category) -> f64 {
    match c {
        Category::Warning => 0.0,
        Category::Error => 1.0,
        Category::Suggestion => 2.0,
        Category::Message => 3.0,
    }
}

fn source_lines(file: &SourceFile, first_line: usize, last_line: usize) -> Option<Value> {
    let line_map = file.ecma_line_map();
    if line_map.is_empty() {
        return None;
    }
    let lines: Vec<usize> = if last_line - first_line >= 4 {
        vec![first_line, first_line + 1, last_line - 1, last_line]
    } else {
        (first_line..=last_line).collect()
    };
    let text = file.text();
    let mut result = Vec::with_capacity(lines.len());
    result.extend(lines.into_iter().map(|line| {
        let start = line_map[line] as usize;
        let end =
            if (line + 1) < line_map.len() { line_map[line + 1] as usize } else { text.len() };
        Obj::new()
            .set("line", n(u32::try_from(line).expect("line number exceeds u32")))
            .set("text", s(&text[start..end]))
            .build()
    }));
    Some(Value::Array(result))
}

/// Go `NewDiagnosticResponse`.
pub(crate) fn diagnostic_response(d: &Diagnostic) -> Value {
    let file = d.file();
    let pos = d.pos();
    let end = d.end();
    let mut file_name = None;
    let mut positions = None;
    let mut o = Obj::new();
    if let Some(file) = file {
        let len = u32::try_from(file.text().len()).expect("source length exceeds u32");
        let pos = pos.unwrap_or(0).min(len);
        let end = end.unwrap_or(pos).min(len).max(pos);
        file_name = Some(s(file.file_name()));
        let map = file.get_position_map();
        let (start_line, start_char) =
            tsrs_scanner::get_ecma_line_and_utf16_character_of_position(&*file, pos);
        let (end_line, end_char) =
            tsrs_scanner::get_ecma_line_and_utf16_character_of_position(&*file, end);
        positions = Some((
            Obj::new()
                .set("line", n(u32::try_from(start_line).expect("line number exceeds u32")))
                .set("character", n(start_char))
                .build(),
            Obj::new()
                .set("line", n(u32::try_from(end_line).expect("line number exceeds u32")))
                .set("character", n(end_char))
                .build(),
            source_lines(&file, start_line, end_line),
        ));
        o = o.set("pos", n(map.utf8_to_utf16(pos))).set("end", n(map.utf8_to_utf16(end)));
    } else {
        o = o
            .set("pos", n(pos.map_or(-1, |value| i32::try_from(value).unwrap_or(i32::MAX))))
            .set("end", n(end.map_or(-1, |value| i32::try_from(value).unwrap_or(i32::MAX))));
    }
    o = o.set_opt("fileName", file_name);
    if let Some((start, end, lines)) = positions {
        o = o.set("startPosition", start).set("endPosition", end).set_opt("sourceLines", lines);
    }
    o = o.set("code", n(d.code())).set("category", Value::Number(category_number(d.category())));
    if !d.source().is_empty() {
        o = o.set("source", s(d.source()));
    }
    o = o.set("text", s(d.localize()));
    if d.reports_unnecessary() {
        o = o.set("reportsUnnecessary", b(true));
    }
    if d.reports_deprecated() {
        o = o.set("reportsDeprecated", b(true));
    }
    let chain = d.message_chain();
    if !chain.is_empty() {
        o = o.set(
            "messageChain",
            Value::Array(chain.iter().map(|c| diagnostic_response(c)).collect()),
        );
    }
    let related = d.related_information();
    if !related.is_empty() {
        o = o.set(
            "relatedInformation",
            Value::Array(related.iter().map(|r| diagnostic_response(r)).collect()),
        );
    }
    o.build()
}

/// Go `NewDiagnosticResponses` (nil for an empty list; callers decide between `[]` and omission).
pub fn diagnostic_responses(diags: &[P<Diagnostic>]) -> Value {
    Value::Array(diags.iter().map(|d| diagnostic_response(d)).collect())
}

/// Go `DiagnosticResponse.ToDiagnostic` (file-less, positions taken as given).
pub(crate) fn diagnostic_from_response(v: &Value) -> ApiResult<P<Diagnostic>> {
    let p = Params(v);
    p.object()?;
    let num = |key: &str| -> ApiResult<i64> {
        match p.get(key) {
            Value::Number(x) if x.fract() == 0.0 => Ok(*x as i64),
            Value::Null => Ok(0),
            _ => Err(ApiError::invalid_request(format!("diagnostic {key} must be an integer"))),
        }
    };
    let category = category_from(num("category")?)
        .ok_or_else(|| ApiError::invalid_request("invalid diagnostic category"))?;
    let chain = p
        .array("messageChain")?
        .iter()
        .map(diagnostic_from_response)
        .collect::<ApiResult<Vec<_>>>()?;
    let related = p
        .array("relatedInformation")?
        .iter()
        .map(diagnostic_from_response)
        .collect::<ApiResult<Vec<_>>>()?;
    let pos = num("pos")?;
    let end = num("end")?;
    let loc = match (u32::try_from(pos), u32::try_from(end)) {
        (Ok(pos), Ok(end)) => TextRange::new(pos, end),
        _ => TextRange::synthetic(),
    };
    Ok(tsrs_ast::new_diagnostic_from_text(
        None,
        loc,
        num("code")? as i32,
        category,
        p.str("text")?,
        &chain,
        &related,
        p.bool("reportsUnnecessary")?,
        p.bool("reportsDeprecated")?,
    ))
}
