// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! How the Start client bundle goes over the wire: an ETag from the chunk's
//! content hash (a reload revalidates to a 304), immutable caching for the
//! bundle's hashed chunks, and gzip for clients that accept it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use oj_cache::start_bundle::{PinnedBundle, PinnedChunk};
use oj_server::etag::{not_modified, weak_etag};

/// The client bundles the dev server serves: the current one, whose entry
/// every new document loads, and the ones before it, kept for the documents
/// still open on an older entry.
///
/// A rebundle renames every chunk whose content changed (rolldown's
/// `[name]-[hash]`), and a document served before it still holds the old
/// names: its entry imports a route's chunk lazily when the user first
/// navigates there. Served from the current bundle alone, that import got a
/// 404 and the page went blank with "Failed to fetch dynamically imported
/// module" until a manual reload, which the dev server may hold back (the
/// editor's flush) or the document may have missed. Vite's dev server never
/// has this problem, since it serves modules by path and only ever changes a
/// url's `?t=` stamp; its bundled dev keeps every output of the session in
/// memory (server/bundledDev.ts `storeOutputFiles` never clears
/// `memoryFiles`, since an open page may still import any of them). So the
/// dev server keeps the bundles behind the current one, and a chunk no
/// longer in the current manifest is looked up in them, newest first: the
/// chunks are content-hashed and immutable, so an old name served from an
/// old generation is exactly what the document expects. The entry is the
/// exception: it has a fixed name, so only the current bundle's is ever
/// served.
///
/// How far back: a page is behind by as many bundles as were made since it
/// loaded, which under a held reload (the editor's flush, minutes) or a hot
/// session (the pages never reload; every pause in typing rebundles) is not
/// a small number, so the window is not a count of generations but a budget
/// on disk, [`RETAINED_CHUNK_BYTES`] of chunks beyond the current bundle's
/// (shared chunks keep their names and count once), with
/// [`RETAINED_GENERATIONS`] as a cap. The retained chunks are the files in
/// the build dir's `client-chunks/`, which every bundle writes and nothing
/// clears (see `write_outputs`); a bundle's chunk files are removed when it
/// falls out of the window ([`ServedBundles::install`]), if no served bundle
/// still names them. The bundle store's blobs are not relied on: it keeps a
/// fixed window of generations of its own and sweeps the rest.
pub(crate) struct ServedBundles {
    current: Arc<PinnedBundle>,
    /// The bundles before `current`, newest first, within the window.
    previous: VecDeque<Arc<PinnedBundle>>,
    /// The build dir's `client-chunks/`, where every served bundle's chunk
    /// files are: a retained bundle's chunks are read from here.
    chunks_dir: PathBuf,
}

/// How much of the retained bundles' chunks stay on disk beyond the current
/// bundle: enough for the rebundles of a long editing session, where each one
/// renames the entry and the few chunks the edit touched; a generation costs
/// about what changed.
pub(crate) const RETAINED_CHUNK_BYTES: u64 = 256 * 1024 * 1024;

/// The most bundles kept behind the current one, whatever their size.
pub(crate) const RETAINED_GENERATIONS: usize = 64;

/// A chunk looked up across the served bundles.
pub(crate) struct ServedChunk {
    pub(crate) chunk: PinnedChunk,
    /// Whether the current bundle holds it, or an earlier one.
    pub(crate) current: bool,
    /// Whether it is the current bundle's entry, the one chunk with a fixed
    /// name (served without immutable caching).
    pub(crate) entry: bool,
}

impl ServedBundles {
    /// `chunks_dir` is the build dir's `client-chunks/`, holding `current`'s
    /// files (see [`keep_chunk_files`]) and those of every bundle installed
    /// after it.
    pub(crate) fn new(current: PinnedBundle, chunks_dir: PathBuf) -> Self {
        Self {
            current: Arc::new(current),
            previous: VecDeque::new(),
            chunks_dir,
        }
    }

    /// Serve `next`; the bundle it replaces joins the retained ones, unless
    /// it had no chunks (a failed boot's placeholder) or is the same bundle
    /// (a rebundle that changed nothing renames nothing). The oldest
    /// retained bundles past the window are forgotten; returned are the
    /// names of their chunks that no served bundle has any more, for the
    /// caller to remove from the chunks dir (off this lock).
    pub(crate) fn install(&mut self, next: PinnedBundle) -> Vec<String> {
        if *self.current != next && !self.current.is_empty() {
            let replaced = std::mem::replace(&mut self.current, Arc::new(next));
            self.previous.push_front(replaced);
        } else {
            self.current = Arc::new(next);
        }
        let keep = self.within_window();
        let forgotten: Vec<Arc<PinnedBundle>> = self.previous.drain(keep..).collect();
        let mut gone: Vec<String> = forgotten
            .iter()
            .flat_map(|bundle| bundle.chunks().map(|(name, _)| name))
            .filter(|name| self.chunk(name).is_none())
            .map(str::to_string)
            .collect();
        gone.sort_unstable();
        gone.dedup();
        gone
    }

    /// How many of the retained bundles, newest first, fit the window: the
    /// chunks a bundle adds beyond the ones already counted (the current
    /// bundle's, then each newer bundle's) are what it costs on disk. The
    /// newest retained bundle always fits: the pages open before the last
    /// rebundle are the common case.
    fn within_window(&self) -> usize {
        let mut seen: HashSet<&str> = self.current.chunks().map(|(name, _)| name).collect();
        let mut bytes = 0u64;
        let mut keep = 0;
        for bundle in &self.previous {
            if keep >= RETAINED_GENERATIONS {
                break;
            }
            let added: u64 = bundle
                .chunks()
                .filter(|(name, _)| seen.insert(name))
                .map(|(_, chunk)| chunk.size)
                .sum();
            bytes += added;
            if bytes > RETAINED_CHUNK_BYTES && keep > 0 {
                break;
            }
            keep += 1;
        }
        keep
    }

    /// The chunk `name` serves: the current bundle's, else the newest retained
    /// bundle's chunk of that name, read from the chunks dir. A retained
    /// bundle's entry is never served, whatever name asks for it.
    pub(crate) fn chunk(&self, name: &str) -> Option<ServedChunk> {
        if let Some(chunk) = self.current.chunk(name) {
            return Some(ServedChunk {
                chunk: chunk.clone(),
                current: true,
                entry: self.current.is_entry(name),
            });
        }
        self.previous
            .iter()
            .filter(|bundle| !bundle.is_entry(name))
            .find_map(|bundle| bundle.chunk(name))
            .map(|chunk| ServedChunk {
                chunk: PinnedChunk {
                    path: self.chunks_dir.join(name),
                    size: chunk.size,
                    hash: chunk.hash.clone(),
                },
                current: false,
                entry: false,
            })
    }

    /// The bundle every new document loads.
    pub(crate) fn current(&self) -> Arc<PinnedBundle> {
        Arc::clone(&self.current)
    }
}

/// Makes `chunks_dir` hold exactly `bundle`'s chunk files, at the boot of
/// the dev server: a bundle restored from the store has its chunks in the
/// store's blobs alone, so each is linked (or copied) in under its name, and
/// whatever an earlier run of the server left there goes, since no page of
/// this run holds it (a page left open across the restart reloads when it
/// trips over a missing chunk, see `chunk_gone`).
pub(crate) fn keep_chunk_files(bundle: &PinnedBundle, chunks_dir: &Path) {
    let named: HashSet<&str> = bundle.chunks().map(|(name, _)| name).collect();
    remove_files_except(chunks_dir, chunks_dir, &named);
    for (name, chunk) in bundle.chunks() {
        let dest = chunks_dir.join(name);
        if dest.is_file() {
            continue;
        }
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::hard_link(&chunk.path, &dest).is_err() {
            let _ = std::fs::copy(&chunk.path, &dest);
        }
    }
}

fn remove_files_except(root: &Path, dir: &Path, named: &HashSet<&str>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            remove_files_except(root, &path, named);
            // Left empty, or already so.
            let _ = std::fs::remove_dir(&path);
        } else if path
            .strip_prefix(root)
            .ok()
            .and_then(|rel| rel.to_str())
            .is_none_or(|rel| !named.contains(rel))
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Removes the chunk files [`ServedBundles::install`] said no served bundle
/// names any more.
pub(crate) fn remove_chunk_files(chunks_dir: &Path, names: &[String]) {
    for name in names {
        let _ = std::fs::remove_file(chunks_dir.join(name));
    }
}

/// The frame that tells the open pages a chunk one of them asked for is gone
/// from the dev server: its name fell out of the bundles still served, or
/// its file was swept. The page that asked reloads (its entry is too old to
/// run on); the others ignore it. Vite's dev server answers a request for a
/// pre-bundled chunk it no longer has the same way, the page reloaded by the
/// `full-reload` it sent when the chunks changed (optimizer.ts `fullReload`,
/// transform.ts "Outdated Optimize Dep"); here a page can be behind without
/// having been told to reload (the editor's gate holds it), so the server
/// says so when the page trips over it. Not on the page's own promise
/// rejection: a router catches a failed lazy import, and nothing would hear.
pub(crate) fn chunk_gone_frame(name: &str) -> String {
    serde_json::json!({ "type": "oj:chunk-gone", "name": name }).to_string()
}

/// Below this a gzip frame costs more than it saves.
const MIN_GZIP_BYTES: usize = 1024;

/// Compressed chunk bodies by content hash, pruned to the live bundle at each
/// rebundle swap ([`GzipCache::retain`]).
#[derive(Default)]
pub(crate) struct GzipCache {
    by_hash: Mutex<GzipBodies>,
}

/// One body per hash, compressed once even under concurrent first requests.
type GzipBodies = HashMap<String, Arc<tokio::sync::OnceCell<Bytes>>>;

impl GzipCache {
    /// The gzip body for `hash`, compressing `plain` off the async workers on
    /// first use. Concurrent first requests share one compression.
    async fn get_or_compress(&self, hash: &str, plain: Bytes) -> Bytes {
        let cell = self.lock().entry(hash.to_string()).or_default().clone();
        cell.get_or_init(|| compress(plain)).await.clone()
    }

    /// Drop bodies whose hash is no longer `live`. Called at the rebundle
    /// swap; a request still serving the old bundle can re-insert a dead hash
    /// after it, which the next swap removes.
    pub(crate) fn retain(&self, live: impl Fn(&str) -> bool) {
        self.lock().retain(|h, _| live(h));
    }

    /// Keep the bodies of `bundle`'s chunks only. The current bundle's: a
    /// retained bundle's chunk is asked for once in a while, by a page left
    /// behind, and is compressed again then, so the cache holds one bundle's
    /// bodies and not eight's. The hashes are gathered into a set first: one
    /// pass over the bundle and one over the cache, where asking the bundle
    /// about each cached hash is a scan of every chunk per body.
    pub(crate) fn retain_hashes_of(&self, bundle: &PinnedBundle) {
        let live: std::collections::HashSet<&str> = bundle.hashes().collect();
        self.retain(|h| live.contains(h));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, GzipBodies> {
        self.by_hash.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A multi-MB bundle takes tens of milliseconds at the default level.
async fn compress(plain: Bytes) -> Bytes {
    tokio::task::spawn_blocking(move || gzip(&plain))
        .await
        .expect("gzip task must not panic")
}

fn gzip(plain: &[u8]) -> Bytes {
    let mut enc = flate2::write::GzEncoder::new(
        Vec::with_capacity(plain.len() / 4),
        flate2::Compression::default(),
    );
    enc.write_all(plain).expect("writing to a Vec");
    Bytes::from(enc.finish().expect("writing to a Vec"))
}

/// Whether `Accept-Encoding` accepts gzip: an explicit `gzip` entry decides,
/// else a `*` wildcard; `q=0` refuses either (the parameter name is
/// case-insensitive, RFC 9110 5.6.6).
fn accepts_gzip(req: &HeaderMap) -> bool {
    let Some(v) = req
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let mut wildcard = false;
    for entry in v.split(',') {
        let mut parts = entry.trim().split(';');
        let name = parts.next().unwrap_or("").trim();
        let refused = parts.any(|p| {
            let mut kv = p.splitn(2, '=');
            kv.next()
                .is_some_and(|k| k.trim().eq_ignore_ascii_case("q"))
                && kv.next().and_then(|q| q.trim().parse::<f32>().ok()) == Some(0.0)
        });
        if name.eq_ignore_ascii_case("gzip") {
            return !refused;
        }
        if name == "*" {
            wildcard = !refused;
        }
    }
    wildcard
}

/// Compressible text types.
fn compressible(mime: &str) -> bool {
    mime.starts_with("text/")
        || mime.contains("javascript")
        || mime.contains("json")
        || mime.contains("svg")
}

/// One chunk of the client bundle. `immutable`: its name carries its content
/// hash (every chunk but the entry, named by rolldown's `[name]-[hash]`).
pub(crate) struct Chunk<'a> {
    pub mime: &'static str,
    pub hash: &'a str,
    pub immutable: bool,
}

/// The response for a chunk whose content hash is known. `body` is only
/// called when the bytes are needed (a 304 never reads the file). Without a
/// `store` a gzip body is compressed per request and not kept.
pub(crate) async fn respond<F, Fut>(
    req: &HeaderMap,
    chunk: Chunk<'_>,
    store: Option<&GzipCache>,
    body: F,
) -> Response
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Bytes, Response>>,
{
    let etag = weak_etag(chunk.hash);
    let cache_control = if chunk.immutable {
        "max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(chunk.mime));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    if let Ok(v) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, v);
    }
    if not_modified(req, &etag) {
        headers.remove(header::CONTENT_TYPE);
        return (StatusCode::NOT_MODIFIED, headers).into_response();
    }
    let plain = match body().await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    if plain.len() >= MIN_GZIP_BYTES && compressible(chunk.mime) && accepts_gzip(req) {
        let gz = match store {
            Some(cache) => cache.get_or_compress(chunk.hash, plain).await,
            None => compress(plain).await,
        };
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        return (headers, gz).into_response();
    }
    (headers, plain).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(k.clone(), HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn body() -> Bytes {
        Bytes::from("console.log('chunk');".repeat(200))
    }

    fn chunk(immutable: bool) -> Chunk<'static> {
        Chunk {
            mime: "text/javascript",
            hash: "abc123",
            immutable,
        }
    }

    async fn serve(req: &HeaderMap, immutable: bool, cache: &GzipCache) -> Response {
        respond(req, chunk(immutable), Some(cache), || async { Ok(body()) }).await
    }

    #[tokio::test]
    async fn a_matching_etag_is_a_304_without_reading_the_body() {
        let cache = GzipCache::default();
        let r = req(&[(header::IF_NONE_MATCH, "W/\"abc123\"")]);
        let resp = respond(&r, chunk(false), Some(&cache), || async {
            panic!("a 304 must not read the chunk")
        })
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(resp.headers()[header::ETAG], "W/\"abc123\"");
    }

    #[tokio::test]
    async fn the_entry_revalidates_and_hashed_chunks_are_immutable() {
        let cache = GzipCache::default();
        let entry = serve(&HeaderMap::new(), false, &cache).await;
        assert_eq!(entry.headers()[header::CACHE_CONTROL], "no-cache");
        let hashed = serve(&HeaderMap::new(), true, &cache).await;
        assert_eq!(
            hashed.headers()[header::CACHE_CONTROL],
            "max-age=31536000, immutable"
        );
    }

    #[tokio::test]
    async fn gzip_only_when_accepted_and_round_trips() {
        let cache = GzipCache::default();
        let plain = serve(&HeaderMap::new(), false, &cache).await;
        assert!(plain.headers().get(header::CONTENT_ENCODING).is_none());
        let refused = serve(
            &req(&[(header::ACCEPT_ENCODING, "gzip;q=0, br")]),
            false,
            &cache,
        )
        .await;
        assert!(refused.headers().get(header::CONTENT_ENCODING).is_none());

        let gz = serve(
            &req(&[(header::ACCEPT_ENCODING, "gzip, deflate, br")]),
            false,
            &cache,
        )
        .await;
        assert_eq!(gz.headers()[header::CONTENT_ENCODING], "gzip");
        let bytes = axum::body::to_bytes(gz.into_body(), usize::MAX)
            .await
            .unwrap();
        let mut out = String::new();
        std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&bytes[..]), &mut out)
            .unwrap();
        assert_eq!(out, "console.log('chunk');".repeat(200));
    }

    #[test]
    fn accept_encoding_wildcard_and_q_casing() {
        let ae = |v: &str| req(&[(header::ACCEPT_ENCODING, v)]);
        assert!(accepts_gzip(&ae("*")));
        assert!(accepts_gzip(&ae("br, *;q=0.5")));
        assert!(!accepts_gzip(&ae("*;q=0")));
        assert!(!accepts_gzip(&ae("gzip;Q=0, *")));
        assert!(accepts_gzip(&ae("GZIP;q=0.5")));
        assert!(!accepts_gzip(&ae("identity")));
    }

    #[tokio::test]
    async fn without_a_store_gzip_is_served_but_not_kept() {
        let r = req(&[(header::ACCEPT_ENCODING, "gzip")]);
        let resp = respond(&r, chunk(false), None, || async { Ok(body()) }).await;
        assert_eq!(resp.headers()[header::CONTENT_ENCODING], "gzip");
    }

    #[tokio::test]
    async fn retain_keeps_only_live_hashes() {
        let cache = GzipCache::default();
        cache.get_or_compress("old", body()).await;
        cache.get_or_compress("new", body()).await;
        cache.retain(|h| h == "new");
        let map = cache.lock();
        assert!(map.contains_key("new") && !map.contains_key("old"));
    }

    // A rebundle prunes the gzip bodies to the current bundle's chunks: a
    // retained bundle's are dropped and compressed again when asked for.
    #[tokio::test]
    async fn a_rebundle_keeps_the_current_bundles_gzip_bodies_only() {
        let cache = GzipCache::default();
        let mut served = ServedBundles::new(generation(1), no_dir());
        for hash in [
            "hash-route-1.js",
            "hash-entry-1",
            "hash-route-2.js",
            "hash-entry-2",
        ] {
            cache.get_or_compress(hash, body()).await;
        }
        served.install(generation(2));
        cache.retain_hashes_of(&served.current());
        let map = cache.lock();
        let mut kept: Vec<&str> = map.keys().map(String::as_str).collect();
        kept.sort_unstable();
        assert_eq!(kept, ["hash-entry-2", "hash-route-2.js"]);
        // The retained chunk is still served, without a cached body.
        assert!(served.chunk("route-1.js").is_some_and(|c| !c.current));
    }

    // Pruning a large cache against a large bundle keeps exactly the
    // bundle's bodies. (It is one pass over the bundle and one over the
    // cache, where asking the bundle about each cached hash was the chunk
    // count squared; no clock is asserted here, a loaded machine has none
    // to give.)
    #[test]
    fn pruning_a_large_gzip_cache_keeps_the_bundles_bodies() {
        let chunks = 2_000usize;
        let bundle = PinnedBundle::new(
            "client-entry.js".into(),
            (0..chunks).map(|i| {
                (
                    format!("c{i}.js"),
                    PinnedChunk {
                        path: "/blobs/x".into(),
                        size: 1,
                        hash: Some(format!("current-{i:08}")),
                    },
                )
            }),
        );
        let cache = GzipCache::default();
        {
            let mut map = cache.lock();
            for i in 0..chunks {
                map.insert(format!("current-{i:08}"), Default::default());
                map.insert(format!("retired-{i:08}"), Default::default());
            }
        }
        cache.retain_hashes_of(&bundle);
        let map = cache.lock();
        assert_eq!(map.len(), chunks);
        assert!(map.keys().all(|hash| hash.starts_with("current-")));
    }

    #[test]
    fn the_chunk_gone_frame_names_the_chunk() {
        let frame: serde_json::Value =
            serde_json::from_str(&chunk_gone_frame("about-\"x\".js")).unwrap();
        assert_eq!(frame["type"], "oj:chunk-gone");
        assert_eq!(frame["name"], "about-\"x\".js");
    }

    /// A bundle of generation `n`: an entry and a route chunk named by `n`,
    /// each `size` bytes.
    fn generation_of(n: usize, size: u64) -> PinnedBundle {
        let chunk = |name: &str| PinnedChunk {
            path: std::path::PathBuf::from(format!("/blobs/{name}")),
            size,
            hash: Some(format!("hash-{name}")),
        };
        let route = format!("route-{n}.js");
        PinnedBundle::new(
            "client-entry.js".into(),
            [
                ("client-entry.js".to_string(), chunk(&format!("entry-{n}"))),
                (route.clone(), chunk(&route)),
                // A chunk every generation shares, under one name.
                ("shared.js".to_string(), chunk("shared")),
            ],
        )
    }

    fn generation(n: usize) -> PinnedBundle {
        generation_of(n, 1)
    }

    /// A chunks dir no test reads.
    fn no_dir() -> PathBuf {
        PathBuf::from("/nonexistent/client-chunks")
    }

    #[test]
    fn a_chunk_renamed_by_a_rebundle_is_still_served_from_the_bundle_before() {
        let mut served = ServedBundles::new(generation(1), no_dir());
        assert!(
            served.install(generation(2)).is_empty(),
            "nothing is forgotten"
        );
        let old = served.chunk("route-1.js").expect("the old route chunk");
        assert!(!old.current && !old.entry);
        assert_eq!(old.chunk.hash.as_deref(), Some("hash-route-1.js"));
        // Read from the chunks dir, which the dev server keeps, not from the
        // bundle store's blob, which it may sweep.
        assert_eq!(old.chunk.path, no_dir().join("route-1.js"));
        let shared = served.chunk("shared.js").expect("the shared chunk");
        assert!(shared.current, "a name the current bundle has is its own");
        let new = served.chunk("route-2.js").expect("the new route chunk");
        assert!(new.current && !new.entry);
        assert!(served.chunk("route-3.js").is_none());
    }

    #[test]
    fn the_entry_is_only_ever_the_current_bundles() {
        let mut served = ServedBundles::new(generation(1), no_dir());
        served.install(generation(2));
        let entry = served.chunk("client-entry.js").expect("the entry");
        assert!(entry.current && entry.entry);
        assert_eq!(entry.chunk.hash.as_deref(), Some("hash-entry-2"));
        // A retained bundle whose entry has a hashed name of its own is not
        // served under that name either.
        let mut served = ServedBundles::new(
            PinnedBundle::new(
                "entry-DEADBEEF.js".into(),
                [(
                    "entry-DEADBEEF.js".to_string(),
                    PinnedChunk {
                        path: "/blobs/e".into(),
                        size: 1,
                        hash: Some("e".into()),
                    },
                )],
            ),
            no_dir(),
        );
        served.install(generation(2));
        assert!(served.chunk("entry-DEADBEEF.js").is_none());
    }

    /// A page is behind by as many bundles as were made since it loaded: a
    /// held reload or a hot session makes that many, so the window is not a
    /// handful of generations.
    #[test]
    fn a_chunk_of_the_bundle_many_rebundles_back_is_still_served() {
        let mut served = ServedBundles::new(generation(0), no_dir());
        for n in 1..=12 {
            assert!(served.install(generation(n)).is_empty());
        }
        assert_eq!(served.previous.len(), 12);
        for n in 0..12 {
            assert!(
                served.chunk(&format!("route-{n}.js")).is_some(),
                "route-{n}.js is served"
            );
        }
    }

    /// The window is a byte budget over the chunks the retained bundles add
    /// beyond the current one: a shared chunk counts once, the newest retained
    /// bundle always fits, and the forgotten bundles' own chunks are named
    /// for removal while the shared one is not.
    #[test]
    fn the_retained_window_is_a_byte_budget_and_forgets_the_oldest() {
        // Beyond the current bundle each generation adds its route alone (the
        // entry's name and the shared chunk's are the current bundle's too):
        // four generations fill the budget exactly.
        let size = RETAINED_CHUNK_BYTES / 4;
        let mut served = ServedBundles::new(generation_of(0, size), no_dir());
        for n in 1..=4 {
            assert!(
                served.install(generation_of(n, size)).is_empty(),
                "within the budget"
            );
        }
        assert_eq!(served.previous.len(), 4);
        let gone = served.install(generation_of(5, size));
        assert_eq!(
            served.previous.len(),
            4,
            "the oldest fell out of the budget"
        );
        assert_eq!(gone, vec!["route-0.js".to_string()]);
        assert!(served.chunk("route-0.js").is_none(), "the oldest is gone");
        assert!(
            served.chunk("route-1.js").is_some(),
            "the next oldest is kept"
        );
        assert!(served.chunk("shared.js").is_some_and(|c| c.current));
        assert!(served.chunk("client-entry.js").is_some_and(|c| c.current));

        // One bundle larger than the whole budget is still kept behind the
        // one that replaces it.
        let huge = RETAINED_CHUNK_BYTES * 2;
        let mut served = ServedBundles::new(generation_of(0, huge), no_dir());
        assert!(served.install(generation_of(1, huge)).is_empty());
        assert!(served.chunk("route-0.js").is_some());
        let gone = served.install(generation_of(2, huge));
        assert_eq!(gone, vec!["route-0.js".to_string()]);
        assert!(served.chunk("route-1.js").is_some());
    }

    #[test]
    fn the_retained_generations_are_capped() {
        let mut served = ServedBundles::new(generation(0), no_dir());
        for n in 1..=RETAINED_GENERATIONS {
            assert!(served.install(generation(n)).is_empty());
        }
        let gone = served.install(generation(RETAINED_GENERATIONS + 1));
        assert_eq!(served.previous.len(), RETAINED_GENERATIONS);
        assert_eq!(gone, vec!["route-0.js".to_string()]);
        assert!(
            served.chunk("route-1.js").is_some(),
            "the next oldest is kept"
        );
    }

    /// A forgotten bundle's chunk that a served bundle still has under the
    /// same name (a chunk a later rebundle brought back as it was) is not
    /// named for removal.
    #[test]
    fn a_forgotten_bundles_chunk_a_served_bundle_still_names_is_kept() {
        let size = RETAINED_CHUNK_BYTES / 4;
        let mut served = ServedBundles::new(generation_of(0, size), no_dir());
        for n in 1..=4 {
            let mut bundle = generation_of(n, size);
            if n == 3 {
                // Generation 3 has generation 0's route again.
                let route = bundle.chunk("route-3.js").unwrap().clone();
                let mut chunks: Vec<(String, PinnedChunk)> = bundle
                    .chunks()
                    .map(|(name, chunk)| (name.to_string(), chunk.clone()))
                    .collect();
                chunks.push(("route-0.js".to_string(), route));
                bundle = PinnedBundle::new(bundle.entry.clone(), chunks);
            }
            served.install(bundle);
        }
        // Generations 4, 3 (two routes) and 2 fill the budget; 1 and 0 go.
        let gone = served.install(generation_of(5, size));
        assert_eq!(served.previous.len(), 3);
        assert_eq!(
            gone,
            vec!["route-1.js".to_string()],
            "route-0.js is generation 3's too"
        );
        assert!(served.chunk("route-0.js").is_some());
        assert!(served.chunk("route-1.js").is_none());
    }

    #[test]
    fn the_chunk_files_follow_the_served_bundles() {
        let dir = std::env::temp_dir().join(format!("oj-served-chunks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let chunks = dir.join("client-chunks");
        let blobs = dir.join("blobs");
        std::fs::create_dir_all(chunks.join("assets")).unwrap();
        std::fs::create_dir_all(&blobs).unwrap();
        // An earlier run's leftovers, one in a subdirectory.
        std::fs::write(chunks.join("stale-1.js"), "stale").unwrap();
        std::fs::write(chunks.join("assets/stale-2.css"), "stale").unwrap();
        // The restored bundle's chunks, in the store's blobs alone.
        std::fs::write(blobs.join("aaaa"), "entry code").unwrap();
        std::fs::write(blobs.join("bbbb"), "route code").unwrap();
        let chunk = |hash: &str, size: u64| PinnedChunk {
            path: blobs.join(hash),
            size,
            hash: Some(hash.into()),
        };
        let restored = PinnedBundle::new(
            "client-entry.js".into(),
            [
                ("client-entry.js".to_string(), chunk("aaaa", 10)),
                ("assets/route-1.js".to_string(), chunk("bbbb", 10)),
            ],
        );
        keep_chunk_files(&restored, &chunks);
        assert!(
            !chunks.join("stale-1.js").exists(),
            "the leftover is removed"
        );
        assert!(!chunks.join("assets/stale-2.css").exists());
        assert_eq!(
            std::fs::read_to_string(chunks.join("client-entry.js")).unwrap(),
            "entry code"
        );
        assert_eq!(
            std::fs::read_to_string(chunks.join("assets/route-1.js")).unwrap(),
            "route code"
        );

        // The forgotten names go; what is not named stays.
        std::fs::write(chunks.join("route-2.js"), "newer").unwrap();
        remove_chunk_files(
            &chunks,
            &["assets/route-1.js".to_string(), "missing.js".to_string()],
        );
        assert!(!chunks.join("assets/route-1.js").exists());
        assert!(chunks.join("route-2.js").exists());
        assert!(chunks.join("client-entry.js").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_placeholder_and_an_unchanged_bundle_take_no_slot() {
        let mut served = ServedBundles::new(PinnedBundle::empty(), no_dir());
        served.install(generation(1));
        assert!(
            served.previous.is_empty(),
            "a failed boot's placeholder is not a generation"
        );
        served.install(generation(1));
        assert!(
            served.previous.is_empty(),
            "the same bundle again renames nothing"
        );
        assert!(served.chunk("route-1.js").is_some_and(|c| c.current));
    }
}
