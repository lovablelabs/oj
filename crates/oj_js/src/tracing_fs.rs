// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A [`FileSystem`] wrapper that records content reads (pulp P2): the one
//! seam every fs binding style crosses — `Deno.*` ops, `node:fs` CJS, and
//! named ESM imports alike — so plugin reads the JS-level interposition
//! cannot see still land in the trace. Delegates every operation verbatim;
//! recording happens on `open` with read intent and on the `read_file`
//! family (RealFs overrides the trait's open-routed defaults).
//!
//! Generated against deno_fs 0.169.0's trait surface; `cargo test
//! -p oj_js tracing_fs` carries a coverage guard that fails when a
//! deno_runtime bump changes the method set, so a new read path can never
//! silently bypass the recorder.

use std::borrow::Cow;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;

use deno_runtime::deno_fs::FileSystem;
use deno_runtime::deno_fs::FsDirEntry;
use deno_runtime::deno_fs::FsFileType;
use deno_runtime::deno_fs::FsReadDirRc;
use deno_runtime::deno_fs::OpenOptions;
use deno_runtime::deno_fs::RealFs;
use deno_runtime::deno_io::fs::File;
use deno_runtime::deno_io::fs::FsResult;
use deno_runtime::deno_io::fs::FsStat;
use deno_runtime::deno_io::fs::FsStatFs;
use deno_runtime::deno_permissions::CheckedPath;
use deno_runtime::deno_permissions::CheckedPathBuf;

/// Shared sink for recorded reads. The plugin host opens an action window
/// around each hook dispatch; every content read that lands while a window
/// is open is charged to EVERY open window (sound over-approximation: hooks
/// interleave on one isolate, so Rust-side attribution is impossible — the
/// JS-side AsyncLocalStorage buckets carry the precise attribution, and this
/// log is the safety net for reads they cannot see).
#[derive(Debug, Default)]
pub struct ReadLog {
    windows: Mutex<std::collections::HashMap<u64, std::collections::HashSet<PathBuf>>>,
    next: std::sync::atomic::AtomicU64,
}

impl ReadLog {
    pub fn begin(&self) -> u64 {
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.windows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, Default::default());
        id
    }

    pub fn finish(&self, id: u64) -> Vec<PathBuf> {
        self.windows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
            .map(|set| set.into_iter().collect())
            .unwrap_or_default()
    }

    fn record(&self, path: &Path) {
        let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        if windows.is_empty() {
            return;
        }
        for set in windows.values_mut() {
            set.insert(path.to_path_buf());
        }
    }
}

#[derive(Debug)]
pub struct TracingFs {
    inner: RealFs,
    log: Arc<ReadLog>,
}

impl TracingFs {
    pub fn new(log: Arc<ReadLog>) -> Self {
        Self { inner: RealFs, log }
    }

    fn record(&self, path: &Path) {
        self.log.record(path);
    }
}

#[async_trait::async_trait(?Send)]
impl FileSystem for TracingFs {
    fn cwd(&self) -> FsResult<PathBuf> {
        self.inner.cwd()
    }

    fn tmp_dir(&self) -> FsResult<PathBuf> {
        self.inner.tmp_dir()
    }

    fn chdir(&self, path: &CheckedPath) -> FsResult<()> {
        self.inner.chdir(path)
    }

    fn umask(&self, mask: Option<u32>) -> FsResult<u32> {
        self.inner.umask(mask)
    }

    fn open_sync(&self, path: &CheckedPath, options: OpenOptions) -> FsResult<Rc<dyn File>> {
        if options.read {
            self.record(path.as_ref());
        }
        self.inner.open_sync(path, options)
    }

    async fn open_async<'a>(
        &'a self,
        path: CheckedPathBuf,
        options: OpenOptions,
    ) -> FsResult<Rc<dyn File>> {
        if options.read {
            self.record(path.as_ref());
        }
        self.inner.open_async(path, options).await
    }

    fn mkdir_sync(&self, path: &CheckedPath, recursive: bool, mode: Option<u32>) -> FsResult<()> {
        self.inner.mkdir_sync(path, recursive, mode)
    }

    async fn mkdir_async(
        &self,
        path: CheckedPathBuf,
        recursive: bool,
        mode: Option<u32>,
    ) -> FsResult<()> {
        self.inner.mkdir_async(path, recursive, mode).await
    }

    #[cfg(unix)]
    fn chmod_sync(&self, path: &CheckedPath, mode: u32) -> FsResult<()> {
        self.inner.chmod_sync(path, mode)
    }

    #[cfg(not(unix))]
    fn chmod_sync(&self, path: &CheckedPath, mode: i32) -> FsResult<()> {
        self.inner.chmod_sync(path, mode)
    }

    #[cfg(unix)]
    async fn chmod_async(&self, path: CheckedPathBuf, mode: u32) -> FsResult<()> {
        self.inner.chmod_async(path, mode).await
    }

    #[cfg(not(unix))]
    async fn chmod_async(&self, path: CheckedPathBuf, mode: i32) -> FsResult<()> {
        self.inner.chmod_async(path, mode).await
    }

    fn chown_sync(&self, path: &CheckedPath, uid: Option<u32>, gid: Option<u32>) -> FsResult<()> {
        self.inner.chown_sync(path, uid, gid)
    }

    async fn chown_async(
        &self,
        path: CheckedPathBuf,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> FsResult<()> {
        self.inner.chown_async(path, uid, gid).await
    }

    fn lchmod_sync(&self, path: &CheckedPath, mode: u32) -> FsResult<()> {
        self.inner.lchmod_sync(path, mode)
    }

    async fn lchmod_async(&self, path: CheckedPathBuf, mode: u32) -> FsResult<()> {
        self.inner.lchmod_async(path, mode).await
    }

    fn lchown_sync(&self, path: &CheckedPath, uid: Option<u32>, gid: Option<u32>) -> FsResult<()> {
        self.inner.lchown_sync(path, uid, gid)
    }

    async fn lchown_async(
        &self,
        path: CheckedPathBuf,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> FsResult<()> {
        self.inner.lchown_async(path, uid, gid).await
    }

    fn remove_sync(&self, path: &CheckedPath, recursive: bool) -> FsResult<()> {
        self.inner.remove_sync(path, recursive)
    }

    async fn remove_async(&self, path: CheckedPathBuf, recursive: bool) -> FsResult<()> {
        self.inner.remove_async(path, recursive).await
    }

    fn copy_file_sync(&self, oldpath: &CheckedPath, newpath: &CheckedPath) -> FsResult<()> {
        self.inner.copy_file_sync(oldpath, newpath)
    }

    async fn copy_file_async(
        &self,
        oldpath: CheckedPathBuf,
        newpath: CheckedPathBuf,
    ) -> FsResult<()> {
        self.inner.copy_file_async(oldpath, newpath).await
    }

    fn cp_sync(&self, path: &CheckedPath, new_path: &CheckedPath) -> FsResult<()> {
        self.inner.cp_sync(path, new_path)
    }

    async fn cp_async(&self, path: CheckedPathBuf, new_path: CheckedPathBuf) -> FsResult<()> {
        self.inner.cp_async(path, new_path).await
    }

    fn stat_sync(&self, path: &CheckedPath) -> FsResult<FsStat> {
        self.inner.stat_sync(path)
    }

    async fn stat_async(&self, path: CheckedPathBuf) -> FsResult<FsStat> {
        self.inner.stat_async(path).await
    }

    fn lstat_sync(&self, path: &CheckedPath) -> FsResult<FsStat> {
        self.inner.lstat_sync(path)
    }

    async fn lstat_async(&self, path: CheckedPathBuf) -> FsResult<FsStat> {
        self.inner.lstat_async(path).await
    }

    fn statfs_sync(&self, path: &CheckedPath, bigint: bool) -> FsResult<FsStatFs> {
        self.inner.statfs_sync(path, bigint)
    }

    async fn statfs_async(&self, path: CheckedPathBuf, bigint: bool) -> FsResult<FsStatFs> {
        self.inner.statfs_async(path, bigint).await
    }

    fn realpath_sync(&self, path: &CheckedPath) -> FsResult<PathBuf> {
        self.inner.realpath_sync(path)
    }

    async fn realpath_async(&self, path: CheckedPathBuf) -> FsResult<PathBuf> {
        self.inner.realpath_async(path).await
    }

    fn read_dir_sync(&self, path: &CheckedPath) -> FsResult<Vec<FsDirEntry>> {
        self.inner.read_dir_sync(path)
    }

    async fn read_dir_async(&self, path: CheckedPathBuf) -> FsResult<FsReadDirRc> {
        self.inner.read_dir_async(path).await
    }

    fn rename_sync(&self, oldpath: &CheckedPath, newpath: &CheckedPath) -> FsResult<()> {
        self.inner.rename_sync(oldpath, newpath)
    }

    async fn rename_async(&self, oldpath: CheckedPathBuf, newpath: CheckedPathBuf) -> FsResult<()> {
        self.inner.rename_async(oldpath, newpath).await
    }

    fn rmdir_sync(&self, path: &CheckedPath) -> FsResult<()> {
        self.inner.rmdir_sync(path)
    }

    async fn rmdir_async(&self, path: CheckedPathBuf) -> FsResult<()> {
        self.inner.rmdir_async(path).await
    }

    fn link_sync(&self, oldpath: &CheckedPath, newpath: &CheckedPath) -> FsResult<()> {
        self.inner.link_sync(oldpath, newpath)
    }

    async fn link_async(&self, oldpath: CheckedPathBuf, newpath: CheckedPathBuf) -> FsResult<()> {
        self.inner.link_async(oldpath, newpath).await
    }

    fn symlink_sync(
        &self,
        oldpath: &CheckedPath,
        newpath: &CheckedPath,
        file_type: Option<FsFileType>,
    ) -> FsResult<()> {
        self.inner.symlink_sync(oldpath, newpath, file_type)
    }

    async fn symlink_async(
        &self,
        oldpath: CheckedPathBuf,
        newpath: CheckedPathBuf,
        file_type: Option<FsFileType>,
    ) -> FsResult<()> {
        self.inner.symlink_async(oldpath, newpath, file_type).await
    }

    fn read_link_sync(&self, path: &CheckedPath) -> FsResult<PathBuf> {
        self.inner.read_link_sync(path)
    }

    async fn read_link_async(&self, path: CheckedPathBuf) -> FsResult<PathBuf> {
        self.inner.read_link_async(path).await
    }

    fn truncate_sync(&self, path: &CheckedPath, len: u64) -> FsResult<()> {
        self.inner.truncate_sync(path, len)
    }

    async fn truncate_async(&self, path: CheckedPathBuf, len: u64) -> FsResult<()> {
        self.inner.truncate_async(path, len).await
    }

    fn utime_sync(
        &self,
        path: &CheckedPath,
        atime_secs: i64,
        atime_nanos: u32,
        mtime_secs: i64,
        mtime_nanos: u32,
    ) -> FsResult<()> {
        self.inner
            .utime_sync(path, atime_secs, atime_nanos, mtime_secs, mtime_nanos)
    }

    async fn utime_async(
        &self,
        path: CheckedPathBuf,
        atime_secs: i64,
        atime_nanos: u32,
        mtime_secs: i64,
        mtime_nanos: u32,
    ) -> FsResult<()> {
        self.inner
            .utime_async(path, atime_secs, atime_nanos, mtime_secs, mtime_nanos)
            .await
    }

    fn lutime_sync(
        &self,
        path: &CheckedPath,
        atime_secs: i64,
        atime_nanos: u32,
        mtime_secs: i64,
        mtime_nanos: u32,
    ) -> FsResult<()> {
        self.inner
            .lutime_sync(path, atime_secs, atime_nanos, mtime_secs, mtime_nanos)
    }

    async fn lutime_async(
        &self,
        path: CheckedPathBuf,
        atime_secs: i64,
        atime_nanos: u32,
        mtime_secs: i64,
        mtime_nanos: u32,
    ) -> FsResult<()> {
        self.inner
            .lutime_async(path, atime_secs, atime_nanos, mtime_secs, mtime_nanos)
            .await
    }

    fn write_file_sync(
        &self,
        path: &CheckedPath,
        options: OpenOptions,
        data: &[u8],
    ) -> FsResult<()> {
        self.inner.write_file_sync(path, options, data)
    }

    async fn write_file_async<'a>(
        &'a self,
        path: CheckedPathBuf,
        options: OpenOptions,
        data: Box<[u8]>,
    ) -> FsResult<()> {
        self.inner.write_file_async(path, options, data).await
    }

    fn read_file_sync(
        &self,
        path: &CheckedPath,
        options: OpenOptions,
    ) -> FsResult<Cow<'static, [u8]>> {
        self.record(path.as_ref());
        self.inner.read_file_sync(path, options)
    }

    async fn read_file_async<'a>(
        &'a self,
        path: CheckedPathBuf,
        options: OpenOptions,
    ) -> FsResult<Cow<'static, [u8]>> {
        self.record(path.as_ref());
        self.inner.read_file_async(path, options).await
    }

    fn is_file_sync(&self, path: &CheckedPath) -> bool {
        self.inner.is_file_sync(path)
    }

    fn is_dir_sync(&self, path: &CheckedPath) -> bool {
        self.inner.is_dir_sync(path)
    }

    fn exists_sync(&self, path: &CheckedPath) -> bool {
        self.inner.exists_sync(path)
    }

    async fn exists_async(&self, path: CheckedPathBuf) -> FsResult<bool> {
        self.inner.exists_async(path).await
    }

    fn read_text_file_lossy_sync(&self, path: &CheckedPath) -> FsResult<Cow<'static, str>> {
        self.record(path.as_ref());
        self.inner.read_text_file_lossy_sync(path)
    }

    async fn read_text_file_lossy_async<'a>(
        &'a self,
        path: CheckedPathBuf,
    ) -> FsResult<Cow<'static, str>> {
        self.record(path.as_ref());
        self.inner.read_text_file_lossy_async(path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_are_charged_to_every_open_window_and_only_then() {
        let log = ReadLog::default();
        // No window open: recording is a no-op (the hot steady-state).
        log.record(Path::new("/idle.txt"));
        let a = log.begin();
        log.record(Path::new("/one.txt"));
        let b = log.begin();
        log.record(Path::new("/two.txt"));
        let got_a = log.finish(a);
        log.record(Path::new("/three.txt"));
        let got_b = log.finish(b);
        assert!(got_a.contains(&PathBuf::from("/one.txt")));
        assert!(
            got_a.contains(&PathBuf::from("/two.txt")),
            "charge-all while open"
        );
        assert!(!got_a.contains(&PathBuf::from("/idle.txt")));
        assert!(
            !got_b.contains(&PathBuf::from("/one.txt")),
            "opened after that read"
        );
        assert!(
            got_b.contains(&PathBuf::from("/three.txt")),
            "window b still open"
        );
        assert!(log.finish(a).is_empty(), "double finish is inert");
    }

    #[test]
    fn tracing_fs_records_reads_and_delegates_content() {
        use deno_runtime::deno_permissions::CheckedPathBuf;
        let dir = std::env::temp_dir().join(format!("oj-tracingfs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("input.txt");
        std::fs::write(&file, b"hello").unwrap();

        let log = Arc::new(ReadLog::default());
        let fs = TracingFs::new(log.clone());
        let id = log.begin();
        let checked = CheckedPathBuf::unsafe_new(file.clone());
        let read = fs
            .read_file_sync(&checked.as_checked_path(), OpenOptions::read())
            .unwrap();
        assert_eq!(&*read, b"hello", "delegation is verbatim");
        let seen = log.finish(id);
        assert!(
            seen.contains(&file),
            "the read landed in the window: {seen:?}"
        );
        // Writes are not reads: opening for write records nothing.
        let id = log.begin();
        let out = dir.join("out.txt");
        let checked_out = CheckedPathBuf::unsafe_new(out.clone());
        fs.write_file_sync(
            &checked_out.as_checked_path(),
            OpenOptions::write(true, false, true, None),
            b"x",
        )
        .unwrap();
        let seen = log.finish(id);
        assert!(
            !seen.contains(&out),
            "write-only open is not a content read"
        );
    }
}
