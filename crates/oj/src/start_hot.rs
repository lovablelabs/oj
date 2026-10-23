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
}

impl HotUpdates {
    pub(crate) fn new(url_base: &str) -> HotUpdates {
        HotUpdates {
            patch_prefix: format!("{url_base}{PATCH_PREFIX}"),
            tx: broadcast::channel(64).0,
            patches: Mutex::new(VecDeque::new()),
            generation: std::sync::atomic::AtomicU64::new(0),
            stale: tokio::sync::watch::Sender::new(false),
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
        let text =
            serde_json::json!({ "type": "oj:css", "relink": relink, "add": add }).to_string();
        let _ = self.tx.send(Arc::new(Frame { client: None, text }));
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
        })
        .to_string();
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.stale.send_replace(true);
        let _ = self.tx.send(Arc::new(Frame {
            client: Some(patch.client.clone()),
            text,
        }));
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
