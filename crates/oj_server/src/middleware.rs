use super::*;

/// Vite's `server.allowedHosts` (middlewares/hostCheck.ts, server/ws.ts): a
/// request whose `Host` names something other than localhost, an IP literal, the
/// configured host, or an allowed host is refused with 403 so a malicious page
/// cannot reach the dev server through DNS rebinding. WebSocket upgrades apply
/// the same rule to `Origin`.
#[derive(Debug, Clone, Default)]
pub(crate) struct HostPolicy {
    pub(crate) allow_all: bool,
    pub(crate) allowed: Vec<String>,
}

impl HostPolicy {
    pub(crate) fn from_config(server: &oj_config::ServerConfig, cli_host: Option<&str>) -> Self {
        let mut allowed = Vec::new();
        match &server.allowed_hosts {
            Some(oj_config::AllowedHosts::All(true)) => {
                return Self {
                    allow_all: true,
                    allowed,
                }
            }
            Some(oj_config::AllowedHosts::List(list)) => {
                allowed.extend(list.iter().map(|h| h.to_ascii_lowercase()))
            }
            _ => {}
        }
        // A specific hostname the server was asked to bind to is allowed too.
        if let Some(h) = cli_host.or(server.host.as_deref()) {
            if !matches!(h, "true" | "0.0.0.0" | "::" | "[::]" | "localhost")
                && h.parse::<std::net::IpAddr>().is_err()
            {
                allowed.push(h.to_ascii_lowercase());
            }
        }
        Self {
            allow_all: false,
            allowed,
        }
    }

    pub(crate) fn hostname_allowed(&self, hostname: &str) -> bool {
        if self.allow_all {
            return true;
        }
        let host = hostname
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase();
        if host.is_empty()
            || host == "localhost"
            || host.ends_with(".localhost")
            || host.parse::<std::net::IpAddr>().is_ok()
        {
            return true;
        }
        self.allowed.iter().any(|a| {
            if let Some(domain) = a.strip_prefix('.') {
                host == domain || host.ends_with(a.as_str())
            } else {
                host == *a
            }
        })
    }

    /// The hostname of a `Host` header value (`example.com:5173`, `[::1]:5173`).
    pub(crate) fn host_header_name(value: &str) -> &str {
        let v = value.trim();
        if let Some(rest) = v.strip_prefix('[') {
            return rest.split(']').next().unwrap_or(rest);
        }
        v.rsplit_once(':').map(|(h, _)| h).unwrap_or(v)
    }

    pub(crate) fn reject_message(host: &str) -> String {
        format!(
            "Blocked request. This host ({host}) is not allowed.\nTo allow this host, add \"{host}\" to `server.allowedHosts` in your config."
        )
    }

    pub(crate) fn reject_ws_origin(&self, headers: &HeaderMap) -> Option<Response> {
        let origin = headers.get(header::ORIGIN)?.to_str().ok()?;
        let host = origin
            .split("://")
            .nth(1)
            .map(|rest| rest.split('/').next().unwrap_or(rest))
            .map(Self::host_header_name)
            .unwrap_or("");
        if self.hostname_allowed(host) {
            return None;
        }
        Some((StatusCode::FORBIDDEN, Self::reject_message(host)).into_response())
    }
}

pub(crate) async fn host_check_middleware(
    State(state): State<Arc<ServerState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if let Some(raw) = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
    {
        let host = HostPolicy::host_header_name(raw);
        if !state.host_policy.hostname_allowed(host) {
            return (StatusCode::FORBIDDEN, HostPolicy::reject_message(host)).into_response();
        }
    }
    next.run(req).await
}

/// Vite's `server.cors` (the `cors` package behind it). Unset: only localhost
/// origins (Vite's `defaultAllowedOrigins`); `true`: reflect any origin;
/// `false`: no CORS headers; an object: exact origins, methods, headers,
/// credentials, max-age.
#[derive(Debug, Clone)]
pub(crate) struct CorsPolicy {
    origin: CorsOrigin,
    pub(crate) methods: String,
    allowed_headers: Option<String>,
    pub(crate) credentials: bool,
    pub(crate) max_age: Option<u64>,
}

#[derive(Debug, Clone)]
pub(crate) enum CorsOrigin {
    Any,
    LocalhostDefault,
    List(Vec<String>),
}

impl CorsPolicy {
    pub(crate) fn from_config(cfg: Option<&oj_config::CorsConfig>) -> Option<Self> {
        let default_methods = "GET,HEAD,PUT,PATCH,POST,DELETE".to_string();
        let list_or_str = |v: &serde_json::Value| -> Option<String> {
            match v {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Array(a) => Some(
                    a.iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(","),
                ),
                _ => None,
            }
        };
        match cfg {
            Some(oj_config::CorsConfig::Toggle(false)) => None,
            Some(oj_config::CorsConfig::Toggle(true)) => Some(Self {
                origin: CorsOrigin::Any,
                methods: default_methods,
                allowed_headers: None,
                credentials: false,
                max_age: None,
            }),
            Some(oj_config::CorsConfig::Options(o)) => {
                let origin = match &o.origin {
                    Some(serde_json::Value::Bool(true)) | Some(serde_json::Value::String(_))
                        if o.origin.as_ref().and_then(|v| v.as_str()) == Some("*") =>
                    {
                        CorsOrigin::Any
                    }
                    Some(serde_json::Value::Bool(true)) => CorsOrigin::Any,
                    Some(serde_json::Value::Bool(false)) => return None,
                    Some(serde_json::Value::String(s)) => CorsOrigin::List(vec![s.clone()]),
                    Some(serde_json::Value::Array(a)) => CorsOrigin::List(
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect(),
                    ),
                    _ => CorsOrigin::LocalhostDefault,
                };
                Some(Self {
                    origin,
                    methods: o
                        .methods
                        .as_ref()
                        .and_then(list_or_str)
                        .unwrap_or(default_methods),
                    allowed_headers: o.allowed_headers.as_ref().and_then(list_or_str),
                    credentials: o.credentials.unwrap_or(false),
                    max_age: o.max_age,
                })
            }
            None => Some(Self {
                origin: CorsOrigin::LocalhostDefault,
                methods: default_methods,
                allowed_headers: None,
                credentials: false,
                max_age: None,
            }),
        }
    }

    pub(crate) fn allows(&self, origin: &str) -> bool {
        match &self.origin {
            CorsOrigin::Any => true,
            CorsOrigin::List(list) => list.iter().any(|o| o == origin),
            CorsOrigin::LocalhostDefault => is_localhost_origin(origin),
        }
    }
}

/// Vite's `defaultAllowedOrigins`:
/// `/^https?:\/\/(?:(?:[^:]+\.)?localhost|127\.0\.0\.1|\[::1\])(?::\d+)?$/`.
pub(crate) fn is_localhost_origin(origin: &str) -> bool {
    let rest = match origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
    {
        Some(r) => r,
        None => return false,
    };
    let (host, port) = if let Some(r) = rest.strip_prefix("[::1]") {
        ("[::1]", r)
    } else {
        rest.rsplit_once(':').unwrap_or((rest, ""))
    };
    let port_ok = port.is_empty()
        || port
            .strip_prefix(':')
            .unwrap_or(port)
            .chars()
            .all(|c| c.is_ascii_digit())
            && !port.strip_prefix(':').unwrap_or(port).is_empty();
    if !port_ok {
        return false;
    }
    host == "localhost"
        || host == "127.0.0.1"
        || host == "[::1]"
        || (host.ends_with(".localhost") && !host[..host.len() - ".localhost".len()].contains(':'))
}

pub(crate) async fn cors_middleware(
    State(policy): State<Arc<CorsPolicy>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let allowed = origin.as_deref().is_some_and(|o| policy.allows(o));
    let preflight = req.method() == axum::http::Method::OPTIONS
        && req
            .headers()
            .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);
    let mut resp = if preflight && allowed {
        let mut r = StatusCode::NO_CONTENT.into_response();
        let h = r.headers_mut();
        if let Ok(v) = policy.methods.parse() {
            h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, v);
        }
        let requested = req
            .headers()
            .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
            .cloned();
        match (&policy.allowed_headers, requested) {
            (Some(list), _) => {
                if let Ok(v) = list.parse() {
                    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, v);
                }
            }
            (None, Some(v)) => {
                h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, v);
                h.append(
                    header::VARY,
                    header::HeaderValue::from_static("Access-Control-Request-Headers"),
                );
            }
            (None, None) => {}
        }
        if let Some(age) = policy.max_age {
            if let Ok(v) = age.to_string().parse() {
                h.insert(header::ACCESS_CONTROL_MAX_AGE, v);
            }
        }
        h.insert(
            header::CONTENT_LENGTH,
            header::HeaderValue::from_static("0"),
        );
        r
    } else {
        next.run(req).await
    };
    if allowed {
        let h = resp.headers_mut();
        if let Some(v) = origin.as_deref().and_then(|o| o.parse().ok()) {
            h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, v);
        }
        h.append(header::VARY, header::HeaderValue::from_static("Origin"));
        if policy.credentials {
            h.insert(
                header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
                header::HeaderValue::from_static("true"),
            );
        }
    }
    resp
}

pub(crate) async fn apply_dev_headers(
    State(headers): State<Arc<Vec<(header::HeaderName, header::HeaderValue)>>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    for (name, value) in headers.iter() {
        h.insert(name.clone(), value.clone());
    }
    resp
}
