// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Module loading for the engine: Node.js resolution semantics (bare
//! specifiers from the app's node_modules, `node:` builtins) plus on-the-fly
//! CJS-to-ESM translation for CommonJS packages, backed by deno_resolver's
//! byonm ("bring your own node_modules") machinery.

use std::borrow::Cow;
use std::path::Path;

use deno_core::futures::FutureExt;
use deno_core::url::Url;
use deno_core::FastString;
use deno_core::ModuleLoadResponse;
use deno_core::ModuleLoader;
use deno_core::ModuleSource;
use deno_core::ModuleSourceCode;
use deno_core::ModuleSpecifier;
use deno_core::ModuleType;
use deno_core::RequestedModuleType;
use deno_core::ResolutionKind;
use deno_error::JsErrorBox;
use deno_media_type::MediaType;
use deno_resolver::cjs::CjsTrackerRc;
use deno_resolver::loader::DenoNpmModuleLoaderRc;
use deno_resolver::loader::LoadedModuleSource;
use deno_resolver::npm::DenoInNpmPackageChecker;
use deno_resolver::npm::NpmResolver;
use deno_runtime::deno_node::NodeRequireLoader;
use deno_runtime::deno_permissions::PermissionsContainer;
use node_resolver::errors::PackageJsonLoadError;
use node_resolver::DenoIsBuiltInNodeModuleChecker;
use node_resolver::NodeResolutionKind;
use node_resolver::ResolutionMode;

use crate::host::HostBridge;
use crate::host::HostModuleType;
use crate::host::HostResolved;

/// The engine runs against the real filesystem only.
pub(crate) type Sys = sys_traits::impls::RealSys;

pub(crate) type EngineNodeResolver = node_resolver::NodeResolverRc<
    DenoInNpmPackageChecker,
    DenoIsBuiltInNodeModuleChecker,
    NpmResolver<Sys>,
    Sys,
>;

pub(crate) struct EngineModuleLoader {
    pub node_resolver: EngineNodeResolver,
    pub npm_module_loader: DenoNpmModuleLoaderRc<Sys>,
    /// The engine root as a URL: the referrer for Node resolution when the
    /// real referrer is not a file (a host's virtual module importing a bare
    /// npm specifier resolves from the app root, like the app itself would).
    pub root: Url,
    /// When set, the host is consulted before byonm: it sees every import
    /// except absolute `file:`/`node:`/`data:`/`blob:` URLs on resolve, and
    /// every module fetch except `node:` builtins on load.
    pub host: Option<HostBridge>,
}

impl EngineModuleLoader {
    fn node_resolve(&self, specifier: &str, referrer: &str) -> Result<ModuleSpecifier, JsErrorBox> {
        let referrer_url = Url::parse(referrer)
            .map_err(|e| JsErrorBox::type_error(format!("invalid referrer \"{referrer}\": {e}")))?;
        let referrer_url = if referrer_url.scheme() == "file" {
            referrer_url
        } else {
            // A synthetic file identity directly in the root: byonm walks up
            // from the referrer file's directory, so the app root's own
            // node_modules is the first stop.
            self.root
                .join("__oj_virtual__.mjs")
                .unwrap_or_else(|_| self.root.clone())
        };
        self.node_resolver
            .resolve(
                specifier,
                &referrer_url,
                ResolutionMode::Import,
                NodeResolutionKind::Execution,
            )
            .and_then(|resolution| resolution.into_url())
            .map_err(JsErrorBox::from_err)
    }
}

impl ModuleLoader for EngineModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, JsErrorBox> {
        // Absolute URLs pass through. `node:` builtins live in the snapshot's
        // module map (registered by the deno_node extension), so returning the
        // URL as-is is all that is needed for them.
        if let Ok(url) = Url::parse(specifier) {
            if matches!(url.scheme(), "file" | "node" | "data" | "blob") {
                return Ok(url);
            }
        }
        if let Some(host) = &self.host {
            // Synchronous seam over an async host: the future runs on the main
            // runtime while this isolate thread parks on the reply.
            match host
                .resolve_blocking(referrer, specifier)
                .map_err(JsErrorBox::generic)?
            {
                Some(HostResolved::Url(url)) => {
                    return Url::parse(&url).map_err(|e| {
                        JsErrorBox::type_error(format!(
                            "module host returned an invalid URL \"{url}\": {e}"
                        ))
                    });
                }
                Some(HostResolved::External(spec)) => {
                    return self.node_resolve(&spec, referrer);
                }
                None => {
                    // Not the host's module, but already an absolute URL (a
                    // host-scheme specifier the host chose not to re-resolve,
                    // e.g. the root of a dynamic import): pass it through to
                    // load, which consults the host again.
                    if let Ok(url) = Url::parse(specifier) {
                        if url.scheme().len() > 1 {
                            return Ok(url);
                        }
                    }
                }
            }
        }
        self.node_resolve(specifier, referrer)
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        maybe_referrer: Option<&deno_core::ModuleLoadReferrer>,
        options: deno_core::ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        let specifier = module_specifier.clone();
        let host = self.host.clone();
        let loader = self.npm_module_loader.clone();
        let referrer = maybe_referrer.map(|r| r.specifier.clone());
        let requested = options.requested_module_type;
        ModuleLoadResponse::Async(
            async move {
                if let Some(host) = &host {
                    if specifier.scheme() != "node" {
                        if let Some(module) = host
                            .load(specifier.as_str())
                            .await
                            .map_err(JsErrorBox::generic)?
                        {
                            let module_type = match module.module_type {
                                HostModuleType::JavaScript => ModuleType::JavaScript,
                                HostModuleType::Json => ModuleType::Json,
                            };
                            return Ok(ModuleSource::new(
                                module_type,
                                ModuleSourceCode::String(module.code.into()),
                                &specifier,
                                None,
                            ));
                        }
                    }
                }
                if specifier.scheme() != "file" {
                    return Err(JsErrorBox::type_error(format!(
                        "oj_js cannot load modules with scheme \"{}\"",
                        specifier.scheme()
                    )));
                }
                let requested_dr = as_deno_resolver_requested_module_type(&requested);
                let loaded = loader
                    .load(
                        Cow::Owned(specifier.clone()),
                        referrer.as_ref(),
                        &requested_dr,
                        None,
                    )
                    .await
                    .map_err(JsErrorBox::from_err)?;
                Ok(ModuleSource::new_with_redirect(
                    module_type_from_media_and_requested_type(loaded.media_type, &requested),
                    loaded_module_source_to_module_source_code(loaded.source),
                    &specifier,
                    &loaded.specifier,
                    None,
                ))
            }
            .boxed_local(),
        )
    }

    /// For source maps whose `sources` are relative names: the mapped frame
    /// only replaces the compiled specifier when the original file exists
    /// (deno_core's guard against maps that name undistributed sources).
    fn source_map_source_exists(&self, source_url: &str) -> Option<bool> {
        let url = Url::parse(source_url).ok()?;
        let path = deno_path_util::url_to_file_path(&url).ok()?;
        Some(path.is_file())
    }
}

pub(crate) struct EngineRequireLoader {
    pub cjs_tracker: CjsTrackerRc<DenoInNpmPackageChecker, Sys>,
    pub sys: Sys,
}

impl NodeRequireLoader for EngineRequireLoader {
    fn ensure_read_permission<'a>(
        &self,
        _permissions: &mut PermissionsContainer,
        path: Cow<'a, Path>,
    ) -> Result<Cow<'a, Path>, JsErrorBox> {
        // The engine runs trusted, oj-owned scripts with allow-all permissions.
        Ok(path)
    }

    fn load_text_file_lossy(&self, path: &Path) -> Result<FastString, JsErrorBox> {
        use sys_traits::FsRead;
        let text = self
            .sys
            .fs_read_to_string_lossy(path)
            .map_err(JsErrorBox::from_err)?;
        Ok(match text {
            Cow::Borrowed(text) => FastString::from_static(text),
            Cow::Owned(text) => text.into(),
        })
    }

    fn is_maybe_cjs(&self, specifier: &Url) -> Result<bool, PackageJsonLoadError> {
        self.cjs_tracker
            .is_maybe_cjs(specifier, MediaType::from_specifier(specifier))
    }

    fn is_maybe_cjs_from_require(&self, specifier: &Url) -> Result<bool, PackageJsonLoadError> {
        self.cjs_tracker
            .is_maybe_cjs_from_require(specifier, MediaType::from_specifier(specifier))
    }
}

// The three helpers below mirror deno_lib's loader glue (MIT licensed), which
// is the piece of the denort assembly that is not reusable from the published
// crates without dragging in the rest of deno_lib.

fn module_type_from_media_and_requested_type(
    media_type: MediaType,
    requested_module_type: &RequestedModuleType,
) -> ModuleType {
    match requested_module_type {
        RequestedModuleType::Text => ModuleType::Text,
        RequestedModuleType::Bytes => ModuleType::Bytes,
        RequestedModuleType::Other(kind) => ModuleType::Other(kind.clone()),
        RequestedModuleType::None | RequestedModuleType::Json => match media_type {
            MediaType::Json => ModuleType::Json,
            MediaType::Wasm => ModuleType::Wasm,
            _ => ModuleType::JavaScript,
        },
    }
}

fn loaded_module_source_to_module_source_code(source: LoadedModuleSource) -> ModuleSourceCode {
    match source {
        LoadedModuleSource::ArcStr(text) => ModuleSourceCode::String(text.into()),
        LoadedModuleSource::ArcBytes(bytes) => ModuleSourceCode::Bytes(bytes.into()),
        LoadedModuleSource::String(text) => match text {
            Cow::Borrowed(text) => ModuleSourceCode::String(FastString::from_static(text)),
            Cow::Owned(text) => ModuleSourceCode::String(text.into()),
        },
        LoadedModuleSource::Bytes(bytes) => match bytes {
            Cow::Borrowed(bytes) => ModuleSourceCode::Bytes(bytes.into()),
            Cow::Owned(bytes) => ModuleSourceCode::Bytes(bytes.into_boxed_slice().into()),
        },
    }
}

fn as_deno_resolver_requested_module_type(
    value: &RequestedModuleType,
) -> deno_resolver::loader::RequestedModuleType<'_> {
    match value {
        RequestedModuleType::None => deno_resolver::loader::RequestedModuleType::None,
        RequestedModuleType::Json => deno_resolver::loader::RequestedModuleType::Json,
        RequestedModuleType::Text => deno_resolver::loader::RequestedModuleType::Text,
        RequestedModuleType::Bytes => deno_resolver::loader::RequestedModuleType::Bytes,
        RequestedModuleType::Other(text) => deno_resolver::loader::RequestedModuleType::Other(text),
    }
}
