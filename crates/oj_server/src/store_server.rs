// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The oj store's wire (pulp P3): a deliberately boring six-endpoint HTTP
//! substituter, shaped like Metro's store and Nix's substituters had a child.
//!
//! ```text
//! GET  /v1/ac/{ns}/{key}   action-cache entry (modules: the CachedModule JSON)
//! PUT  /v1/ac/{ns}/{key}   write-once: 201, or 409 if present
//! GET  /v1/cas/{blake3}    content-addressed blob
//! HEAD /v1/cas/{blake3}
//! PUT  /v1/cas/{blake3}    write-once; body must match its digest
//! POST /v1/cas/missing     {"digests":[..]} -> {"missing":[..]}
//! ```
//!
//! Write-once entries and split read/write tokens are the CREEP lesson
//! (CVE-2025-36852): in a shared first-write-wins cache with one credential,
//! anyone who can open a PR can poison what main will later hit. Here a PUT
//! can never replace, untrusted consumers hold read-only tokens, and a bad
//! entry costs one key.
//!
//! Auth: `OJ_STORE_READ_TOKEN` (unset = public reads) and
//! `OJ_STORE_WRITE_TOKEN` (unset = writes refused), both bearer tokens.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path as AxPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;

const MAX_BLOB_BYTES: usize = 64 * 1024 * 1024;
const MAX_AC_BYTES: usize = 16 * 1024 * 1024;

pub struct StoreServer {
    dir: PathBuf,
    read_token: Option<String>,
    write_token: Option<String>,
}

pub async fn serve(dir: PathBuf, host: &str, port: u16) -> Result<(), String> {
    let state = Arc::new(StoreServer {
        dir,
        read_token: oj_env::get().knobs.store_read_token.clone(),
        write_token: oj_env::get().knobs.store_write_token.clone(),
    });
    std::fs::create_dir_all(&state.dir).map_err(|e| e.to_string())?;
    let app = Router::new()
        .route("/v1/ac/{ns}/{key}", get(ac_get).put(ac_put))
        .route("/v1/cas/{hash}", get(cas_get).head(cas_head).put(cas_put))
        .route("/v1/cas/missing", post(cas_missing))
        .with_state(Arc::clone(&state));
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;
    eprintln!(
        "oj store: serving {} on http://{addr} (reads {}, writes {})",
        state.dir.display(),
        if state.read_token.is_some() {
            "token"
        } else {
            "public"
        },
        if state.write_token.is_some() {
            "token"
        } else {
            "DISABLED"
        },
    );
    axum::serve(listener, app).await.map_err(|e| e.to_string())
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

fn can_read(s: &StoreServer, headers: &HeaderMap) -> bool {
    match &s.read_token {
        None => true,
        // The write token also reads: a pusher must be able to probe.
        Some(t) => {
            let b = bearer(headers);
            b == Some(t.as_str())
                || (s.write_token.as_deref().is_some() && b == s.write_token.as_deref())
        }
    }
}

fn can_write(s: &StoreServer, headers: &HeaderMap) -> bool {
    matches!((&s.write_token, bearer(headers)), (Some(t), Some(b)) if b == t)
}

fn valid_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_ns(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn ac_path(s: &StoreServer, ns: &str, key: &str) -> Option<PathBuf> {
    (valid_ns(ns) && valid_hex(key, 64)).then(|| {
        s.dir
            .join("ac")
            .join(ns)
            .join(&key[..2])
            .join(format!("{key}.json"))
    })
}

fn cas_path(s: &StoreServer, hash: &str) -> Option<PathBuf> {
    valid_hex(hash, 64).then(|| s.dir.join("cas").join(&hash[..2]).join(hash))
}

/// Write-once atomic publish: tmp + rename, 409 when the destination exists
/// (checked again after the rename barrier is irrelevant here: rename over an
/// existing path would replace it, so existence is checked first and the race
/// loser merely re-publishes identical content under a content-derived name).
fn write_once(path: &Path, bytes: &[u8]) -> Result<(), StatusCode> {
    if path.exists() {
        return Err(StatusCode::CONFLICT);
    }
    let parent = path.parent().ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    std::fs::create_dir_all(parent).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if path.exists() {
        let _ = std::fs::remove_file(&tmp);
        return Err(StatusCode::CONFLICT);
    }
    std::fs::rename(&tmp, path).map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

async fn ac_get(
    State(s): State<Arc<StoreServer>>,
    AxPath((ns, key)): AxPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if !can_read(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(path) = ac_path(&s, &ns, &key) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match std::fs::read(&path) {
        Ok(bytes) => ([(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn ac_put(
    State(s): State<Arc<StoreServer>>,
    AxPath((ns, key)): AxPath<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !can_write(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(path) = ac_path(&s, &ns, &key) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if body.len() > MAX_AC_BYTES || serde_json::from_slice::<serde_json::Value>(&body).is_err() {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    match write_once(&path, &body) {
        Ok(()) => StatusCode::CREATED.into_response(),
        Err(code) => code.into_response(),
    }
}

async fn cas_get(
    State(s): State<Arc<StoreServer>>,
    AxPath(hash): AxPath<String>,
    headers: HeaderMap,
) -> Response {
    if !can_read(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(path) = cas_path(&s, &hash) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match std::fs::read(&path) {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream"),
                (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn cas_head(
    State(s): State<Arc<StoreServer>>,
    AxPath(hash): AxPath<String>,
    headers: HeaderMap,
) -> Response {
    if !can_read(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match cas_path(&s, &hash) {
        Some(path) if path.is_file() => StatusCode::OK.into_response(),
        Some(_) => StatusCode::NOT_FOUND.into_response(),
        None => StatusCode::BAD_REQUEST.into_response(),
    }
}

async fn cas_put(
    State(s): State<Arc<StoreServer>>,
    AxPath(hash): AxPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !can_write(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(path) = cas_path(&s, &hash) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if body.len() > MAX_BLOB_BYTES {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    // CAS self-verification: the name IS the digest, so a tampered or
    // corrupted upload cannot land under an honest name.
    if blake3::hash(&body).to_hex().to_string() != hash {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    match write_once(&path, &body) {
        Ok(()) => StatusCode::CREATED.into_response(),
        // A CAS conflict is success by definition: identical content.
        Err(StatusCode::CONFLICT) => StatusCode::OK.into_response(),
        Err(code) => code.into_response(),
    }
}

#[derive(serde::Deserialize)]
struct MissingReq {
    digests: Vec<String>,
}

async fn cas_missing(
    State(s): State<Arc<StoreServer>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !can_read(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(req) = serde_json::from_slice::<MissingReq>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let missing: Vec<&String> = req
        .digests
        .iter()
        .filter(|h| !matches!(cas_path(&s, h), Some(p) if p.is_file()))
        .collect();
    (
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::json!({ "missing": missing }).to_string(),
    )
        .into_response()
}
