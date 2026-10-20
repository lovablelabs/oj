// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Dev server behavior checked in-process: `DevServer::build_app()` builds the
//! same router `oj dev` serves, and each request goes through it with
//! `oneshot`, so a test needs no port, no child process and no polling. One
//! binary for all of them (each integration binary links V8), and an
//! integration test so build.rs's exported-symbols link flag applies to the
//! napi addons a plugin host loads.

mod assets;
mod browser;
mod browser_errors;
mod build_config_flag;
mod config_env;
mod config_flag;
mod config_function;
mod config_proxy;
mod config_temp_file;
mod config_wrapper;
mod css_import_inline;
mod css_importer_invalidation;
mod css_outside_root_deps;
mod css_preprocessor_options;
mod css_url_inline;
mod dep_plugin_load;
mod dev_condition;
mod dev_cors_host;
mod dev_mode;
mod dev_process_env;
mod dunder_paths;
mod dynamic_import;
mod fs_allow_node_modules;
mod hmr_client_behaviors;
mod hmr_dep_update;
mod hmr_disabled;
mod hmr_gate;
mod hmr_hot_accept;
mod hmr_protocol;
mod host_binding;
mod hot_update_burst;
mod html_entry;
mod html_env;
mod html_proxy;
mod jsx_import_source;
mod lazy_flag;
mod lingui_macro_shim;
mod listen;
mod new_url_asset;
mod optimize_alias;
mod optimize_linked;
mod optimize_plan;
mod plugin_close_hooks;
mod plugin_ctx_chain;
mod plugin_error_surfaced;
mod plugin_middleware;
mod plugin_module_graph;
mod plugin_object_hooks;
mod plugin_parse;
mod plugin_skip;
mod plugin_watch_change;
mod plugin_ws;
mod plugin_ws_execute;
mod postcss_config_discovery;
mod preprocessors;
mod query_assets;
mod resolve_created_files;
mod svelte;
mod svgr;
mod tailwind_js_import;
mod tailwind_sheet_stale;
mod unresolved_bare;
mod vite_hmr_socket;
mod warm_transform_virtual;
mod wasm;
mod watch_reads;
mod watcher;
mod worker_modes;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use axum::body::Body;
use axum::http::{HeaderMap, Request};
use tower::util::ServiceExt;

/// A scratch app under this run's dir. Never deleted while the run lives:
/// an in-process plugin host chdirs the process into its app root, so a
/// removed dir could be every later test's cwd. The next run sweeps it.
pub struct App {
    pub root: PathBuf,
}

/// This process's parent dir for apps; the first call removes the dirs of
/// runs whose process is gone.
pub fn run_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let tmp = std::env::temp_dir().canonicalize().unwrap();
        for entry in std::fs::read_dir(&tmp).into_iter().flatten().flatten() {
            let name = entry.file_name();
            let pid = name.to_str().and_then(|n| n.strip_prefix("oj-dev-tests-"));
            if let Some(pid) = pid.and_then(|p| p.parse::<i32>().ok()) {
                // SAFETY: signal 0 only probes whether the pid exists.
                if unsafe { libc::kill(pid, 0) } != 0 {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
        tmp.join(format!("oj-dev-tests-{}", std::process::id()))
    })
}

impl App {
    pub fn new(files: &[(&str, &str)]) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = run_dir().join(NEXT.fetch_add(1, Ordering::Relaxed).to_string());
        std::fs::create_dir_all(&root).unwrap();
        let app = App { root };
        app.write(files);
        app
    }

    pub fn write(&self, files: &[(&str, &str)]) {
        for (rel, body) in files {
            let path = self.root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
    }

    /// Symlinks `node_modules/<pkg>` to the start-app fixture's copy.
    pub fn link_fixture(&self, pkg: &str) {
        let target = fixture(pkg).unwrap_or_else(|| panic!("fixture {pkg} installed"));
        let link = self.root.join("node_modules").join(pkg);
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    /// `oj dev <root>` with the CLI defaults; adjust fields before `serve`.
    pub fn dev(&self) -> oj_server::DevServer {
        oj_server::DevServer {
            root: self.root.clone(),
            port: None,
            host: None,
            config: None,
            enable_cache: false,
            no_cache: false,
            lazy: false,
            mode: None,
        }
    }

    pub async fn serve(&self) -> Server {
        serve(self.dev()).await
    }
}

/// The start-app fixture's installed package, or None when the fixture is
/// not installed (CI's first workspace run): callers skip on None.
pub fn fixture(pkg: &str) -> Option<PathBuf> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../e2e/fixtures/start-app/node_modules")
        .join(pkg);
    path.exists().then(|| path.canonicalize().unwrap())
}

/// Returns early from a test when the fixture lacks `pkg`.
#[macro_export]
macro_rules! require_fixture {
    ($($pkg:literal),+) => {
        $(if $crate::fixture($pkg).is_none() {
            // CI installs the fixture; a skip there would pass the test unrun.
            assert!(
                std::env::var_os("OJ_REQUIRE_FIXTURES").is_none(),
                "OJ_REQUIRE_FIXTURES is set but fixture {} is not installed",
                $pkg
            );
            eprintln!("skipping: fixture {} not installed", $pkg);
            return;
        })+
    };
}

pub struct Server {
    pub built: oj_server::BuiltApp,
    root: PathBuf,
}

/// Restarts asked for so far, by any server in this process.
pub(crate) static RESTARTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Whether a restart belongs to the app at `root`: a watcher restart names
/// the file that changed ("<file> changed"), relative to the cwd or absolute.
fn restart_is_under(reason: &str, root: &Path) -> bool {
    let Some(file) = reason.strip_suffix(" changed") else {
        return false;
    };
    let file = std::env::current_dir().unwrap().join(file);
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    file.starts_with(root) || file.starts_with(&canonical)
}

/// Takes out the recorded restarts `keep` claims, leaving the rest.
fn take_restarts_where(keep: impl Fn(&str) -> bool) -> Vec<String> {
    let mut all = RESTARTS.lock().unwrap();
    let (ours, rest) = std::mem::take(&mut *all)
        .into_iter()
        .partition(|r: &String| keep(r));
    *all = rest;
    ours
}

/// The restarts asked for so far over a file under `root`, taken out: for a
/// test that expects one. Any other server's restart stays for its own drop
/// to fail on.
pub fn take_restarts_for(root: &Path) -> Vec<String> {
    take_restarts_where(|r| restart_is_under(r, root))
}

/// No test expects a restart, so one over this app's files fails the test
/// as its server drops (a panic on the asking thread would never reach a
/// test). A restart naming no file (a plugin's `server.restart()`) can't be
/// told apart, so it fails whichever server drops next.
impl Drop for Server {
    fn drop(&mut self) {
        self.built.close_watcher();
        let root = self.root.clone();
        let restarts =
            take_restarts_where(|r| restart_is_under(r, &root) || !r.ends_with(" changed"));
        if !restarts.is_empty() && !std::thread::panicking() {
            panic!("a dev server asked for a restart: {restarts:?}");
        }
    }
}

/// A restart would exec the test binary over every running test: record it
/// and park the asking thread (the watcher, or a plugin's server.restart()
/// task), as the exec would have stopped it.
fn restart_in_test(reason: &str) -> ! {
    RESTARTS.lock().unwrap().push(reason.to_string());
    loop {
        std::thread::park();
    }
}

pub async fn serve(dev: oj_server::DevServer) -> Server {
    oj_server::set_restart_hook(restart_in_test);
    let root = dev.root.clone();
    match dev.build_app().await {
        Ok(built) => {
            // An edit a test makes must not race the watcher's start.
            built.watcher_ready().await;
            Server { built, root }
        }
        Err(err) => panic!("dev server failed to build:\n{err:#}"),
    }
}

pub struct Response {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: String,
}

impl Server {
    pub async fn get(&self, path: &str) -> Response {
        self.request(Request::get(path).body(Body::empty()).unwrap())
            .await
    }

    /// Sends `req` with `Host: localhost` unless it carries its own.
    pub async fn request(&self, mut req: Request<Body>) -> Response {
        if !req.headers().contains_key("host") {
            req.headers_mut()
                .insert("host", "localhost".parse().unwrap());
        }
        let res = self.built.router.clone().oneshot(req).await.unwrap();
        let status = res.status().as_u16();
        let headers = res.headers().clone();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        Response {
            status,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        }
    }

    /// GET that must answer 200; returns the body.
    pub async fn text(&self, path: &str) -> String {
        let res = self.get(path).await;
        assert_eq!(res.status, 200, "GET {path}:\n{}", res.body);
        res.body
    }
}

pub const INDEX_HTML: &str = r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#;
