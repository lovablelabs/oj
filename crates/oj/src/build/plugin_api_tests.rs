// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;
use std::path::{Path, PathBuf};

use super::{build, CliOptions};

mod build_assets_dir;
mod build_css_options;
mod build_unresolved;
mod css_plugin_transform;
mod emit_chunk;
mod emit_chunk_buildstart;
mod emit_html_chunk;
mod generate_bundle_fidelity;
mod html_attribute_quotes;
mod multi_html_input;
mod plugin_close_hooks;
mod plugin_ctx_chain;
mod plugin_hook_gating_build;
mod plugin_load_jsx;
mod rolldown_options;
mod ssr_options;
mod vite_build_manifest_in_bundle;
mod vite_css_post_shim;

/// A fresh app dir, pid-keyed and cleaned before use, never after: a later
/// run reclaims it, and nothing deletes a directory an in-process host's
/// boot chdir might still point at.
pub(super) fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("oj-api-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes `files` (relative path, contents) under `root`, creating parents.
pub(super) fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (rel, contents) in files {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
}

/// `oj build <root>` (production mode unless given) with default CLI flags.
pub(super) async fn build_app(root: &Path) -> anyhow::Result<()> {
    build(root.to_path_buf(), None, CliOptions::default()).await
}

/// Every file under `dir`, relative path to contents (lossy UTF-8), sorted.
pub(super) fn files_under(dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let rel = p
                    .strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((
                    rel,
                    String::from_utf8_lossy(&fs::read(&p).unwrap()).into_owned(),
                ));
            }
        }
    }
    out.sort();
    out
}

/// The concatenated contents of every `.js` file under `dir`.
pub(super) fn js_under(dir: &Path) -> String {
    files_under(dir)
        .into_iter()
        .filter(|(rel, _)| rel.ends_with(".js") || rel.ends_with(".mjs"))
        .map(|(_, code)| code)
        .collect::<Vec<_>>()
        .join("\n")
}
