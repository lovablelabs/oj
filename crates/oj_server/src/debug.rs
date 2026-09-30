use super::*;

pub(crate) fn debug_mem() -> bool {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        std::env::var("OJ_DEBUG_MEM").is_ok_and(|v| !v.is_empty() && v != "0")
    });
    *ON
}

/// Deterministic resident-byte split (OJ_DEBUG_MEM=1): budgeted module cache
/// plus served-dep store, so soaks measure retention without RSS noise.
pub(crate) async fn debug_mem_stats(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<ServerState>>,
) -> Response {
    if !debug_mem() {
        return (axum::http::StatusCode::NOT_FOUND, "").into_response();
    }
    if headers.contains_key(axum::http::header::ORIGIN) {
        return (axum::http::StatusCode::FORBIDDEN, "").into_response();
    }
    let (entries, total, code, map) = state.memory.lock().unwrap().stats();
    let (pb_entries, pb_bytes) = pkg_bundle::debug_stats();
    let (pr_entries, pr_bytes) = pkg_rolldown::debug_stats();
    let (pkg_entries, pkg_bytes) = (pb_entries + pr_entries, pb_bytes + pr_bytes);
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::json!({
            "module_cache": { "entries": entries, "bytes": total, "code_bytes": code, "map_bytes": map },
            "pkg_store_memory": { "entries": pkg_entries, "bytes": pkg_bytes },
        })
        .to_string(),
    )
        .into_response()
}

pub(crate) async fn debug_gc(headers: axum::http::HeaderMap) -> Response {
    if !debug_mem() {
        return (axum::http::StatusCode::NOT_FOUND, "").into_response();
    }
    // Probes send no Origin header; a hostile page's cross-origin fetch always
    // does, and GC-hammering from a tab is the only remote vector here.
    if headers.contains_key(axum::http::header::ORIGIN) {
        return (axum::http::StatusCode::FORBIDDEN, "").into_response();
    }
    // Barrier across every registered engine: returns only after each one
    // acknowledged its collection, so a probe reading RSS next sees post-GC numbers.
    let collected = tokio::task::spawn_blocking(|| {
        oj_js::collect_all_garbage(std::time::Duration::from_secs(10))
    })
    .await
    .unwrap_or(0);
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        format!("{{\"collected\":{collected}}}"),
    )
        .into_response()
}
