// parity: docs/design-verification.md Layer-1b — conformance normalize-js.mjs
// shells to `reprint` / `parse-check`; flag names and JSON shape are that contract.

use oxc_allocator::Allocator;
use oxc_codegen::Codegen;
use oxc_parser::Parser;
use oxc_span::SourceType;
use serde_json::json;

#[derive(Clone, Copy)]
pub enum Mode {
    Reprint,
    ParseCheck,
}

pub fn run(mode: Mode, filename: Option<&str>) {
    let source = read_stdin();
    let source_type = source_type_for(filename);
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, &source, source_type).parse();

    if ret.fatal_error || ret.diagnostics.has_errors() {
        let diagnostics: Vec<serde_json::Value> = ret
            .diagnostics
            .errors()
            .map(|diagnostic| {
                let span = diagnostic
                    .labels
                    .iter()
                    .find(|label| label.primary())
                    .or_else(|| diagnostic.labels.first());
                json!({
                    "message": diagnostic.message.as_ref(),
                    "start": span.map(|s| s.offset()),
                    "end": span.map(|s| s.offset() + s.len()),
                })
            })
            .collect();
        eprintln!(
            "{}",
            json!({ "error": "PARSE", "diagnostics": diagnostics })
        );
        std::process::exit(1);
    }

    if matches!(mode, Mode::Reprint) {
        // Codegen::new() = CodegenOptions::default(): double quotes, minify off, comments kept
        // (normal+jsdoc+annotation+legal), tab indent ×1, initial indent 0, no sourcemap.
        let printed = Codegen::new().build(&ret.program).code;
        print!("{printed}");
    }
}

fn source_type_for(filename: Option<&str>) -> SourceType {
    filename
        .and_then(|name| SourceType::from_path(name).ok())
        .unwrap_or_else(SourceType::tsx)
}

fn read_stdin() -> String {
    match std::io::read_to_string(std::io::stdin().lock()) {
        Ok(source) => source,
        Err(err) => {
            eprintln!("{}", json!({ "error": "IO", "detail": err.to_string() }));
            std::process::exit(1);
        }
    }
}
