// Memory regions of the language server (docs/LSP.md "Memory plan for a long-lived server"; not in Go, where the GC
// frees a program version and everything only it refers to).
//
// A program version owns:
// - its own region: what `CreateProgram` allocated (the cloned file list and resolution data of a reused program,
//   or everything a full build allocated on the calling thread);
// - the base region: the full build's region, shared by every version cloned from it (`UpdateProgram` hands the
//   shared processed-file data and the symlink cache to the next version);
// - the regions of its source files (each parsed file version has one, see parsecache.rs);
// - through its checker pool, the checkers' regions.
//
// `programOwner` is held by every `Project` value that refers to the program (projects are cloned per snapshot and
// live as long as a snapshot or a language service refers to them), so it is dropped exactly when nothing can reach
// the program any more. Dropping it frees the checkers and their regions, the `Program` itself, then the regions.

use std::sync::Arc;

use tsrs_compiler::Program;
use tsrs_core::arena::Region;

use crate::checkerpool::CheckerPoolImpl;

pub struct ProgramOwner {
    program: &'static Program,
    checker_pool: Option<Arc<CheckerPoolImpl>>,
    // Dropped in this order after `drop` freed the checkers and the program.
    region: Region,
    pub(crate) base: Region,
    files: Vec<Region>,
}

impl ProgramOwner {
    // `full_build`: the program does not share data with an older version; `base` is then its own region, which
    // also becomes the owner of the shared processed-file data (`Program::get_symlink_cache` routes there).
    pub(crate) fn new(
        program: &'static Program,
        checker_pool: Option<Arc<CheckerPoolImpl>>,
        region: Region,
        base: Region,
        full_build: bool,
    ) -> ProgramOwner {
        if full_build {
            base.adopt_owner(processed_files_addr(program));
            // The data shared by every version cloned from this build lives exactly as long as `base` (each
            // version's owner holds `base`), so it is freed with it. Before this it was leaked per full build.
            let shared = tsrs_compiler::shared_program_data(program);
            base.on_free(Box::new(move || {
                // SAFETY: `base` is freed after the last program owner holding it freed its program.
                unsafe { shared.free() }
            }));
        }
        let files = Region::containing_all(program.source_files().iter().map(|f| f.addr()))
            .into_iter()
            .flatten()
            .collect();
        log_region(|| {
            format!(
                "program {:p}: created (region {} KiB, base {} KiB)",
                program,
                region.allocated_bytes() >> 10,
                base.allocated_bytes() >> 10
            )
        });
        ProgramOwner { program, checker_pool, region, base, files }
    }
}

// The address `Program::get_symlink_cache` routes by (`Program` derefs to its shared `processedFiles`).
fn processed_files_addr(program: &'static Program) -> usize {
    std::ptr::from_ref(&**program).cast::<()>() as usize
}

impl Drop for ProgramOwner {
    fn drop(&mut self) {
        log_region(|| {
            format!(
                "program {:p}: freed ({} files, region {} KiB)",
                self.program,
                self.files.len(),
                self.region.allocated_bytes() >> 10
            )
        });
        if let Some(pool) = &self.checker_pool {
            pool.free_checkers();
        }
        // SAFETY: the last project value referring to the program is being dropped: no snapshot, language service or
        // held checker can reach it (see the file comment).
        unsafe { tsrs_compiler::free_program(self.program) };
    }
}

// `TSRS_REGION_LOG=1`: one stderr line per program created / freed and per disposed checker freed / parked.
pub fn log_region(msg: impl FnOnce() -> String) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("TSRS_REGION_LOG").is_some_and(|v| v == "1")) {
        eprintln!("regions: {}", msg());
    }
}
