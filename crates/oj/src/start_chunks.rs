// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! How the Start client bundle goes over the wire: a strong ETag from the
//! chunk's content hash (a reload revalidates to a 304), immutable caching for
//! content-hashed chunk names, and gzip for clients that accept it.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Mutex;

use axum::body::Bytes;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

/// Below this a gzip frame costs more than it saves.
const MIN_GZIP_BYTES: usize = 1024;

/// Compressed chunk bodies by content hash, kept only for the live bundle.
#[derive(Default)]
pub(crate) struct GzipCache {
    by_hash: Mutex<HashMap<String, Bytes>>,
}

impl GzipCache {
    /// The gzip body for `hash`, compressing `plain` on first use. Entries for
    /// hashes outside `live` (an older bundle) are dropped on insert.
    fn get_or_compress(
        &self,
        hash: &str,
        plain: &[u8],
        live: &(dyn Fn(&str) -> bool + Sync),
    ) -> Bytes {
        if let Some(hit) = self.lock().get(hash) {
            return hit.clone();
        }
        let gz = gzip(plain);
        let mut map = self.lock();
        map.retain(|h, _| live(h));
        map.insert(hash.to_string(), gz.clone());
        gz
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Bytes>> {
        self.by_hash.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn gzip(plain: &[u8]) -> Bytes {
    let mut enc = flate2::write::GzEncoder::new(
        Vec::with_capacity(plain.len() / 4),
        flate2::Compression::fast(),
    );
    enc.write_all(plain).expect("writing to a Vec");
    Bytes::from(enc.finish().expect("writing to a Vec"))
}

pub(crate) fn etag_of(hash: &str) -> String {
    format!("\"{hash}\"")
}

/// Whether the request's `If-None-Match` names `etag`.
pub(crate) fn not_modified(req: &HeaderMap, etag: &str) -> bool {
    req.get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*"))
}

/// Rolldown's default chunk and asset names end in `-<hash>`; those never
/// change content, so the browser may keep them without revalidating. The
/// entry keeps its fixed name and is revalidated.
pub(crate) fn content_hashed(name: &str) -> bool {
    let file = name.rsplit('/').next().unwrap_or(name);
    let stem = file.split('.').next().unwrap_or(file);
    stem.rsplit_once('-').is_some_and(|(_, h)| {
        h.len() >= 8
            && h.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    })
}

fn accepts_gzip(req: &HeaderMap) -> bool {
    req.get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',').any(|e| {
                let mut parts = e.trim().split(';');
                let name = parts.next().unwrap_or("").trim();
                let q0 = parts.any(|p| matches!(p.trim(), "q=0" | "q=0.0" | "q=0.00" | "q=0.000"));
                name.eq_ignore_ascii_case("gzip") && !q0
            })
        })
}

/// Compressible text types.
fn compressible(mime: &str) -> bool {
    mime.starts_with("text/")
        || mime.contains("javascript")
        || mime.contains("json")
        || mime.contains("svg")
}

/// The response for a chunk whose content hash is known. `body` is only
/// called when the bytes are needed (a 304 never reads the file).
pub(crate) async fn respond<F, Fut>(
    req: &HeaderMap,
    name: &str,
    mime: &'static str,
    hash: &str,
    gzip_cache: &GzipCache,
    live: &(dyn Fn(&str) -> bool + Sync),
    body: F,
) -> Response
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Bytes, Response>>,
{
    let etag = etag_of(hash);
    let cache_control = if content_hashed(name) {
        "max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
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
    if plain.len() >= MIN_GZIP_BYTES && compressible(mime) && accepts_gzip(req) {
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        return (headers, gzip_cache.get_or_compress(hash, &plain, live)).into_response();
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

    async fn serve(req: &HeaderMap, name: &str, cache: &GzipCache) -> Response {
        let body = Bytes::from("console.log('chunk');".repeat(200));
        respond(
            req,
            name,
            "text/javascript",
            "abc123",
            cache,
            &|_| true,
            move || async move { Ok(body) },
        )
        .await
    }

    #[test]
    fn hashed_names_are_recognized() {
        assert!(content_hashed("index-B2E7EObn.js"));
        assert!(content_hashed("assets/logo-DQ2Nmene.svg"));
        assert!(!content_hashed("client-entry.js"));
        assert!(!content_hashed("vendor-x.js"));
    }

    #[tokio::test]
    async fn a_matching_etag_is_a_304_without_reading_the_body() {
        let cache = GzipCache::default();
        let r = req(&[(header::IF_NONE_MATCH, "\"abc123\"")]);
        let resp = respond(
            &r,
            "client-entry.js",
            "text/javascript",
            "abc123",
            &cache,
            &|_| true,
            || async { panic!("a 304 must not read the chunk") },
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(resp.headers()[header::ETAG], "\"abc123\"");
    }

    #[tokio::test]
    async fn the_entry_revalidates_and_hashed_chunks_are_immutable() {
        let cache = GzipCache::default();
        let entry = serve(&HeaderMap::new(), "client-entry.js", &cache).await;
        assert_eq!(entry.headers()[header::CACHE_CONTROL], "no-cache");
        let chunk = serve(&HeaderMap::new(), "dist-D79n85g1.js", &cache).await;
        assert_eq!(
            chunk.headers()[header::CACHE_CONTROL],
            "max-age=31536000, immutable"
        );
    }

    #[tokio::test]
    async fn gzip_only_when_accepted_and_round_trips() {
        let cache = GzipCache::default();
        let plain = serve(&HeaderMap::new(), "client-entry.js", &cache).await;
        assert!(plain.headers().get(header::CONTENT_ENCODING).is_none());
        let refused = serve(
            &req(&[(header::ACCEPT_ENCODING, "gzip;q=0, br")]),
            "client-entry.js",
            &cache,
        )
        .await;
        assert!(refused.headers().get(header::CONTENT_ENCODING).is_none());

        let gz = serve(
            &req(&[(header::ACCEPT_ENCODING, "gzip, deflate, br")]),
            "client-entry.js",
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
    fn the_cache_keeps_only_live_hashes() {
        let cache = GzipCache::default();
        cache.get_or_compress("old", b"a", &|_| true);
        cache.get_or_compress("new", b"b", &|h| h == "new");
        let map = cache.lock();
        assert!(map.contains_key("new") && !map.contains_key("old"));
    }
}
