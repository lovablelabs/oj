// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;
use std::path::{Path, PathBuf};

use super::{build, CliOptions};

mod assets_inline_build;
mod build_assets_dir;
mod build_config_flag_build;
mod build_css_options;
mod build_mode_build;
mod build_options;
mod build_target_raw_inline_build;
mod build_unresolved;
mod config_flag_build;
mod config_function_build;
mod css_code_split_build;
mod css_import_inline_build;
mod css_plugin_served;
mod css_plugin_transform;
mod css_preprocessor_options_build;
mod css_rebase;
mod css_url_inline_build;
mod dev_condition_build;
mod dynamic_import_build;
mod emit_chunk;
mod emit_chunk_buildstart;
mod emit_html_chunk;
mod entry_names_build;
mod generate_bundle_fidelity;
mod html_attribute_quotes;
mod html_entries_build;
mod html_entry_build;
mod html_env_build;
mod inlined_entries_build;
mod jsx_import_source_build;
mod load_error_build;
mod manifest;
mod manual_chunks_build;
mod mixed_raw_query;
mod module_graph_build;
mod modulepreload;
mod multi_html_input;
mod new_url_asset_build;
mod plugin_close_hooks;
mod plugin_ctx_chain;
mod plugin_error_surfaced_build;
mod plugin_hook_gating_build;
mod plugin_load_jsx;
mod plugin_object_hooks_build;
mod plugin_sourcemap_build;
mod postcss_config_discovery_build;
mod preload_relative_base_build;
mod preprocessors_build;
mod rolldown_options;
mod root_absolute_imports_build;
mod ssr_options;
mod stylex_build;
mod svelte_build;
mod svgr_build;
mod transform_index_html_order;
mod vite_build_manifest_in_bundle;
mod vite_config_build;
mod vite_css_post_shim;
mod wasm_build;
mod worker_modes_build;

/// Headless Chrome for build checks only a page can make; shared with the
/// dev tests.
#[path = "../../../oj_server/tests/dev/browser.rs"]
pub(super) mod browser;

/// `oj preview` of `dist` on a loopback port: the same router, in-process.
/// Serving stops when the value drops.
pub(super) struct Preview {
    pub url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) async fn preview(dist: &Path, base: &str) -> Preview {
    let opts = oj_server::PreviewOptions {
        dir: dist.to_path_buf(),
        base: base.to_string(),
        spa_fallback: true,
        assets_dir: "assets".to_string(),
        ..Default::default()
    };
    let (router, _) = oj_server::preview_router(&opts).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        let _ = oj_server::tls::serve_router(listener, router, None).await;
    });
    Preview {
        url: format!("http://127.0.0.1:{port}{base}"),
        task,
    }
}

/// The start-app fixture's installed package, or None when it is missing.
pub(super) fn fixture(pkg: &str) -> Option<PathBuf> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../e2e/fixtures/start-app/node_modules")
        .join(pkg);
    path.exists().then(|| path.canonicalize().unwrap())
}

/// Symlinks each `node_modules/<pkg>` under `root` to the fixture's copy;
/// false (after saying which are missing) when any is not installed. With
/// OJ_REQUIRE_FIXTURES set (CI installs them) a missing one fails instead.
pub(super) fn link_fixtures(root: &Path, pkgs: &[&str]) -> bool {
    let missing: Vec<&str> = pkgs
        .iter()
        .copied()
        .filter(|p| fixture(p).is_none())
        .collect();
    if !missing.is_empty() {
        if std::env::var_os("OJ_REQUIRE_FIXTURES").is_some() {
            panic!(
                "OJ_REQUIRE_FIXTURES is set but {} is not installed",
                missing.join(", ")
            );
        }
        eprintln!("skipping: fixture {} not installed", missing.join(", "));
        return false;
    }
    for pkg in pkgs {
        let link = root.join("node_modules").join(pkg);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(fixture(pkg).unwrap(), link).unwrap();
    }
    true
}

/// A fresh app dir, pid-keyed and cleaned before use, never after: a later
/// run reclaims it, and nothing deletes a directory an in-process host's
/// boot chdir might still point at.
pub(super) fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("oj-api-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes `files` (relative path, contents) under `root`, creating parents.
pub(super) fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (rel, contents) in files {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
}

/// `oj build <root>` (production mode unless given) with default CLI flags.
pub(super) async fn build_app(root: &Path) -> anyhow::Result<()> {
    build(
        root.to_path_buf(),
        None,
        &tokio::runtime::Handle::current(),
        CliOptions::default(),
    )
    .await
}

/// Every file under `dir`, relative path to contents (lossy UTF-8), sorted.
pub(super) fn files_under(dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let rel = p
                    .strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((
                    rel,
                    String::from_utf8_lossy(&fs::read(&p).unwrap()).into_owned(),
                ));
            }
        }
    }
    out.sort();
    out
}

/// The concatenated contents of every `.js` file under `dir`.
pub(super) fn js_under(dir: &Path) -> String {
    files_under(dir)
        .into_iter()
        .filter(|(rel, _)| rel.ends_with(".js") || rel.ends_with(".mjs"))
        .map(|(_, code)| code)
        .collect::<Vec<_>>()
        .join("\n")
}

/// A build's in-process host chdirs the whole test process into the app root,
/// so a test deleting its root afterwards leaves every concurrent test on a
/// deleted cwd (engine loads then fail with NotFound). [`scratch`] cleans
/// before use instead.
#[test]
fn no_check_deletes_its_app_root() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/build/plugin_api_tests");
    for entry in fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let code = fs::read_to_string(&path).unwrap();
        for bad in ["remove_dir_all(&root)", "remove_dir_all(root)"] {
            assert!(!code.contains(bad), "{} calls {bad}", path.display());
        }
    }
}
