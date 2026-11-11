// tsrs-only: Go starts a goroutine per task (`sync.WaitGroup.Go`, `core.WorkGroup`); the language service runs such
// tasks on scoped threads instead, one per core at most.

/// Stack of each thread: checker recursion needs what the checker threads get (`CHECKER_STACK_SIZE`, `tsrs_compiler`).
const STACK_SIZE: usize = 512 << 20;

/// Runs `work` on every item and returns the results in item order.
///
/// One item runs on the calling thread; more run on up to one scoped thread per core. Each worker calls `around` once.
/// `around` sets up what the calling thread has and the others lack (an allocation region, a checker data lease).
pub fn run_concurrently<T: Send, R: Send>(
    items: Vec<T>,
    work: &(impl Fn(T) -> R + Sync),
    around: &(impl Fn(&mut dyn FnMut()) + Sync),
) -> Vec<R> {
    if items.len() <= 1 || tsrs_core::NO_THREADS {
        return items.into_iter().map(work).collect();
    }
    let threads =
        std::thread::available_parallelism().map_or(1, std::num::NonZero::get).min(items.len());
    let count = items.len();
    let queue = std::sync::Mutex::new(items.into_iter().enumerate().rev().collect::<Vec<_>>());
    let done = std::sync::Mutex::new(Vec::with_capacity(count));
    tsrs_core::runslots::blocking(|| {
        std::thread::scope(|scope| {
            for _ in 0..threads {
                std::thread::Builder::new()
                    .name("tsrs-ls-worker".to_owned())
                    .stack_size(STACK_SIZE)
                    .spawn_scoped(scope, || {
                        tsrs_core::runslots::run(|| {
                            around(&mut || {
                                loop {
                                    let next = queue.lock().unwrap().pop();
                                    let Some((index, item)) = next else { break };
                                    let result = work(item);
                                    done.lock().unwrap().push((index, result));
                                }
                            });
                        });
                    })
                    .expect("failed to spawn a language service worker thread");
            }
        });
    });
    let mut done = done.into_inner().unwrap();
    done.sort_by_key(|(index, _)| *index);
    done.into_iter().map(|(_, result)| result).collect()
}
