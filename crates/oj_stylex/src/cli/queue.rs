//! Bounded, ordered job feed for `batch` and `serve`: readers publish blocks in
//! stdin order, workers claim indices with one atomic add and take their item.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

/// Bytes an item holds in the backlog.
pub trait Weigh {
    fn weight(&self) -> usize;
}

impl Weigh for String {
    fn weight(&self) -> usize {
        self.len()
    }
}

/// Backlog and block sizes, scaled by worker count: one worker buffers a few
/// dozen jobs, a full machine up to 1,024 lines / 8 MiB.
#[derive(Clone, Copy)]
pub struct Bounds {
    pub backlog_lines: usize,
    pub backlog_bytes: usize,
    /// Lines one reader takes from stdin per turn; small, so turns rotate fast.
    pub block_lines: usize,
    /// Bytes a reader publishes at once before it keeps going.
    pub block_bytes: usize,
    /// Fewer queued items than this means the workers are starving.
    pub low_water: usize,
}

impl Bounds {
    pub fn for_threads(threads: usize) -> Self {
        let scale = threads.clamp(1, 32);
        Self {
            backlog_lines: 32 * scale,
            backlog_bytes: (256 << 10) * scale,
            block_lines: 32,
            block_bytes: (32 << 10) * scale,
            low_water: 2 * scale,
        }
    }
}

struct State<T> {
    items: VecDeque<Option<T>>,
    base: usize,
    backlog_bytes: usize,
    next_seq: u64,
    done: bool,
    aborted: bool,
    reader_waiting: bool,
}

pub struct JobQueue<T> {
    next: AtomicUsize,
    // Absolute publication count keeps starvation checks independent of reclamation.
    published_len: AtomicUsize,
    state: Mutex<State<T>>,
    bounds: Bounds,
    published: Condvar,
    drained: Condvar,
    turn: Condvar,
}

impl<T: Weigh> JobQueue<T> {
    pub fn new(bounds: Bounds) -> Self {
        Self {
            next: AtomicUsize::new(0),
            published_len: AtomicUsize::new(0),
            state: Mutex::new(State {
                items: VecDeque::new(),
                base: 0,
                backlog_bytes: 0,
                next_seq: 0,
                done: false,
                aborted: false,
                reader_waiting: false,
            }),
            bounds,
            published: Condvar::new(),
            drained: Condvar::new(),
            turn: Condvar::new(),
        }
    }

    pub fn bounds(&self) -> Bounds {
        self.bounds
    }

    /// Published items not yet claimed (a snapshot; claims may be in flight).
    pub fn backlog(&self) -> usize {
        self.published_len
            .load(Ordering::Relaxed)
            .saturating_sub(self.next.load(Ordering::Relaxed))
    }

    /// Single-reader side: appends `block` (drained) as the next sequence.
    /// `false` = a worker aborted.
    pub fn publish(&self, block: &mut Vec<T>) -> bool {
        self.publish_inner(None, true, block)
    }

    /// Pooled-reader side: `block` lands once `seq` holds the turn and the backlog
    /// admits it; `last` passes the turn on, so one sequence may arrive in pieces.
    pub fn publish_seq(&self, seq: u64, last: bool, block: &mut Vec<T>) -> bool {
        self.publish_inner(Some(seq), last, block)
    }

    fn publish_inner(&self, seq: Option<u64>, last: bool, block: &mut Vec<T>) -> bool {
        let bytes: usize = block.iter().map(Weigh::weight).sum();
        let mut state = self.state.lock().expect("job queue lock");
        loop {
            if state.aborted {
                return false;
            }
            if seq.is_some_and(|seq| seq != state.next_seq) {
                state = self.turn.wait(state).expect("job queue lock");
                continue;
            }
            let backlog = state.items.len();
            if backlog == 0
                || (backlog < self.bounds.backlog_lines
                    && state.backlog_bytes < self.bounds.backlog_bytes)
            {
                break;
            }
            state.reader_waiting = true;
            state = self.drained.wait(state).expect("job queue lock");
        }
        state.backlog_bytes += bytes;
        state.items.extend(block.drain(..).map(Some));
        self.published_len
            .store(state.base + state.items.len(), Ordering::Relaxed);
        if last {
            state.next_seq += 1;
            self.turn.notify_all();
        }
        self.published.notify_all();
        true
    }

    pub fn finish(&self) {
        self.state.lock().expect("job queue lock").done = true;
        self.published.notify_all();
    }

    /// Worker side: the next index and its item, waiting for the readers when
    /// caught up; `None` once the feed is finished and drained.
    pub fn claim(&self) -> Option<(usize, T)> {
        let i = self.next.fetch_add(1, Ordering::Relaxed);
        let mut state = self.state.lock().expect("job queue lock");
        loop {
            let offset = i - state.base;
            if let Some(slot) = state.items.get_mut(offset) {
                let item = slot.take().expect("each index is claimed once");
                state.backlog_bytes -= item.weight();
                while state.items.front().is_some_and(Option::is_none) {
                    state.items.pop_front();
                    state.base += 1;
                }
                if state.reader_waiting
                    && state.items.len() <= self.bounds.backlog_lines / 2
                    && state.backlog_bytes <= self.bounds.backlog_bytes / 2
                {
                    state.reader_waiting = false;
                    self.drained.notify_one();
                }
                return Some((i, item));
            }
            if state.done {
                return None;
            }
            state = self.published.wait(state).expect("job queue lock");
        }
    }

    /// Worker side: the result writer is gone, so the readers must stop too.
    pub fn abort(&self) {
        self.state.lock().expect("job queue lock").aborted = true;
        self.drained.notify_all();
        self.turn.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drained_requests_reuse_storage_and_preserve_absolute_indices() {
        let queue = JobQueue::new(Bounds::for_threads(1));
        for i in 0..100_000 {
            assert!(queue.publish(&mut vec![i.to_string()]));
            assert_eq!(queue.claim(), Some((i, i.to_string())));
        }
        let state = queue.state.lock().unwrap();
        assert!(state.items.is_empty());
        assert!(state.items.capacity() <= 32);
        assert_eq!(state.base, 100_000);
    }

    #[test]
    fn a_delayed_claim_keeps_its_slot_until_it_is_taken() {
        let queue = JobQueue::new(Bounds::for_threads(1));
        assert!(queue.publish(&mut vec!["first".to_owned(), "second".to_owned()]));
        assert_eq!(queue.next.fetch_add(1, Ordering::Relaxed), 0);
        assert_eq!(queue.claim(), Some((1, "second".to_owned())));
        let state = queue.state.lock().unwrap();
        assert_eq!(state.base, 0);
        assert_eq!(state.items[0].as_deref(), Some("first"));
        assert_eq!(state.items[1], None);
    }

    #[test]
    fn every_index_is_delivered_once_through_the_backlog_bound() {
        const N: usize = 5000;
        let queue = JobQueue::new(Bounds::for_threads(8));
        let got = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    while let Some(job) = queue.claim() {
                        got.lock().unwrap().push(job);
                    }
                });
            }
            let mut block = Vec::new();
            for i in 0..N {
                block.push(format!("line{i}"));
                if block.len() == 100 {
                    assert!(queue.publish(&mut block));
                }
            }
            assert!(queue.publish(&mut block));
            queue.finish();
        });
        let mut got = got.into_inner().unwrap();
        got.sort_by_key(|(i, _)| *i);
        assert_eq!(got.len(), N);
        for (k, (i, line)) in got.iter().enumerate() {
            assert_eq!(*i, k);
            assert_eq!(line, &format!("line{k}"));
        }
    }

    #[test]
    fn pooled_readers_publish_in_sequence_order_across_pieces() {
        const CHUNKS: u64 = 40;
        let queue = JobQueue::new(Bounds::for_threads(4));
        let next_chunk = Mutex::new(0u64);
        let got = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..3 {
                scope.spawn(|| {
                    while let Some(job) = queue.claim() {
                        got.lock().unwrap().push(job);
                    }
                });
            }
            scope.spawn(|| {
                std::thread::scope(|readers| {
                    for _ in 0..4 {
                        readers.spawn(|| {
                            loop {
                                let seq = {
                                    let mut next = next_chunk.lock().unwrap();
                                    if *next == CHUNKS {
                                        return;
                                    }
                                    *next += 1;
                                    *next - 1
                                };
                                // Later sequences finish first on purpose.
                                std::thread::sleep(std::time::Duration::from_micros(
                                    (CHUNKS - seq) * 20,
                                ));
                                let mut piece: Vec<String> =
                                    (0..5).map(|k| format!("{seq}:{k}")).collect();
                                assert!(queue.publish_seq(seq, false, &mut piece));
                                let mut piece: Vec<String> =
                                    (5..10).map(|k| format!("{seq}:{k}")).collect();
                                assert!(queue.publish_seq(seq, true, &mut piece));
                            }
                        });
                    }
                });
                queue.finish();
            });
        });
        let mut got = got.into_inner().unwrap();
        got.sort_by_key(|(i, _)| *i);
        assert_eq!(got.len(), (CHUNKS * 10) as usize);
        for (k, (i, line)) in got.iter().enumerate() {
            assert_eq!(*i, k);
            assert_eq!(line, &format!("{}:{}", k / 10, k % 10));
        }
    }

    #[test]
    fn backlog_counts_published_items_not_yet_claimed() {
        let queue: JobQueue<String> = JobQueue::new(Bounds::for_threads(1));
        assert_eq!(queue.backlog(), 0);
        let mut block: Vec<String> = (0..10).map(|k| k.to_string()).collect();
        assert!(queue.publish(&mut block));
        assert_eq!(queue.backlog(), 10);
        for _ in 0..4 {
            queue.claim();
        }
        assert_eq!(queue.backlog(), 6);
    }

    #[test]
    fn abort_releases_a_reader_blocked_on_the_bound() {
        let bounds = Bounds::for_threads(32);
        let queue = JobQueue::new(bounds);
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                let mut published = 0;
                loop {
                    let mut block: Vec<String> = (0..100).map(|_| "x".to_string()).collect();
                    if !queue.publish(&mut block) {
                        return published;
                    }
                    published += 100;
                }
            });
            std::thread::sleep(std::time::Duration::from_millis(50));
            queue.abort();
            let published = reader.join().unwrap();
            assert!(
                (bounds.backlog_lines..bounds.backlog_lines + 100).contains(&published),
                "{published}"
            );
        });
    }

    #[test]
    fn abort_releases_a_reader_waiting_for_its_turn() {
        let queue: JobQueue<String> = JobQueue::new(Bounds::for_threads(2));
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| queue.publish_seq(1, true, &mut vec!["late".into()]));
            std::thread::sleep(std::time::Duration::from_millis(20));
            queue.abort();
            assert!(!reader.join().unwrap());
        });
    }

    #[test]
    fn finish_wakes_a_worker_waiting_for_lines() {
        let queue: JobQueue<String> = JobQueue::new(Bounds::for_threads(1));
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| queue.claim());
            std::thread::sleep(std::time::Duration::from_millis(20));
            queue.finish();
            assert!(worker.join().unwrap().is_none());
        });
    }
}
