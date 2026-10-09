use super::*;

/// `GET /@oj/diagnostics`: the ring of recent diagnostic events (see
/// `oj_diag`) plus plugin-host health, for supervisors, agents and tests.
/// Always on, but refused for anything carrying an `Origin` header, like the
/// debug routes: the consumers are local tools (which send none), and the
/// event text names project file paths a hostile page must not read.
/// `?after=<epoch_ms>` keeps only newer events (a poller's cursor).
pub(crate) async fn serve_diagnostics(
    headers: HeaderMap,
    uri: Uri,
    State(state): State<Arc<ServerState>>,
) -> Response {
    if headers.contains_key(header::ORIGIN) {
        return (StatusCode::FORBIDDEN, "").into_response();
    }
    let after = uri
        .query()
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("after=")))
        .and_then(|v| v.parse::<u64>().ok());
    let mut body = oj_diag::global().snapshot(after);
    body["startedAt"] = state.started_at_ms.into();
    body["pluginHost"] = match &state.plugins {
        None => serde_json::json!({ "present": false }),
        Some(host) => serde_json::json!({
            "present": true,
            "initialized": host.is_initialized(),
            "gone": *host.host_gone_updates().borrow(),
            "canRevive": host.can_revive(),
        }),
    };
    (
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}
