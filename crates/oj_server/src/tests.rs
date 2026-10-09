use super::*;

// The cached body reaches the wire verbatim and the header tuple
// *overwrites* the `application/octet-stream` that `Bytes` sets (insert,
// not append), so content-type stays a single text/javascript.
#[tokio::test]
async fn cached_js_response_serves_bytes_verbatim_with_text_javascript() {
    let body = Bytes::from("console.log(1)".to_string());
    let resp = cached_js_response(&HeaderMap::new(), "\"e\"".to_string(), body.clone());
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[header::CONTENT_TYPE], "text/javascript");
    assert_eq!(
        resp.headers().get_all(header::CONTENT_TYPE).iter().count(),
        1
    );
    assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-cache");
    assert_eq!(resp.headers()[header::ETAG], "\"e\"");
    let got = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(got, body);
}

#[tokio::test]
async fn cached_js_response_304_on_matching_etag_has_empty_body() {
    let mut headers = HeaderMap::new();
    headers.insert(header::IF_NONE_MATCH, "\"e\"".parse().unwrap());
    let resp = cached_js_response(&headers, "\"e\"".to_string(), Bytes::from_static(b"x"));
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(resp.headers()[header::ETAG], "\"e\"");
    let got = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(got.is_empty());
}

// The watcher's pre-init fast-skip toward the lazy SSR host: recording a
// skipped event is instant (the watcher path makes NO RPC toward a
// still-initializing host — dispatching used to serially burn a full
// per-call init window per hook per save on a wedged init), the backlog
// dedups by file keeping the latest change type, and once the host
// initializes the catch-up task replays the backlog as watchChange.
#[tokio::test]
async fn ssr_watch_skip_is_instant_and_replays_at_the_hosts_init() {
    let root = std::env::temp_dir().join(format!("oj-ssr-watch-skip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let config = serde_json::json!({
        "config": { "root": root.display().to_string() },
        "env": { "command": "serve", "mode": "development" },
    })
    .to_string();

    // A wedged lazy host: skips must complete in milliseconds anyway.
    let plugins = root.join("oj.plugins.mjs");
    std::fs::write(
        &plugins,
        "setInterval(() => {}, 1000);\nawait new Promise(() => {});\nexport default [];\n",
    )
    .unwrap();
    let wedged = match plugins::PluginHost::spawn_lazy_with_wait(
        &root,
        &plugins,
        &config,
        std::time::Duration::from_secs(5),
    )
    .await
    {
        Ok(h) => h,
        Err(_) => return, // no node on this machine
    };
    let queue = Arc::new(SsrWatchQueue::default());
    assert!(!wedged.is_initialized());
    let t0 = std::time::Instant::now();
    // What decide() does per save while the host is pre-init.
    note_ssr_watch_skip(&queue, "/app/a.ts", "update");
    note_ssr_watch_skip(&queue, "/app/a.ts", "delete");
    note_ssr_watch_skip(&queue, "/app/b.ts", "update");
    assert!(
        t0.elapsed() < std::time::Duration::from_millis(200),
        "a skipped dispatch never waits on the host: {:?}",
        t0.elapsed()
    );
    assert_eq!(
        *queue.backlog.lock().unwrap(),
        vec![
            ("/app/a.ts".to_string(), "delete".to_string()),
            ("/app/b.ts".to_string(), "update".to_string()),
        ],
        "deduped by file, latest change type wins"
    );
    wedged.shutdown();

    // A healthy slow host: the catch-up task replays the backlog at init.
    std::fs::write(
        &plugins,
        "await new Promise((r) => setTimeout(r, 1000));\nexport default [];\n",
    )
    .unwrap();
    let host = match plugins::PluginHost::spawn_lazy_with_wait(
        &root,
        &plugins,
        &config,
        std::time::Duration::from_secs(30),
    )
    .await
    {
        Ok(h) => h,
        Err(_) => return,
    };
    spawn_ssr_watch_catch_up(std::sync::Arc::clone(&host), Arc::clone(&queue));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !queue.backlog.lock().unwrap().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the backlog must drain once the host initializes"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(host.is_initialized(), "the replay only runs post-init");
    host.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

// The ordering guarantee: every dispatch toward the SSR host serializes
// on the queue's order lock with the backlog drained first, so a queued
// (older) watchChange always reaches the host BEFORE a live (newer) event
// for the same file — even when the live dispatch races the catch-up
// task mid-replay. The host-side plugin logs arrivals; the log's last
// line must be the live event.
#[tokio::test]
async fn ssr_watch_catch_up_events_land_before_a_racing_live_event() {
    let root = std::env::temp_dir().join(format!("oj-ssr-watch-order-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let log = root.join("watch.log");
    // A slow-boot host whose watchChange hook is slow and logs arrivals:
    // the live dispatch below races the catch-up task mid-replay.
    let plugins = root.join("oj.plugins.mjs");
    std::fs::write(
        &plugins,
        format!(
            r#"import {{ appendFileSync }} from "node:fs";
await new Promise((r) => setTimeout(r, 800));
export default [{{
  name: "watch-logger",
  async watchChange(id, change) {{
await new Promise((r) => setTimeout(r, 150));
appendFileSync({log:?}, id + "|" + ((change && change.event) || "?") + "\n");
  }},
}}];
"#,
            log = log.display().to_string(),
        ),
    )
    .unwrap();
    let config = serde_json::json!({
        "config": { "root": root.display().to_string() },
        "env": { "command": "serve", "mode": "development" },
    })
    .to_string();
    let host = match plugins::PluginHost::spawn_lazy_with_wait(
        &root,
        &plugins,
        &config,
        std::time::Duration::from_secs(30),
    )
    .await
    {
        Ok(h) => h,
        Err(_) => return, // no node on this machine
    };
    let queue = Arc::new(SsrWatchQueue::default());
    note_ssr_watch_skip(&queue, "/app/a.ts", "update");
    note_ssr_watch_skip(&queue, "/app/b.ts", "update");
    spawn_ssr_watch_catch_up(std::sync::Arc::clone(&host), Arc::clone(&queue));
    // Wait out init, then dispatch a LIVE event while the catch-up task
    // is (very likely) mid-replay — the order lock, not luck, is what
    // guarantees the outcome under every interleaving.
    let mut init = host.initialized_updates();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(20), init.wait_for(|v| *v),)
            .await
            .is_ok_and(|r| r.is_ok())
    );
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    // What decide() does for a live post-init event: flush, then send.
    replay_ssr_watch_backlog(&host, &queue).await;
    host.watch_change("/app/a.ts", "live")
        .await
        .expect("live dispatch reaches the host");
    let lines = std::fs::read_to_string(&log).unwrap_or_default();
    let lines: Vec<&str> = lines.lines().collect();
    assert_eq!(
        lines.last().copied(),
        Some("/app/a.ts|live"),
        "the live (newer) event lands LAST: {lines:?}"
    );
    assert!(
        lines.contains(&"/app/a.ts|update") && lines.contains(&"/app/b.ts|update"),
        "both queued events were replayed before it: {lines:?}"
    );
    host.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

// The late-activation resync ENQUEUE is confirmed only on an ACK:
// transient failures are retried with backoff, and a middleware that never
// acknowledges makes the helper report failure (the caller then warns
// about stale edits) instead of logging progress over a resync that never
// reached the queue. (Execution is a separate signal — see
// resync_completion_is_claimed_on_the_done_signal_never_on_the_ack.)
#[tokio::test]
async fn resync_retries_until_the_middleware_acks() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut n = 0u32;
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            n += 1;
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            // The first two attempts fail; the third is the real ACK.
            let resp = if n < 3 {
                "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            } else {
                "HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n"
            };
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });
    assert!(!notify_plugin_mw_resync(port).await, "a 500 is not an ACK");
    assert!(
        resync_plugin_mw_with_retry(port).await,
        "the retry loop must reach the eventual ACK"
    );
}

#[tokio::test]
async fn resync_reports_failure_when_nothing_acks() {
    // A port with nothing listening: every attempt is refused.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    assert!(!resync_plugin_mw_with_retry(port).await);
}

// The ack is 202-style ("enqueued"); "resynced" is claimed only when the
// host's { ojResyncDone } completion signal moves the counter past the
// pre-enqueue baseline. A busy queue completes late (still claimed), a
// completion racing ahead of the wait is not missed (baseline semantics),
// and a stuck queue times out into the caller's warning path.
#[tokio::test]
async fn resync_completion_is_claimed_on_the_done_signal_never_on_the_ack() {
    let (tx, mut rx) = tokio::sync::watch::channel(0u64);
    // Busy queue: the completion lands after the wait began.
    let baseline = *rx.borrow_and_update();
    let signal = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        tx.send_modify(|c| *c += 1);
        tx
    });
    assert!(
        await_resync_completion(&mut rx, baseline, std::time::Duration::from_secs(5)).await,
        "a late completion is still claimed"
    );
    let tx = signal.await.unwrap();

    // Fast queue: the completion already landed before the wait started —
    // the pre-enqueue baseline catches it.
    let baseline = *rx.borrow_and_update();
    tx.send_modify(|c| *c += 1);
    assert!(
        await_resync_completion(&mut rx, baseline, std::time::Duration::from_secs(5)).await,
        "a completion racing ahead of the wait is never missed"
    );

    // Stuck queue: no signal within the bound — the caller warns instead
    // of logging success off the enqueue ack.
    let baseline = *rx.borrow_and_update();
    assert!(
        !await_resync_completion(&mut rx, baseline, std::time::Duration::from_millis(50)).await,
        "a stuck queue must not be reported as resynced"
    );
}

// One packed word: every reader sees (port, runner_environments) from the
// same write, and a late activation runs the handler BEFORE the flip is
// visible — no reader can observe the new mode with the catch-up unarmed.
#[test]
fn plugin_serve_packs_one_snapshot_and_arms_before_the_flip() {
    let info = |port: Option<u16>, runner: bool| plugins::ServeInfo {
        middleware_port: port,
        runner_environments: runner,
    };
    let serve = PluginServe::default();
    assert_eq!(serve.mw_port(), None);
    assert!(!serve.runner_environments());

    // The handler observes the pre-flip state: set() runs it first.
    let seen = Arc::new(Mutex::new(None::<(Option<u16>, bool)>));
    {
        let serve = Arc::new(PluginServe::default());
        let inner = Arc::clone(&serve);
        let sink = Arc::clone(&seen);
        serve.set_on_activate(Box::new(move || {
            *sink.lock().unwrap() = Some((inner.mw_port(), inner.runner_environments()));
        }));
        assert!(!serve.activated_late());
        serve.set(&info(Some(4001), true));
        assert_eq!(
            *seen.lock().unwrap(),
            Some((None, false)),
            "the activation handler must run before readers can see the flip"
        );
        assert_eq!(serve.mw_port(), Some(4001));
        assert!(serve.runner_environments());
        // A registrar that lost the race to this activation can catch up.
        assert!(serve.activated_late());
        // A repeat set with the same info is not a second activation.
        *seen.lock().unwrap() = None;
        serve.set(&info(Some(4001), true));
        assert_eq!(*seen.lock().unwrap(), None);
    }

    // runner_environments only counts with a middleware port.
    let no_port = PluginServe::from_info(&info(None, true));
    assert_eq!(no_port.mw_port(), None);
    assert!(!no_port.runner_environments());
    let no_runner = PluginServe::from_info(&info(Some(4002), false));
    assert_eq!(no_runner.mw_port(), Some(4002));
    assert!(!no_runner.runner_environments());
    // The boot fill is not a late activation: a caller's post-registration
    // catch-up must not fire (and reload the runner) on every normal boot.
    assert!(!no_runner.activated_late());
}

#[test]
fn dep_transform_gate_matches_only_marker_sources() {
    // The plugins' own transform code-filter patterns (getDepTransformFilters).
    let res: Vec<regex::Regex> = [
        r"\bcreateServerFn\b|\.\s*handler\s*\(",
        "createIsomorphicFn",
    ]
    .iter()
    .map(|s| regex::Regex::new(s).unwrap())
    .collect();
    let wants = |src: &str| res.iter().any(|re| re.is_match(src));
    assert!(wants(
        "export const f = createIsomorphicFn().client(() => 1)"
    ));
    assert!(wants("const x = createServerFn()"));
    assert!(wants("route.handler ( () => {} )"));
    assert!(!wants("export const x = 1;"));
    assert!(!wants(
        "import { getStartContext } from '@tanstack/start-storage-context'"
    ));
}

#[test]
fn decode_at_id_handles_hex_and_raw_vite_ids() {
    assert_eq!(decode_at_id(&hex_encode("virtual:x")), "virtual:x");
    assert_eq!(
        decode_at_id("virtual:tanstack-start-dev-client-entry"),
        "virtual:tanstack-start-dev-client-entry"
    );
    assert_eq!(decode_at_id("__x00__virtual:foo"), "\0virtual:foo");
}

#[test]
fn parse_hmr_filter_reads_filter_module_urls() {
    let seeds =
        parse_hmr_filter(r#"{"action":"filter","modules":["/src/a.tsx","/src/b.tsx"]}"#).unwrap();
    assert_eq!(
        seeds,
        vec![PathBuf::from("/src/a.tsx"), PathBuf::from("/src/b.tsx")]
    );
}

#[test]
fn parse_hmr_filter_ignores_non_filter_payloads() {
    assert!(parse_hmr_filter("skip").is_none());
    assert!(parse_hmr_filter("full-reload").is_none());
    assert!(parse_hmr_filter(r#"{"action":"reload"}"#).is_none());
    assert!(parse_hmr_filter(r#"{"action":"filter"}"#).is_none());
    assert!(parse_hmr_filter("not json at all").is_none());
}

#[tokio::test]
async fn bind_dev_listener_increments_unless_strict() {
    use std::net::{IpAddr, Ipv4Addr};
    let host = IpAddr::V4(Ipv4Addr::LOCALHOST);
    // Hold an ephemeral port so the preferred one is busy.
    let occupied = tokio::net::TcpListener::bind((host, 0)).await.unwrap();
    let taken = occupied.local_addr().unwrap().port();

    // strict: a busy preferred port is a hard error, never moved.
    assert!(
        bind_dev_listener(host, taken, true).await.is_err(),
        "strict must reject a busy port",
    );

    // non-strict (Vite default): hop to the next free port.
    let (listener, port) = bind_dev_listener(host, taken, false).await.unwrap();
    assert_ne!(port, taken, "non-strict must pick a different port");
    assert_eq!(listener.local_addr().unwrap().port(), port);
}

// A stylesheet's `?inline` (compiled css string through the full pipeline,
// not a data URI) is covered end to end by e2e/css-vite-parity.mjs, since
// it runs the server's compile path.

#[tokio::test]
async fn non_css_inline_stays_a_data_uri() {
    let dir = tempfile::tempdir().unwrap();
    let png = dir.path().join("pixel.png");
    std::fs::write(&png, [0u8, 1, 2, 3]).unwrap();
    let out = asset_module(&png, "/pixel.png?inline", "inline")
        .await
        .unwrap();
    assert!(
        out.contains("data:"),
        "binary asset stays a data URI: {out}"
    );
}

#[test]
fn postcss_config_is_found_like_postcss_load_config() {
    let base = std::env::temp_dir().join(format!("oj-postcss-find-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let app = base.join("packages/web");
    std::fs::create_dir_all(&app).unwrap();
    // Workspace root marker: Vite's searchForWorkspaceRoot is the stopDir of
    // its postcss-load-config search (`.git` is not a marker in Vite).
    std::fs::write(base.join("pnpm-workspace.yaml"), "packages: ['packages/*']").unwrap();
    assert!(find_postcss_config(&app).is_none());
    // A config at the workspace root applies to the package.
    std::fs::write(base.join(".postcssrc.json"), r#"{"plugins":{}}"#).unwrap();
    assert_eq!(
        find_postcss_config(&app),
        Some(base.join(".postcssrc.json"))
    );
    // package.json#postcss in the package itself wins (nearest first).
    std::fs::write(
        app.join("package.json"),
        r#"{"name":"web","postcss":{"plugins":{}}}"#,
    )
    .unwrap();
    assert_eq!(find_postcss_config(&app), Some(app.join("package.json")));
    // ...and a config file in the package beats its package.json key.
    std::fs::write(app.join("postcss.config.ts"), "export default {}").unwrap();
    assert_eq!(
        find_postcss_config(&app),
        Some(app.join("postcss.config.ts"))
    );
    // A package.json without the key does not count.
    std::fs::remove_file(app.join("postcss.config.ts")).unwrap();
    std::fs::write(app.join("package.json"), r#"{"name":"web"}"#).unwrap();
    assert_eq!(
        find_postcss_config(&app),
        Some(base.join(".postcssrc.json"))
    );
    // Nothing above the workspace root is consulted.
    std::fs::remove_file(base.join(".postcssrc.json")).unwrap();
    assert!(find_postcss_config(&app).is_none());
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn fs_deny_blocks_dotenv_and_git_by_default() {
    // No user config: Vite's default deny set still protects secrets.
    let root = Path::new("/proj");
    let deny = compile_fs_deny(&[]);
    assert!(path_is_denied(&root.join(".env"), root, &deny));
    assert!(path_is_denied(&root.join(".env.local"), root, &deny));
    assert!(path_is_denied(&root.join("certs/server.pem"), root, &deny));
    assert!(path_is_denied(&root.join(".git/config"), root, &deny));
    assert!(path_is_denied(
        &root.join("packages/app/.git/HEAD"),
        root,
        &deny
    ));
    // Ordinary source is served.
    assert!(!path_is_denied(&root.join("src/main.tsx"), root, &deny));
    assert!(!path_is_denied(&root.join("src/env.ts"), root, &deny));
}

#[test]
fn fs_deny_honors_user_patterns() {
    let root = Path::new("/proj");
    let deny = compile_fs_deny(&["secrets/**".to_string(), "*.key".to_string()]);
    assert!(path_is_denied(&root.join("secrets/token.txt"), root, &deny));
    assert!(path_is_denied(&root.join("id_rsa.key"), root, &deny));
    assert!(!path_is_denied(&root.join("public/logo.svg"), root, &deny));
}

#[test]
fn fs_deny_is_case_insensitive() {
    // On a case-insensitive filesystem `.ENV` opens the same bytes as
    // `.env`, so a case-sensitive deny glob would leak it. Every case
    // variant of a denied name (base-name and path patterns) must be denied.
    let root = Path::new("/proj");
    let deny = compile_fs_deny(&["secrets/**".to_string(), "*.key".to_string()]);
    assert!(path_is_denied(&root.join(".ENV"), root, &deny));
    assert!(path_is_denied(&root.join(".Env.Production"), root, &deny));
    assert!(path_is_denied(&root.join("certs/SERVER.PEM"), root, &deny));
    assert!(path_is_denied(&root.join(".GIT/config"), root, &deny));
    assert!(path_is_denied(&root.join("Secrets/token.txt"), root, &deny));
    assert!(path_is_denied(&root.join("id_rsa.KEY"), root, &deny));
}

#[test]
fn fs_deny_expands_brace_groups() {
    // Vite's own default deny list is brace-form (*.{crt,pem,key,...});
    // the glob crate has no brace support, so without expansion those
    // patterns match nothing and denied files get served.
    let root = Path::new("/proj");
    let deny = compile_fs_deny(&[
        "*.{key,p12,pfx}".to_string(),
        "secrets/{a,b}/**".to_string(),
    ]);
    assert!(path_is_denied(&root.join("server.key"), root, &deny));
    assert!(path_is_denied(&root.join("bundle.p12"), root, &deny));
    assert!(path_is_denied(&root.join("cert.pfx"), root, &deny));
    assert!(path_is_denied(&root.join("secrets/a/token"), root, &deny));
    assert!(path_is_denied(&root.join("secrets/b/token"), root, &deny));
    assert!(!path_is_denied(&root.join("secrets/c/token"), root, &deny));
    assert!(!path_is_denied(&root.join("server.kee"), root, &deny));
}

#[test]
fn warmup_paths_matches_exclusions_root_relative() {
    // A './'-spelled exclusion and a bare positive pattern must agree
    // about the same file, and a root containing glob metacharacters must
    // not break the walk (the root is escaped, patterns are relative).
    let dir = std::env::temp_dir().join(format!("oj-warmup-[x]-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src/generated")).unwrap();
    std::fs::write(dir.join("src/a.js"), "a").unwrap();
    std::fs::write(dir.join("src/generated/b.js"), "b").unwrap();
    let picked = warmup_paths(
        &dir,
        &[
            "src/**/*.js".to_string(),
            "!./src/generated/*.js".to_string(),
        ],
    );
    assert_eq!(picked.len(), 1, "exclusion must apply: {picked:?}");
    assert!(picked[0].ends_with("src/a.js"));
}

#[test]
fn bare_specifier_classification_routes_plugin_virtuals_to_the_fallback() {
    // Plugin virtuals (`virtual:pwa-register`, \0-prefixed ids) count as
    // bare, so the plugin-host fallback (/@id/) resolves them; relative and
    // absolute-URL specifiers do not.
    assert!(is_bare_specifier("react"));
    assert!(is_bare_specifier("virtual:pwa-register"));
    assert!(is_bare_specifier("\0oj-virtual"));
    assert!(!is_bare_specifier("./local"));
    assert!(!is_bare_specifier("../up"));
    assert!(!is_bare_specifier("/abs"));
    assert!(!is_bare_specifier("https://cdn/x.js"));
}

#[test]
fn sec_fetch_dest_decides_raw_vs_module_form() {
    // A `<link>`/`<img>`/@font-face request wants the raw resource; a JS
    // `import` (script/empty dest) wants the JS-module form. This is how a
    // `.css` reached from JS is served as JS, not a text/css module script.
    let raw = |d: &str| {
        let mut h = HeaderMap::new();
        h.insert("sec-fetch-dest", d.parse().unwrap());
        wants_raw_resource(&h)
    };
    assert!(raw("style"));
    assert!(raw("image"));
    assert!(raw("font"));
    assert!(!raw("script"));
    assert!(!raw("empty"));
    // No header (older browsers / non-browser clients): default to the
    // module form so JS imports keep working.
    assert!(!wants_raw_resource(&HeaderMap::new()));
}

#[test]
fn imports_a_plugin_virtual_flags_only_missing_fs_paths() {
    // A cached module is re-transformed on warm start only if it imports a
    // plugin-served virtual: a filesystem-path import with no file on disk.
    // Real files (source, svgr's .svg) and oj-internal /@ routes keep the
    // fast persistent cache.
    let root = std::env::temp_dir().join(format!("oj-ipv-test-{}", std::process::id()));
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("real.ts"), "export {}\n").unwrap();
    let dc = Mutex::new(DirCache::new());
    let r = root.as_path();

    // All-real imports -> keep cache.
    assert!(!imports_a_plugin_virtual(
        &["/src/real.ts".to_string()],
        r,
        &dc
    ));
    // oj-internal routes and external urls are never plugin-state virtuals.
    assert!(!imports_a_plugin_virtual(
        &[
            "/@id/abc".to_string(),
            "/@virtual/x".to_string(),
            "https://cdn/x.js".to_string()
        ],
        r,
        &dc,
    ));
    // A missing absolute path (wyw's .wyw-in-js.css) -> re-transform.
    assert!(imports_a_plugin_virtual(
        &[
            "/src/real.ts".to_string(),
            "/Users/nope/x.wyw-in-js.css".to_string()
        ],
        r,
        &dc,
    ));
    // A missing root-relative path -> re-transform.
    assert!(imports_a_plugin_virtual(
        &["/src/gone.css".to_string()],
        r,
        &dc
    ));
    // Query strings are stripped before the on-disk check.
    assert!(!imports_a_plugin_virtual(
        &["/src/real.ts?import".to_string()],
        r,
        &dc
    ));

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn lingui_macro_specifiers_are_matched_exactly() {
    assert!(is_lingui_macro_specifier("@lingui/macro"));
    assert!(is_lingui_macro_specifier("@lingui/core/macro"));
    assert!(is_lingui_macro_specifier("@lingui/react/macro"));
    // The runtime packages (not the macro entrypoints) must NOT be shimmed.
    assert!(!is_lingui_macro_specifier("@lingui/core"));
    assert!(!is_lingui_macro_specifier("@lingui/react"));
    assert!(!is_lingui_macro_specifier("@lingui/macro/extra"));
}

#[test]
fn node_builtins_are_recognized_for_stubbing() {
    assert!(is_node_builtin("fs"));
    assert!(is_node_builtin("node:fs"));
    assert!(is_node_builtin("fs/promises"));
    assert!(is_node_builtin("crypto"));
    assert!(is_node_builtin("perf_hooks"));
    // Vite's isNodeBuiltin: every `node:` id is a builtin, including the
    // scheme-only modules and ones newer than any hardcoded list.
    assert!(is_node_builtin("node:sqlite"));
    assert!(is_node_builtin("node:sea"));
    assert!(is_node_builtin("node:test"));
    assert!(is_node_builtin("node:whatever-node-adds-next"));
    assert!(is_node_builtin("_http_common"));
    // Not builtins: real packages and app specifiers. The scheme-only names
    // are not builtins when bare, exactly like Node.
    assert!(!is_node_builtin("sqlite"));
    assert!(!is_node_builtin("test"));
    assert!(!is_node_builtin("source-map-support"));
    assert!(!is_node_builtin("react"));
    assert!(!is_node_builtin("./local"));
}

#[test]
fn workspace_root_matches_vite_search_for_workspace_root() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().canonicalize().unwrap();
    // A git repository is NOT a workspace marker (Vite comments `.git` out):
    // the default fs.allow for a project nested in a repo stays the project.
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(repo.join("package.json"), "{}").unwrap();
    let app = repo.join("apps/web");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(app.join("package.json"), r#"{"name":"web"}"#).unwrap();
    assert_eq!(
        workspace_root(&app),
        app,
        "nearest package.json, not the repo"
    );

    // A pnpm workspace marker above it widens the root to the workspace.
    std::fs::write(repo.join("pnpm-workspace.yaml"), "packages: ['apps/*']").unwrap();
    assert_eq!(workspace_root(&app), repo);
    std::fs::remove_file(repo.join("pnpm-workspace.yaml")).unwrap();

    // ...as does a root package.json with a `workspaces` field.
    std::fs::write(repo.join("package.json"), r#"{"workspaces":["apps/*"]}"#).unwrap();
    assert_eq!(workspace_root(&app), repo);

    // No package.json anywhere: the app root itself.
    let bare = repo.join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    std::fs::write(repo.join("package.json"), "{}").unwrap();
    assert_eq!(
        workspace_root(&bare),
        repo,
        "nearest ancestor with a package.json"
    );

    // The PARSED field decides, not a substring: a dependency literally
    // named "workspaces" must not widen the served root.
    std::fs::write(
        repo.join("package.json"),
        r#"{"dependencies":{"workspaces":"1.0.0"}}"#,
    )
    .unwrap();
    assert_eq!(workspace_root(&app), app, "substring is not a marker");
    std::fs::write(repo.join("package.json"), "{}").unwrap();

    // A deno.json with a truthy `workspace` is a marker (Vite's
    // hasWorkspaceDenoJSON); a deno.jsonc counts only as valid JSON.
    std::fs::write(repo.join("deno.json"), r#"{"workspace":["./apps/web"]}"#).unwrap();
    assert_eq!(workspace_root(&app), repo, "deno.json workspace widens");
    std::fs::remove_file(repo.join("deno.json")).unwrap();
    std::fs::write(
        repo.join("deno.jsonc"),
        "// comment\n{\"workspace\":[\"./apps/web\"]}",
    )
    .unwrap();
    assert_eq!(
        workspace_root(&app),
        app,
        "a deno.jsonc with comments is skipped like Vite"
    );
}

#[test]
fn browser_external_stub_warns_like_vite_on_property_access() {
    let src = browser_external_stub_source("node:fs");
    assert!(src.contains("has been externalized for browser compatibility"));
    assert!(src.contains("Cannot access \"${\"node:fs\"}.${key}\" in client code"));
    // Interop reads names off __cjs_exports and probes __esModule first; the
    // probe must stay silent and the module must still export both shapes.
    assert!(src.contains("key !== \"__esModule\""));
    assert!(src.contains("export default __oj_ext"));
    assert!(src.contains("export const __cjs_exports = __oj_ext"));
    // The stub is JS-safe for any id (quotes escaped through JSON).
    let quoted = browser_external_stub_source("a\"b");
    assert!(quoted.contains("\"a\\\"b\""));
}

#[test]
fn importable_asset_exts_exclude_code_and_svg() {
    assert!(is_importable_asset_ext("webp"));
    assert!(is_importable_asset_ext("png"));
    assert!(is_importable_asset_ext("woff2"));
    // Vite's list is case-insensitive and includes documents and more media.
    assert!(is_importable_asset_ext("PNG"));
    assert!(is_importable_asset_ext("pdf"));
    assert!(is_importable_asset_ext("flac"));
    assert!(is_asset_ext("Jpg"));
    assert_eq!(content_type("JPG"), "image/jpeg");
    // svg is routed to vite-plugin-svgr, not URL-exported here.
    assert!(!is_importable_asset_ext("svg"));
    assert!(!is_importable_asset_ext("SVG"));
    assert!(!is_importable_asset_ext("css"));
    assert!(!is_importable_asset_ext("js"));
}

#[test]
fn ts_source_outside_root_is_not_treated_as_a_dep() {
    // A monorepo package reached through a resolve.alias is served via /@fs/
    // but is TS/JSX source that must be transpiled, not dep/CJS-interop'd.
    let fs = Path::new("/repo/packages/ui/src/Button.tsx");
    assert!(!is_dep_module("/@fs/repo/packages/ui/src/Button.tsx", fs));
    let ts = Path::new("/repo/packages/ui/src/index.ts");
    assert!(!is_dep_module("/@fs/repo/packages/ui/src/index.ts", ts));
    // A real dependency (.js/.mjs, or anything under node_modules that is not
    // TS/JSX source) stays on the dep path.
    let dep = Path::new("/app/node_modules/react/index.js");
    assert!(is_dep_module("/node_modules/react/index.js", dep));
    // A linked workspace package (realpath outside node_modules) is source,
    // not a dep: plugins and the source compile path apply (Vite treats
    // linked packages the same way).
    let fs_js = Path::new("/repo/packages/ui/dist/index.mjs");
    assert!(!is_dep_module(
        "/@fs/repo/packages/ui/dist/index.mjs",
        fs_js
    ));
    let fs_dep = Path::new("/repo/node_modules/.pnpm/x@1/node_modules/x/index.js");
    assert!(is_dep_module(
        "/@fs/repo/node_modules/.pnpm/x@1/node_modules/x/index.js",
        fs_dep
    ));
    // App-local source (not node_modules, not /@fs/) is never a dep.
    let local = Path::new("/app/src/App.tsx");
    assert!(!is_dep_module("/src/App.tsx", local));
}

#[test]
fn html_injection_puts_preamble_first_in_head() {
    let out = inject_dev_scripts("<html><head><title>x</title></head></html>".into());
    let preamble = out.find("refresh-preamble").unwrap();
    let title = out.find("<title>").unwrap();
    assert!(preamble < title);
}

#[test]
fn glue_only_added_for_boundary_modules() {
    assert!(hot_glue("/src/util.ts", None, false, false).is_empty());
    let glue = hot_glue("/src/App.tsx", Some("t=1700000000000"), true, false);
    assert!(glue.contains(r#"createHotContext("/src/App.tsx")"#));
    assert!(
        glue.contains(r#"from "/src/App.tsx?t=1700000000000""#),
        "{glue}"
    );
    assert!(glue.contains("validateRefreshBoundaryAndEnqueueUpdate"));
    assert!(glue.contains("function $RefreshReg$"));
}

// A module that reads import.meta.hot itself gets the hot-context banner
// PREPENDED (svelte_hot_glue); the appended refresh glue must then reuse
// that context, never re-import it — an import binding is a lexical
// declaration, and a second `__oj_createHotContext` in the same module
// scope is a SyntaxError that kills the module (seen on TanStack route
// files, whose router plugin injects its own import.meta.hot handler).
#[test]
fn glue_reuses_a_predefined_hot_context_instead_of_redeclaring_it() {
    let banner = svelte_hot_glue("/src/routes/a.tsx");
    let glue = hot_glue("/src/routes/a.tsx", Some("t=1700000000000"), true, true);
    assert!(!glue.contains("__oj_createHotContext"), "{glue}");
    assert!(glue.contains("registerExportsForReactRefresh"), "{glue}");
    let combined = format!("{banner}{glue}");
    assert_eq!(
        combined
            .matches("createHotContext as __oj_createHotContext")
            .count(),
        1,
        "exactly one declaration per module scope: {combined}"
    );
}

#[test]
fn glue_never_doubles_a_query_the_url_already_carries() {
    // serve_compiled keys per full url, so the real call hands hot_glue a url
    // that already has its query AND the same query again. The self-import
    // must not grow (`?t=X?t=X` grew per edit until hyper answered 414) and
    // the hot-context id must stay the clean path the server sends updates for.
    let glue = hot_glue(
        "/src/App.tsx?t=1700000000000",
        Some("t=1700000000000"),
        true,
        false,
    );
    assert!(
        glue.contains(r#"createHotContext("/src/App.tsx")"#),
        "{glue}"
    );
    assert!(
        glue.contains(r#"from "/src/App.tsx?t=1700000000000""#),
        "{glue}"
    );
    assert!(!glue.contains("?t=1700000000000?t=1700000000000"), "{glue}");
    assert!(
        glue.contains(r#"registerExportsForReactRefresh("/src/App.tsx","#),
        "{glue}"
    );
    // Only the hmr timestamp is stripped from the id; a semantic query that
    // makes a distinct module (router `?tsr-shared=1`) is kept in both.
    let v = hot_glue(
        "/src/r.tsx?tsr-shared=1&t=1700000000000",
        Some("tsr-shared=1&t=1700000000000"),
        true,
        false,
    );
    assert!(
        v.contains(r#"createHotContext("/src/r.tsx?tsr-shared=1")"#),
        "{v}"
    );
    assert!(
        v.contains(r#"from "/src/r.tsx?tsr-shared=1&t=1700000000000""#),
        "{v}"
    );
}

#[test]
fn restart_triggers_include_every_config_flavor() {
    for f in [
        "oj.config.ts",
        "oj.config.json",
        "vite.config.mts",
        ".env",
        ".env.staging",
        "postcss.config.cjs",
    ] {
        assert!(is_restart_trigger(Path::new(f)), "{f}");
    }
    for f in ["src/main.ts", "package.json", "config.json", "env.ts"] {
        assert!(!is_restart_trigger(Path::new(f)), "{f}");
    }
}

#[test]
fn tsconfig_files_trigger_the_reload_path() {
    for f in ["tsconfig.json", "app/tsconfig.json", "tsconfig.base.json"] {
        assert!(is_tsconfig_file(Path::new(f)), "{f}");
    }
    for f in ["package.json", "src/tsconfig.ts", "not-tsconfig.json"] {
        assert!(!is_tsconfig_file(Path::new(f)), "{f}");
    }
}

#[test]
fn error_frame_is_a_vite_error_payload() {
    let f: serde_json::Value = serde_json::from_str(&error_frame(
        "compile error:\nsrc/App.tsx:3:7 Unexpected token\n  | <div>",
    ))
    .unwrap();
    assert_eq!(f["type"], "error");
    assert_eq!(
        f["err"]["message"],
        "compile error:\nsrc/App.tsx:3:7 Unexpected token\n  | <div>"
    );
    assert_eq!(f["err"]["id"], "src/App.tsx");
    assert_eq!(f["err"]["loc"]["line"], 3);
    assert_eq!(f["err"]["loc"]["column"], 7);
    assert!(f["err"]["frame"]
        .as_str()
        .unwrap()
        .contains("Unexpected token"));
    let plain: serde_json::Value = serde_json::from_str(&error_frame("boom")).unwrap();
    assert!(plain["err"]["id"].is_null() && plain["err"]["frame"].is_null());
    let u = update_entry("css-update", "/a.css", 5);
    assert_eq!(u["acceptedPath"], "/a.css");
    assert_eq!(u["type"], "css-update");
}

#[test]
fn ws_proxy_target_and_header_rules() {
    assert_eq!(
        ws_target_url("http://localhost:4000"),
        "ws://localhost:4000"
    );
    assert_eq!(ws_target_url("https://api.test"), "wss://api.test");
    assert_eq!(
        ws_target_origin("wss://api.test:8443/socket?x=1"),
        "https://api.test:8443"
    );
    assert_eq!(
        ws_target_origin("ws://localhost:3000"),
        "http://localhost:3000"
    );
    assert_eq!(ws_target_url("ws://x:1"), "ws://x:1");
    assert!(ws_forwardable_header(&header::COOKIE));
    assert!(ws_forwardable_header(&header::SEC_WEBSOCKET_PROTOCOL));
    assert!(ws_forwardable_header(&header::ORIGIN));
    for h in [
        header::HOST,
        header::CONNECTION,
        header::UPGRADE,
        header::SEC_WEBSOCKET_KEY,
        header::SEC_WEBSOCKET_VERSION,
        header::SEC_WEBSOCKET_EXTENSIONS,
    ] {
        assert!(!ws_forwardable_header(&h), "{h}");
    }
    let mut h = HeaderMap::new();
    h.insert(header::UPGRADE, "WebSocket".parse().unwrap());
    assert!(is_websocket_upgrade(&h));
    assert!(!is_websocket_upgrade(&HeaderMap::new()));
}

#[test]
fn proxy_tls_configs_build_for_both_secure_settings() {
    assert!(proxy_tls_config(false).is_ok(), "accept-any config");
    assert!(proxy_tls_config(true).is_ok(), "platform verifier config");
}

#[test]
fn localhost_origin_default_matches_vite_regex() {
    for ok in [
        "http://localhost",
        "http://localhost:5173",
        "https://app.localhost:3000",
        "http://127.0.0.1:8080",
        "http://[::1]:5173",
    ] {
        assert!(is_localhost_origin(ok), "{ok}");
    }
    for bad in [
        "http://evil.com",
        "http://localhost.evil.com",
        "http://127.0.0.1.nip.io",
        "ftp://localhost",
        "http://localhost:abc",
    ] {
        assert!(!is_localhost_origin(bad), "{bad}");
    }
}

#[test]
fn host_policy_allows_localhost_ips_and_configured_hosts_only() {
    let server = oj_config::ServerConfig {
        allowed_hosts: Some(oj_config::AllowedHosts::List(vec![
            "app.test".into(),
            ".corp.example".into(),
        ])),
        ..Default::default()
    };
    let p = HostPolicy::from_config(&server, Some("dev.local"));
    for ok in [
        "localhost",
        "sub.localhost",
        "127.0.0.1",
        "[::1]",
        "10.0.0.5",
        "app.test",
        "APP.TEST",
        "corp.example",
        "x.corp.example",
        "dev.local",
    ] {
        assert!(p.hostname_allowed(ok), "{ok}");
    }
    for bad in ["evil.com", "notcorp.example", "app.test.evil", ""] {
        assert_eq!(p.hostname_allowed(bad), bad.is_empty(), "{bad}");
    }
    assert_eq!(
        HostPolicy::host_header_name("example.com:5173"),
        "example.com"
    );
    assert_eq!(HostPolicy::host_header_name("[::1]:5173"), "::1");
    assert_eq!(HostPolicy::host_header_name("example.com"), "example.com");
    let all = HostPolicy::from_config(
        &oj_config::ServerConfig {
            allowed_hosts: Some(oj_config::AllowedHosts::All(true)),
            ..Default::default()
        },
        None,
    );
    assert!(all.allow_all && all.hostname_allowed("evil.com"));
}

#[test]
fn cors_policy_forms() {
    let default = CorsPolicy::from_config(None).unwrap();
    assert!(default.allows("http://localhost:5173") && !default.allows("http://evil.com"));
    assert!(CorsPolicy::from_config(Some(&oj_config::CorsConfig::Toggle(false))).is_none());
    let any = CorsPolicy::from_config(Some(&oj_config::CorsConfig::Toggle(true))).unwrap();
    assert!(any.allows("http://evil.com"));
    let opts: oj_config::CorsOptions = serde_json::from_value(serde_json::json!({
        "origin": ["http://a.test", "http://b.test"], "credentials": true, "methods": ["GET", "POST"], "maxAge": 60
    }))
    .unwrap();
    let list =
        CorsPolicy::from_config(Some(&oj_config::CorsConfig::Options(Box::new(opts)))).unwrap();
    assert!(list.allows("http://a.test") && !list.allows("http://localhost:5173"));
    assert!(list.credentials && list.methods == "GET,POST" && list.max_age == Some(60));
}

#[test]
fn asset_requests_are_modules_only_for_imports() {
    let mut h = HeaderMap::new();
    assert!(!wants_module_import(&h, None), "a bare fetch gets the file");
    assert!(wants_module_import(&h, Some("import")));
    assert!(wants_module_import(&h, Some("t=1&import")));
    h.insert("sec-fetch-dest", "empty".parse().unwrap());
    assert!(!wants_module_import(&h, None));
    h.insert("sec-fetch-dest", "image".parse().unwrap());
    assert!(!wants_module_import(&h, None));
    h.insert("sec-fetch-dest", "script".parse().unwrap());
    assert!(wants_module_import(&h, None), "a module import of the url");
}

#[test]
fn watch_ignored_globs_match_relative_and_absolute_paths() {
    let root = Path::new("/app");
    let pats = watch_ignored_patterns(
        root,
        &[
            "**/generated/**".to_string(),
            "docs/*.md".to_string(),
            "/tmp/out/**".to_string(),
        ],
    );
    assert!(is_watch_ignored(
        &pats,
        root,
        Path::new("/app/src/generated/x.ts")
    ));
    assert!(
        is_watch_ignored(&pats, root, Path::new("/app/docs/intro.md")),
        "root-relative pattern"
    );
    assert!(
        !is_watch_ignored(&pats, root, Path::new("/app/docs/deep/intro.md")),
        "* stops at /"
    );
    assert!(
        is_watch_ignored(&pats, root, Path::new("/tmp/out/a/b.js")),
        "absolute pattern"
    );
    assert!(!is_watch_ignored(
        &pats,
        root,
        Path::new("/app/src/main.ts")
    ));
    assert!(!is_watch_ignored(
        &[],
        root,
        Path::new("/app/src/generated/x.ts")
    ));
}

#[test]
fn html_fallback_rewrites_like_vite() {
    assert_eq!(
        html_fallback_candidate("nested/").as_deref(),
        Some("nested/index.html")
    );
    assert_eq!(
        html_fallback_candidate("about").as_deref(),
        Some("about.html")
    );
    assert_eq!(
        html_fallback_candidate("docs/intro").as_deref(),
        Some("docs/intro.html")
    );
    assert_eq!(
        html_fallback_candidate("about.html"),
        None,
        "an explicit html request is not rewritten"
    );
    assert_eq!(html_fallback_candidate(""), None);
    let mut h = HeaderMap::new();
    assert!(accepts_html_fallback(&h), "no Accept is */*");
    h.insert(
        header::ACCEPT,
        "text/html,application/xhtml+xml".parse().unwrap(),
    );
    assert!(accepts_html_fallback(&h));
    h.insert(header::ACCEPT, "*/*".parse().unwrap());
    assert!(accepts_html_fallback(&h));
    h.insert(header::ACCEPT, "application/json".parse().unwrap());
    assert!(
        !accepts_html_fallback(&h),
        "an API-style request never gets html"
    );
}

#[test]
fn client_js_is_rendered_from_server_hmr_options() {
    let tpl = "a=__HMR_PROTOCOL__;b=__HMR_HOSTNAME__;c=__HMR_PORT__;d=__HMR_PATH__;e=__HMR_ENABLE_OVERLAY__;f=__WS_TOKEN__;";
    let fc_off = resolve_forward_console(None, &oj_env::Knobs::default());
    assert_eq!(hmr_socket_path(None), "/__ws");
    assert_eq!(
        render_client_js(tpl, None, "/__ws", "tok", &fc_off),
        r#"a=null;b=null;c=null;d="/__ws";e=true;f="tok";"#
    );
    let opts = oj_config::HmrOptions {
        path: Some("hmr".into()),
        port: Some(24678),
        client_port: Some(443),
        host: Some("app.test".into()),
        protocol: Some("wss".into()),
        overlay: Some(false),
        timeout: None,
    };
    let path = hmr_socket_path(Some(&opts));
    assert_eq!(path, "/hmr", "a relative hmr.path is made absolute");
    assert_eq!(
        render_client_js(tpl, Some(&opts), &path, "tok", &fc_off),
        r#"a="wss";b="app.test";c=443;d="/hmr";e=false;f="tok";"#,
        "clientPort, not port, is what the browser dials"
    );
    let real = render_client_js(CLIENT_JS, None, "/__ws", "tok", &fc_off);
    assert!(
        !real.contains("__HMR_")
            && !real.contains("__WS_TOKEN__")
            && !real.contains("__FORWARD_CONSOLE__"),
        "no placeholder left"
    );
}

#[test]
fn forward_console_resolves_like_vite() {
    // Unset, no agent environment: off.
    let plain = oj_env::Knobs::default();
    let off = resolve_forward_console(None, &plain);
    assert_eq!(off["enabled"], false);
    // An agent environment or the editor gate turns the default on.
    let mut agent = oj_env::Knobs {
        agent_env: true,
        ..Default::default()
    };
    let on = resolve_forward_console(None, &agent);
    assert_eq!(on["enabled"], true);
    assert_eq!(on["unhandledErrors"], true);
    assert_eq!(on["logLevels"], serde_json::json!(["error", "warn"]));
    let gated = oj_env::Knobs {
        hmr_gate: true,
        ..Default::default()
    };
    assert_eq!(resolve_forward_console(None, &gated)["enabled"], true);
    // The env override beats detection; the config beats the env override.
    agent.forward_console = Some(false);
    assert_eq!(resolve_forward_console(None, &agent)["enabled"], false);
    let cfg = serde_json::json!(true);
    assert_eq!(resolve_forward_console(Some(&cfg), &agent)["enabled"], true);
    // The object form: unhandledErrors defaults true; empty levels alone
    // still enable via unhandledErrors, and levels alone enable too.
    let obj = serde_json::json!({ "logLevels": ["warn"] });
    let r = resolve_forward_console(Some(&obj), &plain);
    assert_eq!(r["enabled"], true);
    assert_eq!(r["unhandledErrors"], true);
    assert_eq!(r["logLevels"], serde_json::json!(["warn"]));
    let neither = serde_json::json!({ "unhandledErrors": false });
    assert_eq!(
        resolve_forward_console(Some(&neither), &plain)["enabled"],
        false
    );
}

#[test]
fn ws_token_is_demanded_only_from_browser_upgrades() {
    let mut h = HeaderMap::new();
    assert!(
        !ws_token_rejected(true, "t0k", &h, None),
        "no Origin: not a browser"
    );
    h.insert(header::ORIGIN, "http://localhost:5199".parse().unwrap());
    assert!(ws_token_rejected(true, "t0k", &h, None));
    assert!(ws_token_rejected(true, "t0k", &h, Some("token=nope")));
    assert!(!ws_token_rejected(true, "t0k", &h, Some("token=t0k")));
    assert!(!ws_token_rejected(true, "t0k", &h, Some("a=1&token=t0k")));
    assert!(
        !ws_token_rejected(false, "t0k", &h, None),
        "legacy.skipWebSocketTokenCheck"
    );
    let token = new_ws_token();
    assert_eq!(token.len(), 32);
    assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_ne!(token, new_ws_token());
}

#[test]
fn stamp_import_url_marks_only_compilable_module_urls() {
    assert_eq!(stamp_import_url("/src/utils.ts", 5), "/src/utils.ts?t=5");
    assert_eq!(
        stamp_import_url("/src/r.tsx?tsr-shared=1", 5),
        "/src/r.tsx?tsr-shared=1&t=5"
    );
    assert_eq!(
        stamp_import_url("/src/utils.ts", 0),
        "/src/utils.ts",
        "unstamped module"
    );
    assert_eq!(
        stamp_import_url("/src/utils.ts?t=3", 5),
        "/src/utils.ts?t=3",
        "never doubled"
    );
    assert_eq!(
        stamp_import_url("/src/data.json", 5),
        "/src/data.json?t=5",
        "json is a module"
    );
    assert_eq!(
        stamp_import_url("/src/a.css?import", 5),
        "/src/a.css?import&t=5",
        "a CSS module's importer must fetch its new class exports"
    );
    assert_eq!(
        stamp_import_url("/src/a.css?inline", 5),
        "/src/a.css?inline&t=5"
    );
    assert_eq!(stamp_import_url("/logo.svg?url", 5), "/logo.svg?url");
    assert_eq!(
        stamp_import_url("/@oj-deps/react.js", 5),
        "/@oj-deps/react.js"
    );
    assert_eq!(
        stamp_import_url("/@fs/x/node_modules/a/index.js", 5),
        "/@fs/x/node_modules/a/index.js"
    );
}

#[test]
fn proxy_regex_contexts_match_path_and_query_like_vite() {
    let entries: Vec<(String, oj_config::ProxyEntry)> = [
        ("/api", "http://a"),
        ("/api/v2", "http://a2"),
        ("^/re/.*", "http://re"),
        ("^/search\\?q=", "http://q"),
        ("^(", "http://bad"),
    ]
    .iter()
    .map(|(c, t)| (c.to_string(), oj_config::ProxyEntry::Target(t.to_string())))
    .collect();
    let regexes: Vec<Option<regex::Regex>> = entries
        .iter()
        .map(|(c, _)| proxy_context_regex(c))
        .collect();
    assert!(regexes[0].is_none() && regexes[2].is_some());
    assert!(
        regexes[4].is_none(),
        "an invalid pattern degrades to a never-matching prefix"
    );
    let pick =
        |url: &str| select_proxy(&entries, &regexes, url).map(|(_, e)| e.target().to_string());
    assert_eq!(pick("/api/x"), Some("http://a".into()));
    assert_eq!(
        pick("/api/v2/x"),
        Some("http://a2".into()),
        "longest prefix wins"
    );
    assert_eq!(pick("/re/anything?x=1"), Some("http://re".into()));
    assert_eq!(
        pick("/search?q=oj"),
        Some("http://q".into()),
        "regex sees the query"
    );
    assert_eq!(pick("/search"), None);
    assert_eq!(pick("/other"), None);
    assert!(proxy_context_matches("^/re/.*", "/re/x"));
    assert!(
        !proxy_context_matches("^/re/.*", "/api/re/x"),
        "anchored, not a substring"
    );
    assert!(proxy_context_matches("/api", "/api/x?y=1"));
}

#[test]
fn unresolved_import_error_points_at_the_specifier() {
    let root = Path::new("/app");
    let file = Path::new("/app/src/App.tsx");
    let source = "import { useState } from \"react\";\nimport { Later } from './Later';\n";
    let err = unresolved_import_error(root, file, source, "./Later");
    assert!(is_unresolved_import_error(&err));
    assert!(
        err.contains("src/App.tsx:2:24 Failed to resolve import \"./Later\" from \"src/App.tsx\". Does the file exist?"),
        "{err}"
    );
    assert!(
        err.contains("   2 | import { Later } from './Later';"),
        "{err}"
    );
    // The overlay's ErrorPayload lifts the location out of the message.
    let frame: serde_json::Value = serde_json::from_str(&error_frame(&err)).unwrap();
    assert_eq!(frame["err"]["id"], "src/App.tsx");
    assert_eq!(frame["err"]["loc"]["line"], 2);
    assert_eq!(frame["err"]["loc"]["column"], 24);
    // A specifier not found verbatim (plugin-rewritten source) still errors.
    let err = unresolved_import_error(root, file, "export {};\n", "./gone");
    assert!(
        err.contains("src/App.tsx:1:1 Failed to resolve import \"./gone\""),
        "{err}"
    );
    assert!(!is_unresolved_import_error(
        "compile error:\nparse error in x.tsx"
    ));
}

#[test]
fn preload_hints_name_the_exact_import_url() {
    assert_eq!(preload_href("/src/main.tsx", "abcd1234"), "/src/main.tsx");
    assert_eq!(preload_href("/src/a.css", "abcd1234"), "/src/a.css?import");
    // Optimized deps and package bundles preload under their versioned URL,
    // so the preload and the later import hit one immutable cache entry.
    assert_eq!(
        preload_href("/@oj-deps/react.mjs", "abcd1234"),
        "/@oj-deps/react.mjs?v=abcd1234"
    );
    assert_eq!(
        preload_href("/@oj-deps/react.mjs", ""),
        "/@oj-deps/react.mjs",
        "no version, no query"
    );
    assert_eq!(
        preload_href("/@id/6e6f6465", "abcd1234"),
        "/@id/6e6f6465",
        "stubs are unversioned"
    );
}

#[test]
fn optional_peer_dep_resolves_to_a_lazy_error_stub() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let parent = root.join("node_modules/devtools-hook");
    std::fs::create_dir_all(parent.join("lib")).unwrap();
    std::fs::write(
        parent.join("package.json"),
        r#"{"name":"devtools-hook","peerDependencies":{"react":">=18","@scope/opt":"*","required-peer":"*"},
            "peerDependenciesMeta":{"react":{"optional":false},"@scope/opt":{"optional":true}}}"#,
    )
    .unwrap();
    // The lookup runs from the importing file's directory inside the package.
    let from = parent.join("lib");
    let url = optional_peer_dep_url(&root, &from, "@scope/opt/sub").expect("optional peer");
    assert!(url.starts_with(OPTIONAL_PEER_PREFIX), "{url}");
    let stub = optional_peer_dep_stub(url.strip_prefix(OPTIONAL_PEER_PREFIX).unwrap()).unwrap();
    assert!(stub.contains("Could not resolve \"${\"@scope/opt/sub\"}\" imported by \"${\"devtools-hook\"}\". Is it installed?"), "{stub}");
    assert!(
        stub.contains("throw new Error("),
        "errors when evaluated: {stub}"
    );
    // A declared but non-optional peer, an undeclared package, a builtin, and
    // an import from the app root itself all fall through to the normal error.
    assert!(
        optional_peer_dep_url(&root, &from, "react").is_none(),
        "optional: false"
    );
    assert!(
        optional_peer_dep_url(&root, &from, "required-peer").is_none(),
        "no meta"
    );
    assert!(optional_peer_dep_url(&root, &from, "unknown-pkg").is_none());
    assert!(optional_peer_dep_url(&root, &from, "node:fs").is_none());
    assert!(optional_peer_dep_url(&root, &from, "./local").is_none());
    std::fs::write(root.join("package.json"), r#"{"name":"app","peerDependencies":{"x":"*"},"peerDependenciesMeta":{"x":{"optional":true}}}"#).unwrap();
    assert!(
        optional_peer_dep_url(&root, &root, "x").is_none(),
        "root has no peer deps (Vite: basedir !== root)"
    );
    assert!(optional_peer_dep_stub("zz").is_none(), "malformed id");
}

#[test]
fn bare_import_unresolved_only_for_packages_nothing_answers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("node_modules/real")).unwrap();
    std::fs::write(
        root.join("node_modules/real/package.json"),
        r#"{"name":"real","main":"index.js"}"#,
    )
    .unwrap();
    std::fs::write(
        root.join("node_modules/real/index.js"),
        "module.exports = 1;\n",
    )
    .unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{"name":"app","browser":{"mapped-off":false}}"#,
    )
    .unwrap();
    let resolver = OjResolver::new(root);
    // Vite's importAnalysis fails the importer for a package that is not
    // installed (typo, missing install), subpaths included.
    assert!(bare_import_unresolved(root, &resolver, "not-installed-pkg"));
    assert!(bare_import_unresolved(
        root,
        &resolver,
        "@scope/not-installed/sub"
    ));
    assert!(
        bare_import_unresolved(root, &resolver, "not-installed-pkg?url"),
        "query dropped"
    );
    // Everything something else answers is not an error here.
    assert!(
        !bare_import_unresolved(root, &resolver, "real"),
        "installed"
    );
    assert!(
        !bare_import_unresolved(root, &resolver, "node:fs"),
        "builtin stub"
    );
    assert!(
        !bare_import_unresolved(root, &resolver, "fs"),
        "builtin stub"
    );
    assert!(
        !bare_import_unresolved(root, &resolver, "virtual:thing"),
        "plugin virtual"
    );
    assert!(
        !bare_import_unresolved(root, &resolver, "\0resolved"),
        "plugin resolved id"
    );
    assert!(
        !bare_import_unresolved(root, &resolver, "data:text/javascript,export{}"),
        "data url"
    );
    assert!(
        !bare_import_unresolved(root, &resolver, "https://cdn.example/x.js"),
        "external url"
    );
    assert!(
        !bare_import_unresolved(root, &resolver, "./nope"),
        "relative is the other check"
    );
    assert!(
        !bare_import_unresolved(root, &resolver, "/src/nope.ts"),
        "root-absolute"
    );
    assert!(
        !bare_import_unresolved(root, &resolver, "@lingui/macro"),
        "shimmed macro entry"
    );
}

#[test]
fn relative_import_missing_only_for_unresolvable_relative_specifiers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/widgets")).unwrap();
    std::fs::write(root.join("src/util.ts"), "export const u = 1;\n").unwrap();
    std::fs::write(root.join("src/widgets/index.ts"), "export const w = 1;\n").unwrap();
    let resolver = OjResolver::new(root);
    let src = root.join("src");
    assert!(relative_import_missing(&src, &resolver, "./nope"));
    assert!(relative_import_missing(&src, &resolver, "../nope.js"));
    assert!(
        !relative_import_missing(&src, &resolver, "./util"),
        "extension probing"
    );
    assert!(!relative_import_missing(&src, &resolver, "./util.ts"));
    assert!(
        !relative_import_missing(&src, &resolver, "./widgets"),
        "directory index"
    );
    assert!(
        !relative_import_missing(&src, &resolver, "./util.ts?worker&inline"),
        "query dropped"
    );
    assert!(
        !relative_import_missing(&src, &resolver, "react"),
        "bare specifiers are not ours"
    );
    assert!(
        !relative_import_missing(&src, &resolver, "/src/nope.ts"),
        "root-absolute is not ours"
    );
    // A miss cached by the resolver clears once the file exists.
    assert!(relative_import_missing(&src, &resolver, "./later"));
    std::fs::write(root.join("src/later.ts"), "export {};\n").unwrap();
    resolver.clear_cache();
    assert!(!relative_import_missing(&src, &resolver, "./later"));
}

#[test]
fn strip_hmr_timestamp_removes_only_the_t_param() {
    assert_eq!(strip_hmr_timestamp("/src/App.tsx"), "/src/App.tsx");
    assert_eq!(
        strip_hmr_timestamp("/src/App.tsx?t=1700000000000"),
        "/src/App.tsx"
    );
    assert_eq!(
        strip_hmr_timestamp("/a.tsx?tsr-shared=1&t=1700000000000"),
        "/a.tsx?tsr-shared=1"
    );
    assert_eq!(
        strip_hmr_timestamp("/a.tsx?t=1700000000000&tsr-shared=1"),
        "/a.tsx?tsr-shared=1"
    );
    // Only a 13-digit millisecond timestamp is oj's `t=` (Vite's timestampRE is
    // /\bt=\d{13}&?\b/). A short numeric or non-numeric `t=` is a user's own
    // query and must be kept.
    assert_eq!(strip_hmr_timestamp("/a.tsx?t=9"), "/a.tsx?t=9");
    assert_eq!(strip_hmr_timestamp("/a.tsx?t=123"), "/a.tsx?t=123");
    assert_eq!(strip_hmr_timestamp("/a.tsx?t=abc"), "/a.tsx?t=abc");
    assert_eq!(strip_hmr_timestamp("/a.tsx?type=x"), "/a.tsx?type=x");
}

#[test]
fn resolve_host_maps_wildcards_to_all_interfaces() {
    let any: std::net::IpAddr = [0, 0, 0, 0].into();
    let local: std::net::IpAddr = [127, 0, 0, 1].into();
    assert_eq!(resolve_host(Some("true")), any);
    assert_eq!(resolve_host(Some("0.0.0.0")), any);
    assert_eq!(resolve_host(Some("::")), any);
    assert_eq!(resolve_host(Some("[::]")), any);
    assert_eq!(resolve_host(Some("localhost")), local);
    assert_eq!(resolve_host(None), local);
    let lan: std::net::IpAddr = [192, 168, 1, 5].into();
    assert_eq!(resolve_host(Some("192.168.1.5")), lan);
    assert_eq!(resolve_host(Some("bogus")), local);
}

#[test]
fn normalize_resolves_parent_components() {
    assert_eq!(
        normalize(Path::new("/a/b/../c/./d.ts")),
        PathBuf::from("/a/c/d.ts")
    );
}

#[test]
fn preview_html_fallback_prefers_page_index_then_sibling_then_root() {
    let dir = std::env::temp_dir().join(format!("oj-preview-fb-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("nested")).unwrap();
    std::fs::write(dir.join("index.html"), "root").unwrap();
    std::fs::write(dir.join("nested/index.html"), "nested").unwrap();
    std::fs::write(dir.join("about.html"), "about").unwrap();
    assert_eq!(
        preview_html_fallback(&dir, "nested/", true),
        Some(dir.join("nested/index.html"))
    );
    assert_eq!(
        preview_html_fallback(&dir, "nested", true),
        Some(dir.join("nested/index.html"))
    );
    assert_eq!(
        preview_html_fallback(&dir, "about", true),
        Some(dir.join("about.html"))
    );
    assert_eq!(
        preview_html_fallback(&dir, "missing/route", true),
        Some(dir.join("index.html"))
    );
    assert_eq!(
        preview_html_fallback(&dir, "", true),
        Some(dir.join("index.html"))
    );
    // appType mpa: `/x.html` and `/x/index.html` still resolve, nothing falls back to the root page.
    assert_eq!(
        preview_html_fallback(&dir, "about", false),
        Some(dir.join("about.html"))
    );
    assert_eq!(
        preview_html_fallback(&dir, "nested", false),
        Some(dir.join("nested/index.html"))
    );
    assert_eq!(preview_html_fallback(&dir, "missing/route", false), None);
    assert_eq!(
        preview_html_fallback(&dir, "", false),
        Some(dir.join("index.html"))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn preview_rel_maps_base_and_guards_traversal() {
    assert_eq!(preview_rel("/", "/").as_deref(), Some("index.html"));
    assert_eq!(
        preview_rel("/assets/x.js", "/").as_deref(),
        Some("assets/x.js")
    );
    assert_eq!(
        preview_rel("/app/assets/x.js", "/app/").as_deref(),
        Some("assets/x.js")
    );
    assert_eq!(preview_rel("/app/", "/app/").as_deref(), Some("index.html"));
    assert_eq!(preview_rel("/../etc/passwd", "/"), None);
}

#[test]
fn html_entry_src_normalizes_relative_and_excludes_external() {
    assert_eq!(
        html_entry_src("src/index.tsx").as_deref(),
        Some("/src/index.tsx")
    );
    assert_eq!(
        html_entry_src("./src/index.tsx").as_deref(),
        Some("/src/index.tsx")
    );
    assert_eq!(
        html_entry_src("/src/index.tsx").as_deref(),
        Some("/src/index.tsx")
    );
    assert_eq!(html_entry_src("https://cdn/x.js"), None);
    assert_eq!(html_entry_src("//cdn/x.js"), None);
    assert_eq!(html_entry_src("data:text/js,1"), None);
}

#[test]
fn csp_nonce_stamps_scripts_styles_and_preload_links_once() {
    let html = "<html><head>\
        <link rel=\"stylesheet\" href=\"/a.css\">\
        <link rel=\"icon\" href=\"/i.png\">\
        <link rel=\"modulepreload\" href=\"/m.js\" />\
        <style>.a{color:red}</style>\
        <script nonce=\"keep\">if (1 < 2) {}</script>\
        </head><body><script type='module' src='/main.js'></script><p>a < b</p></body></html>";
    let out = inject_csp_nonce(html, "n0nce");
    assert!(
        out.contains("<link rel=\"stylesheet\" href=\"/a.css\" nonce=\"n0nce\">"),
        "{out}"
    );
    assert!(
        out.contains("<link rel=\"icon\" href=\"/i.png\">"),
        "non-preload links untouched: {out}"
    );
    assert!(
        out.contains("<link rel=\"modulepreload\" href=\"/m.js\" nonce=\"n0nce\" />"),
        "{out}"
    );
    assert!(
        out.contains("<style nonce=\"n0nce\">.a{color:red}</style>"),
        "{out}"
    );
    assert!(
        out.contains("<script nonce=\"keep\">if (1 < 2) {}</script>"),
        "existing nonce kept: {out}"
    );
    assert!(
        out.contains("<script type='module' src='/main.js' nonce=\"n0nce\">"),
        "{out}"
    );
    assert!(
        out.contains("<head>\n<meta property=\"csp-nonce\" nonce=\"n0nce\">"),
        "{out}"
    );
    assert!(out.contains("<p>a < b</p>"), "{out}");
    assert_eq!(out.matches("csp-nonce").count(), 1);
    // Idempotent: a second pass adds nothing.
    assert_eq!(inject_csp_nonce(&out, "n0nce"), out);
}

#[test]
fn html_entries_read_module_scripts_with_any_quoting() {
    let root = std::env::temp_dir().join(format!("oj-html-entries-quoting-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("index.html"),
        "<html><body>\
         <script type='module' src='/src/a.ts'></script>\
         <script type=module src=src/b.ts></script>\
         <script type = \"module\" data-src=\"/ignored.js\" src = \"./src/c.ts\"></script>\
         <script src=\"/legacy.js\"></script>\
         <script type=\"module\">inline()</script>\
         </body></html>",
    )
    .unwrap();
    assert_eq!(
        html_entries(&root),
        vec![
            "/src/a.ts".to_string(),
            "/src/b.ts".to_string(),
            "/src/c.ts".to_string()
        ]
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn spa_navigation_falls_back_only_for_routes() {
    let html = {
        let mut h = HeaderMap::new();
        h.insert(
            header::ACCEPT,
            "text/html,application/xhtml+xml".parse().unwrap(),
        );
        h
    };
    let empty = HeaderMap::new();
    assert!(is_spa_navigation("dashboard", &empty));
    assert!(is_spa_navigation("users/123/edit", &empty));
    assert!(is_spa_navigation("report.v2", &html));
    assert!(!is_spa_navigation("missing.png", &empty));
    assert!(!is_spa_navigation("assets/app.js", &empty));
    assert!(!is_spa_navigation("@vite/client", &html));
    assert!(!is_spa_navigation("src/does-not-exist.tsx", &html));
    assert!(!is_spa_navigation("node_modules/react/missing.js", &html));
}

// The persisted child registry: a dead owner's identity-verified children are
// SIGKILLed and the file dropped; a live owner's file is untouched; a child
// whose recorded start time no longer matches (recycled pid) is never signaled.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn sweep_reaps_only_identified_children_of_dead_owners() {
    use std::process::Command;

    let dir = tempfile::tempdir().unwrap();

    // A pid that is certainly dead: spawn and fully reap a child.
    let dead = {
        let mut c = Command::new("true").spawn().unwrap();
        c.wait().unwrap();
        c.id()
    };
    let mut sleeper = Command::new("sleep").arg("60").spawn().unwrap();
    let mut survivor = Command::new("sleep").arg("60").spawn().unwrap();
    let start = child_groups::proc_start_time(sleeper.id()).unwrap();
    let survivor_start = child_groups::proc_start_time(survivor.id()).unwrap();

    // Dead owner: the identified sleeper dies, the mismatched one survives.
    let stale = dir.path().join("1.json");
    std::fs::write(
        &stale,
        serde_json::json!({
            "owner": { "pid": dead, "start": 1 },
            "children": [
                { "pid": sleeper.id(), "start": start, "own_group": false },
                { "pid": survivor.id(), "start": survivor_start + 1, "own_group": false },
            ],
        })
        .to_string(),
    )
    .unwrap();
    // Live owner (us): its file and children must be left alone entirely.
    let own = std::process::id();
    let live = dir.path().join("2.json");
    std::fs::write(
        &live,
        serde_json::json!({
            "owner": { "pid": own, "start": child_groups::proc_start_time(own) },
            "children": [{ "pid": survivor.id(), "start": survivor_start, "own_group": false }],
        })
        .to_string(),
    )
    .unwrap();

    assert_eq!(child_groups::sweep_stale(dir.path()), 1);
    assert!(!stale.exists(), "dead owner's file must be removed");
    assert!(live.exists(), "live owner's file must be kept");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = sleeper.try_wait().unwrap() {
            assert!(
                !status.success(),
                "sleeper must die from the sweep's SIGKILL"
            );
            break;
        }
        assert!(std::time::Instant::now() < deadline, "sleeper not killed");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        survivor.try_wait().unwrap().is_none(),
        "mismatched identity must survive"
    );
    survivor.kill().unwrap();
    survivor.wait().unwrap();
}

// Start-time identity: stable for a live process, gone once the pid is reaped.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn proc_start_time_identifies_live_processes() {
    let own = std::process::id();
    let a = child_groups::proc_start_time(own).unwrap();
    let b = child_groups::proc_start_time(own).unwrap();
    assert_eq!(a, b, "identity must be stable across reads");
    let mut gone = std::process::Command::new("true").spawn().unwrap();
    let pid = gone.id();
    gone.wait().unwrap();
    assert_eq!(child_groups::proc_start_time(pid), None);
}

// An unreadable registry file is dropped without signaling anything; our own
// process's file is never touched by a sweep.
#[test]
fn sweep_drops_unreadable_files_and_skips_our_own() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("7.json");
    std::fs::write(&bad, "not json").unwrap();
    let ours = dir.path().join(format!("{}.json", std::process::id()));
    std::fs::write(&ours, "also not json").unwrap();
    assert_eq!(child_groups::sweep_stale(dir.path()), 0);
    assert!(!bad.exists(), "unreadable stale file must be dropped");
    assert!(ours.exists(), "our own file must never be swept");
}

// The registry round trip behind the deno_process hook: init writes our file,
// register records the child with its identity, unregister removes it, and
// kill_all persists the drained state. The one test allowed to init_registry
// (the process-global registry path is set once).
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn child_registry_lifecycle_persists_and_drains() {
    let dir = tempfile::tempdir().unwrap();
    child_groups::init_registry(dir.path());
    let own_file = dir.path().join(format!("{}.json", std::process::id()));
    let read = || -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(&own_file).unwrap()).unwrap()
    };
    assert!(own_file.exists(), "init must write our registry file");
    assert_eq!(
        read()["owner"]["pid"].as_u64(),
        Some(std::process::id() as u64)
    );
    assert!(
        read()["owner"]["start"].as_u64().is_some(),
        "owner identity recorded"
    );

    let mut sleeper = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let pid = sleeper.id();
    child_groups::register(pid, false);
    let doc = read();
    let kid = doc["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["pid"].as_u64() == Some(pid as u64))
        .expect("registered child must be persisted");
    assert_eq!(kid["own_group"].as_bool(), Some(false));
    assert_eq!(
        kid["start"].as_u64(),
        child_groups::proc_start_time(pid),
        "child identity recorded"
    );

    child_groups::unregister(pid);
    let no_entry = |doc: serde_json::Value| {
        doc["children"]
            .as_array()
            .unwrap()
            .iter()
            .all(|k| k["pid"].as_u64() != Some(pid as u64))
    };
    assert!(
        no_entry(read()),
        "unregister must remove the persisted entry"
    );

    child_groups::register(pid, false);
    assert!(
        child_groups::kill_all() >= 1,
        "kill_all must kill the sleeper"
    );
    assert!(
        !own_file.exists(),
        "a drained kill_all must remove our registry file"
    );
    child_groups::register(std::process::id(), false);
    assert!(
        own_file.exists(),
        "a register after kill_all must recreate the file"
    );
    child_groups::unregister(std::process::id());
    drop(sleeper.try_wait());
}
