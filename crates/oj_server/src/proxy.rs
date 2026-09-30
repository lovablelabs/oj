use super::*;

/// The upstream url for a matched proxy entry: the target joined with the
/// (rewritten) request path and the original query. Applies the `{from,to}`
/// string rewrite form only; the FUNCTION rewrite form is honored by the Node
/// proxy (the browser path delegates there when a plugin host is running).
/// Delegate a request to the single Vite-shaped proxy in the plugin host's
/// middleware stack: stream the body there, stream the reply back. If the host's
/// proxy declined (`x-oj-fallthrough` — a `bypass` that returned a string and
/// fell through, or a JS-RegExp non-match on an uncompilable context), serve the
/// possibly-rewritten url through normal oj routing instead of returning the
/// host's 404. The re-routed request carries no body (it was streamed to the
/// host): this covers the realistic GET bypass rewrite; a non-GET fallthrough
/// re-route loses its body.
pub(crate) async fn delegate_to_node_proxy(
    next: axum::middleware::Next,
    port: u16,
    req: axum::extract::Request,
) -> Response {
    let (parts, body) = req.into_parts();
    let method = parts.method.as_str().to_string();
    let pq = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| parts.uri.path().to_string());
    match proxy_to_loopback_streaming(port, &method, &pq, &parts.headers, Some(body)).await {
        Ok(resp) => {
            if resp.headers().contains_key("x-oj-fallthrough") {
                let mut parts = parts;
                // Re-route only to an origin-form path (starts with a single
                // `/`): a `bypass` returning an absolute or protocol-relative
                // URL must never be fed to native routing — treat that as
                // no-rewrite and serve the original path.
                if let Some(uri) = resp
                    .headers()
                    .get("x-oj-rewritten-url")
                    .and_then(|v| v.to_str().ok())
                    .filter(|s| s.starts_with('/') && !s.starts_with("//"))
                    .and_then(|s| s.parse::<Uri>().ok())
                {
                    parts.uri = uri;
                }
                let rreq = axum::extract::Request::from_parts(parts, Body::empty());
                return next.run(rreq).await;
            }
            resp
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("oj proxy delegation to plugin host failed: {e}"),
        )
            .into_response(),
    }
}

pub(crate) fn proxy_target(entry: &oj_config::ProxyEntry, path: &str, query: Option<&str>) -> String {
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
    // Vite matches contexts against `req.url`, path and query together, so a
    // `^` regex context can key off a query parameter.
    let url = match req.uri().query() {
        Some(q) => format!("{path}?{q}"),
        None => path.clone(),
    };
    let matched = select_proxy(&state.proxy, &state.proxy_regex, &url);
    let Some((prefix, entry)) = matched else {
        // No context matched. A `^`-context oj's regex engine could not compile
        // (a JS RegExp using lookaround/backreference/alternation) never matches
        // on this browser-facing path and is served natively — oj's pre-existing
        // behavior, with a loud one-time warning at startup (proxy_context_regex).
        // The worker path still proxies such a context (the Node proxy holds the
        // full JS RegExp); full JS-regex-context support for the browser path is
        // a deferred follow-up rather than an unsound approximation here.
        return next.run(req).await;
    };
    let prefix = prefix.to_string();
    let entry = entry.clone();

    // A WebSocket upgrade on a `ws: true` entry is tunneled message by message
    // by the Rust proxy: the inbound listener owns the browser's upgrade, and
    // the worker's outbound fetch is never a ws upgrade, so the single Node
    // proxy need not handle upgrades — nothing regresses.
    if entry.ws() && is_websocket_upgrade(req.headers()) {
        let target = proxy_target(&entry, &path, req.uri().query());
        return proxy_websocket(state, req, Some(&entry), &target).await;
    }

    // `ws: false` (Vite's default): the upgrade belongs to the shared
    // httpServer's `upgrade` listeners, so relay it to the plugin middleware
    // server as a real upgrade. vite-hmr/vite-ping stay with oj's own
    // endpoint (this middleware wraps outside vite_hmr_upgrade).
    if is_websocket_upgrade(req.headers()) && vite_ws_subprotocol(req.headers()).is_none() {
        if let Some(port) = state.plugin_serve.mw_port() {
            return relay_upgrade_to_plugin_middleware(state, req, port).await;
        }
    }

    // The single, Vite-shaped proxy lives in the plugin host's middleware stack.
    // Whenever a plugin host is running, delegate the matched request there so
    // the browser path and the worker's outbound fetch share ONE proxy, with the
    // app's real config (function `rewrite`, `configure`, `bypass`) intact. The
    // ORIGINAL unstripped path is forwarded; the Node proxy re-matches and
    // rewrites it. The request body streams through, so uploads are not capped;
    // a `bypass` that returned a string (host proxy falls through) is served via
    // normal routing at the rewritten url rather than returning the host's 404.
    if let Some(port) = state.plugin_serve.mw_port() {
        return delegate_to_node_proxy(next, port, req).await;
    }

    // Fallback: no plugin host (a plain `oj dev` app with `server.proxy`, or the
    // brief boot window before the middleware port is known). The Rust proxy
    // forwards directly, applying the `{from,to}` string rewrite form.
    let target = proxy_target(&entry, &path, req.uri().query());
    let method = req.method().clone();
    let req_headers = req.headers().clone();
    // Small bodies are buffered so their Content-Length is preserved; a chunked
    // or large body streams through, so uploads are not capped by a buffer.
    let content_length = req_headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let chunked = req_headers.contains_key(header::TRANSFER_ENCODING);
    let stream_request = chunked || content_length.is_some_and(|n| n > 1024 * 1024);
    let body: reqwest::Body = if stream_request {
        reqwest::Body::wrap_stream(req.into_body().into_data_stream())
    } else {
        match axum::body::to_bytes(req.into_body(), 1024 * 1024).await {
            Ok(b) => reqwest::Body::from(b),
            Err(e) => {
                return (StatusCode::BAD_GATEWAY, format!("oj proxy: body read: {e}"))
                    .into_response()
            }
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
    let mut out = client.request(method, &target).body(body);
    for (name, value) in req_headers.iter() {
        if entry.change_origin() && name == header::HOST {
            continue;
        }
        out = out.header(name, value);
    }

    match out.send().await {
        Ok(resp) => {
            let status = resp.status();
            let headers = resp.headers().clone();
            // Stream the upstream body: server-sent events and long-polling
            // responses reach the browser chunk by chunk instead of at the end.
            let mut response = Response::new(Body::from_stream(resp.bytes_stream()));
            *response.status_mut() = status;
            for (name, value) in headers.iter() {
                if name == header::TRANSFER_ENCODING || name == header::CONTENT_LENGTH {
                    continue;
                }
                response.headers_mut().append(name, value.clone());
            }
            response
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("oj proxy to {prefix} failed: {e}"),
        )
            .into_response(),
    }
}

/// The compiled pattern of a `server.proxy` context that starts with `^` (Vite:
/// `new RegExp(context)`); `None` for a plain prefix context. A pattern that
/// does not compile is reported once and then behaves as a prefix, which for a
/// `^...` string never matches, the same as Vite throwing at startup would leave
/// it unreachable.
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

/// The proxy entry for a request url. Vite takes the first matching context in
/// config order; oj's config is a sorted map, so the most specific plain prefix
/// wins and a regex context applies when no prefix matches.
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

/// The upstream WebSocket url for a proxy target (`http://` targets become `ws://`).
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
    /// The rustls config for a proxied `wss://` target, built once: the platform
    /// trust store (what reqwest verifies with for the HTTP side), or no
    /// certificate check at all when the proxy entry says `secure: false`
    /// (http-proxy's `secure`), for self-signed dev backends.
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

/// A browser upgrade no Rust endpoint claims: hand it to the plugin
/// middleware server as a real upgrade, so configureServer `upgrade`
/// listeners fire like on Vite's shared httpServer.
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

/// `server.proxy` with `ws: true`: accept the browser's WebSocket, open one to
/// the target with the same path, query, subprotocols and cookies, and relay
/// messages both ways until either side closes (Vite: http-proxy `ws`).
pub(crate) async fn proxy_websocket(
    state: Arc<ServerState>,
    req: axum::extract::Request,
    entry: Option<&oj_config::ProxyEntry>,
    target: &str,
) -> Response {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite as ts;

    let (mut parts, _body) = req.into_parts();
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        Ok(u) => u,
        Err(rejection) => return rejection.into_response(),
    };
    let url = ws_target_url(target);
    // tungstenite takes a ready-made request as-is: the handshake headers have
    // to be present (it generates them only for a bare url), then the browser's
    // remaining headers (cookies, origin, subprotocols) ride along.
    let target_uri: axum::http::Uri = match url.parse() {
        Ok(u) => u,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("oj proxy: bad websocket target {url}: {e}"),
            )
                .into_response()
        }
    };
    // host:port without any userinfo, the same authority the dial below uses.
    let target_host = match (target_uri.host(), target_uri.port_u16()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_string(),
        _ => String::new(),
    };
    // http-proxy keeps the browser's Host unless `changeOrigin` asks for the
    // target's; `rewriteWsOrigin` (Vite) swaps the Origin for the target's origin.
    let host: String = if entry.is_some_and(|e| e.change_origin()) {
        target_host.clone()
    } else {
        parts
            .headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .unwrap_or(target_host)
    };
    let mut builder = axum::http::Request::builder()
        .uri(url.as_str())
        .header(header::HOST, host)
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "websocket")
        .header(header::SEC_WEBSOCKET_VERSION, "13")
        .header(
            header::SEC_WEBSOCKET_KEY,
            ts::handshake::client::generate_key(),
        );
    for (name, value) in parts.headers.iter() {
        if !ws_forwardable_header(name) {
            continue;
        }
        if *name == header::ORIGIN && entry.is_some_and(|e| e.rewrite_ws_origin()) {
            builder = builder.header(name, ws_target_origin(&url));
            continue;
        }
        builder = builder.header(name, value);
    }
    let upstream_req = match builder.body(()) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("oj proxy: ws request: {e}"),
            )
                .into_response()
        }
    };
    // `wss://` targets: tungstenite dials TCP and TLS itself, given a rustls
    // config (system trust store, or any certificate when the entry says
    // `secure: false`), as http-proxy does for a `wss:` target.
    let connector = if url.starts_with("wss://") {
        match state.proxy_tls_config(entry.is_none_or(|e| e.secure())) {
            Ok(cfg) => Some(tokio_tungstenite::Connector::Rustls(cfg)),
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    format!("oj proxy: tls config: {e}"),
                )
                    .into_response()
            }
        }
    } else {
        Some(tokio_tungstenite::Connector::Plain)
    };
    let connect =
        tokio_tungstenite::connect_async_tls_with_config(upstream_req, None, false, connector);
    // No oj-side deadline, matching Vite: its plugin `upgrade` listeners
    // share the http server, so an upgrade nobody claims simply dangles until
    // the CLIENT gives up — the client owns the patience. Here the dangle is
    // this pending dial (the middleware server accepted TCP but no listener
    // answered the handshake); hyper aborts this future when the client
    // disconnects, so the dangling cost is one socket and one parked task per
    // waiting client, the same surface Vite exposes.
    let connected = connect.await;
    let (upstream, upstream_resp) = match connected {
        Ok(pair) => pair,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("oj proxy: websocket to {url} failed: {e}"),
            )
                .into_response()
        }
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

    upgrade.on_upgrade(move |client| async move {
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
    })
}
