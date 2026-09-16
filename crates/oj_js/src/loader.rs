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
        let referrer_url = Url::parse(referrer)
            .map_err(|e| JsErrorBox::type_error(format!("invalid referrer \"{referrer}\": {e}")))?;
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

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        maybe_referrer: Option<&deno_core::ModuleLoadReferrer>,
        options: deno_core::ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        let specifier = module_specifier.clone();
        if specifier.scheme() != "file" {
            return ModuleLoadResponse::Sync(Err(JsErrorBox::type_error(format!(
                "oj_js cannot load modules with scheme \"{}\"",
                specifier.scheme()
            ))));
        }
        let loader = self.npm_module_loader.clone();
        let referrer = maybe_referrer.map(|r| r.specifier.clone());
        let requested = options.requested_module_type;
        ModuleLoadResponse::Async(
            async move {
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
