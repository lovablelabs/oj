//! `serve --stdio`: framing, id correlation, registry ops and shutdown order.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Value, json};

const SOURCE: &str = r#"import * as stylex from '@stylexjs/stylex';
export const styles = stylex.create({ box: { color: 'blue', ':hover': { color: 'red' } } });
"#;

const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_fru")
}

struct Daemon {
    child: Child,
    stdin: Option<ChildStdin>,
    replies: mpsc::Receiver<String>,
}

impl Daemon {
    fn spawn(threads: &str) -> Self {
        let mut child = Command::new(bin())
            .args(["serve", "--stdio", "--threads", threads])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fru serve");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout");
        let (tx, replies) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx.send(line.expect("utf8 reply")).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            replies,
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(bytes).expect("write request");
        stdin.flush().expect("flush request");
    }

    fn reply_line(&self) -> String {
        self.replies
            .recv_timeout(REPLY_TIMEOUT)
            .expect("a reply before the timeout")
    }

    fn reply(&self) -> Value {
        serde_json::from_str(&self.reply_line()).expect("reply is JSON")
    }

    fn ask(&mut self, request: &Value) -> Value {
        self.send(format!("{request}\n").as_bytes());
        self.reply()
    }

    /// Closes stdin and returns every reply not yet taken, with the exit code.
    fn finish(mut self) -> (Vec<String>, Option<i32>) {
        drop(self.stdin.take());
        let rest = self.replies.iter().collect();
        (rest, self.child.wait().expect("wait").code())
    }
}

fn transform(id: Value) -> Value {
    json!({
        "id": id,
        "root": "/app",
        "filename": "src/box.ts",
        "source": SOURCE,
        "options": {},
    })
}

fn batch(input: &str) -> String {
    let mut child = Command::new(bin())
        .args(["batch", "--format", "ndjson", "--threads", "1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn fru batch");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write jobs");
    let out = child.wait_with_output().expect("wait");
    String::from_utf8(out.stdout).expect("utf8 results")
}

#[test]
fn a_transform_reply_is_the_batch_result_line() {
    let job = format!("{}\n", transform(json!("j")));
    let mut daemon = Daemon::spawn("1");
    daemon.send(job.as_bytes());
    let reply = daemon.reply_line();
    assert!(reply.contains(r#""ok":true"#), "transform failed: {reply}");
    assert_eq!(format!("{reply}\n"), batch(&job));
    assert_eq!(daemon.finish(), (Vec::new(), Some(0)));
}

#[test]
fn ids_come_back_as_json_would_reserialize_them() {
    let mut daemon = Daemon::spawn("1");
    for (sent, echoed) in [
        ("7", "7"),
        ("-12", "-12"),
        (r#""plain""#, r#""plain""#),
        (r#""a\u0041""#, r#""aA""#),
        (r#"{"a": [1, "b"]}"#, r#"{"a":[1,"b"]}"#),
        ("null", "null"),
    ] {
        daemon.send(format!("{{\"id\":{sent},\"op\":\"hello\"}}\n").as_bytes());
        let reply = daemon.reply_line();
        assert!(
            reply.starts_with(&format!("{{\"id\":{echoed},\"ok\":true,\"fingerprint\":")),
            "id {sent}: {reply}"
        );
    }
    let hello = daemon.ask(&json!({ "op": "hello" }));
    assert_eq!(hello["id"], Value::Null);
    assert_eq!(hello["threads"], 1);
    assert_eq!(hello["pid"], daemon.child.id());
    daemon.finish();
}

#[test]
fn the_registry_follows_keyed_transforms() {
    let mut daemon = Daemon::spawn("1");
    let mut keyed = transform(json!(1));
    keyed["key"] = json!("/app/src/box.ts");
    let first = daemon.ask(&keyed);
    assert_eq!(first["ok"], true, "{first}");
    assert_eq!(first["generation"], 1);
    assert_eq!(first["rulesChanged"], true);
    assert!(first["code"].as_str().is_some_and(|code| !code.is_empty()));
    let rules = first["rules"].clone();
    assert_eq!(rules.as_array().map(Vec::len), Some(2));

    keyed["wantCode"] = json!(false);
    let again = daemon.ask(&keyed);
    assert_eq!(again["rulesChanged"], false);
    assert_eq!(again["generation"], 1);
    assert!(again.get("code").is_none(), "{again}");
    assert_eq!(again["rules"], rules);

    let unkeyed = daemon.ask(&transform(json!(2)));
    assert!(unkeyed.get("generation").is_none(), "{unkeyed}");

    let sheet = daemon.ask(&json!({ "id": 3, "op": "sheet", "config": {} }));
    assert_eq!(sheet["files"], 1);
    let css = sheet["css"].as_str().expect("css").to_owned();
    for rule in rules.as_array().expect("rules") {
        let class = rule[0].as_str().expect("class name");
        assert!(css.contains(class), "{class} missing from {css}");
    }

    let removed = daemon.ask(&json!({ "id": 4, "op": "removeFile", "key": "/app/src/box.ts" }));
    assert_eq!(removed["removed"], true);
    let empty = daemon.ask(&json!({ "id": 5, "op": "sheet" }));
    assert_eq!(empty["files"], 0);
    assert_eq!(empty["css"], "");

    let restored = daemon.ask(&json!({
        "id": 6,
        "op": "setFileRules",
        "key": "/elsewhere.ts",
        "rules": rules,
    }));
    assert_eq!(restored["rulesChanged"], true);
    let sheet = daemon.ask(&json!({ "id": 7, "op": "sheet" }));
    assert_eq!(sheet["css"], css.as_str());

    let failed = daemon.ask(&json!({
        "id": 8,
        "root": "/app",
        "filename": "src/box.ts",
        "source": "import * as stylex from '@stylexjs/stylex';\nstylex.create(1);\n",
        "options": {},
        "key": "/elsewhere.ts",
    }));
    assert_eq!(failed["ok"], false, "{failed}");
    let sheet = daemon.ask(&json!({ "id": 9, "op": "sheet" }));
    assert_eq!(sheet["css"], css.as_str());
    assert_eq!(daemon.finish(), (Vec::new(), Some(0)));
}

#[test]
fn bad_lines_get_error_replies_and_the_daemon_stays_up() {
    let mut daemon = Daemon::spawn("1");
    daemon.send(b"\n   \r\n\t\nnope\n");
    let malformed = daemon.reply();
    assert_eq!(malformed["id"], Value::Null);
    assert_eq!(malformed["ok"], false);
    assert_eq!(malformed["errorClass"], "unknown");

    daemon.send(b"{\"id\":1,\"op\":\"wat\"}\r\n");
    let bad_op = daemon.reply();
    assert_eq!(bad_op["id"], 1);
    assert_eq!(bad_op["errorClass"], "BAD_OP");
    assert_eq!(bad_op["message"], "unknown op wat");

    daemon.send(b"{\"id\":2,\"op\":\"hello\",\"root\":\"\xff\"}\n");
    let not_utf8 = daemon.reply();
    assert_eq!(not_utf8["ok"], false, "{not_utf8}");

    let missing = daemon.ask(&json!({ "id": 3, "op": "transform", "root": "/app" }));
    assert_eq!(missing["id"], 3);
    assert_eq!(missing["ok"], false);

    let bad_rule = daemon.ask(&json!({ "id": 4, "op": "setFileRules", "key": "k", "rules": [1] }));
    assert_eq!(bad_rule["errorClass"], "BAD_RULE");

    // The last request has no newline: end of input still delivers it.
    daemon.send(b"{\"id\":5,\"op\":\"hello\"}");
    let (rest, code) = daemon.finish();
    assert_eq!(rest.len(), 1, "{rest:?}");
    assert!(rest[0].starts_with("{\"id\":5,\"ok\":true"), "{rest:?}");
    assert_eq!(code, Some(0));
}

#[test]
fn a_request_larger_than_the_read_buffer_round_trips() {
    let padding = format!("/* {} */\n", "padding ".repeat(100_000));
    let source = format!("{padding}{SOURCE}");
    let mut big = transform(json!("big"));
    big["source"] = json!(source);
    let mut small = transform(json!("small"));
    small["wantCode"] = json!(false);
    let mut daemon = Daemon::spawn("2");
    let reply = daemon.ask(&big);
    assert_eq!(reply["ok"], true);
    assert!(
        reply["code"]
            .as_str()
            .is_some_and(|code| code.starts_with(&padding))
    );
    let expected_rules = daemon.ask(&small)["rules"].clone();
    assert_eq!(reply["rules"], expected_rules);
    daemon.finish();
}

// Every request of a burst is answered exactly once at any worker count, and
// the shutdown reply is the last line the daemon writes.
#[test]
fn a_burst_is_answered_once_each_and_shutdown_comes_last() {
    const REQUESTS: usize = 600;
    for threads in ["1", "4"] {
        let mut input = String::new();
        for i in 0..REQUESTS {
            let mut request = transform(json!(i));
            if i.is_multiple_of(3) {
                request["key"] = json!(format!("/app/src/file-{}.ts", i % 7));
            }
            if i.is_multiple_of(5) {
                request["wantCode"] = json!(false);
            }
            input.push_str(&request.to_string());
            input.push('\n');
        }
        input.push_str("{\"id\":\"bye\",\"op\":\"shutdown\"}\n");
        let mut daemon = Daemon::spawn(threads);
        let mut stdin = daemon.stdin.take().expect("stdin");
        let writer = std::thread::spawn(move || {
            // The daemon may exit before the pipe is drained; that is its right.
            let _ = stdin.write_all(input.as_bytes());
            stdin
        });
        let mut seen = vec![false; REQUESTS];
        for _ in 0..REQUESTS {
            let reply = daemon.reply();
            assert_eq!(reply["ok"], true, "{reply}");
            let id = reply["id"].as_u64().expect("numeric id") as usize;
            assert!(!std::mem::replace(&mut seen[id], true), "id {id} twice");
            assert_eq!(reply.get("code").is_none(), id.is_multiple_of(5), "id {id}");
            assert_eq!(
                reply.get("generation").is_some(),
                id.is_multiple_of(3),
                "id {id}"
            );
        }
        assert_eq!(daemon.reply_line(), r#"{"id":"bye","ok":true}"#);
        // stdin is still open here: the exit is the shutdown's, not end of input.
        let code = daemon.child.wait().expect("wait").code();
        assert_eq!(code, Some(0), "threads {threads}");
        assert!(daemon.replies.iter().next().is_none());
        drop(writer.join().expect("writer"));
    }
}

// A client that writes one request and waits for its reply before the next
// must never be left waiting on a line the daemon is holding back.
#[test]
fn request_reply_clients_are_answered_without_further_input() {
    let mut daemon = Daemon::spawn("4");
    for i in 0..50 {
        let reply = daemon.ask(&transform(json!(i)));
        assert_eq!(reply["id"], i);
        assert_eq!(reply["ok"], true);
    }
    // Two requests in one write are both answered, in either order.
    let pair = format!("{}\n{}\n", transform(json!("a")), transform(json!("b")));
    daemon.send(pair.as_bytes());
    let mut ids = [daemon.reply()["id"].clone(), daemon.reply()["id"].clone()];
    ids.sort_by_key(Value::to_string);
    assert_eq!(ids, [json!("a"), json!("b")]);
    assert_eq!(daemon.finish(), (Vec::new(), Some(0)));
}
