//! `oj stylex` is the `fru` command set; these pin that the subcommand reaches
//! it before oj's runtime boots, one-shot and resident.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::Value;

const SOURCE: &str = "import * as stylex from '@stylexjs/stylex';\n\
                      export const styles = stylex.create({ root: { color: 'red' } });\n";

#[test]
fn transform_compiles_stdin() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_oj"))
        .args(["stylex", "transform", "--filename", "/app/src/a.ts"])
        .args(["--cwd", "/app", "--emit", "json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(SOURCE.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let line: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(line["ok"], true);
    assert!(line["code"].as_str().unwrap().contains("x1e2nbdu"));
    assert_eq!(line["rules"][0][1]["ltr"], ".x1e2nbdu{color:red}");
}

#[test]
fn serve_answers_over_stdio() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_oj"))
        .args(["stylex", "serve", "--stdio", "--threads", "2"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut ask = |request: Value| -> Value {
        writeln!(stdin, "{request}").unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };

    let hello = ask(serde_json::json!({ "id": 1, "op": "hello" }));
    assert_eq!(hello["ok"], true);
    let transform = ask(serde_json::json!({
        "id": 2, "op": "transform", "root": "/app", "filename": "src/a.ts",
        "source": SOURCE, "key": "/app/src/a.ts", "wantCode": false,
    }));
    assert_eq!(transform["ok"], true);
    let sheet = ask(serde_json::json!({ "id": 3, "op": "sheet", "config": {} }));
    assert!(sheet["css"]
        .as_str()
        .unwrap()
        .contains(".x1e2nbdu{color:red}"));

    drop(stdin);
    assert!(child.wait().unwrap().success());
}
