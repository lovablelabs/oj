//! `extract-objects`: canonical JSON of compiled create() outputs (NDJSON,
//! one line per call) — the shadow gate's comparison surface.

use crate::eval::value::EvalValue;
use crate::transform::js_out::to_canonical_json;

use crate::cli::transform::{compile, exit_with, read_inputs};

pub fn run(filename: Option<&str>, cwd: Option<&str>, options_json: Option<&str>) {
    let inputs = match read_inputs(filename, cwd, options_json) {
        Ok(inputs) => inputs,
        Err(error) => exit_with(&error),
    };
    let result = match compile(&inputs) {
        Ok(result) => result,
        Err(error) => exit_with(&error),
    };
    let Some(result) = result else {
        return;
    };
    for (var_name, compiled) in &result.create_objects {
        let name = match var_name {
            Some(name) => serde_json::json!(name).to_string(),
            None => "null".to_string(),
        };
        let namespaces = to_canonical_json(&EvalValue::Obj(compiled.clone()));
        println!("{{\"varName\":{name},\"namespaces\":{namespaces}}}");
    }
}
