//! `sourcePath` jobs must compile byte-identically to the same job inlined.

use std::io::Write;
use std::process::{Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_fru")
}

fn batch(input: &str) -> String {
    batch_at("1", input)
}

fn batch_at(threads: &str, input: &str) -> String {
    let mut child = Command::new(bin())
        .args(["batch", "--format", "ndjson", "--threads", threads])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn stylex-rs");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write jobs");
    let out = child.wait_with_output().expect("wait");
    String::from_utf8(out.stdout).expect("utf8 results")
}

struct Fixture {
    dir: std::path::PathBuf,
}

impl Fixture {
    fn new(name: &str, source: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("stylex-rs-sourcepath-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).expect("mkdir");
        std::fs::write(dir.join("package.json"), r#"{"name":"fixture"}"#).expect("package.json");
        std::fs::write(dir.join("src/input.ts"), source).expect("source");
        Self { dir }
    }
    fn root(&self) -> String {
        self.dir.to_string_lossy().replace('\\', "/")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

const SOURCE: &str = r#"import * as stylex from '@stylexjs/stylex';
export const styles = stylex.create({ box: { color: 'blue', ':hover': { color: 'red' } } });
"#;

#[test]
fn source_path_matches_inline_source() {
    let fixture = Fixture::new("identity", SOURCE);
    let root = fixture.root();
    let inline = format!(
        r#"{{"id":"j","root":"{root}","filename":"src/input.ts","source":{},"options":{{}}}}"#,
        serde_json::to_string(SOURCE).unwrap()
    );
    let by_path = format!(
        r#"{{"id":"j","root":"{root}","filename":"src/input.ts","sourcePath":"src/input.ts","options":{{}}}}"#
    );
    let from_inline = batch(&inline);
    assert!(
        from_inline.contains(r#""ok":true"#),
        "inline job failed: {from_inline}"
    );
    assert_eq!(from_inline, batch(&by_path));
}

// Enough jobs to span several reader chunks, with inline and unreadable jobs
// interleaved: the prefetched stream must keep input order at any thread count.
#[test]
fn mixed_prefetched_stream_matches_inline_at_every_thread_count() {
    let fixture = Fixture::new("mixed", SOURCE);
    let root = fixture.root();
    let source_json = serde_json::to_string(SOURCE).unwrap();
    let (mut by_path, mut inline) = (Vec::new(), Vec::new());
    for i in 0..300 {
        let (path_job, inline_job) = match i % 3 {
            0 => (
                format!(
                    r#"{{"id":"{i}","root":"{root}","filename":"src/input.ts","sourcePath":"src/input.ts","options":{{}}}}"#
                ),
                format!(
                    r#"{{"id":"{i}","root":"{root}","filename":"src/input.ts","source":{source_json},"options":{{}}}}"#
                ),
            ),
            1 => {
                let job = format!(
                    r#"{{"id":"{i}","root":"{root}","filename":"src/input.ts","source":{source_json},"options":{{}}}}"#
                );
                (job.clone(), job)
            }
            _ => {
                let job = format!(
                    r#"{{"id":"{i}","root":"{root}","filename":"src/input.ts","sourcePath":"src/missing-{i}.ts","options":{{}}}}"#
                );
                (job.clone(), job)
            }
        };
        by_path.push(path_job);
        inline.push(inline_job);
    }
    let expected = batch(&inline.join("\n"));
    assert_eq!(expected.lines().count(), 300);
    assert_eq!(
        expected
            .lines()
            .filter(|l| l.contains("could not read"))
            .count(),
        100
    );
    let by_path = by_path.join("\n");
    for threads in ["1", "3", "16"] {
        assert_eq!(batch_at(threads, &by_path), expected, "threads {threads}");
    }
}

#[test]
fn source_path_errors_are_protocol_errors() {
    let fixture = Fixture::new("errors", SOURCE);
    let root = fixture.root();
    let cases = [
        (
            format!(
                r#"{{"id":"missing","root":"{root}","filename":"src/input.ts","sourcePath":"src/nope.ts","options":{{}}}}"#
            ),
            "could not read",
        ),
        (
            format!(
                r#"{{"id":"neither","root":"{root}","filename":"src/input.ts","options":{{}}}}"#
            ),
            "neither source nor sourcePath",
        ),
        (
            format!(
                r#"{{"id":"both","root":"{root}","filename":"src/input.ts","source":"x","sourcePath":"src/input.ts","options":{{}}}}"#
            ),
            "both source and sourcePath",
        ),
    ];
    for (job, expected) in cases {
        let out = batch(&job);
        assert!(out.contains(r#""ok":false"#), "expected failure: {out}");
        assert!(out.contains(expected), "expected {expected:?} in {out}");
    }
}

#[test]
fn absolute_source_path_is_used_as_given() {
    let fixture = Fixture::new("absolute", SOURCE);
    let root = fixture.root();
    let absolute = format!("{root}/src/input.ts");
    let job = format!(
        r#"{{"id":"j","root":"{root}","filename":"src/input.ts","sourcePath":"{absolute}","options":{{}}}}"#
    );
    let out = batch(&job);
    assert!(out.contains(r#""ok":true"#), "absolute path failed: {out}");
}
