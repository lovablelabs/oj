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
    pub(crate) fn flushed(&self) {
        let latest = self
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.publish_locked(*latest);
    }

    fn publish_locked(&self, latest: u64) {
        self.published.send_if_modified(|p| {
            let moved = *p < latest;
            *p = latest.max(*p);
            moved
        });
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
}
