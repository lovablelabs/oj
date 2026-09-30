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
    // Stream the worker/plugin response through instead of buffering it. TanStack
    // Start streams its dehydrated data (queryStream + deferred promises) into the
    // HTML; buffering with resp.bytes() withholds the whole document until the SSR
    // stream closes, so the client's hydration never sees it progressively. Vite
    // pipes the worker Response body through (Readable.fromWeb); this is the same.
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

// Forward a GET to a plugin's configureServer middleware; returns None when the
// middleware falls through (x-oj-fallthrough), so the caller can fall back to
// SSR. Used by the TanStack start path, where GET requests are otherwise
// SSR'd and would never reach editor endpoints (the dev-server bridge).
// Tell a plugin's configureServer middleware server that source files changed,
// so it can invalidate the DevEnvironments' module graphs and send targeted
// HMR updates (the Cloudflare-plugin HMR path). Each change carries Vite's
// watcher event type: "update" | "create" | "delete". Fire-and-forget.
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
    // The endpoint answers only after the plugin hotUpdate hooks ran, and the
    // settled-batch call blocks the watcher thread: a hung hook must not
    // freeze rebuilds for the session, so the request is bounded.
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_default()
    })
}

/// Tell the plugin middleware to resynchronize: invalidate every runner-backed
/// DevEnvironment's whole module graph and send a full-reload, bypassing the
/// per-change dedup. Sent on late activation, covering every edit made while
/// the middleware path was down (the watcher had no port to invalidate).
/// Returns whether the middleware acknowledged the ENQUEUE: the host answers
/// the moment the resync is on its serialized invalidate queue (guaranteed to
/// run after everything already queued), so the ACK is fast even behind a
/// slow queue. 202-style semantics: the ACK means only "enqueued", never
/// "ran" — the host pushes `{ ojResyncDone }` when the resync EXECUTES, and
/// the caller claims "resynced" only on that completion signal
/// ([`await_resync_completion`]); a queue that never drains warns instead of
/// logging success.
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

/// [`notify_plugin_mw_resync`] with a few backed-off retries: the resync races
/// the host's middleware server settling in, and a transient failure must not
/// leave the degraded window's edits silently stale. The host ACKs on enqueue
/// and coalesces duplicates (a pending resync absorbs them), so retrying is
/// safe — it can never stack full-reloads — and a client timeout means only
/// "enqueue unconfirmed", which the next attempt settles either way. `false`
/// after the last attempt — the caller then warns instead of logging success.
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

/// Waits for the host's resync-executed signal — the `{ ojResyncDone }`
/// counter moving past the `baseline` snapshotted BEFORE the enqueue (so a
/// completion racing ahead of this wait is never missed, and one push may
/// answer several coalesced enqueues) — bounded, so a stuck invalidate queue
/// turns into a caller warning rather than an eternal wait or a false
/// "resynced" claim off the enqueue ack.
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

// Method+body version of forward_get_to_plugin_mw, for the /_serverFn/ path so
// server functions reach a Cloudflare plugin's worker (Miniflare) like documents
// do, instead of running in the Node runner without the real runtime/bindings.
/// Proxy one request to a loopback HTTP service (the plugin middleware server or
/// the Start SSR runner) and stream its response back. Bodies are passed as
/// bytes, so binary uploads and responses survive; the original `Host` travels
/// as `x-oj-host` (see `loopback_request_headers`) so the service can build the
/// app's own absolute URLs.
/// Stream the response through instead of buffering it: TanStack Start streams
/// its dehydrated data (queryStream + deferred promises) into the HTML, and
/// buffering withholds the whole document until the SSR stream closes, so the
/// client's hydration never sees it progressively. Vite pipes the Response body
/// through (Readable.fromWeb); this is the same.
pub async fn proxy_to_loopback(
    port: u16,
    method: &str,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Option<Vec<u8>>,
) -> Result<Response, String> {
    proxy_to_loopback_streaming(port, method, path_and_query, headers, body.map(Body::from)).await
}

/// The headers a request forwarded to a loopback service (the plugin middleware
/// server, the Start SSR runner) carries. hyper writes the loopback `Host`
/// itself, so the browser's `Host` travels as `x-oj-host` and the service
/// rebuilds `Host` from it. Only the first `Host` is taken: Node discards
/// duplicate `Host` headers and keeps the first, so a joined value would never
/// reach an app under Vite. A proxy's own `x-forwarded-host` passes through
/// untouched, as under Vite where the app reads it next to the dev server's
/// `Host`; sending it as `x-forwarded-host` too made Node join the two into
/// `proxy-host, localhost:port`, which no URL parser accepts. An incoming
/// `x-oj-host` is dropped so a client cannot spoof it.
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
