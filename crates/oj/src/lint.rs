// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `oj lint`: the project's oxlint with oj as its type-aware backend.
//!
//! oxlint lints syntax itself and hands the rules that need types to a
//! backend process, tsgolint by default (`<OXLINT_TSGOLINT_PATH> headless`,
//! a JSON payload of tsconfigs, files and rules on stdin, framed diagnostics
//! on stdout). oj carries that backend: tsrs's port of tsgolint's rules, on
//! the checker `oj typecheck` runs. `oj lint` runs oxlint pointed at this
//! binary; `oj headless` is the backend oxlint then starts.

use std::path::{Path, PathBuf};
use std::process::Command;

/// oxlint's flag for the rules that need types.
const TYPE_AWARE: &str = "--type-aware";

/// Runs the project's oxlint with `args`, type-aware rules on and oj as their
/// backend; returns oxlint's exit status.
pub fn lint(args: Vec<String>) -> i32 {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => return fail(&format!("cannot read the current directory: {e}")),
    };
    let Some(oxlint) = oxlint_path(&cwd) else {
        return fail(
            "oxlint is not installed: oj lint runs the project's oxlint (npm install -D oxlint), \
             or the one OJ_OXLINT_PATH names",
        );
    };
    let backend = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return fail(&format!("cannot find the oj binary: {e}")),
    };
    let mut command = Command::new(&oxlint);
    if !args.iter().any(|a| a == TYPE_AWARE) {
        command.arg(TYPE_AWARE);
    }
    match command
        .args(&args)
        .env("OXLINT_TSGOLINT_PATH", backend)
        .status()
    {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => fail(&format!("cannot run {}: {e}", oxlint.display())),
    }
}

/// The backend oxlint starts: `args` are what follows `headless`.
pub fn headless(args: Vec<String>) -> u8 {
    let mut all = vec!["headless".to_string()];
    all.extend(args);
    oj_typecheck::lint_headless(all)
}

/// `OJ_OXLINT_PATH`, or the nearest `node_modules/.bin/oxlint` from `dir` up.
fn oxlint_path(dir: &Path) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("OJ_OXLINT_PATH").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let name = if cfg!(windows) {
        "oxlint.cmd"
    } else {
        "oxlint"
    };
    dir.ancestors()
        .map(|d| d.join("node_modules/.bin").join(name))
        .find(|p| p.is_file())
}

fn fail(message: &str) -> i32 {
    eprintln!("oj lint: {message}");
    1
}
