use super::*;
use notify::RecursiveMode::{NonRecursive, Recursive};
use std::time::Duration;

fn paths(list: &[&str]) -> Vec<PathBuf> {
    list.iter().map(PathBuf::from).collect()
}

// chokidar's fsevents handler: a dir under a watched one joins its stream,
// the tenth sibling consolidates onto the parent, and past 128 streams the
// root watcher takes it.
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
    assert_eq!(stream_for(&full, Path::new("/other/y")), StreamPlan::Root);
    assert_eq!(
        stream_for(&full, Path::new("/ws/d3/x/deep")),
        StreamPlan::Join(PathBuf::from("/ws/d3/x")),
        "a covered dir still joins past the cap"
    );
}

// A shared stream passes only its watched dirs' events: a flat watch keeps
// the dir and its direct children, a recursive one everything under it.
#[test]
fn a_shared_stream_passes_only_its_watched_dirs_events() {
    let dirs = std::collections::HashMap::from([
        (PathBuf::from("/ws/pkg/a"), NonRecursive),
        (PathBuf::from("/ws/pkg/b"), Recursive),
    ]);
    assert!(watched_by(&dirs, Path::new("/ws/pkg/a")));
    assert!(watched_by(&dirs, Path::new("/ws/pkg/a/x.ts")));
    assert!(!watched_by(&dirs, Path::new("/ws/pkg/a/sub/x.ts")));
    assert!(watched_by(&dirs, Path::new("/ws/pkg/b/sub/deep/x.ts")));
    assert!(!watched_by(&dirs, Path::new("/ws/pkg/c/x.ts")));
    assert!(!watched_by(&dirs, Path::new("/ws/pkg/x.ts")));
}

// Served dirs past chokidar's cap never restart the root watcher's stream:
// 300 sibling package dirs share a handful of streams, and an edit in the
// last one still arrives while its unwatched sibling's does not.
#[cfg(target_os = "macos")]
#[test]
fn many_served_dirs_share_a_few_fsevents_streams() {
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<notify::Event>();
    let sink: EventSink = Arc::new(move |ev: notify::Result<notify::Event>| {
        if let Ok(ev) = ev {
            let _ = tx.send(ev);
        }
    });
    let root_sink = Arc::clone(&sink);
    let mut watchers = Watchers {
        root: new_watcher(move |ev| root_sink(ev)).unwrap(),
        sink,
        own: Default::default(),
    };
    for i in 0..300 {
        let dir = ws.join(format!("pkg{i}"));
        std::fs::create_dir_all(&dir).unwrap();
        watchers.watch(&dir, NonRecursive).unwrap();
    }
    std::fs::create_dir_all(ws.join("unwatched")).unwrap();
    assert!(
        watchers.own.len() <= CONSOLIDATE_AT + 1,
        "{} streams for 300 sibling dirs",
        watchers.own.len()
    );

    std::thread::sleep(Duration::from_millis(500));
    while rx.try_recv().is_ok() {}
    std::fs::write(ws.join("unwatched/x.ts"), "1").unwrap();
    let edited = ws.join("pkg299/x.ts");
    std::fs::write(&edited, "1").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    while std::time::Instant::now() < deadline && !seen.contains(&edited) {
        if let Ok(ev) = rx.recv_timeout(Duration::from_millis(200)) {
            seen.extend(ev.paths);
        }
    }
    assert!(seen.contains(&edited), "the edit never arrived: {seen:?}");
    assert!(
        !seen.iter().any(|p| p.starts_with(ws.join("unwatched"))),
        "an unwatched dir's event passed: {seen:?}"
    );
}
