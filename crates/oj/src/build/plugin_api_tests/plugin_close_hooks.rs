// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;
use std::path::{Path, PathBuf};

use super::{build_app, scratch, write_files};

/// An app whose single plugin appends `buildEnd:<ok|first error line>` and
/// `closeBundle` to `<marks>/events`. Returns the events file path.
fn lifecycle_app(root: &Path, marks: &Path, main_js: &str) -> PathBuf {
    let events = marks.join("events");
    let plugins = format!(
        r#"import fs from "node:fs";
const log = (line) => fs.appendFileSync({events}, line + "\n");
export default [{{
  name: "lifecycle",
  buildEnd(err) {{ log("buildEnd:" + (err ? err.message.split("\n")[0] : "ok")); }},
  closeBundle() {{ log("closeBundle"); }},
}}];
"#,
        events = serde_json::to_string(&events.display().to_string()).unwrap(),
    );
    write_files(
        root,
        &[
            ("package.json", r#"{"name":"closehooks","version":"1.0.0"}"#),
            ("src/main.js", main_js),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
            ("oj.plugins.mjs", &plugins),
        ],
    );
    events
}

fn events(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Vite closes the bundle in a `finally` (build.ts), so a build that fails in
/// rolldown (here a syntax error) still runs every plugin's buildEnd with the
/// error and then closeBundle, exactly once each. oj used to skip both.
#[tokio::test]
async fn a_failed_build_still_runs_build_end_with_the_error_and_close_bundle_once() {
    let root = scratch("close-hooks-fail");
    let marks = scratch("close-hooks-fail-marks");
    let log = lifecycle_app(&root, &marks, "export const = ;\n");
    assert!(build_app(&root).await.is_err(), "the broken build fails");

    let ev = events(&log);
    let build_ends: Vec<&str> = ev
        .iter()
        .map(String::as_str)
        .filter(|e| e.starts_with("buildEnd:"))
        .collect();
    assert_eq!(
        build_ends.len(),
        1,
        "buildEnd ran once after the failure:\n{}",
        ev.join("\n")
    );
    assert!(
        !build_ends[0].starts_with("buildEnd:ok"),
        "buildEnd received the error:\n{}",
        ev.join("\n")
    );
    assert_eq!(
        ev.iter().filter(|e| *e == "closeBundle").count(),
        1,
        "closeBundle ran once after the failure:\n{}",
        ev.join("\n")
    );
}

/// A successful build: buildEnd(undefined) once, then closeBundle once.
#[tokio::test]
async fn a_successful_build_runs_build_end_without_error_and_close_bundle_once() {
    let root = scratch("close-hooks-ok");
    let marks = scratch("close-hooks-ok-marks");
    let log = lifecycle_app(&root, &marks, "document.body.dataset.ok = \"1\";\n");
    if let Err(err) = build_app(&root).await {
        panic!("the build succeeds:\n{err:#}");
    }

    let ev = events(&log);
    let build_ends: Vec<&str> = ev
        .iter()
        .map(String::as_str)
        .filter(|e| e.starts_with("buildEnd:"))
        .collect();
    assert_eq!(
        build_ends,
        ["buildEnd:ok"],
        "buildEnd(undefined) on success:\n{}",
        ev.join("\n")
    );
    assert_eq!(
        ev.iter().filter(|e| *e == "closeBundle").count(),
        1,
        "closeBundle once on success:\n{}",
        ev.join("\n")
    );
}
