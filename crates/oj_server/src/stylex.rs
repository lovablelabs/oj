//! The dev side of oj's native `@stylexjs/unplugin`: every compiled module's
//! rules feed one stylesheet, served at the plugin's endpoint, and a changed
//! sheet tells the plugin's runtimes to refetch it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;

use oj_stylex::plugin::{
    DevMode, StylexPlugin, CSS_UPDATE_EVENT, DEV_CSS_PATH, DEV_RUNTIME_JS, DEV_RUNTIME_PATH,
    RUNTIME_ID,
};

use super::*;

/// The plugin polls its rule store every 150ms and sends one update per
/// change it sees; oj coalesces changes over the same window.
const UPDATE_COALESCE: std::time::Duration = std::time::Duration::from_millis(150);

/// One live plugin per app, command and options: the dev server and a Start
/// dev bundler that load the same config share its rule store, as the plugin's
/// own store is shared across Vite's environments. Held weakly, so a session
/// that ends takes its rules with it.
static SHARED: LazyLock<Mutex<HashMap<String, std::sync::Weak<StylexPlugin>>>> =
    LazyLock::new(Default::default);

/// The config's `@stylexjs/unplugin` as oj runs it for `command`; `None`
/// without one, or under `serve` with `devMode: 'off'` (the plugin's `apply`).
pub fn plugin_for(
    root: &Path,
    config: &oj_config::OjConfig,
    command: &str,
) -> Result<Option<Arc<StylexPlugin>>, String> {
    let Some(stylex) = &config.vite_stylex else {
        return Ok(None);
    };
    let options = stylex.get("options").unwrap_or(&serde_json::Value::Null);
    let build = command == "build";
    // The plugin's `dev` default reads NODE_ENV as the config loads, which is
    // the shell's value or Vite's per-command default.
    let node_env = oj_env::get().knobs.node_env.as_deref().unwrap_or(if build {
        "production"
    } else {
        "development"
    });
    // Canonical, so callers spelling the root differently share one plugin.
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let key = format!("{command}\0{node_env}\0{}\0{options}", canonical.display());
    let mut shared = SHARED.lock().unwrap();
    shared.retain(|_, plugin| plugin.strong_count() > 0);
    let plugin = match shared.get(&key).and_then(std::sync::Weak::upgrade) {
        Some(plugin) => plugin,
        None => {
            let plugin = Arc::new(StylexPlugin::new(
                options,
                root,
                node_env == "development",
                !build,
            )?);
            shared.insert(key, Arc::downgrade(&plugin));
            plugin
        }
    };
    if !build && plugin.dev_mode() == DevMode::Off {
        return Ok(None);
    }
    Ok(Some(plugin))
}

/// Sends the plugin's update event to the dev clients after a sheet change,
/// one per coalescing window.
pub(crate) fn notify_clients(plugin: &StylexPlugin, tx: broadcast::Sender<String>) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let pending = Arc::new(AtomicBool::new(false));
    plugin.set_change_listener(Arc::new(move || {
        if pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let (pending, tx) = (Arc::clone(&pending), tx.clone());
        runtime.spawn(async move {
            tokio::time::sleep(UPDATE_COALESCE).await;
            pending.store(false, Ordering::Release);
            let event = serde_json::json!({ "type": "custom", "event": CSS_UPDATE_EVENT });
            let _ = tx.send(event.to_string());
        });
    }));
}

/// Stores a module's rules under its served url (query dropped: one file is
/// one entry, whichever variant compiled last).
pub(crate) fn record(state: &ServerState, url: &str, rules: &[oj_stylex::rules::StylexRule]) {
    if let Some(plugin) = &state.stylex {
        plugin.set_module_rules(url.split('?').next().unwrap_or(url), rules.to_vec());
    }
}

/// The plugin's dev routes: the stylesheet and, in `full` mode, the plain
/// runtime script. `None` for any other path.
pub(crate) fn serve_route(state: &ServerState, path: &str) -> Option<Response> {
    let plugin = state.stylex.as_ref()?;
    if path == DEV_CSS_PATH {
        let css = match plugin.stylesheet() {
            Ok(Some(sheet)) => match oj_css::process_for_browserslist(
                "stylex.css",
                &sheet,
                &state.root,
                "development",
            ) {
                Ok(css) => css,
                Err(e) => {
                    return Some(
                        (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {e}")).into_response(),
                    )
                }
            },
            Ok(None) => String::new(),
            Err(e) => {
                return Some(
                    (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {e}")).into_response(),
                )
            }
        };
        return Some(
            (
                [
                    (header::CONTENT_TYPE, "text/css"),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                css,
            )
                .into_response(),
        );
    }
    if path == DEV_RUNTIME_PATH && plugin.dev_mode() == DevMode::Full {
        return Some(js(DEV_RUNTIME_JS));
    }
    None
}

/// The plugin's virtual module for `id`: its source, compiled like any module
/// (it reads `import.meta.hot`).
pub(crate) fn virtual_module(state: &ServerState, id: &str) -> Option<&'static str> {
    state.stylex.as_ref()?.virtual_module(id)
}

/// `full` mode's `transformIndexHtml`: the runtime and stylesheet link, at the
/// end of `<head>`, under `base`.
pub(crate) fn html_tags(
    state: &ServerState,
    base: &str,
) -> Vec<(&'static str, Vec<(&'static str, String)>)> {
    match state.stylex.as_ref().map(|plugin| plugin.dev_mode()) {
        Some(DevMode::Full) => vec![
            (
                "script",
                vec![
                    ("type", "module".to_string()),
                    ("src", dev_html::posix_join(&[base, "/@id/", RUNTIME_ID])),
                ],
            ),
            (
                "link",
                vec![
                    ("rel", "stylesheet".to_string()),
                    ("href", dev_html::posix_join(&[base, DEV_CSS_PATH])),
                ],
            ),
        ],
        _ => Vec::new(),
    }
}
