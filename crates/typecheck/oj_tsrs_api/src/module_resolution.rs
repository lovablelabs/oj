// Port of tsc/internal/api/module_resolution.go (static + callback module resolvers, createModuleResolver /
// releaseModuleResolver / resolveModuleName) and tsc/internal/module/staticresolver.go.

use std::collections::hash_map::Entry;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use rustc_hash::FxHashMap;
use tsrs_core::context::Context;
use tsrs_core::json::{self, Value};
use tsrs_core::tspath::{self, Path};
use tsrs_core::{CompilerOptions, ModuleKind, P, ResolutionMode, alloc_str};
use tsrs_module::{
    DiagAndArgs, ResolutionData, ResolutionHost, ResolvedModule, ResolvedProjectReference,
    ResolvedTypeReferenceDirective, Resolver, ResolverOptions,
};
use tsrs_project::ModuleResolverFactory;
use tsrs_vfs::FS;

use crate::handler::{ApiError, ApiResult, ClientConn};
use crate::session::Session;
use crate::wire::{DocumentIdentifier, Obj, Params, b, s, strings};

#[derive(Clone, PartialEq, Eq, Hash)]
struct StaticKey {
    module_name: String,
    directory: Option<Path>,
    mode: Option<ResolutionMode>,
}

/// Go `module.StaticResolutions`.
pub struct StaticResolutions {
    fallback_to_resolver: bool,
    entries: FxHashMap<StaticKey, P<ResolvedModule>>,
    current_directory: String,
    use_case_sensitive_file_names: bool,
}

impl StaticResolutions {
    fn lookup(
        &self,
        module_name: &str,
        containing_directory: &str,
        mode: ResolutionMode,
    ) -> Option<P<ResolvedModule>> {
        let directory = tspath::to_path(
            containing_directory,
            &self.current_directory,
            self.use_case_sensitive_file_names,
        );
        let m = module_name.to_string();
        let keys = [
            StaticKey {
                module_name: m.clone(),
                directory: Some(directory.clone()),
                mode: Some(mode),
            },
            StaticKey { module_name: m.clone(), directory: Some(directory), mode: None },
            StaticKey { module_name: m.clone(), directory: None, mode: Some(mode) },
            StaticKey { module_name: m, directory: None, mode: None },
        ];
        keys.iter().find_map(|k| self.entries.get(k).copied())
    }
}

fn unresolved() -> P<ResolvedModule> {
    P::new(ResolvedModule::default())
}

/// Go `module.StaticResolver`.
struct StaticResolver<'a> {
    fallback: Box<dyn Resolver + 'a>,
    resolutions: Arc<StaticResolutions>,
}

impl Resolver for StaticResolver<'_> {
    fn resolve_module_name(
        &self,
        module_name: &str,
        containing_file: &str,
        mode: ResolutionMode,
        redirected: Option<&dyn ResolvedProjectReference>,
    ) -> Result<(P<ResolvedModule>, Vec<DiagAndArgs>), String> {
        if let Some(r) =
            self.resolutions.lookup(module_name, &tspath::get_directory_path(containing_file), mode)
        {
            return Ok((r, Vec::new()));
        }
        if !self.resolutions.fallback_to_resolver {
            return Ok((unresolved(), Vec::new()));
        }
        self.fallback.resolve_module_name(module_name, containing_file, mode, redirected)
    }
    fn resolve_module_name_from_directory(
        &self,
        module_name: &str,
        dir: &str,
        mode: ResolutionMode,
    ) -> Result<(P<ResolvedModule>, Vec<DiagAndArgs>), String> {
        if let Some(r) = self.resolutions.lookup(module_name, dir, mode) {
            return Ok((r, Vec::new()));
        }
        if !self.resolutions.fallback_to_resolver {
            return Ok((unresolved(), Vec::new()));
        }
        self.fallback.resolve_module_name_from_directory(module_name, dir, mode)
    }
    fn resolve_type_reference_directive(
        &self,
        name: &str,
        containing_file: &str,
        mode: ResolutionMode,
        redirected: Option<&dyn ResolvedProjectReference>,
    ) -> (P<ResolvedTypeReferenceDirective>, Vec<DiagAndArgs>) {
        self.fallback.resolve_type_reference_directive(name, containing_file, mode, redirected)
    }
    fn get_resolution_data(&self) -> P<ResolutionData> {
        self.fallback.get_resolution_data()
    }
}

/// Go `moduleResolverRegistration`.
pub(crate) struct ModuleResolverRegistration {
    id: u64,
    compiler_options: P<CompilerOptions>,
    resolutions: Option<Arc<StaticResolutions>>,
    callback: String,
}

/// Session-owned module resolver state.
#[derive(Default)]
pub(crate) struct ModuleResolvers {
    next_id: AtomicU64,
    registrations: Mutex<FxHashMap<u64, Arc<ModuleResolverRegistration>>>,
    next_context_id: AtomicU64,
    /// Go `programResolutionContexts`: resolvers live while a program is being built with a callback resolver.
    contexts: Mutex<FxHashMap<u64, Arc<ProgramResolutionContext>>>,
}

pub(crate) struct ProgramResolutionContext {
    options: ResolverOptionsTemplate,
    resolvers: Mutex<FxHashMap<u64, Arc<dyn Resolver>>>,
}

/// The parts of `module.ResolverOptions` needed to build another resolver for the same program.
#[derive(Clone)]
struct ResolverOptionsTemplate {
    host: &'static dyn ResolutionHost,
    typings_location: String,
    project_name: String,
    extra_extensions: Vec<String>,
}

impl ResolverOptionsTemplate {
    fn from(o: &ResolverOptions<'static>) -> Self {
        ResolverOptionsTemplate {
            host: o.host,
            typings_location: o.typings_location.clone(),
            project_name: o.project_name.clone(),
            extra_extensions: o.extra_extensions.clone(),
        }
    }
    fn with_options(&self, compiler_options: P<CompilerOptions>) -> ResolverOptions<'static> {
        let mut o = ResolverOptions::new(self.host, compiler_options);
        o.typings_location.clone_from(&self.typings_location);
        o.project_name.clone_from(&self.project_name);
        o.extra_extensions.clone_from(&self.extra_extensions);
        o
    }
}

/// Arc-shareable resolver (programs take a `Box<dyn Resolver>`; contexts keep a shared handle).
struct SharedResolver(Arc<dyn Resolver>);

impl Resolver for SharedResolver {
    fn resolve_module_name(
        &self,
        a: &str,
        b: &str,
        c: ResolutionMode,
        d: Option<&dyn ResolvedProjectReference>,
    ) -> Result<(P<ResolvedModule>, Vec<DiagAndArgs>), String> {
        self.0.resolve_module_name(a, b, c, d)
    }
    fn resolve_module_name_from_directory(
        &self,
        a: &str,
        b: &str,
        c: ResolutionMode,
    ) -> Result<(P<ResolvedModule>, Vec<DiagAndArgs>), String> {
        self.0.resolve_module_name_from_directory(a, b, c)
    }
    fn resolve_type_reference_directive(
        &self,
        a: &str,
        b: &str,
        c: ResolutionMode,
        d: Option<&dyn ResolvedProjectReference>,
    ) -> (P<ResolvedTypeReferenceDirective>, Vec<DiagAndArgs>) {
        self.0.resolve_type_reference_directive(a, b, c, d)
    }
    fn get_resolution_data(&self) -> P<ResolutionData> {
        self.0.get_resolution_data()
    }
}

/// Go `callbackModuleResolver`: asks the client, falling back for type reference directives.
struct CallbackResolver<'a> {
    registration: Arc<ModuleResolverRegistration>,
    conn: Arc<dyn ClientConn>,
    current_directory: String,
    snapshot: u64,
    context_id: u64,
    fallback: Box<dyn Resolver + 'a>,
}

impl CallbackResolver<'_> {
    fn resolve(
        &self,
        module_name: &str,
        containing_directory: &str,
        mode: ResolutionMode,
    ) -> Result<(P<ResolvedModule>, Vec<DiagAndArgs>), String> {
        let params = Obj::new()
            .set("moduleName", s(module_name))
            .set("containingDirectory", s(containing_directory))
            .set("resolutionMode", Value::Number(mode.value() as f64))
            .set_opt("snapshot", (self.snapshot != 0).then(|| Value::Number(self.snapshot as f64)))
            .set_opt(
                "inProgressSnapshot",
                (self.context_id != 0).then(|| Value::Number(self.context_id as f64)),
            )
            .build();
        let text = json::marshal(&params)?;
        let result = self
            .conn
            .call(&self.registration.callback, &text)
            .map_err(|e| format!("resolveModuleName callback failed: {e}"))?;
        if result.is_empty() || result == "null" {
            return Ok((unresolved(), Vec::new()));
        }
        let v = json::unmarshal(&result)
            .map_err(|e| format!("invalid resolveModuleName callback result: {e}"))?;
        let r = static_resolution_to_resolved_module(&v, &self.current_directory)
            .map_err(|e| format!("invalid resolveModuleName callback result: {e}"))?;
        Ok((r, Vec::new()))
    }
}

impl Resolver for CallbackResolver<'_> {
    fn resolve_module_name(
        &self,
        module_name: &str,
        containing_file: &str,
        mode: ResolutionMode,
        _redirected: Option<&dyn ResolvedProjectReference>,
    ) -> Result<(P<ResolvedModule>, Vec<DiagAndArgs>), String> {
        self.resolve(module_name, &tspath::get_directory_path(containing_file), mode)
    }
    fn resolve_module_name_from_directory(
        &self,
        module_name: &str,
        dir: &str,
        mode: ResolutionMode,
    ) -> Result<(P<ResolvedModule>, Vec<DiagAndArgs>), String> {
        self.resolve(module_name, dir, mode)
    }
    fn resolve_type_reference_directive(
        &self,
        name: &str,
        containing_file: &str,
        mode: ResolutionMode,
        redirected: Option<&dyn ResolvedProjectReference>,
    ) -> (P<ResolvedTypeReferenceDirective>, Vec<DiagAndArgs>) {
        self.fallback.resolve_type_reference_directive(name, containing_file, mode, redirected)
    }
    fn get_resolution_data(&self) -> P<ResolutionData> {
        self.fallback.get_resolution_data()
    }
}

/// Go `staticModuleResolutionToResolvedModule` (`StaticModuleResolution` JSON).
fn static_resolution_to_resolved_module(v: &Value, cwd: &str) -> ApiResult<P<ResolvedModule>> {
    let p = Params(v);
    if matches!(v, Value::Null) || !p.has("resolvedFileName") {
        return Ok(unresolved());
    }
    let abs = |key: &str| -> ApiResult<String> {
        let d = DocumentIdentifier::parse(p.get(key), key)?;
        Ok(tspath::get_normalized_absolute_path(&d.to_absolute_file_name(cwd), cwd))
    };
    let resolved_file_name = abs("resolvedFileName")?;
    let original_path = if p.has("originalPath") { abs("originalPath")? } else { String::new() };
    let mut r = ResolvedModule::default();
    if p.has("packageId") {
        let pkg = Params(p.get("packageId"));
        r.package_id = tsrs_module::PackageId {
            name: alloc_str(pkg.str("name")?),
            sub_module_name: alloc_str(pkg.str("subModuleName")?),
            version: alloc_str(pkg.str("version")?),
            peer_dependencies: alloc_str(pkg.str("peerDependencies")?),
        };
    }
    let original_for_check =
        if original_path.is_empty() { &resolved_file_name } else { &original_path };
    r.is_external_library_import = original_for_check.contains("/node_modules/");
    r.extension = tspath::try_get_extension_from_path(&resolved_file_name);
    r.resolved_file_name = alloc_str(&resolved_file_name);
    r.original_path = alloc_str(&original_path);
    r.is_custom_resolution = true;
    Ok(P::new(r))
}

fn mode_from(v: &Value, what: &str) -> ApiResult<Option<ResolutionMode>> {
    match v {
        Value::Null => Ok(None),
        Value::Number(n) => {
            let mode = match *n as i64 {
                0 => ModuleKind::NONE,
                1 => ModuleKind::COMMON_JS,
                99 => ModuleKind::ES_NEXT,
                x => {
                    return Err(ApiError::client(format!("{what} has invalid resolutionMode {x}")));
                }
            };
            Ok(Some(mode))
        }
        _ => Err(ApiError::invalid_request(format!("{what}: resolutionMode must be a number"))),
    }
}

/// Go `NewPackageId` (nil when the package has no name).
pub(crate) fn package_id_response(p: &tsrs_module::PackageId) -> Option<Value> {
    (!p.name.is_empty()).then(|| {
        Obj::new()
            .set("name", s(p.name))
            .set("subModuleName", s(p.sub_module_name))
            .set("version", s(p.version))
            .set("peerDependencies", s(p.peer_dependencies))
            .build()
    })
}

/// Go `newResolvedModuleResponse`.
pub(crate) fn resolved_module_response(r: &ResolvedModule) -> Value {
    if !r.is_resolved() {
        return Value::Null;
    }
    let pkg = package_id_response(&r.package_id);
    Obj::new()
        .set("resolvedFileName", s(r.resolved_file_name))
        .set_omitempty("originalPath", s(r.original_path))
        .set("extension", s(r.extension))
        // encoding/json/v2 `omitempty` keeps `false`.
        .set("resolvedUsingTsExtension", b(r.resolved_using_ts_extension))
        .set("resolvedUsingExtraExtensions", b(r.resolved_using_extra_extensions))
        .set_opt("packageId", pkg)
        .set("isExternalLibraryImport", b(r.is_external_library_import))
        .set_omitempty("alternateResult", s(r.alternate_result))
        .build()
}

/// Go `moduleResolverFactory`.
struct Factory {
    registration: Arc<ModuleResolverRegistration>,
    session: std::sync::Weak<Session>,
    conn: Option<Arc<dyn ClientConn>>,
    current_directory: String,
}

impl ModuleResolverFactory for Factory {
    fn new_resolver(
        &self,
        _ctx: &Context,
        mut options: ResolverOptions<'static>,
    ) -> (Box<dyn Resolver>, Box<dyn FnOnce() + Send>) {
        options.compiler_options = self.registration.compiler_options;
        let template = ResolverOptionsTemplate::from(&options);
        let mut fallback: Box<dyn Resolver> = Box::new(tsrs_module::new_resolver(options));
        let (Some(conn), false) = (&self.conn, self.registration.callback.is_empty()) else {
            if let Some(res) = &self.registration.resolutions {
                fallback = Box::new(StaticResolver { fallback, resolutions: Arc::clone(res) });
            }
            return (fallback, Box::new(|| {}));
        };
        let shared: Arc<dyn Resolver> = Arc::from(fallback);
        let (context_id, release): (u64, Box<dyn FnOnce() + Send>) = match self.session.upgrade() {
            Some(session) => {
                let id =
                    session.module_resolvers.next_context_id.fetch_add(1, Ordering::SeqCst) + 1;
                let mut resolvers = FxHashMap::default();
                resolvers.insert(self.registration.id, Arc::clone(&shared));
                session.module_resolvers.contexts.lock().unwrap().insert(
                    id,
                    Arc::new(ProgramResolutionContext {
                        options: template,
                        resolvers: Mutex::new(resolvers),
                    }),
                );
                let weak = Weak::clone(&self.session);
                (
                    id,
                    Box::new(move || {
                        if let Some(s) = weak.upgrade() {
                            s.module_resolvers.contexts.lock().unwrap().remove(&id);
                        }
                    }),
                )
            }
            None => (0, Box::new(|| {})),
        };
        let mut resolver: Box<dyn Resolver> = Box::new(CallbackResolver {
            registration: Arc::clone(&self.registration),
            conn: Arc::clone(conn),
            current_directory: self.current_directory.clone(),
            snapshot: 0,
            context_id,
            fallback: Box::new(SharedResolver(shared)),
        });
        if let Some(res) = &self.registration.resolutions {
            resolver =
                Box::new(StaticResolver { fallback: resolver, resolutions: Arc::clone(res) });
        }
        (resolver, release)
    }
}

/// `module.ResolutionHost` over a filesystem, for standalone and snapshot-scoped `resolveModuleName`.
struct FsHost {
    fs: Arc<dyn FS>,
    cwd: String,
}

impl ResolutionHost for FsHost {
    fn fs(&self) -> &dyn FS {
        &*self.fs
    }
    fn get_current_directory(&self) -> &str {
        &self.cwd
    }
}

impl Session {
    /// Go `compileModuleResolutionSpec`.
    fn compile_module_resolution_spec(
        &self,
        v: &Value,
    ) -> ApiResult<Option<Arc<StaticResolutions>>> {
        if matches!(v, Value::Null) {
            return Ok(None);
        }
        let p = Params(v);
        p.object()?;
        let cwd = self.current_directory().to_string();
        let fallback = match p.str("fallback")? {
            "resolve" => true,
            "unresolved" => false,
            other => {
                return Err(ApiError::client(format!(
                    "invalid module resolution fallback {other:?}"
                )));
            }
        };
        let mut entries = FxHashMap::default();
        for (i, entry) in p.array("entries")?.iter().enumerate() {
            if matches!(entry, Value::Null) {
                return Err(ApiError::client(format!("module resolution entry {i} is null")));
            }
            let e = Params(entry);
            let module_name = e.str("moduleName")?.to_string();
            if module_name.is_empty() {
                return Err(ApiError::client(format!(
                    "module resolution entry {i} has an empty moduleName"
                )));
            }
            if !e.has("result") {
                return Err(ApiError::client(format!("module resolution entry {i} has no result")));
            }
            let directory = if e.has("containingDirectory") {
                let d =
                    DocumentIdentifier::parse(e.get("containingDirectory"), "containingDirectory")?;
                let dir =
                    tspath::get_normalized_absolute_path(&d.to_absolute_file_name(&cwd), &cwd);
                Some(tspath::to_path(&dir, &cwd, self.use_case_sensitive_file_names()))
            } else {
                None
            };
            let mode = mode_from(e.get("resolutionMode"), &format!("module resolution entry {i}"))?;
            let key = StaticKey { module_name: module_name.clone(), directory, mode };
            match entries.entry(key) {
                Entry::Occupied(_) => {
                    return Err(ApiError::client(format!(
                        "duplicate static module resolution for {module_name:?}"
                    )));
                }
                Entry::Vacant(entry) => {
                    entry.insert(static_resolution_to_resolved_module(e.get("result"), &cwd)?);
                }
            }
        }
        Ok(Some(Arc::new(StaticResolutions {
            fallback_to_resolver: fallback,
            entries,
            current_directory: cwd,
            use_case_sensitive_file_names: self.use_case_sensitive_file_names(),
        })))
    }

    pub(crate) fn handle_create_module_resolver(&self, p: Params) -> ApiResult<Value> {
        p.object()?;
        let options = crate::raw_options::compiler_options_from_raw(
            p.get("compilerOptions"),
            self.current_directory(),
        )?;
        let resolutions = self.compile_module_resolution_spec(p.get("moduleResolutions"))?;
        let id = self.module_resolvers.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let registration = Arc::new(ModuleResolverRegistration {
            id,
            compiler_options: P::new(options),
            resolutions,
            callback: p.str("resolveModuleNameCallback")?.to_string(),
        });
        self.module_resolvers.registrations.lock().unwrap().insert(id, registration);
        Ok(Value::Number(id as f64))
    }

    pub(crate) fn handle_release_module_resolver(&self, p: Params) -> ApiResult<Value> {
        let id = p.u64("resolver")?;
        let removed = self.module_resolvers.registrations.lock().unwrap().remove(&id);
        match removed {
            Some(_) => Ok(Value::Null),
            None => Err(ApiError::client(format!("module resolver {id} not found"))),
        }
    }

    fn registration(&self, id: u64) -> ApiResult<Arc<ModuleResolverRegistration>> {
        self.module_resolvers
            .registrations
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| ApiError::client(format!("module resolver {id} not found")))
    }

    /// Go `Session.moduleResolverFactory` for `createPrograms[].options.moduleResolver`.
    pub(crate) fn module_resolver_factory(
        &self,
        id: u64,
    ) -> ApiResult<(Arc<dyn ModuleResolverFactory>, u64)> {
        let registration = self.registration(id)?;
        let conn = self.connection();
        if !registration.callback.is_empty() && conn.is_none() {
            return Err(ApiError::client("API connection is not initialized"));
        }
        let factory = Factory {
            registration,
            session: self.weak_self(),
            conn,
            current_directory: self.current_directory().to_string(),
        };
        Ok((Arc::new(factory), id))
    }

    pub(crate) fn handle_resolve_module_name(&self, p: Params) -> ApiResult<Value> {
        let module_name = p.str("moduleName")?;
        if module_name.is_empty() {
            return Err(ApiError::client("moduleName is empty"));
        }
        let data = self.registration(p.u64("resolver")?)?;
        let mode = mode_from(p.get("resolutionMode"), "request")?.unwrap_or(ModuleKind::NONE);
        let cwd = self.current_directory().to_string();
        let dir = p.document("containingDirectory")?;
        let containing_directory =
            tspath::get_normalized_absolute_path(&dir.to_absolute_file_name(&cwd), &cwd);
        let snapshot = p.u64("snapshot")?;
        let in_progress = p.u64("inProgressSnapshot")?;
        if snapshot != 0 && in_progress != 0 {
            return Err(ApiError::client("snapshot and inProgressSnapshot are mutually exclusive"));
        }

        // Resolver host for this call: the resolver borrows it, so it lives on this frame until the resolver is
        // dropped (issue #646).
        let call_host: FsHost;
        let _sd_pin;
        let mut resolver: Box<dyn Resolver + '_> = if in_progress != 0 {
            let ctx = self.module_resolvers.contexts.lock().unwrap().get(&in_progress).cloned();
            let ctx = ctx.ok_or_else(|| {
                ApiError::client(format!("in-progress snapshot {in_progress} not found"))
            })?;
            let mut resolvers = ctx.resolvers.lock().unwrap();
            let r = Arc::clone(resolvers.entry(data.id).or_insert_with(|| {
                Arc::new(tsrs_module::new_resolver(ctx.options.with_options(data.compiler_options)))
                    as Arc<dyn Resolver>
            }));
            Box::new(SharedResolver(r))
        } else {
            let fs: Arc<dyn FS> = if snapshot != 0 {
                let sd = self.snapshot_data(snapshot)?;
                let fs = sd.snapshot.fs();
                _sd_pin = sd;
                fs
            } else {
                self.fs()
            };
            call_host = FsHost { fs, cwd: cwd.clone() };
            Box::new(tsrs_module::new_resolver(ResolverOptions::new(
                &call_host,
                data.compiler_options,
            )))
        };
        if !data.callback.is_empty() {
            let conn = self
                .connection()
                .ok_or_else(|| ApiError::client("API connection is not initialized"))?;
            resolver = Box::new(CallbackResolver {
                registration: Arc::clone(&data),
                conn,
                current_directory: cwd,
                snapshot,
                context_id: in_progress,
                fallback: resolver,
            });
        }
        if let Some(res) = &data.resolutions {
            resolver =
                Box::new(StaticResolver { fallback: resolver, resolutions: Arc::clone(res) });
        }
        let result =
            resolver.resolve_module_name_from_directory(module_name, &containing_directory, mode);
        drop(resolver);
        let (resolved, trace) = result.map_err(ApiError::internal)?;
        let trace: Vec<String> = trace
            .iter()
            .map(|t| {
                tsrs_diagnostics::localize(
                    Some(t.message),
                    tsrs_diagnostics::Key::default(),
                    &t.args,
                )
            })
            .collect();
        Ok(Obj::new()
            .set("resolvedModule", resolved_module_response(&resolved))
            .set_opt("trace", (!trace.is_empty()).then(|| strings(trace)))
            .build())
    }
}
