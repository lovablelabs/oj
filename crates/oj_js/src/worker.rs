// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Assembles a deno_runtime `MainWorker` with Node compatibility over the
//! app's existing node_modules (byonm), booted from the prebuilt
//! `deno_snapshots::CLI_SNAPSHOT` so startup never transpiles internal TS.

use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use deno_config::deno_json::NodeModulesDirMode;
use deno_core::url::Url;
use deno_core::v8;
use deno_core::CompiledWasmModuleStore;
use deno_core::SharedArrayBufferStore;
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
use deno_runtime::deno_web::InMemoryBroadcastChannel;
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

/// The byonm resolution stack the engine gives every isolate: workspace and
/// resolver factories over the app's own node_modules, the module and require
/// loaders, and the node-compat init services. Built once for the main worker
/// and rebuilt per worker thread by the worker callback, from ONE recipe so
/// main-thread and worker resolution semantics can never drift apart (a
/// resolver flag landing in one copy and not the other is the
/// hardest-to-debug class of worker bug).
struct EngineLoaders {
    module_loader: Rc<EngineModuleLoader>,
    node_services: NodeExtInitServices<DenoInNpmPackageChecker, NpmResolver<Sys>, Sys>,
    code_cache: Option<Arc<FsCodeCache>>,
    sys: Sys,
}

fn engine_loaders(
    root: &Path,
    code_cache_dir: Option<PathBuf>,
    host: Option<HostBridge>,
) -> Result<EngineLoaders, EngineError> {
    let sys = Sys::default();

    let code_cache = code_cache_dir.map(|dir| Arc::new(FsCodeCache::new(dir)));

    let workspace_factory = Arc::new(WorkspaceFactory::new(
        sys.clone(),
        root.to_path_buf(),
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

    let root_url = deno_path_util::url_from_directory_path(root).map_err(boot)?;
    let module_loader = Rc::new(EngineModuleLoader {
        node_resolver: node_resolver.clone(),
        npm_module_loader,
        root: root_url,
        host,
        code_cache: code_cache.clone(),
    });
    let require_loader = Rc::new(EngineRequireLoader {
        cjs_tracker,
        sys: sys.clone(),
    });

    Ok(EngineLoaders {
        module_loader,
        node_services: NodeExtInitServices {
            node_require_loader: require_loader,
            node_resolver,
            pkg_json_resolver,
            sys: sys.clone(),
        },
        code_cache,
        sys,
    })
}

/// Services one engine shares with every worker it spawns (Deno CLI shares
/// the same set): the SharedArrayBuffer and compiled-wasm stores so buffers
/// and modules can cross threads (Atomics-based pools, piscina's sync mode,
/// depend on it), the BroadcastChannel bus so Node 18+ `BroadcastChannel`
/// reaches workers, and the blob store so a blob URL minted on one thread
/// resolves on another (Node's blob registry is process-wide). The heap cap
/// travels too: a worker spawned without `resourceLimits` inherits the
/// engine's own limit rather than escaping it.
#[derive(Clone)]
struct WorkerShared {
    root: PathBuf,
    code_cache_dir: Option<PathBuf>,
    memory_limit_bytes: Option<usize>,
    blob_store: Arc<BlobStore>,
    broadcast_channel: InMemoryBroadcastChannel,
    shared_array_buffer_store: SharedArrayBufferStore,
    compiled_wasm_module_store: CompiledWasmModuleStore,
}

pub(crate) fn build_worker(
    config: &EngineConfig,
    main_module: &Url,
    host: Option<HostBridge>,
) -> Result<MainWorker, EngineError> {
    let loaders = engine_loaders(&config.root, config.code_cache_dir.clone(), host)?;

    // Trusted oj-owned scripts run against the user's project: allow-all.
    let permissions = PermissionsContainer::new(
        Arc::new(RuntimePermissionDescriptorParser::new(loaders.sys.clone())),
        Permissions::allow_all(),
    );

    let shared = WorkerShared {
        root: config.root.clone(),
        code_cache_dir: config.code_cache_dir.clone(),
        memory_limit_bytes: config.memory_limit_bytes,
        blob_store: Arc::new(BlobStore::default()),
        broadcast_channel: InMemoryBroadcastChannel::default(),
        shared_array_buffer_store: SharedArrayBufferStore::default(),
        compiled_wasm_module_store: CompiledWasmModuleStore::default(),
    };

    let services = WorkerServiceOptions::<DenoInNpmPackageChecker, NpmResolver<Sys>, Sys> {
        blob_store: shared.blob_store.clone(),
        broadcast_channel: shared.broadcast_channel.clone(),
        deno_rt_native_addon_loader: None,
        feature_checker: Default::default(),
        fs: Arc::new(RealFs),
        module_loader: loaders.module_loader,
        node_services: Some(loaders.node_services),
        npm_process_state_provider: None,
        permissions,
        root_cert_store_provider: None,
        fetch_dns_resolver: Default::default(),
        shared_array_buffer_store: Some(shared.shared_array_buffer_store.clone()),
        compiled_wasm_module_store: Some(shared.compiled_wasm_module_store.clone()),
        // Covers the CJS path: deno_runtime wires this into the eval-context
        // compile callbacks `require` goes through. The ESM and ext-script
        // paths ride the module loader (see loader.rs).
        v8_code_cache: loaders
            .code_cache
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
        create_web_worker_cb: create_web_worker_cb(shared),
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
/// workers): terser, workbox/vite-plugin-pwa, jest-worker all spawn them
/// during a plain Node build, so the deno_runtime default of panicking the
/// process here took the whole build down with it. Vite runs plugins in Node
/// where worker_threads just works; this restores that baseline.
///
/// worker_host's op runs this callback ON the spawned worker thread, so the
/// closure captures only Send data and rebuilds the byonm service stack there
/// through the same `engine_loaders` recipe as the main worker. Workers get
/// no host bridge: their payloads are dependency code (a minifier job, a
/// workbox build), which under Node resolves with plain Node semantics; the
/// Vite-style seam is a Start-SSR concern that never applies here.
fn create_web_worker_cb(
    shared: WorkerShared,
) -> Arc<deno_runtime::ops::worker_host::CreateWebWorkerCb> {
    use deno_runtime::deno_node::ops::worker_threads::ResolvedResourceLimits;
    use deno_runtime::deno_node::ops::worker_threads::DEFAULT_STACK_SIZE_MB;
    use deno_runtime::web_worker::WebWorker;
    use deno_runtime::web_worker::WebWorkerOptions;
    use deno_runtime::web_worker::WebWorkerServiceOptions;

    Arc::new(move |args| {
        // The factory getters only fail on an unreadable workspace, which the
        // parent engine already booted from. The callback type is infallible,
        // so a failure here PANICS BY DESIGN — the panic unwinds only this
        // worker thread (the workspace does not set panic=abort), the handle
        // channel drops unsent, and worker_host's recv error surfaces it to
        // JS as the worker's boot error, never a process kill.
        let loaders = engine_loaders(&shared.root, shared.code_cache_dir.clone(), None)
            .expect("worker loaders (parent booted from this workspace)");

        let services = WebWorkerServiceOptions::<DenoInNpmPackageChecker, NpmResolver<Sys>, Sys> {
            blob_store: shared.blob_store.clone(),
            broadcast_channel: shared.broadcast_channel.clone(),
            deno_rt_native_addon_loader: None,
            compiled_wasm_module_store: Some(shared.compiled_wasm_module_store.clone()),
            feature_checker: Default::default(),
            fs: Arc::new(RealFs),
            main_inspector_session_tx: Default::default(),
            module_loader: loaders.module_loader,
            node_services: Some(loaders.node_services),
            npm_process_state_provider: None,
            permissions: args.permissions,
            root_cert_store_provider: None,
            shared_array_buffer_store: Some(shared.shared_array_buffer_store.clone()),
            bundle_provider: None,
        };

        // Node `resourceLimits` -> V8 CreateParams, Deno CLI's recipe
        // (cli/lib/worker.rs, matching node_worker.cc UpdateResourceConstraints):
        // individual constraint setters, then read back the resolved values so
        // the worker_threads polyfill reports what V8 actually applied. A
        // worker spawned WITHOUT limits inherits the engine's own heap cap
        // instead of escaping it. Either way the near-heap-limit callback
        // below turns exhaustion into ERR_WORKER_OUT_OF_MEMORY for this one
        // worker; a bare CreateParams cap would abort the whole process on
        // V8's fatal OOM.
        let mb = 1024 * 1024;
        let (create_params, resolved_limits) = if let Some(ref limits) = args.resource_limits {
            let mut params = v8::CreateParams::default();
            if let Some(v) = limits.max_old_generation_size_mb.filter(|&v| v > 0) {
                params = params.set_max_old_generation_size_in_bytes(v * mb);
            }
            if let Some(v) = limits.max_young_generation_size_mb.filter(|&v| v > 0) {
                params = params.set_max_young_generation_size_in_bytes(v * mb);
            }
            if let Some(v) = limits.code_range_size_mb.filter(|&v| v > 0) {
                params = params.set_code_range_size_in_bytes(v * mb);
            }
            let resolved = ResolvedResourceLimits {
                max_young_generation_size_mb: params.max_young_generation_size_in_bytes() / mb,
                max_old_generation_size_mb: params.max_old_generation_size_in_bytes() / mb,
                code_range_size_mb: params.code_range_size_in_bytes() / mb,
                stack_size_mb: limits.stack_size_mb.unwrap_or(DEFAULT_STACK_SIZE_MB),
            };
            (Some(params), Some(resolved))
        } else {
            (
                shared
                    .memory_limit_bytes
                    .map(|limit| v8::CreateParams::default().heap_limits(0, limit)),
                None,
            )
        };
        let has_heap_limit = create_params.is_some();

        let options = WebWorkerOptions {
            name: args.name,
            main_module: args.main_module.clone(),
            worker_id: args.worker_id,
            bootstrap: BootstrapOptions {
                mode: WorkerExecutionMode::Worker,
                has_node_modules_dir: true,
                // Deno CLI gives every worker its main module as `location`.
                // Without one, the worker runtime bootstrap unconditionally
                // calls setLocationHref(null) and WorkerLocation's
                // `new URL(null)` throws `Invalid URL: 'null'` before any
                // user code runs. (The worker_threads filename itself comes
                // from op_worker_threads_filename, not from location.)
                location: Some(args.main_module.clone()),
                ..Default::default()
            },
            extensions: vec![],
            startup_snapshot: deno_snapshots::CLI_SNAPSHOT,
            residual_lazy_js_sources: deno_snapshots::RESIDUAL_LAZY_JS,
            residual_lazy_esm_sources: deno_snapshots::RESIDUAL_LAZY_ESM,
            unsafely_ignore_certificate_errors: None,
            create_params,
            seed: None,
            create_web_worker_cb: create_web_worker_cb(shared.clone()),
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

        // from_options + explicit bootstrap, not bootstrap_from_options: the
        // resolved limits must sit in op_state BEFORE bootstrap so the
        // worker_threads polyfill can read them during init (Deno CLI's
        // ordering).
        let (mut worker, handle, bootstrap_options) = WebWorker::from_options(services, options);
        if let Some(resolved) = resolved_limits {
            worker.js_runtime.op_state().borrow_mut().put(resolved);
        }
        worker.bootstrap(&bootstrap_options);

        // Graceful OOM for a capped worker: the callback flags oom_triggered
        // (run_web_worker's error handler turns it into
        // ERR_WORKER_OUT_OF_MEMORY for Node workers) and terminates only this
        // isolate; without it a capped worker dies in V8's fatal OOM, taking
        // the whole build process with it.
        if has_heap_limit {
            let ts_handle = worker.js_runtime.v8_isolate().thread_safe_handle();
            let oom_flag = worker.oom_triggered.clone();
            worker
                .js_runtime
                .add_near_heap_limit_callback(move |current_limit, _initial_limit| {
                    oom_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    ts_handle.terminate_execution();
                    current_limit * 2
                });
        }

        (worker, handle)
    })
}
