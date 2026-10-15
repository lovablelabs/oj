// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use super::ensure_asset;

/// Two writers of one asset in one process (the plugin host and the config
/// extractor at boot) must both succeed and leave only the asset behind.
#[test]
fn concurrent_writers_in_one_process_all_succeed() {
    for round in 0..50 {
        let dir = tempfile::tempdir().unwrap();
        let start = std::sync::Arc::new(std::sync::Barrier::new(8));
        let writers: Vec<_> = (0..8)
            .map(|_| {
                let (dir, start) = (dir.path().to_path_buf(), std::sync::Arc::clone(&start));
                std::thread::spawn(move || {
                    start.wait();
                    ensure_asset(&dir, "shared.mjs", "export const x = 1;\n")
                })
            })
            .collect();
        for writer in writers {
            let result = writer.join().unwrap();
            assert!(result.is_ok(), "round {round}: {result:?}");
        }
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["shared.mjs"], "round {round}");
        let written = std::fs::read_to_string(dir.path().join("shared.mjs")).unwrap();
        assert_eq!(written, "export const x = 1;\n");
    }
}
