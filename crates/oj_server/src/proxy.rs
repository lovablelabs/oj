use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite as ts;

/// Delegate a matched request to the Vite-shaped proxy in the plugin host. On
/// `x-oj-fallthrough`, re-route the (possibly rewritten) url natively, bodyless.
pub(crate) async fn delegate_to_node_proxy(
    next: axum::middleware::Next,
    port: u16,
    req: axum::extract::Request,
) -> Response {
    let (mut parts, body) = req.into_parts();
    let method = parts.method.as_str().to_string();
    let pq = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| parts.uri.path().to_string());
    let resp = match proxy_to_loopback_streaming(port, &method, &pq, &parts.headers, Some(body))
        .await
    {
        Ok(resp) => resp,
        Err(e) => return bad_gateway(format!("oj proxy delegation to plugin host failed: {e}")),
    };
    if !resp.headers().contains_key("x-oj-fallthrough") {
        return resp;
    }
    // Re-route only origin-form paths; an absolute or
    // protocol-relative bypass URL must never hit native routing.
    if let Some(uri) = resp
        .headers()
        .get("x-oj-rewritten-url")
        .and_then(|v| v.to_str().ok())
        .filter(|s| s.starts_with('/') && !s.starts_with("//"))
        .and_then(|s| s.parse::<Uri>().ok())
    {
        parts.uri = uri;
    }
    next.run(axum::extract::Request::from_parts(parts, Body::empty()))
        .await
}

fn bad_gateway(msg: String) -> Response {
    (StatusCode::BAD_GATEWAY, msg).into_response()
}

pub(crate) fn proxy_target(
    entry: &oj_config::ProxyEntry,
    path: &str,
    query: Option<&str>,
) -> String {
    let mut fwd_path = path.to_string();
    if let Some((from, to)) = entry.rewrite() {
        if let Some(stripped) = from.strip_prefix('^') {
            if let Some(rest) = fwd_path.strip_prefix(stripped) {
                fwd_path = format!("{to}{rest}");
            }
        } else {
            fwd_path = fwd_path.replacen(from, to, 1);
        }
    }
    let query = query.map(|q| format!("?{q}")).unwrap_or_default();
    format!(
        "{}{}{}",
        entry.target().trim_end_matches('/'),
        fwd_path,
        query
    )
}

pub(crate) async fn proxy_middleware(
    State(state): State<Arc<ServerState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = req.uri().path().to_string();
    // Vite matches contexts against path plus query, so a `^` regex context
    // can key off a query parameter.
    let url = match req.uri().query() {
        Some(q) => format!("{path}?{q}"),
        None => path.clone(),
    };
    let matched = select_proxy(&state.proxy, &state.proxy_regex, &url);
    let Some((prefix, entry)) = matched else {
        // A `^` context oj's regex engine cannot compile never matches here
        // (warned once at startup); the worker path still proxies it.
        return next.run(req).await;
    };
    let prefix = prefix.to_string();
    let entry = entry.clone();

    // `ws: true` upgrades are tunneled by the Rust proxy; the Node proxy never
    // handles upgrades (a worker's outbound fetch is never one).
    if entry.ws() && is_websocket_upgrade(req.headers()) {
        let target = proxy_target(&entry, &path, req.uri().query());
        return proxy_websocket(state, req, Some(&entry), &target).await;
    }

    // `ws: false` (Vite default): relay the upgrade to the plugin middleware
    // server's `upgrade` listeners; vite-hmr/vite-ping stay with oj's endpoint.
    if is_websocket_upgrade(req.headers()) && vite_ws_subprotocol(req.headers()).is_none() {
        if let Some(port) = state.plugin_serve.mw_port() {
            return relay_upgrade_to_plugin_middleware(state, req, port).await;
        }
    }

    // With a plugin host running, browser and worker share ONE Vite-shaped proxy
    // there (function rewrite/configure/bypass intact); the ORIGINAL unstripped
    // path is forwarded and re-matched by the Node proxy.
    if let Some(port) = state.plugin_serve.mw_port() {
        return delegate_to_node_proxy(next, port, req).await;
    }

    // Fallback (no plugin host, or the boot window before the middleware port
    // is known): the Rust proxy forwards directly with the {from,to} rewrite.
    let target = proxy_target(&entry, &path, req.uri().query());
    forward_direct(&state, req, &entry, &prefix, &target).await
}

/// The Rust-side proxy: forward to `target` and stream the reply back.
async fn forward_direct(
    state: &ServerState,
    req: axum::extract::Request,
    entry: &oj_config::ProxyEntry,
    prefix: &str,
    target: &str,
) -> Response {
    const BUFFER_LIMIT: usize = 1024 * 1024;
    let method = req.method().clone();
    let req_headers = req.headers().clone();
    // Buffer small bodies to preserve Content-Length; stream chunked or large
    // bodies so uploads are not capped.
    let content_length = req_headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let chunked = req_headers.contains_key(header::TRANSFER_ENCODING);
    let stream_request = chunked || content_length.is_some_and(|n| n > BUFFER_LIMIT as u64);
    let body: reqwest::Body = if stream_request {
        reqwest::Body::wrap_stream(req.into_body().into_data_stream())
    } else {
        match axum::body::to_bytes(req.into_body(), BUFFER_LIMIT).await {
            Ok(b) => reqwest::Body::from(b),
            Err(e) => return bad_gateway(format!("oj proxy: body read: {e}")),
        }
    };

    let client = if entry.secure() {
        &state.http
    } else {
        state.http_insecure.get_or_init(|| {
            reqwest::Client::builder()
                .danger_accept_invalid_certs(true)
                .build()
                .expect("reqwest client")
        })
    };
    let mut out = client.request(method, target).body(body);
    for (name, value) in req_headers.iter() {
        if entry.change_origin() && name == header::HOST {
            continue;
        }
        out = out.header(name, value);
    }

    match out.send().await {
        // Stream the upstream body so SSE/long-polling arrive chunk by chunk.
        Ok(resp) => stream_reqwest_response(resp),
        Err(e) => bad_gateway(format!("oj proxy to {prefix} failed: {e}")),
    }
}

/// Compiled `^` proxy context (Vite: `new RegExp(context)`); `None` for prefixes.
/// An uncompilable pattern warns once and then never matches.
pub(crate) fn proxy_context_regex(context: &str) -> Option<regex::Regex> {
    if !context.starts_with('^') {
        return None;
    }
    match regex::Regex::new(context) {
        Ok(re) => Some(re),
        Err(err) => {
            eprintln!(
                "oj: warning: server.proxy context {context:?} uses a regex oj cannot compile \
                 ({err}) — likely a JS RegExp lookahead/backreference/alternation. Browser \
                 requests to it are matched by literal prefix and may not proxy (the worker path \
                 still proxies it); please file an issue if you need full JS-regex proxy contexts."
            );
            None
        }
    }
}

/// Vite's `doesProxyContextMatchUrl`: a `^` context is a regex tested against
/// the request url (path plus query), any other context is a path prefix.
pub fn proxy_context_matches(context: &str, url: &str) -> bool {
    if let Some(re) = proxy_context_regex(context) {
        return re.is_match(url);
    }
    url.starts_with(context)
}

/// The proxy entry for a url: longest matching plain prefix wins, a regex
/// context applies when no prefix matches (oj's config is a sorted map).
pub(crate) fn select_proxy<'a>(
    entries: &'a [(String, oj_config::ProxyEntry)],
    regexes: &[Option<regex::Regex>],
    url: &str,
) -> Option<(&'a str, &'a oj_config::ProxyEntry)> {
    let prefix = entries
        .iter()
        .zip(regexes)
        .filter(|((ctx, _), re)| re.is_none() && url.starts_with(ctx.as_str()))
        .max_by_key(|((ctx, _), _)| ctx.len())
        .map(|((ctx, entry), _)| (ctx.as_str(), entry));
    prefix.or_else(|| {
        entries
            .iter()
            .zip(regexes)
            .find(|(_, re)| re.as_ref().is_some_and(|re| re.is_match(url)))
            .map(|((ctx, entry), _)| (ctx.as_str(), entry))
    })
}

pub fn is_websocket_upgrade(h: &HeaderMap) -> bool {
    h.get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// The http(s) origin of a ws(s) target url, for `rewriteWsOrigin`.
pub(crate) fn ws_target_origin(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (s, r),
        None => return url.to_string(),
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    let scheme = match scheme {
        "wss" => "https",
        "ws" => "http",
        other => other,
    };
    format!("{scheme}://{authority}")
}

pub(crate) fn ws_target_url(target: &str) -> String {
    if let Some(rest) = target.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = target.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        target.to_string()
    }
}

/// Request headers that travel to the upstream WebSocket. The handshake headers
/// are regenerated by the client library (duplicates would fail its handshake).
pub(crate) fn ws_forwardable_header(name: &header::HeaderName) -> bool {
    !matches!(
        *name,
        header::HOST
            | header::CONNECTION
            | header::UPGRADE
            | header::SEC_WEBSOCKET_KEY
            | header::SEC_WEBSOCKET_VERSION
            | header::SEC_WEBSOCKET_ACCEPT
            | header::SEC_WEBSOCKET_EXTENSIONS
            | header::CONTENT_LENGTH
            | header::TRANSFER_ENCODING
    )
}

impl ServerState {
    /// rustls config for a proxied `wss://` target, built once: platform trust
    /// store, or no certificate check when the entry says `secure: false`.
    fn proxy_tls_config(
        &self,
        secure: bool,
    ) -> Result<std::sync::Arc<rustls::ClientConfig>, String> {
        self.proxy_tls[usize::from(!secure)]
            .get_or_init(|| proxy_tls_config(secure).map(std::sync::Arc::new))
            .clone()
    }
}

pub(crate) fn proxy_tls_config(secure: bool) -> Result<rustls::ClientConfig, String> {
    use rustls_platform_verifier::BuilderVerifierExt;
    let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(std::sync::Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?;
    if secure {
        return Ok(builder
            .with_platform_verifier()
            .map_err(|e| e.to_string())?
            .with_no_client_auth());
    }
    Ok(builder
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(AcceptAnyCertificate(provider)))
        .with_no_client_auth())
}

/// `secure: false`: the certificate is not checked; signatures still are, so the
/// connection is at least the one the server we reached is speaking on.
#[derive(Debug)]
pub(crate) struct AcceptAnyCertificate(std::sync::Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Hand an unclaimed browser upgrade to the plugin middleware server so
/// configureServer `upgrade` listeners fire (Vite shared-httpServer parity).
pub(crate) async fn relay_upgrade_to_plugin_middleware(
    state: Arc<ServerState>,
    req: axum::extract::Request,
    port: u16,
) -> Response {
    let raw = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| req.uri().path().to_string());
    let target = format!("http://127.0.0.1:{port}{raw}");
    proxy_websocket(state, req, None, &target).await
}

/// `ws: true`: accept the browser's WebSocket, open one to the target with the
/// same path/query/subprotocols/cookies, relay both ways (Vite: http-proxy `ws`).
pub(crate) async fn proxy_websocket(
    state: Arc<ServerState>,
    req: axum::extract::Request,
    entry: Option<&oj_config::ProxyEntry>,
    target: &str,
) -> Response {
    let (mut parts, _body) = req.into_parts();
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        Ok(u) => u,
        Err(rejection) => return rejection.into_response(),
    };
    let url = ws_target_url(target);
    let upstream_req = match upstream_ws_request(&url, &parts.headers, entry) {
        Ok(r) => r,
        Err(msg) => return bad_gateway(msg),
    };
    // `wss://`: tungstenite dials TCP and TLS itself with the rustls config
    // (`secure: false` accepts any certificate), as http-proxy does.
    let connector = if url.starts_with("wss://") {
        match state.proxy_tls_config(entry.is_none_or(|e| e.secure())) {
            Ok(cfg) => Some(tokio_tungstenite::Connector::Rustls(cfg)),
            Err(e) => return bad_gateway(format!("oj proxy: tls config: {e}")),
        }
    } else {
        Some(tokio_tungstenite::Connector::Plain)
    };
    // No oj-side deadline, matching Vite: an unclaimed upgrade dangles until
    // the client gives up; hyper aborts this future on client disconnect.
    let connected =
        tokio_tungstenite::connect_async_tls_with_config(upstream_req, None, false, connector)
            .await;
    let (upstream, upstream_resp) = match connected {
        Ok(pair) => pair,
        Err(e) => return bad_gateway(format!("oj proxy: websocket to {url} failed: {e}")),
    };
    let selected = upstream_resp
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let upgrade = match selected {
        Some(p) => upgrade.protocols([p]),
        None => upgrade,
    };
    upgrade.on_upgrade(move |client| relay_ws(client, upstream))
}

/// The upstream handshake request (Err: the 502 message). tungstenite takes a
/// ready-made request as-is: handshake headers must be set explicitly (it
/// generates them only for a bare url).
fn upstream_ws_request(
    url: &str,
    headers: &HeaderMap,
    entry: Option<&oj_config::ProxyEntry>,
) -> Result<axum::http::Request<()>, String> {
    let target_uri: Uri = url
        .parse()
        .map_err(|e| format!("oj proxy: bad websocket target {url}: {e}"))?;
    // host:port without userinfo, the same authority the dial uses.
    let target_host = match (target_uri.host(), target_uri.port_u16()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_string(),
        _ => String::new(),
    };
    // http-proxy keeps the browser's Host unless `changeOrigin` asks for the
    // target's; `rewriteWsOrigin` (Vite) swaps the Origin for the target's origin.
    let host = if entry.is_some_and(|e| e.change_origin()) {
        target_host
    } else {
        headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .unwrap_or(target_host)
    };
    let mut builder = axum::http::Request::builder()
        .uri(url)
        .header(header::HOST, host)
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "websocket")
        .header(header::SEC_WEBSOCKET_VERSION, "13")
        .header(
            header::SEC_WEBSOCKET_KEY,
            ts::handshake::client::generate_key(),
        );
    let rewrite_origin = entry.is_some_and(|e| e.rewrite_ws_origin());
    for (name, value) in headers.iter() {
        if !ws_forwardable_header(name) {
            continue;
        }
        if *name == header::ORIGIN && rewrite_origin {
            builder = builder.header(name, ws_target_origin(url));
            continue;
        }
        builder = builder.header(name, value);
    }
    builder
        .body(())
        .map_err(|e| format!("oj proxy: ws request: {e}"))
}

/// Pump messages both ways until either side ends, then close the other.
async fn relay_ws(
    client: axum::extract::ws::WebSocket,
    upstream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) {
    let (mut up_tx, mut up_rx) = upstream.split();
    let (mut cl_tx, mut cl_rx) = client.split();
    let client_to_upstream = async {
        while let Some(Ok(m)) = cl_rx.next().await {
            if up_tx.send(to_ts(m)).await.is_err() {
                break;
            }
        }
        let _ = up_tx.close().await;
    };
    let upstream_to_client = async {
        while let Some(Ok(m)) = up_rx.next().await {
            if let Some(m) = from_ts(m) {
                if cl_tx.send(m).await.is_err() {
                    break;
                }
            }
        }
        let _ = cl_tx.close().await;
    };
    tokio::join!(client_to_upstream, upstream_to_client);
}

fn to_ts(m: Message) -> ts::Message {
    match m {
        Message::Text(t) => ts::Message::Text(ts::Utf8Bytes::from(t.as_str())),
        Message::Binary(b) => ts::Message::Binary(b),
        Message::Ping(b) => ts::Message::Ping(b),
        Message::Pong(b) => ts::Message::Pong(b),
        Message::Close(c) => ts::Message::Close(c.map(|c| ts::protocol::CloseFrame {
            code: ts::protocol::frame::coding::CloseCode::from(c.code),
            reason: ts::Utf8Bytes::from(c.reason.as_str()),
        })),
    }
}

fn from_ts(m: ts::Message) -> Option<Message> {
    Some(match m {
        ts::Message::Text(t) => Message::Text(t.as_str().into()),
        ts::Message::Binary(b) => Message::Binary(b),
        ts::Message::Ping(b) => Message::Ping(b),
        ts::Message::Pong(b) => Message::Pong(b),
        ts::Message::Close(c) => Message::Close(c.map(|c| axum::extract::ws::CloseFrame {
            code: c.code.into(),
            reason: c.reason.as_str().into(),
        })),
        ts::Message::Frame(_) => return None,
    })
}
