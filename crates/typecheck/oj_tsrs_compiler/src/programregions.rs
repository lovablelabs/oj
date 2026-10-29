// tsrs-only (watch mode): regions for what building a program allocates, freed when nothing uses the program's data
// any more (tsrs_execute watcher.rs: with the programs updated from it). The file loader allocates on the calling
// thread and on the worker pool (`fileloader::process_all_program_files`, `filesParser`), which other programs share
// under `tsc -b --watch`, so each thread gets a region of its own and every loader job enters it.
//
// The regions are not registered (`Region::new_scratch`): lazily filled data of an object in one goes to the thread
// arena (`arena::enter_owner`), so another thread never waits for a region a loader job holds while that job waits on
// the pool.

use std::sync::Mutex;
use std::thread::ThreadId;

use rustc_hash::FxHashMap;
use tsrs_core::arena::{Region, RegionScope};

#[derive(Default)]
pub struct ProgramRegions(Mutex<FxHashMap<ThreadId, Region>>);

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
