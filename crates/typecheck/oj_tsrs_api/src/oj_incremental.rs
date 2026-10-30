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
// - `ojWaitSignatureJob` and `ojIncrementalPrograms`: what the job emitted and which snapshot each project's
//   program holds, for tests.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{JoinHandle, ThreadId};
use std::time::Duration;

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_compiler::ProgramLike;
use tsrs_core::P;
use tsrs_core::arena::Region;
use tsrs_core::context::{CancelFunc, CheckerLifetime, Context, with_checker_lifetime};
use tsrs_core::json::{JsonMap, Value};
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
    job: Option<(CancelFunc, JoinHandle<Emitted>)>,
    /// The files a job took `SLOW_EMIT` or more to emit, by file name.
    slow: Arc<Mutex<FxHashSet<String>>>,
}

impl State {
    /// Frees the programs of the projects `sd` no longer has (closed): no check asks for them again.
    fn free_closed(&mut self, sd: &SnapshotData) {
        let collection = &sd.snapshot.project_collection;
        let closed: Vec<String> = self
            .programs
            .keys()
            .filter(|name| collection.get_project(&ProjectID((*name).clone())).is_none())
            .cloned()
            .collect();
        for name in closed {
            if let Some(entry) = self.programs.remove(&name) {
                entry.free();
            }
        }
    }
}

/// The files a signature job emitted, in order, with each one's transform time.
type Emitted = Vec<(String, Duration)>;

fn object<const N: usize>(fields: [(&'static str, Value); N]) -> Value {
    let mut map = JsonMap::new();
    for (key, value) in fields {
        map.insert(key, value);
    }
    Value::Object(map)
}

/// A lock that a panic while it was held does not poison: the state changes only once a program is complete
/// (`programs.insert`), so what a panic leaves is consistent, and `close` must still free the programs.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A project's incremental program and what it needs alive. Its fields are dropped after `free`.
struct Entry {
    program: P<tsrs_incremental::Program>,
    /// The thread that made `program`, whose arena may take its blocks back.
    thread: ThreadId,
    /// The snapshot of the compiler program it wraps: a `release` of that snapshot must not free the program,
    /// which the next incremental program reads.
    snapshot: Arc<SnapshotData>,
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
        let mut state = lock(&self.oj_incremental);
        let old = state.programs.get(&project).map(|e| e.program);
        let ctx = with_checker_lifetime(&Context::background(), CheckerLifetime::Diagnostics);
        // A project the change left alone keeps its compiler program, and so its incremental program: a new one
        // made from it would share the old one's state, which `free` then releases.
        if let Some(old) = old
            && old.compiler_program().is_some_and(|p| std::ptr::eq(p, program))
        {
            // This snapshot holds the same compiler program: hold it instead, so the one the entry came from (which
            // holds every project's program of its time) is freed once released.
            if let Some(entry) = state.programs.get_mut(&project) {
                entry.snapshot = Arc::clone(&sd);
            }
            state.free_closed(&sd);
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
            snapshot: Arc::clone(&sd),
            _diagnostics: diagnostics,
            _files: files,
        };
        if let Some(old) = state.programs.insert(project, entry) {
            // The new program was made from it, the job that read it is stopped, and the session keeps no other
            // reference.
            old.free();
        }
        state.free_closed(&sd);
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
        let slow_after = match p.opt_u64("slowMs")? {
            Some(ms) => Duration::from_millis(ms),
            None => SLOW_EMIT,
        };
        let mut state = lock(&self.oj_incremental);
        let programs: Vec<P<tsrs_incremental::Program>> =
            state.programs.values().map(|e| e.program).collect();
        if programs.is_empty() {
            return Ok(Value::Null);
        }
        skip.extend(lock(&state.slow).iter().cloned());
        let slow = Arc::clone(&state.slow);
        let (ctx, cancel) = Context::default().with_cancel();
        let job = std::thread::Builder::new()
            .name("oj-tsrs-signatures".to_string())
            .stack_size(SIGNATURE_JOB_STACK)
            .spawn(move || {
                let emitted = Mutex::new(Vec::new());
                let timed = |file: P<tsrs_ast::SourceFile>, took: Duration| {
                    if took >= slow_after {
                        lock(&slow).insert(file.file_name().to_string());
                    }
                    lock(&emitted).push((file.file_name().to_string(), took));
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
                emitted.into_inner().unwrap_or_else(PoisonError::into_inner)
            })
            .map_err(|e| ApiError::internal(format!("signature job: {e}")))?;
        state.job = Some((cancel, job));
        Ok(Value::Null)
    }

    /// Waits for the signature job to finish, without stopping it: the files it emitted, with their transform
    /// times, and the ones it found slow. Reads nothing the job uses, so `handle_request` lets the job run.
    pub(crate) fn handle_oj_wait_signature_job(&self) -> Value {
        let job = lock(&self.oj_incremental).job.take();
        let emitted = job.map(|(_, job)| job.join().unwrap_or_default()).unwrap_or_default();
        let mut slow: Vec<String> =
            lock(&lock(&self.oj_incremental).slow).iter().cloned().collect();
        slow.sort();
        let emitted: Vec<Value> = emitted
            .into_iter()
            .map(|(file, took)| {
                object([
                    ("file", Value::String(file)),
                    ("ms", Value::Number(took.as_secs_f64() * 1000.0)),
                ])
            })
            .collect();
        object([
            ("emitted", Value::Array(emitted)),
            ("slow", Value::Array(slow.into_iter().map(Value::String).collect())),
        ])
    }

    /// The projects with an incremental program, and whether each holds `snapshot`.
    pub(crate) fn handle_oj_incremental_programs(&self, p: Params) -> ApiResult<Value> {
        let sd = self.snapshot_data(p.u64("snapshot")?)?;
        let state = lock(&self.oj_incremental);
        let mut projects: Vec<(&String, bool)> =
            state.programs.iter().map(|(name, e)| (name, Arc::ptr_eq(&e.snapshot, &sd))).collect();
        projects.sort();
        Ok(Value::Array(
            projects
                .into_iter()
                .map(|(name, current)| {
                    object([
                        ("project", Value::String(name.clone())),
                        ("current", Value::Bool(current)),
                    ])
                })
                .collect(),
        ))
    }

    /// Cancels the signature job and waits for the file in progress.
    pub(crate) fn stop_signature_job(&self) {
        let job = lock(&self.oj_incremental).job.take();
        if let Some((cancel, job)) = job {
            cancel.call();
            let _ = job.join();
        }
    }

    /// At close: no request follows, so the programs are freed with nothing made from them.
    pub(crate) fn free_incremental_programs(&self) {
        self.stop_signature_job();
        let programs: Vec<Entry> =
            lock(&self.oj_incremental).programs.drain().map(|(_, e)| e).collect();
        for entry in programs {
            entry.free();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use tsrs_core::context::Context;
    use tsrs_core::json::{Value, unmarshal};
    use tsrs_vfs::{FS, bundled, osvfs};

    use crate::handler::{Handler, Response};
    use crate::session::{Session, SessionOptions};

    /// A temporary directory with two projects, `a` and `b`, removed on drop.
    struct Projects(PathBuf);

    impl Projects {
        fn new(name: &str) -> Projects {
            let dir = std::env::temp_dir()
                .join(format!("oj-tsrs-incremental-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            for project in ["a", "b"] {
                std::fs::create_dir_all(dir.join(project)).unwrap();
                std::fs::write(
                    dir.join(project).join("tsconfig.json"),
                    r#"{ "compilerOptions": { "strict": true, "noEmit": true, "lib": ["ES2022"] }, "include": ["*.ts"] }"#,
                )
                .unwrap();
                std::fs::write(dir.join(project).join("x.ts"), "export const x: number = 1;\n")
                    .unwrap();
                std::fs::write(dir.join(project).join("y.ts"), "export const y = 'y';\n").unwrap();
            }
            Projects(dir.canonicalize().unwrap())
        }

        fn config(&self, project: &str) -> String {
            self.0.join(project).join("tsconfig.json").to_string_lossy().into_owned()
        }
    }

    impl Drop for Projects {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn session(root: &Path) -> Arc<Session> {
        let fs: Arc<dyn FS> = Arc::new(bundled::wrap_fs(osvfs::fs()));
        Session::new(SessionOptions::new(
            root.to_string_lossy().into_owned(),
            bundled::default_lib_path(),
            fs,
            false,
        ))
    }

    fn call(session: &Session, method: &str, params: &str) -> Value {
        match session.handle_request(method, params.as_bytes()) {
            Ok(Response::Json(text)) => unmarshal(&text).unwrap(),
            Ok(Response::Binary(_)) => panic!("{method}: binary response"),
            Err(e) => panic!("{method}: {e}"),
        }
    }

    fn field<'a>(v: &'a Value, key: &str) -> &'a Value {
        match v {
            Value::Object(map) => map.get(key).unwrap_or(&Value::Null),
            _ => &Value::Null,
        }
    }

    fn snapshot_of(v: &Value) -> u64 {
        match field(v, "snapshot") {
            Value::Number(n) => *n as u64,
            Value::Integer(n) => *n as u64,
            other => panic!("no snapshot: {other:?}"),
        }
    }

    /// The project ids a snapshot response lists, by config file name.
    fn project_ids(v: &Value) -> Vec<(String, String)> {
        let Value::Array(projects) = field(v, "projects") else { panic!("no projects") };
        projects
            .iter()
            .filter_map(|p| match (field(p, "configFileName"), field(p, "id")) {
                (Value::String(config), Value::String(id)) => Some((config.clone(), id.clone())),
                _ => None,
            })
            .collect()
    }

    fn check(session: &Session, snapshot: u64, project: &str) {
        call(
            session,
            "ojGetIncrementalSemanticDiagnostics",
            &format!(r#"{{"snapshot":{snapshot},"project":{project:?}}}"#),
        );
    }

    #[test]
    fn a_panic_while_the_state_is_locked_leaves_requests_and_close_working() {
        let projects = Projects::new("poison");
        let session = session(&projects.0);
        let held = Arc::clone(&session);
        let _ = std::thread::spawn(move || {
            let _state = held.oj_incremental.lock().unwrap();
            panic!("a panic while making a program");
        })
        .join();
        assert!(session.oj_incremental.is_poisoned());
        let opened = call(
            &session,
            "createSnapshot",
            &format!(r#"{{"openProjects":[{:?}]}}"#, projects.config("a")),
        );
        let (_, id) = project_ids(&opened).into_iter().next().unwrap();
        check(&session, snapshot_of(&opened), &id);
        call(&session, "ojStartSignatureJob", "{}");
        session.close();
    }

    #[test]
    fn a_closed_projects_program_is_freed_at_the_next_check() {
        let projects = Projects::new("closed");
        let session = session(&projects.0);
        let opened = call(
            &session,
            "createSnapshot",
            &format!(
                r#"{{"openProjects":[{:?},{:?}]}}"#,
                projects.config("a"),
                projects.config("b")
            ),
        );
        let ids = project_ids(&opened);
        assert_eq!(ids.len(), 2, "{ids:?}");
        let first = snapshot_of(&opened);
        for (_, id) in &ids {
            check(&session, first, id);
        }
        let a =
            ids.iter().find(|(config, _)| config.ends_with("/a/tsconfig.json")).unwrap().1.clone();
        let next = call(
            &session,
            "updateSnapshot",
            &format!(
                r#"{{"snapshot":{first},"changes":{{"closeProjects":[{:?}]}}}}"#,
                projects.config("b")
            ),
        );
        let second = snapshot_of(&next);
        check(&session, second, &a);
        let Value::Array(programs) =
            call(&session, "ojIncrementalPrograms", &format!(r#"{{"snapshot":{second}}}"#))
        else {
            panic!("ojIncrementalPrograms: not a list");
        };
        let names: Vec<&Value> = programs.iter().map(|p| field(p, "project")).collect();
        assert_eq!(names, [&Value::String(a)]);
        session.close();
    }

    #[test]
    fn only_an_interruptible_emit_stops_once_cancelled() {
        let projects = Projects::new("cancel");
        let session = session(&projects.0);
        let opened = call(
            &session,
            "createSnapshot",
            &format!(r#"{{"openProjects":[{:?}]}}"#, projects.config("a")),
        );
        let (_, id) = project_ids(&opened).into_iter().next().unwrap();
        let sd = session.snapshot_data(snapshot_of(&opened)).unwrap();
        let program = sd.get_program(&tsrs_project::ID(id)).unwrap();
        let (ctx, cancel) = Context::default().with_cancel();
        cancel.call();
        let write = |_: &str, _: &str, _: &mut tsrs_compiler::WriteFileData| Ok(());
        let timed = |_: tsrs_core::P<tsrs_ast::SourceFile>, _: std::time::Duration| {};
        let emit = |interruptible: bool| {
            program
                .emit_each_file(
                    &ctx,
                    &tsrs_compiler::EmitOptions {
                        emit_only: tsrs_compiler::EmitOnly::EmitOnlyBuilderSignature,
                        write_file: Some(&write),
                        oj_interruptible: interruptible.then_some(&timed as _),
                        ..Default::default()
                    },
                )
                .len()
        };
        // Upstream callers read every file's output, cancelled or not.
        assert_eq!(emit(false), 2);
        assert_eq!(emit(true), 0);
        drop(sd);
        session.close();
    }
}
