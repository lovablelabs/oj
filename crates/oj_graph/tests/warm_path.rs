// SPDX-License-Identifier: MIT
// Copyright (c) 2026 William Rudenmalm

//! The dev server re-registers a module in the graph on every request, so
//! re-recording edges that are already there must not touch the heap.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::{Path, PathBuf};

use oj_graph::ModuleGraph;

// Counted per thread: libtest's own threads allocate while a test runs.
thread_local! {
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.with(|c| c.set(c.get() + 1));
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn allocations(f: impl FnOnce()) -> usize {
    let before = ALLOCS.with(Cell::get);
    f();
    ALLOCS.with(Cell::get) - before
}

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

#[test]
fn re_recording_identical_edges_allocates_nothing() {
    let mut g = ModuleGraph::new();
    let app = p("/src/App.tsx");
    let imports = [p("/src/Button.tsx"), p("/src/util.ts"), p("/src/a.css")];
    g.set_imports(&app, &imports);
    g.set_self_accepting(&app, true);
    g.set_accepted_deps(&app, &[]);

    // What register_in_graph hands over on a warm request: borrowed paths, in
    // a different order, with a repeat (`./a.css` + `./a.css?inline`).
    let borrowed: Vec<&Path> = [2, 0, 1, 2]
        .into_iter()
        .map(|i| imports[i].as_path())
        .collect();
    assert_eq!(
        allocations(|| assert!(g.set_imports(&app, &borrowed).is_empty())),
        0
    );
    assert_eq!(
        allocations(|| assert!(g.set_imports(&app, &imports).is_empty())),
        0
    );
    assert_eq!(allocations(|| g.set_self_accepting(&app, false)), 0);
    // No declared hot.accept deps (nearly every module): an empty set is free.
    assert_eq!(allocations(|| g.set_accepted_deps(&app, &[])), 0);
    // add_import of an edge that is already there is a no-op on both sides.
    assert_eq!(allocations(|| g.add_import(&app, &imports[0])), 0);
    // Partial-accept metadata (acceptExports + importedBindings) re-recorded
    // unchanged, as every warm request does since #272: compared borrowed,
    // materialized only on change.
    let names = ["a".to_string(), "b".to_string()];
    g.set_accepted_exports(&app, Some(&names));
    let bindings = [
        (imports[0].as_path(), names.as_slice()),
        (imports[1].as_path(), &names[..1]),
        // A repeated clean path (`./a.css` + `./a.css?inline`), like the
        // server's query-stripped iterator can produce.
        (imports[2].as_path(), [].as_slice()),
        (imports[2].as_path(), [].as_slice()),
    ];
    g.set_imported_bindings(&app, bindings.iter().copied());
    assert_eq!(
        allocations(|| g.set_accepted_exports(&app, Some(&names))),
        0
    );
    assert_eq!(
        allocations(|| g.set_imported_bindings(&app, bindings.iter().copied())),
        0
    );
    assert_eq!(
        allocations(|| g.set_accepted_exports(&imports[0], None)),
        0,
        "the no-hot common case is free"
    );
    assert_eq!(
        allocations(
            || g.set_imported_bindings(&imports[0], std::iter::empty::<(&Path, &[String])>())
        ),
        0,
        "the no-bindings common case is free"
    );
    // The urls the imports are named by, re-recorded unchanged: a queried
    // one (`./util.ts?raw` beside `./util.ts`), a repeat, and bare ones the
    // graph keeps nothing for.
    let urls = [
        (imports[0].as_path(), "/src/Button.tsx"),
        (imports[1].as_path(), "/src/util.ts"),
        (imports[1].as_path(), "/src/util.ts?raw"),
        (imports[1].as_path(), "/src/util.ts?raw"),
        (imports[2].as_path(), "/src/a.css?import"),
    ];
    g.set_import_urls(&app, urls.iter().copied());
    let named: Vec<(&Path, &str)> = g
        .node(&app)
        .unwrap()
        .import_urls
        .iter()
        .map(|(p, url)| (p.as_path(), url.as_str()))
        .collect();
    assert_eq!(named, urls[1..]);
    assert_eq!(
        allocations(|| g.set_import_urls(&app, urls.iter().copied())),
        0
    );
    assert_eq!(
        allocations(|| g.set_import_urls(&imports[0], std::iter::empty::<(&Path, &str)>())),
        0,
        "the no-imports common case is free"
    );
    // A bare url that is stored (its node has a queried one too) is not
    // taken for one that is not: dropping the queried url is a change, and
    // an import no longer named with its query is forgotten.
    g.set_import_urls(&app, urls[..2].iter().copied());
    assert!(g.node(&app).unwrap().import_urls.is_empty());
    // A node first named by its path, then with a query: both are kept.
    g.set_import_urls(&app, urls[..3].iter().copied());
    assert_eq!(g.node(&app).unwrap().import_urls.len(), 2);
    // An eager glob's worth of queried imports re-records in one pass.
    let members: Vec<(PathBuf, String)> = (0..20_000)
        .map(|i| {
            (
                p(&format!("/src/posts/{i}.md")),
                format!("/src/posts/{i}.md?raw"),
            )
        })
        .collect();
    let glob = || members.iter().map(|(p, url)| (p.as_path(), url.as_str()));
    g.set_import_urls(&app, glob());
    let started = std::time::Instant::now();
    assert_eq!(allocations(|| g.set_import_urls(&app, glob())), 0);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "re-recording 20000 unchanged urls took {:?}",
        started.elapsed()
    );
    // ensure_module of a known module no longer builds an owned key.
    assert_eq!(
        allocations(|| {
            g.ensure_module(&app);
        }),
        0
    );
}
