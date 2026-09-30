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

    if let Some(name) = uri.path().strip_prefix("/@oj-deps/") {
        if name.contains('/') || name.contains("..") {
            return (StatusCode::FORBIDDEN, "oj: bad optimized dep path").into_response();
        }
        state.optimized.ready().await;
        return match tokio::fs::read(state.optimized.dir().join(name)).await {
            Ok(bytes) => dep_response(&headers, has_version_query(uri.query()), bytes),
            Err(_) => (
                StatusCode::NOT_FOUND,
                format!("oj: no optimized dep {name}"),
            )
                .into_response(),
        };
    }

    if let Some(hex) = uri.path().strip_prefix(OPTIONAL_PEER_PREFIX) {
        return match optional_peer_dep_stub(hex) {
            Some(code) => (
                [
                    (header::CONTENT_TYPE, "text/javascript"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                code,
            )
                .into_response(),
            None => (StatusCode::NOT_FOUND, "oj: bad optional peer id").into_response(),
        };
    }

    if uri.path() == "/@oj-empty" {
        return (
            [
                (header::CONTENT_TYPE, "text/javascript"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            "// oj: browser-externalized module (package maps it to false)\nexport default {};\nexport const __cjs_exports = {};\n",
        )
            .into_response();
    }

    if uri.path().starts_with(pkg_bundle::PKG_PREFIX) {
        return serve_pkg_bundle(&state, uri.path(), has_version_query(uri.query())).await;
    }

    if let Some(id) = uri.path().strip_prefix("/@virtual/") {
        return match state.virtual_modules.get(id) {
            Some(code) => (
                [
                    (header::CONTENT_TYPE, "text/javascript"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                code.clone(),
            )
                .into_response(),
            None => (StatusCode::NOT_FOUND, format!("oj: no virtual module {id}")).into_response(),
        };
    }

    if let Some(hex) = uri.path().strip_prefix("/@presolve/") {
        let id = hex_decode(hex).unwrap_or_default();
        return serve_plugin_resolve(&state, &id).await;
    }

    if let Some(seg) = uri.path().strip_prefix("/@id/") {
        let spec = decode_at_id(seg);
        let importer = uri
            .query()
            .and_then(|q| q.strip_prefix("importer="))
            .map(decode_at_id)
            .unwrap_or_default();
        return serve_plugin_id(&state, &spec, &importer).await;
    }

    let file = if let Some(abs) = uri.path().strip_prefix("/@fs") {
        match fs_gate(&state, &PathBuf::from(urldecode(abs))) {
            Some(real) => real,
            None => {
                return (StatusCode::FORBIDDEN, "oj: /@fs path not allow-listed").into_response();
            }
        }
    } else {
        match locate(&state.root, state.public_dir.as_deref(), rel) {
            Some(file) => file,
            None => {
                if let Some(resp) =
                    forward_to_plugin_middleware(&state, &Method::GET, &uri, &headers, Vec::new())
                        .await
                {
                    return resp;
                }
                if let Some(resp) = serve_plugin_load_fallback(&state, &uri).await {
                    return resp;
                }
                // Vite's htmlFallback: `/dir/` serves dir/index.html, `/page` serves
                // page.html; only appType `spa` then falls back to the root index.html.
                if state.app_type != "custom" && accepts_html_fallback(&headers) {
                    if let Some(page) = html_fallback_candidate(rel)
                        .and_then(|c| locate(&state.root, state.public_dir.as_deref(), &c))
                    {
                        return match tokio::fs::read(&page).await {
                            Ok(bytes) => serve_html(&state, bytes, &format!("/{rel}"), &page).await,
                            Err(_) => (StatusCode::NOT_FOUND, format!("oj: no such file: /{rel}"))
                                .into_response(),
                        };
                    }
                }
                if state.app_type == "spa" && is_spa_navigation(rel, &headers) {
                    return serve_index_html(&state).await;
                }
                return (StatusCode::NOT_FOUND, format!("oj: no such file: /{rel}"))
                    .into_response();
            }
        }
    };

    if path_is_denied(&file, &state.root, &state.fs_deny) {
        return (StatusCode::FORBIDDEN, "oj: path denied by server.fs.deny").into_response();
    }

    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
    // A publicDir file is served verbatim, never compiled (Vite's servePublicMiddleware
    // runs before transform); only an explicit asset query still yields a module.
    let in_public = state
        .public_dir
        .as_deref()
        .is_some_and(|p| file.starts_with(p));
    if let Some(kind) = query_asset_kind(uri.query()) {
        // A public file's url is its path under the public dir (`/logo.svg`,
        // Vite's checkPublicFile), not `/public/logo.svg`.
        let url = match state.public_dir.as_deref().filter(|_| in_public) {
            Some(p) => format!("/{}", file.strip_prefix(p).unwrap_or(&file).display()),
            None => url_of(&state.root, &file),
        };
        let js = if kind == "inline" && is_style_ext(ext) {
            inline_css_module(&state, &file, &url).await
        } else {
            asset_module(&file, &url, kind).await
        };
        return match js {
            Ok(js) => (
                [
                    (header::CONTENT_TYPE, "text/javascript"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                js,
            )
                .into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {e}")).into_response(),
        };
    }
    if is_style_ext(ext) && !in_public {
        let url = url_of(&state.root, &file);
        let q = uri.query();
        let direct = q.is_some_and(|q| q.split('&').any(|kv| kv == "direct"));
        let import_query = q.is_some_and(|q| q.split('&').any(|kv| kv == "import"));
        if direct || (wants_raw_resource(&headers) && !import_query) {
            // `<link href>`, fetch() of a `?url` stylesheet, or `?direct`: compiled
            // CSS text, not the preprocessor source and not the JS wrapper.
            return serve_css_direct(&state, &file, &url).await;
        }
        return serve_css_wrapper(&state, &file, &url).await;
    }
    if in_public {
        return serve_static_file(&file, ext).await;
    }
    if COMPILABLE.contains(&ext) {
        let url = url_of(&state.root, &file);
        return serve_compiled(&state, &file, &url, uri.query(), &headers).await;
    }
    if ext == "svg"
        && uri
            .query()
            .is_some_and(|q| q.split('&').any(|kv| kv == "react"))
    {
        let url = format!("{}?react", url_of(&state.root, &file));
        return serve_compiled(&state, &file, &url, None, &headers).await;
    }
    // A plain `.svg` import goes through the compile path so a configured svgr can
    // componentize it; unmatched svgs fall back to a URL asset there.
    if ext.eq_ignore_ascii_case("svg")
        && query_asset_kind(uri.query()).is_none()
        && !wants_raw_resource(&headers)
    {
        let url = url_of(&state.root, &file);
        if state.plugins_have_transform {
            return serve_compiled(&state, &file, &url, uri.query(), &headers).await;
        }
        // No plugin can componentize it: a module import of an svg (relative,
        // aliased or root-absolute) is its URL, like any other asset in Vite.
        return match asset_module(&file, &url, "url").await {
            Ok(js) => (
                [
                    (header::CONTENT_TYPE, "text/javascript"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                js,
            )
                .into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {e}")).into_response(),
        };
    }
    if ext == "json" {
        let url = url_of(&state.root, &file);
        return serve_compiled(&state, &file, &url, uri.query(), &headers).await;
    }
    if is_importable_asset_ext(ext)
        && query_asset_kind(uri.query()).is_none()
        && wants_module_import(&headers, uri.query())
    {
        let url = url_of(&state.root, &file);
        return match asset_module(&file, &url, "url").await {
            Ok(js) => (
                [
                    (header::CONTENT_TYPE, "text/javascript"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                js,
            )
                .into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {e}")).into_response(),
        };
    }

    match tokio::fs::read(&file).await {
        Ok(bytes) if ext == "html" => {
            serve_html(&state, bytes, &url_of(&state.root, &file), &file).await
        }
        Ok(bytes) if ext == "css" => {
            let source = String::from_utf8_lossy(&bytes).into_owned();
            if is_tailwind_css(&source) {
                let url = url_of(&state.root, &file);
                return match compile_tailwind(&state, &url, &source).await {
                    Ok(css) => (
                        [
                            (header::CONTENT_TYPE, "text/css"),
                            (header::CACHE_CONTROL, "no-cache"),
                        ],
                        css,
                    )
                        .into_response(),
                    Err(err) => {
                        send_error(&state, &err);
                        (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {err}")).into_response()
                    }
                };
            }
            ([(header::CONTENT_TYPE, "text/css")], source).into_response()
        }
        Ok(bytes) => {
            let mut response = Response::new(Body::from(bytes));
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, content_type(ext).parse().unwrap());
            response
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("oj: read error: {err}"),
        )
            .into_response(),
    }
}

/// The file's bytes as-is with its content type (publicDir files).
pub(crate) async fn serve_static_file(file: &Path, ext: &str) -> Response {
    match tokio::fs::read(file).await {
        Ok(bytes) => {
            let mut response = Response::new(Body::from(bytes));
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, content_type(ext).parse().unwrap());
            response
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("oj: read error: {err}"),
        )
            .into_response(),
    }
}

pub(crate) fn inject_dev_scripts(html: String) -> String {
    let tags = "<script type=\"module\" src=\"/@oj/refresh-preamble.js\"></script>\n\
                <script type=\"module\" src=\"/@oj/client.js\"></script>";
    match html.find("<head>") {
        Some(idx) => {
            let insert_at = idx + "<head>".len();
            format!("{}\n{}{}", &html[..insert_at], tags, &html[insert_at..])
        }
        None => format!("{tags}\n{html}"),
    }
}

pub(crate) async fn serve_compiled(
    state: &Arc<ServerState>,
    file: &Path,
    url: &str,
    query: Option<&str>,
    headers: &HeaderMap,
) -> Response {
    // Key and transform per full url incl. query (the same file yields per-query
    // variants), but the HMR `t=` buster is stripped: not part of module identity.
    let base_url = url;
    let url_with_query = match query {
        Some(q) => strip_hmr_timestamp(&format!("{url}?{q}")),
        None => url.to_string(),
    };
    let url = url_with_query.as_str();
    let (key, module) = match ensure_module(state, file, url).await {
        Ok(pair) => pair,
        Err(err) => {
            send_error(state, &err);
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {err}")).into_response();
        }
    };

    let etag = format!("\"{key}\"");
    if query.is_none() {
        if let Some(inm) = headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
        {
            if inm == etag {
                return (
                    StatusCode::NOT_MODIFIED,
                    [
                        (header::ETAG, etag),
                        (header::CACHE_CONTROL, "no-cache".to_string()),
                    ],
                )
                    .into_response();
            }
        }
    }

    let mut body = if module.kind == "svelte" {
        format!("{}{}", svelte_hot_glue(url), module.code)
    } else {
        module.code.clone()
    };
    if module.kind != "svelte" {
        let ctx_predefined = module.hot.is_some();
        if ctx_predefined {
            // The module reads import.meta.hot itself: define the context before its
            // body runs; the refresh glue below REUSES it (a re-declare is a SyntaxError).
            let full = match query {
                Some(q) if !q.is_empty() => format!("{base_url}?{q}"),
                _ => base_url.to_string(),
            };
            body = format!("{}{}", svelte_hot_glue(&strip_hmr_timestamp(&full)), body);
        }
        body.push_str(&hot_glue(
            base_url,
            query,
            module.is_boundary,
            ctx_predefined,
        ));
    }
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
