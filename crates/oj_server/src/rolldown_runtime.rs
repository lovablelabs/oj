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
use std::sync::LazyLock;
use std::task::{Context, Poll};

use tokio::runtime::{Handle, Runtime};

/// rolldown's binding default.
pub const MAX_BLOCKING_THREADS: usize = 4;

static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads((cpus * 3 / 2).max(1))
        .max_blocking_threads(MAX_BLOCKING_THREADS)
        .thread_name("oj-rolldown")
        .thread_stack_size(oj_compiler::COMPILE_STACK_SIZE)
        .enable_all()
        .build()
        .expect("the rolldown runtime builds")
});

/// The rolldown runtime's handle.
pub fn handle() -> &'static Handle {
    RUNTIME.handle()
}

/// Polls `fut` with the rolldown runtime as the current one, so the tasks
/// and blocking reads it starts run there. The caller still awaits it from
/// its own runtime; nothing needs to be `'static`.
pub fn on_rolldown<F: Future>(fut: F) -> OnRolldown<F> {
    OnRolldown { fut: Box::pin(fut) }
}

pub struct OnRolldown<F> {
    fut: Pin<Box<F>>,
}

impl<F: Future> Future for OnRolldown<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let _entered = handle().enter();
        self.fut.as_mut().poll(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    #[tokio::test(flavor = "multi_thread")]
    async fn spawned_work_runs_on_the_rolldown_runtime() {
        let names = on_rolldown(async {
            let task = tokio::spawn(async { std::thread::current().name().map(str::to_owned) });
            let blocking = tokio::runtime::Handle::current()
                .spawn_blocking(|| std::thread::current().name().map(str::to_owned));
            (task.await.unwrap(), blocking.await.unwrap())
        })
        .await;
        assert_eq!(names.0.as_deref(), Some("oj-rolldown"));
        assert_eq!(names.1.as_deref(), Some("oj-rolldown"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_flood_of_blocking_reads_stays_within_the_cap() {
        // What a build does: one blocking read per module, all at once.
        let threads = Arc::new(Mutex::new(HashSet::new()));
        on_rolldown(async {
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
        })
        .await;
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

    #[tokio::test(flavor = "multi_thread")]
    async fn a_build_runs_its_module_tasks_on_the_rolldown_runtime() {
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
        let built = on_rolldown(async {
            let out = bundler.generate().await;
            let _ = bundler.close().await;
            out
        })
        .await;
        let _ = std::fs::remove_dir_all(&dir);
        assert!(built.is_ok());
        assert_eq!(counts.loads.load(Ordering::Relaxed), 51);
        assert_eq!(counts.elsewhere.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn the_callers_runtime_stays_current_outside() {
        on_rolldown(async {}).await;
        let name = tokio::spawn(async { std::thread::current().name().map(str::to_owned) })
            .await
            .unwrap();
        assert_ne!(name.as_deref(), Some("oj-rolldown"));
    }
}
