// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `oj lint`: the project's oxlint with oj as its type-aware backend, and
//! `oj headless`, the backend oxlint starts (tsgolint's headless protocol on
//! tsrs's port of its rules).
//!
//! oxlint itself is not installed for these tests. The backend is driven the
//! way oxlint drives it (a V2 payload on stdin, framed diagnostics on
//! stdout), and `oj lint` runs a stand-in for oxlint that reports how it was
//! started and then runs the backend it was pointed at.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A project whose `src/a.ts` leaves a promise floating on line 3, removed
/// on drop.
struct App(PathBuf);

impl App {
    fn new(name: &str) -> App {
        let dir = std::env::temp_dir().join(format!("oj-lint-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let app = App(dir.canonicalize().unwrap());
        app.write(
            "tsconfig.json",
            r#"{ "compilerOptions": { "strict": true, "noEmit": true, "target": "ES2022", "module": "ESNext", "moduleResolution": "bundler" }, "include": ["src"] }"#,
        );
        app.write(
            "src/a.ts",
            "async function load(): Promise<number> { return 1; }\nexport function run() {\n  load();\n}\n",
        );
        app.write(
            "src/clean.ts",
            "async function load(): Promise<number> { return 1; }\nexport async function run() {\n  await load();\n}\n",
        );
        app
    }

    fn write(&self, rel: &str, text: &str) {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// The V2 payload oxlint sends for `files` with `rules` on.
    fn payload(&self, files: &[&str], rules: &[&str]) -> String {
        let files: Vec<String> = files
            .iter()
            .map(|f| self.0.join(f).to_string_lossy().into_owned())
            .collect();
        let rules: Vec<serde_json::Value> = rules
            .iter()
            .map(|name| serde_json::json!({ "name": name }))
            .collect();
        serde_json::json!({ "version": 2, "configs": [{ "file_paths": files, "rules": rules }] })
            .to_string()
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `oj headless <args>` in `cwd` with `payload` on stdin: the exit status,
/// the frames it wrote as `(type, json)`, and stderr.
fn headless(
    cwd: &Path,
    args: &[&str],
    payload: &str,
) -> (i32, Vec<(u8, serde_json::Value)>, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_oj"))
        .arg("headless")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        frames(&out.stdout),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// tsgolint's framing: four bytes of little-endian JSON length, one byte of
/// message type (0 error, 1 diagnostic, 2 timing), then the JSON.
fn frames(mut bytes: &[u8]) -> Vec<(u8, serde_json::Value)> {
    let mut out = Vec::new();
    while bytes.len() >= 5 {
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let kind = bytes[4];
        let json = serde_json::from_slice(&bytes[5..5 + len])
            .unwrap_or_else(|e| panic!("frame is not JSON ({e}): {:?}", &bytes[5..5 + len]));
        out.push((kind, json));
        bytes = &bytes[5 + len..];
    }
    assert!(
        bytes.is_empty(),
        "{} bytes after the last frame",
        bytes.len()
    );
    out
}

const DIAGNOSTIC: u8 = 1;

// The backend as oxlint drives it: the rule that needs types reports the
// floating promise, where it is, and nothing for the file that awaits.
#[test]
fn the_backend_reports_a_type_aware_rule_over_the_headless_protocol() {
    let app = App::new("headless");
    let payload = app.payload(&["src/a.ts", "src/clean.ts"], &["no-floating-promises"]);
    let (code, frames, stderr) = headless(&app.0, &[], &payload);
    assert_eq!(code, 0, "{stderr}\n{frames:?}");
    let diagnostics: Vec<&serde_json::Value> = frames
        .iter()
        .filter(|(kind, _)| *kind == DIAGNOSTIC)
        .map(|(_, json)| json)
        .collect();
    assert_eq!(diagnostics.len(), 1, "{frames:?}");
    let diagnostic = diagnostics[0];
    assert_eq!(diagnostic["rule"], "no-floating-promises", "{diagnostic}");
    assert!(
        diagnostic["file_path"]
            .as_str()
            .is_some_and(|p| p.ends_with("src/a.ts")),
        "{diagnostic}"
    );
    // `load()` on line 3: byte 79 to 85 of the file.
    assert_eq!(diagnostic["range"]["pos"], 79, "{diagnostic}");
    assert_eq!(diagnostic["range"]["end"], 85, "{diagnostic}");

    // A rule that is off reports nothing.
    let payload = app.payload(&["src/a.ts"], &["await-thenable"]);
    let (code, frames, stderr) = headless(&app.0, &[], &payload);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        !frames.iter().any(|(kind, _)| *kind == DIAGNOSTIC),
        "{frames:?}"
    );
}

// What oxlint cannot use is said the way tsgolint says it: a payload that is
// no payload is an error frame and exit 1; a rule the backend does not have
// fails by name; an unknown flag is refused.
#[test]
fn the_backend_refuses_what_it_cannot_run() {
    let app = App::new("refuse");
    let (code, frames, _) = headless(&app.0, &[], "{ not json");
    assert_eq!(code, 1);
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(frames[0].0, 0, "an error frame: {frames:?}");
    assert!(
        frames[0].1.to_string().contains("error parsing config"),
        "{frames:?}"
    );

    let payload = app.payload(&["src/a.ts"], &["no-such-rule"]);
    let (code, frames, stderr) = headless(&app.0, &[], &payload);
    assert_eq!(code, 1, "{stderr}\n{frames:?}");
    assert!(
        format!("{frames:?}{stderr}").contains("no-such-rule"),
        "{frames:?}\n{stderr}"
    );

    let (code, _, stderr) = headless(&app.0, &["--nope"], "{}");
    assert_eq!(code, 1);
    assert!(stderr.contains("unknown option: --nope"), "{stderr}");
}

/// A stand-in for oxlint: prints its arguments and the backend it was
/// pointed at, runs that backend on `payload.json` as oxlint would, and
/// exits 3.
#[cfg(unix)]
fn fake_oxlint(app: &App, rel: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    app.write(
        rel,
        "#!/bin/sh\necho \"args: $*\"\necho \"backend: $OXLINT_TSGOLINT_PATH\"\n\"$OXLINT_TSGOLINT_PATH\" headless < payload.json | wc -c | tr -d ' '\nexit 3\n",
    );
    let path = app.0.join(rel);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[cfg(unix)]
fn oj_lint(cwd: &Path, args: &[&str], env: &[(&str, &Path)]) -> (i32, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oj"));
    command
        .arg("lint")
        .args(args)
        .current_dir(cwd)
        .env_remove("OJ_OXLINT_PATH")
        .env_remove("OXLINT_TSGOLINT_PATH");
    for (name, value) in env {
        command.env(name, value);
    }
    let out = command.output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

// `oj lint` is the project's oxlint: found in node_modules from the current
// directory up, started with oxlint's own arguments and `--type-aware`,
// pointed at this binary for the rules that need types, and exiting as it
// exits. The backend it names answers oxlint's `headless` call.
#[cfg(unix)]
#[test]
fn oj_lint_runs_the_projects_oxlint_with_oj_as_the_type_aware_backend() {
    let app = App::new("wrapper");
    fake_oxlint(&app, "node_modules/.bin/oxlint");
    app.write(
        "payload.json",
        &app.payload(&["src/a.ts"], &["no-floating-promises"]),
    );

    let (code, stdout, stderr) = oj_lint(&app.0, &["--fix", "src"], &[]);
    assert_eq!(code, 3, "oxlint's exit status: {stdout}\n{stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "args: --type-aware --fix src", "{stdout}");
    let backend = lines[1].strip_prefix("backend: ").unwrap();
    assert_eq!(
        Path::new(backend).canonicalize().unwrap(),
        Path::new(env!("CARGO_BIN_EXE_oj")).canonicalize().unwrap()
    );
    // The backend answered with a diagnostic frame, not an empty stream.
    assert!(
        lines[2].parse::<usize>().unwrap() > 5,
        "the backend wrote {} bytes",
        lines[2]
    );

    // `--type-aware` given is not given twice, and a nested directory finds
    // the same oxlint.
    app.write(
        "packages/web/payload.json",
        "{\"version\":2,\"configs\":[]}",
    );
    let (code, stdout, _) = oj_lint(&app.0.join("packages/web"), &["--type-aware"], &[]);
    assert_eq!(code, 3, "{stdout}");
    assert_eq!(
        stdout.lines().next(),
        Some("args: --type-aware"),
        "{stdout}"
    );

    // `OJ_OXLINT_PATH` names another one.
    let other = fake_oxlint(&app, "tools/oxlint");
    std::fs::remove_file(app.0.join("node_modules/.bin/oxlint")).unwrap();
    let (code, stdout, stderr) = oj_lint(&app.0, &[], &[("OJ_OXLINT_PATH", &other)]);
    assert_eq!(code, 3, "{stdout}\n{stderr}");
    assert_eq!(
        stdout.lines().next(),
        Some("args: --type-aware"),
        "{stdout}"
    );
}

#[cfg(unix)]
#[test]
fn oj_lint_without_oxlint_says_what_to_install() {
    let app = App::new("no-oxlint");
    let (code, stdout, stderr) = oj_lint(&app.0, &[], &[]);
    assert_eq!(code, 1, "{stdout}\n{stderr}");
    assert!(
        stderr.starts_with("oj lint: oxlint is not installed"),
        "{stderr}"
    );
}
