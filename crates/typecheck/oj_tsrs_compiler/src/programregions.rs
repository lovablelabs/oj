// tsrs-only: regions for what building a program allocates, freed when nothing uses the program's data
// any more (tsrs_execute watcher.rs: with the programs updated from it). The file loader allocates on the calling
// thread and on the worker pool (`fileloader::process_all_program_files`, `filesParser`), which other programs share
// under `tsc -b --watch`, so each thread gets a region of its own and every loader job enters it.
//
// Unregistered regions prevent owner-based routing from locking another thread's loader region while its job
// waits on the worker pool.

use std::sync::Mutex;
use std::thread::ThreadId;

use rustc_hash::FxHashMap;
use tsrs_core::arena::{Region, RegionScope};

#[derive(Default)]
pub struct ProgramRegions(pub Mutex<FxHashMap<ThreadId, Region>>);

impl ProgramRegions {
    /// Makes this thread's region of the set the allocation target until the scope is dropped.
    fn enter(&self) -> RegionScope {
        let region = self
            .0
            .lock()
            .unwrap()
            .entry(std::thread::current().id())
            .or_insert_with(|| Region::new_scratch(1 << 20))
            .clone();
        region.enter()
    }
}

/// `ProgramRegions::enter` if the program has regions.
pub fn enter(regions: Option<&ProgramRegions>) -> Option<RegionScope> {
    regions.map(ProgramRegions::enter)
}
