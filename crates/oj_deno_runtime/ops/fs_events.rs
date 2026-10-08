// Copyright 2018-2026 the Deno authors. MIT license.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::convert::From;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use deno_core::AsyncRefCell;
use deno_core::CancelFuture;
use deno_core::CancelHandle;
use deno_core::OpState;
use deno_core::RcRef;
use deno_core::Resource;
use deno_core::ResourceId;
use deno_core::ToV8;
use deno_core::op2;
use deno_core::parking_lot::Mutex;
use deno_error::JsErrorClass;
use deno_error::builtin_classes::GENERIC_ERROR;
use deno_permissions::PermissionsContainer;
use notify::Error as NotifyError;
use notify::EventKind;
use notify::RecommendedWatcher;
use notify::RecursiveMode;
use notify::Watcher;
use notify::event::AccessKind;
use notify::event::AccessMode;
use notify::event::Event as NotifyEvent;
use notify::event::ModifyKind;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

deno_core::extension!(
  deno_fs_events,
  ops = [op_fs_events_open, op_fs_events_poll],
);

/// Capacity of the queue between the notify backend thread and the JS-side
/// consumer. The watcher callback must not block, so when this queue is full
/// further events are dropped; the drop is recorded and surfaced to the
/// consumer as a `flag: "rescan"` event (see [`op_fs_events_poll`]) instead
/// of being silently lost (see denoland/deno#11373). The capacity is sized so
/// that ordinary bursts (e.g. unpacking an archive into a watched directory)
/// fit without dropping; tokio's bounded channel allocates lazily, so an
/// idle watcher does not pay for it.
const FS_EVENT_QUEUE_CAPACITY: usize = 1024;

struct FsEventsResource {
  receiver: AsyncRefCell<mpsc::Receiver<Result<FsEvent, NotifyError>>>,
  cancel: CancelHandle,
  /// Shared backend used to clean up our watch on drop.
  inner: Arc<WatcherInner>,
  /// Identifies our `WatchSender` entry in [`WatcherInner::senders`].
  id: u64,
  /// The (path, recursive_mode) pairs this resource registered. Tracked so the
  /// shared watcher unwatches them when the last interested resource closes.
  watched: Vec<(PathBuf, RecursiveMode)>,
  /// Set by the watcher callback when the event queue overflowed and events
  /// were dropped; drained by `op_fs_events_poll`, which surfaces the loss as
  /// a `flag: "rescan"` event.
  overflowed: Arc<AtomicBool>,
}

impl Resource for FsEventsResource {
  fn name(&self) -> Cow<'_, str> {
    "fsEvents".into()
  }

  fn close(self: Rc<Self>) {
    self.cancel.cancel();
  }
}

impl Drop for FsEventsResource {
  fn drop(&mut self) {
    // Remove this resource's sender from the shared dispatch list so the
    // watcher callback stops trying to deliver events to a dead channel.
    self.inner.senders.lock().remove(self.id);

    // Reference-count the underlying watches: only unwatch when no other
    // resource still depends on this path. Without this, calling
    // `Deno.watchFs(path)` repeatedly leaks watches in the shared
    // `RecommendedWatcher` — on Windows each leaked watch registers a
    // separate `ReadDirectoryChangesW` request, so the next watcher created
    // for the same path receives every event N times.
    let mut watched_paths = self.inner.watched_paths.lock();
    let mut watcher = self.inner.watcher.lock();
    for (path, mode) in &self.watched {
      let key = (path.clone(), *mode);
      let Some(count) = watched_paths.get_mut(&key) else {
        continue;
      };
      *count = count.saturating_sub(1);
      if *count == 0 {
        watched_paths.remove(&key);
        // Best-effort: ignore errors (e.g. the watcher already lost track of
        // the path because the path was deleted).
        let _ = watcher.unwatch(path);
      }
    }
  }
}

/// Represents a file system event.
///
/// We do not use the event directly from the notify crate. We flatten
/// the structure into this simpler structure. We want to only make it more
/// complex as needed.
///
/// Feel free to expand this struct as long as you can add tests to demonstrate
/// the complexity.
#[derive(ToV8, Debug, Clone)]
struct FsEvent {
  kind: &'static str,
  #[to_v8(serde)]
  paths: Vec<PathBuf>,
  flag: Option<&'static str>,
}

impl From<NotifyEvent> for FsEvent {
  fn from(e: NotifyEvent) -> Self {
    let kind = match e.kind {
      EventKind::Any => "any",
      EventKind::Access(_) => "access",
      EventKind::Create(_) => "create",
      EventKind::Modify(modify_kind) => match modify_kind {
        ModifyKind::Name(_) => "rename",
        ModifyKind::Any
        | ModifyKind::Data(_)
        | ModifyKind::Metadata(_)
        | ModifyKind::Other => "modify",
      },
      EventKind::Remove(_) => "remove",
      EventKind::Other => "other",
    };
    let flag = e.flag().map(|f| match f {
      notify::event::Flag::Rescan => "rescan",
    });
    FsEvent {
      kind,
      paths: e.paths,
      flag,
    }
  }
}

fn is_ignored_notify_event(event: &NotifyEvent) -> bool {
  // notify 8 added inotify OPEN events to the default watch mask. Deno did not
  // expose these with notify 6, and forwarding them can self-amplify because
  // our path filtering performs filesystem reads.
  matches!(
    event.kind,
    EventKind::Access(AccessKind::Open(AccessMode::Any))
  )
}

struct WatchSender {
  /// Unique identifier so the matching entry can be removed when the
  /// owning [`FsEventsResource`] is dropped.
  id: u64,
  /// Original paths as provided by the caller.
  paths: Vec<PathBuf>,
  /// Pre-canonicalized versions of `paths`, computed once at watch time
  /// to avoid repeated syscalls in the event callback hot path.
  /// Entries are `None` if canonicalization failed for that path.
  canonical_paths: Vec<Option<PathBuf>>,
  /// Identities of `paths`, computed alongside `canonical_paths`. Frozen
  /// at registration: after an atomic-save replace the stored inode is
  /// stale and the canonical-prefix route carries the match (the same
  /// trade `canonical_paths` already makes).
  identities: Vec<Option<FileIdentity>>,
  /// Paths whose events should be filtered out (the `ignore` option).
  ignore: Vec<PathBuf>,
  /// Pre-canonicalized versions of `ignore`, mirroring `canonical_paths`.
  canonical_ignore: Vec<Option<PathBuf>>,
  /// Identities of `ignore`, mirroring `identities`.
  ignore_identities: Vec<Option<FileIdentity>>,
  sender: mpsc::Sender<Result<FsEvent, NotifyError>>,
  /// See [`FsEventsResource::overflowed`].
  overflowed: Arc<AtomicBool>,
}

impl WatchSender {
  /// Deliver an event or error without blocking the notify backend thread.
  /// If the queue is full the message is dropped and the overflow is
  /// recorded so the consumer learns about the loss via a `"rescan"` event.
  /// A closed channel just means the watcher resource is gone; that is not
  /// a loss.
  fn send_or_record_overflow(&self, msg: Result<FsEvent, NotifyError>) {
    if let Err(TrySendError::Full(_)) = self.sender.try_send(msg) {
      self.overflowed.store(true, Ordering::Relaxed);
    }
  }
}

/// The live watchers, indexed by what an event can match them on, so the
/// callback visits only candidates instead of every watcher per event: under
/// a steady event stream a linear scan over thousands of watchers outruns the
/// notify thread, which then never gets back to registering the next watch.
#[derive(Default)]
struct SenderSet {
  senders: HashMap<u64, WatchSender>,
  /// Watchers by each watched path, raw and canonical.
  by_path: HashMap<PathBuf, Vec<u64>>,
  by_identity: HashMap<FileIdentity, Vec<u64>>,
}

impl SenderSet {
  fn keys(ws: &WatchSender) -> (Vec<PathBuf>, Vec<FileIdentity>) {
    let paths = ws.paths.iter().chain(ws.canonical_paths.iter().flatten());
    let identities = ws.identities.iter().flatten().copied();
    (paths.cloned().collect(), identities.collect())
  }

  fn insert(&mut self, ws: WatchSender) {
    let (paths, identities) = Self::keys(&ws);
    for path in paths {
      self.by_path.entry(path).or_default().push(ws.id);
    }
    for identity in identities {
      self.by_identity.entry(identity).or_default().push(ws.id);
    }
    self.senders.insert(ws.id, ws);
  }

  fn remove(&mut self, id: u64) {
    let Some(ws) = self.senders.remove(&id) else {
      return;
    };
    let (paths, identities) = Self::keys(&ws);
    for path in paths {
      if let Some(ids) = self.by_path.get_mut(&path) {
        ids.retain(|other| *other != id);
        if ids.is_empty() {
          self.by_path.remove(&path);
        }
      }
    }
    for identity in identities {
      if let Some(ids) = self.by_identity.get_mut(&identity) {
        ids.retain(|other| *other != id);
        if ids.is_empty() {
          self.by_identity.remove(&identity);
        }
      }
    }
  }

  /// Every watcher an event on `event_paths` can match, in registration
  /// order. A superset: the caller still applies the exact checks.
  fn candidates(&self, event_paths: &[EventPath]) -> Vec<&WatchSender> {
    if !cfg!(unix) {
      let mut all: Vec<&WatchSender> = self.senders.values().collect();
      all.sort_by_key(|ws| ws.id);
      return all;
    }
    let mut ids = Vec::new();
    let add_ancestors = |path: &Path, ids: &mut Vec<u64>| {
      for ancestor in path.ancestors() {
        if let Some(found) = self.by_path.get(ancestor) {
          ids.extend_from_slice(found);
        }
      }
    };
    for event in event_paths {
      if let Some(found) = self.by_path.get(event.path) {
        ids.extend_from_slice(found);
      }
      if let Some(canonical) = &event.canonical {
        add_ancestors(canonical, &mut ids);
      }
      if let Some(parent) = &event.canonical_parent {
        add_ancestors(parent, &mut ids);
      }
      if let Some(found) = event.identity.and_then(|i| self.by_identity.get(&i))
      {
        ids.extend_from_slice(found);
      }
    }
    ids.sort_unstable();
    ids.dedup();
    ids.iter().filter_map(|id| self.senders.get(id)).collect()
  }
}

/// `notify::Error` is not `Clone` (it can wrap an `std::io::Error`), but the
/// shared watcher callback has to deliver an error to every interested
/// resource, so rebuild an equivalent error for each receiver. For I/O errors
/// the original `ErrorKind` and message are preserved.
fn clone_notify_error(err: &NotifyError) -> NotifyError {
  let kind = match &err.kind {
    notify::ErrorKind::Generic(msg) => notify::ErrorKind::Generic(msg.clone()),
    notify::ErrorKind::Io(io_err) => notify::ErrorKind::Io(
      std::io::Error::new(io_err.kind(), io_err.to_string()),
    ),
    notify::ErrorKind::PathNotFound => notify::ErrorKind::PathNotFound,
    notify::ErrorKind::WatchNotFound => notify::ErrorKind::WatchNotFound,
    notify::ErrorKind::InvalidConfig(config) => {
      notify::ErrorKind::InvalidConfig(*config)
    }
    notify::ErrorKind::MaxFilesWatch => notify::ErrorKind::MaxFilesWatch,
  };
  NotifyError {
    kind,
    paths: err.paths.clone(),
  }
}

struct WatcherInner {
  /// Per-watcher dispatch list. The shared notify callback iterates this
  /// list on every event. Wrapped in `Arc<Mutex<...>>` so the watcher
  /// callback (which lives on the notify backend thread) can hold a
  /// reference without keeping the rest of [`WatcherInner`] alive.
  senders: Arc<Mutex<SenderSet>>,
  /// The shared `RecommendedWatcher` instance backing every
  /// `Deno.watchFs(...)` call in this OpState.
  watcher: Mutex<RecommendedWatcher>,
  /// Reference count per (path, recursive_mode) registered with `watcher`.
  /// When a count drops to zero, the path is unwatched.
  watched_paths: Mutex<HashMap<(PathBuf, RecursiveMode), usize>>,
  /// Monotonic counter for assigning `WatchSender::id`.
  next_id: AtomicU64,
}

struct WatcherState {
  inner: Arc<WatcherInner>,
}

#[allow(
  clippy::disallowed_methods,
  reason = "always using real fs with watcher"
)]
fn canonicalize_path(path: &Path) -> Option<PathBuf> {
  path.canonicalize().ok()
}

/// A file's (device, inode), compared in place of `same_file::is_same_file`,
/// which opens both files: per watcher and per event that floods the notify
/// thread once a process holds thousands of watchers.
type FileIdentity = (u64, u64);

#[cfg(unix)]
#[allow(
  clippy::disallowed_methods,
  reason = "always using real fs with watcher"
)]
fn file_identity(path: &Path) -> Option<FileIdentity> {
  use std::os::unix::fs::MetadataExt;
  std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

#[cfg(not(unix))]
fn file_identity(_path: &Path) -> Option<FileIdentity> {
  None
}

/// An event path resolved once per event, so matching it against every
/// watcher costs no syscalls per watcher.
struct EventPath<'a> {
  path: &'a Path,
  canonical: Option<PathBuf>,
  identity: Option<FileIdentity>,
  removed: bool,
  canonical_parent: Option<PathBuf>,
}

impl<'a> EventPath<'a> {
  fn new(path: &'a Path) -> Self {
    let removed = is_file_removed(path);
    EventPath {
      path,
      canonical: canonicalize_path(path),
      identity: file_identity(path),
      removed,
      canonical_parent: if removed {
        path.parent().and_then(canonicalize_path)
      } else {
        None
      },
    }
  }

  fn is_same_file(&self, path: &Path, identity: Option<FileIdentity>) -> bool {
    match (self.identity, identity) {
      (Some(event), Some(watched)) => event == watched,
      _ if cfg!(unix) => false,
      _ => same_file::is_same_file(self.path, path).unwrap_or(false),
    }
  }
}

/// Check if `event_path` (or its canonicalized form) matches one of the
/// watched paths. The watched paths are pre-canonicalized to avoid
/// repeated syscalls in the hot path.
fn event_matches_watched_paths(
  event: &EventPath,
  paths: &[PathBuf],
  canonical_paths: &[Option<PathBuf>],
  identities: &[Option<FileIdentity>],
) -> bool {
  for ((path, canonical_path), identity) in paths
    .iter()
    .zip(canonical_paths.iter())
    .zip(identities.iter())
  {
    if event.is_same_file(path, *identity) {
      return true;
    }
    if matches!(
      (&event.canonical, canonical_path),
      (Some(ce), Some(cp)) if ce.starts_with(cp)
    ) {
      return true;
    }
  }
  false
}

/// Check if `event_path` refers to a file that has been removed and
/// that file is within one of the watched paths. This is needed because
/// `event_matches_watched_paths` will fail for removed files (canonicalize
/// and is_same_file don't work on non-existent paths). On macOS with
/// FSEvents, remove events may arrive as generic events for a path that
/// no longer exists.
fn removed_event_matches_watched_paths(
  event: &EventPath,
  paths: &[PathBuf],
  canonical_paths: &[Option<PathBuf>],
) -> bool {
  if !event.removed {
    return false;
  }
  let event_path = event.path;
  let canonical_parent = &event.canonical_parent;
  for (path, canonical_path) in paths.iter().zip(canonical_paths.iter()) {
    // Direct path comparison: the file is gone so is_same_file won't work,
    // but the event path may match the watched path or its canonical form
    // exactly (e.g. when watching a single file that gets deleted).
    if event_path == path {
      return true;
    }
    if canonical_path
      .as_ref()
      .is_some_and(|cp| event_path == cp.as_path())
    {
      return true;
    }
    // Check if the removed file's parent is within a watched directory.
    if matches!(
      (canonical_parent, canonical_path),
      (Some(cp_event), Some(cp_watched)) if cp_event.starts_with(cp_watched)
    ) {
      return true;
    }
  }
  false
}

fn is_file_removed(event_path: &Path) -> bool {
  let exists_path = std::fs::exists(event_path);
  match exists_path {
    Ok(res) => !res,
    Err(_) => false,
  }
}

// The notify crate's Windows backend reports a missing watch path as a
// Generic error with this message (see `add_watch` in notify's
// src/windows.rs), unlike the inotify backend which reports PathNotFound.
// Normalize it so a watch on a vanished path surfaces as NotFound on all
// platforms; the node:fs `fs.watch` polyfill relies on this to emit a
// recoverable ENOENT instead of an unrecognized error that kills watchers
// like chokidar/vite (see denoland/deno#35855).
const WINDOWS_PATH_NOT_FOUND_MSG: &str =
  "Input watch path is neither a file nor a directory.";

deno_error::js_error_wrapper!(NotifyError, JsNotifyError, |err| {
  match &err.kind {
    notify::ErrorKind::Generic(msg) if msg == WINDOWS_PATH_NOT_FOUND_MSG => {
      "NotFound".into()
    }
    notify::ErrorKind::Generic(_) => GENERIC_ERROR.into(),
    notify::ErrorKind::Io(e) => e.get_class(),
    notify::ErrorKind::PathNotFound => "NotFound".into(),
    notify::ErrorKind::WatchNotFound => "NotFound".into(),
    notify::ErrorKind::InvalidConfig(_) => "InvalidData".into(),
    notify::ErrorKind::MaxFilesWatch => GENERIC_ERROR.into(),
  }
});

#[derive(Debug, thiserror::Error, deno_error::JsError)]
pub enum FsEventsError {
  #[class(inherit)]
  #[error(transparent)]
  Resource(#[from] deno_core::error::ResourceError),
  #[class(inherit)]
  #[error(transparent)]
  Permission(#[from] deno_permissions::PermissionCheckError),
  #[class(inherit)]
  #[error(transparent)]
  Notify(JsNotifyError),
  #[class(inherit)]
  #[error(transparent)]
  Canceled(#[from] deno_core::Canceled),
}

fn make_watch_sender(
  id: u64,
  paths: Vec<PathBuf>,
  ignore: Vec<PathBuf>,
  sender: mpsc::Sender<Result<FsEvent, NotifyError>>,
  overflowed: Arc<AtomicBool>,
) -> WatchSender {
  let canonical_paths = paths.iter().map(|p| canonicalize_path(p)).collect();
  let identities = paths.iter().map(|p| file_identity(p)).collect();
  let canonical_ignore = ignore.iter().map(|p| canonicalize_path(p)).collect();
  let ignore_identities = ignore.iter().map(|p| file_identity(p)).collect();
  WatchSender {
    id,
    paths,
    canonical_paths,
    identities,
    ignore,
    canonical_ignore,
    ignore_identities,
    sender,
    overflowed,
  }
}

/// Resolve one notify payload and deliver it to the matching watchers. Runs
/// on the dispatch thread, never on notify's backend thread: that thread
/// also serves `watch()`/`unwatch()` requests, so any per-event work there
/// (path resolution alone is a handful of syscalls) stalls registration once
/// events outpace it. libuv and chokidar likewise do no matching on their
/// watcher threads.
fn dispatch_event(
  senders: &Mutex<SenderSet>,
  res: Result<NotifyEvent, NotifyError>,
) {
  let res2 = match res {
    Ok(event) => Ok(FsEvent::from(event)),
    Err(e) => Err(e),
  };
  let event_paths: Vec<EventPath> = match &res2 {
    Ok(event) => event.paths.iter().map(|p| EventPath::new(p)).collect(),
    Err(_) => Vec::new(),
  };
  let senders = senders.lock();
  let targets = match &res2 {
    Ok(_) => senders.candidates(&event_paths),
    Err(_) => {
      let mut all: Vec<&WatchSender> = senders.senders.values().collect();
      all.sort_by_key(|ws| ws.id);
      all
    }
  };
  for ws in targets {
    match &res2 {
      // Only send the event if the path matches one of the paths
      // that the user is watching.
      Ok(event) => {
        // Skip events whose paths all fall under one of the ignored
        // paths (the `ignore` option). Removed files are checked too so
        // deletions inside an ignored directory don't leak through
        // (canonicalize/is_same_file fail for paths that no longer
        // exist). The `is_empty` guard keeps this off the event-callback
        // hot path for the common case where `ignore` is unused.
        //
        // `all` (not `any`) means an event is dropped only when every
        // path is ignored, so a rename out of an ignored dir into a
        // watched one (paths = [from, to] with `from` ignored and `to`
        // watched) is still delivered. The reverse case (rename into an
        // ignored dir) still delivers an event containing the ignored
        // `to` path; that is intentional.
        if !ws.ignore.is_empty()
          && !event.paths.is_empty()
          && event_paths.iter().all(|event_path| {
            event_matches_watched_paths(
              event_path,
              &ws.ignore,
              &ws.canonical_ignore,
              &ws.ignore_identities,
            ) || removed_event_matches_watched_paths(
              event_path,
              &ws.ignore,
              &ws.canonical_ignore,
            )
          })
        {
          continue;
        }
        if event_paths.iter().any(|event_path| {
          event_matches_watched_paths(
            event_path,
            &ws.paths,
            &ws.canonical_paths,
            &ws.identities,
          )
        }) {
          ws.send_or_record_overflow(Ok(event.clone()));
        } else if event_paths.iter().any(|event_path| {
          removed_event_matches_watched_paths(
            event_path,
            &ws.paths,
            &ws.canonical_paths,
          )
        }) {
          let remove_event = FsEvent {
            kind: "remove",
            paths: event.paths.clone(),
            flag: None,
          };
          ws.send_or_record_overflow(Ok(remove_event));
        }
      }
      // Watcher errors are not reliably path-scoped (their `paths` are
      // often empty), so deliver them to every watcher rather than
      // dropping them on the floor.
      Err(err) => {
        ws.send_or_record_overflow(Err(clone_notify_error(err)));
      }
    }
  }
}

fn ensure_watcher(
  state: &mut OpState,
) -> Result<Arc<WatcherInner>, FsEventsError> {
  if let Some(ws) = state.try_borrow::<WatcherState>() {
    return Ok(ws.inner.clone());
  }

  let senders: Arc<Mutex<SenderSet>> =
    Arc::new(Mutex::new(SenderSet::default()));
  // The queue between notify's backend thread and the dispatch thread is
  // unbounded, but per-resource delivery stays bounded
  // (send_or_record_overflow), so it only holds events for as long as
  // matching takes; the index keeps that cheap. The thread exits when the
  // watcher (and with it the callback's sender) drops.
  let (dispatch_tx, dispatch_rx) =
    std::sync::mpsc::channel::<Result<NotifyEvent, NotifyError>>();
  {
    let senders = senders.clone();
    std::thread::Builder::new()
      .name("oj-fs-events-dispatch".into())
      .spawn(move || {
        while let Ok(res) = dispatch_rx.recv() {
          dispatch_event(&senders, res);
        }
      })
      .map_err(|e| FsEventsError::Notify(JsNotifyError(NotifyError::io(e))))?;
  }
  let watcher: RecommendedWatcher = Watcher::new(
    move |res: Result<NotifyEvent, NotifyError>| {
      // Only the kind filter runs here (no syscalls); everything else is
      // the dispatch thread's.
      if let Ok(event) = &res
        && is_ignored_notify_event(event)
      {
        return;
      }
      let _ = dispatch_tx.send(res);
    },
    Default::default(),
  )
  .map_err(|e| FsEventsError::Notify(JsNotifyError(e)))?;

  let inner = Arc::new(WatcherInner {
    senders,
    watcher: Mutex::new(watcher),
    watched_paths: Mutex::new(HashMap::new()),
    next_id: AtomicU64::new(0),
  });

  state.put::<WatcherState>(WatcherState {
    inner: inner.clone(),
  });

  Ok(inner)
}

/// What actually registers with the notify backend for a watch of `path`.
/// On FSEvents (macOS) every `watch()` call tears down and recreates the
/// event stream over every path so far (notify's fsevent.rs:
/// stop/append_path/run), so registering thousands of FILES is quadratic in
/// stream rebuilds and stalls the host exactly like the dispatch scan did.
/// FSEvents is directory-grained anyway: a file watch registers its parent
/// directory instead (the refcount below dedups, so a directory of watched
/// files costs ONE registration), and the per-watcher matching that a shared
/// watcher already needs keeps delivery file-precise — chokidar watches
/// directories and filters to listeners the same way. Other backends
/// (inotify's per-inode watches are cheap and precise) register as asked.
#[cfg(target_os = "macos")]
fn watch_target(path: &Path, mode: RecursiveMode) -> (PathBuf, RecursiveMode) {
  #[allow(
    clippy::disallowed_methods,
    reason = "always using real fs with watcher"
  )]
  let is_file = path.is_file();
  if is_file
    && let Some(parent) = path.parent()
    && !parent.as_os_str().is_empty()
  {
    return (parent.to_path_buf(), RecursiveMode::NonRecursive);
  }
  (path.to_path_buf(), mode)
}

#[cfg(not(target_os = "macos"))]
fn watch_target(path: &Path, mode: RecursiveMode) -> (PathBuf, RecursiveMode) {
  (path.to_path_buf(), mode)
}

/// Make `path` absolute and collapse `.` / `..` segments so that paths
/// reported back in `FsEvent` don't carry the leftover relative bits notify
/// pastes onto its event paths (see denoland/deno#32000). Symlinks are
/// intentionally not resolved here so user-visible event paths still reflect
/// the path the caller passed in.
fn normalize_watch_path(path: PathBuf) -> PathBuf {
  if path.is_absolute() {
    return deno_path_util::normalize_path(Cow::Owned(path)).into_owned();
  }
  #[allow(
    clippy::disallowed_methods,
    reason = "fs watcher needs the real cwd to absolutize the watch path"
  )]
  let cwd = std::env::current_dir();
  match cwd {
    Ok(cwd) => {
      deno_path_util::normalize_path(Cow::Owned(cwd.join(&path))).into_owned()
    }
    Err(_) => path,
  }
}

#[op2(stack_trace)]
#[smi]
fn op_fs_events_open(
  state: &mut OpState,
  recursive: bool,
  #[scoped] ignore: Vec<String>,
  #[scoped] paths: Vec<String>,
) -> Result<ResourceId, FsEventsError> {
  let mut resolved_paths = Vec::with_capacity(paths.len());
  let mut ignore_paths = Vec::with_capacity(ignore.len());
  {
    let permissions_container = state.borrow_mut::<PermissionsContainer>();
    for path in ignore {
      let checked = permissions_container
        .check_open(
          Cow::Owned(PathBuf::from(path)),
          deno_permissions::OpenAccessKind::ReadNoFollow,
          Some("Deno.watchFs()"),
        )?
        .into_owned_path();
      ignore_paths.push(normalize_watch_path(checked));
    }
    for path in paths {
      let checked = permissions_container
        .check_open(
          Cow::Owned(PathBuf::from(path)),
          deno_permissions::OpenAccessKind::ReadNoFollow,
          Some("Deno.watchFs()"),
        )?
        .into_owned_path();
      resolved_paths.push(normalize_watch_path(checked));
    }
  }

  let (sender, receiver) =
    mpsc::channel::<Result<FsEvent, NotifyError>>(FS_EVENT_QUEUE_CAPACITY);
  let overflowed = Arc::new(AtomicBool::new(false));

  let inner = ensure_watcher(state)?;

  let id = inner.next_id.fetch_add(1, Ordering::Relaxed);

  inner.senders.lock().insert(make_watch_sender(
    id,
    resolved_paths.clone(),
    ignore_paths,
    sender,
    overflowed.clone(),
  ));

  let recursive_mode = if recursive {
    RecursiveMode::Recursive
  } else {
    RecursiveMode::NonRecursive
  };

  // Register each path with the shared watcher exactly once per
  // (path, mode) pair. Subsequent resources requesting the same
  // (path, mode) bump the refcount but skip the `watch` syscall.
  // This is the core of the duplicate-event fix on Windows (see
  // denoland/deno#27742): otherwise repeated `watch` calls register
  // duplicate ReadDirectoryChangesW operations whose callbacks all
  // fire on every change.
  let mut watched = Vec::with_capacity(resolved_paths.len());
  {
    let mut watched_paths = inner.watched_paths.lock();
    let mut watcher = inner.watcher.lock();
    for path in &resolved_paths {
      // The backend target may be coarser than the requested path (a file's
      // parent directory on FSEvents, see `watch_target`); the matching on
      // the requested paths keeps delivery exact either way.
      let key = watch_target(path, recursive_mode);
      let count = watched_paths.entry(key.clone()).or_insert(0);
      if *count == 0
        && let Err(e) = watcher.watch(&key.0, key.1)
      {
        // Roll back any partial state we accumulated for this call so
        // a failed open doesn't leave dangling refcounts/senders.
        watched_paths.remove(&key);
        drop(watcher);
        drop(watched_paths);
        rollback_partial_open(&inner, id, &watched);
        return Err(FsEventsError::Notify(JsNotifyError(e)));
      }
      *count += 1;
      watched.push(key);
    }
  }

  let resource = FsEventsResource {
    receiver: AsyncRefCell::new(receiver),
    cancel: Default::default(),
    inner,
    id,
    watched,
    overflowed,
  };
  let rid = state.resource_table.add(resource);
  Ok(rid)
}

/// Undo the `senders` push and any `watch` calls we performed before
/// hitting an error in `op_fs_events_open`. Mirrors the cleanup that
/// would run via [`FsEventsResource`]'s `Drop`, but is needed because
/// the resource itself was never constructed.
fn rollback_partial_open(
  inner: &Arc<WatcherInner>,
  id: u64,
  watched: &[(PathBuf, RecursiveMode)],
) {
  inner.senders.lock().remove(id);

  let mut watched_paths = inner.watched_paths.lock();
  let mut watcher = inner.watcher.lock();
  for (path, mode) in watched {
    let key = (path.clone(), *mode);
    if let Some(count) = watched_paths.get_mut(&key) {
      *count = count.saturating_sub(1);
      if *count == 0 {
        watched_paths.remove(&key);
        let _ = watcher.unwatch(path);
      }
    }
  }
}

#[op2]
async fn op_fs_events_poll(
  state: Rc<RefCell<OpState>>,
  #[smi] rid: ResourceId,
) -> Result<Option<FsEvent>, FsEventsError> {
  let resource = state.borrow().resource_table.get::<FsEventsResource>(rid)?;
  let mut receiver = RcRef::map(&resource, |r| &r.receiver).borrow_mut().await;

  // If the event queue overflowed since the last poll, events were dropped:
  // tell the consumer via a `"rescan"`-flagged event rather than losing them
  // silently (denoland/deno#11373). Everything still queued predates the
  // rescan and is superseded by it, so drain and discard those stale events
  // rather than delivering them after the rescan. Queued watcher errors must
  // not be lost though: deliver such an error now and re-arm the flag so the
  // next poll still reports the rescan. There is no missed-wakeup hazard
  // here: the flag can only be set while the queue is full, so a consumer
  // blocked in `recv()` below always has queued events to wake it, and the
  // flag is checked again on the next poll.
  if resource.overflowed.swap(false, Ordering::Relaxed) {
    loop {
      match receiver.try_recv() {
        Ok(Ok(_)) => continue,
        Ok(Err(err)) => {
          resource.overflowed.store(true, Ordering::Relaxed);
          return Err(FsEventsError::Notify(JsNotifyError(err)));
        }
        Err(_) => break,
      }
    }
    let paths = resource.watched.iter().map(|(p, _)| p.clone()).collect();
    return Ok(Some(FsEvent {
      kind: "any",
      paths,
      flag: Some("rescan"),
    }));
  }

  let cancel = RcRef::map(resource, |r| &r.cancel);
  let maybe_result = receiver.recv().or_cancel(cancel).await?;
  match maybe_result {
    Some(Ok(value)) => Ok(Some(value)),
    Some(Err(err)) => Err(FsEventsError::Notify(JsNotifyError(err))),
    None => Ok(None),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn notify_event(kind: EventKind) -> NotifyEvent {
    NotifyEvent {
      kind,
      paths: vec![PathBuf::from("file.txt")],
      attrs: Default::default(),
    }
  }

  #[test]
  fn ignores_notify_open_any_events() {
    let event =
      notify_event(EventKind::Access(AccessKind::Open(AccessMode::Any)));

    assert!(is_ignored_notify_event(&event));
  }

  #[test]
  fn preserves_notify_close_write_as_access() {
    let event =
      notify_event(EventKind::Access(AccessKind::Close(AccessMode::Write)));

    assert!(!is_ignored_notify_event(&event));
    assert_eq!(FsEvent::from(event).kind, "access");
  }

  #[test]
  fn maps_windows_missing_path_generic_error_to_not_found() {
    let err = JsNotifyError(NotifyError {
      kind: notify::ErrorKind::Generic(WINDOWS_PATH_NOT_FOUND_MSG.to_string()),
      paths: vec![PathBuf::from("watched/file.txt")],
    });
    assert_eq!(err.get_class(), "NotFound");

    let other = JsNotifyError(NotifyError {
      kind: notify::ErrorKind::Generic("some other failure".to_string()),
      paths: vec![],
    });
    assert_eq!(other.get_class(), GENERIC_ERROR);
  }

  // The test above only proves the match arm fires for our own constant. This
  // drives the real notify backend against a path that does not exist so a
  // notify upgrade that rewords the Windows missing-path message fails loudly
  // here instead of silently regressing fs.watch to a non-ENOENT error (see
  // denoland/deno#35855). notify's `watch_inner` returns this Generic error
  // synchronously from its is_dir/is_file check, so no real watch is opened.
  #[cfg(windows)]
  #[test]
  fn notify_windows_missing_path_message_matches_constant() {
    let mut watcher: RecommendedWatcher = Watcher::new(
      |_res: Result<NotifyEvent, NotifyError>| {},
      Default::default(),
    )
    .unwrap();

    let missing = PathBuf::from(r"C:\deno-nonexistent\watch\target");
    let err = watcher
      .watch(&missing, RecursiveMode::NonRecursive)
      .expect_err("watching a nonexistent path should fail");

    assert!(
      matches!(
        &err.kind,
        notify::ErrorKind::Generic(msg) if msg == WINDOWS_PATH_NOT_FOUND_MSG
      ),
      "notify no longer reports the expected Generic message; got {:?}",
      err.kind,
    );
    assert_eq!(JsNotifyError(err).get_class(), "NotFound");
  }

  #[test]
  fn clone_notify_error_preserves_kind_and_paths() {
    let err = NotifyError {
      kind: notify::ErrorKind::Io(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "denied",
      )),
      paths: vec![PathBuf::from("watched/dir")],
    };

    let cloned = clone_notify_error(&err);

    assert_eq!(cloned.paths, err.paths);
    match cloned.kind {
      notify::ErrorKind::Io(io_err) => {
        assert_eq!(io_err.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(io_err.to_string(), "denied");
      }
      kind => panic!("expected Io error, got {kind:?}"),
    }
  }
}
