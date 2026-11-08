//! Start dev hot updates: the client bundle's DevEngine patches
//! (start_bundle::hot) on their way to the pages, as Vite's bundled dev
//! serves them (server/bundledDev.ts). The page a patch belongs to gets an
//! `oj:hmr` frame on /@oj-start/hmr and fetches the patch from
//! /@oj-start/hmr/<file>.

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// Where a page fetches its patches.
pub(crate) const PATCH_PREFIX: &str = "/@oj-start/hmr/";
/// Patches a page fetches right after its frame; older ones only matter to a
/// page far behind, which the sequence check reloads anyway.
const KEPT_PATCHES: usize = 64;

/// One page's hot update, as the dev engine made it.
#[derive(Debug, Clone)]
pub(crate) struct Patch {
    pub client: String,
    pub filename: String,
    pub seq: u32,
    pub changed_ids: Vec<String>,
    pub code: String,
}

/// A frame for the pages: one page's patch, or (client `None`) every page's.
#[derive(Debug)]
pub(crate) struct Frame {
    pub client: Option<String>,
    pub text: String,
}

pub(crate) struct HotUpdates {
    tx: broadcast::Sender<Arc<Frame>>,
    patches: Mutex<VecDeque<(String, axum::body::Bytes)>>,
    /// Patches sent so far. A patch outruns the client output (the next
    /// written output carries it), so each one marks that output stale
    /// until an output generated after it is served.
    generation: std::sync::atomic::AtomicU64,
    stale: tokio::sync::watch::Sender<bool>,
    /// Where pages fetch a patch: [`PATCH_PREFIX`] under the dev `base`.
    patch_prefix: String,
    /// Changes sent to the pages in place: one per batch of patches (every
    /// page's patch for one edit) and one per style refresh. Each frame
    /// carries its change; a page reports the newest it applied when its
    /// socket connects, and one that is behind reloads ([`HotUpdates::behind`]).
    /// A frame goes once, to whoever is connected: the count is what makes
    /// a page that missed one (a socket that dropped, a document still
    /// loading) find out, as the reload count does for reloads.
    changes: std::sync::atomic::AtomicU64,
    /// The editor's granular HMR gate: while it holds, frames park here
    /// until its flush ([`HotUpdates::release`]). The check and the push share the
    /// lock a release drains under, so a frame is never stranded by a flush
    /// landing between them.
    gate: std::sync::OnceLock<crate::start_reloads::Held>,
    parked: Mutex<Vec<Arc<Frame>>>,
}

impl HotUpdates {
    pub(crate) fn new(url_base: &str) -> HotUpdates {
        HotUpdates {
            patch_prefix: format!("{url_base}{PATCH_PREFIX}"),
            tx: broadcast::channel(64).0,
            patches: Mutex::new(VecDeque::new()),
            generation: std::sync::atomic::AtomicU64::new(0),
            stale: tokio::sync::watch::Sender::new(false),
            gate: std::sync::OnceLock::new(),
            parked: Mutex::new(Vec::new()),
            changes: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// A new change for the pages, before its frames go out: the batch of
    /// patches one edit makes (one per page), counted once.
    pub(crate) fn begin_change(&self) -> u64 {
        self.changes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
    }

    /// `change` went out: every page hears which clients it covered (a patch
    /// or nothing to patch). A page left out (its client registered while
    /// the change was being built, or its patch never made it) is behind and
    /// reloads; a page named in it has the change. After the change's
    /// patches, through the same gate, so it never overtakes them.
    pub(crate) fn end_change(&self, change: u64, covered: &[String]) {
        let text = serde_json::json!({
            "type": "oj:change",
            "change": change,
            "clients": covered,
        })
        .to_string();
        self.send(Frame { client: None, text });
    }

    /// The changes sent so far.
    pub(crate) fn changes(&self) -> u64 {
        self.changes.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Whether a page that has applied the changes up to `has` missed one.
    /// A page that names none (no hot client, an older document) is never
    /// behind here; the reload count still covers it.
    pub(crate) fn behind(&self, has: Option<u64>) -> bool {
        has.is_some_and(|has| self.changes() > has)
    }

    /// Hold the pages' frames while `held` says the editor's gate holds.
    pub(crate) fn set_gate(&self, held: crate::start_reloads::Held) {
        let _ = self.gate.set(held);
    }

    /// The gate flushed: send what it held, in order. Sent under the lock, so
    /// a frame made after the flush cannot overtake them (a patch sequence
    /// gap reloads the page).
    pub(crate) fn release(&self) {
        let mut parked = self
            .parked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for frame in parked.drain(..) {
            let _ = self.tx.send(frame);
        }
    }

    /// The gate flushed with a page reload: the parked frames are moot.
    pub(crate) fn discard(&self) {
        self.parked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    fn send(&self, frame: Frame) {
        let frame = Arc::new(frame);
        let mut parked = self
            .parked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.gate.get().is_some_and(|held| held()) {
            parked.push(frame);
        } else {
            let _ = self.tx.send(frame);
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// An output generated after `generation` patches is now served: fresh,
    /// unless a patch went out meanwhile.
    pub(crate) fn served_output_of(&self, generation: u64) {
        self.stale.send_if_modified(|stale| {
            let fresh = self.generation() == generation;
            let changed = *stale && fresh;
            if changed {
                *stale = false;
            }
            changed
        });
    }

    pub(crate) fn stale(&self) -> tokio::sync::watch::Receiver<bool> {
        self.stale.subscribe()
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Arc<Frame>> {
        self.tx.subscribe()
    }

    /// Every page's stylesheets after an update applied in place: `relink`
    /// re-links the ones it has (a Tailwind sheet's rules follow the sources
    /// the update changed), `add` links the ones it newly imported.
    pub(crate) fn refresh_styles(&self, relink: bool, add: &[String]) {
        let change = self.begin_change();
        let text = serde_json::json!({
            "type": "oj:css",
            "relink": relink,
            "add": add,
            "change": change,
        })
        .to_string();
        self.send(Frame { client: None, text });
    }

    /// A patch for its page: kept for the page to fetch, announced on its
    /// socket, and marking the written client output stale.
    pub(crate) fn accept(&self, patch: &Patch) {
        let filename = patch.filename.as_str();
        if filename.contains('/') {
            return;
        }
        // Vite's payloadDeliveredAck: the page reports the patch once its
        // factories registered, never on the response alone. The trailing
        // export keeps it an ES module, which a classic script cannot load
        // cross-origin (Vite's guard against GHSA-4v9v-hfq4-rm2v).
        let body = format!(
            "{}\n;__rolldown_runtime__.payloadDelivered({});\n; export {{}}",
            patch.code,
            serde_json::to_string(filename).unwrap_or_default()
        );
        {
            let mut kept = self
                .patches
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if kept.len() >= KEPT_PATCHES {
                kept.pop_front();
            }
            kept.push_back((filename.to_string(), body.into()));
        }
        let text = serde_json::json!({
            "type": "oj:hmr",
            "url": format!("{}{filename}", self.patch_prefix),
            "seq": patch.seq,
            "changedIds": patch.changed_ids,
            "change": self.changes(),
        })
        .to_string();
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.stale.send_replace(true);
        self.send(Frame {
            client: Some(patch.client.clone()),
            text,
        });
    }

    pub(crate) fn serve(&self, name: &str) -> Response {
        let kept = self
            .patches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match kept.iter().rev().find(|(n, _)| n == name) {
            Some((_, body)) => (
                [
                    (header::CONTENT_TYPE, "text/javascript"),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                body.clone(),
            )
                .into_response(),
            None => (StatusCode::NOT_FOUND, "oj start: no such hot update").into_response(),
        }
    }
}

/// The hot changes a page has (`h` in its socket's or script's query), when
/// it names them.
pub(crate) fn hot_change_in(query: Option<&str>) -> Option<u64> {
    query?
        .split('&')
        .find_map(|pair| pair.strip_prefix("h="))?
        .parse()
        .ok()
}

/// What a page says on /@oj-start/hmr.
#[derive(Debug, PartialEq)]
pub(crate) enum PageMessage {
    /// The page's runtime id (hmr-runtime.js), its address for patches.
    Client(String),
    /// The page ran a patch.
    Delivered(String),
    /// The page found no boundary for an update and needs a reload.
    ReloadNeeded,
    /// `import.meta.hot.send(event, data)`, the data as JSON.
    Custom { event: String, data: String },
}

pub(crate) fn page_message(text: &str) -> Option<PageMessage> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let field = |k: &str| v.get(k).and_then(|s| s.as_str()).map(str::to_string);
    match v.get("type")?.as_str()? {
        "oj:client" => field("id")
            .filter(|id| !id.is_empty())
            .map(PageMessage::Client),
        "oj:delivered" => field("filename").map(PageMessage::Delivered),
        "oj:reload-needed" => Some(PageMessage::ReloadNeeded),
        "custom" => Some(PageMessage::Custom {
            event: field("event")?,
            data: v
                .get("data")
                .unwrap_or(&serde_json::Value::Null)
                .to_string(),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch(client: &str, file: &str, seq: u32) -> Patch {
        Patch {
            client: client.into(),
            filename: file.into(),
            seq,
            changed_ids: vec!["src/a.tsx".into()],
            code: "__rolldown_runtime__.registerFactory(\"src/a.tsx\", () => {});".into(),
        }
    }

    #[tokio::test]
    async fn a_patch_goes_to_its_page_and_is_served_with_the_delivery_ack() {
        let hot = HotUpdates::new("");
        let mut rx = hot.subscribe();
        hot.accept(&patch("c1", "hmr_patch_0.js", 1));
        let frame = rx.recv().await.unwrap();
        assert_eq!(frame.client.as_deref(), Some("c1"));
        let v: serde_json::Value = serde_json::from_str(&frame.text).unwrap();
        assert_eq!(v["type"], "oj:hmr");
        assert_eq!(v["url"], "/@oj-start/hmr/hmr_patch_0.js");
        assert_eq!(v["seq"], 1);
        assert_eq!(v["changedIds"][0], "src/a.tsx");

        let resp = hot.serve("hmr_patch_0.js");
        assert_eq!(resp.status(), StatusCode::OK);

        // Under a dev `base`, pages fetch the patch there.
        let based = HotUpdates::new("/app");
        let mut rx = based.subscribe();
        based.accept(&patch("c1", "hmr_patch_1.js", 1));
        let v: serde_json::Value = serde_json::from_str(&rx.recv().await.unwrap().text).unwrap();
        assert_eq!(v["url"], "/app/@oj-start/hmr/hmr_patch_1.js");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.starts_with("__rolldown_runtime__.registerFactory"));
        assert!(body.contains("__rolldown_runtime__.payloadDelivered(\"hmr_patch_0.js\")"));
        assert!(body.ends_with("export {}"));
        assert_eq!(hot.serve("hmr_patch_9.js").status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn only_recent_patches_are_kept_and_escaping_names_are_dropped() {
        let hot = HotUpdates::new("");
        for i in 0..(KEPT_PATCHES as u32 + 5) {
            hot.accept(&patch("c1", &format!("hmr_patch_{i}.js"), i + 1));
        }
        assert_eq!(hot.serve("hmr_patch_0.js").status(), StatusCode::NOT_FOUND);
        assert_eq!(
            hot.serve(&format!("hmr_patch_{}.js", KEPT_PATCHES + 4))
                .status(),
            StatusCode::OK
        );
        hot.accept(&patch("c1", "../escape.js", 1));
        assert_eq!(hot.serve("../escape.js").status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_style_refresh_tells_every_page_what_to_relink_and_link() {
        let hot = HotUpdates::new("");
        let mut rx = hot.subscribe();
        hot.refresh_styles(false, &["/@oj-start/fs/app/src/new.css".to_string()]);
        let frame = rx.recv().await.unwrap();
        assert_eq!(frame.client, None);
        let v: serde_json::Value = serde_json::from_str(&frame.text).unwrap();
        assert_eq!(v["type"], "oj:css");
        assert_eq!(v["relink"], false);
        assert_eq!(v["add"][0], "/@oj-start/fs/app/src/new.css");
    }

    #[tokio::test]
    async fn a_held_gate_parks_frames_until_its_flush_releases_them_in_order() {
        let hot = HotUpdates::new("");
        let held = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let h = Arc::clone(&held);
        hot.set_gate(Arc::new(move || {
            h.load(std::sync::atomic::Ordering::SeqCst)
        }));
        let mut rx = hot.subscribe();
        hot.accept(&patch("c1", "hmr_patch_0.js", 1));
        hot.refresh_styles(true, &[]);
        assert!(
            rx.try_recv().is_err(),
            "nothing reaches the pages while held"
        );
        // The patch is fetchable already: only its announcement waits.
        assert_eq!(hot.serve("hmr_patch_0.js").status(), StatusCode::OK);

        held.store(false, std::sync::atomic::Ordering::SeqCst);
        hot.release();
        let first: serde_json::Value =
            serde_json::from_str(&rx.recv().await.unwrap().text).unwrap();
        let second: serde_json::Value =
            serde_json::from_str(&rx.recv().await.unwrap().text).unwrap();
        assert_eq!(first["type"], "oj:hmr");
        assert_eq!(second["type"], "oj:css");

        // Released, the gate passes frames straight through.
        hot.accept(&patch("c1", "hmr_patch_1.js", 2));
        let v: serde_json::Value = serde_json::from_str(&rx.recv().await.unwrap().text).unwrap();
        assert_eq!(v["seq"], 2);
        hot.release();
        assert!(
            rx.try_recv().is_err(),
            "a release with nothing parked sends nothing"
        );

        // A flush that reloads the pages drops what was parked.
        held.store(true, std::sync::atomic::Ordering::SeqCst);
        hot.accept(&patch("c1", "hmr_patch_2.js", 3));
        held.store(false, std::sync::atomic::Ordering::SeqCst);
        hot.discard();
        hot.release();
        assert!(rx.try_recv().is_err(), "a discarded patch reached the page");
    }

    #[tokio::test]
    async fn every_frame_carries_its_change_and_one_edit_is_one_change() {
        let hot = HotUpdates::new("");
        let mut rx = hot.subscribe();
        // One edit, two pages: one change, a patch for each.
        let change = hot.begin_change();
        hot.accept(&patch("c1", "hmr_patch_0.js", 1));
        hot.accept(&patch("c2", "hmr_patch_1.js", 1));
        for _ in 0..2 {
            let v: serde_json::Value =
                serde_json::from_str(&rx.recv().await.unwrap().text).unwrap();
            assert_eq!(v["change"], change);
        }
        // A style refresh is a change of its own.
        hot.refresh_styles(true, &[]);
        let v: serde_json::Value = serde_json::from_str(&rx.recv().await.unwrap().text).unwrap();
        assert_eq!(v["type"], "oj:css");
        assert_eq!(v["change"], change + 1);
        assert_eq!(hot.changes(), change + 1);
    }

    #[tokio::test]
    async fn a_change_ends_by_naming_every_page_it_covered_after_its_patches() {
        let hot = HotUpdates::new("");
        let held = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let h = Arc::clone(&held);
        hot.set_gate(Arc::new(move || {
            h.load(std::sync::atomic::Ordering::SeqCst)
        }));
        let mut rx = hot.subscribe();
        let change = hot.begin_change();
        hot.accept(&patch("c1", "hmr_patch_0.js", 1));
        hot.end_change(change, &["c1".to_string(), "c2".to_string()]);
        assert!(rx.try_recv().is_err(), "held like the patches");
        held.store(false, std::sync::atomic::Ordering::SeqCst);
        hot.release();
        let first: serde_json::Value =
            serde_json::from_str(&rx.recv().await.unwrap().text).unwrap();
        let end = rx.recv().await.unwrap();
        assert_eq!(first["type"], "oj:hmr", "the patch first");
        assert_eq!(end.client, None, "every page hears it");
        let end: serde_json::Value = serde_json::from_str(&end.text).unwrap();
        assert_eq!(end["type"], "oj:change");
        assert_eq!(end["change"], change);
        assert_eq!(end["clients"], serde_json::json!(["c1", "c2"]));
    }

    #[test]
    fn a_page_is_behind_only_when_a_change_it_never_applied_went_out() {
        let hot = HotUpdates::new("");
        // A page that names nothing is left to the reload count.
        assert!(!hot.behind(None));
        assert!(!hot.behind(Some(0)));
        let first = hot.begin_change();
        // Rendered before it, or its socket dropped as it went out.
        assert!(hot.behind(Some(0)));
        // Applied it: current.
        assert!(!hot.behind(Some(first)));
        hot.begin_change();
        assert!(hot.behind(Some(first)));
    }

    #[test]
    fn hot_change_parses_from_a_query() {
        assert_eq!(hot_change_in(Some("g=ff-3&h=7")), Some(7));
        assert_eq!(hot_change_in(Some("h=0")), Some(0));
        assert_eq!(hot_change_in(Some("g=ff-3")), None);
        assert_eq!(hot_change_in(Some("h=x")), None);
        assert_eq!(hot_change_in(None), None);
    }

    #[test]
    fn page_messages_parse() {
        assert_eq!(
            page_message(r#"{"type":"oj:client","id":"abc"}"#),
            Some(PageMessage::Client("abc".into()))
        );
        assert_eq!(
            page_message(r#"{"type":"oj:delivered","filename":"hmr_patch_1.js"}"#),
            Some(PageMessage::Delivered("hmr_patch_1.js".into()))
        );
        assert_eq!(
            page_message(r#"{"type":"oj:reload-needed","reason":"x"}"#),
            Some(PageMessage::ReloadNeeded)
        );
        assert_eq!(
            page_message(r#"{"type":"custom","event":"app:ping","data":{"n":1}}"#),
            Some(PageMessage::Custom {
                event: "app:ping".into(),
                data: r#"{"n":1}"#.into()
            })
        );
        assert_eq!(
            page_message(r#"{"type":"custom","event":"app:ping"}"#),
            Some(PageMessage::Custom {
                event: "app:ping".into(),
                data: "null".into()
            })
        );
        assert_eq!(page_message(r#"{"type":"custom","data":1}"#), None);
        assert_eq!(page_message(r#"{"type":"oj:client","id":""}"#), None);
        assert_eq!(page_message("ping"), None);
    }
}
