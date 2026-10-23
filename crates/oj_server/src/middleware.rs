use super::*;

/// Vite's `server.allowedHosts`: a Host outside localhost/IP/configured/allowed
/// gets 403 (DNS-rebinding guard); ws upgrades apply the same rule to Origin.
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

    /// 403 for a `Host` header outside the policy; `None` lets the request through.
    pub(crate) fn reject_host(&self, headers: &HeaderMap) -> Option<Response> {
        let raw = headers.get(header::HOST)?.to_str().ok()?;
        let host = Self::host_header_name(raw);
        if self.hostname_allowed(host) {
            return None;
        }
        Some((StatusCode::FORBIDDEN, Self::reject_message(host)).into_response())
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

/// The dev server's base, as Vite's resolveConfig gives it in serve: `""`,
/// `"./"` and other relative bases are the root, and a URL base keeps only
/// its path. `None` for the root, which needs no base handling.
pub(crate) fn dev_base(raw: Option<&str>) -> Option<String> {
    let raw = raw?;
    if raw.is_empty() || raw.starts_with('.') {
        return None;
    }
    let path = match raw.split_once("//") {
        Some((scheme, rest)) if scheme.is_empty() || scheme.ends_with(':') => {
            rest.find('/').map_or("/", |i| &rest[i..])
        }
        _ => raw,
    };
    let path = path.split(['?', '#']).next().unwrap_or("/");
    let base = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    (base != "/").then_some(base)
}

/// Vite's stripBase: the path past `base`, starting with `/`, or `None` when
/// the path is not under it (`/apple.svg` is not under `/app/`).
pub(crate) fn strip_base<'a>(path: &'a str, base: &str) -> Option<&'a str> {
    let base = base.trim_end_matches('/');
    if path == base {
        return Some("/");
    }
    path.strip_prefix(base).filter(|rest| rest.starts_with('/'))
}

/// Vite's baseMiddleware: a request under `base` reaches the routes with the
/// base stripped, so oj's own `/@oj/...` routes answer `/app/@oj/client.js`,
/// and a page visit to `/` redirects to the base. Unbased requests pass
/// through, as in Vite's middleware mode: oj's rewritten import URLs carry no
/// base. Handlers that need the browser's URL read `OriginalUri`.
pub(crate) async fn base_middleware(
    State(base): State<Arc<str>>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = req.uri().path();
    if let Some(rest) = strip_base(path, &base) {
        let pq = match req.uri().query() {
            Some(q) => format!("{rest}?{q}"),
            None => rest.to_string(),
        };
        if let Ok(uri) = pq.parse::<Uri>() {
            *req.uri_mut() = uri;
        }
        return next.run(req).await;
    }
    if (path == "/" || path == "/index.html") && !is_websocket_upgrade(req.headers()) {
        let query = req
            .uri()
            .query()
            .map(|q| format!("?{q}"))
            .unwrap_or_default();
        return (
            StatusCode::FOUND,
            [(header::LOCATION, format!("{base}{query}"))],
        )
            .into_response();
    }
    next.run(req).await
}

pub(crate) async fn host_check_middleware(
    State(state): State<Arc<ServerState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if let Some(resp) = state.host_policy.reject_host(req.headers()) {
        return resp;
    }
    next.run(req).await
}

/// Vite's `server.cors`. Unset: localhost origins only; `true`: reflect any
/// origin; `false`: no CORS headers; object: exact origins/methods/headers.
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
    /// Vite's defaults for everything but the origin rule.
    fn with_origin(origin: CorsOrigin) -> Self {
        Self {
            origin,
            methods: "GET,HEAD,PUT,PATCH,POST,DELETE".to_string(),
            allowed_headers: None,
            credentials: false,
            max_age: None,
        }
    }

    pub(crate) fn from_config(cfg: Option<&oj_config::CorsConfig>) -> Option<Self> {
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
            Some(oj_config::CorsConfig::Toggle(true)) => Some(Self::with_origin(CorsOrigin::Any)),
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
                let mut policy = Self::with_origin(origin);
                if let Some(methods) = o.methods.as_ref().and_then(list_or_str) {
                    policy.methods = methods;
                }
                policy.allowed_headers = o.allowed_headers.as_ref().and_then(list_or_str);
                policy.credentials = o.credentials.unwrap_or(false);
                policy.max_age = o.max_age;
                Some(policy)
            }
            None => Some(Self::with_origin(CorsOrigin::LocalhostDefault)),
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

#[cfg(test)]
mod tests {
    use super::{dev_base, strip_base};

    #[test]
    fn dev_base_resolves_like_vite_serve() {
        for root in [None, Some(""), Some("./"), Some("/"), Some("./app/")] {
            assert_eq!(dev_base(root), None, "{root:?}");
        }
        assert_eq!(dev_base(Some("/app/")).as_deref(), Some("/app/"));
        assert_eq!(dev_base(Some("app/")).as_deref(), Some("/app/"));
        assert_eq!(
            dev_base(Some("https://cdn.example.com/app/")).as_deref(),
            Some("/app/")
        );
        assert_eq!(
            dev_base(Some("//cdn.example.com/app/")).as_deref(),
            Some("/app/")
        );
        assert_eq!(dev_base(Some("https://cdn.example.com")), None);
    }

    #[test]
    fn start_dev_urls_take_the_resolved_base() {
        use crate::start_dev_url_base;
        assert_eq!(start_dev_url_base("/app/"), "/app");
        assert_eq!(start_dev_url_base("app/"), "/app");
        assert_eq!(start_dev_url_base("https://cdn.example.com/app/"), "/app");
        for root in ["/", "./", "", "./app/"] {
            assert_eq!(start_dev_url_base(root), "", "{root:?}");
        }
    }

    #[test]
    fn strip_base_needs_a_segment_boundary() {
        assert_eq!(strip_base("/app/x.js", "/app/"), Some("/x.js"));
        assert_eq!(strip_base("/app/", "/app/"), Some("/"));
        assert_eq!(strip_base("/app", "/app/"), Some("/"));
        assert_eq!(strip_base("/apple.svg", "/app/"), None);
        assert_eq!(strip_base("/x.js", "/app"), None);
    }
}
