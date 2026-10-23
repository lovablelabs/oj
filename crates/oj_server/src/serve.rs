use super::*;

pub(crate) async fn serve_path(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let path = state
        .base
        .as_deref()
        .and_then(|b| uri.path().strip_prefix(b.trim_end_matches('/')))
        .unwrap_or_else(|| uri.path());
    let decoded = urldecode(path.trim_start_matches('/'));
    let rel = if decoded.is_empty() {
        "index.html"
    } else {
        decoded.as_str()
    };

    if let Some(resp) = serve_internal_route(&state, &headers, &uri).await {
        return resp;
    }
    let file = match request_file(&state, &headers, &uri, rel).await {
        Ok(file) => file,
        Err(resp) => return *resp,
    };
    if path_is_denied(&file, &state.root, &state.fs_deny) {
        return (StatusCode::FORBIDDEN, "oj: path denied by server.fs.deny").into_response();
    }
    serve_file(&state, &headers, &uri, &file).await
}

/// oj's own `/@...` routes (optimized deps, stubs, package bundles, virtual
/// modules, plugin resolve/id); `None` for an ordinary file path.
async fn serve_internal_route(
    state: &Arc<ServerState>,
    headers: &HeaderMap,
    uri: &Uri,
) -> Option<Response> {
    let path = uri.path();
    if let Some(name) = path.strip_prefix("/@oj-deps/") {
        if name.contains('/') || name.contains("..") {
            return Some((StatusCode::FORBIDDEN, "oj: bad optimized dep path").into_response());
        }
        state.optimized.ready().await;
        // A pending (registered, not yet bundled) dep's request waits for the
        // re-optimization to commit; a `?v=` the committed entry no longer
        // carries is Vite's outdated-request 504 (the page is mid-reload).
        let req_version = uri
            .query()
            .and_then(|q| q.split('&').find_map(|p| p.strip_prefix("v=")));
        if let optimize::DepServe::Outdated = state.optimized.await_dep(name, req_version).await {
            return Some(
                (
                    StatusCode::GATEWAY_TIMEOUT,
                    format!("oj: outdated optimized dep {name}"),
                )
                    .into_response(),
            );
        }
        return Some(
            match tokio::fs::read(state.optimized.dir().join(name)).await {
                Ok(bytes) => dep_response(
                    headers,
                    has_version_query(uri.query()),
                    state.optimized.link_entries(name, bytes),
                ),
                // Vite's optimizedDeps load: a versioned file that is gone
                // (a rerun or another session replaced the bundle) is an
                // outdated request the page reloads past; only an unversioned
                // miss is a dep the optimizer never wrote.
                Err(_) if req_version.is_some() => (
                    StatusCode::GATEWAY_TIMEOUT,
                    format!("oj: outdated optimized dep {name}"),
                )
                    .into_response(),
                Err(_) => (
                    StatusCode::NOT_FOUND,
                    format!("oj: no optimized dep {name}"),
                )
                    .into_response(),
            },
        );
    }
    if let Some(hex) = path.strip_prefix(OPTIONAL_PEER_PREFIX) {
        return Some(match optional_peer_dep_stub(hex) {
            Some(code) => js_response(code),
            None => (StatusCode::NOT_FOUND, "oj: bad optional peer id").into_response(),
        });
    }
    if path == "/@oj-empty" {
        return Some(js_response(
            "// oj: browser-externalized module (package maps it to false)\nexport default {};\nexport const __cjs_exports = {};\n",
        ));
    }
    if let Some(id) = path.strip_prefix("/@virtual/") {
        return Some(match state.virtual_modules.get(id) {
            Some(code) => js_response(code.clone()),
            None => (StatusCode::NOT_FOUND, format!("oj: no virtual module {id}")).into_response(),
        });
    }
    if let Some(hex) = path.strip_prefix("/@presolve/") {
        let id = hex_decode(hex).unwrap_or_default();
        return Some(serve_plugin_resolve(state, &id).await);
    }
    if let Some(seg) = path.strip_prefix("/@id/") {
        // `/@id/<id>` names the whole id, query included (Vite's unwrapId of
        // the url): `/@id/__x00__/page?html-proxy&index=0.js`. oj's own
        // `?importer=` is the one query that is not part of the id.
        let mut spec = decode_at_id(seg);
        let id_path = path.to_string();
        let importer = match uri.query() {
            Some(q) => match q.strip_prefix("importer=") {
                Some(importer) => decode_at_id(importer),
                None => {
                    spec = at_id_with_query(&spec, q);
                    String::new()
                }
            },
            None => String::new(),
        };
        if is_html_proxy(&spec) {
            return Some(serve_html_proxy(state, headers, &spec, &id_path).await);
        }
        return Some(serve_plugin_id(state, &spec, &importer).await);
    }
    None
}

/// A `/@id/` id with its url query, minus the HMR timestamp and the `import`
/// flag, which Vite's transform middleware strips before resolving.
fn at_id_with_query(spec: &str, query: &str) -> String {
    let joined = strip_hmr_timestamp(&format!("{spec}?{query}"));
    let Some((id, query)) = joined.split_once('?') else {
        return joined;
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|kv| *kv != "import" && *kv != "import=")
        .collect();
    if kept.is_empty() {
        id.to_string()
    } else {
        format!("{id}?{}", kept.join("&"))
    }
}

/// Vite's `isHtmlProxyRE` (`/[?&]html-proxy\b/`) over a url or id.
fn is_html_proxy(id: &str) -> bool {
    id.match_indices("html-proxy").any(|(at, m)| {
        at > 0
            && matches!(id.as_bytes()[at - 1], b'?' | b'&')
            && id[at + m.len()..]
                .chars()
                .next()
                .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'))
    })
}

/// An inline module the dev HTML transform lifted out of a page: `id` is
/// Vite's html-proxy id (`/index.html?html-proxy&index=0.js`, or
/// `\0/page?...` for a page with no file), `url` the request path. The
/// plugin host resolves and loads it (Vite's html-inline-proxy plugin), then
/// it compiles like any module, as `<page>.js` beside its page so its
/// relative imports resolve from the page's directory.
async fn serve_html_proxy(
    state: &Arc<ServerState>,
    headers: &HeaderMap,
    id: &str,
    url: &str,
) -> Response {
    // A page the native transform served keeps its modules here.
    let (id, source) = match state.html_proxy.load(id, &state.root) {
        Some(source) => (id.to_string(), source),
        None => {
            let Some(host) = &state.plugins else {
                return (
                    StatusCode::NOT_FOUND,
                    format!("oj: no matching HTML proxy module found from {id}"),
                )
                    .into_response();
            };
            let id = match host.resolve_id(id, "").await {
                Ok(Some(resolved)) => resolved,
                Ok(None) => id.to_string(),
                Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
            };
            match host.load(&id).await {
                Ok(Some(source)) => (id, source),
                Ok(None) => {
                    return (StatusCode::NOT_FOUND, format!("oj: no plugin loaded {id}"))
                        .into_response()
                }
                Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
            }
        }
    };
    let (page, query) = id.split_once('?').unwrap_or((&id, ""));
    let page = page.trim_start_matches('\0').trim_start_matches('/');
    let file = PathBuf::from(format!("{}.js", state.root.join(page).display()));
    serve_compiled_from(state, &file, url, Some(query), headers, Some(source)).await
}

/// The original request url (Vite's `req.originalUrl`): path and query.
fn original_url(uri: &Uri) -> String {
    uri.path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| uri.path().to_string())
}

/// The file a request names: a gated `/@fs` path, or `rel` under the root or
/// publicDir. A miss is answered here (plugin middleware, plugin load, html
/// fallback, 404).
async fn request_file(
    state: &Arc<ServerState>,
    headers: &HeaderMap,
    uri: &Uri,
    rel: &str,
) -> Result<PathBuf, Box<Response>> {
    if let Some(abs) = uri.path().strip_prefix("/@fs") {
        let Some(real) = fs_gate(state, &PathBuf::from(urldecode(abs))) else {
            return Err(Box::new(
                (StatusCode::FORBIDDEN, "oj: /@fs path not allow-listed").into_response(),
            ));
        };
        // Served from outside the root (a linked workspace package):
        // the root watch does not cover it.
        ensure_watched_file(state, &real);
        return Ok(real);
    }
    match locate(&state.root, state.public_dir.as_deref(), rel) {
        Some(file) => Ok(file),
        None => Err(Box::new(serve_missing(state, headers, uri, rel).await)),
    }
}

async fn serve_missing(
    state: &Arc<ServerState>,
    headers: &HeaderMap,
    uri: &Uri,
    rel: &str,
) -> Response {
    if let Some(resp) =
        forward_to_plugin_middleware(state, &Method::GET, uri, headers, Vec::new()).await
    {
        return resp;
    }
    if let Some(resp) = serve_plugin_load_fallback(state, uri).await {
        return resp;
    }
    // Vite's htmlFallback: `/dir/` serves dir/index.html, `/page` serves
    // page.html; only appType `spa` then falls back to the root index.html.
    if state.app_type != "custom" && accepts_html_fallback(headers) {
        if let Some(page) = html_fallback_candidate(rel)
            .and_then(|c| locate(&state.root, state.public_dir.as_deref(), &c))
        {
            return match tokio::fs::read(&page).await {
                Ok(bytes) => {
                    serve_html(state, bytes, &format!("/{rel}"), &original_url(uri), &page).await
                }
                Err(_) => {
                    (StatusCode::NOT_FOUND, format!("oj: no such file: /{rel}")).into_response()
                }
            };
        }
    }
    if state.app_type == "spa" && is_spa_navigation(rel, headers) {
        return serve_index_html(state, &original_url(uri)).await;
    }
    (StatusCode::NOT_FOUND, format!("oj: no such file: /{rel}")).into_response()
}

/// Dispatches a located file by query and extension.
async fn serve_file(
    state: &Arc<ServerState>,
    headers: &HeaderMap,
    uri: &Uri,
    file: &Path,
) -> Response {
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
    let query = uri.query();
    // An inline module the dev HTML transform lifted out of this page
    // (`/index.html?html-proxy&index=0.js`): Vite's html-inline-proxy plugin
    // resolves and loads it, then it compiles like any module.
    if ext == "html" {
        if let Some(q) = query.filter(|q| is_html_proxy(&format!("?{q}"))) {
            let url = url_of(&state.root, file);
            return serve_html_proxy(state, headers, &at_id_with_query(&url, q), &url).await;
        }
    }
    // A publicDir file is served verbatim, never compiled (Vite's servePublicMiddleware
    // runs before transform); only an explicit asset query still yields a module.
    let in_public = state
        .public_dir
        .as_deref()
        .is_some_and(|p| file.starts_with(p));
    if let Some(kind) = query_asset_kind(query) {
        return serve_asset_query(state, file, ext, kind, in_public).await;
    }
    if is_style_ext(ext) && !in_public {
        return serve_style(state, headers, file, query).await;
    }
    if in_public {
        return serve_static_file(file, ext).await;
    }
    if COMPILABLE.contains(&ext) || ext == "json" {
        let url = url_of(&state.root, file);
        return serve_compiled(state, file, &url, query, headers).await;
    }
    if ext == "svg" && has_query_flag(query, "react") {
        let url = format!("{}?react", url_of(&state.root, file));
        return serve_compiled(state, file, &url, None, headers).await;
    }
    // A plain `.svg` import goes through the compile path so a configured svgr can
    // componentize it; unmatched svgs fall back to a URL asset there.
    if ext.eq_ignore_ascii_case("svg") && !wants_raw_resource(headers) {
        let url = url_of(&state.root, file);
        if state.plugins_have_transform {
            return serve_compiled(state, file, &url, query, headers).await;
        }
        // No plugin can componentize it: a module import of an svg (relative,
        // aliased or root-absolute) is its URL, like any other asset in Vite.
        return js_module_response(asset_module(file, &url, "url").await);
    }
    if is_importable_asset_ext(ext) && wants_module_import(headers, query) {
        let url = url_of(&state.root, file);
        return js_module_response(asset_module(file, &url, "url").await);
    }
    serve_raw_file(state, file, ext).await
}

/// `?url`, `?raw`, `?inline`, `?worker`...: the file as a JS module.
async fn serve_asset_query(
    state: &Arc<ServerState>,
    file: &Path,
    ext: &str,
    kind: &str,
    in_public: bool,
) -> Response {
    // A public file's url is its path under the public dir (`/logo.svg`,
    // Vite's checkPublicFile), not `/public/logo.svg`.
    let url = match state.public_dir.as_deref().filter(|_| in_public) {
        Some(p) => format!("/{}", file.strip_prefix(p).unwrap_or(file).display()),
        None => url_of(&state.root, file),
    };
    let js = if kind == "inline" && is_style_ext(ext) {
        inline_css_module(state, file, &url).await
    } else {
        asset_module(file, &url, kind).await
    };
    js_module_response(js)
}

async fn serve_style(
    state: &Arc<ServerState>,
    headers: &HeaderMap,
    file: &Path,
    query: Option<&str>,
) -> Response {
    let url = url_of(&state.root, file);
    if has_query_flag(query, "direct")
        || (wants_raw_resource(headers) && !has_query_flag(query, "import"))
    {
        // `<link href>`, fetch() of a `?url` stylesheet, or `?direct`: compiled
        // CSS text, not the preprocessor source and not the JS wrapper.
        return serve_css_direct(state, file, &url).await;
    }
    serve_css_wrapper(state, file, &url).await
}

/// A file with no module treatment: html through the html pipeline, a
/// Tailwind entry compiled, anything else as-is.
async fn serve_raw_file(state: &Arc<ServerState>, file: &Path, ext: &str) -> Response {
    let bytes = match tokio::fs::read(file).await {
        Ok(bytes) => bytes,
        Err(err) => return read_error(err),
    };
    match ext {
        "html" => {
            let url = url_of(&state.root, file);
            serve_html(state, bytes, &url, &url, file).await
        }
        "css" => {
            let source = String::from_utf8_lossy(&bytes).into_owned();
            if !is_tailwind_css(&source) {
                return ([(header::CONTENT_TYPE, "text/css")], source).into_response();
            }
            let url = url_of(&state.root, file);
            match compile_tailwind(state, file, &url, &source).await {
                Ok((css, _)) => (
                    [
                        (header::CONTENT_TYPE, "text/css"),
                        (header::CACHE_CONTROL, "no-cache"),
                    ],
                    css,
                )
                    .into_response(),
                Err(err) => {
                    send_error(state, &err);
                    (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {err}")).into_response()
                }
            }
        }
        _ => bytes_response(bytes, ext),
    }
}

/// The file's bytes as-is with its content type (publicDir files).
pub(crate) async fn serve_static_file(file: &Path, ext: &str) -> Response {
    match tokio::fs::read(file).await {
        Ok(bytes) => bytes_response(bytes, ext),
        Err(err) => read_error(err),
    }
}

fn bytes_response(bytes: Vec<u8>, ext: &str) -> Response {
    let mut response = Response::new(Body::from(bytes));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type(ext).parse().unwrap());
    response
}

fn read_error(err: std::io::Error) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("oj: read error: {err}"),
    )
        .into_response()
}

/// A no-cache JavaScript response.
fn js_response(code: impl IntoResponse) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        code,
    )
        .into_response()
}

fn js_module_response(js: Result<String, String>) -> Response {
    match js {
        Ok(js) => js_response(js),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {e}")).into_response(),
    }
}

/// Whether the query has the bare `flag` key (`?direct`, `?import`, `?react`).
fn has_query_flag(query: Option<&str>, flag: &str) -> bool {
    query.is_some_and(|q| q.split('&').any(|kv| kv == flag))
}

pub(crate) async fn serve_compiled(
    state: &Arc<ServerState>,
    file: &Path,
    url: &str,
    query: Option<&str>,
    headers: &HeaderMap,
) -> Response {
    serve_compiled_from(state, file, url, query, headers, None).await
}

/// [`serve_compiled`] for a module whose source a plugin `load` produced
/// (see [`ensure_module_from`]).
async fn serve_compiled_from(
    state: &Arc<ServerState>,
    file: &Path,
    url: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    preloaded: Option<String>,
) -> Response {
    // Key and transform per full url incl. query (the same file yields per-query
    // variants), but the HMR `t=` buster is stripped: not part of module identity.
    let base_url = url;
    let url_with_query = match query {
        Some(q) => strip_hmr_timestamp(&format!("{url}?{q}")),
        None => url.to_string(),
    };
    let url = url_with_query.as_str();
    let (key, module) = match ensure_module_from(state, file, url, preloaded).await {
        Ok(pair) => pair,
        Err(err) => {
            send_error(state, &err);
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {err}")).into_response();
        }
    };

    let etag = format!("\"{key}\"");
    if query.is_none() && crate::etag::not_modified(headers, &etag) {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag),
                (header::CACHE_CONTROL, "no-cache".to_string()),
            ],
        )
            .into_response();
    }

    let mut body = if module.kind == "svelte" {
        format!("{}{}", svelte_hot_glue(url), module.code)
    } else {
        let ctx_predefined = module.hot.is_some();
        let mut body = if ctx_predefined {
            // The module reads import.meta.hot itself: define the context before its
            // body runs; the refresh glue below REUSES it (a re-declare is a SyntaxError).
            let full = match query {
                Some(q) if !q.is_empty() => format!("{base_url}?{q}"),
                _ => base_url.to_string(),
            };
            format!(
                "{}{}",
                svelte_hot_glue(&strip_hmr_timestamp(&full)),
                module.code
            )
        } else {
            module.code.clone()
        };
        body.push_str(&hot_glue(
            base_url,
            query,
            module.is_boundary,
            ctx_predefined,
        ));
        body
    };
    if let Some(map) = &module.map_json {
        // Vite builds the data URL per send (genSourceMapUrl); caches retain
        // the raw JSON, 25% smaller than base64.
        body.push_str("\n//# sourceMappingURL=");
        body.push_str(&oj_compiler::map_json_to_data_url(map));
        body.push('\n');
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
