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
        create_web_worker_cb: create_web_worker_cb(
            config.root.clone(),
            config.code_cache_dir.clone(),
        ),
        ..Default::default()
    };

    Ok(MainWorker::bootstrap_from_options(
        main_module,
        services,
        options,
    ))
}

/// Web workers for the engine, which Node code reaches through the
/// `node:worker_threads` compat layer (deno_runtime implements threads on web
/// workers) — terser, workbox/vite-plugin-pwa, jest-worker all spawn them
/// during a plain Node build, so the deno_runtime default of panicking the
/// process here took the whole build down with it. Vite runs plugins in Node
/// where worker_threads just works; this restores that baseline.
///
/// worker_host's op runs this callback ON the spawned worker thread, so the
/// closure captures only Send data (paths) and rebuilds the byonm service
/// stack there, the same recipe as `build_worker`. Workers get no host
/// bridge: their payloads are dependency code (a minifier job, a workbox
/// build), which under Node resolves with plain Node semantics — the
/// Vite-style seam is a Start-SSR concern that never applies here.
fn create_web_worker_cb(
    root: std::path::PathBuf,
    code_cache_dir: Option<std::path::PathBuf>,
) -> Arc<deno_runtime::ops::worker_host::CreateWebWorkerCb> {
    use deno_runtime::web_worker::WebWorker;
    use deno_runtime::web_worker::WebWorkerOptions;
    use deno_runtime::web_worker::WebWorkerServiceOptions;

    Arc::new(move |args| {
        let sys = Sys::default();
        let code_cache = code_cache_dir
            .clone()
            .map(|dir| Arc::new(FsCodeCache::new(dir)));
        let workspace_factory = Arc::new(WorkspaceFactory::new(
            sys.clone(),
            root.clone(),
            WorkspaceFactoryOptions {
                node_modules_dir: Some(NodeModulesDirMode::Manual),
                ..Default::default()
            },
        ));
        let resolver_factory = ResolverFactory::new(
            workspace_factory,
            ResolverFactoryOptions {
                is_cjs_resolution_mode: IsCjsResolutionMode::ImplicitTypeCommonJs,
                node_analysis_cache: code_cache
                    .clone()
                    .map(|cache| cache as deno_resolver::cjs::analyzer::NodeAnalysisCacheRc),
                ..Default::default()
            },
        );
        // The factory getters only fail on an unreadable workspace, which the
        // parent engine already booted from; a worker hitting it anyway must
        // not panic the process, so surface it as the worker's boot error.
        let node_resolver = resolver_factory
            .node_resolver()
            .expect("worker node resolver (parent booted from this workspace)")
            .clone();
        let npm_module_loader = resolver_factory
            .npm_module_loader()
            .expect("worker npm module loader (parent booted from this workspace)")
            .clone();
        let cjs_tracker = resolver_factory
            .cjs_tracker()
            .expect("worker cjs tracker (parent booted from this workspace)")
            .clone();
        let pkg_json_resolver = resolver_factory.pkg_json_resolver().clone();
        let root_url = deno_path_util::url_from_directory_path(&root)
            .expect("worker root url (parent booted from this root)");

        let module_loader = Rc::new(EngineModuleLoader {
            node_resolver: node_resolver.clone(),
            npm_module_loader,
            root: root_url,
            host: None,
            code_cache: code_cache.clone(),
        });
        let require_loader = Rc::new(EngineRequireLoader {
            cjs_tracker,
            sys: sys.clone(),
        });

        let services = WebWorkerServiceOptions::<DenoInNpmPackageChecker, NpmResolver<Sys>, Sys> {
            blob_store: Arc::new(BlobStore::default()),
            broadcast_channel: Default::default(),
            deno_rt_native_addon_loader: None,
            compiled_wasm_module_store: None,
            feature_checker: Default::default(),
            fs: Arc::new(RealFs),
            main_inspector_session_tx: Default::default(),
            module_loader,
            node_services: Some(NodeExtInitServices {
                node_require_loader: require_loader,
                node_resolver,
                pkg_json_resolver,
                sys,
            }),
            npm_process_state_provider: None,
            permissions: args.permissions,
            root_cert_store_provider: None,
            shared_array_buffer_store: None,
            bundle_provider: None,
        };

        let options = WebWorkerOptions {
            name: args.name,
            main_module: args.main_module.clone(),
            worker_id: args.worker_id,
            bootstrap: BootstrapOptions {
                mode: WorkerExecutionMode::Worker,
                has_node_modules_dir: true,
                // Deno CLI gives every worker its main module as `location`;
                // the node worker_threads polyfill derives the worker's
                // filename from it, and a missing location surfaces as
                // `Invalid URL: 'null'` before user code runs.
                location: Some(args.main_module.clone()),
                ..Default::default()
            },
            extensions: vec![],
            startup_snapshot: deno_snapshots::CLI_SNAPSHOT,
            residual_lazy_js_sources: deno_snapshots::RESIDUAL_LAZY_JS,
            residual_lazy_esm_sources: deno_snapshots::RESIDUAL_LAZY_ESM,
            unsafely_ignore_certificate_errors: None,
            create_params: None,
            seed: None,
            create_web_worker_cb: create_web_worker_cb(root.clone(), code_cache_dir.clone()),
            format_js_error_fn: None,
            worker_type: args.worker_type,
            cache_storage_dir: None,
            stdio: Default::default(),
            trace_ops: None,
            close_on_idle: args.close_on_idle,
            maybe_worker_metadata: args.maybe_worker_metadata,
            maybe_main_module_blob: args.maybe_main_module_blob,
            maybe_coverage_dir: None,
            maybe_cpu_prof_config: None,
            enable_raw_imports: false,
            enable_stack_trace_arg_in_ops: false,
            wait_for_debugger_on_start: args.wait_for_debugger_on_start,
            wait_for_page_wait_for_debugger: args.wait_for_page_wait_for_debugger,
        };

        WebWorker::bootstrap_from_options(services, options)
    })
}
