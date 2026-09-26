// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Shared helpers for the cache suites.

use oj_cache::{CachedModule, PersistentCache};

pub const VERSION: &str = "0.0.0-test";

pub struct Fixture {
    pub dir: tempfile::TempDir,
    pub cache: PersistentCache,
}

pub fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = PersistentCache::new(dir.path().to_path_buf(), VERSION);
    Fixture { dir, cache }
}

/// The on-disk location of an entry. `path_for` is private, so the layout is
/// re-derived here on purpose: it is a compatibility contract between oj
/// versions sharing a `.oj-cache`.
#[allow(dead_code)] // not every test binary that includes `common` uses it
pub fn entry_path(dir: &std::path::Path, key: &str) -> std::path::PathBuf {
    dir.join(&key[..2]).join(format!("{key}.json"))
}

pub fn module(code: &str) -> CachedModule {
    CachedModule {
        code: code.to_string(),
        map_data_url: Some("data:application/json;base64,e30=".into()),
        imports: vec!["/node_modules/react/index.js".into()],
        is_boundary: true,
        kind: "esm".into(),
        require_map: vec![("react".into(), "/node_modules/react/index.js".into())],
        css_exports: vec![("button".into(), "button_x1".into())],
        fs_allow: vec!["/node_modules/react".into()],
        watch_files: vec!["/tailwind.config.ts".into()],
        hot: None,
    }
}

/// Every file under `dir`, relative and sorted.
pub fn walk(dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(
                    path.strip_prefix(dir)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    out.sort();
    out
}
