// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Its own test binary: the held store and its flush hooks are process-wide.

use std::time::Duration;
use std::time::Instant;

use deno_core::url::Url;
use deno_runtime::code_cache::CodeCacheType;
use oj_js::code_cache::FsCodeCache;

const CHILD_DIR: &str = "OJ_TEST_CODE_CACHE_EXIT_DIR";

fn url(s: &str) -> Url {
    Url::parse(s).unwrap()
}

fn entries(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir).map_or(0, |d| {
        d.filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".bin"))
            .count()
    })
}

// A plugin's `process.exit()` (deno's op_exit) and the dev server's signal
// shutdown leave through `std::process::exit`, which skips `main`'s scope:
// the entries held since the last timed flush must land anyway.
#[test]
fn a_process_exit_still_writes_held_entries() {
    if let Ok(dir) = std::env::var(CHILD_DIR) {
        let cache = FsCodeCache::new(dir.into());
        cache.put(
            &url("file:///exit.js"),
            CodeCacheType::EsModule,
            1,
            b"bytecode",
        );
        std::process::exit(0);
    }
    let dir = tempfile::tempdir().unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_process_exit_still_writes_held_entries",
            "--nocapture",
        ])
        .env(CHILD_DIR, dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(entries(dir.path()), 1, "the held entry was lost at exit");
}

// A big app's first boot compiles faster than the timer drains: past the
// byte cap the store writes at once instead of holding it all for 10s.
#[test]
fn a_burst_over_the_cap_is_written_before_the_timer() {
    let dir = tempfile::tempdir().unwrap();
    let cache = FsCodeCache::new(dir.path().to_path_buf());
    let payload = vec![7u8; 1 << 20];
    for i in 0..40u64 {
        cache.put(
            &url(&format!("file:///big{i}.js")),
            CodeCacheType::EsModule,
            i,
            &payload,
        );
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while entries(dir.path()) < 33 {
        assert!(
            Instant::now() < deadline,
            "nothing written before the 10s timer: {}",
            entries(dir.path())
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
