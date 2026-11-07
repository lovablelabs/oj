// tsrs-only (Go's GC): what a watch session frees as it goes. The batch compiler leaks every parsed file
// (docs/PORTING.md "Memory model"); a watch session reparses each changed file every cycle, so without this a branch
// switch that rewrites a few thousand files keeps every old version (about 0.9 GB per switch on vscode).
//
// Each parsed file version gets a region of its own, as in the language server (tsrs_project parsecache.rs): the text,
// tree and binder data (`tsrs_compiler` fileregions.rs `bind`), and the file's lazily filled data (`arena::enter_owner`).
// A region is freed once nothing that is read again refers to its file. The session lists those referrers each cycle
// (`FileRegions::take_unreachable`); the programs it replaced refer to old versions too, so the regions are freed on
// the `Freer` thread after them.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::SourceFile;
use tsrs_contentmapper::{self as contentmapper, SourceFiles};
use tsrs_core::P;
use tsrs_core::arena::Region;

/// The regions of the files a watch session parsed, by the address of the file.
#[derive(Default)]
pub struct FileRegions(Mutex<FxHashMap<usize, Region>>);

impl FileRegions {
    /// Runs `parse` (a compiler host's `get_source_file`) with a fresh region as the allocation target. `size` is the
    /// file's size on disk, for the region's first chunk.
    pub(crate) fn parse(
        &self,
        size: usize,
        parse: impl FnOnce() -> Option<P<SourceFile>>,
    ) -> Option<P<SourceFile>> {
        let region = Region::new(first_chunk(size));
        let file = {
            let _scope = region.enter();
            parse()
        }?;
        region.trim();
        // A host that returned a file it had parsed before (none does today) keeps it where it is.
        if Region::containing(file.addr()).is_some_and(|r| r.ptr_eq(&region)) {
            self.0.lock().unwrap().insert(file.addr(), region);
        }
        Some(file)
    }

    /// `parse` with a fresh region, for a content-mapped file: its canonical and supplemental files share the region,
    /// which is freed once none of them is reachable.
    pub(crate) fn parse_content_mapped(
        &self,
        size: usize,
        parse: impl FnOnce() -> Result<SourceFiles, contentmapper::Error>,
    ) -> Result<SourceFiles, contentmapper::Error> {
        let region = Region::new(first_chunk(size));
        let files = {
            let _scope = region.enter();
            parse()
        }?;
        region.trim();
        let mut regions = self.0.lock().unwrap();
        for file in files.canonical.iter().chain(&files.supplemental) {
            if Region::containing(file.addr()).is_some_and(|r| r.ptr_eq(&region)) {
                regions.insert(file.addr(), region.clone());
            }
        }
        Ok(files)
    }

    /// Removes the regions of the files that are not in `live` and returns them, to be dropped once nothing that may
    /// still read those files runs (`Freer`).
    pub(crate) fn take_unreachable(&self, live: &FxHashSet<usize>) -> Vec<Region> {
        self.0
            .lock()
            .unwrap()
            .extract_if(|addr, _| !live.contains(addr))
            .map(|(_, region)| region)
            .collect()
    }
}

// As tsrs_project parsecache.rs: text, AST and binder data take about 8 times the text.
fn first_chunk(text_len: usize) -> usize {
    text_len.saturating_mul(10).saturating_add(4 << 10).min(64 << 20)
}

type FreeJob = Box<dyn FnOnce() + Send>;

/// Runs the frees of a watch session on one thread, in the order they were queued: a file region is freed only after
/// the programs that were replaced before it was found unreachable (they may refer to it). Freeing a large program's
/// checker regions takes a while (~150 ms on a 9k-file project) and nothing waits for it. A `tsc -b` that is not a
/// watch session frees each project's program here once it is built (`BuildTask::free_built_program`). The test
/// harness frees in place, for deterministic runs, and so does a build without threads (WebAssembly). The thread starts
/// with the first queued free, so a build that frees nothing (an API build, which creates one with its orchestrator)
/// starts none: in the WebAssembly module, which cannot spawn threads, `-b` panicked creating it.
pub struct Freer {
    in_place: bool,
    tx: std::sync::OnceLock<Mutex<mpsc::Sender<FreeJob>>>,
    bounded_pending: Arc<AtomicUsize>,
}

impl Freer {
    pub(crate) fn new(in_place: bool) -> Freer {
        Freer {
            in_place: in_place || tsrs_core::NO_THREADS,
            tx: std::sync::OnceLock::new(),
            bounded_pending: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn sender(&self) -> &Mutex<mpsc::Sender<FreeJob>> {
        self.tx.get_or_init(|| {
            let (tx, rx) = mpsc::channel::<FreeJob>();
            std::thread::Builder::new()
                .name("tsrs-free-program".to_string())
                .spawn(move || {
                    for job in rx {
                        job();
                    }
                })
                .expect("failed to spawn a thread");
            Mutex::new(tx)
        })
    }

    /// tsrs-only: once the frees queued so far are done, after each cycle (and the first build):
    /// - macOS: the allocator gives its freed memory back (`mi_collect(true)`), on every worker-pool thread and here.
    ///   mimalloc keeps much of what a cycle freed resident there (after a few full rebuilds of vscode, its
    ///   heap stood ~1 GB above the initial build), and purging at once on every free slows the cycles
    ///   themselves (`MIMALLOC_PURGE_DELAY=0`: the hub edit 1.3 -> 2.0 s). The pool's threads matter as much
    ///   as this one: what they allocated during the cycle is freed here, from another thread, and such a
    ///   block stays allocated in its page (unpurgeable) until the owning thread collects
    ///   (notes/watch-macos.md, "memory").
    pub(crate) fn after_cycle(&self) {
        self.run(|| {
            #[cfg(target_os = "macos")]
            {
                tsrs_compiler::worker_pool().broadcast(|_| tsrs_core::memsplit::heap_collect(true));
                tsrs_core::memsplit::heap_collect(true);
            }
        });
    }

    pub(crate) fn run(&self, job: impl FnOnce() + Send + 'static) {
        if self.in_place {
            job();
            return;
        }
        // The thread only ends when the sender is dropped, with the session.
        self.sender().lock().unwrap().send(Box::new(job)).expect("the free thread ended");
    }

    // Ordinary CLI builds only: their completed programs can be freed independently. Watch-mode jobs must stay in
    // FIFO order, so they use `run`. Once the free thread falls behind, builders free their own programs instead of
    // retaining an unbounded number of programs in its queue.
    pub(crate) fn run_bounded(&self, max_pending: usize, job: impl FnOnce() + Send + 'static) {
        if self.in_place
            || self
                .bounded_pending
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |pending| {
                    (pending < max_pending).then_some(pending + 1)
                })
                .is_err()
        {
            job();
            return;
        }
        let pending = Arc::clone(&self.bounded_pending);
        self.run(move || {
            job();
            pending.fetch_sub(1, Ordering::SeqCst);
        });
    }
}
