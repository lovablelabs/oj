use super::*;

pub(crate) async fn forward_to_plugin_middleware(
    state: &ServerState,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: Vec<u8>,
) -> Option<Response> {
    let port = state.plugin_serve.mw_port()?;
    let pq = uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or(uri.path());
    let target = format!("http://127.0.0.1:{port}{pq}");
    let rmethod = reqwest::Method::from_bytes(method.as_str().as_bytes()).ok()?;
    let mut out = state.http.request(rmethod, &target);
    for (name, value) in headers.iter() {
        if name == header::HOST {
            continue;
        }
        out = out.header(name, value);
    }
    if !body.is_empty() {
        out = out.body(body);
    }
    let resp = out.send().await.ok()?;
    if resp.headers().contains_key("x-oj-fallthrough") {
        return None;
    }
    let status = resp.status();
    let resp_headers = resp.headers().clone();
    // Stream the response through (Vite pipes it): TanStack Start streams
    // dehydrated data into the HTML; buffering breaks progressive hydration.
    let mut response = Response::new(Body::from_stream(resp.bytes_stream()));
    *response.status_mut() = status;
    for (name, value) in resp_headers.iter() {
        if name == header::TRANSFER_ENCODING || name == header::CONTENT_LENGTH {
            continue;
        }
        response.headers_mut().append(name, value.clone());
    }
    Some(response)
}

// Tell the plugin middleware server that files changed so it can invalidate
// module graphs and send HMR; type is "update" | "create" | "delete". Fire-and-forget.
pub async fn notify_plugin_mw_invalidate(port: u16, changes: &[(String, &'static str)]) {
    let client = plugin_mw_client();
    let changes: Vec<serde_json::Value> = changes
        .iter()
        .map(|(path, kind)| serde_json::json!({ "path": path, "type": kind }))
        .collect();
    let body = serde_json::json!({ "changes": changes }).to_string();
    let _ = client
        .post(format!("http://127.0.0.1:{port}/__oj_invalidate"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await;
}

pub(crate) fn plugin_mw_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    // Bounded: the settled-batch call blocks the watcher thread, and a hung
    // hotUpdate hook must not freeze rebuilds for the session.
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_default()
    })
}

/// Full resync: invalidate every runner-backed DevEnvironment's graph and
/// full-reload, bypassing per-change dedup; sent on late activation to cover
/// edits made while the middleware path was down. Returns whether the ENQUEUE
/// was acked (202 semantics, ack means "queued" never "ran"): the host pushes
/// `{ ojResyncDone }` when the resync EXECUTES, and only that completion
/// justifies a "resynced" claim ([`await_resync_completion`]).
pub async fn notify_plugin_mw_resync(port: u16) -> bool {
    match plugin_mw_client()
        .post(format!("http://127.0.0.1:{port}/__oj_invalidate"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"resync":true}"#)
        .send()
        .await
    {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

/// [`notify_plugin_mw_resync`] with backed-off retries: the host coalesces
/// duplicate resyncs, so retrying never stacks full-reloads; `false` after the
/// last attempt makes the caller warn instead of logging success.
pub async fn resync_plugin_mw_with_retry(port: u16) -> bool {
    for delay in [
        std::time::Duration::ZERO,
        std::time::Duration::from_millis(250),
        std::time::Duration::from_millis(750),
    ] {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if notify_plugin_mw_resync(port).await {
            return true;
        }
    }
    false
}

/// Wait, bounded, for `{ ojResyncDone }` to pass the pre-enqueue `baseline`;
/// a completion racing ahead is never missed, one push may answer coalesced enqueues.
pub async fn await_resync_completion(
    done: &mut tokio::sync::watch::Receiver<u64>,
    baseline: u64,
    bound: std::time::Duration,
) -> bool {
    tokio::time::timeout(bound, async {
        loop {
            if *done.borrow_and_update() > baseline {
                return true;
            }
            if done.changed().await.is_err() {
                return false;
            }
        }
    })
    .await
    .unwrap_or(false)
}

/// Proxy one request to a loopback service and stream the response back
/// (buffering breaks progressive hydration); Host travels as `x-oj-host`.
pub async fn proxy_to_loopback(
    port: u16,
    method: &str,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Option<Vec<u8>>,
) -> Result<Response, String> {
    proxy_to_loopback_streaming(port, method, path_and_query, headers, body.map(Body::from)).await
}

/// Headers forwarded to a loopback service: hyper writes the loopback `Host`
/// itself, so the browser's `Host` travels as `x-oj-host` (first value only,
/// Node semantics). A proxy's `x-forwarded-host` passes through untouched as
/// under Vite; duplicating it made Node join values into an unparseable URL.
/// An incoming `x-oj-host` is dropped so a client cannot spoof it.
pub fn loopback_request_headers(
    headers: &HeaderMap,
) -> Vec<(header::HeaderName, header::HeaderValue)> {
    let mut out = Vec::with_capacity(headers.len() + 1);
    if let Some(host) = headers.get(header::HOST) {
        out.push((header::HeaderName::from_static("x-oj-host"), host.clone()));
    }
    for (name, value) in headers.iter() {
        if name == header::HOST || name.as_str() == "x-oj-host" {
            continue;
        }
        out.push((name.clone(), value.clone()));
    }
    out
}

/// `proxy_to_loopback` with the request body streamed through as it arrives
/// (Vite pipes `req` into the app; an upload is never held whole in memory).
pub async fn proxy_to_loopback_streaming(
    port: u16,
    method: &str,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Option<Body>,
) -> Result<Response, String> {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let client = CLIENT.get_or_init(reqwest::Client::new);
    let target = format!("http://127.0.0.1:{port}{path_and_query}");
    let m = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| e.to_string())?;
    let mut out = client.request(m, &target);
    for (name, value) in loopback_request_headers(headers) {
        out = out.header(name, value);
    }
    if let Some(b) = body {
        out = out.body(reqwest::Body::wrap_stream(b.into_data_stream()));
    }
    let resp = out.send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let resp_headers = resp.headers().clone();
    let mut response = Response::new(Body::from_stream(resp.bytes_stream()));
    *response.status_mut() = status;
    for (name, value) in resp_headers.iter() {
        if name == header::TRANSFER_ENCODING || name == header::CONTENT_LENGTH {
            continue;
        }
        response.headers_mut().append(name, value.clone());
    }
    Ok(response)
}

pub async fn forward_to_plugin_mw(
    port: u16,
    method: &str,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Option<Vec<u8>>,
) -> Option<Response> {
    let response = proxy_to_loopback(port, method, path_and_query, headers, body)
        .await
        .ok()?;
    if response.headers().contains_key("x-oj-fallthrough") {
        return None;
    }
    Some(response)
}

pub async fn forward_get_to_plugin_mw(
    port: u16,
    path_and_query: &str,
    headers: &HeaderMap,
) -> Option<Response> {
    forward_to_plugin_mw(port, "GET", path_and_query, headers, None).await
}
