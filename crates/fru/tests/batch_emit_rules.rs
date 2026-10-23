//! `--emit rules` is the full result line with the `code` key removed (and no
//! framed body); error lines and ordering are untouched.

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::Value;

fn batch(args: &[&str], input: &str) -> Vec<u8> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fru"))
        .args(["batch", "--format"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn fru");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write jobs");
    child.wait_with_output().expect("wait").stdout
}

fn lines(buf: &[u8]) -> Vec<String> {
    String::from_utf8(buf.to_vec())
        .expect("utf8")
        .lines()
        .map(str::to_string)
        .collect()
}

fn without_code(line: &str) -> String {
    let mut value: Value = serde_json::from_str(line).expect("json line");
    value.as_object_mut().expect("object").remove("code");
    value.to_string()
}

fn job(id: &str, source: &str) -> String {
    let root = std::env::temp_dir().to_string_lossy().replace('\\', "/");
    format!(
        r#"{{"id":"{id}","root":"{root}","filename":"src/{id}.ts","source":{},"options":{{}}}}"#,
        serde_json::to_string(source).expect("json string")
    )
}

// defineConsts hashes the module path, so it needs a resolution root.
fn consts_job(id: &str, source: &str) -> String {
    let root = std::env::temp_dir().to_string_lossy().replace('\\', "/");
    format!(
        r#"{{"id":"{id}","root":"{root}","filename":"src/{id}.stylex.ts","source":{},"options":{{"unstable_moduleResolution":{{"type":"commonJS","rootDir":"{root}"}}}}}}"#,
        serde_json::to_string(source).expect("json string")
    )
}

fn jobs() -> String {
    [
        job(
            "styled",
            "import * as stylex from '@stylexjs/stylex';\nexport const s = stylex.create({ box: { color: 'red', ':hover': { content: '\"quoted\"' } } });\n",
        ),
        job("plain", "export const answer = 42;\n"),
        job(
            "broken",
            "import * as stylex from '@stylexjs/stylex';\nexport const s = stylex.create({ a: { color: 1n } });\n",
        ),
        "{ not json".to_string(),
        consts_job(
            "consts",
            "import * as stylex from '@stylexjs/stylex';\nexport const c = stylex.defineConsts({ gap: '4px' });\n",
        ),
    ]
    .join("\n")
}

#[test]
fn rules_only_lines_are_the_full_lines_minus_code() {
    let input = jobs();
    let full = lines(&batch(&["ndjson", "--threads", "1"], &input));
    let rules = lines(&batch(
        &["ndjson", "--threads", "1", "--emit", "rules"],
        &input,
    ));
    assert_eq!(full.len(), 5);
    assert_eq!(rules.len(), 5);
    for (full, rules) in full.iter().zip(&rules) {
        assert_eq!(rules, &without_code(full));
        assert!(!rules.contains("\"code\""), "{rules}");
    }
    assert!(
        rules[0].contains(r#""ok":true,"rules":[["x"#),
        "{}",
        rules[0]
    );
    assert!(
        rules[1].ends_with(r#""ok":true,"rules":[]}"#),
        "{}",
        rules[1]
    );
    assert!(rules[2].contains(r#""ok":false"#), "{}", rules[2]);
    assert!(
        rules[3].starts_with(r#"{"id":"?","ok":false"#),
        "{}",
        rules[3]
    );
    assert!(rules[4].contains("constKey"), "{}", rules[4]);
}

#[test]
fn rules_only_is_thread_count_and_backend_independent_and_has_no_framed_body() {
    let input = jobs();
    let t1 = batch(&["ndjson", "--threads", "1", "--emit", "rules"], &input);
    assert_eq!(
        t1,
        batch(&["ndjson", "--threads", "3", "--emit", "rules"], &input)
    );
    assert_eq!(
        t1,
        batch(
            &[
                "ndjson",
                "--threads",
                "3",
                "--emit",
                "rules",
                "--backend",
                "ast"
            ],
            &input
        )
    );
    assert_eq!(
        t1,
        batch(&["framed", "--threads", "3", "--emit", "rules"], &input)
    );
}

#[test]
fn unknown_emit_is_a_usage_error() {
    let out = Command::new(env!("CARGO_BIN_EXE_fru"))
        .args(["batch", "--format", "ndjson", "--emit", "code"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("run fru");
    assert_eq!(out.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&out.stderr).contains("BAD_EMIT"));
}
