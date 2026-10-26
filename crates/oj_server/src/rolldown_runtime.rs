// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The runtime rolldown runs on, sized like rolldown's own npm binding.
//!
//! rolldown spawns its module tasks with `tokio::spawn` and reads every
//! source file through `Handle::current().spawn_blocking`, so both land on
//! whatever runtime polls the build. On oj's main runtime (Tokio's default
//! 512 blocking threads) a build of a few thousand modules grows the pool to
//! hundreds of threads that contend on its lock. rolldown's binding caps the
//! pool at 4 for this reason (`ROLLDOWN_MAX_BLOCKING_THREADS`), with 1.5
//! workers per CPU since module tasks also block on their workers.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::runtime::{Handle, Runtime};

/// rolldown's binding default.
pub const MAX_BLOCKING_THREADS: usize = 4;

/// A runtime for rolldown builds. `oj` builds one beside its main runtime and
/// hands its [`Handle`] to whatever bundles.
pub fn build() -> std::io::Result<Runtime> {
    // The binding's count: 1.5 per physical core this process may use.
    let cores =
        num_cpus::get_physical().min(std::thread::available_parallelism().map_or(1, |n| n.get()));
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cores * 3 / 2)
        .max_blocking_threads(MAX_BLOCKING_THREADS)
        .thread_name("oj-rolldown")
        .thread_stack_size(oj_compiler::COMPILE_STACK_SIZE)
        .enable_all()
        .build()
}

/// Polls `fut` with `rolldown` as the current runtime, so the tasks and
/// blocking reads a build starts run there. The caller still awaits it from
/// its own runtime; nothing needs to be `'static`.
pub fn on_rolldown<F: Future>(rolldown: &Handle, fut: F) -> OnRolldown<F> {
    OnRolldown {
        rolldown: rolldown.clone(),
        fut: Box::pin(fut),
    }
}

pub struct OnRolldown<F> {
    rolldown: Handle,
    fut: Pin<Box<F>>,
}

impl<F: Future> Future for OnRolldown<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let this = self.get_mut();
        let _entered = this.rolldown.enter();
        this.fut.as_mut().poll(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// The runtime a caller awaits from, as `oj`'s main one. Each test builds
    /// its own runtimes and drops them outside async code.
    fn caller() -> Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn spawned_work_runs_on_the_rolldown_runtime() {
        let rolldown = build().unwrap();
        let names = caller().block_on(on_rolldown(rolldown.handle(), async {
            let task = tokio::spawn(async { std::thread::current().name().map(str::to_owned) });
            let blocking = tokio::runtime::Handle::current()
                .spawn_blocking(|| std::thread::current().name().map(str::to_owned));
            (task.await.unwrap(), blocking.await.unwrap())
        }));
        assert_eq!(names.0.as_deref(), Some("oj-rolldown"));
        assert_eq!(names.1.as_deref(), Some("oj-rolldown"));
    }

    #[test]
    fn a_flood_of_blocking_reads_stays_within_the_cap() {
        // What a build does: one blocking read per module, all at once.
        let rolldown = build().unwrap();
        let threads = Arc::new(Mutex::new(HashSet::new()));
        caller().block_on(on_rolldown(rolldown.handle(), async {
            let reads: Vec<_> = (0..400)
                .map(|_| {
                    let threads = Arc::clone(&threads);
                    tokio::runtime::Handle::current().spawn_blocking(move || {
                        threads.lock().unwrap().insert(std::thread::current().id());
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    })
                })
                .collect();
            for read in reads {
                read.await.unwrap();
            }
        }));
        let used = threads.lock().unwrap().len();
        assert!(
            used <= MAX_BLOCKING_THREADS,
            "{used} blocking threads served the reads"
        );
    }

    /// Counts each module's `load` hook, and those that ran off the rolldown
    /// runtime: rolldown calls it from the module task that then reads the
    /// file.
    #[derive(Debug, Default)]
    struct LoadCounts {
        loads: AtomicUsize,
        elsewhere: AtomicUsize,
    }

    #[derive(Debug)]
    struct LoadThreads(Arc<LoadCounts>);

    impl rolldown_plugin::Plugin for LoadThreads {
        fn name(&self) -> std::borrow::Cow<'static, str> {
            "test:load-threads".into()
        }

        fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
            rolldown_plugin::HookUsage::Load
        }

        async fn load(
            &self,
            _ctx: rolldown_plugin::SharedLoadPluginContext,
            _args: &rolldown_plugin::HookLoadArgs<'_>,
        ) -> rolldown_plugin::HookLoadReturn {
            self.0.loads.fetch_add(1, Ordering::Relaxed);
            if std::thread::current().name() != Some("oj-rolldown") {
                self.0.elsewhere.fetch_add(1, Ordering::Relaxed);
            }
            Ok(None)
        }
    }

    #[test]
    fn a_build_runs_its_module_tasks_on_the_rolldown_runtime() {
        let dir = std::env::temp_dir().join(format!("oj-rolldown-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut entry = String::new();
        for i in 0..50 {
            std::fs::write(
                dir.join(format!("m{i}.js")),
                format!("export const v{i} = {i};\n"),
            )
            .unwrap();
            entry.push_str(&format!("export * from \"./m{i}.js\";\n"));
        }
        std::fs::write(dir.join("entry.js"), entry).unwrap();
        let counts = Arc::new(LoadCounts::default());
        let mut bundler = rolldown::BundlerBuilder::default()
            .with_options(rolldown::BundlerOptions {
                input: Some(vec![rolldown::InputItem {
                    name: None,
                    import: dir.join("entry.js").to_string_lossy().into_owned(),
                }]),
                cwd: Some(dir.clone()),
                ..Default::default()
            })
            .with_plugins(vec![rolldown_plugin::__inner::Pluginable::new_shared(
                LoadThreads(Arc::clone(&counts)),
            )])
            .build()
            .unwrap();
        let rolldown = build().unwrap();
        let built = caller().block_on(on_rolldown(rolldown.handle(), async {
            let out = bundler.generate().await;
            let _ = bundler.close().await;
            out
        }));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(built.is_ok());
        assert_eq!(counts.loads.load(Ordering::Relaxed), 51);
        assert_eq!(counts.elsewhere.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn the_callers_runtime_stays_current_outside() {
        let rolldown = build().unwrap();
        let name = caller().block_on(async {
            on_rolldown(rolldown.handle(), async {}).await;
            tokio::spawn(async { std::thread::current().name().map(str::to_owned) })
                .await
                .unwrap()
        });
        assert_ne!(name.as_deref(), Some("oj-rolldown"));
    }
}
