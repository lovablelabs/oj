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
    // ensure_module of a known module no longer builds an owned key.
    assert_eq!(
        allocations(|| {
            g.ensure_module(&app);
        }),
        0
    );
}
