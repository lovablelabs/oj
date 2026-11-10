// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Temp app roots for the tests that boot an in-process plugin host. The
//! host boots with a REAL `chdir` into its root (plugin-host.mjs: a plugin's
//! relative fs paths anchor there, as they did in the child it replaced),
//! and the working directory is process-global: deleting a root the process
//! still sits in leaves every concurrently running test with no current
//! directory, and `std::env::current_dir()` fails until another host boots.
//! rolldown's `DiagnosticOptions::default()` panics on that, so a test that
//! rendered a diagnostic at the wrong moment failed at random. A root goes
//! only after the process has stepped out of it.

use std::path::Path;

/// Removes `root`, stepping the process out of it first when it is (or was,
/// before it vanished) the current directory.
pub(crate) fn remove_root(root: &Path) {
    step_out_of(root);
    let _ = std::fs::remove_dir_all(root);
}

/// Moves the process to the temp dir when its current directory is under
/// `root` (as written, or as the host saw it after resolving symlinks: macOS
/// reports `/private/var/…` for a `/var/…` temp root), or no longer exists
/// at all.
pub(crate) fn step_out_of(root: &Path) {
    let real = std::fs::canonicalize(root).ok();
    let inside = std::env::current_dir().map_or(true, |cwd| {
        cwd.starts_with(root) || real.as_deref().is_some_and(|r| cwd.starts_with(r))
    });
    if inside {
        let _ = std::env::set_current_dir(std::env::temp_dir());
    }
}
