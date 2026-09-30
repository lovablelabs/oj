use super::*;

/// `url`: the page's request path (Vite's ctx.path); `file`: the html on disk
/// (ctx.filename). A throwing transformIndexHtml fails the request with the
/// plugin error (Vite's indexHtml middleware lets it reach the error
/// middleware) instead of serving the untransformed page.
pub(crate) async fn serve_html(state: &ServerState, bytes: Vec<u8>, url: &str, file: &Path) -> Response {
    let mut raw = String::from_utf8_lossy(&bytes).into_owned();
    // %VITE_*% / import.meta.env substitution (Vite's htmlEnvHook), a pre-hook
    // before any plugin transformIndexHtml.
    raw = oj_env::replace_html_env(&raw, &state.html_env);
    if let Some(host) = &state.plugins {
        let ctx = serde_json::json!({
            "path": url,
            "filename": file.display().to_string(),
            "originalUrl": url,
        })
        .to_string();
        match host.transform_index_html(&raw, &ctx).await {
            Ok(out) => raw = out,
            Err(e) => {
                eprintln!("oj: transformIndexHtml failed for {url}: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    format!("oj: transformIndexHtml failed for {url}\n{e}"),
                )
                    .into_response();
            }
        }
    }
    let mut html = inject_module_preloads(inject_dev_scripts(raw), state);
    if let Some(nonce) = &state.csp_nonce {
        html = inject_csp_nonce(&html, nonce);
    }
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

/// Vite's `html.cspNonce` (injectNonceAttributeTagHook + injectCspNonceMetaTagHook):
/// every `<script>`, `<style>` and stylesheet/modulepreload/preload `<link>`
/// without a `nonce` gets `nonce="<nonce>"`, and `<head>` gets a
/// `<meta property="csp-nonce" nonce="<nonce>">` the runtime reads back.
pub fn inject_csp_nonce(html: &str, nonce: &str) -> String {
    let mut out = String::with_capacity(html.len() + 256);
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        let (before, at) = rest.split_at(lt);
        out.push_str(before);
        let name_end = at[1..]
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '-')
            .map(|i| i + 1)
            .unwrap_or(at.len());
        let name = at[1..name_end].to_ascii_lowercase();
        let Some(gt) = at.find('>') else {
            out.push_str(at);
            return out;
        };
        let tag = &at[..gt];
        let wants = match name.as_str() {
            "script" | "style" => true,
            "link" => html_tag_attr(tag, "rel").is_some_and(|rel| {
                rel.split_whitespace().any(|r| {
                    matches!(
                        r.to_ascii_lowercase().as_str(),
                        "stylesheet" | "modulepreload" | "preload"
                    )
                })
            }),
            _ => false,
        };
        if wants && html_tag_attr(tag, "nonce").is_none() {
            let body = tag.trim_end_matches('/').trim_end();
            let self_closing = tag.trim_end().ends_with('/');
            out.push_str(body);
            out.push_str(&format!(" nonce=\"{nonce}\""));
            out.push_str(if self_closing { " />" } else { ">" });
        } else {
            out.push_str(&at[..=gt]);
        }
        rest = &at[gt + 1..];
        // Skip raw text content so a `<` inside a script or style body is not
        // read as a tag.
        if matches!(name.as_str(), "script" | "style") && !tag.trim_end().ends_with('/') {
            let close = format!("</{name}");
            if let Some(end) = rest.to_ascii_lowercase().find(&close) {
                out.push_str(&rest[..end]);
                rest = &rest[end..];
            }
        }
    }
    out.push_str(rest);
    if !out.contains("property=\"csp-nonce\"") {
        let meta = format!("<meta property=\"csp-nonce\" nonce=\"{nonce}\">");
        out = match out.find("<head>") {
            Some(i) => {
                let at = i + "<head>".len();
                format!("{}\n{meta}{}", &out[..at], &out[at..])
            }
            None => format!("{meta}\n{out}"),
        };
    }
    out
}

pub(crate) async fn serve_index_html(state: &ServerState) -> Response {
    match tokio::fs::read(state.root.join("index.html")).await {
        Ok(bytes) => serve_html(state, bytes, "/index.html", &state.root.join("index.html")).await,
        Err(_) => (StatusCode::NOT_FOUND, "oj: index.html not found").into_response(),
    }
}

/// The html file Vite's htmlFallback middleware rewrites an unmatched path to:
/// a trailing slash asks for that directory's `index.html`, anything else for
/// the `.html` sibling. An explicit `.html` request is left alone (it either
/// exists, and was found already, or is a 404).
pub(crate) fn html_fallback_candidate(rel: &str) -> Option<String> {
    if rel.is_empty() || rel.ends_with(".html") {
        return None;
    }
    if rel.ends_with('/') {
        Some(format!("{rel}index.html"))
    } else {
        Some(format!("{rel}.html"))
    }
}

/// Vite's htmlFallback only acts on requests that accept html: no `Accept`, an
/// empty one, or one naming `text/html` or `*/*`.
pub(crate) fn accepts_html_fallback(headers: &HeaderMap) -> bool {
    match headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()) {
        None => true,
        Some(a) => a.is_empty() || a.contains("text/html") || a.contains("*/*"),
    }
}

pub(crate) fn is_spa_navigation(rel: &str, headers: &HeaderMap) -> bool {
    if rel.starts_with('@')
        || rel.starts_with("__")
        || rel.starts_with("src/")
        || rel.starts_with("node_modules/")
    {
        return false;
    }
    let last = rel.rsplit('/').next().unwrap_or("");
    let no_extension = !last.contains('.');
    let accepts_html = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("text/html"));
    no_extension || accepts_html
}

pub(crate) async fn serve_fallback(
    State(state): State<Arc<ServerState>>,
    req: axum::extract::Request,
) -> Response {
    // A WebSocket upgrade nothing above claimed (oj's own ws endpoints and
    // `ws: true` proxies run earlier): plugin `upgrade` listeners get it.
    if is_websocket_upgrade(req.headers()) && vite_ws_subprotocol(req.headers()).is_none() {
        if let Some(port) = state.plugin_serve.mw_port() {
            return relay_upgrade_to_plugin_middleware(state, req, port).await;
        }
    }
    if req.method() == Method::GET {
        let headers = req.headers().clone();
        let uri = req.uri().clone();
        return serve_path(State(state), headers, uri).await;
    }
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let body = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .unwrap_or_default()
        .to_vec();
    forward_to_plugin_middleware(&state, &method, &uri, &headers, body)
        .await
        .unwrap_or_else(|| (StatusCode::NOT_FOUND, "oj: not found").into_response())
}

/// The href a graph module is preloaded under: the exact URL its importer names,
/// so the preload and the import share one cache entry. A stylesheet is the
/// `?import` module; an optimized dep or package bundle carries the same
/// `?v=<version>` its import URLs do (the graph keys them without the query).
pub(crate) fn preload_href(path: &str, version: &str) -> String {
    if is_style_url(path) {
        format!("{path}?import")
    } else if !version.is_empty()
        && (path.starts_with("/@oj-deps/") || path.starts_with(pkg_bundle::PKG_PREFIX))
    {
        format!("{path}?v={version}")
    } else {
        path.to_string()
    }
}

pub(crate) fn inject_module_preloads(html: String, state: &ServerState) -> String {
    let paths: Vec<String> = if *state.crawl_done.borrow() {
        state
            .graph
            .lock()
            .unwrap()
            .module_paths()
            .iter()
            .map(|p| p.display().to_string())
            .collect()
    } else {
        state.preload_snapshot.clone()
    };
    if paths.is_empty() {
        return html;
    }
    let version = state.optimized.version();
    let links: String = paths
        .iter()
        .map(|p| {
            format!(
                "<link rel=\"modulepreload\" href=\"{}\" />\n",
                preload_href(p, version)
            )
        })
        .collect();
    match html.find("</head>") {
        Some(idx) => format!("{}{links}{}", &html[..idx], &html[idx..]),
        None => format!("{html}\n{links}"),
    }
}

pub fn html_entry_src(src: &str) -> Option<String> {
    let s = src.trim();
    if s.is_empty()
        || s.starts_with("http://")
        || s.starts_with("https://")
        || s.starts_with("//")
        || s.starts_with("data:")
    {
        return None;
    }
    let s = s.strip_prefix("./").unwrap_or(s);
    Some(if s.starts_with('/') {
        s.to_string()
    } else {
        format!("/{s}")
    })
}

pub(crate) fn html_entries(root: &Path) -> Vec<String> {
    let Ok(html) = std::fs::read_to_string(root.join("index.html")) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for tag_start in html.match_indices("<script").map(|(i, _)| i) {
        let Some(tag_end) = html[tag_start..].find('>') else {
            continue;
        };
        let tag = &html[tag_start..tag_start + tag_end];
        if !html_tag_attr(tag, "type").is_some_and(|t| t.eq_ignore_ascii_case("module")) {
            continue;
        }
        if let Some(entry) = html_tag_attr(tag, "src").and_then(html_entry_src) {
            entries.push(entry);
        }
    }
    entries
}

/// The value of attribute `name` in an opening tag (`<script type=...`),
/// whether double-quoted, single-quoted or unquoted, with optional spaces
/// around `=`; attribute names match case-insensitively and as whole words
/// (`data-src` is not `src`).
pub(crate) fn html_tag_attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let bytes = tag.as_bytes();
    let mut i = tag.find(char::is_whitespace)?;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'='
            && bytes[i] != b'/'
        {
            i += 1;
        }
        if i == start {
            i += 1;
            continue;
        }
        let attr = &tag[start..i];
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        let (vs, ve) = if matches!(bytes[i], b'"' | b'\'') {
            let q = bytes[i];
            let s = i + 1;
            i = s;
            while i < bytes.len() && bytes[i] != q {
                i += 1;
            }
            let e = i;
            i += usize::from(i < bytes.len());
            (s, e)
        } else {
            let s = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            (s, i)
        };
        if attr.eq_ignore_ascii_case(name) {
            return Some(&tag[vs..ve]);
        }
    }
    None
}
