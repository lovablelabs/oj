// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! How the Start client bundle goes over the wire: an ETag from the chunk's
//! content hash (a reload revalidates to a 304), immutable caching for the
//! bundle's hashed chunks, and gzip for clients that accept it.

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use oj_server::etag::{not_modified, weak_etag};

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
}
