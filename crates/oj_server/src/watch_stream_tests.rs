use super::*;
use notify::RecursiveMode::{NonRecursive, Recursive};
use std::sync::Mutex;
use std::time::Duration;

fn paths(list: &[&str]) -> Vec<PathBuf> {
    list.iter().map(PathBuf::from).collect()
}

/// FSEvents behind notify, in memory, as `Watchers` meets it. Each backend
/// opened is one stream: it watches real paths, recursively or flat, never
/// follows a symlink under them, and reports each path spelled as its watch
/// was (notify's `reported_path`). A watch the stream does not already cover
/// restarts it, as notify's does. Events are delivered synchronously.
#[derive(Clone, Default)]
struct FakeFsEvents(Arc<Mutex<Vec<FakeStream>>>);

struct FakeStream {
    sink: EventSink,
    watches: Vec<FakeWatch>,
    restarts: usize,
}

struct FakeWatch {
    spelled: PathBuf,
    real: PathBuf,
    mode: notify::RecursiveMode,
}

impl FakeWatch {
    /// `real_path` as this watch reports it, if the stream sees it here.
    fn report(&self, real_path: &Path) -> Option<PathBuf> {
        let rest = real_path.strip_prefix(&self.real).ok()?;
        if self.mode == NonRecursive && rest.components().count() > 1 {
            return None;
        }
        Some(match rest.as_os_str().is_empty() {
            true => self.spelled.clone(),
            false => self.spelled.join(rest),
        })
    }
}

struct FakeBackend {
    streams: FakeFsEvents,
    id: usize,
}

impl WatchBackend for FakeBackend {
    fn watch(&mut self, path: &Path, mode: notify::RecursiveMode) -> notify::Result<()> {
        let real = std::fs::canonicalize(path).map_err(notify::Error::io)?;
        let mut streams = self.streams.0.lock().unwrap();
        let stream = &mut streams[self.id];
        let covered = stream
            .watches
            .iter()
            .any(|w| w.mode == Recursive && real.starts_with(&w.real));
        if !covered && !stream.watches.is_empty() {
            stream.restarts += 1;
        }
        stream.watches.push(FakeWatch {
            spelled: path.to_path_buf(),
            real,
            mode,
        });
        Ok(())
    }
}

/// The stream every `Watchers` opens first.
const ROOT: usize = 0;

impl FakeFsEvents {
    /// `Watchers` on these streams, placing them as on macOS on every
    /// platform, and the events that reach its sink.
    fn watchers(&self) -> (Watchers, std::sync::mpsc::Receiver<notify::Event>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = Mutex::new(tx);
        let sink: EventSink = Arc::new(move |ev: notify::Result<notify::Event>| {
            let _ = tx.lock().unwrap().send(ev.unwrap());
        });
        let streams = self.clone();
        let open: OpenBackend = Box::new(move |sink| {
            let mut all = streams.0.lock().unwrap();
            all.push(FakeStream {
                sink,
                watches: Vec::new(),
                restarts: 0,
            });
            Ok(Box::new(FakeBackend {
                streams: streams.clone(),
                id: all.len() - 1,
            }))
        });
        (Watchers::with_backends(sink, open, true).unwrap(), rx)
    }

    /// A write to `real_path`, through every stream that sees it.
    fn write(&self, real_path: &Path) {
        let ev = notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Content,
        )));
        self.deliver(real_path, ev);
    }

    /// FSEvents' MustScanSubDirs at `real_path`, which it reports at a
    /// stream's path when the stream dropped events.
    fn lose_events_at(&self, real_path: &Path) {
        let ev = notify::Event::new(notify::EventKind::Other).set_flag(notify::event::Flag::Rescan);
        self.deliver(real_path, ev);
    }

    fn deliver(&self, real_path: &Path, ev: notify::Event) {
        let sends: Vec<(EventSink, PathBuf)> = self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|s| {
                let at = s.watches.iter().find_map(|w| w.report(real_path))?;
                Some((Arc::clone(&s.sink), at))
            })
            .collect();
        for (sink, at) in sends {
            let mut ev = ev.clone();
            ev.paths = vec![at];
            sink(Ok(ev));
        }
    }

    fn count(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    fn watched(&self, id: usize) -> Vec<PathBuf> {
        self.0.lock().unwrap()[id]
            .watches
            .iter()
            .map(|w| w.spelled.clone())
            .collect()
    }

    fn restarts(&self, id: usize) -> usize {
        self.0.lock().unwrap()[id].restarts
    }
}

/// Every path the sink got so far, one entry per event path.
fn received(rx: &std::sync::mpsc::Receiver<notify::Event>) -> Vec<PathBuf> {
    rx.try_iter().flat_map(|ev| ev.paths).collect()
}

/// A canonical temp dir holding `dirs`, created.
fn tree(dirs: impl IntoIterator<Item = String>) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().canonicalize().unwrap();
    for d in dirs {
        std::fs::create_dir_all(ws.join(d)).unwrap();
    }
    (tmp, ws)
}

// chokidar's fsevents handler: a dir under a watched one joins its stream,
// the eleventh sibling consolidates onto the parent, and past 128 streams
// the overflow watcher takes it.
#[test]
fn served_dirs_share_streams_as_chokidars_fsevents_handler_places_them() {
    let none: Vec<&Path> = Vec::new();
    assert_eq!(
        stream_for(&none, Path::new("/ws/pkg/a")),
        StreamPlan::Open(PathBuf::from("/ws/pkg/a"))
    );

    let one = paths(&["/ws/pkg/a"]);
    let one: Vec<&Path> = one.iter().map(PathBuf::as_path).collect();
    assert_eq!(
        stream_for(&one, Path::new("/ws/pkg/a/src/lib")),
        StreamPlan::Join(PathBuf::from("/ws/pkg/a")),
        "a nested dir rides the stream above it"
    );
    assert_eq!(
        stream_for(&one, Path::new("/ws/pkg/ab")),
        StreamPlan::Open(PathBuf::from("/ws/pkg/ab")),
        "a name prefix is not a parent"
    );

    let nine: Vec<PathBuf> = (0..9)
        .map(|i| PathBuf::from(format!("/ws/pkg/p{i}")))
        .collect();
    let nine: Vec<&Path> = nine.iter().map(PathBuf::as_path).collect();
    assert_eq!(
        stream_for(&nine, Path::new("/ws/pkg/p9")),
        StreamPlan::Open(PathBuf::from("/ws/pkg/p9"))
    );
    let ten: Vec<PathBuf> = (0..10)
        .map(|i| PathBuf::from(format!("/ws/pkg/p{i}")))
        .collect();
    let ten: Vec<&Path> = ten.iter().map(PathBuf::as_path).collect();
    assert_eq!(
        stream_for(&ten, Path::new("/ws/pkg/p10")),
        StreamPlan::Open(PathBuf::from("/ws/pkg")),
        "the eleventh sibling watches the parent"
    );

    let full: Vec<PathBuf> = (0..MAX_OWN_STREAMS)
        .map(|i| PathBuf::from(format!("/ws/d{i}/x")))
        .collect();
    let full: Vec<&Path> = full.iter().map(PathBuf::as_path).collect();
    assert_eq!(
        stream_for(&full, Path::new("/other/y")),
        StreamPlan::Overflow
    );
    assert_eq!(
        stream_for(&full, Path::new("/ws/d3/x/deep")),
        StreamPlan::Join(PathBuf::from("/ws/d3/x")),
        "a covered dir still joins past the cap"
    );
}

fn dirs(list: &[(&str, &str, notify::RecursiveMode)]) -> StreamDirs {
    list.iter()
        .map(|(real, spelled, mode)| (PathBuf::from(real), (PathBuf::from(spelled), *mode)))
        .collect()
}

// A shared stream passes only its watched dirs' events: a flat watch keeps
// the dir and its direct children, a recursive one everything under it.
// Each path is spelled as its dir was watched, a symlink's real path not.
#[test]
fn a_shared_stream_passes_only_its_watched_dirs_events() {
    let dirs = dirs(&[
        ("/ws/pkg/a", "/ws/pkg/a", NonRecursive),
        ("/ws/pkg/b", "/ws/pkg/b", Recursive),
        ("/real/c", "/ws/pkg/c", NonRecursive),
    ]);
    let at = |p: &str| watched_as(&dirs, Path::new(p));
    assert_eq!(at("/ws/pkg/a"), Some(PathBuf::from("/ws/pkg/a")));
    assert_eq!(at("/ws/pkg/a/x.ts"), Some(PathBuf::from("/ws/pkg/a/x.ts")));
    assert_eq!(at("/ws/pkg/a/sub/x.ts"), None);
    assert_eq!(
        at("/ws/pkg/b/sub/deep/x.ts"),
        Some(PathBuf::from("/ws/pkg/b/sub/deep/x.ts"))
    );
    assert_eq!(at("/real/c/x.ts"), Some(PathBuf::from("/ws/pkg/c/x.ts")));
    assert_eq!(at("/ws/pkg/d/x.ts"), None);
    assert_eq!(at("/ws/pkg/x.ts"), None);
}

// 300 sibling package dirs: ten streams of their own, then one on their
// parent for the rest. Each edit arrives once, an unwatched sibling's and a
// flat-watched dir's subdir's never, and the root stream is never touched.
#[test]
fn sibling_dirs_consolidate_onto_their_parent() {
    let (_tmp, ws) = tree(
        (0..300)
            .map(|i| format!("pkg{i}/sub"))
            .chain(["other".into()]),
    );
    let fs = FakeFsEvents::default();
    let (mut watchers, rx) = fs.watchers();
    for i in 0..300 {
        watchers
            .watch(&ws.join(format!("pkg{i}")), NonRecursive)
            .unwrap();
    }
    assert_eq!(watchers.own.len(), CONSOLIDATE_AT + 1);
    assert!(watchers.own.contains_key(&ws));
    assert!(watchers.overflow.is_none());
    assert!(fs.watched(ROOT).is_empty());

    for i in [0, 9, 10, 299] {
        let edited = ws.join(format!("pkg{i}/x.ts"));
        fs.write(&edited);
        assert_eq!(received(&rx), [edited]);
    }
    fs.write(&ws.join("other/x.ts"));
    fs.write(&ws.join("pkg299/sub/x.ts"));
    fs.write(&ws.join("pkg0/sub/x.ts"));
    assert_eq!(received(&rx), Vec::<PathBuf>::new());
}

// Dirs that never share a parent fill the 128 streams; the rest go to one
// overflow watcher, so they never restart the root watcher's stream.
#[test]
fn scattered_dirs_past_the_cap_never_touch_the_root_stream() {
    let (_tmp, ws) = tree((0..300).map(|i| format!("packages/p{i}/src")));
    let fs = FakeFsEvents::default();
    let (mut watchers, rx) = fs.watchers();
    for i in 0..300 {
        watchers
            .watch(&ws.join(format!("packages/p{i}/src")), NonRecursive)
            .unwrap();
    }
    assert_eq!(watchers.own.len(), MAX_OWN_STREAMS);
    assert!(
        fs.watched(ROOT).is_empty(),
        "the root stream took served dirs"
    );
    assert_eq!(fs.restarts(ROOT), 0);
    let overflow = fs.count() - 1;
    assert_eq!(fs.watched(overflow).len(), 300 - MAX_OWN_STREAMS);

    for i in [0, 127, 128, 299] {
        let edited = ws.join(format!("packages/p{i}/src/x.ts"));
        fs.write(&edited);
        assert_eq!(received(&rx), [edited]);
    }
}

// A served dir reached through a symlink lives elsewhere on disk: a stream
// on its spelled parent never sees it, so it gets a stream on its real
// path, and its events come back under the spelling it was watched as.
#[test]
fn a_dir_behind_a_symlink_gets_a_stream_on_its_real_path() {
    let (_tmp, ws) = tree((0..11).map(|i| format!("pkg{i}")));
    let (_elsewhere, real) = tree(["src".to_string()]);
    std::os::unix::fs::symlink(&real, ws.join("pkg-linked")).unwrap();
    let fs = FakeFsEvents::default();
    let (mut watchers, rx) = fs.watchers();
    for i in 0..11 {
        watchers
            .watch(&ws.join(format!("pkg{i}")), NonRecursive)
            .unwrap();
    }
    assert!(watchers.own.contains_key(&ws), "the parent stream is open");
    let linked = ws.join("pkg-linked/src");
    watchers.watch(&linked, NonRecursive).unwrap();
    assert!(watchers.own.contains_key(&real.join("src")));

    fs.write(&real.join("src/x.ts"));
    assert_eq!(received(&rx), [linked.join("x.ts")]);
}

// FSEvents reports lost events at the stream's path: on a stream moved up
// to a parent outside the root that is no reason to rescan the root, as it
// was not when each served dir had its own stream.
#[test]
fn a_rescan_on_a_consolidated_stream_does_not_rescan_the_root() {
    let (_tmp, ws) = tree((0..11).map(|i| format!("pkg{i}")));
    let fs = FakeFsEvents::default();
    let (mut watchers, rx) = fs.watchers();
    for i in 0..11 {
        watchers
            .watch(&ws.join(format!("pkg{i}")), NonRecursive)
            .unwrap();
    }
    fs.lose_events_at(&ws);
    let lost: Vec<notify::Event> = rx.try_iter().collect();
    assert_eq!(lost.len(), 1);
    assert!(lost[0].need_rescan());
    let root = Path::new("/elsewhere/app");
    assert_eq!(
        rescan_roots(&[], root, &root.join(".oj-cache"), &lost[0]),
        Vec::<PathBuf>::new()
    );
}

// A dir under a stream joins it with no new stream, and a recursive watch of
// a dir the stream holds flat widens it to the dir's whole subtree.
#[test]
fn joined_dirs_ride_their_stream_and_a_recursive_watch_widens_a_flat_one() {
    let (_tmp, ws) = tree((0..11).map(|i| format!("pkg{i}/sub/deep")));
    let fs = FakeFsEvents::default();
    let (mut watchers, rx) = fs.watchers();
    for i in 0..11 {
        watchers
            .watch(&ws.join(format!("pkg{i}")), NonRecursive)
            .unwrap();
    }
    let streams = fs.count();

    watchers.watch(&ws.join("pkg3/sub"), NonRecursive).unwrap();
    assert_eq!(fs.count(), streams);
    fs.write(&ws.join("pkg3/sub/x.ts"));
    assert_eq!(received(&rx), [ws.join("pkg3/sub/x.ts")]);

    let deep = ws.join("pkg10/sub/deep/x.ts");
    fs.write(&deep);
    assert_eq!(received(&rx), Vec::<PathBuf>::new());
    watchers.watch(&ws.join("pkg10"), Recursive).unwrap();
    watchers.watch(&ws.join("pkg10"), NonRecursive).unwrap();
    fs.write(&deep);
    assert_eq!(received(&rx), [deep], "a flat watch narrowed it back");
    assert_eq!(fs.count(), streams);
}

// Off macOS a watch restarts nothing, so the root watcher takes them all.
#[test]
fn without_streams_every_watch_goes_to_the_root_watcher() {
    let (_tmp, ws) = tree((0..3).map(|i| format!("pkg{i}")));
    let fs = FakeFsEvents::default();
    let (mut watchers, _rx) = fs.watchers();
    watchers.streams = false;
    for i in 0..3 {
        watchers
            .watch(&ws.join(format!("pkg{i}")), NonRecursive)
            .unwrap();
    }
    assert_eq!(fs.count(), 1);
    assert_eq!(fs.watched(ROOT).len(), 3);
}

/// `Watchers` on real FSEvents streams, and their events.
#[cfg(target_os = "macos")]
fn fsevents() -> (Watchers, std::sync::mpsc::Receiver<notify::Event>) {
    let (tx, rx) = std::sync::mpsc::channel::<notify::Event>();
    let tx = Mutex::new(tx);
    let sink: EventSink = Arc::new(move |ev: notify::Result<notify::Event>| {
        if let Ok(ev) = ev {
            let _ = tx.lock().unwrap().send(ev);
        }
    });
    (Watchers::new(sink).unwrap(), rx)
}

/// Writes each of `writes`, then collects what arrives until `expected` has
/// and a little after, so a stray event following it is caught too.
#[cfg(target_os = "macos")]
fn fsevents_after(
    rx: &std::sync::mpsc::Receiver<notify::Event>,
    writes: &[PathBuf],
    expected: &Path,
) -> Vec<PathBuf> {
    // A new stream reports from its start, which lags the call.
    std::thread::sleep(Duration::from_millis(500));
    while rx.try_recv().is_ok() {}
    for w in writes {
        std::fs::write(w, "1").unwrap();
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    while std::time::Instant::now() < deadline && !seen.iter().any(|p| p == expected) {
        if let Ok(ev) = rx.recv_timeout(Duration::from_millis(200)) {
            seen.extend(ev.paths);
        }
    }
    let settle = std::time::Instant::now() + Duration::from_millis(500);
    while let Some(left) = settle.checked_duration_since(std::time::Instant::now()) {
        if let Ok(ev) = rx.recv_timeout(left) {
            seen.extend(ev.paths);
        }
    }
    seen
}

// The same on real FSEvents: 300 sibling dirs share a handful of streams, an
// edit in the last one arrives, and its unwatched sibling's does not.
#[cfg(target_os = "macos")]
#[test]
fn many_served_dirs_share_a_few_fsevents_streams() {
    let (_tmp, ws) = tree(
        (0..300)
            .map(|i| format!("pkg{i}"))
            .chain(["unwatched".into()]),
    );
    let (mut watchers, rx) = fsevents();
    for i in 0..300 {
        watchers
            .watch(&ws.join(format!("pkg{i}")), NonRecursive)
            .unwrap();
    }
    assert_eq!(watchers.own.len(), CONSOLIDATE_AT + 1);

    let edited = ws.join("pkg299/x.ts");
    let seen = fsevents_after(&rx, &[ws.join("unwatched/x.ts"), edited.clone()], &edited);
    assert!(seen.contains(&edited), "the edit never arrived: {seen:?}");
    assert!(
        !seen.iter().any(|p| p.starts_with(ws.join("unwatched"))),
        "an unwatched dir's event passed: {seen:?}"
    );
}

// On real FSEvents a stream does not follow a symlink under it: the dir
// behind one still hears its edits, spelled as it was watched.
#[cfg(target_os = "macos")]
#[test]
fn a_symlinked_served_dir_hears_its_edits_on_fsevents() {
    let (_tmp, ws) = tree((0..11).map(|i| format!("pkg{i}")));
    let (_elsewhere, real) = tree(["src".to_string()]);
    std::os::unix::fs::symlink(&real, ws.join("pkg-linked")).unwrap();
    let (mut watchers, rx) = fsevents();
    for i in 0..11 {
        watchers
            .watch(&ws.join(format!("pkg{i}")), NonRecursive)
            .unwrap();
    }
    let linked = ws.join("pkg-linked/src");
    watchers.watch(&linked, NonRecursive).unwrap();

    let expected = linked.join("x.ts");
    let seen = fsevents_after(&rx, &[real.join("src/x.ts")], &expected);
    assert!(seen.contains(&expected), "the edit never arrived: {seen:?}");
}
