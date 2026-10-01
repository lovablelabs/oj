use super::*;

pub(crate) fn debug_mem() -> bool {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        std::env::var("OJ_DEBUG_MEM").is_ok_and(|v| !v.is_empty() && v != "0")
    });
    *ON
}

/// 404 unless OJ_DEBUG_MEM is set; 403 for anything carrying an `Origin`.
/// Probes send no Origin header; a hostile page's cross-origin fetch always
/// does, and GC-hammering from a tab is the only remote vector here.
fn debug_gate(headers: &axum::http::HeaderMap) -> Option<Response> {
    if !debug_mem() {
        return Some((axum::http::StatusCode::NOT_FOUND, "").into_response());
    }
    if headers.contains_key(axum::http::header::ORIGIN) {
        return Some((axum::http::StatusCode::FORBIDDEN, "").into_response());
    }
    None
}

/// Deterministic resident-byte split (OJ_DEBUG_MEM=1): budgeted module cache
/// plus served-dep store, so soaks measure retention without RSS noise.
pub(crate) async fn debug_mem_stats(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<ServerState>>,
) -> Response {
    if let Some(resp) = debug_gate(&headers) {
        return resp;
    }
    let (entries, total, code, map) = state.memory.lock().unwrap().stats();
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::json!({
            "module_cache": { "entries": entries, "bytes": total, "code_bytes": code, "map_bytes": map },
        })
        .to_string(),
    )
        .into_response()
}

pub(crate) async fn debug_gc(
    State(state): State<Arc<ServerState>>,
    headers: axum::http::HeaderMap,
) -> Response {
    if let Some(resp) = debug_gate(&headers) {
        return resp;
    }
    // Barrier across every registered engine: returns only after each one
    // acknowledged its collection, so a probe reading RSS next sees post-GC numbers.
    let registry = state.engine_registry.clone();
    let collected = tokio::task::spawn_blocking(move || {
        registry.collect_garbage(std::time::Duration::from_secs(10))
    })
    .await
    .unwrap_or(0);
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        format!("{{\"collected\":{collected}}}"),
    )
        .into_response()
}
