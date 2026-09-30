use super::*;

/// What `oj preview` serves with: Vite's resolved preview options (each one
/// `preview.x ?? server.x`, the port aside) plus the build's base and assetsDir.
#[derive(Debug, Default, Clone)]
pub struct PreviewOptions {
    pub dir: PathBuf,
    pub port: u16,
    pub base: String,
    pub headers: Vec<(String, String)>,
    pub host: Option<String>,
    pub strict_port: bool,
    /// `preview.open`: `Some(path)` opens `url + path` once listening.
    pub open: Option<String>,
    pub cors: Option<oj_config::CorsConfig>,
    pub allowed_hosts: Option<oj_config::AllowedHosts>,
    /// `appType: "spa"` falls back to index.html for unknown paths; `mpa`/`custom` 404.
    pub spa_fallback: bool,
    /// `build.assetsDir`: hashed files under it are immutable.
    pub assets_dir: String,
}

/// The static preview server's per-request state.
pub(crate) struct PreviewState {
    dir: PathBuf,
    base: String,
    headers: Vec<(header::HeaderName, header::HeaderValue)>,
    spa_fallback: bool,
    assets_prefix: String,
}

pub(crate) async fn preview_host_check(
    State(policy): State<Arc<HostPolicy>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if let Some(raw) = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
    {
        let host = HostPolicy::host_header_name(raw);
        if !policy.hostname_allowed(host) {
            return (StatusCode::FORBIDDEN, HostPolicy::reject_message(host)).into_response();
        }
    }
    next.run(req).await
}

pub async fn preview(opts: PreviewOptions) -> anyhow::Result<()> {
    let dir = opts.dir.canonicalize().with_context(|| {
        format!(
            "build dir not found: {} (run `oj build` first)",
            opts.dir.display()
        )
    })?;
    let headers: Vec<(header::HeaderName, header::HeaderValue)> = opts
        .headers
        .iter()
        .filter_map(|(k, v)| Some((k.parse().ok()?, v.parse().ok()?)))
        .collect();
    let assets_dir = opts.assets_dir.trim_matches('/');
    let state = Arc::new(PreviewState {
        dir: dir.clone(),
        base: opts.base.clone(),
        headers,
        spa_fallback: opts.spa_fallback,
        assets_prefix: if assets_dir.is_empty() {
            "assets/".to_string()
        } else {
            format!("{assets_dir}/")
        },
    });
    let mut app = Router::new().fallback(get(preview_serve)).with_state(state);
    // Vite's preview stack: cors (unless `false`), then the Host check against
    // DNS rebinding (unless `allowedHosts: true`), then static files.
    if let Some(cors) = CorsPolicy::from_config(opts.cors.as_ref()) {
        app = app.layer(axum::middleware::from_fn_with_state(
            Arc::new(cors),
            cors_middleware,
        ));
    }
    let host_policy = HostPolicy::from_config(
        &oj_config::ServerConfig {
            allowed_hosts: opts.allowed_hosts.clone(),
            host: opts.host.clone(),
            ..Default::default()
        },
        None,
    );
    if !host_policy.allow_all {
        app = app.layer(axum::middleware::from_fn_with_state(
            Arc::new(host_policy),
            preview_host_check,
        ));
    }
    let (listener, port) = bind_dev_listener(
        resolve_host(opts.host.as_deref()),
        opts.port,
        opts.strict_port,
    )
    .await?;
    println!("  {} preview", oj_brand());
    println!("  serving: {}", dir.display());
    let url = format!("http://localhost:{port}{}", opts.base);
    println!("  {}", link(&url, &cell(&url)));
    if let Some(path) = &opts.open {
        let target = if path.starts_with("http://") || path.starts_with("https://") {
            path.clone()
        } else {
            format!(
                "{}{}",
                url.trim_end_matches('/'),
                if path.starts_with('/') {
                    path.clone()
                } else {
                    format!("/{path}")
                }
            )
        };
        open_browser(&target);
    }
    axum::serve(listener, app).await?;
    Ok(())
}

pub(crate) fn preview_rel(path: &str, base: &str) -> Option<String> {
    let trimmed = path
        .strip_prefix(base.trim_end_matches('/'))
        .unwrap_or(path);
    let rel = urldecode(trimmed.trim_start_matches('/'));
    if rel.split('/').any(|seg| seg == "..") {
        return None;
    }
    Some(if rel.is_empty() {
        "index.html".to_string()
    } else {
        rel
    })
}

/// Extensionless preview fallback: the page's own `index.html`, then
/// `<path>.html`, then (SPA only) the root index.html; never the wrong page.
pub(crate) fn preview_html_fallback(dir: &Path, rel: &str, spa: bool) -> Option<PathBuf> {
    let rel = rel.trim_end_matches('/');
    if !rel.is_empty() {
        let dir_index = dir.join(rel).join("index.html");
        if dir_index.is_file() {
            return Some(dir_index);
        }
        let sibling = dir.join(format!("{rel}.html"));
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    (spa || rel.is_empty() || rel == "index.html").then(|| dir.join("index.html"))
}

pub(crate) async fn preview_serve(State(state): State<Arc<PreviewState>>, uri: Uri) -> Response {
    let PreviewState {
        dir,
        base,
        headers: extra_headers,
        spa_fallback,
        assets_prefix,
    } = &*state;
    let Some(rel) = preview_rel(uri.path(), base) else {
        return (StatusCode::FORBIDDEN, "oj: path traversal denied").into_response();
    };
    let file = dir.join(&rel);
    let ext = Path::new(&rel)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    let (target, ctype) = if file.is_file() {
        (file, content_type(ext))
    } else if ext.is_empty() {
        // Vite's htmlFallbackMiddleware: `/x` -> `/x.html` or `/x/index.html`;
        // only `appType: "spa"` then falls back to the root index.html.
        match preview_html_fallback(dir, &rel, *spa_fallback) {
            Some(target) => (target, "text/html; charset=utf-8"),
            None => {
                return (StatusCode::NOT_FOUND, format!("oj: not found: {rel}")).into_response()
            }
        }
    } else {
        return (StatusCode::NOT_FOUND, format!("oj: not found: {rel}")).into_response();
    };

    // Hashed build assets cache forever; unhashed HTML must revalidate.
    let cache_control = if rel.starts_with(assets_prefix.as_str()) {
        "public, max-age=31536000, immutable"
    } else if ctype.starts_with("text/html") {
        "no-cache"
    } else {
        ""
    };

    match tokio::fs::read(&target).await {
        Ok(bytes) => {
            let mut resp = ([(header::CONTENT_TYPE, ctype)], bytes).into_response();
            let h = resp.headers_mut();
            if !cache_control.is_empty() {
                h.insert(
                    header::CACHE_CONTROL,
                    header::HeaderValue::from_static(cache_control),
                );
            }
            for (name, value) in extra_headers {
                h.insert(name.clone(), value.clone());
            }
            resp
        }
        Err(_) => (StatusCode::NOT_FOUND, "oj: not found").into_response(),
    }
}
