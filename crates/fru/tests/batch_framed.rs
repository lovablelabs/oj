//! `--format framed` carries exactly the NDJSON results, with the code as raw
//! bytes behind the header line instead of a JSON string.

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::Value;

fn batch(format: &str, threads: &str, timings: bool, input: &str) -> Vec<u8> {
    let mut args = vec!["batch", "--format", format, "--threads", threads];
    if timings {
        args.push("--timings");
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_fru"))
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

fn decode_ndjson(buf: &[u8]) -> Vec<Value> {
    buf.split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).expect("ndjson line"))
        .collect()
}

fn decode_framed(mut buf: &[u8]) -> Vec<Value> {
    let mut out = Vec::new();
    while !buf.is_empty() {
        let nl = buf.iter().position(|&b| b == b'\n').expect("header line");
        let mut head: Value = serde_json::from_slice(&buf[..nl]).expect("header json");
        buf = &buf[nl + 1..];
        if let Some(len) = head.get("codeLen").and_then(Value::as_u64) {
            let len = usize::try_from(len).expect("codeLen fits");
            let code = std::str::from_utf8(&buf[..len]).expect("code is utf-8");
            assert_eq!(buf[len], b'\n', "frame terminator after the code bytes");
            let obj = head.as_object_mut().expect("header object");
            obj.remove("codeLen");
            obj.insert("code".into(), Value::String(code.to_string()));
            buf = &buf[len + 1..];
        }
        out.push(head);
    }
    out
}

fn job(id: &str, source: &str) -> String {
    let root = std::env::temp_dir().to_string_lossy().replace('\\', "/");
    format!(
        r#"{{"id":"{id}","root":"{root}","filename":"src/{id}.ts","source":{},"options":{{}}}}"#,
        serde_json::to_string(source).expect("json string")
    )
}

fn jobs() -> String {
    [
        job(
            "styled",
            "import * as stylex from '@stylexjs/stylex';\nexport const label = 'ünïcödé';\nexport const s = stylex.create({ box: { color: 'red', ':hover': { content: '\"quoted\"' } } });\n",
        ),
        job("plain", "export const answer = 42; // no stylex here → passes through\n"),
        job(
            "broken",
            "import * as stylex from '@stylexjs/stylex';\nexport const s = stylex.create({ a: { color: 1n } });\n",
        ),
        "{ not json".to_string(),
        job("empty", ""),
    ]
    .join("\n")
}

#[test]
fn framed_decodes_to_the_ndjson_results() {
    let input = jobs();
    let ndjson = decode_ndjson(&batch("ndjson", "1", false, &input));
    let framed = decode_framed(&batch("framed", "1", false, &input));
    assert_eq!(ndjson.len(), 5);
    assert_eq!(ndjson, framed);
    assert_eq!(framed[0]["ok"], Value::Bool(true));
    assert!(framed[0]["code"].as_str().unwrap().contains("ünïcödé"));
    assert_eq!(
        framed[1]["code"].as_str().unwrap(),
        "export const answer = 42; // no stylex here → passes through\n"
    );
    assert_eq!(framed[2]["ok"], Value::Bool(false));
    assert_eq!(framed[3]["id"], Value::String("?".into()));
    assert_eq!(framed[4]["code"], Value::String(String::new()));
}

#[test]
fn framed_is_thread_count_independent_and_keeps_wall_ns_on_the_header() {
    let input = jobs();
    assert_eq!(
        batch("framed", "1", false, &input),
        batch("framed", "3", false, &input)
    );
    let timed = decode_framed(&batch("framed", "3", true, &input));
    for result in &timed {
        assert!(result["wall_ns"].is_u64(), "{result}");
    }
    let mut ndjson = decode_ndjson(&batch("ndjson", "3", true, &input));
    let mut framed = timed;
    for result in ndjson.iter_mut().chain(framed.iter_mut()) {
        result.as_object_mut().unwrap().remove("wall_ns");
    }
    assert_eq!(ndjson, framed);
}

#[test]
fn unknown_format_is_a_usage_error() {
    let out = Command::new(env!("CARGO_BIN_EXE_fru"))
        .args(["batch", "--format", "xml"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("run fru");
    assert_eq!(out.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&out.stderr).contains("BAD_FORMAT"));
}
