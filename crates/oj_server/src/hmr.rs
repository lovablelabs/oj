use super::*;

pub(crate) async fn ws_upgrade(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    uri: Uri,
    upgrade: WebSocketUpgrade,
) -> Response {
    if let Some(resp) = state.host_policy.reject_ws_origin(&headers) {
        return resp;
    }
    if ws_token_rejected(state.ws_token_check, &state.ws_token, &headers, uri.query()) {
        return (
            StatusCode::UNAUTHORIZED,
            "oj: websocket token missing or invalid",
        )
            .into_response();
    }
    hmr_socket(upgrade, state, false)
}

pub(crate) fn vite_ws_subprotocol(h: &HeaderMap) -> Option<&'static str> {
    let is_ws = h
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);
    if !is_ws {
        return None;
    }
    let raw = h.get(header::SEC_WEBSOCKET_PROTOCOL)?.to_str().ok()?;
    raw.split(',').map(|p| p.trim()).find_map(|p| match p {
        "vite-hmr" => Some("vite-hmr"),
        "vite-ping" => Some("vite-ping"),
        _ => None,
    })
}

pub(crate) async fn vite_hmr_upgrade(
    State(state): State<Arc<ServerState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let Some(proto) = vite_ws_subprotocol(req.headers()) else {
        return next.run(req).await;
    };
    if let Some(resp) = state.host_policy.reject_ws_origin(req.headers()) {
        return resp;
    }
    if proto != "vite-ping"
        && ws_token_rejected(
            state.ws_token_check,
            &state.ws_token,
            req.headers(),
            req.uri().query(),
        )
    {
        return (
            StatusCode::UNAUTHORIZED,
            "oj: websocket token missing or invalid",
        )
            .into_response();
    }
    let (mut parts, body) = req.into_parts();
    match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        Ok(upgrade) if proto == "vite-ping" => {
            upgrade
                .protocols(["vite-ping"])
                .on_upgrade(|mut socket| async move {
                    let _ = socket.send(Message::Close(None)).await;
                })
        }
        Ok(upgrade) => hmr_socket(upgrade, state, true),
        Err(_) => {
            next.run(axum::extract::Request::from_parts(parts, body))
                .await
        }
    }
}

pub(crate) fn hmr_socket(upgrade: WebSocketUpgrade, state: Arc<ServerState>, vite: bool) -> Response {
    let upgrade = if vite {
        upgrade.protocols(["vite-hmr"])
    } else {
        upgrade
    };
    upgrade.on_upgrade(move |mut socket| async move {
        let mut rx = state.reload_tx.subscribe();
        if vite {
            let connected = serde_json::json!({ "type": "connected" }).to_string();
            let _ = socket.send(Message::Text(connected.into())).await;
        }
        // Vite's ws server emits "connection" per accepted client; plugins that
        // push initial state from `server.ws.on("connection")` need the event.
        if let Some(host) = state.plugins.clone() {
            tokio::spawn(async move {
                let _ = host.ws_connection().await;
            });
        }
        let buffered = state.buffered_error.lock().unwrap().take();
        if let Some(frame) = buffered {
            let _ = socket.send(Message::Text(frame.into())).await;
        }
        if state.hmr_gate.is_some() {
            let mode = serde_json::json!({
                "type": "custom",
                "event": "lovable:dev-server-mode",
                "data": { "mode": "classic" },
            })
            .to_string();
            let _ = socket.send(Message::Text(mode.into())).await;
            let boot = boot_progress_frame(0, state.preload_snapshot.len(), Some(0));
            let _ = socket.send(Message::Text(boot.into())).await;
        }
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Ok(text) => {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                incoming = socket.recv() => match incoming {
                    None | Some(Err(_)) => break,
                    Some(Ok(Message::Text(text))) => handle_client_message(&state, &text),
                    Some(Ok(_)) => {}
                },
            }
        }
    })
}

/// A plugin drove `server.moduleGraph.invalidateModule(...)` or `server.restart()`
/// from the host. Invalidation drops the module's compiled output (so its next
/// request recompiles, re-running plugin transforms) and, for a file in the app,
/// propagates an HMR update exactly as a change to that file would; a virtual id
/// only loses its cache, as in Vite (plugins push their own ws message then).
pub(crate) async fn handle_plugin_server_event(state: &Arc<ServerState>, ev: &serde_json::Value) {
    match ev.get("action").and_then(|a| a.as_str()) {
        Some("restart") => {
            println!("oj: plugin requested server.restart()");
            restart_process();
        }
        Some("invalidateAll") => {
            state.mtime_keys.lock().unwrap().clear();
            state.memory.lock().unwrap().clear();
            let _ = state
                .reload_tx
                .send(full_reload_frame("plugin invalidateAll", None, None));
        }
        Some("invalidate") => {
            let Some(id) = ev.get("id").and_then(|i| i.as_str()) else {
                return;
            };
            let clean = id.split('?').next().unwrap_or(id);
            let path = Path::new(clean);
            let url = if path.is_absolute() && path.starts_with(&state.root) {
                url_of(&state.root, path)
            } else if clean.starts_with('/') {
                clean.to_string()
            } else {
                return;
            };
            state.mtime_keys.lock().unwrap().remove(&url);
            state.memory.lock().unwrap().remove(&url);
            if path.is_file() && state.graph.lock().unwrap().contains(Path::new(&url)) {
                for message in decide(state, &[path.to_path_buf()], &Default::default()).await {
                    let _ = state.reload_tx.send(message);
                }
            }
        }
        _ => {}
    }
}

pub(crate) fn handle_client_message(state: &Arc<ServerState>, text: &str) {
    let Ok(msg) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    // Vite's client sends `import.meta.hot.invalidate()` as the custom event
    // `vite:invalidate` (`{path, message, firstInvalidatedBy}`).
    if msg["type"] == "custom" && msg["event"] == "vite:invalidate" {
        let body = &msg["data"];
        let Some(path) = body["path"].as_str() else {
            return;
        };
        let first_invalidated_by = body["firstInvalidatedBy"].as_str();
        let reply = {
            let timestamp = now_millis() as u64;
            let (dirty, targets) = {
                let mut graph = state.graph.lock().unwrap();
                // Only a self-accepting module an update touched, once per update
                // (Vite's lastHMRInvalidationReceived); its importers are stamped
                // so the boundary's re-fetch sees the invalidated module's version.
                let Some(dirty) = graph.accept_invalidation(Path::new(path), timestamp) else {
                    println!("oj: invalidate {path} ignored (no pending update)");
                    return;
                };
                (dirty, graph.update_targets_from_importers(Path::new(path)))
            };
            {
                let mut keys = state.mtime_keys.lock().unwrap();
                for d in &dirty {
                    keys.remove(&d.display().to_string());
                }
            }
            match targets {
                // The invalidate came back around to the module that started the
                // chain: no importer can hot update it, so reload (Vite's
                // 'circular import invalidate').
                Ok(targets)
                    if first_invalidated_by.is_some_and(|first| {
                        targets
                            .iter()
                            .any(|t| t.accepted.display().to_string() == first)
                    }) =>
                {
                    let reason = "circular import invalidate";
                    println!("oj: invalidate {path} -> full-reload ({reason})");
                    full_reload_frame(reason, None, None)
                }
                Ok(targets) => {
                    let boundaries: Vec<&Path> =
                        targets.iter().map(|t| t.boundary.as_path()).collect();
                    println!("oj: invalidate {path} -> update {boundaries:?}");
                    let first = first_invalidated_by.unwrap_or(path);
                    let updates: Vec<_> = targets
                        .iter()
                        .map(|t| update_entry_for(t, timestamp, Some(first)))
                        .collect();
                    serde_json::json!({ "type": "update", "updates": updates }).to_string()
                }
                Err(reason) => {
                    println!("oj: invalidate {path} -> full-reload ({reason})");
                    full_reload_frame(&reason, None, None)
                }
            }
        };
        let _ = state.reload_tx.send(reply);
    } else if msg["type"] == "custom" && msg["event"].is_string() {
        let _ = state.reload_tx.send(
            serde_json::json!({
                "type": "custom",
                "event": msg["event"],
                "data": msg["data"],
            })
            .to_string(),
        );
        if let Some(host) = state.plugins.clone() {
            let event = msg["event"].as_str().unwrap_or_default().to_string();
            let data = msg["data"].to_string();
            tokio::spawn(async move {
                let _ = host.ws_message(&event, &data).await;
            });
        }
    }
}

/// The id a module's hot context and Fast Refresh registration use: its url with
/// oj's HMR cache-busting `t=<timestamp>` removed but every other query kept, so a
/// `?tsr-shared=1` variant stays a distinct module while the id is stable across
/// updates. It must match the clean path the server names in its update messages;
/// a timestamped id would never match, so accept callbacks would never fire.
/// Mirrors Vite's `removeTimestampQuery`.
pub(crate) fn strip_hmr_timestamp(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };
    // oj's HMR timestamp is `now_millis()`: exactly 13 digits. Match only that,
    // as Vite's `timestampRE` (`/\bt=\d{13}&?\b/`) does, so a user's own short or
    // non-numeric `t=` query is never mistaken for it and stripped.
    let kept: Vec<&str> = query
        .split('&')
        .filter(|kv| {
            !(kv.starts_with("t=") && kv.len() == 15 && kv[2..].bytes().all(|b| b.is_ascii_digit()))
        })
        .collect();
    if kept.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", kept.join("&"))
    }
}

/// Append the HMR timestamp to a served module url when its module has one. Only
/// modules served as JS are stamped (compilable sources, JSON, and stylesheet
/// wrappers, which Vite's importAnalysis also stamps so an edited data file or
/// CSS module is re-fetched rather than read from the browser's module cache):
/// asset (`?url`, `?raw`) and oj-internal (`/@oj-deps/`, `/@fs/`, `/@id/`, ...)
/// urls are left alone, since deps never take part in HMR.
pub(crate) fn stamp_import_url(url: &str, timestamp: u64) -> String {
    if timestamp == 0 || url.starts_with("/@") || !url.starts_with('/') {
        return url.to_string();
    }
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if !COMPILABLE.contains(&ext) && ext != "json" && !is_style_ext(ext) {
        return url.to_string();
    }
    if query.split('&').any(|kv| kv.starts_with("t=")) {
        return url.to_string();
    }
    if query.is_empty() {
        format!("{path}?t={timestamp}")
    } else {
        format!("{url}&t={timestamp}")
    }
}

/// `ctx_predefined`: the served body already carries the hot-context banner
/// (serve_compiled prepends it when the module reads `import.meta.hot`
/// itself). The glue must then REUSE `import.meta.hot` — as Vite's refresh
/// footer reuses the import-analysis banner — never re-import: an import
/// binding is a lexical declaration, and a second
/// `import {{ createHotContext as __oj_createHotContext }}` in the same
/// module scope is a SyntaxError that kills the whole module.
pub(crate) fn hot_glue(url: &str, query: Option<&str>, is_boundary: bool, ctx_predefined: bool) -> String {
    if !is_boundary {
        return String::new();
    }
    // serve_compiled keys modules per full url, so `url` usually already carries
    // its query and `query` repeats it; it can also arrive clean with the query
    // separate. Either way the self-import must name exactly the module being
    // served (keeping its `t=` so the browser dedupes to the running instance) and
    // must never re-append a query the url already has. Doing so once per edit
    // grew the url without bound (`?t=X?t=Y...`) until hyper answered 414.
    let self_specifier = match query {
        Some(q) if !q.is_empty() && !url.contains('?') => format!("{url}?{q}"),
        _ => url.to_string(),
    };
    let id = strip_hmr_timestamp(&self_specifier);
    let ctx = if ctx_predefined {
        String::new()
    } else {
        format!(
            "import {{ createHotContext as __oj_createHotContext }} from \"/@oj/client.js\";\nimport.meta.hot ??= __oj_createHotContext({id:?});\n"
        )
    };
    format!(
        r#"
{ctx}import * as RefreshRuntime from "/@oj/refresh-runtime.js";
import * as __oj_currentExports from {self_specifier:?};
if (import.meta.hot) {{
  if (!window.__oj_refresh_installed__) {{
    throw new Error("oj: Fast Refresh preamble missing; was index.html served by oj?");
  }}
  const currentExports = __oj_currentExports;
  RefreshRuntime.registerExportsForReactRefresh({id:?}, currentExports);
  import.meta.hot.accept((nextExports) => {{
    if (!nextExports) return;
    const invalidateMessage = RefreshRuntime.validateRefreshBoundaryAndEnqueueUpdate({id:?}, currentExports, nextExports);
    if (invalidateMessage) import.meta.hot.invalidate(invalidateMessage);
  }});
}}
function $RefreshReg$(type, id) {{ return RefreshRuntime.register(type, {id:?} + " " + id); }}
function $RefreshSig$() {{ return RefreshRuntime.createSignatureFunctionForTransform(); }}
"#
    )
}

/// Vite's `ErrorPayload` (`{type:'error', err:{message, stack, id, loc, frame, plugin}}`).
/// oj's messages are "title\n<file>:<line>:<col>...\nframe" style text; the file
/// location is lifted into `id`/`loc` so a Vite-protocol overlay shows it.
/// Broadcast an error frame to the connected clients, or hold it for the next
/// one when none is connected yet (Vite's ws server does the same: a page whose
/// first module request 500s has not opened its socket by then, and without the
/// buffered frame it would show a blank page instead of the overlay).
pub(crate) fn send_error(state: &ServerState, message: &str) {
    let frame = error_frame(message);
    if state.reload_tx.receiver_count() == 0 {
        *state.buffered_error.lock().unwrap() = Some(frame);
    } else {
        let _ = state.reload_tx.send(frame);
    }
}

pub(crate) fn error_frame(message: &str) -> String {
    let mut err = serde_json::json!({ "message": message, "stack": "", "plugin": "oj" });
    static LOC: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = LOC.get_or_init(|| {
        regex::Regex::new(r"([^\s():]+\.[A-Za-z0-9]+):(\d+)(?::(\d+))?").expect("loc regex")
    });
    if let Some(c) = re.captures(message) {
        err["id"] = serde_json::Value::String(c[1].to_string());
        let line = c[2].parse::<u64>().unwrap_or(0);
        let column = c
            .get(3)
            .and_then(|m| m.as_str().parse::<u64>().ok())
            .unwrap_or(0);
        err["loc"] = serde_json::json!({ "file": &c[1], "line": line, "column": column });
    }
    if let Some((_, frame)) = message.split_once('\n') {
        if !frame.trim().is_empty() {
            err["frame"] = serde_json::Value::String(frame.to_string());
        }
    }
    serde_json::json!({ "type": "error", "err": err }).to_string()
}

/// One entry of Vite's `UpdatePayload.updates`.
pub(crate) fn update_entry(kind: &str, path: &str, timestamp: u64) -> serde_json::Value {
    serde_json::json!({
        "type": kind,
        "path": path,
        "acceptedPath": path,
        "timestamp": timestamp,
    })
}

/// The `js-update` entry for a graph boundary: stylesheet boundaries are named by
/// their module wrapper (`?import`); `isWithinCircularImport` and
/// `firstInvalidatedBy` are carried like Vite's `Update` so the client can reset
/// the page when a re-import inside a cycle fails and escalate a repeated
/// `hot.invalidate` instead of looping.
pub(crate) fn update_entry_for(
    target: &oj_graph::UpdateTarget,
    timestamp: u64,
    first_invalidated_by: Option<&str>,
) -> serde_json::Value {
    let style = |p: &Path| {
        let p = p.display().to_string();
        if is_style_url(&p) {
            format!("{p}?import")
        } else {
            p
        }
    };
    let mut entry = update_entry("js-update", &style(&target.boundary), timestamp);
    entry["acceptedPath"] = serde_json::Value::String(style(&target.accepted));
    if target.within_circular_import {
        entry["isWithinCircularImport"] = serde_json::Value::Bool(true);
    }
    if let Some(first) = first_invalidated_by {
        entry["firstInvalidatedBy"] = serde_json::Value::String(first.to_string());
    }
    entry
}

/// Vite's `FullReloadPayload`: `path` is the edited page (`/about.html`) so the
/// client reloads only tabs showing it, or `*` for every page; `triggeredBy` is
/// the absolute file. oj's `reason` is kept for its own log and tooling.
pub(crate) fn full_reload_frame(reason: &str, page: Option<&str>, triggered_by: Option<&Path>) -> String {
    let mut frame = serde_json::json!({
        "type": "full-reload",
        "reason": reason,
        "path": page.unwrap_or("*"),
    });
    if let Some(file) = triggered_by {
        frame["triggeredBy"] = serde_json::Value::String(file.display().to_string());
    }
    frame.to_string()
}

pub(crate) fn svelte_hot_glue(url: &str) -> String {
    format!(
        "\nimport {{ createHotContext as __oj_createHotContext }} from \"/@oj/client.js\";\nimport.meta.hot = __oj_createHotContext({url:?});\n"
    )
}

pub(crate) struct HmrGate {
    pub(crate) full_reload: bool,
    pub(crate) max_hold: Duration,
    pub(crate) inner: Mutex<GateInner>,
    /// A page reload the Start server is holding for the flush (the editor's
    /// gate plugin's `heldReload`: a bundled dev server's reload is one event,
    /// not a set of hot updates).
    pub(crate) held_reload: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
pub(crate) struct GateInner {
    pending: std::collections::BTreeMap<PathBuf, std::collections::BTreeSet<String>>,
    generation: u64,
}

pub(crate) fn gate_relevant(path: &Path) -> bool {
    !path.components().any(|c| {
        let c = c.as_os_str();
        c == "node_modules" || c == ".oj-cache" || c == "dist"
    })
}

impl HmrGate {
    pub(crate) fn hold(&self, state: &Arc<ServerState>, paths: &[PathBuf]) -> bool {
        let relevant: Vec<&PathBuf> = paths.iter().filter(|p| gate_relevant(p)).collect();
        if relevant.is_empty() {
            return false;
        }
        let mut inner = self.inner.lock().unwrap();
        let was_empty = inner.pending.is_empty();
        for p in relevant {
            inner
                .pending
                .entry(p.clone())
                .or_default()
                .insert("change".to_string());
        }
        if was_empty {
            inner.generation += 1;
            let generation = inner.generation;
            let state = Arc::clone(state);
            let max_hold = self.max_hold;
            let rt = state.rt.clone();
            rt.spawn(async move {
                tokio::time::sleep(max_hold).await;
                if let Some(gate) = &state.hmr_gate {
                    let expired = {
                        let g = gate.inner.lock().unwrap();
                        g.generation == generation && !g.pending.is_empty()
                    };
                    if expired {
                        gate.flush(&state).await;
                    }
                }
            });
        }
        true
    }

    async fn flush(&self, state: &Arc<ServerState>) -> (Vec<String>, usize) {
        let entries: Vec<(PathBuf, std::collections::BTreeSet<String>)> = {
            let mut inner = self.inner.lock().unwrap();
            inner.generation += 1;
            std::mem::take(&mut inner.pending).into_iter().collect()
        };
        let files: Vec<String> = entries
            .iter()
            .map(|(p, _)| p.display().to_string())
            .collect();
        let count = entries.len();
        let held_reload = self
            .held_reload
            .swap(false, std::sync::atomic::Ordering::SeqCst);
        if held_reload || !entries.is_empty() {
            let _ = state.gate_flush_tx.send(());
        }
        if !entries.is_empty() {
            state.dir_cache.lock().unwrap().clear();
            if self.full_reload {
                let _ = state
                    .reload_tx
                    .send(full_reload_frame("hmr-flush", None, None));
            } else {
                let paths: Vec<PathBuf> = entries.into_iter().map(|(p, _)| p).collect();
                let sref: &ServerState = state;
                for message in decide(sref, &paths, &Default::default()).await {
                    let _ = state.reload_tx.send(message);
                }
            }
        }
        (files, count)
    }

    fn mode(&self) -> &'static str {
        if self.full_reload {
            "full-reload"
        } else {
            "granular"
        }
    }

    fn status(&self, state: &ServerState) -> serde_json::Value {
        let inner = self.inner.lock().unwrap();
        let mut pending = serde_json::Map::new();
        for (p, events) in &inner.pending {
            pending.insert(
                p.display().to_string(),
                serde_json::json!(events.iter().collect::<Vec<_>>()),
            );
        }
        let held_reload = self.held_reload.load(std::sync::atomic::Ordering::SeqCst);
        serde_json::json!({
            "enabled": true,
            "pending": pending,
            "count": inner.pending.len(),
            "mode": self.mode(),
            "heldReload": held_reload,
            "startedAt": state.started_at_ms,
        })
    }
}

/// The HMR gate as the Start server sees it: hold the page reload a rebuild
/// would send until the editor flushes (`POST /__hmr_flush`) or the hold cap
/// releases it, like the editor's Vite gate plugin holds a bundled dev server's
/// `full-reload`. Without it every write under `src/` reloaded the preview at
/// once, gate or not.
#[derive(Clone)]
pub struct HmrGateHandle {
    pub(crate) state: Arc<ServerState>,
}

impl HmrGateHandle {
    /// Record the changed paths and hold the reload. False when nothing relevant
    /// changed (build output, caches), in which case the caller reloads now.
    pub fn hold_reload(&self, paths: &[PathBuf]) -> bool {
        let Some(gate) = &self.state.hmr_gate else {
            return false;
        };
        if !gate.hold(&self.state, paths) {
            return false;
        }
        gate.held_reload
            .store(true, std::sync::atomic::Ordering::SeqCst);
        true
    }

    /// Fires once per flush that released something.
    pub fn subscribe_flush(&self) -> broadcast::Receiver<()> {
        self.state.gate_flush_tx.subscribe()
    }
}

pub(crate) async fn hmr_flush(State(state): State<Arc<ServerState>>) -> Response {
    let Some(gate) = &state.hmr_gate else {
        return js_response_json(
            serde_json::json!({ "flushed": [], "count": 0, "mode": "disabled" }),
        );
    };
    let held_reload = gate.held_reload.load(std::sync::atomic::Ordering::SeqCst);
    let (files, count) = gate.flush(&state).await;
    js_response_json(
        serde_json::json!({ "flushed": files, "count": count, "mode": gate.mode(), "reload": held_reload || count > 0 }),
    )
}

pub(crate) async fn hmr_gate_status(State(state): State<Arc<ServerState>>) -> Response {
    match &state.hmr_gate {
        Some(gate) => js_response_json(gate.status(&state)),
        None => js_response_json(serde_json::json!({ "enabled": false })),
    }
}

/// Which paths of a watcher event count as a content change, by chokidar's
/// rule: the data changed, or the modification time moved since this watcher
/// last saw the file. An attribute-only event whose mtime is unchanged (or the
/// first such event for a path, with nothing to compare against) is not a
/// change. On Linux the first read of a file after it was written updates its
/// atime under relatime, and inotify reports that as an attribute change, so a
/// rebuild that reads every source file looked like an edit of every source
/// file and triggered another rebuild, until the atimes settled.
pub struct ContentChanges {
    mtimes: std::collections::HashMap<PathBuf, std::time::SystemTime>,
}

impl Default for ContentChanges {
    fn default() -> Self {
        Self::new()
    }
}

impl ContentChanges {
    pub fn new() -> Self {
        Self {
            mtimes: std::collections::HashMap::new(),
        }
    }

    pub fn changed_paths(&mut self, ev: &notify::Event) -> Vec<PathBuf> {
        match &ev.kind {
            notify::EventKind::Access(_) => Vec::new(),
            notify::EventKind::Modify(notify::event::ModifyKind::Metadata(_)) => ev
                .paths
                .iter()
                .filter(|p| self.mtime_moved(p))
                .cloned()
                .collect(),
            _ => {
                for p in &ev.paths {
                    if let Ok(mtime) = std::fs::metadata(p).and_then(|m| m.modified()) {
                        self.mtimes.insert(p.clone(), mtime);
                    }
                }
                ev.paths.clone()
            }
        }
    }

    fn mtime_moved(&mut self, p: &Path) -> bool {
        let Ok(mtime) = std::fs::metadata(p).and_then(|m| m.modified()) else {
            self.mtimes.remove(p);
            return true;
        };
        match self.mtimes.insert(p.to_path_buf(), mtime) {
            Some(prev) => prev != mtime,
            // No baseline to compare against (there is no initial scan): a
            // fresh mtime is a touch/utimes change that must count once, an old
            // one is the relatime atime noise this filter exists to ignore.
            None => mtime
                .elapsed()
                .map(|age| age < std::time::Duration::from_secs(10))
                .unwrap_or(true),
        }
    }
}

// Async because it is reached both from the watcher thread (via block_on) and
// from the async /__hmr_flush handler; using block_on here panicked ("runtime
// within a runtime") when the gate flushed on an async worker thread.
pub(crate) fn parse_hmr_filter(raw: &str) -> Option<Vec<PathBuf>> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    if v.get("action")?.as_str()? != "filter" {
        return None;
    }
    let arr = v.get("modules")?.as_array()?;
    Some(
        arr.iter()
            .filter_map(|m| m.as_str().map(PathBuf::from))
            .collect(),
    )
}

/// `created`: the paths the watcher reported as newly created among `paths`
/// (the rest are edits, or removals when the file is gone).
pub(crate) async fn decide(
    state: &ServerState,
    paths: &[PathBuf],
    created: &std::collections::HashSet<PathBuf>,
) -> Vec<String> {
    if !state.hmr_enabled {
        return Vec::new();
    }
    let mut messages: Vec<String> = Vec::new();
    let mut updates: Vec<serde_json::Value> = Vec::new();

    let plugin_watched: std::collections::HashSet<PathBuf> = {
        let mut raw: Vec<String> = match &state.plugins {
            Some(host) => host.watch_files().await.unwrap_or_default(),
            None => Vec::new(),
        };
        raw.extend(
            state
                .plugin_watched
                .lock()
                .unwrap()
                .iter()
                .map(|p| p.to_string_lossy().into_owned()),
        );
        raw.into_iter()
            .map(|p| std::fs::canonicalize(&p).unwrap_or_else(|_| PathBuf::from(p)))
            .collect()
    };

    let source_changed = paths.iter().any(|p| {
        !p.components().any(|c| {
            let c = c.as_os_str();
            c == "node_modules" || c == ".oj-cache" || c == "dist"
        }) && p
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| COMPILABLE.contains(&e))
    });
    if source_changed {
        let timestamp = now_millis() as u64;
        for url in state.tailwind_urls.lock().unwrap().iter() {
            updates.push(update_entry("css-update", url, timestamp));
        }
    }

    // A file appearing on disk may be the one a module failed to import: Vite
    // adds `_hasResolveFailedErrorModules` to a `create` event's module set so
    // those importers are re-processed (hmr.ts). notify does not tell a create
    // from a modify reliably, so a file that exists but is not in the graph is
    // taken as new; the importers then go through the loop as changed files.
    let mut paths: Vec<PathBuf> = paths.to_vec();
    let new_file = paths.iter().any(|p| {
        p.is_file()
            && !state
                .graph
                .lock()
                .unwrap()
                .contains(Path::new(&url_of(&state.root, p)))
    });
    if new_file {
        // The resolver caches misses too: without this a created `./dir/index.ts`
        // or extension-probed file stays "not found" for the importer's retry.
        state.resolver.clear_cache();
        state.ssr_resolver.clear_cache();
        let failed: Vec<String> = state.resolve_failed.lock().unwrap().drain().collect();
        for url in failed {
            paths.push(state.root.join(url.trim_start_matches('/')));
        }
    }
    // A file created or deleted under an `import.meta.glob` pattern changes
    // what the importer expands to: recompile and update the importer as if it
    // had been edited (Vite's importMetaGlob hotUpdate on create/delete).
    let glob_importers: Vec<String> = {
        let globs = state.glob_importers.lock().unwrap();
        if globs.is_empty() {
            Vec::new()
        } else {
            let graph = state.graph.lock().unwrap();
            // `*` stops at `/`, as the directory walk expanding the glob does.
            let opts = glob::MatchOptions {
                require_literal_separator: true,
                ..Default::default()
            };
            let mut hit: Vec<String> = Vec::new();
            for p in &paths {
                let known = graph.contains(Path::new(&url_of(&state.root, p)));
                let added_or_removed = !p.exists() || !known;
                if !added_or_removed {
                    continue;
                }
                for (importer, patterns) in globs.iter() {
                    if patterns.iter().any(|pat| pat.matches_path_with(p, opts))
                        && !hit.contains(importer)
                    {
                        hit.push(importer.clone());
                    }
                }
            }
            hit
        }
    };
    for importer in glob_importers {
        println!("oj: glob importer {importer} re-expanded");
        state.mtime_keys.lock().unwrap().remove(&importer);
        state.memory.lock().unwrap().remove(&importer);
        let file = state.root.join(importer.trim_start_matches('/'));
        if !paths.contains(&file) {
            paths.push(file);
        }
    }

    for path in &paths {
        // Vite's default watch ignores: `**/node_modules/**` and `**/.git/**`
        // at any depth (a nested package's node_modules included).
        if path.components().any(|c| {
            let c = c.as_os_str();
            c == "node_modules" || c == ".oj-cache" || c == "dist" || c == ".git"
        }) {
            continue;
        }

        if let Some(host) = &state.plugins {
            let file = path.display().to_string();
            // Vite's watcher hands plugins the change kind (hmr.ts HotUpdateOptions
            // type): a new file is "create" (chokidar add), an edit "update", and
            // a removed file still reaches watchChange / hotUpdate as "delete".
            let change_type = if !path.exists() {
                "delete"
            } else if created.contains(path)
                && !state
                    .graph
                    .lock()
                    .unwrap()
                    .contains(Path::new(&url_of(&state.root, path)))
            {
                // Newly created and never served: a file the graph already holds
                // was only rewritten (editors that replace files on save).
                "create"
            } else {
                "update"
            };
            // The ssr environment's plugin instances (Vite dispatches hotUpdate
            // and watchChange to every environment) when that host is up.
            let ssr_host = state.plugins_ssr.get().and_then(|h| h.clone());
            // Pre-init fast-skip: this dispatch path is serial, and each hook
            // toward a still-initializing lazy host would await a full
            // per-call init window — on a wedged init that froze every save's
            // HMR for 2× the window, forever. Skip both hooks and queue a
            // watchChange catch-up instead (replayed at the host's init by
            // spawn_ssr_watch_catch_up); a healthy slow boot still gets
            // post-init events normally. The re-check after queuing closes
            // the race where init lands between the decision and the push —
            // the catch-up task may have already drained.
            let ssr_host = match ssr_host {
                Some(ssr)
                    if !ssr.is_initialized()
                        && (state.plugins_watch_change || state.plugins_hot_update) =>
                {
                    note_ssr_watch_skip(&state.ssr_watch, &file, change_type);
                    if ssr.is_initialized() {
                        replay_ssr_watch_backlog(&ssr, &state.ssr_watch).await;
                    }
                    None
                }
                Some(ssr) if state.plugins_watch_change || state.plugins_hot_update => {
                    // Initialized: flush any queued catch-up events FIRST —
                    // under the queue's order lock, blocking while the
                    // catch-up task is mid-replay — so a stale queued
                    // watchChange can never land after this newer live event
                    // for the same file. Empty-queue cost is one lock check.
                    replay_ssr_watch_backlog(&ssr, &state.ssr_watch).await;
                    Some(ssr)
                }
                other => other,
            };
            if state.plugins_watch_change {
                if let Err(e) = host.watch_change(&file, change_type).await {
                    eprintln!("oj: watchChange failed for {file}: {e}");
                }
                if let Some(ssr) = &ssr_host {
                    if let Err(e) = ssr.watch_change(&file, change_type).await {
                        eprintln!("oj: watchChange (ssr) failed for {file}: {e}");
                    }
                }
            }
            if state.plugins_hot_update {
                let ts = now_millis() as u64;
                let hmr_url = url_of(&state.root, path);
                let modules_json = {
                    let g = state.graph.lock().unwrap();
                    match g.node(Path::new(&hmr_url)) {
                        Some(n) => serde_json::json!([{
                            "url": hmr_url,
                            "id": hmr_url,
                            "isSelfAccepting": n.is_self_accepting,
                            "importers": n
                                .importers
                                .iter()
                                .map(|p| p.display().to_string())
                                .collect::<Vec<_>>(),
                        }])
                        .to_string(),
                        None => "[]".to_string(),
                    }
                };
                if let Some(ssr) = &ssr_host {
                    // Its result steers no client update (the ssr environment has
                    // no browser); only a throwing hook is worth reporting.
                    if let Err(e) = ssr
                        .handle_hot_update(&file, ts, change_type, &modules_json)
                        .await
                    {
                        eprintln!("oj: hotUpdate (ssr) failed for {file}: {e}");
                    }
                }
                match host
                    .handle_hot_update(&file, ts, change_type, &modules_json)
                    .await
                {
                    // Vite (hmr.ts): a throwing hotUpdate is logged and sent to
                    // the client as an error payload (the overlay), and no update
                    // is dispatched for that file.
                    Err(e) => {
                        eprintln!("oj: hotUpdate failed for {file}: {e}");
                        messages.push(error_frame(&e));
                        continue;
                    }
                    Ok(Some(d)) if d == "skip" => {
                        println!("oj: change {file} -> HMR suppressed by plugin");
                        continue;
                    }
                    Ok(Some(d)) if d == "full-reload" => {
                        println!("oj: change {file} -> full-reload (plugin)");
                        messages.push(full_reload_frame("plugin", None, Some(path)));
                        return messages;
                    }
                    Ok(Some(d)) => {
                        if let Some(seeds) = parse_hmr_filter(&d) {
                            let seed_refs: Vec<&Path> =
                                seeds.iter().map(PathBuf::as_path).collect();
                            let decision =
                                state.graph.lock().unwrap().propagate_from_seeds(&seed_refs);
                            match decision {
                                HmrDecision::Update { boundaries } => {
                                    println!(
                                        "oj: change {file} -> plugin-filtered update {boundaries:?}"
                                    );
                                    let timestamp = now_millis() as u64;
                                    updates.extend(boundaries.iter().map(|b| {
                                        let mut p = format!("{}", b.display());
                                        if is_style_url(&p) {
                                            p.push_str("?import");
                                        }
                                        update_entry("js-update", &p, timestamp)
                                    }));
                                    continue;
                                }
                                HmrDecision::FullReload { reason } => {
                                    println!("oj: change {file} -> full-reload ({reason})");
                                    messages.push(full_reload_frame(&reason, None, Some(path)));
                                    return messages;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        if !plugin_watched.is_empty() {
            let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            if plugin_watched.contains(&canon) {
                println!(
                    "oj: change {} -> full-reload (plugin watch)",
                    path.display()
                );
                messages.push(full_reload_frame("plugin-watch", None, Some(path)));
                return messages;
            }
        }

        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if is_style_ext(ext) {
            let url = url_of(&state.root, path);
            // A stylesheet nothing imports is loaded by a `<link>` (serving it
            // compiled registers it in the graph, with no importers): swap the
            // link rather than dispatching a JS update it has no handler for.
            let link_loaded = {
                let g = state.graph.lock().unwrap();
                match g.node(Path::new(&url)) {
                    None => true,
                    Some(n) => n.importers.is_empty(),
                }
            };
            if link_loaded {
                println!("oj: change {url} -> css-update");
                updates.push(update_entry("css-update", &url, now_millis() as u64));
                continue;
            }
        }
        if ext == "html" {
            // Vite names the edited page (`path: '/about.html'`) so only the tab
            // showing it reloads; the reason keeps the absolute path oj's own
            // tooling reads.
            let page = url_of(&state.root, path);
            println!("oj: change {} -> full-reload", path.display());
            messages.push(full_reload_frame(
                &path.display().to_string(),
                Some(&page),
                Some(path),
            ));
            return messages;
        }
        if !(COMPILABLE.contains(&ext) || is_style_ext(ext) || ext == "json") {
            continue;
        }

        let url = url_of(&state.root, path);
        if !state.graph.lock().unwrap().contains(Path::new(&url)) {
            continue;
        }
        let targets = state.graph.lock().unwrap().update_targets(Path::new(&url));
        // A changed stylesheet may also be inlined into OTHER sheets (@import,
        // sass @use), which record it among their imports: each such importer
        // must hot-swap itself too. Vite dispatches both because the file-only
        // dep entry exists next to the real module; oj keeps one node per path
        // (self-accepting when the sheet is also served directly), so the walk
        // stops there and the css importers are seeded explicitly.
        let targets = targets.and_then(|mut targets| {
            if is_style_ext(ext) {
                let css_importers: Vec<PathBuf> = {
                    let g = state.graph.lock().unwrap();
                    g.node(Path::new(&url))
                        .map(|n| {
                            n.importers
                                .iter()
                                .filter(|p| is_style_url(&p.to_string_lossy()))
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default()
                };
                for importer in css_importers {
                    // A css importer that cannot reach a boundary (a css
                    // module whose component importer does not accept) needs
                    // the same full reload the direct walk would force.
                    targets.extend(state.graph.lock().unwrap().update_targets(&importer)?);
                }
                targets.sort();
                targets.dedup();
            }
            Ok(targets)
        });
        match targets {
            Ok(targets) => {
                let boundaries: Vec<&Path> = targets.iter().map(|t| t.boundary.as_path()).collect();
                println!("oj: change {url} -> update {boundaries:?}");
                let timestamp = now_millis() as u64;
                // Stamp the invalidated chain so re-fetched importers point at the
                // new versions of their (unchanged-on-disk) dependencies, and drop
                // those importers' mtime fast-path keys so they recompile with the
                // stamps rather than serving the cached code.
                let dirty = state
                    .graph
                    .lock()
                    .unwrap()
                    .stamp_update(Path::new(&url), timestamp);
                {
                    let mut keys = state.mtime_keys.lock().unwrap();
                    for d in &dirty {
                        keys.remove(&d.display().to_string());
                    }
                }
                if targets.is_empty() {
                    println!("oj: change {url} -> no update (nothing loaded imports it)");
                }
                updates.extend(targets.iter().map(|t| update_entry_for(t, timestamp, None)));
            }
            Err(reason) => {
                println!("oj: change {url} -> full-reload ({reason})");
                messages.push(full_reload_frame(&reason, None, Some(path)));
                return messages;
            }
        }
    }

    if !updates.is_empty() {
        messages.push(serde_json::json!({ "type": "update", "updates": updates }).to_string());
    }
    messages
}
