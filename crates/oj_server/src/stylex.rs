//! The dev side of oj's native `@stylexjs/unplugin`: every compiled module's
//! rules feed one stylesheet, served at the plugin's endpoint, and a changed
//! sheet tells the plugin's runtimes to refetch it.

use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, OnceLock, RwLock, Weak};

use oj_stylex::plugin::{
    DevMode, StylexPlugin, CSS_UPDATE_EVENT, DEV_CSS_PATH, DEV_RUNTIME_JS, DEV_RUNTIME_PATH,
    RUNTIME_ID,
};
use oj_stylex::rules::StylexRule;
use rustc_hash::FxHashMap;

use super::*;

/// The plugin polls its rule store every 150ms and sends one update per
/// change it sees; oj coalesces changes over the same window.
const UPDATE_COALESCE: std::time::Duration = std::time::Duration::from_millis(150);

/// One live plugin per app, command and options: the dev server and a Start
/// dev bundler that load the same config share its rule store, as the plugin's
/// own store is shared across Vite's environments. Held weakly, so a session
/// that ends takes its rules with it.
static SHARED: LazyLock<Mutex<HashMap<String, Weak<StylexPlugin>>>> =
    LazyLock::new(Default::default);

/// How many plugins [`plugin_for`] has made: what [`SERVING`] remembers
/// holds until the next one.
static PLUGINS_MADE: AtomicU64 = AtomicU64::new(0);

/// What [`serving_plugin`] last found for each root, as its caller spelled it.
static SERVING: LazyLock<RwLock<FxHashMap<OsString, Serving>>> = LazyLock::new(Default::default);

struct Serving {
    /// The [`PLUGINS_MADE`] the answer was found at.
    made: u64,
    plugin: Option<Weak<StylexPlugin>>,
}

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
    let plugin = match shared.get(&key).and_then(Weak::upgrade) {
        Some(plugin) => plugin,
        None => {
            let plugin = Arc::new(StylexPlugin::new(options, root, dev, !build)?);
            for warning in plugin.warnings() {
                eprintln!("{warning}");
            }
            shared.insert(key, Arc::downgrade(&plugin));
            PLUGINS_MADE.fetch_add(1, Ordering::Release);
            plugin
        }
    };
    if !build && plugin.dev_mode() == DevMode::Off {
        return Ok(None);
    }
    Ok(Some(plugin))
}

/// The live dev plugin for `root`, as [`plugin_for`] made it for `serve`.
/// Every module a runner environment loads asks, so the answer is kept per
/// root for as long as it stands: until a plugin is made, or the one found
/// ends with its session.
pub(crate) fn serving_plugin(root: &Path) -> Option<Arc<StylexPlugin>> {
    // Read first: a plugin made while this looks leaves the answer stale.
    let made = PLUGINS_MADE.load(Ordering::Acquire);
    if let Some(found) = SERVING.read().unwrap().get(root.as_os_str()) {
        if found.made == made {
            match found.plugin.as_ref().map(Weak::upgrade) {
                Some(Some(plugin)) => return Some(plugin),
                // Its session ended.
                Some(None) => {}
                None => return None,
            }
        }
    }
    let plugin = find_serving_plugin(root);
    let mut serving = SERVING.write().unwrap();
    serving.retain(|_, found| found.made == made);
    serving.insert(
        root.into(),
        Serving {
            made,
            plugin: plugin.as_ref().map(Arc::downgrade),
        },
    );
    plugin
}

fn find_serving_plugin(root: &Path) -> Option<Arc<StylexPlugin>> {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let prefix = format!("\0{}\0", canonical.display());
    let shared = SHARED.lock().unwrap();
    shared
        .iter()
        .filter(|(key, _)| key.starts_with("serve\0") && key.contains(&prefix))
        .find_map(|(_, plugin)| plugin.upgrade())
}

/// The plugin's transform for a module a runner environment (one the plugin
/// host runs through the app's Vite, a Cloudflare worker's) loads: `None`
/// when the plugin passes over it. Its rules join the sheet under the same id
/// the browser's copy records, as the plugin's store is shared across Vite's
/// environments.
pub(crate) fn runner_transform(
    root: &Path,
    id: &str,
    code: &str,
) -> Result<Option<(String, Option<String>)>, String> {
    let Some(plugin) = serving_plugin(root) else {
        return Ok(None);
    };
    if !plugin.handles(id, code) {
        // An edit dropped the module's StyleX imports: its rules leave the
        // sheet, as they do when it compiles to nothing.
        plugin.clear_module_rules(id);
        return Ok(None);
    }
    let file = Path::new(id.split('?').next().unwrap_or(id));
    let Some(out) = plugin.transform_source(file, code)? else {
        // Nothing compiled: the module's earlier rules leave the sheet.
        plugin.set_module_rules(id, Vec::new());
        return Ok(None);
    };
    plugin.set_module_rules(id, out.rules);
    Ok(Some((out.code, out.map)))
}

/// Sends the plugin's update event to the dev clients after a sheet change,
/// one per coalescing window.
pub(crate) fn notify_clients(plugin: &StylexPlugin, tx: broadcast::Sender<String>) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let pending = Arc::new(AtomicBool::new(false));
    let event = serde_json::json!({ "type": "custom", "event": CSS_UPDATE_EVENT }).to_string();
    plugin.set_change_listener(Arc::new(move || {
        // Every change within a window but the first only reads the flag.
        if pending.load(Ordering::Acquire) || pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let (pending, tx, event) = (Arc::clone(&pending), tx.clone(), event.clone());
        runtime.spawn(async move {
            tokio::time::sleep(UPDATE_COALESCE).await;
            pending.store(false, Ordering::Release);
            let _ = tx.send(event);
        });
    }));
}

/// What the plugin's transform made of a module ahead of the app's plugins.
pub(crate) struct PreTransformed {
    pub code: String,
    pub map: Option<String>,
    pub rules: Vec<StylexRule>,
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
pub(crate) fn record(state: &ServerState, url: &str, rules: &[StylexRule]) {
    if let Some(plugin) = &state.stylex {
        record_url(plugin, &state.root, url, rules);
    }
}

fn record_url(plugin: &StylexPlugin, root: &Path, url: &str, rules: &[StylexRule]) {
    plugin.record_module_rules(&module_id(root, url), rules);
}

/// Stores a module's rules under `id`, Vite's module id: the plugin keys its
/// rule store by it, so every environment (and a Start bundle) that compiles
/// one module shares one entry, and each query variant has its own.
pub(crate) fn record_id(state: &ServerState, id: &str, rules: &[StylexRule]) {
    if let Some(plugin) = &state.stylex {
        plugin.record_module_rules(id, rules);
    }
}

/// A served url's module id: the file's absolute path with the query kept,
/// minus the HMR timestamp and a dependency's `v=` version (which the SSR id
/// of the same file never carries); oj's own `/@` urls stay as they are.
fn module_id(root: &Path, url: &str) -> String {
    let (path, query) = match url.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (url, None),
    };
    let mut id = String::with_capacity(root.as_os_str().len() + url.len());
    if let Some(path) = path.strip_prefix("/@fs/") {
        id.push('/');
        id.push_str(path);
    } else if crate::is_dev_server_path(path) {
        id.push_str(path);
    } else {
        id.push_str(&root.to_string_lossy());
        id.push_str(path);
    }
    // Only the dependency version Vite and oj add (`v=` and 8 hex digits):
    // a user's own `v=` query is part of the id.
    let dep_version = |kv: &str| {
        kv.strip_prefix("v=")
            .is_some_and(|v| v.len() == 8 && v.bytes().all(|b| b.is_ascii_hexdigit()))
    };
    let mut separator = '?';
    for kv in query.into_iter().flat_map(|query| query.split('&')) {
        if is_hmr_timestamp(kv) || dep_version(kv) {
            continue;
        }
        id.push(separator);
        id.push_str(kv);
        separator = '&';
    }
    id
}

/// The plugin's dev routes: the stylesheet and, in `full` mode, the plain
/// runtime script. `None` for any other path.
pub(crate) fn serve_route(state: &ServerState, path: &str) -> Option<Response> {
    let plugin = state.stylex.as_ref()?;
    if path == DEV_CSS_PATH {
        let css = match state.stylex_css.css(&state.root, plugin) {
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

/// The browserslist files' mtimes.
type TargetsStamp = [Option<SystemTime>; 3];

/// How long a read of the browserslist files' mtimes stands. The tabs fetch
/// the sheet in bursts (each update event, every tab), and a poll that finds
/// nothing new should cost no syscall; an edit to the app's browserslist
/// tells no tab to refetch, so it reaches the first fetch this long after it.
const TARGETS_RECHECK: Duration = UPDATE_COALESCE;

/// The dev stylesheet as the plugin serves it (assembled, then lightningcss
/// for the app's browserslist and `lightningcssOptions`), rebuilt only when
/// the rule store or the browserslist files moved: every update event makes
/// each open tab refetch it.
#[derive(Default)]
pub(crate) struct DevSheet {
    /// Whether `lightningcssOptions` names the targets, which then never
    /// come from the browserslist files.
    own_targets: OnceLock<bool>,
    /// The browserslist files' mtimes, and when they were read.
    targets: RwLock<Option<(Instant, TargetsStamp)>>,
    /// The sheet polls are answered with, and the store generation and
    /// targets it was built from.
    served: RwLock<Option<(u64, TargetsStamp, Bytes)>>,
    /// Held across a rebuild: tabs refetching at once wait for one.
    built: Mutex<Option<Built>>,
}

/// The last lightningcss pass: what went in, what came out.
struct Built {
    assembled: Option<String>,
    targets: TargetsStamp,
    css: Bytes,
}

impl DevSheet {
    pub(crate) fn css(&self, root: &Path, plugin: &StylexPlugin) -> Result<Bytes, String> {
        let targets = self.targets(root, plugin);
        if let Some(css) = self.served(plugin.generation(), &targets) {
            return Ok(css);
        }
        let mut built = self.built.lock().unwrap();
        // The generation the assembled sheet is at least as new as.
        let generation = plugin.generation();
        if let Some(css) = self.served(generation, &targets) {
            return Ok(css);
        }
        let assembled = plugin.stylesheet()?;
        let css = match &*built {
            // The store moved and the sheet did not (a module whose rules
            // others hold too came or went): lightningcss has nothing new.
            Some(last) if last.assembled == assembled && last.targets == targets => {
                last.css.clone()
            }
            _ => {
                let css = match &assembled {
                    Some(sheet) => Bytes::from(oj_css::process_for_browserslist(
                        "stylex.css",
                        sheet,
                        root,
                        "development",
                        plugin.lightningcss_options(),
                    )?),
                    None => Bytes::new(),
                };
                *built = Some(Built {
                    assembled,
                    targets,
                    css: css.clone(),
                });
                css
            }
        };
        *self.served.write().unwrap() = Some((generation, targets, css.clone()));
        Ok(css)
    }

    fn served(&self, generation: u64, targets: &TargetsStamp) -> Option<Bytes> {
        match &*self.served.read().unwrap() {
            Some((built, from, css)) if *built == generation && from == targets => {
                Some(css.clone())
            }
            _ => None,
        }
    }

    fn targets(&self, root: &Path, plugin: &StylexPlugin) -> TargetsStamp {
        let own_targets = *self.own_targets.get_or_init(|| {
            plugin
                .lightningcss_options()
                .get("targets")
                .is_some_and(serde_json::Value::is_object)
        });
        if own_targets {
            return TargetsStamp::default();
        }
        let now = Instant::now();
        if let Some((read, stamp)) = *self.targets.read().unwrap() {
            if now.saturating_duration_since(read) < TARGETS_RECHECK {
                return stamp;
            }
        }
        let stamp = BROWSERSLIST_FILES.map(|f| {
            std::fs::metadata(root.join(f))
                .and_then(|m| m.modified())
                .ok()
        });
        *self.targets.write().unwrap() = Some((now, stamp));
        stamp
    }
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
    use super::*;
    use serde_json::json;

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
        assert_eq!(
            module_id(root, "/src/a.tsx?t=1700000000000&alt&v=e80770d4&lang.ts"),
            "/app/src/a.tsx?alt&lang.ts"
        );
        // A user's own queries are part of the id.
        assert_eq!(module_id(root, "/src/a.tsx?t=1"), "/app/src/a.tsx?t=1");
        assert_eq!(module_id(root, "/src/a.tsx?v=2"), "/app/src/a.tsx?v=2");
        assert_eq!(module_id(root, "/src/a.tsx?"), "/app/src/a.tsx?");
        assert_eq!(module_id(root, "/@fs/opt/lib/x.ts"), "/opt/lib/x.ts");
        assert_eq!(
            module_id(root, "/@fs/opt/lib/x.ts?v=e80770d4&raw"),
            "/opt/lib/x.ts?raw"
        );
        assert_eq!(module_id(root, "/@id/virtual:x"), "/@id/virtual:x");
        assert_eq!(
            module_id(root, "/@x/a.tsx?t=1700000000000"),
            "/app/@x/a.tsx"
        );
        assert_eq!(
            module_id(root, "/@id/virtual:x?t=1700000000000"),
            "/@id/virtual:x"
        );
    }

    fn plugin(root: &Path, options: serde_json::Value) -> StylexPlugin {
        StylexPlugin::new(&options, root, true, true).unwrap()
    }

    fn rule(class: &str, declaration: &str) -> StylexRule {
        StylexRule {
            class_name: class.into(),
            ltr: format!(".{class}{{{declaration}}}").into(),
            rtl: None,
            const_key: None,
            const_val: None,
            priority: 3000.0,
        }
    }

    fn text(css: &Bytes) -> &str {
        std::str::from_utf8(css).unwrap()
    }

    #[test]
    fn the_dev_sheet_is_processed_once_per_sheet() {
        let dir = tempfile::tempdir().unwrap();
        let (root, p) = (dir.path(), plugin(dir.path(), json!({})));
        let sheet = DevSheet::default();
        assert!(sheet.css(root, &p).unwrap().is_empty());

        assert!(p.record_module_rules("/app/a.ts", &[rule("xa", "color:red")]));
        let first = sheet.css(root, &p).unwrap();
        assert!(text(&first).contains("color: red"), "{}", text(&first));
        // A poll that finds nothing new is answered with the same bytes.
        assert_eq!(sheet.css(root, &p).unwrap().as_ptr(), first.as_ptr());

        // The store moved and the sheet did not: no second lightningcss pass.
        assert!(p.record_module_rules("/app/b.ts", &[rule("xa", "color:red")]));
        assert_eq!(sheet.css(root, &p).unwrap().as_ptr(), first.as_ptr());

        assert!(p.record_module_rules("/app/a.ts", &[rule("xb", "color:blue")]));
        let edited = sheet.css(root, &p).unwrap();
        assert!(
            text(&edited).contains(".xb") && text(&edited).contains(".xa"),
            "{}",
            text(&edited)
        );

        assert!(p.record_module_rules("/app/a.ts", &[]));
        assert!(p.record_module_rules("/app/b.ts", &[]));
        assert!(sheet.css(root, &p).unwrap().is_empty());
    }

    #[test]
    fn tabs_refetching_at_once_wait_for_one_sheet() {
        let dir = tempfile::tempdir().unwrap();
        let (root, p) = (dir.path(), plugin(dir.path(), json!({})));
        let sheet = DevSheet::default();
        let rules: Vec<StylexRule> = (0..200)
            .map(|i| rule(&format!("x{i}"), &format!("margin-top:{i}px")))
            .collect();
        assert!(p.record_module_rules("/app/a.ts", &rules));
        let start = std::sync::Barrier::new(8);
        let fetched: Vec<usize> = std::thread::scope(|scope| {
            let tabs: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        start.wait();
                        sheet.css(root, &p).unwrap().as_ptr() as usize
                    })
                })
                .collect();
            tabs.into_iter().map(|tab| tab.join().unwrap()).collect()
        });
        assert!(fetched.iter().all(|css| *css == fetched[0]), "{fetched:?}");
    }

    #[test]
    fn a_browserslist_edit_reaches_the_dev_sheet() {
        let dir = tempfile::tempdir().unwrap();
        let (root, p) = (dir.path(), plugin(dir.path(), json!({})));
        std::fs::write(root.join(".browserslistrc"), "chrome 80\n").unwrap();
        assert!(p.record_module_rules("/app/a.ts", &[rule("xa", "inset:0")]));
        let sheet = DevSheet::default();
        let old = sheet.css(root, &p).unwrap();
        assert!(
            text(&old).contains("top: 0") && !text(&old).contains("inset"),
            "{}",
            text(&old)
        );

        // No rule changes: the files' mtimes alone say the targets moved.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(root.join(".browserslistrc"), "chrome 120\n").unwrap();
        std::thread::sleep(TARGETS_RECHECK + Duration::from_millis(50));
        let new = sheet.css(root, &p).unwrap();
        assert!(text(&new).contains("inset: 0"), "{}", text(&new));
    }

    #[test]
    fn targets_the_options_name_leave_the_browserslist_files_unread() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let p = plugin(
            root,
            json!({ "lightningcssOptions": { "targets": { "chrome": 120 << 16 } } }),
        );
        std::fs::write(root.join(".browserslistrc"), "chrome 80\n").unwrap();
        assert!(p.record_module_rules("/app/a.ts", &[rule("xa", "inset:0")]));
        let sheet = DevSheet::default();
        let css = sheet.css(root, &p).unwrap();
        assert!(text(&css).contains("inset: 0"), "{}", text(&css));
        assert!(sheet.targets.read().unwrap().is_none());
    }

    #[test]
    fn the_serving_plugin_is_found_until_its_session_ends() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("src")).unwrap();
        let config = oj_config::OjConfig {
            vite_stylex: Some(json!({ "options": {} })),
            ..Default::default()
        };
        assert!(serving_plugin(root).is_none());

        let serving = plugin_for(root, &config, "serve").unwrap().unwrap();
        let found = |root: &Path| serving_plugin(root).expect("the dev plugin");
        assert!(Arc::ptr_eq(&found(root), &serving));
        assert!(Arc::ptr_eq(&found(root), &serving));
        // However the caller spells the root.
        assert!(Arc::ptr_eq(&found(&root.join("src/..")), &serving));
        // A build's plugin is not the one serving.
        let building = plugin_for(root, &config, "build").unwrap().unwrap();
        assert!(Arc::ptr_eq(&found(root), &serving));

        drop((serving, building));
        assert!(serving_plugin(root).is_none());
        let next = plugin_for(root, &config, "serve").unwrap().unwrap();
        assert!(Arc::ptr_eq(&found(root), &next));
    }
}
