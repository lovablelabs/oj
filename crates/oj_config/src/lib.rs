// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! oj's config: the `oj.config.*` / Vite config loader and the typed
//! accessors the dev server and build read it through.

mod build;
mod css;
mod jsx;
mod load;
mod optimize;
mod resolve;
mod schema;
mod server;
mod ssr;

pub use build::{
    assets_dir_path, build_assets_dir, build_chunk_size_warning_limit, build_css_minify,
    build_css_targets, build_inputs, build_manifest_name, build_minify, build_out_dir,
    build_report_compressed_size, build_sourcemap, build_ssr_entry, build_targets,
    environment_build_bool, module_preload_links, module_preload_polyfill, rolldown_options,
    ssr_manifest_name, Sourcemap, BASELINE_WIDELY_AVAILABLE, MODULES_TARGET,
};
pub use css::{
    css_additional_data, css_load_paths, css_modules, css_preprocessor_json, CssModulesSettings,
};
pub use jsx::{jsx_settings, JsxSettings};
pub use load::{load, load_with, ConfigError};
pub use optimize::{
    optimize_deps_bundler_options, optimize_deps_force, optimize_deps_lists,
    optimize_deps_needs_interop,
};
pub use resolve::{
    node_server_conditions, node_server_external_conditions, resolve_alias, resolve_conditions,
    resolve_conditions_for, resolve_dedupe, resolve_extensions, resolve_main_fields,
    resolve_preserve_symlinks, user_external_conditions, user_resolve_conditions,
};
pub use schema::*;
pub use server::{
    config_defines, env_prefixes, environment_defines, html_csp_nonce, public_dir, server_fs_deny,
    server_strict_port, server_warmup_files,
};
pub use ssr::{package_name_of, ssr_externals, ssr_runner_backed, SsrExternals};
