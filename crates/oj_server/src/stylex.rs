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
    // The plugin's `dev` default reads NODE_ENV (the shell's value or Vite's
    // per-command default) and BABEL_ENV as the config loads.
    let knobs = &oj_env::get().knobs;
    let node_env =
        knobs
            .node_env
            .as_deref()
            .unwrap_or(if build { "production" } else { "development" });
    let dev = node_env == "development" || knobs.babel_env.as_deref() == Some("development");
    // Canonical, so callers spelling the root differently share one plugin.
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let key = format!("{command}\0{dev}\0{}\0{options}", canonical.display());
    let mut shared = SHARED.lock().unwrap();
    shared.retain(|_, plugin| plugin.strong_count() > 0);
    let plugin = match shared.get(&key).and_then(std::sync::Weak::upgrade) {
        Some(plugin) => plugin,
        None => {
            let plugin = Arc::new(StylexPlugin::new(options, root, dev, !build)?);
            for warning in plugin.warnings() {
                eprintln!("{warning}");
            }
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

/// What the plugin's transform made of a module ahead of the app's plugins.
pub(crate) struct PreTransformed {
    pub code: String,
    pub map: Option<String>,
    pub rules: Vec<oj_stylex::rules::StylexRule>,
}

impl PreTransformed {
    /// A module the transform left as it was: no rules.
    pub(crate) fn unchanged(code: String) -> Self {
        Self {
            code,
            map: None,
            rules: Vec::new(),
        }
    }
}

/// The plugin's transform where Vite runs it, before the app plugins'
/// (`enforce: 'pre'`): `None` when the plugin passes over `id`.
pub(crate) fn pre_transform(
    state: &ServerState,
    id: &str,
    source: &str,
) -> Result<Option<PreTransformed>, String> {
    let Some(plugin) = &state.stylex else {
        return Ok(None);
    };
    if !plugin.handles(id, source) {
        return Ok(None);
    }
    let file = Path::new(id.split('?').next().unwrap_or(id));
    Ok(Some(match plugin.transform_source(file, source)? {
        Some(out) => PreTransformed {
            code: out.code,
            map: out.map,
            rules: out.rules,
        },
        None => PreTransformed {
            code: source.to_string(),
            map: None,
            rules: Vec::new(),
        },
    }))
}

/// Stores a served module's rules under its id (see [`module_id`]).
pub(crate) fn record(state: &ServerState, url: &str, rules: &[oj_stylex::rules::StylexRule]) {
    record_id(state, &module_id(&state.root, url), rules);
}

/// Stores a module's rules under `id`, Vite's module id: the plugin keys its
/// rule store by it, so every environment (and a Start bundle) that compiles
/// one module shares one entry, and each query variant has its own.
pub(crate) fn record_id(state: &ServerState, id: &str, rules: &[oj_stylex::rules::StylexRule]) {
    if let Some(plugin) = &state.stylex {
        plugin.set_module_rules(id, rules.to_vec());
    }
}

/// A served url's module id: the file's absolute path with the query kept,
/// minus the HMR timestamp and a dependency's `v=` version (which the SSR id
/// of the same file never carries); oj's own `/@` urls stay as they are.
fn module_id(root: &Path, url: &str) -> String {
    let url = strip_hmr_timestamp(url);
    let url = match url.split_once('?') {
        Some((path, query)) => {
            // Only the dependency version Vite and oj add (`v=` and 8 hex
            // digits): a user's own `v=` query is part of the id.
            let dep_version = |kv: &&str| {
                kv.strip_prefix("v=")
                    .is_some_and(|v| v.len() == 8 && v.bytes().all(|b| b.is_ascii_hexdigit()))
            };
            let kept: Vec<&str> = query.split('&').filter(|kv| !dep_version(kv)).collect();
            if kept.is_empty() {
                path.to_string()
            } else {
                format!("{path}?{}", kept.join("&"))
            }
        }
        None => url,
    };
    if let Some(path) = url.strip_prefix("/@fs/") {
        return format!("/{path}");
    }
    if url.starts_with("/@") {
        return url;
    }
    format!("{}{url}", root.display())
}

/// The plugin's dev routes: the stylesheet and, in `full` mode, the plain
/// runtime script. `None` for any other path.
pub(crate) fn serve_route(state: &ServerState, path: &str) -> Option<Response> {
    let plugin = state.stylex.as_ref()?;
    if path == DEV_CSS_PATH {
        let css = match dev_stylesheet(state, plugin) {
            Ok(css) => css,
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

/// The browserslist config files at the app root, whose edits change the
/// sheet's targets without changing any rule.
const BROWSERSLIST_FILES: [&str; 3] = ["package.json", ".browserslistrc", "browserslist"];

/// What a processed dev stylesheet was built from: the registry generation
/// and the root's browserslist files' mtimes.
pub(crate) type SheetStamp = (u64, [Option<std::time::SystemTime>; 3]);

/// The dev stylesheet as the plugin serves it (assembled, then lightningcss
/// for the app's browserslist and `lightningcssOptions`), rebuilt only when
/// its stamp moved: every update event makes each open tab refetch it.
fn dev_stylesheet(state: &ServerState, plugin: &StylexPlugin) -> Result<String, String> {
    let stamp: SheetStamp = (
        plugin.generation(),
        BROWSERSLIST_FILES.map(|f| {
            std::fs::metadata(state.root.join(f))
                .and_then(|m| m.modified())
                .ok()
        }),
    );
    if let Some((built, css)) = &*state.stylex_css.lock().unwrap() {
        if *built == stamp {
            return Ok(css.clone());
        }
    }
    let css = match plugin.stylesheet()? {
        Some(sheet) => oj_css::process_for_browserslist(
            "stylex.css",
            &sheet,
            &state.root,
            "development",
            plugin.lightningcss_options(),
        )?,
        None => String::new(),
    };
    *state.stylex_css.lock().unwrap() = Some((stamp, css.clone()));
    Ok(css)
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

#[cfg(test)]
mod tests {
    use super::module_id;
    use std::path::Path;

    #[test]
    fn module_ids_are_the_files_path_with_its_variant_query() {
        let root = Path::new("/app");
        assert_eq!(module_id(root, "/src/a.tsx"), "/app/src/a.tsx");
        assert_eq!(
            module_id(root, "/src/a.tsx?t=1700000000000"),
            "/app/src/a.tsx"
        );
        assert_eq!(
            module_id(root, "/src/a.tsx?alt&t=1700000000000"),
            "/app/src/a.tsx?alt"
        );
        assert_eq!(
            module_id(root, "/node_modules/ui/index.js?v=e80770d4"),
            "/app/node_modules/ui/index.js"
        );
        // A user's own queries are part of the id.
        assert_eq!(module_id(root, "/src/a.tsx?t=1"), "/app/src/a.tsx?t=1");
        assert_eq!(module_id(root, "/src/a.tsx?v=2"), "/app/src/a.tsx?v=2");
        assert_eq!(module_id(root, "/@fs/opt/lib/x.ts"), "/opt/lib/x.ts");
        assert_eq!(module_id(root, "/@id/virtual:x"), "/@id/virtual:x");
    }
}
