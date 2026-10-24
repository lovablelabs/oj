//! `transform`: stdin source → stdout compiled code (+ optional rules JSON),
//! or one batch-shaped result line with `--emit json`.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::api::{FileContext, MapMode, SourceCompileResult, transform_source_in_with_map};
use crate::errors::StylexError;
use crate::module_resolution::StdFs;
use crate::options::{CompilerOptions, ResolvedOptions};
use oxc_allocator::Allocator;
use serde_json::json;

use crate::cli::classify::classify;
use crate::cli::wire::{OkLine, json_str, protocol_error_line};

pub struct Inputs {
    pub filename: PathBuf,
    pub cwd: PathBuf,
    pub options: ResolvedOptions,
    pub source: String,
}

pub fn read_inputs(
    filename: Option<&str>,
    cwd: Option<&str>,
    options_json: Option<&str>,
) -> Result<Inputs, StylexError> {
    let raw = match options_json {
        None => serde_json::Value::Null,
        Some(text) => serde_json::from_str(text).map_err(|err| {
            StylexError::parse_error(&format!("--options is not valid JSON: {err}"))
        })?,
    };
    let options = CompilerOptions::from_json(&raw)?.resolve()?;
    let cwd = cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
    let filename = filename
        .map(PathBuf::from)
        .unwrap_or_else(|| cwd.join("input.tsx"));
    let source = match std::io::read_to_string(std::io::stdin().lock()) {
        Ok(source) => source,
        Err(err) => {
            eprintln!("{}", json!({ "error": "IO", "detail": err.to_string() }));
            std::process::exit(1);
        }
    };
    Ok(Inputs {
        filename,
        cwd,
        options,
        source,
    })
}

pub fn compile(inputs: &Inputs) -> Result<Option<SourceCompileResult>, StylexError> {
    compile_with_map(inputs, MapMode::Off)
}

fn compile_with_map(
    inputs: &Inputs,
    map_mode: MapMode,
) -> Result<Option<SourceCompileResult>, StylexError> {
    let ctx = FileContext {
        filename: Path::new(&inputs.filename),
        source_text: &inputs.source,
        cwd: Path::new(&inputs.cwd),
    };
    let allocator = Allocator::default();
    transform_source_in_with_map(&allocator, &ctx, &inputs.options, &StdFs, map_mode)
}

pub fn exit_with(error: &StylexError) -> ! {
    eprintln!(
        "{}",
        json!({
            "error": classify(error),
            "message": error.message,
        })
    );
    std::process::exit(1);
}

pub fn run(
    filename: Option<&str>,
    cwd: Option<&str>,
    options_json: Option<&str>,
    rules_out: Option<&Path>,
    emit: Option<&str>,
) {
    let json_line = match emit {
        None | Some("code") => false,
        Some("json") => true,
        Some(other) => {
            eprintln!(
                "{}",
                json!({ "error": "BAD_EMIT", "detail": format!("--emit must be code|json, got {other}") })
            );
            std::process::exit(64);
        }
    };
    if json_line {
        run_json(filename, cwd, options_json, rules_out);
        return;
    }
    let inputs = match read_inputs(filename, cwd, options_json) {
        Ok(inputs) => inputs,
        Err(error) => exit_with(&error),
    };
    let result = match compile(&inputs) {
        Ok(result) => result,
        Err(error) => exit_with(&error),
    };
    let (code, rules) = match &result {
        Some(r) => (
            r.code.as_str(),
            r.rules
                .iter()
                .map(|rule| rule.to_metadata_tuple())
                .collect(),
        ),
        None => (inputs.source.as_str(), Vec::new()),
    };
    print!("{code}");
    write_rules_out(rules_out, rules);
    std::io::stdout().flush().ok();
}

// The result line is the batch contract keyed by the filename, plus a `map`
// (mappings only) so a dev-server caller never needs the temp file.
fn run_json(
    filename: Option<&str>,
    cwd: Option<&str>,
    options_json: Option<&str>,
    rules_out: Option<&Path>,
) {
    let inputs = match read_inputs(filename, cwd, options_json) {
        Ok(inputs) => inputs,
        Err(error) => exit_json_error("?", &error),
    };
    let id = json_str(&inputs.filename.to_string_lossy());
    let result = match compile_with_map(&inputs, MapMode::Mappings) {
        Ok(result) => result,
        Err(error) => exit_json_error(&id, &error),
    };
    let (code, rules, map) = match &result {
        Some(r) => (r.code.as_str(), r.rules.as_slice(), r.map.as_deref()),
        None => (inputs.source.as_str(), &[][..], None),
    };
    let line = OkLine::new(&id, code.len() + code.len() / 4)
        .str("code", code)
        .rules(rules)
        .raw("map", map.unwrap_or("null"))
        .finish();
    let mut out = std::io::stdout().lock();
    out.write_all(line.as_bytes()).expect("stdout write");
    out.write_all(b"\n").expect("stdout write");
    out.flush().ok();
    write_rules_out(
        rules_out,
        rules.iter().map(|rule| rule.to_metadata_tuple()).collect(),
    );
}

fn exit_json_error(id_json: &str, error: &StylexError) -> ! {
    println!(
        "{}",
        protocol_error_line(id_json, classify(error), &error.message)
    );
    std::process::exit(1);
}

fn write_rules_out(rules_out: Option<&Path>, rules: Vec<serde_json::Value>) {
    if let Some(path) = rules_out {
        let rules = serde_json::Value::Array(rules);
        if let Err(err) = std::fs::write(path, format!("{rules}\n")) {
            eprintln!("{}", json!({ "error": "IO", "detail": err.to_string() }));
            std::process::exit(1);
        }
    }
}
