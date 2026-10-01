// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use super::memo::{build_memo, write_memo};
use super::{
    hash_file, par_map, read_chunk_index, read_closure, read_css_urls, touch, ChunkIndexFile,
    GenerationManifest, ManifestFile, PinnedBundle, StartBundleStore, ARTIFACTS, BLOBS_DIR,
    CHUNKS_DIR, CLOSURE_FILE, KEEP_GENERATIONS, MANIFEST_FILE, START_BUNDLE_FORMAT,
};

impl StartBundleStore {
    pub fn persist(&self, start_dir: &Path) -> Option<(String, PinnedBundle)> {
        let files = read_closure(&start_dir.join(CLOSURE_FILE))?;
        let digests = par_map(&files, |p| hash_file(p));
        let key = self.key_from(&files, &digests).ok()?;
        let index = read_chunk_index(start_dir)?;
        let css_urls = read_css_urls(start_dir);
        let chunk_dir = start_dir.join(CHUNKS_DIR);
        let blobs = self.dir.join(BLOBS_DIR);
        let manifest_files = store_chunk_blobs(&index.files, &chunk_dir, &blobs)?;
        let manifest = GenerationManifest {
            format: START_BUNDLE_FORMAT,
            entry: index.entry,
            css_urls,
            files: manifest_files,
        };
        let entry = self.dir.join(&key);
        if !entry.is_dir() {
            self.commit_generation(start_dir, &key, &manifest, &entry)?;
        }
        write_memo(&entry, &build_memo(&files, &digests));
        touch(&entry);
        self.write_current(&key);
        // Enforce the window at write time like Vite's dep-cache commit:
        // constant work here, deletion and blob accounting in the background.
        // The full budget prune stays a boot-only pass.
        self.enforce_window(KEEP_GENERATIONS);
        // Another instance's prune can sweep a blob this persist dedup-skipped,
        // between the existence check and the generation rename (its scan saw
        // no generation referencing it yet). The chunk sources are still in
        // the build dir, so re-copy whatever is missing; a sweep that starts
        // after the rename sees this manifest and keeps its blobs.
        for (name, f) in &manifest.files {
            let blob = blobs.join(&f.hash);
            if blob.is_file() {
                continue;
            }
            if !write_blob(&chunk_dir.join(name), &blobs, &f.hash) && !blob.is_file() {
                return None;
            }
        }
        Some((key, self.pin(&manifest)))
    }

    /// Stages the generation dir under `.tmp-` and renames it into place. A
    /// failed rename is fine when a racing writer committed the same key.
    fn commit_generation(
        &self,
        start_dir: &Path,
        key: &str,
        manifest: &GenerationManifest,
        entry: &Path,
    ) -> Option<()> {
        let tmp = self
            .dir
            .join(format!(".tmp-{}-{}", key.get(..16)?, std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).ok()?;
        for name in ARTIFACTS.into_iter().chain([CLOSURE_FILE]) {
            if fs::copy(start_dir.join(name), tmp.join(name)).is_err() {
                let _ = fs::remove_dir_all(&tmp);
                return None;
            }
        }
        let bytes = serde_json::to_vec(manifest).ok()?;
        if fs::write(tmp.join(MANIFEST_FILE), bytes).is_err() {
            let _ = fs::remove_dir_all(&tmp);
            return None;
        }
        if fs::rename(&tmp, entry).is_err() {
            let _ = fs::remove_dir_all(&tmp);
            if !entry.is_dir() {
                return None;
            }
        }
        Some(())
    }
}

/// Hashes every chunk, copies the ones not yet in the content-addressed blob
/// store, and returns the manifest's name -> (hash, size) map.
fn store_chunk_blobs(
    chunks: &[ChunkIndexFile],
    chunk_dir: &Path,
    blobs: &Path,
) -> Option<BTreeMap<String, ManifestFile>> {
    let chunk_paths: Vec<PathBuf> = chunks.iter().map(|f| chunk_dir.join(&f.name)).collect();
    let chunk_hashes = par_map(&chunk_paths, |p| hash_file(p));
    fs::create_dir_all(blobs).ok()?;
    let mut manifest_files = BTreeMap::new();
    for (f, (path, hash)) in chunks.iter().zip(chunk_paths.iter().zip(&chunk_hashes)) {
        let hex = hash.as_ref()?.to_hex().to_string();
        let size = fs::metadata(path).ok()?.len();
        if !blobs.join(&hex).is_file() && !write_blob(path, blobs, &hex) {
            return None;
        }
        manifest_files.insert(f.name.clone(), ManifestFile { hash: hex, size });
    }
    Some(manifest_files)
}

/// Copies `src` to `blobs/<hash>` via a `.tmp-` file and rename. A failed
/// rename still succeeds when a racing writer already committed the blob.
fn write_blob(src: &Path, blobs: &Path, hash: &str) -> bool {
    let blob = blobs.join(hash);
    let tmp = blobs.join(format!(
        ".tmp-{}-{}",
        hash.get(..16).unwrap_or(hash),
        std::process::id()
    ));
    if fs::copy(src, &tmp).is_err() {
        let _ = fs::remove_file(&tmp);
        return false;
    }
    if fs::rename(&tmp, &blob).is_err() {
        let _ = fs::remove_file(&tmp);
        return blob.is_file();
    }
    true
}
