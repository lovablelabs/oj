use super::*;

pub(crate) fn debug_mem() -> bool {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        std::env::var("OJ_DEBUG_MEM").is_ok_and(|v| !v.is_empty() && v != "0")
    });
    *ON
}

/// Deterministic resident-byte split (OJ_DEBUG_MEM=1): the budgeted module
/// cache and the served-dep store, so soaks measure retention without RSS
/// noise (the footprint-honest-reporting lever).
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
    // Probes (curl, the bench harness) send no Origin header; a hostile web
    // page's cross-origin fetch always does. GC-hammering from a browser tab
    // is the only remote vector this debug surface opens, so close it.
    if headers.contains_key(axum::http::header::ORIGIN) {
        return (axum::http::StatusCode::FORBIDDEN, "").into_response();
    }
    // Fans out through oj_js's engine registry (plugin hosts, SSR, Start,
    // CSS, addon-keeper alike) and returns only after each counted engine
    // ACKNOWLEDGED running its collection — a barrier, not a request, so a
    // probe reading RSS right after this response sees post-GC numbers.
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
