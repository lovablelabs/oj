//! A debouncer for [notify] that is optimized for ease of use.
//!
//! * Only emits a single `Rename` event if the rename `From` and `To` events can be matched
//! * Merges multiple `Rename` events
//! * Takes `Rename` events into account and updates paths for events that occurred before the rename event, but which haven't been emitted, yet
//! * Optionally keeps track of the file system IDs all files and stitches rename events together (macOS FS Events, Windows)
//! * Emits only one `Remove` event when deleting a directory (inotify)
//! * Doesn't emit duplicate create events
//! * Doesn't emit `Modify` events after a `Create` event
//!
//! # Installation
//!
//! ```toml
//! [dependencies]
//! notify-debouncer-full = "0.5.0"
//! ```
//!
//! In case you want to select specific features of notify,
//! specify notify as dependency explicitly in your dependencies.
//! Otherwise you can just use the re-export of notify from debouncer-full.
//!
//! ```toml
//! notify-debouncer-full = "0.5.0"
//! notify = { version = "..", features = [".."] }
//! ```
//!
//! # Examples
//!
//! ```rust,no_run
//! # use std::path::Path;
//! # use std::time::Duration;
//! use notify_debouncer_full::{notify::*, new_debouncer, DebounceEventResult};
//!
//! // Select recommended watcher for debouncer.
//! // Using a callback here, could also be a channel.
//! let mut debouncer = new_debouncer(Duration::from_secs(2), None, |result: DebounceEventResult| {
//!     match result {
//!         Ok(events) => events.iter().for_each(|event| println!("{event:?}")),
//!         Err(errors) => errors.iter().for_each(|error| println!("{error:?}")),
//!     }
//! }).unwrap();
//!
//! // Add a path to be watched. All files and directories at that path and
//! // below will be monitored for changes.
//! debouncer.watch(".", WatchMode::recursive()).unwrap();
//! ```
//!
//! # Features
//!
//! The following crate features can be turned on or off in your cargo dependency config:
//!
//! - `serde` passed down to notify-types, off by default
//! - `web-time` passed down to notify-types, off by default
//! - `crossbeam-channel` passed down to notify, off by default
//! - `flume` passed down to notify, off by default
//! - `macos_fsevent` passed down to notify, off by default
//! - `macos_kqueue` passed down to notify, off by default
//! - `serialization-compat-6` passed down to notify, off by default
//!
//! # Caveats
//!
//! As all file events are sourced from notify, the [known problems](https://docs.rs/notify/latest/notify/#known-problems) section applies here too.

mod cache;
mod event_queues;
mod time;

#[cfg(test)]
mod testing;

#[cfg(not(target_family = "wasm"))]
mod file_id_map;

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use event_queues::EventQueues;
use time::now;

pub use cache::{FileIdCache, NoCache, RecommendedCache};

#[cfg(not(target_family = "wasm"))]
pub use file_id_map::FileIdMap;

pub use file_id;
pub use notify;
pub use notify_types::debouncer_full::DebouncedEvent;

use file_id::FileId;
use notify::{
    Error, ErrorKind, Event, EventKind, PathsMut, RecommendedWatcher, RecursiveMode, TargetMode,
    WatchMode, Watcher, WatcherKind,
    event::{ModifyKind, RenameMode},
};

/// The set of requirements for watcher debounce event handling functions.
///
/// # Example implementation
///
/// ```rust,no_run
/// # use notify::{Event, Result, EventHandler};
/// # use notify_debouncer_full::{DebounceEventHandler, DebounceEventResult};
///
/// /// Prints received events
/// struct EventPrinter;
///
/// impl DebounceEventHandler for EventPrinter {
///     fn handle_event(&mut self, result: DebounceEventResult) {
///         match result {
///             Ok(events) => events.iter().for_each(|event| println!("{event:?}")),
///             Err(errors) => errors.iter().for_each(|error| println!("{error:?}")),
///         }
///     }
/// }
/// ```
pub trait DebounceEventHandler: Send + 'static {
    /// Handles an event.
    fn handle_event(&mut self, event: DebounceEventResult);
}

impl<F> DebounceEventHandler for F
where
    F: FnMut(DebounceEventResult) + Send + 'static,
{
    fn handle_event(&mut self, event: DebounceEventResult) {
        (self)(event);
    }
}

#[cfg(feature = "crossbeam-channel")]
impl DebounceEventHandler for crossbeam_channel::Sender<DebounceEventResult> {
    fn handle_event(&mut self, event: DebounceEventResult) {
        let result = self.send(event);
        if let Err(e) = result {
            tracing::error!(?e, "failed to send debounce event result");
        }
    }
}

#[cfg(feature = "flume")]
impl DebounceEventHandler for flume::Sender<DebounceEventResult> {
    fn handle_event(&mut self, event: DebounceEventResult) {
        let result = self.send(event);
        if let Err(e) = result {
            tracing::error!(?e, "failed to send debounce event result");
        }
    }
}

impl DebounceEventHandler for std::sync::mpsc::Sender<DebounceEventResult> {
    fn handle_event(&mut self, event: DebounceEventResult) {
        let result = self.send(event);
        if let Err(e) = result {
            tracing::error!(?e, "failed to send debounce event result");
        }
    }
}

/// A result of debounced events.
/// Comes with either a vec of events or vec of errors.
pub type DebounceEventResult = Result<Vec<DebouncedEvent>, Vec<Error>>;

type DebounceData<T> = Arc<SharedDebounceData<T>>;

/// oj (upstream notify-rs/notify#933, in notify-debouncer-full 0.8.0): the debouncer
/// thread waits on `changed` while nothing is queued, instead of waking once a tick.
#[derive(Debug)]
struct SharedDebounceData<T> {
    inner: Mutex<DebounceDataInner<T>>,
    changed: Condvar,
}

#[derive(Debug)]
pub(crate) struct DebounceDataInner<T> {
    queues: EventQueues,
    /// Registered watch roots, kept **sorted by path** so that `add_root`
    /// can dedupe via binary search in O(log N) and doesn't suffer from injection
    roots: VecDeque<(PathBuf, WatchMode)>,
    cache: T,
    rename_event: Option<(DebouncedEvent, Option<FileId>)>,
    rescan_event: Option<DebouncedEvent>,
    errors: Vec<Error>,
    timeout: Duration,
}

impl<T: FileIdCache> DebounceDataInner<T> {
    pub(crate) fn new(cache: T, timeout: Duration) -> Self {
        Self {
            queues: EventQueues::default(),
            roots: VecDeque::new(),
            cache,
            rename_event: None,
            rescan_event: None,
            errors: Vec::new(),
            timeout,
        }
    }

    /// oj: nothing for the debouncer thread to report now or later, so it can wait for
    /// the watcher instead of ticking. A rename `From` kept in `rename_event` for its
    /// `To` is also in the queues, so it needs no term here.
    fn is_idle(&self) -> bool {
        self.queues.is_empty() && self.errors.is_empty() && self.rescan_event.is_none()
    }

    /// Retrieve a vec of debounced events, removing them if not continuous
    pub fn debounced_events(&mut self) -> Vec<DebouncedEvent> {
        self.queues
            .debounced_events(&mut self.rescan_event, self.timeout)
    }

    /// Returns all currently stored errors
    pub fn errors(&mut self) -> Vec<Error> {
        std::mem::take(&mut self.errors)
    }

    /// Add an error entry to re-send later on
    pub fn add_error(&mut self, error: Error) {
        tracing::trace!("raw error: {error:?}");

        self.errors.push(error);
    }

    /// Add new event to debouncer cache
    pub fn add_event(&mut self, event: Event) {
        tracing::trace!("raw event: {event:?}");

        if event.need_rescan() {
            let roots = self.roots.make_contiguous();
            self.cache.rescan(roots);
            self.rescan_event = Some(DebouncedEvent { event, time: now() });
            return;
        }

        let Some(path) = event.paths.first() else {
            tracing::info!("skipping event with no paths: {event:?}");
            return;
        };

        match &event.kind {
            EventKind::Create(_) => {
                let watch_mode = self.watch_mode(path);

                self.cache.add_path(path, watch_mode);

                self.queues.push_event(event, now());
            }
            EventKind::Modify(ModifyKind::Name(rename_mode)) => {
                #[expect(clippy::match_same_arms)]
                match rename_mode {
                    RenameMode::Any => {
                        if event.paths[0].exists() {
                            self.handle_rename_to(event);
                        } else {
                            self.handle_rename_from(event);
                        }
                    }
                    RenameMode::To => {
                        self.handle_rename_to(event);
                    }
                    RenameMode::From => {
                        self.handle_rename_from(event);
                    }
                    RenameMode::Both => {
                        // ignore and handle `To` and `From` events instead
                    }
                    RenameMode::Other => {
                        // unused
                    }
                }
            }
            EventKind::Remove(_) => {
                let time = now();
                // remove cached file ids
                self.cache.remove_path(&event.paths[0]);
                self.queues.push_remove_event(event, time);
            }
            EventKind::Other => {
                // ignore meta events
            }
            _ => {
                if self.cache.cached_file_id(path).is_none() {
                    let watch_mode = self.watch_mode(path);

                    self.cache.add_path(path, watch_mode);
                }

                self.queues.push_event(event, now());
            }
        }
    }

    fn watch_mode(&self, path: &Path) -> WatchMode {
        for ancestor in path.ancestors() {
            if let Ok(index) = self
                .roots
                .binary_search_by(|(root, _)| root.as_path().cmp(ancestor))
            {
                return self.roots[index].1;
            }
        }

        WatchMode {
            recursive_mode: RecursiveMode::NonRecursive,
            target_mode: TargetMode::TrackPath, // TODO: correct default?
        }
    }

    fn handle_rename_from(&mut self, event: Event) {
        let time = now();
        let path = &event.paths[0];

        // store event
        let file_id = self.cache.cached_file_id(path).map(|id| *id.as_ref());
        self.rename_event = Some((DebouncedEvent::new(event.clone(), time), file_id));

        self.cache.remove_path(path);

        self.queues.push_event(event, time);
    }

    fn handle_rename_to(&mut self, event: Event) {
        let watch_mode = self.watch_mode(&event.paths[0]);

        self.cache.add_path(&event.paths[0], watch_mode);

        let trackers_match = self
            .rename_event
            .as_ref()
            .and_then(|(e, _)| e.tracker())
            .zip(event.attrs.tracker())
            .is_some_and(|(from, to)| from == to);

        let file_ids_match = self
            .rename_event
            .as_ref()
            .and_then(|(_, id)| id.as_ref())
            .and_then(|from_file_id| {
                self.cache
                    .cached_file_id(&event.paths[0])
                    .map(|to_file_id| from_file_id == to_file_id.as_ref())
            })
            .unwrap_or_default();

        if trackers_match || file_ids_match {
            // connect rename
            let (mut rename_event, _) = self.rename_event.take().unwrap(); // unwrap is safe because `rename_event` must be set at this point
            let path = rename_event.paths.remove(0);
            self.cache.remove_path(&path);
            self.queues
                .push_rename_event(path, event, rename_event.time);
        } else {
            // move in
            self.queues.push_event(event, now());
        }

        self.rename_event = None;
    }
}

/// Debouncer guard, stops the debouncer on drop.
#[derive(Debug)]
pub struct Debouncer<T: Watcher, C: FileIdCache> {
    watcher: T,
    debouncer_thread: Option<std::thread::JoinHandle<()>>,
    data: DebounceData<C>,
    stop: Arc<AtomicBool>,
    ignore_filter: notify::IgnoreFilter,
}

impl<T: Watcher, C: FileIdCache> Debouncer<T, C> {
    /// Stop the debouncer, waking the event thread and waiting for it to finish.
    /// May block for the duration of one `tick_rate`.
    pub fn stop(mut self) {
        self.set_stop();
        if let Some(t) = self.debouncer_thread.take() {
            let result = t.join();
            if let Err(e) = result {
                tracing::error!(?e, "failed to join debouncer thread");
            }
        }
    }

    /// Stop the debouncer, does not wait for the event thread to finish.
    pub fn stop_nonblocking(self) {
        self.set_stop();
    }

    fn set_stop(&self) {
        // oj: under the lock the thread checks the flag under, so the store cannot fall
        // between its check and its wait; then wake it from that wait. A lock poisoned
        // by a panic under it still serves: this runs from `Drop`, where a panic of its
        // own would abort a process already unwinding.
        let _lock = self
            .data
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.stop.store(true, Ordering::Relaxed);
        self.data.changed.notify_all();
    }

    #[deprecated = "`Debouncer` provides all methods from `Watcher` itself now. Remove `.watcher()` and use those methods directly."]
    pub fn watcher(&mut self) {}

    #[deprecated = "`Debouncer` now manages root paths automatically. Remove all calls to `add_root` and `remove_root`."]
    pub fn cache(&mut self) {}

    fn add_root(&self, path: impl Into<PathBuf>, watch_mode: WatchMode) {
        let path = path.into();

        let mut data = self.data.inner.lock().unwrap();

        match data
            .roots
            .binary_search_by(|(p, _)| p.as_path().cmp(path.as_path()))
        {
            Ok(_) => return, // already registered
            Err(pos) => {
                // `VecDeque::insert` is O(min(pos, len - pos))
                data.roots.insert(pos, (path.clone(), watch_mode));
            }
        }

        data.cache.add_path(&path, watch_mode);
    }

    fn remove_root(&self, path: impl AsRef<Path>) {
        let mut data = self.data.inner.lock().unwrap();

        data.roots.retain(|(root, _)| !root.starts_with(&path));

        data.cache.remove_path(path.as_ref());
    }

    pub fn watch(&mut self, path: impl AsRef<Path>, watch_mode: WatchMode) -> notify::Result<()> {
        self.watcher.watch(path.as_ref(), watch_mode)?;
        // the watcher does not watch an ignored path, so it is not a root
        if !self.ignore_filter.is_path_ignored(path.as_ref()) {
            self.add_root(path.as_ref(), watch_mode);
        }
        Ok(())
    }

    pub fn unwatch(&mut self, path: impl AsRef<Path>) -> notify::Result<()> {
        self.watcher.unwatch(path.as_ref())?;
        self.remove_root(path);
        Ok(())
    }

    pub fn paths_mut<'me>(&'me mut self) -> Box<dyn PathsMut + 'me> {
        self.watcher.paths_mut()
    }

    pub fn configure(&mut self, option: notify::Config) -> notify::Result<bool> {
        self.watcher.configure(option)
    }

    #[must_use]
    pub fn kind() -> WatcherKind
    where
        Self: Sized,
    {
        T::kind()
    }
}

impl<T: Watcher, C: FileIdCache> Drop for Debouncer<T, C> {
    fn drop(&mut self) {
        self.set_stop();
    }
}

/// Creates a new debounced watcher with custom configuration.
///
/// Timeout is the amount of time after which a debounced event is emitted.
///
/// If `tick_rate` is `None`, notify will select a tick rate that is 1/4 of the provided timeout.
///
/// # Panics
/// Panics if the internal mutex is poisoned.
#[tracing::instrument(level = "debug", skip(event_handler, file_id_cache))]
pub fn new_debouncer_opt<F: DebounceEventHandler, T: Watcher, C: FileIdCache + Send + 'static>(
    timeout: Duration,
    tick_rate: Option<Duration>,
    mut event_handler: F,
    mut file_id_cache: C,
    config: notify::Config,
) -> Result<Debouncer<T, C>, Error> {
    let ignore_filter = config.ignored().clone();
    file_id_cache.set_ignore_filter(ignore_filter.clone());
    let data = Arc::new(SharedDebounceData {
        inner: Mutex::new(DebounceDataInner::new(file_id_cache, timeout)),
        changed: Condvar::new(),
    });
    let stop = Arc::new(AtomicBool::new(false));

    let tick_div = 4;
    let tick = match tick_rate {
        Some(v) => {
            if v > timeout {
                return Err(Error::new(ErrorKind::Generic(format!(
                    "Invalid tick_rate, tick rate {v:?} > {timeout:?} timeout!"
                ))));
            }
            v
        }
        None => timeout.checked_div(tick_div).ok_or_else(|| {
            Error::new(ErrorKind::Generic(format!(
                "Failed to calculate tick as {timeout:?}/{tick_div}!"
            )))
        })?,
    };

    // oj: the watcher before the thread, so a watcher that cannot be made leaves no
    // thread behind (upstream notify-rs/notify#1044).
    let data_c = Arc::clone(&data);
    let watcher = T::new(
        move |e: Result<Event, Error>| {
            let mut lock = data_c.inner.lock().unwrap();
            let was_idle = lock.is_idle();
            match e {
                Ok(e) => lock.add_event(e),
                // can't have multiple TX, so we need to pipe that through our debouncer
                Err(e) => lock.add_error(e),
            }
            // The worker only waits while idle. Once work is pending, it already
            // wakes on its debounce tick, so more events need no notification.
            if was_idle && !lock.is_idle() {
                data_c.changed.notify_one();
            }
        },
        config,
    )?;

    let data_c = Arc::clone(&data);
    let stop_c = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("notify-rs debouncer loop".to_string())
        .spawn(move || {
            loop {
                // oj: asleep until the watcher queues something or the debouncer stops;
                // the tick runs only while something is queued.
                let mut lock = data_c.inner.lock().unwrap();
                while lock.is_idle() && !stop_c.load(Ordering::Acquire) {
                    lock = data_c.changed.wait(lock).unwrap();
                }
                if stop_c.load(Ordering::Acquire) {
                    break;
                }
                drop(lock);
                std::thread::sleep(tick);
                if stop_c.load(Ordering::Acquire) {
                    break;
                }
                lock = data_c.inner.lock().unwrap();
                let send_data = lock.debounced_events();
                let errors = lock.errors();
                drop(lock);
                if !send_data.is_empty() {
                    event_handler.handle_event(Ok(send_data));
                }
                if !errors.is_empty() {
                    event_handler.handle_event(Err(errors));
                }
            }
        })?;

    let guard = Debouncer {
        watcher,
        debouncer_thread: Some(thread),
        data,
        stop,
        ignore_filter,
    };

    Ok(guard)
}

/// Short function to create a new debounced watcher with the recommended debouncer and the built-in file ID cache.
///
/// Timeout is the amount of time after which a debounced event is emitted.
///
/// If `tick_rate` is `None`, notify will select a tick rate that is 1/4 of the provided timeout.
pub fn new_debouncer<F: DebounceEventHandler>(
    timeout: Duration,
    tick_rate: Option<Duration>,
    event_handler: F,
) -> Result<Debouncer<RecommendedWatcher, RecommendedCache>, Error> {
    new_debouncer_opt::<F, RecommendedWatcher, RecommendedCache>(
        timeout,
        tick_rate,
        event_handler,
        RecommendedCache::new(),
        notify::Config::default(),
    )
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::Path,
        time::{Duration, Instant},
    };

    use super::*;

    use pretty_assertions::assert_eq;
    use rstest::rstest;
    use tempfile::tempdir;
    use testing::TestCase;
    use time::MockTime;

    #[test]
    fn file_id_cache_gets_the_ignore_filter_of_the_watcher() {
        struct Cache(Arc<Mutex<notify::IgnoreFilter>>);

        impl FileIdCache for Cache {
            fn cached_file_id(&self, _path: &Path) -> Option<impl AsRef<FileId>> {
                Option::<&FileId>::None
            }

            fn add_path(&mut self, _path: &Path, _watch_mode: WatchMode) {}

            fn remove_path(&mut self, _path: &Path) {}

            fn set_ignore_filter(&mut self, ignore_filter: notify::IgnoreFilter) {
                *self.0.lock().unwrap() = ignore_filter;
            }
        }

        let config = notify::Config::default().with_ignored(|_, _| true);
        let ignore_filter = Arc::default();
        let _debouncer = new_debouncer_opt::<_, notify::NullWatcher, _>(
            Duration::from_millis(10),
            None,
            |_: DebounceEventResult| {},
            Cache(Arc::clone(&ignore_filter)),
            config,
        )
        .unwrap();

        // the default filter ignores nothing, so this is the filter of the config
        let ignore_filter = ignore_filter.lock().unwrap();
        assert!(ignore_filter.is_ignored(Path::new("path"), notify::EntryKind::Unknown));
    }

    #[test]
    fn ignored_path_is_not_a_root() {
        // a cache that does not use the ignore filter
        struct Cache(Arc<Mutex<Vec<PathBuf>>>);

        impl FileIdCache for Cache {
            fn cached_file_id(&self, _path: &Path) -> Option<impl AsRef<FileId>> {
                Option::<&FileId>::None
            }

            fn add_path(&mut self, path: &Path, _watch_mode: WatchMode) {
                self.0.lock().unwrap().push(path.to_path_buf());
            }

            fn remove_path(&mut self, _path: &Path) {}
        }

        let config = notify::Config::default().with_ignored(|path, _| {
            path.components()
                .any(|component| component.as_os_str() == "node_modules")
        });
        let added = Arc::default();
        let mut debouncer = new_debouncer_opt::<_, notify::NullWatcher, _>(
            Duration::from_millis(10),
            None,
            |_: DebounceEventResult| {},
            Cache(Arc::clone(&added)),
            config,
        )
        .unwrap();

        let ignored = Path::new("/project/node_modules/pkg");
        let src = Path::new("/project/src");
        debouncer.watch(ignored, WatchMode::recursive()).unwrap();
        debouncer.watch(src, WatchMode::recursive()).unwrap();

        assert_eq!(*added.lock().unwrap(), [src]);
        assert_eq!(
            debouncer.data.inner.lock().unwrap().roots,
            [(src.to_path_buf(), WatchMode::recursive())]
        );
    }

    #[rstest]
    fn state(
        #[values(
            "add_create_and_remove_event",
            "add_create_event",
            "add_create_event_after_remove_event",
            "add_create_dir_event_twice",
            "add_event_with_no_paths_is_ok",
            "add_modify_any_event_after_create_event",
            "add_modify_content_event_after_create_event",
            "add_rename_from_event",
            "add_rename_from_event_after_create_event",
            "add_rename_from_event_after_modify_event",
            "add_rename_from_event_after_create_and_modify_event",
            "add_rename_from_event_after_rename_from_event",
            "add_rename_to_event",
            "add_rename_to_dir_event",
            "add_rename_from_and_to_event",
            "add_rename_from_and_to_event_after_create",
            "add_rename_from_and_to_event_after_rename",
            "add_rename_from_and_to_event_after_modify_content",
            "add_rename_from_and_to_event_override_created",
            "add_rename_from_and_to_event_override_modified",
            "add_rename_from_and_to_event_override_removed",
            "add_rename_from_and_to_event_with_file_ids",
            "add_rename_from_and_to_event_with_different_file_ids",
            "add_rename_from_and_to_event_with_different_tracker",
            "add_rename_both_event",
            "add_remove_event",
            "add_remove_event_after_create_event",
            "add_remove_event_after_modify_event",
            "add_remove_event_after_create_and_modify_event",
            "add_remove_parent_event_after_remove_child_event",
            "add_errors",
            "debounce_modify_events",
            "emit_continuous_modify_content_events",
            "emit_events_in_chronological_order",
            "emit_events_with_a_prepended_rename_event",
            "emit_close_events_only_once",
            "emit_modify_event_after_close_event",
            "emit_needs_rescan_event",
            "read_file_id_without_create_event",
            "sort_events_chronologically",
            "sort_events_with_reordering"
        )]
        file_name: &str,
    ) {
        let file_content =
            fs::read_to_string(Path::new(&format!("./test_cases/{file_name}.hjson"))).unwrap();
        let mut test_case = deser_hjson::from_str::<TestCase>(&file_content).unwrap();

        let time = now();
        MockTime::set_time(time);

        let mut state = test_case.state.into_debounce_data_inner(time);
        state.roots = VecDeque::from([(PathBuf::from("/"), WatchMode::recursive())]);

        let mut prev_event_time = Duration::default();

        for event in test_case.events {
            let event_time = Duration::from_millis(event.time);
            let event = event.into_debounced_event(time, None);
            MockTime::advance(
                event_time
                    .checked_sub(prev_event_time)
                    .expect("prev_event_time be smaller"),
            );
            prev_event_time = event_time;
            state.add_event(event.event);
        }

        for error in test_case.errors {
            let error = error.into_notify_error();
            state.add_error(error);
        }

        let expected_errors = std::mem::take(&mut test_case.expected.errors);
        let expected_events = std::mem::take(&mut test_case.expected.events);
        let expected_state = test_case.expected.into_debounce_data_inner(time);
        assert_eq!(
            state.queues, expected_state.queues,
            "queues not as expected"
        );
        assert_eq!(
            state.rename_event, expected_state.rename_event,
            "rename event not as expected"
        );
        assert_eq!(
            state.rescan_event, expected_state.rescan_event,
            "rescan event not as expected"
        );
        assert_eq!(
            state.cache.paths, expected_state.cache.paths,
            "cache not as expected"
        );

        assert_eq!(
            state
                .errors
                .iter()
                .map(|e| format!("{e:?}"))
                .collect::<Vec<_>>(),
            expected_errors
                .iter()
                .map(|e| format!("{:?}", e.clone().into_notify_error()))
                .collect::<Vec<_>>(),
            "errors not as expected"
        );

        let backup_time = now();
        let backup_queues = state.queues.clone();

        for (delay, events) in expected_events {
            MockTime::set_time(backup_time);
            state.queues = backup_queues.clone();

            match delay.as_str() {
                "none" => {}
                "short" => MockTime::advance(Duration::from_millis(10)),
                "long" => MockTime::advance(Duration::from_millis(100)),
                _ => {
                    if let Ok(ts) = delay.parse::<u64>() {
                        MockTime::set_time(time + Duration::from_millis(ts));
                    }
                }
            }

            let events = events
                .into_iter()
                .map(|event| event.into_debounced_event(time, None))
                .collect::<Vec<_>>();

            assert_eq!(
                state.debounced_events(),
                events,
                "debounced events after a `{delay}` delay"
            );
        }
    }

    #[test]
    fn recursive_mode_uses_recursive_root_for_overlapping_watches() {
        let state = DebounceDataInner {
            queues: EventQueues::default(),
            roots: VecDeque::from([
                (PathBuf::from("root"), WatchMode::non_recursive()),
                (PathBuf::from("root/nested"), WatchMode::recursive()),
            ]),
            cache: NoCache,
            rename_event: None,
            rescan_event: None,
            errors: Vec::new(),
            timeout: Duration::from_millis(50),
        };

        assert_eq!(
            state.watch_mode(Path::new("root/nested/child")),
            WatchMode::recursive()
        );
        assert_eq!(
            state.watch_mode(Path::new("root/other")),
            WatchMode::non_recursive()
        );
    }

    #[expect(clippy::print_stdout)]
    #[test]
    fn integration() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;

        // set up the watcher
        let (tx, rx) = std::sync::mpsc::channel();
        let mut debouncer = new_debouncer(Duration::from_millis(10), None, tx)?;
        debouncer.watch(dir.path(), WatchMode::recursive())?;

        // create a new file
        let file_path = dir.path().join("file.txt");
        fs::write(&file_path, b"Lorem ipsum")?;

        println!("waiting for event at {}", file_path.display());

        // wait for up to 10 seconds for the create event, ignore all other events
        let deadline = Instant::now() + Duration::from_secs(10);
        while deadline > Instant::now() {
            let events = rx
                .recv_timeout(deadline - Instant::now())
                .expect("did not receive expected event")
                .expect("received an error");

            for event in events {
                if event.event.paths == vec![file_path.clone()]
                    || event.event.paths == vec![file_path.canonicalize()?]
                {
                    return Ok(());
                }

                println!("unexpected event: {event:?}");
            }
        }

        panic!("did not receive expected event");
    }

    // oj: a watcher whose callback the test holds, to feed the debouncer an event
    // the way the file system would.
    struct HeldWatcher;

    static HELD: Mutex<Option<Box<dyn FnMut(Result<Event, Error>) + Send>>> = Mutex::new(None);

    impl Watcher for HeldWatcher {
        fn new<F: notify::EventHandler>(
            mut handler: F,
            _config: notify::Config,
        ) -> notify::Result<Self> {
            *HELD.lock().unwrap() = Some(Box::new(move |e| handler.handle_event(e)));
            Ok(Self)
        }

        fn watch(&mut self, _path: &Path, _watch_mode: WatchMode) -> notify::Result<()> {
            Ok(())
        }

        fn unwatch(&mut self, _path: &Path) -> notify::Result<()> {
            Ok(())
        }

        fn kind() -> WatcherKind {
            WatcherKind::NullWatcher
        }
    }

    // oj: the thread waits for the watcher while nothing is queued, and an event the
    // watcher queues into the empty debouncer wakes it and is reported after the delay.
    #[test]
    fn an_event_wakes_the_idle_thread() {
        let (tx, rx) = std::sync::mpsc::channel();
        let debouncer = new_debouncer_opt::<_, HeldWatcher, NoCache>(
            Duration::from_millis(10),
            None,
            tx,
            NoCache::new(),
            notify::Config::default(),
        )
        .unwrap();
        // Long enough for a ticking thread to have looked many times.
        std::thread::sleep(Duration::from_millis(100));
        assert!(rx.try_recv().is_err());
        let path = PathBuf::from("/held/file.txt");
        let started = Instant::now();
        (HELD.lock().unwrap().as_mut().unwrap())(Ok(Event::new(EventKind::Create(
            notify::event::CreateKind::File,
        ))
        .add_path(path.clone())));
        let events = rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].paths, [path]);
        assert!(started.elapsed() >= Duration::from_millis(10));
        debouncer.stop();
    }

    // oj: `stop` wakes the thread from its wait, so an idle debouncer stops at once,
    // not after a tick. The tick here is 2.5 s.
    #[test]
    fn an_idle_debouncer_stops_at_once() {
        let (tx, _rx) = std::sync::mpsc::channel::<DebounceEventResult>();
        let debouncer = new_debouncer_opt::<_, notify::NullWatcher, NoCache>(
            Duration::from_secs(10),
            None,
            tx,
            NoCache::new(),
            notify::Config::default(),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        debouncer.stop();
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
