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

pub(crate) fn hmr_socket(
    upgrade: WebSocketUpgrade,
    state: Arc<ServerState>,
    vite: bool,
) -> Response {
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
        let buffered = state.buffered_frame.lock().unwrap().take();
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

/// Host-driven invalidateModule/restart. Invalidation drops compiled output and,
/// for an app file, propagates HMR like an edit; a virtual id only loses its cache (Vite parity).
pub(crate) async fn handle_plugin_server_event(state: &Arc<ServerState>, ev: &serde_json::Value) {
    match ev.get("action").and_then(|a| a.as_str()) {
        Some("restart") => {
            println!("oj: plugin requested server.restart()");
            restart_process("plugin requested server.restart()");
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
                // (Vite's lastHMRInvalidationReceived); importers are stamped too.
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
                // The chain came back to the module that started it: no importer
                // can hot update it, so reload (Vite's 'circular import invalidate').
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
                    update_frame(updates)
                }
                Err(reason) => {
                    println!("oj: invalidate {path} -> full-reload ({reason})");
                    full_reload_frame(&reason, None, None)
                }
            }
        };
        let _ = state.reload_tx.send(reply);
    } else if msg["type"] == "custom" && msg["event"] == "vite:forward-console" {
        // One page's console, not app state: handled server-side (logged and
        // recorded) and offered to plugins, never re-broadcast to other clients.
        ingest_forward_console(state, &msg["data"]);
        forward_custom_to_host(state, &msg);
    } else if msg["type"] == "custom" && msg["event"] == "oj:hmr-result" {
        ingest_hmr_result(&msg["data"]);
        forward_custom_to_host(state, &msg);
    } else if msg["type"] == "custom" && msg["event"].is_string() {
        let _ = state.reload_tx.send(
            serde_json::json!({
                "type": "custom",
                "event": msg["event"],
                "data": msg["data"],
            })
            .to_string(),
        );
        forward_custom_to_host(state, &msg);
    }
}

/// Hand a client custom event to the plugin host's `server.ws` listeners.
fn forward_custom_to_host(state: &Arc<ServerState>, msg: &serde_json::Value) {
    if let Some(host) = state.plugins.clone() {
        let event = msg["event"].as_str().unwrap_or_default().to_string();
        let data = msg["data"].to_string();
        tokio::spawn(async move {
            let _ = host.ws_message(&event, &data).await;
        });
    }
}

/// Hot-context/Fast Refresh id: the url minus oj's `t=<timestamp>`, other query kept.
/// Must match the clean path in update messages or accepts never fire (Vite removeTimestampQuery).
pub(crate) fn strip_hmr_timestamp(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };
    // The timestamp is exactly 13 digits (now_millis); match only that, as Vite's
    // `timestampRE` does, so a user's own `t=` query is never stripped.
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

/// Append the HMR timestamp to a served module url. Only JS-served modules (compilable,
/// JSON, style wrappers) are stamped per Vite's importAnalysis; asset and oj-internal urls never.
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

/// `ctx_predefined`: the body already carries the hot-context banner; the glue must
/// then REUSE `import.meta.hot`, never re-import: a second createHotContext import in one module scope is a SyntaxError.
pub(crate) fn hot_glue(
    url: &str,
    query: Option<&str>,
    is_boundary: bool,
    ctx_predefined: bool,
) -> String {
    if !is_boundary {
        return String::new();
    }
    // The self-import must name exactly the served url (keeping `t=` so the browser dedupes)
    // and never re-append a query the url already has: per-edit re-appends grew the url until hyper answered 414.
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

/// Broadcast a frame, or hold it for the next client when none is connected
/// yet: a page still booting has no socket open, so a frame it must see (an
/// error from its first module request, a reload from a dep re-optimization
/// that just outdated its URLs) waits in `buffered_frame` and is delivered
/// on connect (Vite's ws `bufferedError`, generalized).
pub(crate) fn send_or_buffer(state: &ServerState, frame: String) {
    if state.reload_tx.receiver_count() == 0 {
        *state.buffered_frame.lock().unwrap() = Some(frame);
    } else {
        let _ = state.reload_tx.send(frame);
    }
}

/// A Vite `ErrorPayload` frame for every connected client, or the next one.
/// Also the diagnostics record for every served compile/transform failure:
/// the browser overlay was the only witness before.
pub(crate) fn send_error(state: &ServerState, message: &str) {
    oj_diag::emit(
        oj_diag::Event::new(oj_diag::Kind::CompileError, message)
            .module(loc_captures(message).map(|c| c[1].to_string())),
    );
    send_or_buffer(state, error_frame(message));
}

/// The `file.ext:line[:col]` capture an error message carries, shared by the
/// overlay frame and the diagnostics record.
fn loc_captures(message: &str) -> Option<regex::Captures<'_>> {
    static LOC: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = LOC.get_or_init(|| {
        regex::Regex::new(r"([^\s():]+\.[A-Za-z0-9]+):(\d+)(?::(\d+))?").expect("loc regex")
    });
    re.captures(message)
}

pub(crate) fn error_frame(message: &str) -> String {
    let mut err = serde_json::json!({ "message": message, "stack": "", "plugin": "oj" });
    if let Some(c) = loc_captures(message) {
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

/// Vite's `UpdatePayload`.
fn update_frame(updates: Vec<serde_json::Value>) -> String {
    serde_json::json!({ "type": "update", "updates": updates }).to_string()
}

/// A boundary's url in an update: stylesheets are named by their `?import` wrapper.
fn boundary_url(p: &Path) -> String {
    let p = p.display().to_string();
    if is_style_url(&p) {
        format!("{p}?import")
    } else {
        p
    }
}

/// The `js-update` entry for a boundary: stylesheet boundaries are named by their
/// `?import` wrapper; carries `isWithinCircularImport` and `firstInvalidatedBy` like Vite's `Update`.
pub(crate) fn update_entry_for(
    target: &oj_graph::UpdateTarget,
    timestamp: u64,
    first_invalidated_by: Option<&str>,
) -> serde_json::Value {
    let mut entry = update_entry("js-update", &boundary_url(&target.boundary), timestamp);
    entry["acceptedPath"] = serde_json::Value::String(boundary_url(&target.accepted));
    if target.within_circular_import {
        entry["isWithinCircularImport"] = serde_json::Value::Bool(true);
    }
    if let Some(first) = first_invalidated_by {
        entry["firstInvalidatedBy"] = serde_json::Value::String(first.to_string());
    }
    entry
}

/// Vite's `FullReloadPayload`: `path` is the edited page or `*` for every page;
/// `triggeredBy` the absolute file. oj's `reason` feeds its own log and tooling.
pub(crate) fn full_reload_frame(
    reason: &str,
    page: Option<&str>,
    triggered_by: Option<&Path>,
) -> String {
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
    /// A page reload the Start server is holding for the flush (the editor gate
    /// plugin's `heldReload`: one event, not a set of hot updates).
    pub(crate) held_reload: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
pub(crate) struct GateInner {
    pending: std::collections::BTreeMap<PathBuf, std::collections::BTreeSet<String>>,
    /// When the last flush ran. A flush covers every write made before it, so
    /// a hold arriving later for one of those writes is already released; the
    /// cap timer compares against the same instant to know whether its hold
    /// set was ever flushed.
    last_flush: Option<std::time::SystemTime>,
}

pub(crate) fn gate_relevant(path: &Path) -> bool {
    !path.components().any(|c| {
        let c = c.as_os_str();
        c == "node_modules" || c == ".oj-cache" || c == "dist"
    })
}

impl HmrGate {
    /// `reload` arms the held page reload with the paths, inside the same
    /// lock: armed after `hold` returns, a flush racing the two steps would
    /// drain the paths yet leave the reload held, with nothing pending to make
    /// the cap timer fire and release it.
    pub(crate) fn hold(&self, state: &Arc<ServerState>, paths: &[PathBuf], reload: bool) -> bool {
        let mut relevant: Vec<&PathBuf> = paths.iter().filter(|p| gate_relevant(p)).collect();
        if relevant.is_empty() {
            return false;
        }
        let mut inner = self.inner.lock().unwrap();
        // The editor writes, then flushes: that flush covers every write made
        // before it, however late the write's watcher event is delivered (the
        // dev server and the Start rebundler watch independently, and a Start
        // rebuild holds only once it finishes). Holding a pre-flush write again
        // would wait out the cap for a flush that already happened, serving
        // the previous content for minutes. A file whose mtime is unreadable
        // (deleted) stays held, the conservative side.
        if let Some(flushed_at) = inner.last_flush {
            relevant.retain(|p| {
                std::fs::metadata(p)
                    .and_then(|m| m.modified())
                    .ok()
                    .is_none_or(|mtime| mtime > flushed_at)
            });
            if relevant.is_empty() {
                return false;
            }
        }
        let was_empty = inner.pending.is_empty();
        for p in relevant {
            inner
                .pending
                .entry(p.clone())
                .or_default()
                .insert("change".to_string());
        }
        if reload {
            self.held_reload
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        if was_empty {
            // The cap timer for this hold set: it fires only if nothing
            // flushed after it was armed, the same instant the write-coverage
            // check above compares against.
            let armed = std::time::SystemTime::now();
            let state = Arc::clone(state);
            let max_hold = self.max_hold;
            let rt = state.rt.clone();
            rt.spawn(async move {
                tokio::time::sleep(max_hold).await;
                if let Some(gate) = &state.hmr_gate {
                    let expired = {
                        let g = gate.inner.lock().unwrap();
                        !g.pending.is_empty() && g.last_flush.is_none_or(|f| f < armed)
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
        // The reload bit drops inside the same lock that drains the paths, so
        // a concurrent hold either lands entirely before this flush (drained
        // and released here) or entirely after it (held for the next one).
        let (entries, held_reload): (Vec<(PathBuf, std::collections::BTreeSet<String>)>, bool) = {
            let mut inner = self.inner.lock().unwrap();
            inner.last_flush = Some(std::time::SystemTime::now());
            (
                std::mem::take(&mut inner.pending).into_iter().collect(),
                self.held_reload
                    .swap(false, std::sync::atomic::Ordering::SeqCst),
            )
        };
        let files: Vec<String> = entries
            .iter()
            .map(|(p, _)| p.display().to_string())
            .collect();
        let count = entries.len();
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

/// The HMR gate for the Start server: hold the reload a rebuild would send until
/// the editor flushes (`POST /__hmr_flush`) or the hold cap releases it.
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
        gate.hold(&self.state, paths, true)
    }

    /// Whether the gate still holds anything: changes awaiting a flush, or an
    /// armed page reload. The Start rebuild reads this when it finishes — the
    /// hold itself was taken at the watcher event, so an editor flush that
    /// landed mid-rebuild has already consumed it and the answer is false.
    pub fn reload_is_held(&self) -> bool {
        self.state.hmr_gate.as_ref().is_some_and(|gate| {
            gate.held_reload.load(std::sync::atomic::Ordering::SeqCst)
                || !gate.inner.lock().unwrap().pending.is_empty()
        })
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

/// Watcher-event paths that count as content changes, per chokidar: data changed or
/// mtime moved. Filters Linux relatime atime noise, where a rebuild's reads of every source looked like edits and re-triggered the rebuild.
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
            // No baseline (no initial scan): a fresh mtime is a touch that must
            // count once, an old one is the relatime noise this filter ignores.
            None => mtime
                .elapsed()
                .map(|age| age < std::time::Duration::from_secs(10))
                .unwrap_or(true),
        }
    }
}

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

/// What `decide` does next with a changed path.
enum Flow {
    /// Keep deciding this path.
    Next,
    /// This path is handled; go to the next one.
    Done,
    /// Stop: send what was collected plus this full-reload frame.
    Reload(String),
}

/// `created`: watcher-reported new paths among `paths` (the rest are edits, or
/// removals). Async, not block_on: also reached from async handlers, and block_on inside panicked (runtime within a runtime).
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
    let plugin_watched = plugin_watched_files(state).await;

    let source_changed = paths.iter().any(|p| {
        gate_relevant(p)
            && p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| COMPILABLE.contains(&e))
    });
    if source_changed {
        let timestamp = now_millis() as u64;
        for url in state.tailwind_urls.lock().unwrap().iter() {
            updates.push(update_entry("css-update", url, timestamp));
        }
    }

    let mut paths: Vec<PathBuf> = paths.to_vec();
    retry_failed_resolves(state, &mut paths);
    for importer in glob_importers_hit(state, &paths) {
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
        if !gate_relevant(path) || path.components().any(|c| c.as_os_str() == ".git") {
            continue;
        }
        let mut flow = match &state.plugins {
            Some(host) => {
                run_plugin_hooks(state, host, path, created, &mut messages, &mut updates).await
            }
            None => Flow::Next,
        };
        if let Flow::Next = flow {
            flow = decide_file(state, path, &plugin_watched, &mut updates);
        }
        if let Flow::Reload(frame) = flow {
            messages.push(frame);
            return messages;
        }
    }

    if !updates.is_empty() {
        messages.push(update_frame(updates));
    }
    messages
}

/// Files plugins asked to watch (`addWatchFile`, `server.watcher.add`), canonicalized
/// (memoized); a change to one reloads the page.
async fn plugin_watched_files(state: &ServerState) -> std::collections::HashSet<PathBuf> {
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
        .map(|p| canonicalize_memo(state, &p))
        .collect()
}

/// A created file may be one a module failed to import: re-process those importers
/// (Vite's `_hasResolveFailedErrorModules`); notify can't tell create from modify, so exists-but-not-in-graph counts as new.
fn retry_failed_resolves(state: &ServerState, paths: &mut Vec<PathBuf>) {
    let new_file = paths.iter().any(|p| {
        p.is_file()
            && !state
                .graph
                .lock()
                .unwrap()
                .contains(Path::new(&url_of(&state.root, p)))
    });
    if !new_file {
        return;
    }
    // The resolver caches misses too: without this a created `./dir/index.ts`
    // or extension-probed file stays "not found" for the importer's retry.
    state.resolver.clear_cache();
    state.ssr_resolver.clear_cache();
    let failed: Vec<String> = state.resolve_failed.lock().unwrap().drain().collect();
    for url in failed {
        paths.push(state.root.join(url.trim_start_matches('/')));
    }
}

/// A file created/deleted under an `import.meta.glob` pattern re-expands the
/// importer: update it as if edited (Vite's importMetaGlob hotUpdate).
fn glob_importers_hit(state: &ServerState, paths: &[PathBuf]) -> Vec<String> {
    let globs = state.glob_importers.lock().unwrap();
    if globs.is_empty() {
        return Vec::new();
    }
    let graph = state.graph.lock().unwrap();
    // `*` stops at `/`, as the directory walk expanding the glob does.
    let opts = glob::MatchOptions {
        require_literal_separator: true,
        ..Default::default()
    };
    let mut hit: Vec<String> = Vec::new();
    for p in paths {
        let known = graph.contains(Path::new(&url_of(&state.root, p)));
        if p.exists() && known {
            continue;
        }
        for (importer, patterns) in globs.iter() {
            if patterns.iter().any(|pat| pat.matches_path_with(p, opts)) && !hit.contains(importer)
            {
                hit.push(importer.clone());
            }
        }
    }
    hit
}

/// Vite hands plugins the change kind: "create" (chokidar add), "update", and a
/// removed file still reaches the hooks as "delete".
fn change_type(
    state: &ServerState,
    path: &PathBuf,
    created: &std::collections::HashSet<PathBuf>,
) -> &'static str {
    if !path.exists() {
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
    }
}

/// The ssr environment's plugin instances (Vite dispatches hotUpdate and
/// watchChange to every environment) when that host is up.
async fn ssr_host_for_hooks(
    state: &ServerState,
    file: &str,
    change_type: &str,
) -> Option<Arc<PluginHost>> {
    let ssr = state.plugins_ssr.get().and_then(|h| h.clone())?;
    if !(state.plugins_watch_change || state.plugins_hot_update) {
        return Some(ssr);
    }
    // Pre-init fast-skip: each hook toward a still-initializing lazy host
    // would await a full per-call init window on this serial path (a wedged
    // init froze every save's HMR). Queue a watchChange catch-up instead,
    // replayed at host init; the re-check after queuing closes the race
    // where init lands between the decision and the push.
    if !ssr.is_initialized() {
        note_ssr_watch_skip(&state.ssr_watch, file, change_type);
        if ssr.is_initialized() {
            replay_ssr_watch_backlog(&ssr, &state.ssr_watch).await;
        }
        return None;
    }
    // Initialized: replay queued catch-up events FIRST, under the
    // queue's order lock, so a stale queued watchChange never
    // lands after this newer live event for the same file.
    replay_ssr_watch_backlog(&ssr, &state.ssr_watch).await;
    Some(ssr)
}

/// The `modules` argument of `hotUpdate` for a path, as JSON.
fn hot_update_modules_json(state: &ServerState, path: &Path) -> String {
    let hmr_url = url_of(&state.root, path);
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
}

/// Dispatch `watchChange` and `hotUpdate` for a path to the client and ssr plugin hosts.
async fn run_plugin_hooks(
    state: &ServerState,
    host: &PluginHost,
    path: &PathBuf,
    created: &std::collections::HashSet<PathBuf>,
    messages: &mut Vec<String>,
    updates: &mut Vec<serde_json::Value>,
) -> Flow {
    let file = path.display().to_string();
    let change_type = change_type(state, path, created);
    let ssr_host = ssr_host_for_hooks(state, &file, change_type).await;
    if state.plugins_watch_change {
        if let Err(e) = host.watch_change(&file, change_type).await {
            eprintln!("oj: watchChange failed for {file}: {e}");
            emit_hook_failure("watchChange", &file, &e);
        }
        if let Some(ssr) = &ssr_host {
            if let Err(e) = ssr.watch_change(&file, change_type).await {
                eprintln!("oj: watchChange (ssr) failed for {file}: {e}");
                emit_hook_failure("watchChange (ssr)", &file, &e);
            }
        }
    }
    if !state.plugins_hot_update {
        return Flow::Next;
    }
    let ts = now_millis() as u64;
    let modules_json = hot_update_modules_json(state, path);
    if let Some(ssr) = &ssr_host {
        // Its result steers no client update (the ssr environment has
        // no browser); only a throwing hook is worth reporting.
        if let Err(e) = ssr
            .handle_hot_update(&file, ts, change_type, &modules_json)
            .await
        {
            eprintln!("oj: hotUpdate (ssr) failed for {file}: {e}");
            emit_hook_failure("hotUpdate (ssr)", &file, &e);
        }
    }
    match host
        .handle_hot_update(&file, ts, change_type, &modules_json)
        .await
    {
        // Vite (hmr.ts): a throwing hotUpdate is logged, sent as an
        // error payload, and no update is dispatched for that file.
        Err(e) => {
            eprintln!("oj: hotUpdate failed for {file}: {e}");
            emit_hook_failure("hotUpdate", &file, &e);
            messages.push(error_frame(&e));
            Flow::Done
        }
        Ok(Some(d)) if d == "skip" => {
            println!("oj: change {file} -> HMR suppressed by plugin");
            Flow::Done
        }
        Ok(Some(d)) if d == "full-reload" => {
            println!("oj: change {file} -> full-reload (plugin)");
            Flow::Reload(full_reload_frame("plugin", None, Some(path)))
        }
        Ok(Some(d)) => match parse_hmr_filter(&d) {
            Some(seeds) => plugin_filtered_update(state, &file, path, &seeds, updates),
            None => Flow::Next,
        },
        Ok(None) => Flow::Next,
    }
}

/// A throwing watcher-driven plugin hook, as a diagnostics record.
fn emit_hook_failure(hook: &str, file: &str, error: &str) {
    oj_diag::emit(
        oj_diag::Event::new(oj_diag::Kind::PluginHook, format!("{hook} failed: {error}"))
            .source(oj_diag::Source::Plugin)
            .module(file.to_string()),
    );
}

/// A `hotUpdate` that returned a module subset: propagate from those seeds.
fn plugin_filtered_update(
    state: &ServerState,
    file: &str,
    path: &Path,
    seeds: &[PathBuf],
    updates: &mut Vec<serde_json::Value>,
) -> Flow {
    let seed_refs: Vec<&Path> = seeds.iter().map(PathBuf::as_path).collect();
    let decision = state.graph.lock().unwrap().propagate_from_seeds(&seed_refs);
    match decision {
        HmrDecision::Update { boundaries } => {
            println!("oj: change {file} -> plugin-filtered update {boundaries:?}");
            let timestamp = now_millis() as u64;
            updates.extend(
                boundaries
                    .iter()
                    .map(|b| update_entry("js-update", &boundary_url(b), timestamp)),
            );
            Flow::Done
        }
        HmrDecision::FullReload { reason } => {
            println!("oj: change {file} -> full-reload ({reason})");
            Flow::Reload(full_reload_frame(&reason, None, Some(path)))
        }
    }
}

/// The non-plugin decision for a path: plugin-watched files, `<link>`
/// stylesheets, html pages, then a graph walk to the accepting boundaries.
fn decide_file(
    state: &ServerState,
    path: &PathBuf,
    plugin_watched: &std::collections::HashSet<PathBuf>,
    updates: &mut Vec<serde_json::Value>,
) -> Flow {
    if !plugin_watched.is_empty() {
        let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        if plugin_watched.contains(&canon) {
            println!(
                "oj: change {} -> full-reload (plugin watch)",
                path.display()
            );
            return Flow::Reload(full_reload_frame("plugin-watch", None, Some(path)));
        }
    }

    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if is_style_ext(ext) {
        let url = url_of(&state.root, path);
        // A stylesheet nothing imports is loaded by a `<link>`: swap the link
        // rather than dispatching a JS update it has no handler for.
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
            return Flow::Done;
        }
    }
    if ext == "html" {
        // Vite names the edited page so only the tab showing it reloads; the
        // reason keeps the absolute path oj's own tooling reads.
        let page = url_of(&state.root, path);
        println!("oj: change {} -> full-reload", path.display());
        return Flow::Reload(full_reload_frame(
            &path.display().to_string(),
            Some(&page),
            Some(path),
        ));
    }
    if !(COMPILABLE.contains(&ext) || is_style_ext(ext) || ext == "json") {
        return Flow::Done;
    }

    let url = url_of(&state.root, path);
    if !state.graph.lock().unwrap().contains(Path::new(&url)) {
        return Flow::Done;
    }
    match graph_update_targets(state, &url, is_style_ext(ext)) {
        Ok(targets) => {
            let boundaries: Vec<&Path> = targets.iter().map(|t| t.boundary.as_path()).collect();
            println!("oj: change {url} -> update {boundaries:?}");
            let timestamp = now_millis() as u64;
            // Stamp the invalidated chain so re-fetched importers point at new versions
            // of unchanged deps; drop their mtime fast-path keys so they recompile with the stamps.
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
            Flow::Done
        }
        Err(reason) => {
            println!("oj: change {url} -> full-reload ({reason})");
            Flow::Reload(full_reload_frame(&reason, None, Some(path)))
        }
    }
}

/// A changed stylesheet may also be inlined into OTHER sheets (@import, sass @use):
/// each importer must hot-swap too; oj keeps one node per path, so css importers are seeded explicitly.
fn graph_update_targets(
    state: &ServerState,
    url: &str,
    is_style: bool,
) -> Result<Vec<oj_graph::UpdateTarget>, String> {
    let mut targets = state.graph.lock().unwrap().update_targets(Path::new(url))?;
    if !is_style {
        return Ok(targets);
    }
    let css_importers: Vec<PathBuf> = {
        let g = state.graph.lock().unwrap();
        g.node(Path::new(url))
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
        // A css importer that cannot reach a boundary needs the
        // same full reload the direct walk would force.
        targets.extend(state.graph.lock().unwrap().update_targets(&importer)?);
    }
    targets.sort();
    targets.dedup();
    Ok(targets)
}
