use super::*;

pub(crate) fn js(body: impl IntoResponse) -> Response {
    ([(header::CONTENT_TYPE, "text/javascript")], body).into_response()
}

pub(crate) async fn serve_client_js(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
) -> Response {
    // Browsers refetch /@oj/client.js on every reload; the body is fixed for
    // the server's lifetime, so a matching validator saves the transfer.
    cached_js_response(
        &headers,
        state.client_js_etag.clone(),
        state.client_js.clone(),
    )
}

/// A cached, immutable-for-this-process JS body: 304 on a matching validator, else the
/// bytes verbatim (the header tuple overwrites the octet-stream type `Bytes` would set).
pub(crate) fn cached_js_response(headers: &HeaderMap, etag: String, body: Bytes) -> Response {
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        == Some(etag.as_str())
    {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag),
                (header::CACHE_CONTROL, "no-cache".to_string()),
            ],
        )
            .into_response();
    }
    (
        [
            (header::CONTENT_TYPE, "text/javascript".to_string()),
            (header::CACHE_CONTROL, "no-cache".to_string()),
            (header::ETAG, etag),
        ],
        body,
    )
        .into_response()
}

/// The path the HMR socket is served at: `server.hmr.path` (made absolute) or
/// oj's `/__ws`.
pub(crate) fn hmr_socket_path(hmr: Option<&oj_config::HmrOptions>) -> String {
    match hmr
        .and_then(|h| h.path.as_deref())
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        Some(p) if p.starts_with('/') => p.to_string(),
        Some(p) => format!("/{p}"),
        None => "/__ws".to_string(),
    }
}

/// Fill the client's `__HMR_*__` / `__WS_TOKEN__` placeholders (Vite's clientInjections):
/// JSON literals, `null` where the config is silent so the client uses the page's location.
pub(crate) fn render_client_js(
    template: &str,
    hmr: Option<&oj_config::HmrOptions>,
    ws_path: &str,
    token: &str,
) -> String {
    let lit = |v: serde_json::Value| v.to_string();
    let protocol = hmr.and_then(|h| h.protocol.clone());
    let hostname = hmr.and_then(|h| h.host.clone());
    // Vite: `ws.clientPort -> ws.port -> the page's port`; oj's socket shares the
    // dev server port, so only clientPort (browser-facing, behind a proxy) moves the dial.
    let port = hmr.and_then(|h| h.client_port);
    let overlay = hmr.and_then(|h| h.overlay).unwrap_or(true);
    template
        .replace("__HMR_PROTOCOL__", &lit(protocol.into()))
        .replace("__HMR_HOSTNAME__", &lit(hostname.into()))
        .replace("__HMR_PORT__", &lit(port.into()))
        .replace("__HMR_PATH__", &lit(ws_path.into()))
        .replace("__HMR_ENABLE_OVERLAY__", &lit(overlay.into()))
        .replace("__WS_TOKEN__", &lit(token.into()))
}

/// A fresh random token for this process (Vite: `crypto.randomBytes(9)` base64url).
/// rustls' provider RNG, with a hash of process-unique state as the fallback.
pub(crate) fn new_ws_token() -> String {
    let mut bytes = [0u8; 16];
    let filled = rustls::crypto::aws_lc_rs::default_provider()
        .secure_random
        .fill(&mut bytes)
        .is_ok();
    if !filled {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seed = format!("{}:{nanos}:{:p}", std::process::id(), &bytes);
        bytes.copy_from_slice(&blake3::hash(seed.as_bytes()).as_bytes()[..16]);
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Vite's ws `shouldHandle`: a request carrying `Origin` (a browser) must present the
/// token; Origin-less clients could already make plain HTTP requests, so they pass.
pub(crate) fn ws_token_rejected(
    check: bool,
    token: &str,
    headers: &HeaderMap,
    query: Option<&str>,
) -> bool {
    if !check || !headers.contains_key(header::ORIGIN) {
        return false;
    }
    !query.is_some_and(|q| {
        q.split('&')
            .any(|kv| kv.strip_prefix("token=") == Some(token))
    })
}

// An SSR module carries its source map inline: the runner maps stack frames
// through it back to the original file (Vite's ssrFixStacktrace).
pub(crate) fn with_inline_map(code: String, map_json: Option<String>) -> String {
    match map_json {
        Some(map) => format!(
            "{code}\n//# sourceMappingURL={}\n",
            oj_compiler::map_json_to_data_url(&map)
        ),
        None => code,
    }
}
