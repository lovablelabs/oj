// oj (not in tsrs): diagnostics for a session that checks the same projects after every edit, kept the way
// `tsc --watch` keeps them (tsrs_execute watcher.rs `install_program` and `start_signature_job`). Applied by
// tools/vendor-tsrs.py from tools/tsrs-patches.
//
// - `ojGetIncrementalSemanticDiagnostics`: a project's semantic diagnostics through an incremental program made
//   from the previous call's, so only the files the change affects are checked again.
// - `ojGetGlobalDiagnostics`: `getGlobalDiagnostics` without its full semantic pass. The global diagnostics are
//   the ones checker initialization reports, which the diagnostics checker has once taken.
// - `ojStartSignatureJob`: records the declaration signatures of the files the last check did not emit, in the
//   background, so the next edit of a file whose exported types stay the same checks only that file. Every
//   request stops it first (`stop_signature_job`), so nothing it reads changes or is freed while it runs; it
//   stops between files, but one file's emit runs to its end. So it skips the files its `skip` param names
//   (ones the caller knows are slow, such as a TanStack route tree) and those it once took `SLOW_EMIT` or more
//   to emit. Their signatures stay their versions: a change to one re-checks the files that import it.

use std::sync::{Arc, Mutex};
use std::thread::{JoinHandle, ThreadId};
use std::time::Duration;

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_compiler::ProgramLike;
use tsrs_core::P;
use tsrs_core::arena::Region;
use tsrs_core::context::{CancelFunc, CheckerLifetime, Context, with_checker_lifetime};
use tsrs_core::json::Value;
use tsrs_project::ID as ProjectID;

use crate::diagnostics::diagnostic_responses;
use crate::handler::{ApiError, ApiResult};
use crate::session::{Session, SnapshotData};
use crate::wire::Params;

/// Files per declaration emit of the signature job. A request that stops the job waits for the file in progress,
/// not the chunk: the emit checks for cancellation before each file (tools/tsrs-patches).
const SIGNATURE_JOB_CHUNK: usize = 256;

/// The job's stack: declaration emit runs the checker, which recurses deeply.
const SIGNATURE_JOB_STACK: usize = 256 << 20;

/// A file whose declaration emit took this long once is left out of later jobs: a request that arrives during
/// it waits for it (a large app's generated route tree takes about 20 s).
const SLOW_EMIT: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct State {
    programs: FxHashMap<String, Entry>,
    job: Option<(CancelFunc, JoinHandle<()>)>,
    /// The files a job took `SLOW_EMIT` or more to emit, by file name.
    slow: Arc<Mutex<FxHashSet<String>>>,
}

/// A project's incremental program and what it needs alive. Its fields are dropped after `free`.
struct Entry {
    program: P<tsrs_incremental::Program>,
    /// The thread that made `program`, whose arena may take its blocks back.
    thread: ThreadId,
    /// The snapshot of the compiler program it wraps: a `release` of that snapshot must not free the program,
    /// which the next incremental program reads.
    _snapshot: Arc<SnapshotData>,
    /// The diagnostics it kept from older programs, copied here so those programs can be freed.
    _diagnostics: Region,
    /// The parsed files those diagnostics refer to. A kept diagnostic can name a file version older than the
    /// program's: related information in a file edited since.
    _files: Vec<Region>,
}

impl Entry {
    /// Frees the program. Nothing may read it afterwards: a program made from it has been created, or none will.
    fn free(self) {
        if self.thread == std::thread::current().id() {
            // SAFETY: the caller's contract, and `program` was allocated on this thread (`thread`).
            unsafe { tsrs_incremental::Program::free_replaced(self.program) };
        } else {
            // SAFETY: the caller's contract; another thread's arena keeps the blocks.
            unsafe { tsrs_incremental::Program::drop_replaced(self.program) };
        }
    }
}

impl Session {
    pub(crate) fn handle_oj_incremental_semantic_diagnostics(&self, p: Params) -> ApiResult<Value> {
        let project = p.str("project")?.to_string();
        let sd = self.snapshot_data(p.u64("snapshot")?)?;
        let program = sd.get_program(&ProjectID(project.clone()))?;
        // Bind before any checker exists, as `install_program` does.
        program.bind_source_files();
        let mut state = self.oj_incremental.lock().unwrap();
        let old = state.programs.get(&project).map(|e| e.program);
        let ctx = with_checker_lifetime(&Context::background(), CheckerLifetime::Diagnostics);
        // A project the change left alone keeps its compiler program, and so its incremental program: a new one
        // made from it would share the old one's state, which `free` then releases.
        if let Some(old) = old
            && old.compiler_program().is_some_and(|p| std::ptr::eq(p, program))
        {
            drop(state);
            let old: &'static tsrs_incremental::Program = old.get();
            return Ok(diagnostic_responses(&old.get_semantic_diagnostics(&ctx, None)));
        }
        // The diagnostics the new program recomputes for itself and those it keeps from the old one.
        let diagnostics = Region::new_scratch(64 << 10);
        let new = {
            let _route = diagnostics.route_diagnostics();
            tsrs_incremental::new_program(
                program,
                old,
                tsrs_incremental::create_host(Arc::clone(program.host())),
                None,
                false,
            )
        };
        {
            let _scope = diagnostics.enter();
            new.copy_diagnostics();
        }
        let mut addrs = Vec::new();
        new.for_each_diagnostic_file(&mut |file| addrs.push(file.addr()));
        addrs.sort_unstable();
        addrs.dedup();
        let files = Region::containing_all(addrs.into_iter()).into_iter().flatten().collect();
        let entry = Entry {
            program: new,
            thread: std::thread::current().id(),
            _snapshot: Arc::clone(&sd),
            _diagnostics: diagnostics,
            _files: files,
        };
        if let Some(old) = state.programs.insert(project, entry) {
            // The new program was made from it, the job that read it is stopped, and the session keeps no other
            // reference.
            old.free();
        }
        drop(state);
        let new: &'static tsrs_incremental::Program = new.get();
        Ok(diagnostic_responses(&new.get_semantic_diagnostics(&ctx, None)))
    }

    pub(crate) fn handle_oj_global_diagnostics(&self, p: Params) -> ApiResult<Value> {
        let sd = self.snapshot_data(p.u64("snapshot")?)?;
        let id = ProjectID(p.str("project")?.to_string());
        let proj = sd
            .snapshot
            .project_collection
            .get_project(&id)
            .ok_or_else(|| ApiError::client(format!("project {} not found", id.0)))?;
        // As `Program::get_global_diagnostics`: a program without files has none (no checker runs for it).
        if sd.get_program(&id)?.get_source_files().is_empty() {
            return Ok(diagnostic_responses(&[]));
        }
        let ctx = with_checker_lifetime(&Context::background(), CheckerLifetime::Diagnostics);
        // Taking the diagnostics checker creates it when no check did; releasing it merges its global
        // diagnostics into the pool's.
        if let Some(pool) = &proj.checker_pool {
            drop(pool.get_checker(&ctx, None));
        }
        let diags: Vec<_> =
            proj.get_project_diagnostics(&ctx).into_iter().filter(|d| d.file().is_none()).collect();
        Ok(diagnostic_responses(&diags))
    }

    pub(crate) fn handle_oj_start_signature_job(&self, p: Params) -> ApiResult<Value> {
        let mut skip: FxHashSet<String> = FxHashSet::default();
        if p.has("skip") {
            for name in p.array("skip")? {
                let Value::String(name) = name else {
                    return Err(ApiError::client("skip: expected file names"));
                };
                skip.insert(name.clone());
            }
        }
        let mut state = self.oj_incremental.lock().unwrap();
        let programs: Vec<P<tsrs_incremental::Program>> =
            state.programs.values().map(|e| e.program).collect();
        if programs.is_empty() {
            return Ok(Value::Null);
        }
        skip.extend(state.slow.lock().unwrap().iter().cloned());
        let slow = Arc::clone(&state.slow);
        let (ctx, cancel) = Context::default().with_cancel();
        let job = std::thread::Builder::new()
            .name("oj-tsrs-signatures".to_string())
            .stack_size(SIGNATURE_JOB_STACK)
            .spawn(move || {
                let timed = |file: P<tsrs_ast::SourceFile>, took: Duration| {
                    if took >= SLOW_EMIT {
                        slow.lock().unwrap().insert(file.file_name().to_string());
                    }
                };
                for program in programs {
                    program.oj_compute_placeholder_signatures(
                        &ctx,
                        SIGNATURE_JOB_CHUNK,
                        &|name| skip.contains(name),
                        &timed,
                    );
                    if ctx.err().is_some() {
                        break;
                    }
                }
            })
            .map_err(|e| ApiError::internal(format!("signature job: {e}")))?;
        state.job = Some((cancel, job));
        Ok(Value::Null)
    }

    /// Cancels the signature job and waits for the chunk in progress.
    pub(crate) fn stop_signature_job(&self) {
        let job = self.oj_incremental.lock().unwrap().job.take();
        if let Some((cancel, job)) = job {
            cancel.call();
            let _ = job.join();
        }
    }

    /// At close: no request follows, so the programs are freed with nothing made from them.
    pub(crate) fn free_incremental_programs(&self) {
        self.stop_signature_job();
        let programs: Vec<Entry> =
            self.oj_incremental.lock().unwrap().programs.drain().map(|(_, e)| e).collect();
        for entry in programs {
            entry.free();
        }
    }
}
