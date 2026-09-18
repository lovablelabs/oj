// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Assembles a deno_runtime `MainWorker` with Node compatibility over the
//! app's existing node_modules (byonm), booted from the prebuilt
//! `deno_snapshots::CLI_SNAPSHOT` so startup never transpiles internal TS.

use std::rc::Rc;
use std::sync::Arc;

use deno_config::deno_json::NodeModulesDirMode;
use deno_core::url::Url;
use deno_core::v8;
use deno_resolver::cjs::IsCjsResolutionMode;
use deno_resolver::factory::ResolverFactory;
use deno_resolver::factory::ResolverFactoryOptions;
use deno_resolver::factory::WorkspaceFactory;
use deno_resolver::factory::WorkspaceFactoryOptions;
use deno_resolver::npm::DenoInNpmPackageChecker;
use deno_resolver::npm::NpmResolver;
use deno_runtime::deno_fs::RealFs;
use deno_runtime::deno_node::NodeExtInitServices;
use deno_runtime::deno_permissions::Permissions;
use deno_runtime::deno_permissions::PermissionsContainer;
use deno_runtime::deno_web::BlobStore;
use deno_runtime::permissions::RuntimePermissionDescriptorParser;
use deno_runtime::worker::MainWorker;
use deno_runtime::worker::WorkerOptions;
use deno_runtime::worker::WorkerServiceOptions;
use deno_runtime::BootstrapOptions;
use deno_runtime::WorkerExecutionMode;

use crate::code_cache::FsCodeCache;
use crate::host::HostBridge;
use crate::loader::EngineModuleLoader;
use crate::loader::EngineRequireLoader;
use crate::loader::Sys;
use crate::EngineConfig;
use crate::EngineError;

fn boot(e: impl std::fmt::Display) -> EngineError {
    EngineError::Boot(e.to_string())
}

pub(crate) fn build_worker(
    config: &EngineConfig,
    main_module: &Url,
    host: Option<HostBridge>,
) -> Result<MainWorker, EngineError> {
    let sys = Sys::default();

    let code_cache = config
        .code_cache_dir
        .clone()
        .map(|dir| Arc::new(FsCodeCache::new(dir)));

    let workspace_factory = Arc::new(WorkspaceFactory::new(
        sys.clone(),
        config.root.clone(),
        WorkspaceFactoryOptions {
            // byonm: resolve npm packages from the node_modules directory the
            // app's own package manager installed, never manage one ourselves.
            node_modules_dir: Some(NodeModulesDirMode::Manual),
            ..Default::default()
        },
    ));
    let resolver_factory = ResolverFactory::new(
        workspace_factory,
        ResolverFactoryOptions {
            // Node semantics: extension-ambiguous files without a package.json
            // "type" are CommonJS.
            is_cjs_resolution_mode: IsCjsResolutionMode::ImplicitTypeCommonJs,
            // Persist CJS export analysis next to the V8 code cache: the
            // swc parse it costs is a per-spawn repeat otherwise.
            node_analysis_cache: code_cache
                .clone()
                .map(|cache| cache as deno_resolver::cjs::analyzer::NodeAnalysisCacheRc),
            ..Default::default()
        },
    );

    let node_resolver = resolver_factory.node_resolver().map_err(boot)?.clone();
    let npm_module_loader = resolver_factory.npm_module_loader().map_err(boot)?.clone();
    let cjs_tracker = resolver_factory.cjs_tracker().map_err(boot)?.clone();
    let pkg_json_resolver = resolver_factory.pkg_json_resolver().clone();

    let root = deno_path_util::url_from_directory_path(&config.root).map_err(boot)?;
    let module_loader = Rc::new(EngineModuleLoader {
        node_resolver: node_resolver.clone(),
        npm_module_loader,
        root,
        host,
        code_cache: code_cache.clone(),
    });
    let require_loader = Rc::new(EngineRequireLoader {
        cjs_tracker,
        sys: sys.clone(),
    });

    // Trusted oj-owned scripts run against the user's project: allow-all.
    let permissions = PermissionsContainer::new(
        Arc::new(RuntimePermissionDescriptorParser::new(sys.clone())),
        Permissions::allow_all(),
    );

    let services = WorkerServiceOptions::<DenoInNpmPackageChecker, NpmResolver<Sys>, Sys> {
        blob_store: Arc::new(BlobStore::default()),
        broadcast_channel: Default::default(),
        deno_rt_native_addon_loader: None,
        feature_checker: Default::default(),
        fs: Arc::new(RealFs),
        module_loader,
        node_services: Some(NodeExtInitServices {
            node_require_loader: require_loader,
            node_resolver,
            pkg_json_resolver,
            sys,
        }),
        npm_process_state_provider: None,
        permissions,
        root_cert_store_provider: None,
        fetch_dns_resolver: Default::default(),
        shared_array_buffer_store: None,
        compiled_wasm_module_store: None,
        // Covers the CJS path: deno_runtime wires this into the eval-context
        // compile callbacks `require` goes through. The ESM and ext-script
        // paths ride the module loader (see loader.rs).
        v8_code_cache: code_cache
            .map(|cache| cache as Arc<dyn deno_runtime::code_cache::CodeCache>),
        bundle_provider: None,
    };

    let create_params = config
        .memory_limit_bytes
        .map(|limit| v8::CreateParams::default().heap_limits(0, limit));

    let options = WorkerOptions {
        bootstrap: BootstrapOptions {
            mode: WorkerExecutionMode::Run,
            has_node_modules_dir: true,
            ..Default::default()
        },
        startup_snapshot: deno_snapshots::CLI_SNAPSHOT,
        residual_lazy_js_sources: deno_snapshots::RESIDUAL_LAZY_JS,
        residual_lazy_esm_sources: deno_snapshots::RESIDUAL_LAZY_ESM,
        create_params,
        ..Default::default()
    };

    Ok(MainWorker::bootstrap_from_options(
        main_module,
        services,
        options,
    ))
}
