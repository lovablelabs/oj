use super::*;

pub(crate) async fn serve_path(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let path = state
        .base
        .as_deref()
        .and_then(|b| strip_base(uri.path(), b))
        .unwrap_or_else(|| uri.path());
    let decoded = urldecode(path.trim_start_matches('/'));
    let rel = if decoded.is_empty() {
        "index.html"
    } else {
        decoded.as_str()
    };

    let internal = match (path.len() != uri.path().len(), uri.query()) {
        (false, _) => None,
        (true, Some(q)) => format!("{path}?{q}").parse::<Uri>().ok(),
        (true, None) => path.parse::<Uri>().ok(),
    };
    if let Some(resp) =
        serve_internal_route(&state, &headers, internal.as_ref().unwrap_or(&uri)).await
    {
        return resp;
    }
    // A pre-bundle requested by its own path (`/@fs/…` or under the root) is
    // the bundle's URL, which waits for the bundle like any dep request.
    let named = match rel.strip_prefix("@fs") {
        Some(abs) if abs.starts_with('/') => PathBuf::from(abs),
        _ => state.root.join(rel),
    };
    if let Some(url) = optimized_url_of(&state, &named.to_string_lossy()) {
        return Redirect::temporary(&url).into_response();
    }
    let file = match request_file(&state, &headers, &uri, rel).await {
        Ok(file) => file,
        Err(resp) => return *resp,
    };
    if path_is_denied(&file, &state.root, &state.fs_deny) {
        return (StatusCode::FORBIDDEN, "oj: path denied by server.fs.deny").into_response();
    }
    serve_file(&state, &headers, &uri, &file, rel).await
}

/// oj's own `/@...` routes (optimized deps, stubs, package bundles, virtual
/// modules, plugin resolve/id); `None` for an ordinary file path.
async fn serve_internal_route(
    state: &Arc<ServerState>,
    headers: &HeaderMap,
    uri: &Uri,
) -> Option<Response> {
    // `base` is already stripped (base_middleware); the StyleX runtime's
    // unprefixed fetch of its sheet passes through it as is.
    let path = uri.path();
    if let Some(resp) = crate::stylex::serve_route(state, path) {
        return Some(resp);
    }
    if path.starts_with(state.vite_deps_prefix.as_str()) {
        return Some(serve_vite_dep_url(state, headers, path).await);
    }
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
    if path == "/@oj/map" {
        return Some(serve_source_map(state, uri.query()).await);
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
        if let Some(source) = crate::stylex::virtual_module(state, &spec) {
            // Compiled like any module: the runtime reads `import.meta.hot`.
            let file = state.root.join("__oj_stylex_virtual.js");
            return Some(
                serve_compiled_from(
                    state,
                    &file,
                    &id_path,
                    None,
                    headers,
                    Some(source.to_string()),
                )
                .await,
            );
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
    match forward_to_plugin_middleware(state, &Method::GET, uri, headers, Vec::new()).await {
        Forwarded::Answered(resp) => return resp,
        // A middleware rewrote `req.url` and passed the request on: Vite's
        // static and HTML middlewares, which run after a plugin's, serve the
        // url it left (an MPA plugin maps `/page` to `/page/index.html` this
        // way), so the rewritten url is served as if asked for, without
        // going through the middleware again.
        Forwarded::FellThrough {
            rewritten: Some(target),
        } => return serve_rewritten(state, headers, &target).await,
        Forwarded::FellThrough { rewritten: None } => {}
    }
    if let Some(resp) = serve_plugin_load_fallback(state, uri).await {
        return resp;
    }
    serve_fallback(state, headers, uri, rel).await
}

/// The url a plugin middleware rewrote a request to: the file it names, or
/// the HTML fallback for it, as Vite's later middlewares would serve it.
async fn serve_rewritten(state: &Arc<ServerState>, headers: &HeaderMap, uri: &Uri) -> Response {
    let path = state
        .base
        .as_deref()
        .and_then(|b| strip_base(uri.path(), b))
        .unwrap_or_else(|| uri.path());
    let decoded = urldecode(path.trim_start_matches('/'));
    let rel = if decoded.is_empty() {
        "index.html"
    } else {
        decoded.as_str()
    };
    if let Some(file) = locate(&state.root, state.public_dir.as_deref(), rel) {
        if path_is_denied(&file, &state.root, &state.fs_deny) {
            return (StatusCode::FORBIDDEN, "oj: path denied by server.fs.deny").into_response();
        }
        return serve_file(state, headers, uri, &file, rel).await;
    }
    serve_fallback(state, headers, uri, rel).await
}

/// Vite's htmlFallback: `/dir/` serves dir/index.html, `/page` serves
/// page.html; only appType `spa` then falls back to the root index.html.
async fn serve_fallback(
    state: &Arc<ServerState>,
    headers: &HeaderMap,
    uri: &Uri,
    rel: &str,
) -> Response {
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
    rel: &str,
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
    // Vite's checkPublicFile: public when the request path names a file
    // under the public dir, so `/public/logo.png` is not `/logo.png`.
    let public_url = state
        .public_dir
        .as_deref()
        .is_some_and(|p| in_public && p.join(rel) == file);
    // `?inline` on a file Vite's asset plugin does not load (not an asset or
    // a stylesheet) is the plain module, as vite:asset's load filter has it.
    let asset_query = query_asset_kind(query)
        .filter(|kind| *kind != "inline" || is_style_ext(ext) || is_asset_path(&state.root, file));
    if let Some(kind) = asset_query {
        return serve_asset_query(state, file, ext, kind, query, public_url).await;
    }
    if is_style_ext(ext) && !in_public {
        return vary_on_fetch_dest(serve_style(state, headers, file, query).await);
    }
    if in_public {
        // A public asset a module imports by its url: Vite's
        // servePublicMiddleware steps aside for an import request (`?import`,
        // isImportRequest) and vite:asset's load answers it with the url
        // (checkPublicFile, fileToDevUrl), so the page gets a module where
        // the bare url is the image. An import the browser marks
        // `Sec-Fetch-Dest: script` is the same request, as it is for a bare
        // asset under the root (serve_bare_asset). Only an asset answers it;
        // a public script or stylesheet is the file, whatever asks.
        let asset = public_url && is_asset_path(&state.root, file);
        if asset && wants_module_import(headers, query) {
            return vary_on_fetch_dest(
                serve_asset_query(state, file, ext, "url", query, true).await,
            );
        }
        // Vite's servePublicMiddleware: the bytes, with sirv's validators
        // and byte ranges.
        let response = match tokio::fs::metadata(file).await {
            Ok(meta) => crate::public_file_response(file, &meta, &Method::GET, headers).await,
            Err(err) => read_error(err),
        };
        return if asset {
            vary_on_fetch_dest(response)
        } else {
            response
        };
    }
    if COMPILABLE.contains(&ext) || ext == "json" {
        let url = url_of(&state.root, file);
        return serve_compiled(state, file, &url, query, headers).await;
    }
    if ext == "svg" && has_query_flag(query, "react") {
        let url = format!("{}?react", url_of(&state.root, file));
        return serve_compiled(state, file, &url, None, headers).await;
    }
    if ext.eq_ignore_ascii_case("svg") || is_importable_asset(&state.root, &file.to_string_lossy())
    {
        return vary_on_fetch_dest(serve_bare_asset(state, headers, file, ext, query).await);
    }
    serve_raw_file(state, file, ext).await
}

/// A bare asset url is the file for an `<img src>` and its URL module for an
/// import the browser marks `Sec-Fetch-Dest: script`.
async fn serve_bare_asset(
    state: &Arc<ServerState>,
    headers: &HeaderMap,
    file: &Path,
    ext: &str,
    query: Option<&str>,
) -> Response {
    // A plain `.svg` import goes through the compile path so a configured svgr can
    // componentize it; unmatched svgs fall back to a URL asset there.
    if ext.eq_ignore_ascii_case("svg") && !wants_raw_resource(headers) {
        let url = url_of(&state.root, file);
        if state.plugins_have_transform {
            return serve_compiled(state, file, &url, query, headers).await;
        }
        // No plugin can componentize it: a module import of an svg (relative,
        // aliased or root-absolute) is its URL, like any other asset in Vite.
        return js_module_response(
            asset_module(file, &url, "url", &state.asset_urls, "client").await,
        );
    }
    if is_importable_asset(&state.root, &file.to_string_lossy())
        && wants_module_import(headers, query)
    {
        let url = url_of(&state.root, file);
        return js_module_response(
            asset_module(file, &url, "url", &state.asset_urls, "client").await,
        );
    }
    serve_raw_file(state, file, ext).await
}

/// `?url`, `?raw`, `?inline`, `?worker`...: the file as a JS module.
async fn serve_asset_query(
    state: &Arc<ServerState>,
    file: &Path,
    ext: &str,
    kind: &str,
    query: Option<&str>,
    in_public: bool,
) -> Response {
    // A public file's url is its path under the public dir (`/logo.svg`,
    // Vite's checkPublicFile), not `/public/logo.svg`.
    let url = match state.public_dir.as_deref().filter(|_| in_public) {
        Some(p) => format!("/{}", file.strip_prefix(p).unwrap_or(file).display()),
        None => url_of(&state.root, file),
    };
    let url = match query {
        Some(q) => format!("{url}?{q}"),
        None => url,
    };
    let js = if kind == "inline" && is_style_ext(ext) {
        inline_css_module(state, file, &url).await
    } else {
        asset_module(file, &url, kind, &state.asset_urls, "client").await
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

/// One url, two bodies picked by the request's `Sec-Fetch-Dest`: a cache must
/// key on it or it hands a module import the image bytes (or the reverse).
fn vary_on_fetch_dest(mut response: Response) -> Response {
    response.headers_mut().append(
        header::VARY,
        header::HeaderValue::from_static("sec-fetch-dest"),
    );
    response
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
        format!("{}{}", svelte_hot_glue(url, &state.oj_paths), module.code)
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
                svelte_hot_glue(&strip_hmr_timestamp(&full), &state.oj_paths),
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
            &state.oj_paths,
        ));
        body
    };
    // A `?worker_file&type=<type>` module gets vite:worker's env prelude
    // (its map moves down by the prelude's lines, see [`serve_source_map`]).
    if let Some(prelude) = worker_file_prelude(
        url,
        &crate::start_dev_url_base(state.base.as_deref().unwrap_or("/")),
        &state.vite_env_js,
    ) {
        body.insert_str(0, &prelude);
    }
    if module.map_json.is_some() {
        // Vite inlines the map as a data URL, which triples a module's bytes
        // for a map the browser reads only with DevTools open. oj names it
        // beside the module instead, by the module's url and compile key
        // (see [`serve_source_map`]): fetched when wanted, cached forever.
        body.push_str("\n//# sourceMappingURL=");
        body.push_str(&source_map_url(
            &state.oj_paths,
            url,
            &key,
            &state.boot_stamp,
        ));
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

/// Where the map of the module `url` compiled under `key` is served
/// (`/@oj/map?u=<url>&k=<key>` under the base). The browser resolves a
/// `sourceMappingURL` against the module's URL, so the path is absolute.
pub(crate) fn source_map_url(paths: &OjPublicPaths, url: &str, key: &str, boot: &str) -> String {
    format!(
        "{}?u={}&k={key}&b={boot}",
        paths.source_map,
        urlencode_component(url)
    )
}

/// `/@oj/map?u=<module url>&k=<compile key>`: the source map of the module
/// `u` as compiled under key `k`, which the module's `sourceMappingURL` names
/// (see [`serve_compiled_from`]). The key hashes the module's source and
/// every setting of its compile, so the map is immutable under this URL. A
/// module the memory cache has let go of is compiled again through its own
/// request first; a `k` no compile of `u` produces any more (the file
/// changed) is a map that no longer exists.
async fn serve_source_map(state: &Arc<ServerState>, query: Option<&str>) -> Response {
    let (mut url, mut key) = (None, None);
    for kv in query.unwrap_or("").split('&') {
        if let Some(u) = kv.strip_prefix("u=") {
            url = Some(urldecode(u));
        } else if let Some(k) = kv.strip_prefix("k=") {
            key = Some(k.to_string());
        }
        // `b=`, the boot stamp, only tells one boot's urls from another's.
    }
    let (Some(url), Some(key)) = (url, key) else {
        return (
            StatusCode::BAD_REQUEST,
            "oj: a source map is named by its module's url (u) and compile key (k)",
        )
            .into_response();
    };
    let mut module = memory_get(state, &url, &key);
    if module.is_none() {
        // Evicted: the module's own request compiles it again, whichever
        // route serves it (a file's, or the plugin and virtual fallbacks a
        // missing file goes to, whose answer is not wanted here, only the
        // entry it leaves in memory).
        let (path, rest) = url
            .split_once('?')
            .map_or((url.as_str(), ""), |(p, q)| (p, q));
        let encoded = match rest {
            "" => urlencode_component(path),
            q => format!("{}?{q}", urlencode_component(path)),
        };
        if let Ok(uri) = encoded.parse::<Uri>() {
            let rel = urldecode(uri.path().trim_start_matches('/'));
            let headers = HeaderMap::new();
            if let Ok(file) = request_file(state, &headers, &uri, &rel).await {
                if !path_is_denied(&file, &state.root, &state.fs_deny) {
                    let _ = serve_file(state, &headers, &uri, &file, &rel).await;
                }
            }
            module = memory_get(state, &url, &key);
        }
    }
    let Some(map) = module.as_ref().and_then(|m| m.map_json.clone()) else {
        return (
            StatusCode::NOT_FOUND,
            format!("oj: no source map for {url} at {key}"),
        )
            .into_response();
    };
    let module = module.expect("a map comes from a module");
    // What [`serve_compiled_from`] puts before the code: the hot-context
    // glue of a module that reads import.meta.hot (or a svelte module),
    // then a worker's env prelude. The map moves down by their lines.
    let mut shift = 0;
    if module.kind == "svelte" || module.hot.is_some() {
        shift += crate::hmr::svelte_hot_glue(&url, &state.oj_paths)
            .matches('\n')
            .count();
    }
    if let Some(prelude) = worker_file_prelude(
        &url,
        &crate::start_dev_url_base(state.base.as_deref().unwrap_or("/")),
        &state.vite_env_js,
    ) {
        shift += prelude.matches('\n').count();
    }
    let map = if shift > 0 {
        shift_map_lines(&map, shift)
    } else {
        map
    };
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "max-age=31536000,immutable"),
        ],
        map,
    )
        .into_response()
}

/// A source map whose generated code moved down `lines` lines.
pub(crate) fn shift_map_lines(map_json: &str, lines: usize) -> String {
    let Ok(mut map) = serde_json::from_str::<serde_json::Value>(map_json) else {
        return map_json.to_string();
    };
    if let Some(serde_json::Value::String(mappings)) = map.get_mut("mappings") {
        mappings.insert_str(0, &";".repeat(lines));
    }
    map.to_string()
}
