// SPDX-License-Identifier: MIT

//! The page reloads of Start dev, as two numbers instead of messages.
//!
//! `latest` counts the changes pages must reload for. `published` is where
//! the pages are told to be: it follows `latest`, except that while the
//! editor's HMR gate holds a reload it waits for the flush. A document is
//! stamped with `latest` when it renders (the server renders the newest
//! code, gate or not), and its socket reloads it whenever `published` passes
//! that stamp: on connect too, so a page still loading when a change went out
//! catches up, and a flush that lands while a rebuild runs publishes nothing
//! new, leaving the rebuild's end to reload the pages once, onto its output.
//!
//! The hot path does the same with its patch sequence (hmr-runtime.js
//! reloads on a gap): a page compares a number, so a lost message cannot
//! leave it behind.

use std::sync::{Arc, Mutex};
use tokio::sync::watch;

/// Whether the editor's gate holds the pages' reload.
pub(crate) type Held = Arc<dyn Fn() -> bool + Send + Sync>;

pub(crate) struct Epochs {
    /// This server's id in page stamps: a stamp from another run of the dev
    /// server says nothing about this one's counts.
    boot: u64,
    latest: Mutex<u64>,
    published: watch::Sender<u64>,
    held: Option<Held>,
}

impl Epochs {
    /// `held` is the gate's hold (None without a gate: every change publishes).
    pub(crate) fn new(held: Option<Held>) -> Self {
        let boot = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        Self {
            boot,
            latest: Mutex::new(0),
            published: watch::channel(0).0,
            held,
        }
    }

    /// A change the pages must reload for. True when it reached them now,
    /// false when the gate holds it for the editor's flush.
    pub(crate) fn bump(&self) -> bool {
        let mut latest = self
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *latest += 1;
        // Read after the count moved: a flush clearing the hold from here on
        // publishes this change itself.
        if self.held.as_ref().is_some_and(|held| held()) {
            return false;
        }
        self.publish_locked(*latest);
        true
    }

    /// The gate released its hold (the editor's flush or the hold cap):
    /// publish what changed meanwhile. A change still being built publishes
    /// when its run bumps, so a flush never reloads onto the old output.
    /// True when this published a reload the pages had not been sent.
    pub(crate) fn flushed(&self) -> bool {
        let latest = self
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.publish_locked(*latest)
    }

    /// Whether a change the pages must reload for is still unpublished (the
    /// gate holds it, or a run in flight will publish it at its end).
    pub(crate) fn parked(&self) -> bool {
        let latest = *self
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        latest > *self.published.borrow()
    }

    fn publish_locked(&self, latest: u64) -> bool {
        self.published.send_if_modified(|p| {
            let moved = *p < latest;
            *p = latest.max(*p);
            moved
        })
    }

    /// The stamp of a document rendered now.
    pub(crate) fn stamp(&self) -> String {
        let latest = *self
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        format!("{:x}-{latest}", self.boot)
    }

    /// The pages' reloads, for a page whose socket asks with `query`. Its
    /// stamp is what the page has; a page with none (or one from another
    /// server run) counts as current when it connects.
    pub(crate) fn page(&self, query: Option<&str>) -> PageEpoch {
        let rx = self.published.subscribe();
        let has = stamp_in(query)
            .and_then(|s| self.count_of(&s))
            .unwrap_or_else(|| *rx.borrow());
        PageEpoch { rx, has }
    }

    #[cfg(test)]
    pub(crate) fn receivers(&self) -> usize {
        self.published.receiver_count()
    }

    fn count_of(&self, stamp: &str) -> Option<u64> {
        let (boot, n) = stamp.split_once('-')?;
        (u64::from_str_radix(boot, 16).ok()? == self.boot)
            .then(|| n.parse().ok())
            .flatten()
    }
}

/// The `g` a url's query carries: a page's stamp, when it is one.
pub(crate) fn stamp_in(query: Option<&str>) -> Option<String> {
    let raw = query?.split('&').find_map(|pair| pair.strip_prefix("g="))?;
    let (boot, n) = raw.split_once('-')?;
    let hex = !boot.is_empty() && boot.bytes().all(|b| b.is_ascii_hexdigit());
    let digits = !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit());
    (hex && digits).then(|| raw.to_string())
}

/// One page's view of the published count.
pub(crate) struct PageEpoch {
    rx: watch::Receiver<u64>,
    has: u64,
}

impl PageEpoch {
    /// Whether the page is behind now (checked once, on connect).
    pub(crate) fn behind(&mut self) -> bool {
        *self.rx.borrow_and_update() > self.has
    }

    /// Waits until the page falls behind. None once the server is gone.
    pub(crate) async fn fell_behind(&mut self) -> Option<()> {
        loop {
            self.rx.changed().await.ok()?;
            if *self.rx.borrow_and_update() > self.has {
                return Some(());
            }
        }
    }
}

/// When a gate flush takes effect against the rebundle worker's runs.
///
/// A flush releases what the gate holds: the parked reloads publish and the
/// parked hot updates go out (or are dropped, when a reload makes them moot).
/// One that lands while a rebundle runs must not do that yet: the counts it
/// would publish can hold a change unrelated to the run (the boot's
/// revalidation found a module edited while it warmed, and bumped), and
/// publishing then reloads the pages onto the output from before the run,
/// after which the run's own end reloads them again. The flush instead waits
/// for the run, whose end releases everything once, onto its output. The
/// editor's gate for a plain Vite dev server has the same shape: the flush
/// consumes the hold at once, the reload goes out with the rebuild's result.
///
/// Lock-free, so a flush and a run's end can cross: whichever of the two
/// sees the other's mark applies the release, and exactly one of them does.
#[derive(Default)]
pub(crate) struct FlushTiming {
    /// A relevant change was seen and its run has not ended yet.
    rebundling: std::sync::atomic::AtomicBool,
    /// A flush landed while `rebundling`, for the run's end to apply.
    deferred: std::sync::atomic::AtomicBool,
    /// The watcher has seen a change this session (never cleared).
    seen: std::sync::atomic::AtomicBool,
    /// The watcher thread is past its initial scan and reading its feed.
    armed: std::sync::atomic::AtomicBool,
    /// The watcher thread is waiting on an empty feed: every event that
    /// reached it has been classified.
    idle: std::sync::atomic::AtomicBool,
}

impl FlushTiming {
    /// A run was asked for (a deferred bundle): in flight until it ends.
    pub(crate) fn rebundle_wanted(&self) {
        self.rebundling
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// The watcher saw a change a run will take (set at the event, where the
    /// gate takes its hold, so no flush can slip between the two).
    pub(crate) fn watcher_change(&self) {
        self.seen.store(true, std::sync::atomic::Ordering::SeqCst);
        self.rebundle_wanted();
    }

    /// Whether the watcher's runs cover a reload the boot's revalidation
    /// would send: it is for an edit the watcher never gets (a file outside
    /// what it watches), which nothing else reloads for; one the watcher saw
    /// is rebuilt and reloaded by its run (whose output the revalidation's
    /// reload would predate), so once a change has been seen, or a run is in
    /// flight, the revalidation only drops its stale modules and sends
    /// nothing. Meaningful once the watcher is [caught up](Self::watcher_caught_up):
    /// an edit it has yet to read from its feed is not a seen change.
    pub(crate) fn runs_cover_reloads(&self) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        self.seen.load(SeqCst) || self.rebundling.load(SeqCst)
    }

    /// The watcher thread finished its initial scan and reads its feed.
    pub(crate) fn watcher_armed(&self) {
        self.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether the watcher thread waits on an empty feed (true) or has an
    /// event in hand (false).
    pub(crate) fn watcher_idle(&self, idle: bool) {
        self.idle.store(idle, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether the watcher has classified every event that reached it: it
    /// is past its scan and its feed is empty. Events queue on the feed
    /// while the scan runs (the OS watcher was registered before the engine
    /// warmed), so an edit the boot's revalidation finds stale is one the
    /// watcher will read, however long the scan takes; until then it is not
    /// a seen change, and a reload sent for it from elsewhere doubles the
    /// run's own (and, under the gate, passes the hold the event takes).
    pub(crate) fn watcher_caught_up(&self) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        self.armed.load(SeqCst) && self.idle.load(SeqCst)
    }

    /// A flush landed: true to release now, false when a run in flight takes
    /// it (its end then reports it from `run_ended`).
    pub(crate) fn flush_now(&self) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        // Marked before the check: a run ending between the two finds the
        // mark and applies it, or clears `rebundling` first and the swap
        // below takes the mark back.
        self.deferred.store(true, SeqCst);
        if self.rebundling.load(SeqCst) {
            return false;
        }
        self.deferred.swap(false, SeqCst)
    }

    /// A run ended; `more` when another is queued behind it. True when a
    /// flush landed during the run and is this caller's to apply.
    pub(crate) fn run_ended(&self, more: bool) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        if !more {
            self.rebundling.store(false, SeqCst);
        }
        self.deferred.swap(false, SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};

    fn gated() -> (Epochs, Arc<AtomicBool>) {
        let hold = Arc::new(AtomicBool::new(false));
        let h = Arc::clone(&hold);
        (Epochs::new(Some(Arc::new(move || h.load(SeqCst)))), hold)
    }

    fn query(stamp: &str) -> String {
        format!("g={stamp}")
    }

    /// True when the page reloads within a moment.
    async fn reloads(page: &mut PageEpoch) -> bool {
        tokio::time::timeout(std::time::Duration::from_millis(50), page.fell_behind())
            .await
            .is_ok()
    }

    #[tokio::test]
    async fn without_a_gate_every_change_reloads_the_pages() {
        let epochs = Epochs::new(None);
        let mut page = epochs.page(Some(&query(&epochs.stamp())));
        assert!(!page.behind());
        assert!(epochs.bump());
        assert!(reloads(&mut page).await);
    }

    #[tokio::test]
    async fn a_page_loading_while_a_change_went_out_catches_up_on_connect() {
        let epochs = Epochs::new(None);
        let stamp = epochs.stamp();
        epochs.bump();
        assert!(epochs.page(Some(&query(&stamp))).behind());
        assert!(!epochs.page(Some(&query(&epochs.stamp()))).behind());
    }

    #[tokio::test]
    async fn a_flush_during_the_rebuild_leaves_one_reload_to_its_end() {
        let (epochs, hold) = gated();
        let mut page = epochs.page(Some(&query(&epochs.stamp())));
        hold.store(true, SeqCst); // the watcher event
        hold.store(false, SeqCst); // the editor's flush, mid rebuild
        epochs.flushed();
        assert!(
            !reloads(&mut page).await,
            "the flush reloaded onto the old bundle"
        );
        assert!(epochs.bump(), "the rebuild's end");
        assert!(reloads(&mut page).await);
        assert!(!reloads(&mut page).await, "one change reloaded twice");
    }

    #[tokio::test]
    async fn a_held_change_reaches_the_pages_with_the_flush() {
        let (epochs, hold) = gated();
        let mut page = epochs.page(Some(&query(&epochs.stamp())));
        hold.store(true, SeqCst);
        assert!(!epochs.bump());
        assert!(
            !reloads(&mut page).await,
            "the gate let a held reload through"
        );
        hold.store(false, SeqCst);
        epochs.flushed();
        assert!(reloads(&mut page).await);
    }

    #[tokio::test]
    async fn a_page_rendered_while_held_has_the_change_and_stays_on_the_flush() {
        let (epochs, hold) = gated();
        hold.store(true, SeqCst);
        epochs.bump();
        // Reloaded by hand: the server renders the new code.
        let mut page = epochs.page(Some(&query(&epochs.stamp())));
        hold.store(false, SeqCst);
        epochs.flushed();
        assert!(!page.behind());
        assert!(!reloads(&mut page).await);
    }

    #[tokio::test]
    async fn a_page_with_no_stamp_or_another_runs_counts_as_current() {
        let epochs = Epochs::new(None);
        epochs.bump();
        epochs.bump();
        assert!(!epochs.page(None).behind());
        assert!(!epochs.page(Some("g=x")).behind());
        assert!(!epochs.page(Some(&query("1-0"))).behind());
        let mut page = epochs.page(None);
        epochs.bump();
        assert!(
            reloads(&mut page).await,
            "a page with no stamp missed the next change"
        );
    }

    #[test]
    fn a_stamp_is_read_from_the_query() {
        assert_eq!(stamp_in(Some("a=1&g=ff-3")).as_deref(), Some("ff-3"));
        for bad in [
            "g=", "g=ff", "g=-3", "g=zz-3", "g=ff-x", "g=ff-3\"", "x=ff-3",
        ] {
            assert_eq!(stamp_in(Some(bad)), None, "{bad}");
        }
        assert_eq!(stamp_in(None), None);
    }

    #[test]
    fn a_held_change_is_parked_until_it_publishes() {
        let (epochs, hold) = gated();
        assert!(!epochs.parked());
        hold.store(true, SeqCst);
        assert!(!epochs.bump());
        assert!(epochs.parked(), "held, not published");
        hold.store(false, SeqCst);
        assert!(epochs.flushed());
        assert!(!epochs.parked());
        assert!(epochs.bump(), "published at once without a hold");
        assert!(!epochs.parked());
    }

    #[test]
    fn the_boot_revalidation_reloads_only_before_the_watcher_saw_a_change() {
        let timing = FlushTiming::default();
        assert!(
            !timing.runs_cover_reloads(),
            "no run, no change: the revalidation reloads"
        );
        timing.rebundle_wanted();
        assert!(
            timing.runs_cover_reloads(),
            "a deferred bundle in flight reloads for it"
        );
        assert!(!timing.run_ended(false));
        assert!(
            !timing.runs_cover_reloads(),
            "ended, and no watcher change yet"
        );
        timing.watcher_change();
        assert!(!timing.run_ended(false));
        assert!(
            timing.runs_cover_reloads(),
            "a watcher change stays covered after its run"
        );
    }

    #[test]
    fn a_flush_between_runs_releases_at_once() {
        let timing = FlushTiming::default();
        assert!(timing.flush_now());
        assert!(!timing.run_ended(false), "nothing waited for a run");
    }

    #[test]
    fn a_flush_during_a_run_is_released_by_the_runs_end() {
        let timing = FlushTiming::default();
        timing.rebundle_wanted();
        assert!(!timing.flush_now(), "the run in flight takes the flush");
        assert!(timing.run_ended(false), "the run's end applies it");
        assert!(!timing.run_ended(false), "once");
        assert!(
            timing.flush_now(),
            "the next flush, between runs, releases at once"
        );
    }

    #[test]
    fn a_run_with_another_queued_still_applies_the_flush_it_took() {
        let timing = FlushTiming::default();
        timing.rebundle_wanted();
        assert!(!timing.flush_now());
        assert!(timing.run_ended(true));
        // The queued run is still in flight for the next flush.
        assert!(!timing.flush_now());
        assert!(timing.run_ended(false));
    }

    #[test]
    fn a_flush_and_a_runs_end_crossing_release_exactly_once() {
        // Many interleavings of the two, each must release once in all.
        for _ in 0..200 {
            let timing = Arc::new(FlushTiming::default());
            timing.rebundle_wanted();
            let t = Arc::clone(&timing);
            let flush = std::thread::spawn(move || t.flush_now());
            let at_end = timing.run_ended(false);
            let at_flush = flush.join().unwrap();
            assert!(at_end != at_flush, "end={at_end} flush={at_flush}");
        }
    }

    /// The boot's revalidation may run while the watcher is still scanning,
    /// with the edit it found queued on the watcher's feed: the watcher is
    /// caught up only once it is armed and waiting on an empty feed, and a
    /// change it then has seen covers the reload.
    #[test]
    fn the_watcher_is_caught_up_once_armed_and_waiting_on_an_empty_feed() {
        let timing = FlushTiming::default();
        assert!(!timing.watcher_caught_up(), "scanning");
        timing.watcher_idle(true);
        assert!(
            !timing.watcher_caught_up(),
            "idle before the scan ended is not caught up"
        );
        timing.watcher_armed();
        assert!(timing.watcher_caught_up());
        assert!(
            !timing.runs_cover_reloads(),
            "nothing seen: the revalidation reloads"
        );
        timing.watcher_idle(false);
        assert!(!timing.watcher_caught_up(), "an event in hand");
        timing.watcher_change();
        timing.watcher_idle(true);
        assert!(timing.watcher_caught_up());
        assert!(
            timing.runs_cover_reloads(),
            "the seen change's run covers the reload"
        );
    }
}
